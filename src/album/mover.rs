use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;

use crate::fs_utils::{
    adaptive_pool::AdaptivePool, move_with_unique_name, previews::remove_previews_for_many,
};

/// Outcome of one queued move: number of bytes that were moved.
pub struct MoveResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves selected images to the `-m/--move` directory
/// (same non-destructive move semantics as trash) and removes their cached
/// previews next to the source.
///
/// Concurrency self-tunes like the [`Deleter`](crate::album::deleter::Deleter):
/// it starts at the ceiling and probes down, so fast storage gets full-width
/// parallelism immediately while HDD/remote mounts settle lower.
pub struct Mover {
    pool: AdaptivePool<Result<u64>>,
    pub pending: HashSet<PathBuf>,
    /// Successfully moved paths whose cached previews still need purging —
    /// batched until the queue drains (one scan per previews dir).
    cleanup: Vec<PathBuf>,
}

impl Mover {
    /// `concurrency` is the parallelism ceiling (e.g. `max(32, -j)`).
    pub fn new(concurrency: usize, dest_dir: PathBuf) -> Self {
        let concurrency = concurrency.max(1);
        Self {
            pool: AdaptivePool::new(concurrency, concurrency, move |path| {
                move_one(path, &dest_dir)
            }),
            pending: HashSet::new(),
            cleanup: Vec::new(),
        }
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
        let mut done = Vec::new();
        for (path, result) in self.pool.poll() {
            self.pending.remove(&path);
            if result.is_ok() {
                self.cleanup.push(path.clone());
            }
            done.push(MoveResult { path, result });
        }
        if self.pending.is_empty() && !self.cleanup.is_empty() {
            remove_previews_for_many(self.cleanup.iter().map(PathBuf::as_path));
            self.cleanup.clear();
        }
        done
    }
}

fn move_one(path: &std::path::Path, dest_dir: &std::path::Path) -> Result<u64> {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    move_with_unique_name(path, dest_dir)?;
    Ok(bytes)
}
