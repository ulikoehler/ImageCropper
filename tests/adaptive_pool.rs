//! Adaptive pool tests: ScaleController on synthetic latency models, plus a
//! real AdaptivePool draining jobs.

use std::path::PathBuf;
use std::time::Duration;

use imagecropper::fs_utils::adaptive_pool::{AdaptivePool, ScaleController};

/// Feed `jobs` completions through the controller, all with `latency`, at the
/// current worker count. Returns the target whenever the controller adjusts.
fn feed_epoch(c: &mut ScaleController, latency: Duration, jobs: u32) -> Vec<usize> {
    let mut targets = Vec::new();
    for _ in 0..jobs {
        if let Some(t) = c.record(latency) {
            targets.push(t);
        }
    }
    targets
}

#[test]
fn ssd_like_latency_scales_up_to_max() {
    // Constant per-job latency regardless of concurrency → tput ∝ workers.
    let mut c = ScaleController::new(1, 8, 2);
    let lat = Duration::from_millis(10);
    // Epochs keep probing up (8% gain threshold easily met on SSD model).
    let mut seen_up = 0;
    for _ in 0..12 {
        for t in feed_epoch(&mut c, lat, 24) {
            assert!(t > 1);
            seen_up = seen_up.max(t);
        }
    }
    assert!(seen_up >= 6, "SSD model should climb, got {seen_up}");
}

#[test]
fn hdd_like_latency_settles_low() {
    // Latency inflates ∝ workers (seek contention): tput stays ~constant.
    // Model the *per-worker* latency at the current count: lat = s * workers.
    let mut c = ScaleController::new(1, 8, 2);
    let s = Duration::from_millis(10);
    let mut min_seen = usize::MAX;
    for _ in 0..30 {
        let lat = s * c.workers() as u32;
        for t in feed_epoch(&mut c, lat, 24) {
            min_seen = min_seen.min(t);
        }
    }
    assert_eq!(
        min_seen, 1,
        "HDD model must probe down to sequential, got {min_seen}"
    );
    assert!(
        c.workers() <= 2,
        "HDD model should stay low, got {}",
        c.workers()
    );
}

#[test]
fn smb_like_latency_finds_knee() {
    // Network latency overlaps until ~4 workers (bandwidth cap): latency is
    // constant up to 4 workers, inflates ∝ workers beyond that.
    let mut c = ScaleController::new(1, 8, 2);
    let base = Duration::from_millis(30);
    let knee = 4usize;
    let lat = |w: usize| {
        if w <= knee {
            base
        } else {
            base * (w - knee + 1) as u32
        }
    };
    let w0 = c.workers();
    feed_epoch(&mut c, lat(w0), 24);
    for _ in 0..30 {
        let l = lat(c.workers());
        feed_epoch(&mut c, l, 24);
    }
    assert!(
        c.workers() <= knee,
        "SMB model should settle at/below the knee, got {}",
        c.workers()
    );
    assert!(
        c.workers() >= 2,
        "should keep some pipelining, got {}",
        c.workers()
    );
}

#[test]
fn probe_failure_cools_down_then_retries() {
    let mut c = ScaleController::new(1, 8, 2);
    let lat = Duration::from_millis(10);
    feed_epoch(&mut c, lat, 24);
    // Force a probe by letting it climb with SSD-like gains once…
    feed_epoch(&mut c, lat, 24);
    assert!(c.workers() > 2);
    // Now simulate a regression window (latency explodes) → revert + cooldown.
    let bad_lat = Duration::from_millis(10 * c.workers() as u64);
    feed_epoch(&mut c, bad_lat, 24);
    // After revert it should not instantly climb again.
    let cur = c.workers();
    let ups: Vec<usize> = feed_epoch(&mut c, bad_lat, 24)
        .into_iter()
        .filter(|&w| w > cur)
        .collect();
    assert!(ups.is_empty(), "cooldown must suppress immediate re-probe");
}

#[test]
fn clamps_to_min_and_max() {
    let mut c = ScaleController::new(1, 3, 99);
    assert_eq!(c.workers(), 3, "start clamped to max");
    let lat = Duration::from_millis(10);
    for _ in 0..40 {
        feed_epoch(&mut c, lat, 24);
    }
    assert!(c.workers() <= 3);
    assert!(c.workers() >= 1);
}

#[test]
fn flush_evaluates_partial_window() {
    let mut c = ScaleController::new(1, 8, 2);
    let lat = Duration::from_millis(10);
    for _ in 0..10 {
        assert!(c.record(lat).is_none(), "partial window stays quiet");
    }
    // flush forces evaluation; may return target or None but must not panic.
    let _ = c.flush();
    assert_eq!(c.workers().clamp(1, 8), c.workers());
}

#[test]
fn real_pool_drains_and_scales() {
    // A real AdaptivePool over trivial work completes all jobs and keeps ≥1
    // worker; with zero-latency work the controller should climb (tput ∝ n).
    let pool: AdaptivePool<u32> = AdaptivePool::new(1, 4, |_| 7u32);
    let paths: Vec<PathBuf> = (0..200).map(|i| PathBuf::from(format!("/x/{i}"))).collect();
    pool.request_all(paths.iter());
    let mut got = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while got < paths.len() && std::time::Instant::now() < deadline {
        got += pool.poll().len();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(got, paths.len(), "all jobs drained");
    assert!(pool.workers() >= 1);
}

#[test]
fn pool_dedupes_requests() {
    let pool: AdaptivePool<u32> = AdaptivePool::new(1, 2, |_| 1u32);
    let p = PathBuf::from("/y/one");
    pool.request(p.clone());
    pool.request(p.clone());
    assert_eq!(pool.pending_len(), 1);
}

#[test]
fn worker_override_pins_exact_count() {
    // Delete-style queue: worker count tracks an external pool's count.
    let pool: AdaptivePool<u32> = AdaptivePool::new(1, 8, |_| 1u32);
    pool.set_worker_override(4);
    assert_eq!(pool.workers(), 4, "override must land exactly");
    // The autotuner stays out of the way even after jobs complete.
    let paths: Vec<PathBuf> = (0..60).map(|i| PathBuf::from(format!("/p/{i}"))).collect();
    pool.request_all(paths.iter());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut got = 0;
    while got < paths.len() && std::time::Instant::now() < deadline {
        got += pool.poll().len();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(got, paths.len());
    assert_eq!(pool.workers(), 4, "pinned count survives epochs");
}

#[test]
fn cancel_drops_queued_jobs_and_stops_workers() {
    // Block jobs behind a barrier so queued work stays queued, then cancel.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let b = barrier.clone();
    let pool: AdaptivePool<u32> = AdaptivePool::new(1, 1, move |_| {
        b.wait();
        1u32
    });
    let paths: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(format!("/c/{i}"))).collect();
    pool.request_all(paths.iter());
    // Let the single worker pick up job #0 (blocks on the barrier), the
    // rest stay queued.
    std::thread::sleep(Duration::from_millis(50));
    pool.cancel();
    assert_eq!(pool.workers(), 0, "cancel retires all workers");
    assert_eq!(pool.pending_len(), 0, "queued jobs are dropped");
    // Job #0 may still be parked on the barrier — at most that one
    // in-flight result can arrive; the 9 queued jobs never run.
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    let mut got = 0;
    while std::time::Instant::now() < deadline {
        got += pool.poll().len();
        if got > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // At most the one in-flight job completed; the 9 queued were dropped.
    assert!(got <= 1, "cancel must not run queued jobs, got {got}");
}
