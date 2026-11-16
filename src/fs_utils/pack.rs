//! Per-directory thumbnail pack: one append-only `thumbs.pack` file inside
//! each `.imagecropper-previews` directory replaces the per-image
//! `<name>.<sig>.jpg` + `.sha256` file pairs, so a folder of 100k images
//! costs one cache file instead of 200k.
//!
//! Format: a log of length-framed records.
//!
//! ```text
//! magic u32 "ICP1" | type u8 | name_len u16 | name bytes
//!   type 0 TOMBSTONE:  removes every record for `name`
//!   type 1 PUT:        len u64 | nanos u128 | sha256 [32] | dim u16 |
//!                      jpeg_len u32 | jpeg bytes
//!   type 2 HASH:       len u64 | nanos u128 | sha256 [32] | dim u16 | 0
//! ```
//!
//! On open the log is replayed into an in-memory index; a torn/corrupt tail
//! (crash mid-append) stops the replay and the file is truncated back to
//! the last good record. Superseded and tombstoned records stay in the file
//! as garbage until compaction rewrites the pack.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

pub const PACK_FILE: &str = "thumbs.pack";
const MAGIC: u32 = 0x3150_4349; // "ICP1" little-endian
const TYPE_TOMBSTONE: u8 = 0;
const TYPE_PUT: u8 = 1;
const TYPE_HASH: u8 = 2;
const MAX_NAME: usize = 4096;
const MAX_JPEG: u64 = 64 * 1024 * 1024;
/// fsync every N appends — per-record fsync would serialize the whole
/// background producer on a slow mount; losing the unsynced tail is safe
/// (it just regenerates).
const SYNC_EVERY: u32 = 256;
/// Compact only once at least this much garbage can be reclaimed — tomb-
/// stoned/superseded records otherwise just accumulate. On a slow mount a
/// pack rewrite is expensive enough that small amounts of dead bytes are
/// not worth chasing.
const COMPACT_MIN_GARBAGE: u64 = 1 << 30; // 1 GiB

#[derive(Clone, Copy)]
struct Entry {
    /// Absolute offset of the jpeg payload in the pack (0 when hash-only).
    off: u64,
    jpeg_len: u32,
    /// Total record length — becomes garbage when the entry is superseded.
    rec_len: u64,
    hash: [u8; 32],
    dim: u16,
    has_jpeg: bool,
}

type Key = (String, u64, u128);

/// Open pack: file handle + index + garbage accounting. All access is
/// serialized by the registry mutex wrapping each `Pack`.
pub struct Pack {
    file: std::fs::File,
    pack_path: PathBuf,
    index: HashMap<Key, Entry>,
    /// Source name → its single live signature. At most one signature
    /// variant per name is ever live (a changed file makes the old
    /// previews dead bytes), so this replaces both the old `hashes` map
    /// and the O(index-size) stale-variant scan `put_entry` used to run
    /// per record — replay and bulk appends were quadratic without it.
    names: HashMap<String, (u64, u128)>,
    /// Append offset == logical file length.
    len: u64,
    /// Bytes belonging to superseded/tombstoned records.
    garbage: u64,
    /// Set when the (empty) pack file was unlinked — writes are refused
    /// and lookups miss, so a later produce recreates the pack cleanly.
    dead: bool,
    since_sync: u32,
}

fn hex(hash: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in hash {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl Pack {
    fn path(dir: &Path) -> PathBuf {
        dir.join(PACK_FILE)
    }

    /// Open (or create) the pack for `dir`, replaying the record log and
    /// sweeping leftover flat-cache files (`.jpg`, `.sha256`, `.tmp-*`)
    /// from before the pack format existed.
    pub fn open(dir: &Path) -> Option<Self> {
        std::fs::create_dir_all(dir).ok()?;
        let path = Self::path(dir);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .ok()?;
        let mut pack = Self {
            pack_path: path,
            index: HashMap::new(),
            names: HashMap::new(),
            len: 0,
            garbage: 0,
            dead: false,
            since_sync: 0,
            file,
        };
        pack.replay();
        pack.sweep_legacy(dir);
        Some(pack)
    }

    /// Replay the log; truncate at the first torn/invalid record.
    ///
    /// Reads through a 1 MiB window instead of ~4 small reads per record:
    /// a large pack replays in `size/CAP` syscalls rather than ~4 per
    /// record, which is what makes opening a 100k-thumbnail pack usable
    /// on slow/remote filesystems.
    fn replay(&mut self) {
        const BODY: usize = 8 + 16 + 32 + 2 + 4;
        let file_len = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        // A cloned fd gives the reader its own file offset and keeps the
        // borrow of `self.file` out of the parse loop. try_clone is a
        // plain dup(2) — if it ever fails, treat the pack as empty-but-
        // present: appends continue at `len` and stale records simply
        // age out as garbage instead of wedging the whole cache.
        let Ok(file) = self.file.try_clone() else {
            self.len = file_len;
            return;
        };
        let mut r = Replay::new(file);
        // Offset just past the last fully-consumed record. A torn tail is
        // cut HERE, not at the parse cursor: a partial record's header may
        // already parse cleanly (magic+type+name_len are only 7 bytes),
        // and truncating past it would leave header bytes that the next
        // append then lands on top of.
        let mut end = 0u64;
        loop {
            if r.pos + 7 > file_len {
                break;
            }
            // Record header: magic u32 | type u8 | name_len u16.
            let (magic, rtype, name_len) = {
                let Some(h) = r.peek(7) else { break };
                (
                    u32::from_le_bytes(h[0..4].try_into().unwrap()),
                    h[4],
                    u16::from_le_bytes(h[5..7].try_into().unwrap()) as usize,
                )
            };
            if magic != MAGIC || rtype > TYPE_HASH || name_len > MAX_NAME {
                break;
            }
            r.advance(7);
            let is_tomb = rtype == TYPE_TOMBSTONE;
            let body_len = if is_tomb { 0 } else { BODY };
            // name (+ fixed body for PUT/HASH) must be contiguous.
            let parsed = {
                let Some(nb) = r.peek(name_len + body_len) else {
                    break;
                };
                let Ok(name) = std::str::from_utf8(&nb[..name_len]) else {
                    break;
                };
                if is_tomb {
                    Some((name.to_string(), 0u64, 0u128, [0u8; 32], 0u16, 0u64))
                } else {
                    let b = &nb[name_len..name_len + BODY];
                    Some((
                        name.to_string(),
                        u64::from_le_bytes(b[0..8].try_into().unwrap()),
                        u128::from_le_bytes(b[8..24].try_into().unwrap()),
                        b[24..56].try_into().unwrap(),
                        u16::from_le_bytes(b[56..58].try_into().unwrap()),
                        u32::from_le_bytes(b[58..62].try_into().unwrap()) as u64,
                    ))
                }
            };
            let Some((name, len, nanos, hash, dim, jpeg_len)) = parsed else {
                break;
            };
            r.advance((name_len + body_len) as u64);
            if is_tomb {
                self.remove_entries(&name);
                end = r.pos;
                continue;
            }
            let rec_len = 7 + name_len as u64 + BODY as u64 + jpeg_len;
            if jpeg_len > MAX_JPEG || r.pos + jpeg_len > file_len {
                break;
            }
            let jpeg_off = r.pos;
            self.put_entry(
                name,
                len,
                nanos,
                Entry {
                    off: jpeg_off,
                    jpeg_len: jpeg_len as u32,
                    rec_len,
                    hash,
                    dim,
                    has_jpeg: rtype == TYPE_PUT && jpeg_len > 0,
                },
                false,
            );
            // Skip the jpeg payload — only its offset was needed.
            r.advance(jpeg_len);
            end = r.pos;
        }
        drop(r);
        if end < file_len {
            // Torn tail (crash mid-append): cut it so later appends don't
            // build on top of garbage.
            let _ = self.file.set_len(end);
        }
        self.len = end;
    }

    /// Drop every index entry for `name` (tombstone replay). O(1) via the
    /// `names` map — a full `index.retain` scan per tombstone made bulk
    /// deletes quadratic on replay too.
    fn remove_entries(&mut self, name: &str) {
        if let Some((l, t)) = self.names.remove(name) {
            if let Some(e) = self.index.remove(&(name.to_string(), l, t)) {
                self.garbage += e.rec_len;
            }
        }
    }

    /// Insert/refresh one record entry, pruning other signature variants of
    /// the same source name — a changed file makes its old previews dead
    /// bytes. `count_garbage` is off during replay (everything superseded
    /// there is already counted by position).
    fn put_entry(&mut self, name: String, len: u64, nanos: u128, ent: Entry, count: bool) {
        if let Some(&(ol, ot)) = self.names.get(&name) {
            if (ol, ot) != (len, nanos) {
                if let Some(e) = self.index.remove(&(name.clone(), ol, ot)) {
                    self.garbage += e.rec_len;
                }
            }
        }
        if let Some(old) = self
            .index
            .insert((name.clone(), len, nanos), ent)
            .filter(|_| count)
        {
            self.garbage += old.rec_len;
        }
        self.names.insert(name, (len, nanos));
    }

    /// The persisted thumbnail for `(name, len, nanos)` — only when it was
    /// produced at `dim`. Returns the jpeg bytes and the recorded source
    /// sha256.
    pub fn lookup(&mut self, name: &str, len: u64, nanos: u128, dim: u16) -> Option<(Vec<u8>, String)> {
        let ent = *self.index.get(&(name.to_string(), len, nanos))?;
        if !ent.has_jpeg || ent.dim != dim || ent.jpeg_len == 0 {
            return None;
        }
        let mut buf = vec![0u8; ent.jpeg_len as usize];
        read_full_at(&mut self.file, &mut buf, ent.off).ok()?;
        Some((buf, hex(&ent.hash)))
    }

    /// Lookup by folded signature — for callers that already know the
    /// scan-time sig and want to skip a `stat` on slow mounts. Resolves
    /// the live `(len, nanos)` through `names`, so a stale sig simply
    /// misses and the caller falls back to a fresh signature.
    pub fn lookup_sig(&mut self, name: &str, sig: u64, dim: u16) -> Option<(Vec<u8>, String)> {
        let &(len, nanos) = self.names.get(name)?;
        if len ^ nanos as u64 ^ (nanos >> 64) as u64 != sig {
            return None;
        }
        self.lookup(name, len, nanos, dim)
    }

    /// Append a full thumbnail record.
    pub fn put(&mut self, name: &str, len: u64, nanos: u128, hash: &str, dim: u16, jpeg: &[u8]) {
        self.append(TYPE_PUT, name, len, nanos, hash, dim, jpeg);
    }

    /// Append a hash-only record (no preview bytes): records "the source's
    /// content hash was X" so later stale-signature checks are exact.
    pub fn put_hash(&mut self, name: &str, len: u64, nanos: u128, hash: &str) {
        self.append(TYPE_HASH, name, len, nanos, hash, 0, &[]);
    }

    /// Tombstone `name`: every record for the source stops resolving after
    /// this append, across restarts too.
    pub fn remove(&mut self, name: &str) {
        let mut rec = Vec::with_capacity(7 + name.len());
        rec.extend_from_slice(&MAGIC.to_le_bytes());
        rec.push(TYPE_TOMBSTONE);
        rec.extend_from_slice(&(name.len() as u16).to_le_bytes());
        rec.extend_from_slice(name.as_bytes());
        if self.write(&rec).is_some() {
            self.remove_entries(name);
            // The tombstone emptied the pack → rewrite it: truncate in
            // place instead of keeping a file full of dead records.
            if self.index.is_empty() && self.names.is_empty() && self.len > 0 {
                let _ = self.file.set_len(0);
                self.len = 0;
                self.garbage = 0;
            } else {
                // Dead records may already exceed the compaction floor —
                // reclaim once the saving is worth the rewrite.
                self.maybe_compact();
            }
        }
    }

    fn append(
        &mut self,
        rtype: u8,
        name: &str,
        len: u64,
        nanos: u128,
        hash: &str,
        dim: u16,
        jpeg: &[u8],
    ) {
        let Some(hash) = unhex(hash) else { return };
        if name.len() > MAX_NAME || jpeg.len() as u64 > MAX_JPEG {
            return;
        }
        let mut rec = Vec::with_capacity(7 + name.len() + 62 + jpeg.len());
        rec.extend_from_slice(&MAGIC.to_le_bytes());
        rec.push(rtype);
        rec.extend_from_slice(&(name.len() as u16).to_le_bytes());
        rec.extend_from_slice(name.as_bytes());
        rec.extend_from_slice(&len.to_le_bytes());
        rec.extend_from_slice(&nanos.to_le_bytes());
        rec.extend_from_slice(&hash);
        rec.extend_from_slice(&dim.to_le_bytes());
        rec.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
        rec.extend_from_slice(jpeg);
        let jpeg_off = 7 + name.len() as u64 + 62;
        let Some(rec_off) = self.write(&rec) else { return };
        self.put_entry(
            name.to_string(),
            len,
            nanos,
            Entry {
                off: rec_off + jpeg_off,
                jpeg_len: jpeg.len() as u32,
                rec_len: rec.len() as u64,
                hash,
                dim,
                has_jpeg: rtype == TYPE_PUT && !jpeg.is_empty(),
            },
            true,
        );
        self.maybe_compact();
    }

    /// Write one record at the append offset; returns the payload offset.
    fn write(&mut self, rec: &[u8]) -> Option<u64> {
        if self.dead {
            return None;
        }
        let off = self.len;
        write_all_at(&mut self.file, rec, off).ok()?;
        self.len += rec.len() as u64;
        self.since_sync += 1;
        if self.since_sync >= SYNC_EVERY {
            self.since_sync = 0;
            // fsync on a cloned fd, off the critical path: a slow device
            // must not stall every lookup/append queued on this pack's
            // mutex. Losing the unsynced tail is safe either way — the
            // records just regenerate.
            if let Ok(f) = self.file.try_clone() {
                std::thread::spawn(move || {
                    let _ = f.sync_data();
                });
            }
        }
        // payload offset = record start + header (7) — callers add their
        // own variable parts; only `append` uses it, so hand back the base.
        Some(off)
    }

    /// Dead bytes currently sitting in the pack file.
    #[doc(hidden)]
    pub fn garbage_bytes(&self) -> u64 {
        self.garbage
    }

    /// Rewrite the pack with only live records once at least
    /// [`COMPACT_MIN_GARBAGE`] dead bytes can be reclaimed.
    fn maybe_compact(&mut self) {
        if self.garbage >= COMPACT_MIN_GARBAGE {
            self.compact_now();
        }
    }

    /// Unconditional compaction — tests and the threshold check share this.
    #[doc(hidden)]
    pub fn compact_now(&mut self) {
        // Rebuild records from the index: every entry carries everything
        // needed to re-encode its record except the jpeg bytes themselves,
        // which are read back out of the current file.
        let mut recs: Vec<(Key, Entry, Vec<u8>)> = Vec::with_capacity(self.index.len());
        for (k, e) in &self.index {
            let mut jpeg = Vec::new();
            if e.has_jpeg {
                jpeg.resize(e.jpeg_len as usize, 0);
                if read_full_at(&mut self.file, &mut jpeg, e.off).is_err() {
                    return; // keep the old pack rather than risk losing data
                }
            }
            recs.push((k.clone(), *e, jpeg));
        }
        let tmp = self.pack_path.with_extension("pack.tmp");
        let Ok(out) = std::fs::File::create(&tmp) else { return };
        let mut out = std::io::BufWriter::with_capacity(1 << 20, out);
        let mut new_index = HashMap::with_capacity(recs.len());
        let mut new_names = HashMap::with_capacity(recs.len());
        let mut pos = 0u64;
        let mut ok = true;
        for ((name, len, nanos), e, jpeg) in recs {
            let mut rec = Vec::with_capacity(7 + name.len() + 62 + jpeg.len());
            rec.extend_from_slice(&MAGIC.to_le_bytes());
            rec.push(if e.has_jpeg { TYPE_PUT } else { TYPE_HASH });
            rec.extend_from_slice(&(name.len() as u16).to_le_bytes());
            rec.extend_from_slice(name.as_bytes());
            rec.extend_from_slice(&len.to_le_bytes());
            rec.extend_from_slice(&nanos.to_le_bytes());
            rec.extend_from_slice(&e.hash);
            rec.extend_from_slice(&e.dim.to_le_bytes());
            rec.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
            rec.extend_from_slice(&jpeg);
            let jpeg_off = pos + 7 + name.len() as u64 + 62;
            if out.write_all(&rec).is_err() {
                ok = false;
                break;
            }
            let mut ne = e;
            ne.off = jpeg_off;
            ne.rec_len = rec.len() as u64;
            new_names.insert(name.clone(), (len, nanos));
            new_index.insert((name, len, nanos), ne);
            pos += rec.len() as u64;
        }
        if !ok
            || out.flush().is_err()
            || out.get_ref().sync_all().is_err()
            || std::fs::rename(&tmp, &self.pack_path).is_err()
        {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.pack_path)
        {
            self.file = f;
            self.index = new_index;
            self.names = new_names;
            self.len = pos;
            self.garbage = 0;
        }
    }

    /// Latest recorded content hash for `name` — the live signature
    /// variant's record carries it.
    pub fn stored_hash(&self, name: &str) -> Option<String> {
        let (l, t) = *self.names.get(name)?;
        let ent = self.index.get(&(name.to_string(), l, t))?;
        Some(hex(&ent.hash))
    }

    /// No live records — the pack file itself is removable.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Absolute offset of `name`'s jpeg payload — tests corrupt bytes at
    /// this position to exercise self-healing.
    #[doc(hidden)]
    pub fn jpeg_offset(&self, name: &str, len: u64, nanos: u128) -> Option<u64> {
        let e = self.index.get(&(name.to_string(), len, nanos))?;
        e.has_jpeg.then_some(e.off)
    }

    /// One-time sweep: legacy flat previews and atomic-write scratch files
    /// predate the pack. `.sha256` sidecars carry the recorded content
    /// hashes stale-signature checks rely on — import them as hash records
    /// before deleting. Flat `.jpg`s are left for dead (a changed
    /// `THUMB_MAX_DIM` would serve wrong-size bytes anyway).
    fn sweep_legacy(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut hashes: Vec<(String, u64, u128, String)> = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == PACK_FILE {
                continue;
            }
            let n = name.to_string_lossy().into_owned();
            if let Some(parsed) = n
                .strip_suffix(".sha256")
                .and_then(|s| s.strip_suffix(".jpg"))
                .and_then(|stem| {
                    let (src, sig) = stem.rsplit_once('.')?;
                    let (len, nanos) = sig.split_once('-')?;
                    Some((
                        src.to_string(),
                        len.parse().ok()?,
                        nanos.parse().ok()?,
                    ))
                })
                .and_then(|(src, len, nanos)| {
                    std::fs::read_to_string(entry.path())
                        .ok()
                        .map(|h| (src, len, nanos, h.trim().to_string()))
                        .filter(|(_, _, _, h)| h.len() == 64)
                })
            {
                hashes.push(parsed);
                files.push(entry.path());
            } else if n.ends_with(".jpg") || n.starts_with(".tmp-") {
                files.push(entry.path());
            }
        }
        for (name, len, nanos, hash) in hashes {
            self.put_hash(&name, len, nanos, &hash);
        }
        for f in files {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Registry of open packs, one per previews directory.
static PACKS: LazyLock<Mutex<HashMap<PathBuf, std::sync::Arc<Mutex<Pack>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The pack for `dir`, opening + replaying it on first use.
pub fn pack_for(dir: &Path) -> Option<std::sync::Arc<Mutex<Pack>>> {
    {
        let packs = PACKS.lock().unwrap();
        if let Some(p) = packs.get(dir) {
            return Some(p.clone());
        }
    }
    // Open outside the registry lock: replaying a large pack is slow I/O
    // and must not serialize every other directory's first lookup. A
    // racing opener for the same dir loses the insert — `Pack::open` is
    // idempotent (replay + legacy sweep), so the dropped handle is fine.
    let pack = Pack::open(dir)?;
    let pack = std::sync::Arc::new(Mutex::new(pack));
    let mut packs = PACKS.lock().unwrap();
    Some(packs.entry(dir.to_path_buf()).or_insert(pack).clone())
}

/// Sequential windowed reader for log replay: parses record headers out
/// of a 1 MiB buffer filled by positioned reads, so a whole pack costs
/// `size/CAP` syscalls instead of ~4 per record. Payloads larger than
/// the remaining window are skipped by offset arithmetic — never read.
struct Replay {
    file: std::fs::File,
    buf: Vec<u8>,
    /// Absolute file offset of `buf[0]`.
    base: u64,
    /// Bytes valid in `buf`.
    len: usize,
    /// Absolute parse offset; always `>= base`.
    pos: u64,
}

impl Replay {
    const CAP: usize = 1 << 20;

    fn new(file: std::fs::File) -> Self {
        Self {
            file,
            buf: vec![0; Self::CAP],
            base: 0,
            len: 0,
            pos: 0,
        }
    }

    /// Ensure `n` bytes are readable at `pos`; false at EOF/error.
    /// `n` must be ≤ CAP (the largest possible record header is ~4.2 KB).
    fn fill(&mut self, n: usize) -> bool {
        debug_assert!(n <= Self::CAP);
        loop {
            let off = (self.pos - self.base) as usize;
            if off + n <= self.len {
                return true;
            }
            // Slide the unread tail to the front — or drop the window
            // entirely when `pos` jumped past it (a skipped payload).
            if off < self.len {
                let keep = self.len - off;
                self.buf.copy_within(off..self.len, 0);
                self.len = keep;
            } else {
                self.len = 0;
            }
            self.base = self.pos;
            let before = self.len;
            while self.len < self.buf.len() {
                match read_at_most(&mut self.file, &mut self.buf[self.len..], self.base + self.len as u64) {
                    Ok(0) | Err(_) => break,
                    Ok(r) => self.len += r,
                }
            }
            if self.len == before {
                // No progress — EOF or a read error; can't satisfy `n`.
                return false;
            }
        }
    }

    /// Immutable view of `n` bytes at `pos` (call `fill` first via peek).
    fn peek(&mut self, n: usize) -> Option<&[u8]> {
        if !self.fill(n) {
            return None;
        }
        let off = (self.pos - self.base) as usize;
        Some(&self.buf[off..off + n])
    }

    /// Move the parse offset forward `n` bytes (must be peeked already).
    fn advance(&mut self, n: u64) {
        self.pos += n;
    }
}

/// Positioned read of up to `buf.len()` bytes at `off`; returns the byte
/// count (0 at EOF). Loops internally past `Interrupted`.
fn read_at_most(file: &mut std::fs::File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        loop {
            match file.read_at(buf, off) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }
    #[cfg(not(unix))]
    {
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(off))?;
        loop {
            match file.read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => return r,
            }
        }
    }
}

/// Read exactly `buf.len()` bytes at `off`; `UnexpectedEof` on short read.
fn read_full_at(file: &mut std::fs::File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match read_at_most(file, buf, off)? {
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "pack record truncated",
                ))
            }
            n => {
                buf = &mut buf[n..];
                off += n as u64;
            }
        }
    }
    Ok(())
}

/// Write all of `buf` at `off`, looping past short writes.
fn write_all_at(file: &mut std::fs::File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            match file.write_at(buf, off) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "pack write made no progress",
                    ))
                }
                Ok(n) => {
                    buf = &buf[n..];
                    off += n as u64;
                }
                Err(e) => return Err(e),
            }
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, SeekFrom, Write};
            file.seek(SeekFrom::Start(off))?;
            match file.write(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "pack write made no progress",
                    ))
                }
                Ok(n) => {
                    buf = &buf[n..];
                    off += n as u64;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// If `dir`'s pack has no live records left, drop it from the registry and
/// unlink the file — a folder whose images were all deleted shouldn't keep
/// even a pack around. The pack is marked dead so any still-shared handle
/// refuses writes instead of appending into an unlinked file.
pub fn remove_pack_if_empty(dir: &Path) {
    let mut packs = PACKS.lock().unwrap();
    let Some(pack) = packs.get(dir).cloned() else {
        return;
    };
    let mut p = pack.lock().unwrap();
    if !p.is_empty() {
        return;
    }
    p.dead = true;
    packs.remove(dir);
    drop(p);
    drop(packs);
    let _ = std::fs::remove_file(dir.join(PACK_FILE));
}
