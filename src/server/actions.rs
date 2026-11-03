//! ActionQueue: signature-checked destructive operations (delete, crop-save,
//! resave) executed on a worker thread, results broadcast to all clients.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use anyhow::{anyhow, Result};
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::app::saver::Saver;
use crate::fs_utils::previews::{preview_sig_u64, remove_previews_for};
use crate::fs_utils::{move_with_unique_name, prepare_dir, TRASH_DIR};
use crate::image_utils::{build_output_image, decode_image_bytes, OutputFormat, SaveRequest};
use crate::selection::Selection;

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
}

/// Server → client result for an action (sent as an EVENT on /data).
#[derive(Debug, Clone, Serialize)]
pub struct ActionDone {
    pub tag: u32,
    pub ok: bool,
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
            verify_sig(path, *sig)?;
            let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let trash = prepare_dir(parent, TRASH_DIR)?;
            move_with_unique_name(path, &trash)?;
            remove_previews_for(path);
            Ok((None, Some(bytes)))
        }
        Action::Move { sig, .. } => {
            let dest = move_dir.ok_or_else(|| anyhow!("no -m/--move directory configured"))?;
            verify_sig(path, *sig)?;
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

/// Serialized ACTION payload wrapper: tag is the client's correlation id.
#[derive(Debug, Deserialize)]
pub struct ActionEnvelope {
    pub tag: u32,
    #[serde(flatten)]
    pub action: Action,
}

/// Background worker executing actions serially (Saver is not Sync).
/// Jobs carry the client's tag so completions can be correlated.
pub struct ActionQueue {
    job_tx: Sender<(u32, Action, PathBuf)>,
    result_rx: std::sync::Mutex<Receiver<ActionDone>>,
}

impl ActionQueue {
    pub fn new(saver_threads: usize, move_dir: Option<PathBuf>) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<(u32, Action, PathBuf)>();
        let (result_tx, result_rx) = mpsc::channel::<ActionDone>();
        thread::spawn(move || {
            let mut saver = Saver::new(saver_threads.max(1));
            // Async saves awaiting completion, keyed to their client's tag.
            let mut pending: Vec<(u32, PathBuf)> = Vec::new();
            loop {
                // Drain completed saves → ActionDone with the new sig.
                for (path, result, _sizes) in saver.check_completions() {
                    if let Some(pos) = pending.iter().position(|(_, p)| p == &path) {
                        let (tag, _) = pending.remove(pos);
                        let new_sig = preview_sig_u64(&path);
                        let done = match result {
                            Ok(()) => ActionDone {
                                tag,
                                ok: true,
                                error: None,
                                new_sig,
                                bytes: None,
                            },
                            Err(e) => ActionDone {
                                tag,
                                ok: false,
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
                        match execute(&action, &path, &mut saver, move_dir.as_deref()) {
                            Ok((new_sig, bytes)) => {
                                if new_sig.is_some() || bytes.is_some() {
                                    if result_tx
                                        .send(ActionDone {
                                            tag,
                                            ok: true,
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
                                    pending.push((tag, path));
                                }
                            }
                            Err(e) => {
                                if result_tx
                                    .send(ActionDone {
                                        tag,
                                        ok: false,
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
            result_rx: std::sync::Mutex::new(result_rx),
        }
    }

    pub fn dispatch(&self, tag: u32, action: Action, path: PathBuf) {
        let _ = self.job_tx.send((tag, action, path));
    }

    pub fn poll(&self) -> Vec<ActionDone> {
        let mut out = Vec::new();
        let result_rx = self.result_rx.lock().unwrap();
        while let Ok(d) = result_rx.try_recv() {
            out.push(d);
        }
        out
    }
}
