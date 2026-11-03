//! Manifest: the server's list of served files, serialized with bincode over
//! the /bulk channel. Versions bump on rescan; clients resume via patches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::fs_utils::previews::preview_sig_u64;

/// u64 signatures cross into JS-land — serialize as strings to survive
/// 2^53 precision limits, accept either form when reading back.
pub fn sig_as_str<S: Serializer>(sig: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&sig.to_string())
}

pub fn sig_from_any<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Sig {
        N(u64),
        S(String),
    }
    match Sig::deserialize(d)? {
        Sig::N(n) => Ok(n),
        Sig::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServeMode {
    Album,
    Crop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestItem {
    pub id: u32,
    /// File name only; parent dir via `dir_index` into `dirs`.
    pub name: String,
    pub dir_index: u32,
    /// Natural image dimensions (0x0 if undetermined).
    pub w: u32,
    pub h: u32,
    pub bytes: u64,
    /// Content signature — same scheme as preview filenames.
    #[serde(serialize_with = "sig_as_str", deserialize_with = "sig_from_any")]
    pub sig: u64,
    /// Tombstone: removed server-side but the id stays positional so every
    /// client's `items[id]` indexing stays consistent. Omitted when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub removed: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub mode: ServeMode,
    pub generated_ms: u64,
    /// Unique parent directories; `items[].dir_index` refers into this.
    pub dirs: Vec<String>,
    /// Scan roots as passed on the command line.
    pub roots: Vec<String>,
    pub items: Vec<ManifestItem>,
    /// Server-side lookup: absolute path → item id. Not serialized.
    #[serde(skip)]
    pub positions: HashMap<PathBuf, u32>,
}

/// Incremental change to a manifest: replace/insert `upsert`, remove `remove`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestPatch {
    pub version: u32,
    /// Full replacement: clients clear `items`/`dirs` before applying the
    /// upserts (used when ids were reassigned wholesale, e.g. final sort
    /// after a streaming scan or an fs-watch rescan).
    #[serde(default, skip_serializing_if = "is_false")]
    pub resync: bool,
    pub upsert: Vec<ManifestItem>,
    pub remove: Vec<u32>,
    /// Added since `version-1`: new dirs appended to the interned list.
    /// For `resync` patches this is the complete dir list instead.
    pub dirs_added: Vec<String>,
}

impl Manifest {
    /// Build a manifest from a sorted file list. Header-sniffs dimensions in
    /// parallel-friendly order (cheap: metadata read only).
    pub fn build(files: &[PathBuf], roots: &[PathBuf], mode: ServeMode, version: u32) -> Self {
        let mut dir_index: HashMap<PathBuf, u32> = HashMap::new();
        let mut dirs: Vec<String> = Vec::new();
        let mut items = Vec::with_capacity(files.len());

        for (i, path) in files.iter().enumerate() {
            let parent = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let idx = *dir_index.entry(parent.clone()).or_insert_with(|| {
                dirs.push(parent.to_string_lossy().into_owned());
                (dirs.len() - 1) as u32
            });
            let (w, h) = image::image_dimensions(path).unwrap_or((0, 0));
            let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let sig = preview_sig_u64(path).unwrap_or(0);
            items.push(ManifestItem {
                id: i as u32,
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                dir_index: idx,
                w,
                h,
                bytes,
                sig,
                removed: false,
            });
        }

        let positions = files
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i as u32))
            .collect();
        Manifest {
            version,
            mode,
            generated_ms: now_ms(),
            dirs,
            roots: roots
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            items,
            positions,
        }
    }

    /// Item id for an absolute path (inverse of [`Self::path_of`]).
    pub fn position_of(&self, path: &Path) -> Option<u32> {
        self.positions.get(path).copied()
    }

    /// Rebuild `positions` after items changed in place (skips tombstones).
    pub fn rebuild_positions(&mut self) {
        let positions = (0..self.items.len())
            .filter(|&i| !self.items[i].removed)
            .filter_map(|i| self.path_of(i as u32).map(|p| (p, i as u32)))
            .collect();
        self.positions = positions;
    }

    /// Absolute path for an item (dirs interned, names are file names only).
    /// `None` for tombstoned items.
    pub fn path_of(&self, id: u32) -> Option<PathBuf> {
        let item = self.items.get(id as usize)?;
        if item.removed {
            return None;
        }
        let dir = self.dirs.get(item.dir_index as usize)?;
        Some(Path::new(dir).join(&item.name))
    }

    /// Apply a patch in place (used by the client side too). Upserts replace
    /// the item at its id; removals are tombstones, never compactions —
    /// `items[id]` indexing stays positional for every client.
    pub fn apply_patch(&mut self, patch: &ManifestPatch) {
        self.dirs.extend(patch.dirs_added.iter().cloned());
        for up in &patch.upsert {
            if (up.id as usize) < self.items.len() {
                self.items[up.id as usize] = up.clone();
            } else {
                self.items.push(up.clone());
            }
        }
        for &id in &patch.remove {
            if let Some(item) = self.items.get_mut(id as usize) {
                item.removed = true;
            }
        }
        self.version = patch.version;
        self.rebuild_positions();
    }

    /// A `resync` patch carrying the entire manifest — used after a full
    /// rebuild so clients replace their item list without a reconnect or
    /// manifest re-pull.
    pub fn full_patch(&self) -> ManifestPatch {
        ManifestPatch {
            version: self.version,
            resync: true,
            upsert: self.items.clone(),
            remove: Vec::new(),
            dirs_added: self.dirs.clone(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
