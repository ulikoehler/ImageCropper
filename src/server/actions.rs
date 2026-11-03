//! ActionQueue: signature-checked destructive operations (delete, crop-save,
//! resave). Crop/resave run on a serial worker (the `Saver` is not `Sync`);
//! delete/move fan out on a parallel pool. Results broadcast to all clients.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::app::saver::Saver;
use crate::fs_utils::previews::{preview_sig_u64, remove_previews_for, remove_previews_for_many};
use crate::fs_utils::{move_with_unique_name, prepare_dir, TRASH_DIR};
use crate::image_utils::{build_output_image, decode_image_bytes, OutputFormat, SaveRequest};
use crate::selection::Selection;
use crate::server::io_gate::IoGate;

/// Rotation applied to a crop save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RotateOp {
    None,
    Cw90,
    Ccw90,
    Rot180,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    Delete {
        id: u32,
        #[serde(deserialize_with = "super::manifest::sig_from_any")]
        sig: u64,
    },
    /// Move to the server's `-m/--move` directory.
    Move {
        id: u32,
        #[serde(deserialize_with = "super::manifest::sig_from_any")]
        sig: u64,
    },
    /// Crop rects are image-space [x, y, w, h] in natural coordinates.
    Crop {
        id: u32,
        #[serde(deserialize_with = "super::manifest::sig_from_any")]
        sig: u64,
        sel: Vec<[u32; 4]>,
        rotate: RotateOp,
        format: OutputFormat,
        quality: u8,
    },
    Resave {
        id: u32,
        #[serde(deserialize_with = "super::manifest::sig_from_any")]
        sig: u64,
        format: OutputFormat,
        quality: u8,
    },
}

impl Action {
    pub fn id(&self) -> u32 {
        match self {
            Action::Delete { id, .. }
            | Action::Move { id, .. }
            | Action::Crop { id, .. }
            | Action::Resave { id, .. } => *id,
        }
    }

    pub fn kind(&self) -> ActionKind {
        match self {
            Action::Delete { .. } => ActionKind::Delete,
            Action::Move { .. } => ActionKind::Move,
            Action::Crop { .. } => ActionKind::Crop,
            Action::Resave { .. } => ActionKind::Resave,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ActionKind {
    Delete,
    Move,
    Crop,
    Resave,
}

/// Server → client result for an action (sent as an EVENT on /data).
#[derive(Debug, Clone, Serialize)]
pub struct ActionDone {
    pub tag: u32,
    pub ok: bool,
    /// Manifest id + kind — lets the server apply differential updates.
    pub id: u32,
    #[serde(rename = "kind")]
    pub kind: ActionKind,
    /// Source path (server-internal; not sent to clients).
    #[serde(skip)]
    pub src: PathBuf,
    /// Output path for crop/resave (same file for crop; new ext for resave).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For crop/resave: the new file's sig; for delete: trashed bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_sig: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// Look up an id → (path, sig) and verify the on-disk signature matches what
/// the client saw. Guards against two clients acting on a changed file.
pub fn verify_sig(path: &Path, expected: u64) -> Result<()> {
    let actual = preview_sig_u64(path).ok_or_else(|| anyhow!("file no longer readable"))?;
    if actual != expected {
        return Err(anyhow!("stale signature: file changed on disk"));
    }
    Ok(())
}

/// Execute one action. `path` is the manifest-resolved source path.
/// `move_dir` is the `-m/--move` destination; required for `Action::Move`.
pub fn execute(
    action: &Action,
    path: &Path,
    saver: &mut Saver,
    move_dir: Option<&Path>,
) -> Result<(Option<u64>, Option<u64>)> {
    match action {
        Action::Delete { sig, .. } => {
            let bytes = trash_one(path, *sig)?;
            remove_previews_for(path);
            Ok((None, Some(bytes)))
        }
        Action::Move { sig, .. } => {
            let bytes = move_one(path, *sig, move_dir)?;
            remove_previews_for(path);
            Ok((None, Some(bytes)))
        }
        Action::Crop {
            sig,
            sel,
            rotate,
            format,
            quality,
            ..
        } => {
            verify_sig(path, *sig)?;
            let bytes = std::fs::read(path)?;
            let image = decode_image_bytes(path, &bytes)?;
            let (w, h) = (image.width() as f32, image.height() as f32);
            let selections: Vec<Selection> = sel
                .iter()
                .map(|[x, y, sw, sh]| Selection {
                    rect: egui::Rect::from_min_size(
                        egui::pos2(*x as f32, *y as f32),
                        egui::vec2(*sw as f32, *sh as f32),
                    ),
                })
                .collect();
            let mut out = build_output_image(&image, &selections)
                .ok_or_else(|| anyhow!("empty crop result"))?;
            out = match rotate {
                RotateOp::None => out,
                RotateOp::Cw90 => out.rotate90(),
                RotateOp::Ccw90 => out.rotate270(),
                RotateOp::Rot180 => out.rotate180(),
            };
            let _ = (w, h);
            saver.queue_save(SaveRequest {
                image: out,
                path: path.to_path_buf(),
                original_path: path.to_path_buf(),
                quality: *quality,
                format: *format,
            })?;
            // New sig is produced asynchronously by the Saver; caller polls
            // completion and emits it.
            Ok((None, None))
        }
        Action::Resave {
            sig,
            format,
            quality,
            ..
        } => {
            verify_sig(path, *sig)?;
            let bytes = std::fs::read(path)?;
            let image = decode_image_bytes(path, &bytes)?;
            let new_path = path.with_extension(format.extension());
            saver.queue_save(SaveRequest {
                image,
                path: new_path.clone(),
                original_path: path.to_path_buf(),
                quality: *quality,
                format: *format,
            })?;
            let _ = new_path;
            Ok((None, None))
        }
    }
}

/// Move `path` into the trash directory next to it. Returns bytes trashed.
fn trash_one(path: &Path, sig: u64) -> Result<u64> {
    verify_sig(path, sig)?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let trash = prepare_dir(parent, TRASH_DIR)?;
    move_with_unique_name(path, &trash)?;
    Ok(bytes)
}

/// Move `path` into `move_dir` (unique name on collision). Returns bytes moved.
fn move_one(path: &Path, sig: u64, move_dir: Option<&Path>) -> Result<u64> {
    let dest = move_dir.ok_or_else(|| anyhow!("no -m/--move directory configured"))?;
    verify_sig(path, sig)?;
    if dest
        .canonicalize()
        .ok()
        .zip(
            path.canonicalize()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf())),
        )
        .map(|(a, b)| a == b)
        .unwrap_or(false)
    {
        return Err(anyhow!("file is already in the move directory"));
    }
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    move_with_unique_name(path, dest)?;
    Ok(bytes)
}

/// Serialized ACTION payload wrapper: tag is the client's correlation id.
#[derive(Debug, Deserialize)]
pub struct ActionEnvelope {
    pub tag: u32,
    #[serde(flatten)]
    pub action: Action,
}

/// One queued delete/move. Unlike crop/resave these never touch the
/// `Saver`, so they run on a parallel pool instead of the serial thread.
struct DestructiveJob {
    tag: u32,
    id: u32,
    kind: ActionKind,
    path: PathBuf,
    sig: u64,
}

/// Bounded worker pool for delete/move actions. A bounded pool keeps a
/// client from spawning one thread per request while still letting a bulk
/// delete fan out.
struct DestructivePool {
    job_tx: Sender<DestructiveJob>,
    result_rx: Mutex<Receiver<ActionDone>>,
}

impl DestructivePool {
    fn new(workers: usize, move_dir: Option<PathBuf>, gate: Arc<IoGate>) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<DestructiveJob>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, result_rx) = mpsc::channel::<ActionDone>();
        for _ in 0..workers.max(1) {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let move_dir = move_dir.clone();
            let gate = Arc::clone(&gate);
            thread::spawn(move || loop {
                let job = {
                    let Ok(rx) = job_rx.lock() else { return };
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                };
                // Priority work first: while the web client is waiting for
                // images — or while the early scan is still indexing — the
                // delete/move workers hold off so they don't compete for IO.
                // The wait is bounded, so the queue always drains.
                gate.yield_to_priority(crate::server::io_gate::SCAN_HOLD_PAUSE);
                // A panic must still produce an ActionDone — otherwise the
                // job vanishes with the path stuck in `destructive` and the
                // client's counter never drains. The worker also survives
                // to take the next job.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match job.kind {
                        ActionKind::Delete => trash_one(&job.path, job.sig),
                        ActionKind::Move => move_one(&job.path, job.sig, move_dir.as_deref()),
                        ActionKind::Crop | ActionKind::Resave => unreachable!(),
                    }
                }))
                .unwrap_or_else(|_| Err(anyhow!("worker panicked")));
                let done = match result {
                    Ok(bytes) => ActionDone {
                        tag: job.tag,
                        ok: true,
                        id: job.id,
                        kind: job.kind,
                        src: job.path,
                        new_path: None,
                        error: None,
                        new_sig: None,
                        bytes: Some(bytes),
                    },
                    Err(e) => ActionDone {
                        tag: job.tag,
                        ok: false,
                        id: job.id,
                        kind: job.kind,
                        src: job.path,
                        new_path: None,
                        error: Some(e.to_string()),
                        new_sig: None,
                        bytes: None,
                    },
                };
                if result_tx.send(done).is_err() {
                    return;
                }
            });
        }
        Self {
            job_tx,
            result_rx: Mutex::new(result_rx),
        }
    }

    fn dispatch(&self, job: DestructiveJob) -> bool {
        self.job_tx.send(job).is_ok()
    }

    fn poll(&self) -> Vec<ActionDone> {
        let rx = self.result_rx.lock().unwrap();
        let mut out = Vec::new();
        while let Ok(done) = rx.try_recv() {
            out.push(done);
        }
        out
    }
}

/// Background worker executing crop/resave serially (Saver is not Sync).
/// Delete/move bypass it on [`DestructivePool`] — they never touch the Saver.
/// Jobs carry the client's tag so completions can be correlated.
/// Dispatched-but-unfinished actions are tracked by path/tag so shutdown can
/// drain them; they are never timed out — a queued delete is finished even if
/// it is slow.
pub struct ActionQueue {
    job_tx: Sender<(u32, Action, PathBuf)>,
    result_rx: Mutex<Receiver<ActionDone>>,
    destructive_pool: DestructivePool,
    /// Duplicate destructive dispatches, answered immediately without
    /// touching the pending set (the original op is still in flight).
    immediate: Mutex<Vec<ActionDone>>,
    /// Delete/move ops queued for a path. While a path is in this set it is
    /// "marked for deletion": manifest rebuilds and upserts skip it so a
    /// rescan or client reload cannot resurrect it before the op lands.
    destructive: Mutex<HashSet<PathBuf>>,
    /// Crop/resave ops, keyed by client tag.
    serial: Mutex<HashSet<u32>>,
    /// Successfully deleted/moved paths whose cached previews still need
    /// purging — batched until the queue drains.
    cleanup: Mutex<Vec<PathBuf>>,
    /// Client-first IO priority, shared with preview production.
    gate: Arc<IoGate>,
}

impl ActionQueue {
    /// `delete_workers` bounds how many deletes/moves run at once.
    pub fn new(saver_threads: usize, delete_workers: usize, move_dir: Option<PathBuf>) -> Self {
        Self::with_gate(saver_threads, delete_workers, move_dir, IoGate::new())
    }

    /// Same as [`ActionQueue::new`] with the server's shared IO gate so
    /// deletes can yield to client requests.
    pub fn with_gate(
        saver_threads: usize,
        delete_workers: usize,
        move_dir: Option<PathBuf>,
        gate: Arc<IoGate>,
    ) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<(u32, Action, PathBuf)>();
        let (result_tx, result_rx) = mpsc::channel::<ActionDone>();
        let serial_move_dir = move_dir.clone();
        let serial_gate = Arc::clone(&gate);
        thread::spawn(move || {
            let mut saver = Saver::new(saver_threads.max(1));
            // Async saves awaiting completion: (tag, kind, id, src, out).
            let mut pending: Vec<(u32, ActionKind, u32, PathBuf, PathBuf)> = Vec::new();
            loop {
                // Drain completed saves → ActionDone with the new sig.
                for (path, result, _sizes) in saver.check_completions() {
                    if let Some(pos) = pending.iter().position(|(.., p)| p == &path) {
                        let (tag, kind, id, src, out) = pending.remove(pos);
                        let new_sig = preview_sig_u64(&path);
                        let done = match result {
                            Ok(()) => ActionDone {
                                tag,
                                ok: true,
                                id,
                                kind,
                                src,
                                new_path: Some(out),
                                error: None,
                                new_sig,
                                bytes: None,
                            },
                            Err(e) => ActionDone {
                                tag,
                                ok: false,
                                id,
                                kind,
                                src,
                                new_path: Some(out),
                                error: Some(e.to_string()),
                                new_sig: None,
                                bytes: None,
                            },
                        };
                        if result_tx.send(done).is_err() {
                            return;
                        }
                    }
                }
                match job_rx.try_recv() {
                    Ok((tag, action, path)) => {
                        let kind = action.kind();
                        let id = action.id();
                        let out_path = match &action {
                            Action::Crop { .. } => Some(path.clone()),
                            Action::Resave { format, .. } => {
                                Some(path.with_extension(format.extension()))
                            }
                            _ => None,
                        };
                        // Priority work first — a crop/resave reads and
                        // writes whole images, so it must not compete with
                        // a client waiting for thumbnails or an early scan
                        // still indexing the album.
                        serial_gate
                            .yield_to_priority(crate::server::io_gate::SCAN_HOLD_PAUSE);
                        // Panic containment: a caught panic becomes a failed
                        // ActionDone instead of silently losing the job (the
                        // tag would sit in `serial` forever).
                        let exec = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            execute(&action, &path, &mut saver, serial_move_dir.as_deref())
                        }))
                        .unwrap_or_else(|_| Err(anyhow!("worker panicked")));
                        match exec {
                            Ok((new_sig, bytes)) => {
                                if new_sig.is_some() || bytes.is_some() {
                                    if result_tx
                                        .send(ActionDone {
                                            tag,
                                            ok: true,
                                            id,
                                            kind,
                                            src: path,
                                            new_path: out_path,
                                            error: None,
                                            new_sig,
                                            bytes,
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                } else {
                                    // Async save queued — completion arrives
                                    // via check_completions.
                                    pending.push((tag, kind, id, path, out_path.unwrap()));
                                }
                            }
                            Err(e) => {
                                if result_tx
                                    .send(ActionDone {
                                        tag,
                                        ok: false,
                                        id,
                                        kind,
                                        src: path,
                                        new_path: out_path,
                                        error: Some(e.to_string()),
                                        new_sig: None,
                                        bytes: None,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
        });
        Self {
            job_tx,
            result_rx: Mutex::new(result_rx),
            destructive_pool: DestructivePool::new(delete_workers, move_dir, Arc::clone(&gate)),
            immediate: Mutex::new(Vec::new()),
            destructive: Mutex::new(HashSet::new()),
            serial: Mutex::new(HashSet::new()),
            cleanup: Mutex::new(Vec::new()),
            gate,
        }
    }

    /// The IO gate this queue yields to (shared with preview production).
    pub fn io_gate(&self) -> Arc<IoGate> {
        Arc::clone(&self.gate)
    }

    /// Actions dispatched but not yet completed — shutdown waits on this.
    pub fn pending(&self) -> usize {
        self.destructive.lock().unwrap().len() + self.serial.lock().unwrap().len()
    }

    /// `path` has a queued delete/move — it is "marked for deletion" and
    /// must stay out of manifests/rebuilds until the op finishes.
    pub fn is_pending(&self, path: &Path) -> bool {
        self.destructive.lock().unwrap().contains(path)
    }

    pub fn dispatch(&self, tag: u32, action: Action, path: PathBuf) {
        let id = action.id();
        let kind = action.kind();
        match action {
            Action::Delete { sig, .. } | Action::Move { sig, .. } => {
                let queued = self.destructive.lock().unwrap().insert(path.clone());
                if !queued {
                    // Already queued/in flight: answer the duplicate instead
                    // of leaving the client waiting for a deduped job.
                    self.immediate.lock().unwrap().push(ActionDone {
                        tag,
                        ok: false,
                        id,
                        kind,
                        src: path,
                        new_path: None,
                        error: Some("action already pending".into()),
                        new_sig: None,
                        bytes: None,
                    });
                    return;
                }
                let job = DestructiveJob {
                    tag,
                    id,
                    kind,
                    path: path.clone(),
                    sig,
                };
                if !self.destructive_pool.dispatch(job) {
                    self.destructive.lock().unwrap().remove(&path);
                }
            }
            other => {
                self.serial.lock().unwrap().insert(tag);
                if self.job_tx.send((tag, other, path)).is_err() {
                    self.serial.lock().unwrap().remove(&tag);
                }
            }
        }
    }

    pub fn poll(&self) -> Vec<ActionDone> {
        let mut out = Vec::new();
        let mut purged: Vec<PathBuf> = Vec::new();

        // Duplicate-dispatch errors: the original op still owns the path.
        out.extend(self.immediate.lock().unwrap().drain(..));

        // Delete/move completions from the parallel pool.
        {
            let mut destructive = self.destructive.lock().unwrap();
            for done in self.destructive_pool.poll() {
                // Absent → the duplicate-dispatch path already answered it.
                if !destructive.remove(&done.src) {
                    continue;
                }
                if done.ok {
                    purged.push(done.src.clone());
                }
                out.push(done);
            }
        }

        // Crop/resave completions from the serial worker.
        {
            let mut serial = self.serial.lock().unwrap();
            let result_rx = self.result_rx.lock().unwrap();
            while let Ok(done) = result_rx.try_recv() {
                if !serial.remove(&done.tag) {
                    continue;
                }
                out.push(done);
            }
        }

        if !purged.is_empty() {
            self.cleanup.lock().unwrap().extend(purged);
        }

        // Batch preview cleanup once the queue drains: one scan per shared
        // `.imagecropper-previews` dir instead of one per deleted file. Runs
        // on its own thread — poll() sits on the server's event pump, and a
        // slow-mount directory scan must not stall event delivery.
        if self.pending() == 0 {
            let purge: Vec<PathBuf> = self.cleanup.lock().unwrap().drain(..).collect();
            if !purge.is_empty() {
                thread::spawn(move || {
                    remove_previews_for_many(purge.iter().map(PathBuf::as_path));
                });
            }
        }
        out
    }
}
