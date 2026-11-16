use std::{collections::HashSet, path::PathBuf};

use eframe::egui;

use crate::{
    fs_utils::{
        adaptive_pool::AdaptivePool,
        previews::{produce_preview_image, produce_screen_image},
    },
    image_utils::to_color_image,
};

/// Re-exported for callers that used the old module-local helpers.
pub use crate::fs_utils::previews::{
    preview_file_name, preview_file_prefix, previews_dir_for, remove_previews_for,
    remove_previews_for_many, source_signature, THUMB_MAX_DIM,
};

/// Result of a thumbnail job: the decoded preview, if it could be produced.
pub struct ThumbResult {
    pub path: PathBuf,
    pub image: Option<egui::ColorImage>,
}

/// Background preview generator that loads/generates small previews for all
/// images. Concurrency self-tunes to the storage device (see
/// [`AdaptivePool`]): sequential-ish for HDD/remote mounts, up to
/// `concurrency` for SSD.
///
/// Previews are persisted in a `.imagecropper-previews` directory next to the
/// source image. The preview file name embeds the source file's size and
/// modification time, so changed files automatically produce fresh previews
/// and stale preview files are cleaned up.
pub struct Thumbnailer {
    pool: AdaptivePool<Option<egui::ColorImage>>,
    /// Fullscreen-size (1920) preview jobs for hold-`f`.
    screen_pool: AdaptivePool<Option<egui::ColorImage>>,
    pub pending: HashSet<PathBuf>,
    pending_screen: HashSet<PathBuf>,
    /// Scan-time signatures per path — a preview cache hit then needs no
    /// source `stat`, one less round-trip per file on slow mounts.
    sigs: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>>,
}

impl Thumbnailer {
    /// `concurrency` is the scaling ceiling (`-j`).
    pub fn new(concurrency: usize) -> Self {
        let sigs =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let thumb_sigs = sigs.clone();
        let screen_sigs = sigs.clone();
        Self {
            pool: AdaptivePool::new(2, concurrency.max(1), move |path| {
                let sig = thumb_sigs.lock().unwrap().get(path).copied();
                produce_preview_image(path, sig)
                    .ok()
                    .map(|i| to_color_image(&i))
            }),
            screen_pool: AdaptivePool::new(2, 4, move |path| {
                let sig = screen_sigs.lock().unwrap().get(path).copied();
                produce_screen_image(path, sig)
                    .ok()
                    .map(|i| to_color_image(&i))
            }),
            pending: HashSet::new(),
            pending_screen: HashSet::new(),
            sigs,
        }
    }

    /// Current worker count (autotuned between 1 and `-j`).
    pub fn workers(&self) -> usize {
        self.pool.workers()
    }

    /// Queue a thumbnail job; deduplicates against in-flight jobs.
    /// `sig` is the scan-time signature (0 = unknown → the produce stats).
    pub fn request(&mut self, path: PathBuf, sig: u64) {
        if self.pending.contains(&path) {
            return;
        }
        self.pending.insert(path.clone());
        if sig != 0 {
            self.sigs.lock().unwrap().insert(path.clone(), sig);
        }
        self.pool.request(path);
    }

    /// Drain finished jobs, updating the pending set.
    pub fn poll(&mut self) -> Vec<ThumbResult> {
        let mut done = Vec::new();
        for (path, image) in self.pool.poll() {
            self.pending.remove(&path);
            done.push(ThumbResult { path, image });
        }
        done
    }

    /// Queue a fullscreen-size (≤1920) preview job for hold-`f`.
    pub fn request_screen(&mut self, path: PathBuf, sig: u64) {
        if self.pending_screen.contains(&path) {
            return;
        }
        self.pending_screen.insert(path.clone());
        if sig != 0 {
            self.sigs.lock().unwrap().insert(path.clone(), sig);
        }
        self.screen_pool.request(path);
    }

    /// Drain finished fullscreen-preview jobs.
    pub fn poll_screen(&mut self) -> Vec<ThumbResult> {
        let mut done = Vec::new();
        for (path, image) in self.screen_pool.poll() {
            self.pending_screen.remove(&path);
            done.push(ThumbResult { path, image });
        }
        done
    }

    /// Stop all preview generation (Ctrl-C): queued jobs are dropped,
    /// workers retire, and no new requests will be processed.
    pub fn cancel(&mut self) {
        self.pool.cancel();
        self.screen_pool.cancel();
        self.pending.clear();
        self.pending_screen.clear();
    }
}
