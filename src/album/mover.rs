use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;

use crate::{
    album::thumbnails::remove_previews_for,
    fs_utils::{adaptive_pool::AdaptivePool, move_with_unique_name},
};

/// Outcome of one queued move: number of bytes that were moved.
pub struct MoveResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves selected images to the `-m/--move` directory
/// (same non-destructive move semantics as trash) and removes their cached
/// previews next to the source. Worker count tracks the preview pool (see
/// [`Deleter`](crate::album::deleter::Deleter)).
pub struct Mover {
    pool: AdaptivePool<Result<u64>>,
    pub pending: HashSet<PathBuf>,
}

impl Mover {
    pub fn new(concurrency: usize, dest_dir: PathBuf) -> Self {
        Self {
            pool: AdaptivePool::new(1, concurrency.max(1), move |path| move_one(path, &dest_dir)),
            pending: HashSet::new(),
        }
    }

    /// Track another pool's live worker count (e.g. the thumbnail pool).
    pub fn set_workers(&self, n: usize) {
        self.pool.set_worker_override(n.max(1));
    }

    /// Queue a file for moving; deduplicates in-flight jobs.
    pub fn queue(&mut self, path: PathBuf) {
        if !self.pending.insert(path.clone()) {
            return;
        }
        self.pool.request(path);
    }

    /// Drain finished moves, updating the pending set.
    pub fn poll(&mut self) -> Vec<MoveResult> {
        self.pool
            .poll()
            .into_iter()
            .map(|(path, result)| {
                self.pending.remove(&path);
                MoveResult { path, result }
            })
            .collect()
    }
}

fn move_one(path: &std::path::Path, dest_dir: &std::path::Path) -> Result<u64> {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    move_with_unique_name(path, dest_dir)?;
    remove_previews_for(path);
    Ok(bytes)
}
