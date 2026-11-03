use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;

use crate::{
    album::thumbnails::remove_previews_for,
    fs_utils::{adaptive_pool::AdaptivePool, move_with_unique_name, prepare_dir, TRASH_DIR},
};

/// Outcome of one queued deletion: number of bytes that were trashed.
pub struct DeleteResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves deleted images to `.imagecropper-trash`
/// (same semantics as the crop UI) and removes their cached previews.
///
/// Worker count is pinned each frame via [`Deleter::set_workers`] to the
/// preview pool's autotuned concurrency — deletions fan out across the
/// same storage-optimal parallelism instead of a fixed thread count.
pub struct Deleter {
    pool: AdaptivePool<Result<u64>>,
    pub pending: HashSet<PathBuf>,
}

impl Deleter {
    pub fn new(concurrency: usize) -> Self {
        Self {
            pool: AdaptivePool::new(1, concurrency.max(1), |path| delete_one(path)),
            pending: HashSet::new(),
        }
    }

    /// Track another pool's live worker count (e.g. the thumbnail pool).
    pub fn set_workers(&self, n: usize) {
        self.pool.set_worker_override(n.max(1));
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
        self.pool
            .poll()
            .into_iter()
            .map(|(path, result)| {
                self.pending.remove(&path);
                DeleteResult { path, result }
            })
            .collect()
    }
}

fn delete_one(path: &std::path::Path) -> Result<u64> {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let trash = prepare_dir(parent, TRASH_DIR)?;
    move_with_unique_name(path, &trash)?;
    remove_previews_for(path);
    Ok(bytes)
}
