//! `files.pack` — binary snapshot of the last scan's file list, stored in
//! `.imagecropper-thumbnails/` next to each scan root. On the next run the
//! entries seed the manifest instantly (no walk, no header sniffing), and
//! directories whose mtime is unchanged are skipped by the walker entirely
//! — so a re-scan only stats dir/file mtimes instead of enumerating and
//! re-parsing everything.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAGIC: u32 = 0x4C_46_43_49; // "ICFL"
const PACK_DIR: &str = ".imagecropper-thumbnails";
const PACK_FILE: &str = "files.pack";

#[derive(Debug, Clone)]
pub struct FileRec {
    pub name: String,
    pub len: u64,
    pub mtime: u128,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Default)]
pub struct DirRec {
    pub mtime: u128,
    pub files: Vec<FileRec>,
}

pub fn pack_path(root: &Path) -> PathBuf {
    root.join(PACK_DIR).join(PACK_FILE)
}

fn dir_mtime(p: &Path) -> u128 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Save the scan result: `files` = (dir, name, len, mtime, w, h) grouped by
/// parent dir; dir mtimes are stat'd now (once per dir).
pub fn save(root: &Path, files: &[(PathBuf, String, u64, u128, u32, u32)]) {
    let mut dirs: HashMap<PathBuf, Vec<&(PathBuf, String, u64, u128, u32, u32)>> = HashMap::new();
    for f in files {
        dirs.entry(f.0.clone()).or_default().push(f);
    }
    let mut buf = Vec::with_capacity(8 + files.len() * 64);
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&(dirs.len() as u32).to_le_bytes());
    for (dir, recs) in &dirs {
        let ds = dir.to_string_lossy();
        buf.extend_from_slice(&(ds.len() as u16).to_le_bytes());
        buf.extend_from_slice(ds.as_bytes());
        buf.extend_from_slice(&dir_mtime(dir).to_le_bytes());
        buf.extend_from_slice(&(recs.len() as u32).to_le_bytes());
        for (_, name, len, mtime, w, h) in recs {
            buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&mtime.to_le_bytes());
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

/// Load and validate: returns (dirs unchanged since last run → their file
/// records, unchanged dir paths for the walker to skip).
pub fn load(root: &Path) -> Option<(HashMap<PathBuf, DirRec>, std::collections::HashSet<PathBuf>)> {
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
    let mut out = HashMap::new();
    let mut skip = std::collections::HashSet::new();
    for _ in 0..dir_count {
        let dlen = num!(2) as usize;
        let dir = PathBuf::from(String::from_utf8_lossy(take!(dlen)).into_owned());
        let mtime = num!(16);
        let fcount = num!(4) as usize;
        let mut files = Vec::with_capacity(fcount);
        for _ in 0..fcount {
            let nlen = num!(2) as usize;
            let name = String::from_utf8_lossy(take!(nlen)).into_owned();
            let len = num!(8) as u64;
            let mt = num!(16);
            let w = num!(4) as u32;
            let h = num!(4) as u32;
            files.push(FileRec { name, len, mtime: mt, w, h });
        }
        // Only dirs whose mtime is unchanged skip the walk.
        if dir_mtime(&dir) == mtime {
            skip.insert(dir.clone());
            out.insert(dir, DirRec { mtime, files });
        }
    }
    Some((out, skip))
}
