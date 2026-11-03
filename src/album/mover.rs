use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

use anyhow::Result;

use crate::{album::thumbnails::remove_previews_for, fs_utils::move_with_unique_name};

/// Outcome of one queued move: number of bytes that were moved.
pub struct MoveResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves selected images to the `-m/--move` directory
/// (same non-destructive move semantics as trash) and removes their cached
/// previews next to the source.
pub struct Mover {
    job_tx: Sender<PathBuf>,
    result_rx: Receiver<MoveResult>,
    pub pending: HashSet<PathBuf>,
}

impl Mover {
    pub fn new(concurrency: usize, dest_dir: PathBuf) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<MoveResult>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..concurrency.max(1).min(4) {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let dest_dir = dest_dir.clone();
            thread::spawn(move || loop {
                let path = {
                    let Ok(rx) = job_rx.lock() else { break };
                    match rx.recv() {
                        Ok(p) => p,
                        Err(_) => break,
                    }
                };
                if result_tx
                    .send(MoveResult {
                        result: move_one(&path, &dest_dir),
                        path,
                    })
                    .is_err()
                {
                    break;
                }
            });
        }

        Self {
            job_tx,
            result_rx,
            pending: HashSet::new(),
        }
    }

    /// Queue a file for moving; deduplicates in-flight jobs.
    pub fn queue(&mut self, path: PathBuf) {
        if self.pending.contains(&path) {
            return;
        }
        self.pending.insert(path.clone());
        if self.job_tx.send(path).is_err() {
            self.pending.clear();
        }
    }

    /// Drain finished moves, updating the pending set.
    pub fn poll(&mut self) -> Vec<MoveResult> {
        let mut done = Vec::new();
        while let Ok(result) = self.result_rx.try_recv() {
            self.pending.remove(&result.path);
            done.push(result);
        }
        done
    }
}

fn move_one(path: &std::path::Path, dest_dir: &std::path::Path) -> Result<u64> {
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    move_with_unique_name(path, dest_dir)?;
    remove_previews_for(path);
    Ok(bytes)
}
