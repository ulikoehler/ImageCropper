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

use crate::{
    album::thumbnails::remove_previews_for,
    fs_utils::{move_with_unique_name, prepare_dir, TRASH_DIR},
};

/// Outcome of one queued deletion: number of bytes that were trashed.
pub struct DeleteResult {
    pub path: PathBuf,
    pub result: Result<u64>,
}

/// Background queue that moves deleted images to `.imagecropper-trash`
/// (same semantics as the crop UI) and removes their cached previews.
pub struct Deleter {
    job_tx: Sender<PathBuf>,
    result_rx: Receiver<DeleteResult>,
    pub pending: HashSet<PathBuf>,
}

impl Deleter {
    pub fn new(concurrency: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<DeleteResult>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..concurrency.max(1).min(4) {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            thread::spawn(move || loop {
                let path = {
                    let Ok(rx) = job_rx.lock() else { break };
                    match rx.recv() {
                        Ok(p) => p,
                        Err(_) => break,
                    }
                };
                if result_tx
                    .send(DeleteResult {
                        result: delete_one(&path),
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

    /// Queue a file for deletion; deduplicates in-flight jobs.
    pub fn queue(&mut self, path: PathBuf) {
        if self.pending.contains(&path) {
            return;
        }
        self.pending.insert(path.clone());
        if self.job_tx.send(path).is_err() {
            self.pending.clear();
        }
    }

    /// Drain finished deletions, updating the pending set.
    pub fn poll(&mut self) -> Vec<DeleteResult> {
        let mut done = Vec::new();
        while let Ok(result) = self.result_rx.try_recv() {
            self.pending.remove(&result.path);
            done.push(result);
        }
        done
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
