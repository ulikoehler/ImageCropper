use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;

use crate::fs_utils::{
    adaptive_pool::AdaptivePool, move_with_unique_name, prepare_dir,
    previews::remove_previews_for_many, TRASH_DIR,
};

/// Outcome of one queued deletion: number of bytes that were trashed.
pub struct DeleteResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves deleted images to `.imagecropper-trash`
/// (same semantics as the crop UI) and removes their cached previews.
///
/// Concurrency self-tunes to the storage device like the preview pools, but
/// with a much higher ceiling: trashing is metadata-only, so fast storage
/// benefits from many parallel renames. It starts at the ceiling and probes
/// down, so a bulk delete runs at full width immediately on SSD/NVMe while
/// HDD/remote mounts settle to a lower count.
pub struct Deleter {
    pool: AdaptivePool<Result<u64>>,
    pub pending: HashSet<PathBuf>,
    /// Successfully trashed paths whose cached previews still need purging.
    /// Batched until the queue drains so a bulk delete scans each shared
    /// `.imagecropper-previews` directory once instead of once per file.
    cleanup: Vec<PathBuf>,
}

impl Deleter {
    /// `concurrency` is the parallelism ceiling (e.g. `max(32, -j)`).
    pub fn new(concurrency: usize) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            pool: AdaptivePool::new(concurrency, concurrency, delete_one),
            pending: HashSet::new(),
            cleanup: Vec::new(),
        }
    }

    /// Queue a file for deletion; deduplicates in-flight jobs.
    pub fn queue(&mut self, path: PathBuf) {
        if !self.pending.insert(path.clone()) {
            return;
        }
        self.pool.request(path);
    }

    /// Drain finished deletions, updating the pending set.
    pub fn poll(&mut self) -> Vec<DeleteResult> {
        let mut done = Vec::new();
        for (path, result) in self.pool.poll() {
            self.pending.remove(&path);
            if result.is_ok() {
                self.cleanup.push(path.clone());
            }
            done.push(DeleteResult { path, result });
        }
        if self.pending.is_empty() && !self.cleanup.is_empty() {
            remove_previews_for_many(self.cleanup.iter().map(PathBuf::as_path));
            self.cleanup.clear();
        }
        done
    }
}

fn delete_one(path: &std::path::Path) -> Result<u64> {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let trash = prepare_dir(parent, TRASH_DIR)?;
    move_with_unique_name(path, &trash)?;
    Ok(bytes)
}
