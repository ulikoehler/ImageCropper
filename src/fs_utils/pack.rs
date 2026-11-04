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
    io::{Read, Seek, SeekFrom, Write},
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
    /// Latest content hash per source name (any signature variant).
    hashes: HashMap<String, [u8; 32]>,
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
            hashes: HashMap::new(),
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
    fn replay(&mut self) {
        let file_len = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut pos = 0u64;
        let mut hdr = [0u8; 7];
        loop {
            if pos + hdr.len() as u64 > file_len {
                break;
            }
            if self.file.seek(SeekFrom::Start(pos)).is_err()
                || self.file.read_exact(&mut hdr).is_err()
            {
                break;
            }
            let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            let rtype = hdr[4];
            let name_len = u16::from_le_bytes(hdr[5..7].try_into().unwrap()) as usize;
            if magic != MAGIC || rtype > TYPE_HASH || name_len > MAX_NAME {
                break;
            }
            let mut name = vec![0u8; name_len];
            if self.file.read_exact(&mut name).is_err() {
                break;
            }
            let name = match String::from_utf8(name) {
                Ok(n) => n,
                Err(_) => break,
            };
            let after_name = pos + 7 + name_len as u64;
            if rtype == TYPE_TOMBSTONE {
                self.remove_entries(&name);
                pos = after_name;
                continue;
            }
            // type 1/2 body header: len u64 | nanos u128 | sha256 | dim | jpeg_len
            const BODY: usize = 8 + 16 + 32 + 2 + 4;
            if after_name + BODY as u64 > file_len {
                break;
            }
            let mut body = [0u8; BODY];
            if self.file.read_exact(&mut body).is_err() {
                break;
            }
            let len = u64::from_le_bytes(body[0..8].try_into().unwrap());
            let nanos = u128::from_le_bytes(body[8..24].try_into().unwrap());
            let hash: [u8; 32] = body[24..56].try_into().unwrap();
            let dim = u16::from_le_bytes(body[56..58].try_into().unwrap());
            let jpeg_len = u32::from_le_bytes(body[58..62].try_into().unwrap()) as u64;
            let rec_len = 7 + name_len as u64 + BODY as u64 + jpeg_len;
            if jpeg_len > MAX_JPEG || pos + rec_len > file_len {
                break;
            }
            let jpeg_off = after_name + BODY as u64;
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
            pos += rec_len;
        }
        if pos < file_len {
            // Torn tail (crash mid-append): cut it so later appends don't
            // build on top of garbage.
            let _ = self.file.set_len(pos);
        }
        self.len = pos;
    }

    /// Drop every index/hash entry for `name` (tombstone replay).
    fn remove_entries(&mut self, name: &str) {
        let mut freed = 0u64;
        self.index.retain(|(n, _, _), e| {
            let dead = n == name;
            if dead {
                freed += e.rec_len;
            }
            !dead
        });
        self.garbage += freed;
        self.hashes.remove(name);
    }

    /// Insert/refresh one record entry, pruning other signature variants of
    /// the same source name — a changed file makes its old previews dead
    /// bytes. `count_garbage` is off during replay (everything superseded
    /// there is already counted by position).
    fn put_entry(&mut self, name: String, len: u64, nanos: u128, ent: Entry, count: bool) {
        let stale: Vec<Key> = self
            .index
            .keys()
            .filter(|(n, l, t)| n == &name && (*l, *t) != (len, nanos))
            .cloned()
            .collect();
        for k in stale {
            if let Some(e) = self.index.remove(&k) {
                self.garbage += e.rec_len;
            }
        }
        if let Some(old) = self
            .index
            .insert((name.clone(), len, nanos), ent)
            .filter(|_| count)
        {
            self.garbage += old.rec_len;
        }
        self.hashes.insert(name, ent.hash);
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
        self.file.seek(SeekFrom::Start(ent.off)).ok()?;
        self.file.read_exact(&mut buf).ok()?;
        Some((buf, hex(&ent.hash)))
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
            if self.index.is_empty() && self.hashes.is_empty() && self.len > 0 {
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
        self.file.seek(SeekFrom::Start(off)).ok()?;
        self.file.write_all(rec).ok()?;
        self.len += rec.len() as u64;
        self.since_sync += 1;
        if self.since_sync >= SYNC_EVERY {
            let _ = self.file.sync_data();
            self.since_sync = 0;
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
                if self.file.seek(SeekFrom::Start(e.off)).is_err()
                    || self.file.read_exact(&mut jpeg).is_err()
                {
                    return; // keep the old pack rather than risk losing data
                }
            }
            recs.push((k.clone(), *e, jpeg));
        }
        let tmp = self.pack_path.with_extension("pack.tmp");
        let Ok(mut out) = std::fs::File::create(&tmp) else { return };
        let mut new_index = HashMap::with_capacity(recs.len());
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
            new_index.insert((name, len, nanos), ne);
            pos += rec.len() as u64;
        }
        if !ok || out.sync_all().is_err() || std::fs::rename(&tmp, &self.pack_path).is_err() {
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
            self.len = pos;
            self.garbage = 0;
        }
    }

    /// Latest recorded content hash for `name` (any signature variant).
    pub fn stored_hash(&self, name: &str) -> Option<String> {
        self.hashes.get(name).map(hex)
    }

    /// No live records and no hashes — the pack file itself is removable.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty() && self.hashes.is_empty()
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
    let mut packs = PACKS.lock().unwrap();
    if let Some(p) = packs.get(dir) {
        return Some(p.clone());
    }
    let pack = Pack::open(dir)?;
    let pack = std::sync::Arc::new(Mutex::new(pack));
    packs.insert(dir.to_path_buf(), pack.clone());
    Some(pack)
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
