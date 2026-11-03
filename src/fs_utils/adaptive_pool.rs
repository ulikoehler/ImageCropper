//! Adaptive worker pool for preview generation.
//!
//! The optimal preview-generation concurrency depends on the storage medium:
//! SSDs and NVMe drives benefit from many parallel readers (until the CPU is
//! saturated), spinning disks and remote mounts (SMB/NFS) do not — there,
//! concurrent reads mostly add seek/network latency and sequential I/O wins.
//!
//! [`ScaleController`] autodetects this at runtime. Workers report the wall
//! clock latency of every completed job; from those samples the controller
//! estimates aggregate throughput with Little's law
//! (`tput ≈ active_workers / avg_latency`) and hill-climbs the worker count:
//!
//! * raise the count — if throughput improves meaningfully, keep climbing;
//! * if throughput is flat (or regresses), the extra parallelism only added
//!   queue latency → revert to the last good count and mark that config as a
//!   ceiling for a cooldown period;
//! * after the cooldown, probe upward again so media/filesystem changes or
//!   mixed-latency batches can find a better operating point.
//!
//! Concretely: HDD scans settle at 1–2 workers, SMB mounts at the point where
//! pipelined network latency stops paying off (typically 2–4), and local SSDs
//! climb until decode CPU saturates — usually well below `-j`.
//!
//! The controller is pure logic (no threads) so the behaviour is unit-testable;
//! [`AdaptivePool`] wires it to a thread pool feeding on a `PathBuf` job queue.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Number of completed jobs per evaluation epoch.
const EPOCH_JOBS: u32 = 24;
/// Minimum relative throughput gain that counts as an improvement.
const MIN_GAIN: f64 = 1.08;
/// A down-probe is accepted if throughput holds within 5% of best: equal
/// throughput at lower concurrency is preferred (less queue latency).
const DOWN_OK: f64 = 0.95;
/// Epochs to wait before probing again after a failed probe.
const COOLDOWN_EPOCHS: u32 = 4;
/// Epochs after which `ceiling`/`floor` are forgotten (media may change).
const REPROBE_EPOCHS: u32 = 12;
/// Idle workers poll the job channel this often so they notice retirement.
const RETIRE_POLL_MS: u64 = 250;

/// Direction of an in-flight probe being evaluated this epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
}

/// Pure concurrency controller: feed per-job latencies, it returns the worker
/// count to run at. Deterministic — no threads, no wall clock.
pub struct ScaleController {
    min: usize,
    max: usize,
    /// Current target worker count.
    workers: usize,
    /// Samples accumulated in the current window.
    win_jobs: u32,
    win_lat: Duration,
    /// Workers active during the current window (for the tput estimate).
    win_workers: usize,
    /// Best throughput accepted so far, and the count that produced it.
    best_tput: f64,
    best_workers: usize,
    /// Smallest worker count that failed to improve on `best_tput`.
    ceiling: usize,
    /// Largest worker count whose down-probe regressed (don't retry below it
    /// via down-probing — it demonstrably needs the parallelism).
    floor: usize,
    /// Worker-count change currently being evaluated, if any.
    probing: Option<Dir>,
    /// Epochs until another probe is allowed.
    cooldown: u32,
    /// Epochs until `ceiling`/`floor` are lifted (allow full re-probing).
    reprobe: u32,
    /// Forced worker count — when set, the controller stops probing and
    /// always returns this value (queues that track another pool's count).
    pinned: Option<usize>,
}

impl ScaleController {
    /// `start` initial workers, clamped into `[min, max]`.
    pub fn new(min: usize, max: usize, start: usize) -> Self {
        let workers = start.clamp(min.max(1), max.max(1));
        Self {
            min: min.max(1),
            max: max.max(1),
            workers,
            win_jobs: 0,
            win_lat: Duration::ZERO,
            win_workers: workers,
            best_tput: 0.0,
            best_workers: workers,
            ceiling: usize::MAX,
            floor: 0,
            probing: None,
            cooldown: 0,
            reprobe: REPROBE_EPOCHS,
            pinned: None,
        }
    }

    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Pin the worker count (0 allowed) — the controller stops autotuning.
    pub fn pin(&mut self, n: usize) {
        self.pinned = Some(n);
    }

    /// Record one completed job's latency. When an epoch completes, returns the
    /// (possibly unchanged) worker count the pool should run at.
    pub fn record(&mut self, latency: Duration) -> Option<usize> {
        if self.win_jobs == 0 {
            self.win_workers = self.workers;
        }
        self.win_jobs += 1;
        self.win_lat += latency;
        if self.win_jobs < EPOCH_JOBS {
            return None;
        }
        let out = self.evaluate();
        self.win_jobs = 0;
        self.win_lat = Duration::ZERO;
        out
    }

    /// Force evaluation of a partial window (e.g. when the queue went idle).
    pub fn flush(&mut self) -> Option<usize> {
        if self.win_jobs == 0 {
            return None;
        }
        let out = self.evaluate();
        self.win_jobs = 0;
        self.win_lat = Duration::ZERO;
        out
    }

    fn evaluate(&mut self) -> Option<usize> {
        if let Some(p) = self.pinned {
            let old = self.workers;
            self.workers = p;
            return (old != p).then_some(p);
        }
        // Little's law: steady-state throughput ≈ active_workers / avg_latency.
        let avg_lat = self.win_lat.as_secs_f64() / self.win_jobs.max(1) as f64;
        let tput = self.win_workers as f64 / avg_lat.max(1e-6);
        let old = self.workers;

        match self.probing.take() {
            Some(Dir::Up) => {
                if tput > self.best_tput * MIN_GAIN {
                    self.best_tput = tput;
                    self.best_workers = self.workers;
                } else {
                    self.ceiling = self.ceiling.min(self.workers);
                    self.workers = self.best_workers;
                    self.cooldown = COOLDOWN_EPOCHS;
                }
            }
            Some(Dir::Down) => {
                if tput >= self.best_tput * DOWN_OK {
                    // Same throughput at lower concurrency — prefer it.
                    self.best_tput = tput.max(self.best_tput);
                    self.best_workers = self.workers;
                } else {
                    self.floor = self.floor.max(self.workers);
                    self.workers = self.best_workers;
                    self.cooldown = COOLDOWN_EPOCHS;
                }
            }
            None => {
                if tput > self.best_tput {
                    self.best_tput = tput;
                    self.best_workers = self.workers;
                }
                if self.cooldown > 0 {
                    self.cooldown -= 1;
                } else if self.reprobe > 0 {
                    self.reprobe -= 1;
                    if self.reprobe == 0 {
                        // Forget ceiling/floor — let the device prove a
                        // different operating point again.
                        self.ceiling = usize::MAX;
                        self.floor = 0;
                        self.reprobe = REPROBE_EPOCHS;
                    }
                }
                if self.cooldown == 0 {
                    if self.workers < self.probe_cap() {
                        self.probing = Some(Dir::Up);
                        self.workers += 1;
                    } else if self.workers > self.min && self.workers - 1 > self.floor {
                        // Upward is blocked — check whether fewer workers lose
                        // nothing (sequential is cheaper on HDD/SMB).
                        self.probing = Some(Dir::Down);
                        self.workers -= 1;
                    }
                }
            }
        }
        (self.workers != old).then_some(self.workers)
    }

    fn probe_cap(&self) -> usize {
        self.max.min(self.ceiling.saturating_sub(1)).max(self.min)
    }
}

/// Shared result of a pool job: what the worker produced plus how long it took.
struct TimedResult<R> {
    path: PathBuf,
    result: R,
    latency: Duration,
}

/// Worker pool over `PathBuf` jobs whose concurrency self-tunes via a
/// [`ScaleController`]. `R` is whatever the work function produces.
pub struct AdaptivePool<R: Send + 'static> {
    job_tx: Sender<PathBuf>,
    job_rx: Arc<Mutex<Receiver<PathBuf>>>,
    result_rx: Mutex<Receiver<TimedResult<R>>>,
    result_tx: Sender<TimedResult<R>>,
    work: Arc<dyn Fn(&Path) -> R + Send + Sync>,
    ctrl: Mutex<ScaleController>,
    /// Live worker handles; `false` tells the worker to exit after its job.
    workers: Mutex<Vec<(u64, Arc<AtomicBool>)>>,
    next_worker_id: AtomicU64,
    /// In-flight/queued paths (dedup + progress introspection).
    pending: Mutex<HashSet<PathBuf>>,
    /// Latched by `cancel()`: no new jobs accepted, workers exit after
    /// their in-flight job. Never cleared — a cancelled pool stays dead.
    cancelled: Arc<AtomicBool>,
}

impl<R: Send + 'static> AdaptivePool<R> {
    /// `max` is the parallelism ceiling (e.g. `-j`); the pool starts at `start`
    /// workers and scales to fit the storage device.
    pub fn new(
        start: usize,
        max: usize,
        work: impl Fn(&Path) -> R + Send + Sync + 'static,
    ) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
        let (result_tx, result_rx) = mpsc::channel();
        let pool = Self {
            job_tx,
            job_rx: Arc::new(Mutex::new(job_rx)),
            result_rx: Mutex::new(result_rx),
            result_tx,
            work: Arc::new(work),
            ctrl: Mutex::new(ScaleController::new(1, max.max(1), start.max(1))),
            workers: Mutex::new(Vec::new()),
            next_worker_id: AtomicU64::new(0),
            pending: Mutex::new(HashSet::new()),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        pool.set_workers(pool.ctrl.lock().unwrap().workers());
        pool
    }

    /// Queue a path; deduplicates in-flight jobs. No-op once cancelled.
    pub fn request(&self, path: PathBuf) {
        if self.cancelled.load(Ordering::Relaxed) {
            return;
        }
        let mut pending = self.pending.lock().unwrap();
        if !pending.insert(path.clone()) {
            return;
        }
        if self.job_tx.send(path).is_err() {
            pending.clear();
        }
    }

    /// Queue every path in order.
    pub fn request_all<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        for p in paths {
            self.request(p.clone());
        }
    }

    /// Drain finished jobs. Each call also feeds the scale controller and
    /// applies any concurrency change it decided on.
    pub fn poll(&self) -> Vec<(PathBuf, R)> {
        let mut done = Vec::new();
        let mut ctrl = self.ctrl.lock().unwrap();
        let result_rx = self.result_rx.lock().unwrap();
        {
            let mut pending = self.pending.lock().unwrap();
            while let Ok(tr) = result_rx.try_recv() {
                pending.remove(&tr.path);
                if let Some(target) = ctrl.record(tr.latency) {
                    drop(pending);
                    drop(ctrl);
                    self.set_workers(target);
                    pending = self.pending.lock().unwrap();
                    ctrl = self.ctrl.lock().unwrap();
                }
                done.push((tr.path, tr.result));
            }
        }
        // Queue idle → flush a partial window so the controller still learns.
        if self.pending.lock().unwrap().is_empty() {
            if let Some(target) = ctrl.flush() {
                drop(ctrl);
                self.set_workers(target);
            }
        }
        done
    }

    /// Number of queued + in-flight jobs.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Current worker count (observability, tests).
    pub fn workers(&self) -> usize {
        self.workers.lock().unwrap().len()
    }

    /// Force the worker count to exactly `n` (0 = fully stopped) and stop
    /// the autotuner — for queues whose concurrency tracks another pool.
    pub fn set_worker_override(&self, n: usize) {
        self.ctrl.lock().unwrap().pin(n);
        self.set_workers(n);
    }

    /// Drop every queued-but-not-started job and retire all workers.
    /// Latches: `request` becomes a no-op, so producers (scan feed, push
    /// WANTs) cannot refill the queue after cancellation. In-flight jobs
    /// finish and still deliver their results.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.set_worker_override(0);
        if let Ok(rx) = self.job_rx.lock() {
            while rx.try_recv().is_ok() {}
        }
        self.pending.lock().unwrap().clear();
    }

    /// Apply the controller's target: retire extra workers, spawn missing ones.
    fn set_workers(&self, target: usize) {
        let mut workers = self.workers.lock().unwrap();
        while workers.len() > target {
            if let Some((_, alive)) = workers.pop() {
                // Worker exits after finishing its current job (or within
                // RETIRE_POLL_MS while idle).
                alive.store(false, Ordering::Relaxed);
            }
        }
        while workers.len() < target {
            let id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
            let alive = Arc::new(AtomicBool::new(true));
            let job_rx = self.job_rx.clone();
            let result_tx = self.result_tx.clone();
            let work = self.work.clone();
            let alive_flag = alive.clone();
            let cancelled = self.cancelled.clone();
            workers.push((id, alive));
            thread::spawn(move || loop {
                if !alive_flag.load(Ordering::Relaxed) || cancelled.load(Ordering::Relaxed) {
                    break;
                }
                let path = {
                    let Ok(rx) = job_rx.lock() else { break };
                    match rx.recv_timeout(Duration::from_millis(RETIRE_POLL_MS)) {
                        Ok(p) => p,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                };
                // Cancelled between dequeue and start — drop the job.
                if cancelled.load(Ordering::Relaxed) {
                    break;
                }
                let t0 = Instant::now();
                let result = work(&path);
                let latency = t0.elapsed();
                if result_tx
                    .send(TimedResult {
                        path,
                        result,
                        latency,
                    })
                    .is_err()
                {
                    break;
                }
            });
        }
    }
}
