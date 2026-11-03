use std::{collections::HashSet, path::PathBuf};

use eframe::egui;

use crate::{
    fs_utils::{adaptive_pool::AdaptivePool, previews::produce_preview_image},
    image_utils::to_color_image,
};

/// Re-exported for callers that used the old module-local helpers.
pub use crate::fs_utils::previews::{
    preview_file_name, preview_file_prefix, previews_dir_for, remove_previews_for,
    source_signature, THUMB_MAX_DIM,
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
    pub pending: HashSet<PathBuf>,
}

impl Thumbnailer {
    /// `concurrency` is the scaling ceiling (`-j`).
    pub fn new(concurrency: usize) -> Self {
        Self {
            pool: AdaptivePool::new(2, concurrency.max(1), |path| {
                produce_preview_image(path).map(|i| to_color_image(&i))
            }),
            pending: HashSet::new(),
        }
    }

    /// Current worker count (autotuned between 1 and `-j`).
    pub fn workers(&self) -> usize {
        self.pool.workers()
    }

    /// Queue a thumbnail job; deduplicates against in-flight jobs.
    pub fn request(&mut self, path: PathBuf) {
        if self.pending.contains(&path) {
            return;
        }
        self.pending.insert(path.clone());
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
}
