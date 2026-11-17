//! Pipelined thumbnail engine: listing → read → compute, joined by
//! concurrent bounded queues.
//!
//! Where [`AdaptivePool`](crate::fs_utils::adaptive_pool::AdaptivePool)
//! runs whole jobs on interchangeable workers, [`ThumbPipeline`] splits
//! each thumbnail job at the natural I/O/CPU boundary so all three stages
//! proceed in parallel:
//!
//! ```text
//! request() ──► pending dedup ──► backlog ──► feeder thread
//!      │
//!      ▼
//!   jobs: BoundedQueue<PathBuf>           — the listing stage. Only a
//!      │                                   bounded run of listed paths is
//!      ▼                                   ever staged ahead of the reads.
//!   read workers (autotuned, I/O-bound): pack lookups, stats, fs::read —
//!      │                                   the "next N files".
//!      ├─ ReadStage::Done(r) ───────┐      (cache hits/fails resolve here)
//!      ▼                          │
//!   mid: BoundedQueue<ReadJob>     │      — byte- AND item-capped, so a
//!      │                           │     fast lister can never stage an
//!      ▼                           │     unbounded amount of image data.
//!   compute workers (fixed `-j`):  │     decode, resize, JPEG encode,
//!      │                           │     pack persist — CPU/GPU-bound.
//!      ▼                           ▼
//!   results: BoundedQueue<TimedResult> ──► poll()
//! ```
//!
//! Backpressure flows backwards through the bounds: a stalled consumer
//! fills `results`, which stalls compute workers, which fills `mid`,
//! which stalls readers, which fills `jobs`, which parks the feeder —
//! never memory.
//!
//! The read stage is what the [`ScaleController`] tunes (its latency is
//! the seek/network-bound component); compute stays at the `-j` ceiling
//! since decode parallelizes up to core/device limits on its own.

use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::fs_utils::{adaptive_pool::ScaleController, previews::ReadJob};

/// Paths staged between the feeder and the read workers. Listing can run
/// this far ahead of the reads; `PathBuf` jobs are small, so an item cap
/// is enough (the *byte* bound lives on `mid`).
const JOB_CAP: usize = 512;
/// Results buffered for `poll()` before producers back off.
const RESULT_CAP: usize = 512;
/// Total source/preview bytes staged between read and compute — the
/// memory bound on "the next N files" being read ahead.
const MID_BYTE_CAP: usize = 256 << 20; // 256 MiB
/// Idle workers (and the feeder) poll this often to notice retirement or
/// cancellation.
const RETIRE_POLL: Duration = Duration::from_millis(250);

/// What a read-stage closure produced for one path.
pub enum ReadStage<R> {
    /// The job is finished — goes straight to the result queue, skipping
    /// compute (cache hits, fast failures mapped into `R`).
    Done(R),
    /// Bytes the compute stage must turn into the result.
    Job(ReadJob),
}

/// A bounded multi-producer/multi-consumer queue: one mutex plus `space`
/// and `data` condvars. `push` blocks while the queue is full (by item
/// count or total item bytes); `pop_wait` blocks while empty. `close`
/// drops queued items and wakes every waiter — the cancellation path.
pub struct BoundedQueue<T> {
    inner: Mutex<BqInner<T>>,
    space: Condvar,
    data: Condvar,
    max_items: usize,
    max_bytes: usize,
    cost: fn(&T) -> usize,
}

struct BqInner<T> {
    items: VecDeque<T>,
    bytes: usize,
    closed: bool,
}

impl<T> BoundedQueue<T> {
    /// `max_bytes == 0` disables the byte bound (item cap only). An item
    /// always pushes onto an empty queue even when it exceeds
    /// `max_bytes` — a single oversized item can never deadlock.
    pub fn new(max_items: usize, max_bytes: usize, cost: fn(&T) -> usize) -> Self {
        Self {
            inner: Mutex::new(BqInner {
                items: VecDeque::new(),
                bytes: 0,
                closed: false,
            }),
            space: Condvar::new(),
            data: Condvar::new(),
            max_items: max_items.max(1),
            max_bytes,
            cost,
        }
    }

    /// Enqueue `item`, blocking while the queue is full. Returns `false`
    /// once closed (the item is dropped).
    pub fn push(&self, item: T) -> bool {
        let cost = (self.cost)(&item);
        let mut inner = self.inner.lock().unwrap();
        loop {
            if inner.closed {
                return false;
            }
            // `!items.is_empty()`: an oversized item must always be
            // admitted once the queue is drained — never block forever.
            let full = !inner.items.is_empty()
                && (inner.items.len() >= self.max_items
                    || (self.max_bytes > 0
                        && inner.bytes.saturating_add(cost) > self.max_bytes));
            if !full {
                inner.items.push_back(item);
                inner.bytes = inner.bytes.saturating_add(cost);
                self.data.notify_one();
                return true;
            }
            inner = self.space.wait(inner).unwrap();
        }
    }

    /// Dequeue the oldest item, waiting up to `timeout` when empty.
    /// `None` on timeout, or once the queue is closed and drained.
    pub fn pop_wait(&self, timeout: Duration) -> Option<T> {
        let mut inner = self.inner.lock().unwrap();
        let item = loop {
            match inner.items.pop_front() {
                Some(item) => break item,
                None => {
                    if inner.closed {
                        return None;
                    }
                    let (guard, _) = self.data.wait_timeout(inner, timeout).unwrap();
                    inner = guard;
                }
            }
        };
        inner.bytes = inner.bytes.saturating_sub((self.cost)(&item));
        self.space.notify_one();
        Some(item)
    }

    /// Dequeue without waiting — `poll()`'s drain path.
    pub fn try_pop(&self) -> Option<T> {
        let mut inner = self.inner.lock().unwrap();
        let item = inner.items.pop_front()?;
        inner.bytes = inner.bytes.saturating_sub((self.cost)(&item));
        self.space.notify_one();
        Some(item)
    }

    /// Drop every queued item and wake all producers/consumers. Queues
    /// stay closed permanently — a cancelled pipeline never reopens.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.items.clear();
        inner.bytes = 0;
        self.space.notify_all();
        self.data.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }
}

/// A read job staged for the compute stage.
struct MidItem {
    path: PathBuf,
    job: ReadJob,
    /// Read-stage latency — credited to the scale controller when the
    /// job's result is polled (the tuning signal for reader count).
    latency: Duration,
}

fn mid_cost(item: &MidItem) -> usize {
    item.job.byte_len()
}

/// Result handed to `poll()`.
struct TimedResult<R> {
    path: PathBuf,
    result: R,
    latency: Duration,
}

/// Shared `request` admission buffer: a `VecDeque` + condvar the feeder
/// thread drains into the bounded `jobs` queue.
struct Backlog {
    paths: Mutex<VecDeque<PathBuf>>,
    cv: Condvar,
}

impl Backlog {
    fn push(&self, path: PathBuf) {
        self.paths.lock().unwrap().push_back(path);
        self.cv.notify_one();
    }

    /// Pop the next path, waiting up to `timeout` when empty.
    fn pop_wait(&self, timeout: Duration) -> Option<PathBuf> {
        let mut paths = self.paths.lock().unwrap();
        loop {
            match paths.pop_front() {
                Some(p) => return Some(p),
                None => {
                    let (guard, _) = self.cv.wait_timeout(paths, timeout).unwrap();
                    paths = guard;
                }
            }
        }
    }

    fn clear(&self) {
        self.paths.lock().unwrap().clear();
        self.cv.notify_all();
    }
}

/// Two-stage thumbnail pool over [`BoundedQueue`]s. `R` is the caller's
/// result type (`Option<ColorImage>`, `Result<Preview, PreviewFail>`, …);
/// the read and compute closures map a path/bytes onto it.
///
/// API mirrors [`AdaptivePool`](crate::fs_utils::adaptive_pool::AdaptivePool):
/// `request`/`request_all`/`poll`/`pending_len`/`workers`/`cancel`.
pub struct ThumbPipeline<R: Send + 'static> {
    /// Queued + in-flight paths (dedup; mirrors AdaptivePool::pending).
    pending: Mutex<HashSet<PathBuf>>,
    /// Admission buffer between `request` and the bounded `jobs` queue —
    /// `request` never blocks; the feeder trickles paths into `jobs`.
    backlog: Arc<Backlog>,
    jobs: Arc<BoundedQueue<PathBuf>>,
    mid: Arc<BoundedQueue<MidItem>>,
    results: Arc<BoundedQueue<TimedResult<R>>>,
    read: Arc<dyn Fn(&Path) -> ReadStage<R> + Send + Sync>,
    compute: Arc<dyn Fn(&Path, ReadJob) -> R + Send + Sync>,
    /// Reader concurrency autotuner (shared logic with AdaptivePool).
    ctrl: Mutex<ScaleController>,
    /// Live read-worker retirement flags.
    readers: Mutex<Vec<Arc<AtomicBool>>>,
    /// Live compute-worker flags — fixed at the `-j` ceiling.
    computers: Mutex<Vec<Arc<AtomicBool>>>,
    /// Latched by `cancel()`: no new work admitted, all workers exit.
    cancelled: Arc<AtomicBool>,
}

impl<R: Send + 'static> ThumbPipeline<R> {
    /// `start` initial readers, `max` the ceiling for both stages.
    /// `read` does the I/O half (cache lookups, stat, `fs::read`);
    /// `compute` turns `ReadJob` bytes into `R` (decode/resize/encode).
    pub fn new(
        start: usize,
        max: usize,
        read: impl Fn(&Path) -> ReadStage<R> + Send + Sync + 'static,
        compute: impl Fn(&Path, ReadJob) -> R + Send + Sync + 'static,
    ) -> Self {
        let max = max.max(1);
        let pipeline = Self {
            pending: Mutex::new(HashSet::new()),
            backlog: Arc::new(Backlog {
                paths: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
            }),
            jobs: Arc::new(BoundedQueue::new(JOB_CAP, 0, |_| 0)),
            mid: Arc::new(BoundedQueue::new(
                max.saturating_mul(2),
                MID_BYTE_CAP,
                mid_cost,
            )),
            results: Arc::new(BoundedQueue::new(RESULT_CAP, 0, |_| 0)),
            read: Arc::new(read),
            compute: Arc::new(compute),
            ctrl: Mutex::new(ScaleController::new(1, max, start.max(1))),
            readers: Mutex::new(Vec::new()),
            computers: Mutex::new(Vec::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        pipeline.spawn_feeder();
        pipeline.set_readers(pipeline.ctrl.lock().unwrap().workers());
        pipeline.set_computers(max);
        pipeline
    }

    /// Queue a path; deduplicates with the pending set. Never blocks —
    /// overflow waits in `backlog` until a `jobs` slot frees.
    pub fn request(&self, path: PathBuf) {
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        if self.pending.lock().unwrap().insert(path.clone()) {
            self.backlog.push(path);
        }
    }

    /// Queue every path (deduplicated). Same semantics as
    /// [`AdaptivePool::request_all`](crate::fs_utils::adaptive_pool::AdaptivePool::request_all).
    pub fn request_all<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let mut pending = self.pending.lock().unwrap();
        for path in paths {
            if pending.insert(path.clone()) {
                self.backlog.push(path.clone());
            }
        }
    }

    /// Drain finished results; each entry clears its `pending` slot so the
    /// path can be re-requested. Reader-stage latencies feed the
    /// [`ScaleController`] here, matching `AdaptivePool::poll`.
    pub fn poll(&self) -> Vec<(PathBuf, R)> {
        let mut done = Vec::new();
        let mut ctrl = self.ctrl.lock().unwrap();
        {
            let mut pending = self.pending.lock().unwrap();
            while let Some(tr) = self.results.try_pop() {
                pending.remove(&tr.path);
                if let Some(target) = ctrl.record(tr.latency) {
                    drop(pending);
                    drop(ctrl);
                    self.set_readers(target);
                    pending = self.pending.lock().unwrap();
                    ctrl = self.ctrl.lock().unwrap();
                }
                done.push((tr.path, tr.result));
            }
            if pending.is_empty() {
                if let Some(target) = ctrl.flush() {
                    drop(pending);
                    drop(ctrl);
                    self.set_readers(target);
                }
            }
        }
        done
    }

    /// Queued + in-flight job count.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Current read-worker count (the autotuned stage).
    pub fn workers(&self) -> usize {
        self.readers.lock().unwrap().len()
    }

    /// Pin the reader count, bypassing the autotuner. Only used by
    /// `cancel(0)` today; kept for parity with `AdaptivePool`.
    pub fn set_worker_override(&self, n: usize) {
        self.ctrl.lock().unwrap().pin(n);
        self.set_readers(n);
    }

    /// Stop accepting work, drain every queue, retire all workers.
    /// In-flight jobs that finish after this point are dropped rather
    /// than delivered — `cancel` is the shutdown path (Ctrl-C/`stop()`).
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.set_worker_override(0);
        self.backlog.clear();
        // Closing wakes every blocked push/pop; queues drop their items.
        self.jobs.close();
        self.mid.close();
        self.results.close();
        self.pending.lock().unwrap().clear();
    }

    /// Feeder thread — the link between the unbounded admission `backlog`
    /// and the bounded `jobs` queue. Blocks on `jobs.push` when readers
    /// fall behind (listing backpressure) and on the backlog condvar
    /// when idle.
    fn spawn_feeder(&self) {
        let backlog = self.backlog.clone();
        let jobs = self.jobs.clone();
        let cancelled = self.cancelled.clone();
        thread::spawn(move || loop {
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            let Some(path) = backlog.pop_wait(RETIRE_POLL) else {
                continue;
            };
            if cancelled.load(Ordering::Relaxed) || !jobs.push(path) {
                return;
            }
        });
    }

    /// Resize the reader stage toward `target`: retire the newest workers
    /// when shrinking, spawn fresh ones when growing.
    fn set_readers(&self, target: usize) {
        let mut readers = self.readers.lock().unwrap();
        while readers.len() > target {
            if let Some(alive) = readers.pop() {
                alive.store(false, Ordering::Relaxed);
            }
        }
        while readers.len() < target {
            readers.push(self.spawn_reader());
        }
    }

    fn set_computers(&self, target: usize) {
        let mut computers = self.computers.lock().unwrap();
        while computers.len() > target {
            if let Some(alive) = computers.pop() {
                alive.store(false, Ordering::Relaxed);
            }
        }
        while computers.len() < target {
            computers.push(self.spawn_computer());
        }
    }

    /// Read worker: path → `ReadStage`. `Done` results go straight to the
    /// result queue; `Job`s flow through the byte-bounded `mid` queue.
    /// The measured latency is the read stage alone — the I/O component
    /// the scale controller tunes readers against.
    fn spawn_reader(&self) -> Arc<AtomicBool> {
        let alive = Arc::new(AtomicBool::new(true));
        let jobs = self.jobs.clone();
        let mid = self.mid.clone();
        let results = self.results.clone();
        let read = self.read.clone();
        let cancelled = self.cancelled.clone();
        let worker_alive = alive.clone();
        thread::spawn(move || loop {
            if cancelled.load(Ordering::Relaxed) || !worker_alive.load(Ordering::Relaxed) {
                return;
            }
            let Some(path) = jobs.pop_wait(RETIRE_POLL) else {
                if jobs.is_closed() {
                    return;
                }
                continue;
            };
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            let t0 = Instant::now();
            match (read)(&path) {
                ReadStage::Done(result) => {
                    if !results.push(TimedResult {
                        path,
                        result,
                        latency: t0.elapsed(),
                    }) {
                        return;
                    }
                }
                ReadStage::Job(job) => {
                    if !mid.push(MidItem {
                        path,
                        job,
                        latency: t0.elapsed(),
                    }) {
                        return;
                    }
                }
            }
        });
        alive
    }

    /// Compute worker: `ReadJob` bytes → `R` → result queue. Fixed count
    /// — decode/resize parallelizes to the `-j` ceiling on its own (GPU
    /// work serializes on the device mutex regardless).
    fn spawn_computer(&self) -> Arc<AtomicBool> {
        let alive = Arc::new(AtomicBool::new(true));
        let mid = self.mid.clone();
        let results = self.results.clone();
        let compute = self.compute.clone();
        let cancelled = self.cancelled.clone();
        let worker_alive = alive.clone();
        thread::spawn(move || loop {
            if cancelled.load(Ordering::Relaxed) || !worker_alive.load(Ordering::Relaxed) {
                return;
            }
            let Some(item) = mid.pop_wait(RETIRE_POLL) else {
                if mid.is_closed() {
                    return;
                }
                continue;
            };
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            let result = (compute)(&item.path, item.job);
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            if !results.push(TimedResult {
                path: item.path,
                result,
                latency: item.latency,
            }) {
                return;
            }
        });
        alive
    }
}

/// Dropping shuts the pipeline down: worker threads hold `Arc`s of the
/// queues (not senders), so without `cancel()` a dropped pipeline would
/// leave them parked on empty-queue waits forever.
impl<R: Send + 'static> Drop for ThumbPipeline<R> {
    fn drop(&mut self) {
        self.cancel();
    }
}
