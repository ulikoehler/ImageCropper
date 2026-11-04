//! PreviewStore: worker pool producing persisted JPEG preview bytes, with an
//! in-RAM LRU in front of `.imagecropper-previews`. Emits "ready" signals so
//! the PushScheduler can stream previews as they're generated.
//!
//! The first [`PIN_LIMIT`] live album items are *pinned*: their previews are
//! kept resident in RAM (never evicted) and re-produced whenever they go
//! missing, so the browser can load the top of the album without touching the
//! disk at all.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, RwLock,
    },
    thread,
};

use lru::LruCache;

use crate::fs_utils::{adaptive_pool::AdaptivePool, previews::produce_preview_bytes};
use crate::server::io_gate::IoGate;

/// How many of the first live album thumbnails stay pinned in RAM.
pub const PIN_LIMIT: usize = 4096;

/// A produced preview: raw JPEG bytes + natural source dimensions.
#[derive(Debug, Clone)]
pub struct Preview {
    pub jpeg: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

/// RAM cache plus the pinned set it must never evict. One lock for both so
/// eviction decisions and pin updates can't race.
struct Ram {
    cache: LruCache<PathBuf, Preview>,
    pinned: HashSet<PathBuf>,
    /// `cache` capacity: the unpinned budget plus room for every pin.
    capacity: usize,
}

impl Ram {
    fn insert(&mut self, path: PathBuf, preview: Preview) {
        if self.cache.len() >= self.capacity && !self.cache.contains(&path) {
            // Evict the least-recently-used entry that is not pinned. Pinned
            // thumbnails stay resident until the album says otherwise.
            let victim = self
                .cache
                .iter()
                .rev()
                .map(|(k, _)| k.clone())
                .find(|k| !self.pinned.contains(k));
            match victim {
                Some(victim) => {
                    self.cache.pop(&victim);
                }
                // Everything resident is pinned — the new preview is not, so
                // caching it would only push a pinned one out.
                None => return,
            }
        }
        self.cache.put(path, preview);
    }
}

/// Dedup slot for inline `get_or_produce` calls: the first caller for a
/// path produces, followers wait on `cv` and clone the result. Without
/// this every want/sweep/pin racer of the same miss pays a full
/// decode+encode — and all of them log/cache-check the same file.
struct Inflight {
    /// None = still producing; Some(result) = finished (result may be None).
    done: Mutex<Option<Option<Preview>>>,
    cv: Condvar,
}

/// Removes the leader's map entry even on panic — waiters must never hang.
/// An unfilled result is completed as `None` (produce failure), which the
/// callers already handle via the normal miss path.
struct InflightGuard<'a> {
    map: &'a Mutex<HashMap<PathBuf, Arc<Inflight>>>,
    path: PathBuf,
    slot: Arc<Inflight>,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        {
            let mut d = self.slot.done.lock().unwrap();
            if d.is_none() {
                *d = Some(None);
            }
        }
        self.slot.cv.notify_all();
        self.map.lock().unwrap().remove(&self.path);
    }
}

/// Concurrency bound on inline produces (`get_or_produce`/`regenerate`).
/// Each want spawns its own blocking task — without a cap, a slow mount
/// ends up running dozens of full-res decodes at once, every read
/// competing with every other until they all hit the produce deadline.
/// The background pool has its own worker bound; this caps the client-
/// driven path at the same `-j` budget, and waits queue behind the gate
/// instead of hammering the mount.
struct ProduceGate {
    running: Mutex<usize>,
    cv: Condvar,
    max: usize,
}

struct GatePermit<'a>(&'a ProduceGate);

impl ProduceGate {
    fn acquire(&self) -> GatePermit<'_> {
        let mut n = self.running.lock().unwrap();
        while *n >= self.max {
            n = self.cv.wait(n).unwrap();
        }
        *n += 1;
        GatePermit(self)
    }
}

impl Drop for GatePermit<'_> {
    fn drop(&mut self) {
        let mut n = self.0.running.lock().unwrap();
        *n -= 1;
        self.0.cv.notify_one();
    }
}

pub struct PreviewStore {
    pool: AdaptivePool<Option<Preview>>,
    ram: Mutex<Ram>,
    /// In-flight/queued paths — kept for tests; the pool dedupes anyway.
    pub pending: Mutex<HashSet<PathBuf>>,
    /// Inline produces currently running, keyed by source path.
    inflight: Mutex<HashMap<PathBuf, Arc<Inflight>>>,
    /// Natural source dims already sniffed at manifest build/upsert time —
    /// lets a preview cache hit skip opening the source file at all.
    dims: Arc<RwLock<HashMap<PathBuf, (u32, u32)>>>,
    /// Set by `stop()`: all new production (pool AND inline `get_or_produce`)
    /// is refused. Cached RAM hits are still served.
    stopped: AtomicBool,
    /// Client-first IO priority: live requests mark it, background preload
    /// and destructive work yield to it.
    gate: Arc<IoGate>,
    /// Deferred fullscreen-preview fill (see [`queue_screen`]).
    screen: Arc<ScreenFill>,
    /// Bounds concurrent inline produces — see [`ProduceGate`].
    produce_gate: ProduceGate,
}

/// Background queue for fullscreen-preview generation. Producing a 4K
/// preview costs a full-res decode+resize, so it must never run inline on
/// the thumbnail hot path — a single tier-3 thread drains it, deduped.
struct ScreenFill {
    queue: Mutex<VecDeque<PathBuf>>,
    pending: Mutex<HashSet<PathBuf>>,
    cv: Condvar,
}

impl ScreenFill {
    fn push(&self, path: PathBuf) {
        if self.pending.lock().unwrap().insert(path.clone()) {
            self.queue.lock().unwrap().push_back(path);
            self.cv.notify_one();
        }
    }
}

impl PreviewStore {
    /// `concurrency` is the scaling ceiling (`-j`); `ram_cache` entries held
    /// decoded-to-JPEG in RAM (on top of the [`PIN_LIMIT`] pinned ones).
    pub fn new(concurrency: usize, ram_cache: usize) -> Self {
        Self::with_limits(concurrency, ram_cache, PIN_LIMIT, IoGate::new())
    }

    /// Same as [`PreviewStore::new`] sharing the server's IO gate.
    pub fn with_gate(concurrency: usize, ram_cache: usize, gate: Arc<IoGate>) -> Self {
        Self::with_limits(concurrency, ram_cache, PIN_LIMIT, gate)
    }

    /// Explicit limits — tests use a small pin budget to observe eviction.
    pub fn with_limits(
        concurrency: usize,
        ram_cache: usize,
        pin_limit: usize,
        gate: Arc<IoGate>,
    ) -> Self {
        let dims = Arc::new(RwLock::new(HashMap::new()));
        let worker_dims = dims.clone();
        let worker_gate = Arc::clone(&gate);
        // The LRU is sized for pins + the unpinned budget; `Ram::insert`
        // decides what to evict, so pinned entries never fall out.
        let capacity = ram_cache.max(1) + pin_limit;
        let screen = Arc::new(ScreenFill {
            queue: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashSet::new()),
            cv: Condvar::new(),
        });
        let store = Self {
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
            ram: Mutex::new(Ram {
                cache: LruCache::new(NonZeroUsize::new(capacity).unwrap()),
                pinned: HashSet::new(),
                capacity,
            }),
            pending: Mutex::new(HashSet::new()),
            inflight: Mutex::new(HashMap::new()),
            dims,
            stopped: AtomicBool::new(false),
            gate,
            screen,
            produce_gate: ProduceGate {
                running: Mutex::new(0),
                cv: Condvar::new(),
                max: concurrency.max(1),
            },
        };
        // Deferred fullscreen-preview fill: `f`-quality previews generate
        // lazily at tier-3 priority instead of inline on the thumbnail hot
        // path. `produce_screen_jpeg` still makes one on demand if the user
        // reaches a file before the fill does.
        {
            let screen = Arc::clone(&store.screen);
            let gate = Arc::clone(&store.gate);
            thread::spawn(move || loop {
                let path = {
                    let Ok(mut q) = screen.queue.lock() else { return };
                    loop {
                        if let Some(p) = q.pop_front() {
                            break p;
                        }
                        let Ok((g, _)) = screen
                            .cv
                            .wait_timeout(q, std::time::Duration::from_millis(250))
                        else {
                            return;
                        };
                        q = g;
                    }
                };
                screen.pending.lock().unwrap().remove(&path);
                // Tier-3: yield to client requests and the scan hold.
                gate.yield_to_priority(crate::server::io_gate::SCAN_HOLD_PAUSE);
                crate::fs_utils::previews::ensure_screen_preview(&path, None, None);
            });
        }
        store
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
        self.ram.lock().unwrap().cache.get(path).cloned()
    }

    /// Pin `paths` as "keep these thumbnails in RAM" (the top of the album).
    /// Pinned previews are never evicted; any that are missing are queued for
    /// production so the set converges on resident. Returns how many pinned
    /// previews are still missing.
    pub fn set_pins(&self, paths: Vec<PathBuf>) -> usize {
        let pins: HashSet<PathBuf> = paths.into_iter().collect();
        let missing: Vec<PathBuf> = {
            let mut ram = self.ram.lock().unwrap();
            ram.pinned = pins;
            ram.pinned
                .iter()
                .filter(|p| !ram.cache.contains(*p))
                .cloned()
                .collect()
        };
        for path in &missing {
            // Idempotent: the pool dedupes, so an already-queued preview is
            // not produced twice.
            self.request(path.clone());
        }
        missing.len()
    }

    /// How many thumbnails are pinned right now.
    pub fn pinned_count(&self) -> usize {
        self.ram.lock().unwrap().pinned.len()
    }

    /// Whether `path` is pinned.
    pub fn is_pinned(&self, path: &PathBuf) -> bool {
        self.ram.lock().unwrap().pinned.contains(path)
    }

    /// Pinned thumbnails that are not resident yet — the pin fill backlog.
    pub fn missing_pins(&self) -> Vec<PathBuf> {
        let ram = self.ram.lock().unwrap();
        ram.pinned
            .iter()
            .filter(|p| !ram.cache.contains(*p))
            .cloned()
            .collect()
    }

    /// Drain finished jobs into the RAM cache; returns (path, preview) pairs
    /// ready to be pushed to clients.
    pub fn poll(&self) -> Vec<(PathBuf, Option<Preview>)> {
        let mut done = Vec::new();
        let mut pending = self.pending.lock().unwrap();
        let mut ram = self.ram.lock().unwrap();
        for (path, preview) in self.pool.poll() {
            pending.remove(&path);
            if let Some(p) = &preview {
                ram.insert(path.clone(), p.clone());
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
    /// background sweep to reach it). Concurrent calls for the same path
    /// share one production: the leader produces, followers wait on the
    /// slot and clone its result.
    pub fn get_or_produce(&self, path: &PathBuf) -> Option<Preview> {
        if let Some(p) = self.cached(path) {
            if crate::fs_utils::verbose() {
                let kind = if self.is_pinned(path) { "ram" } else { "ram-lru" };
                eprintln!("preview hit [{kind}]: {}", path.display());
            }
            return Some(p);
        }
        if self.stopped() {
            return None;
        }
        // Leader produces; followers wait on the shared slot. A wedged
        // produce (network mount) blocks followers too, but the callers'
        // own waits are already bounded by PRODUCE_TIMEOUT — and a leader
        // that panics completes the slot as failure via InflightGuard.
        let (slot, leader) = {
            let mut m = self.inflight.lock().unwrap();
            match m.get(path) {
                Some(s) => (s.clone(), false),
                None => {
                    let s = Arc::new(Inflight {
                        done: Mutex::new(None),
                        cv: Condvar::new(),
                    });
                    m.insert(path.clone(), s.clone());
                    (s, true)
                }
            }
        };
        if !leader {
            let mut g = slot.done.lock().unwrap();
            loop {
                if let Some(res) = &*g {
                    return res.clone();
                }
                // Leader vanished without a result (map entry removed by a
                // different exit path) — take over rather than hang.
                if !self.inflight.lock().unwrap().contains_key(path) {
                    drop(g);
                    return self.get_or_produce(path);
                }
                let (ng, _) = slot
                    .cv
                    .wait_timeout(g, std::time::Duration::from_millis(250))
                    .unwrap();
                g = ng;
            }
        }
        let _guard = InflightGuard {
            map: &self.inflight,
            path: path.clone(),
            slot: slot.clone(),
        };
        // A live client is waiting for this — mark it so background work
        // (preload, deletes, crops) holds off while it runs.
        let _client = self.gate.client_work();
        // Then the produce gate: at most `-j` concurrent decodes on the
        // client-driven path. Followers already waiting on this slot's
        // Inflight just sleep a bit longer — the produce is shared.
        let _permit = self.produce_gate.acquire();
        let hint = self.dims.read().unwrap().get(path).copied();
        let preview = produce_preview_bytes(path, hint).map(|(jpeg, w, h)| Preview {
            jpeg: Arc::new(jpeg),
            w,
            h,
        });
        if let Some(p) = &preview {
            self.ram.lock().unwrap().insert(path.clone(), p.clone());
            // The fullscreen (`f`) preview generates lazily on the tier-3
            // fill thread — never inline on the thumbnail hot path.
            self.screen.push(path.clone());
        }
        *slot.done.lock().unwrap() = Some(preview.clone());
        preview
    }

    pub fn invalidate(&self, path: &PathBuf) {
        self.ram.lock().unwrap().cache.pop(path);
    }

    /// Test hook: insert a preview into the RAM cache directly.
    #[doc(hidden)]
    pub fn ram_insert_for_test(&self, path: PathBuf, preview: Preview) {
        self.ram.lock().unwrap().insert(path, preview);
    }

    /// Test hook: inline produces still in flight (should drain to zero).
    #[doc(hidden)]
    pub fn inflight_len(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }

    /// Drop every cached copy of `path`'s preview (RAM + persisted file)
    /// and produce a fresh one synchronously. Called when a client reports
    /// the served bytes as undecodable — the cached file is presumed
    /// corrupt and must not be served again.
    pub fn regenerate(&self, path: &PathBuf) -> Option<Preview> {
        self.invalidate(path);
        crate::fs_utils::previews::invalidate_preview_bytes(path);
        self.get_or_produce(path)
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
