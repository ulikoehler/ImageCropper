use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::UNIX_EPOCH,
};

use eframe::egui;

use crate::{
    fs_utils::{prepare_dir, PREVIEWS_DIR},
    image_utils::{decode_image_bytes, resize_to_fit, to_color_image},
};

/// Maximum dimension of generated previews (pixels).
pub const THUMB_MAX_DIM: u32 = 512;

/// Quality of the JPEG previews stored in `.imagecropper-previews`.
const PREVIEW_JPEG_QUALITY: u8 = 80;

/// Result of a thumbnail job: the decoded preview, if it could be produced.
pub struct ThumbResult {
    pub path: PathBuf,
    pub image: Option<egui::ColorImage>,
}

/// Background worker pool that loads/generates small previews for all images.
///
/// Previews are persisted in a `.imagecropper-previews` directory next to the
/// source image. The preview file name embeds the source file's size and
/// modification time, so changed files automatically produce fresh previews
/// and stale preview files are cleaned up.
pub struct Thumbnailer {
    job_tx: Sender<PathBuf>,
    result_rx: Receiver<ThumbResult>,
    pub pending: HashSet<PathBuf>,
}

impl Thumbnailer {
    pub fn new(concurrency: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel::<ThumbResult>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        for _ in 0..concurrency.max(1) {
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
                    .send(ThumbResult {
                        image: produce_thumbnail(&path),
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

    /// Queue a thumbnail job; deduplicates against in-flight jobs.
    pub fn request(&mut self, path: PathBuf) {
        if self.pending.contains(&path) {
            return;
        }
        self.pending.insert(path.clone());
        if self.job_tx.send(path).is_err() {
            // Worker pool is gone; don't keep the path pending forever.
            self.pending.clear();
        }
    }

    /// Drain finished jobs, updating the pending set.
    pub fn poll(&mut self) -> Vec<ThumbResult> {
        let mut done = Vec::new();
        while let Ok(result) = self.result_rx.try_recv() {
            self.pending.remove(&result.path);
            done.push(result);
        }
        done
    }
}

/// Signature used to detect that a source file changed: (size, mtime nanos).
fn source_signature(path: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((meta.len(), nanos))
}

/// Deterministic preview file name for a source file: embeds the signature so
/// a stale preview simply has a different name.
pub fn preview_file_name(source_file_name: &str, len: u64, mtime_nanos: u128) -> String {
    format!("{source_file_name}.{len}-{mtime_nanos}.jpg")
}

/// Prefix matching all previews (any signature) of a given source file.
pub fn preview_file_prefix(source_file_name: &str) -> String {
    format!("{source_file_name}.")
}

/// Directory holding the previews for a source image.
pub fn previews_dir_for(source: &Path) -> PathBuf {
    source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PREVIEWS_DIR)
}

/// Remove stale previews of `source` inside `dir`, keeping `keep_name`.
/// Returns the cached preview path if an up-to-date preview exists.
fn find_cached_preview(dir: &Path, source: &Path, expected_name: &str) -> Option<PathBuf> {
    let source_name = source.file_name()?.to_string_lossy();
    let prefix = preview_file_prefix(&source_name);
    let mut cached = None;

    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        if name == expected_name {
            cached = Some(entry.path());
        } else {
            // Stale preview from an older version of the file.
            let _ = std::fs::remove_file(entry.path());
        }
    }
    cached
}

/// Remove every cached preview for a source file (e.g. after it was deleted).
pub fn remove_previews_for(source: &Path) {
    let Some(source_name) = source.file_name() else {
        return;
    };
    let prefix = preview_file_prefix(&source_name.to_string_lossy());
    let dir = previews_dir_for(source);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Load the cached preview or generate, persist and return a fresh one.
fn produce_thumbnail(path: &Path) -> Option<egui::ColorImage> {
    let (len, nanos) = source_signature(path)?;
    let source_name = path.file_name()?.to_string_lossy().into_owned();
    let expected_name = preview_file_name(&source_name, len, nanos);
    let dir = previews_dir_for(path);

    // Fast path: an up-to-date preview exists on disk.
    if let Some(cached) = find_cached_preview(&dir, path, &expected_name) {
        if let Ok(bytes) = std::fs::read(&cached) {
            if let Ok(img) = decode_image_bytes(&cached, &bytes) {
                return Some(to_color_image(&img));
            }
        }
        // Corrupt preview — regenerate.
        let _ = std::fs::remove_file(&cached);
    }

    // Slow path: decode the source, downscale, persist.
    let bytes = std::fs::read(path).ok()?;
    let image = decode_image_bytes(path, &bytes).ok()?;
    drop(bytes);
    let thumb = resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(preview_dir) = prepare_dir(parent, PREVIEWS_DIR) {
        let preview_path = preview_dir.join(&expected_name);
        if let Ok(file) = std::fs::File::create(&preview_path) {
            let writer = std::io::BufWriter::new(file);
            let rgb = thumb.to_rgb8();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                writer,
                PREVIEW_JPEG_QUALITY,
            );
            let _ = rgb.write_with_encoder(encoder);
        }
    }

    Some(to_color_image(&thumb))
}
