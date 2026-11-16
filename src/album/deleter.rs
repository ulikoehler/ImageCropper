use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;

use crate::fs_utils::{
    adaptive_pool::AdaptivePool, move_with_unique_name, prepare_dir,
    previews::remove_previews_for_many, DELETE_WORKERS, TRASH_DIR,
};

/// Outcome of one queued deletion: number of bytes that were trashed.
pub struct DeleteResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves deleted images to `.imagecropper-trash`
/// (same semantics as the crop UI) — or unlinks them under
/// `--force-delete` — and removes their cached previews.
///
/// Both modes run on a fixed pool of [`DELETE_WORKERS`] threads: delete
/// throughput is bound by the filesystem, not decode CPU, and a fixed
/// count avoids autotuning flapping on slow mounts.
pub struct Deleter {
    pool: AdaptivePool<Result<u64>>,
    pub pending: HashSet<PathBuf>,
    /// Successfully trashed paths whose cached previews still need purging.
    /// Batched until the queue drains so a bulk delete scans each shared
    /// `.imagecropper-previews` directory once instead of once per file.
    cleanup: Vec<PathBuf>,
}

impl Deleter {
    /// `force_delete` unlinks instead of trashing. `concurrency` is
    /// accepted for API compatibility — the pool is always exactly
    /// [`DELETE_WORKERS`] threads.
    pub fn new(_concurrency: usize, force_delete: bool) -> Self {
        let work: fn(&std::path::Path) -> Result<u64> =
            if force_delete { force_delete_one } else { delete_one };
        let pool = AdaptivePool::new(DELETE_WORKERS, DELETE_WORKERS, work);
        pool.set_worker_override(DELETE_WORKERS);
        Self {
            pool,
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

/// `--force-delete`: unlink permanently — no trash directory involved.
fn force_delete_one(path: &std::path::Path) -> Result<u64> {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    std::fs::remove_file(path)?;
    Ok(bytes)
}
