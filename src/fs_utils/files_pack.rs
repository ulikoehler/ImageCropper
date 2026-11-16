//! `files.pack` — binary snapshot of the last scan's file list, stored in
//! `.imagecropper-previews/` next to each scan root. On the next run the
//! entries seed the manifest instantly (no walk, no header sniffing), and
//! directories whose mtime is unchanged are skipped by the walker entirely
//! — so a re-scan only stats dir/file mtimes instead of enumerating and
//! re-parsing everything.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
};

// "ICFT" — v3: per-dir leaf flag. Only leaf dirs may be skipped on
// rescan: a dir's mtime does NOT change when files inside a
// *subdirectory* change, so pruning a non-leaf dir would hide nested
// updates. Records also store the folded signature directly — saving
// needs no per-file stat (the manifest already has it).
const MAGIC: u32 = 0x54_46_43_49;
const PACK_FILE: &str = "files.pack";

#[derive(Debug, Clone)]
pub struct FileRec {
    pub name: String,
    pub len: u64,
    /// Folded content signature — the same value
    /// `previews::preview_sig_u64` returns for the unchanged file, stored
    /// directly so no per-file stat is needed at save time.
    pub sig: u64,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Default)]
pub struct DirRec {
    pub mtime: u128,
    pub files: Vec<FileRec>,
}

pub fn pack_path(root: &Path) -> PathBuf {
    root.join(super::PREVIEWS_DIR).join(PACK_FILE)
}

fn dir_mtime(p: &Path) -> u128 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Does `dir` contain no subdirectories? Only leaf dirs are safe to skip
/// on rescan — see the MAGIC comment. Read errors count as non-leaf
/// (never skipped).
fn dir_is_leaf(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in entries.flatten() {
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            return false;
        }
    }
    true
}

/// Save the scan result: `files` = (dir, name, len, sig, w, h) grouped by
/// parent dir; dir mtimes are stat'd once per dir, in parallel.
pub fn save(root: &Path, files: &[(PathBuf, String, u64, u64, u32, u32)]) {
    let mut dirs: HashMap<PathBuf, Vec<&(PathBuf, String, u64, u64, u32, u32)>> =
        HashMap::new();
    for f in files {
        dirs.entry(f.0.clone()).or_default().push(f);
    }
    // One stat + one read_dir per dir — parallel so deep trees don't
    // serialize on disk.
    let dir_vec: Vec<(PathBuf, Vec<&(PathBuf, String, u64, u64, u32, u32)>)> =
        dirs.into_iter().collect();
    let info = super::par_map(&dir_vec, |(d, _)| (dir_mtime(d), dir_is_leaf(d)));
    let mut buf = Vec::with_capacity(8 + files.len() * 64);
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&(dir_vec.len() as u32).to_le_bytes());
    for ((dir, recs), (mtime, leaf)) in dir_vec.iter().zip(info) {
        let ds = dir.to_string_lossy();
        buf.extend_from_slice(&(ds.len() as u16).to_le_bytes());
        buf.extend_from_slice(ds.as_bytes());
        buf.extend_from_slice(&mtime.to_le_bytes());
        buf.push(u8::from(leaf));
        buf.extend_from_slice(&(recs.len() as u32).to_le_bytes());
        for (_, name, len, sig, w, h) in recs {
            buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&sig.to_le_bytes());
            buf.extend_from_slice(&w.to_le_bytes());
            buf.extend_from_slice(&h.to_le_bytes());
        }
    }
    let target = pack_path(root);
    if std::fs::create_dir_all(target.parent().unwrap()).is_err() {
        return;
    }
    let tmp = target.with_extension("tmp");
    if std::fs::write(&tmp, &buf).is_ok() && std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Load and validate: returns (unchanged dirs → their file records for
/// instant seeds, dirs the walker may prune entirely). `recursive` selects
/// the safe skip set: non-recursive scans only ever read the root's direct
/// children, so any unchanged dir record for the root itself is skippable;
/// recursive scans may only prune *leaf* dirs — a non-leaf dir's mtime
/// says nothing about files inside its subdirectories.
pub fn load(
    root: &Path,
    recursive: bool,
) -> Option<(HashMap<PathBuf, DirRec>, std::collections::HashSet<PathBuf>)> {
    let buf = {
        let mut b = Vec::new();
        std::fs::File::open(pack_path(root)).ok()?.read_to_end(&mut b).ok()?;
        b
    };
    let mut pos = 0usize;
    macro_rules! num {
        ($n:expr) => {{
            let b = buf.get(pos..pos + $n)?;
            pos += $n;
            let mut le = [0u8; 16];
            le[..$n].copy_from_slice(b);
            u128::from_le_bytes(le)
        }};
    }
    macro_rules! take {
        ($n:expr) => {{
            let h = buf.get(pos..pos + $n)?;
            pos += $n;
            h
        }};
    }
    if num!(4) != MAGIC as u128 {
        return None;
    }
    let dir_count = num!(4) as usize;
    // Parse the whole pack first, then stat every recorded dir in
    // parallel — serial per-dir stats dominate on HDDs/remote mounts.
    let mut parsed: Vec<(PathBuf, u128, bool, Vec<FileRec>)> =
        Vec::with_capacity(dir_count);
    for _ in 0..dir_count {
        let dlen = num!(2) as usize;
        let dir = PathBuf::from(String::from_utf8_lossy(take!(dlen)).into_owned());
        let mtime = num!(16);
        let leaf = num!(1) != 0;
        let fcount = num!(4) as usize;
        let mut files = Vec::with_capacity(fcount);
        for _ in 0..fcount {
            let nlen = num!(2) as usize;
            let name = String::from_utf8_lossy(take!(nlen)).into_owned();
            let len = num!(8) as u64;
            let sig = num!(8) as u64;
            let w = num!(4) as u32;
            let h = num!(4) as u32;
            files.push(FileRec { name, len, sig, w, h });
        }
        parsed.push((dir, mtime, leaf, files));
    }
    let mtimes = super::par_map(&parsed, |(d, _, _, _)| dir_mtime(d));
    let mut out = HashMap::new();
    let mut skip = std::collections::HashSet::new();
    for ((dir, mtime, leaf, files), cur) in parsed.into_iter().zip(mtimes) {
        // Only dirs whose mtime is unchanged seed/skip at all. A
        // non-recursive scan reads only the root's direct children —
        // records for nested dirs would leak subdirectory files into a
        // flat listing, so they are ignored entirely.
        if cur != mtime || (!recursive && dir.as_path() != root) {
            continue;
        }
        // Non-leaf dirs still seed their direct files (the walker derives
        // the seeded-parent set from the events and suppresses the
        // duplicate Found), but must NOT be pruned: their subdirs may
        // have changed without bumping this dir's mtime.
        let skippable = !recursive || leaf;
        out.insert(dir.clone(), DirRec { mtime, files });
        if skippable {
            skip.insert(dir);
        }
    }
    Some((out, skip))
}
