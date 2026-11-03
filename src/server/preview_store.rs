//! PreviewStore: worker pool producing persisted JPEG preview bytes, with an
//! in-RAM LRU in front of `.imagecropper-previews`. Emits "ready" signals so
//! the PushScheduler can stream previews as they're generated.

use std::{
    collections::HashSet,
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use lru::LruCache;

use crate::fs_utils::{adaptive_pool::AdaptivePool, previews::produce_preview_bytes};

/// A produced preview: raw JPEG bytes + natural source dimensions.
#[derive(Debug, Clone)]
pub struct Preview {
    pub jpeg: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

pub struct PreviewStore {
    pool: AdaptivePool<Option<Preview>>,
    cache: Mutex<LruCache<PathBuf, Preview>>,
    /// In-flight/queued paths — kept for tests; the pool dedupes anyway.
    pub pending: Mutex<HashSet<PathBuf>>,
}

impl PreviewStore {
    /// `concurrency` is the scaling ceiling (`-j`); `ram_cache` entries held
    /// decoded-to-JPEG in RAM.
    pub fn new(concurrency: usize, ram_cache: usize) -> Self {
        Self {
            pool: AdaptivePool::new(2, concurrency.max(1), |path| {
                produce_preview_bytes(path).map(|(jpeg, w, h)| Preview {
                    jpeg: Arc::new(jpeg),
                    w,
                    h,
                })
            }),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(ram_cache.max(1)).unwrap())),
            pending: Mutex::new(HashSet::new()),
        }
    }

    /// Queue a path for preview production; dedupes in-flight jobs.
    pub fn request(&self, path: PathBuf) {
        let mut pending = self.pending.lock().unwrap();
        if !pending.insert(path.clone()) {
            return;
        }
        self.pool.request(path);
    }

    /// Cached (RAM) copy without hitting disk or queueing.
    pub fn cached(&self, path: &PathBuf) -> Option<Preview> {
        self.cache.lock().unwrap().get(path).cloned()
    }

    /// Drain finished jobs into the RAM cache; returns (path, preview) pairs
    /// ready to be pushed to clients.
    pub fn poll(&self) -> Vec<(PathBuf, Option<Preview>)> {
        let mut done = Vec::new();
        let mut pending = self.pending.lock().unwrap();
        let mut cache = self.cache.lock().unwrap();
        for (path, preview) in self.pool.poll() {
            pending.remove(&path);
            if let Some(p) = &preview {
                cache.put(path.clone(), p.clone());
            }
            done.push((path, preview));
        }
        done
    }

    /// Queue every file — the "preload ALL" engine. Order = enqueue order.
    pub fn request_all<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        for p in paths {
            self.request(p.clone());
        }
    }

    /// Synchronous produce for paths that bypass the queue (e.g. a WANT that
    /// isn't cached yet — still cheaper than making the client wait for the
    /// background sweep to reach it).
    pub fn get_or_produce(&self, path: &PathBuf) -> Option<Preview> {
        if let Some(p) = self.cached(path) {
            return Some(p);
        }
        let preview = produce_preview_bytes(path).map(|(jpeg, w, h)| Preview {
            jpeg: Arc::new(jpeg),
            w,
            h,
        });
        if let Some(p) = &preview {
            self.cache.lock().unwrap().put(path.clone(), p.clone());
        }
        preview
    }

    pub fn invalidate(&self, path: &PathBuf) {
        self.cache.lock().unwrap().pop(path);
    }
}
