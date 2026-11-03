//! PreviewStore: worker pool producing persisted JPEG preview bytes, with an
//! in-RAM LRU in front of `.imagecropper-previews`. Emits "ready" signals so
//! the PushScheduler can stream previews as they're generated.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
};

use lru::LruCache;

use crate::fs_utils::{adaptive_pool::AdaptivePool, previews::produce_preview_bytes};
use crate::server::io_gate::IoGate;

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
    /// Natural source dims already sniffed at manifest build/upsert time —
    /// lets a preview cache hit skip opening the source file at all.
    dims: Arc<RwLock<HashMap<PathBuf, (u32, u32)>>>,
    /// Set by `stop()`: all new production (pool AND inline `get_or_produce`)
    /// is refused. Cached RAM hits are still served.
    stopped: AtomicBool,
    /// Client-first IO priority: live requests mark it, background preload
    /// and destructive work yield to it.
    gate: Arc<IoGate>,
}

impl PreviewStore {
    /// `concurrency` is the scaling ceiling (`-j`); `ram_cache` entries held
    /// decoded-to-JPEG in RAM.
    pub fn new(concurrency: usize, ram_cache: usize) -> Self {
        Self::with_gate(concurrency, ram_cache, IoGate::new())
    }

    /// Same as [`PreviewStore::new`] sharing the server's IO gate.
    pub fn with_gate(concurrency: usize, ram_cache: usize, gate: Arc<IoGate>) -> Self {
        let dims = Arc::new(RwLock::new(HashMap::new()));
        let worker_dims = dims.clone();
        let worker_gate = Arc::clone(&gate);
        Self {
            pool: AdaptivePool::new(2, concurrency.max(1), move |path| {
                // Speculative preload: yield to live client requests instead
                // of competing with them for IO.
                worker_gate.yield_to_clients();
                let hint = worker_dims.read().unwrap().get(path).copied();
                produce_preview_bytes(path, hint).map(|(jpeg, w, h)| Preview {
                    jpeg: Arc::new(jpeg),
                    w,
                    h,
                })
            }),
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(ram_cache.max(1)).unwrap())),
            pending: Mutex::new(HashSet::new()),
            dims,
            stopped: AtomicBool::new(false),
            gate,
        }
    }

    /// `stop()` was called — no new production should start.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Record natural dims for a path (from manifest sniffing) so preview
    /// production can answer `w`/`h` without opening the source.
    pub fn note_dims(&self, path: &PathBuf, dims: (u32, u32)) {
        if dims != (0, 0) {
            self.dims.write().unwrap().insert(path.clone(), dims);
        }
    }

    /// Queue a path for preview production; dedupes in-flight jobs.
    pub fn request(&self, path: PathBuf) {
        if self.stopped() {
            return;
        }
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
        if self.stopped() {
            return None;
        }
        // A live client is waiting for this — mark it so background work
        // (preload, deletes, crops) holds off while it runs.
        let _client = self.gate.client_work();
        let hint = self.dims.read().unwrap().get(path).copied();
        let preview = produce_preview_bytes(path, hint).map(|(jpeg, w, h)| Preview {
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

    /// Stop all preview generation (Ctrl-C shutdown): queued jobs are
    /// dropped, workers retire, and every production entry point (pool
    /// requests, inline `get_or_produce`) refuses new work. In-flight
    /// workers still finish their current job — a blocking remote read
    /// can't be aborted — but nothing new starts.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.pool.cancel();
        self.pending.lock().unwrap().clear();
    }
}
