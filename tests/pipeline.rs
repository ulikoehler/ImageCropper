//! ThumbPipeline tests: bounded-queue semantics, read/compute stage
//! overlap, cancellation, and an end-to-end run through the real
//! preview_read/preview_compute_bytes stages.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use imagecropper::fs_utils::pipeline::{BoundedQueue, ReadStage, ThumbPipeline};
use imagecropper::fs_utils::previews::{
    persisted_thumb_for_test, preview_compute_bytes, preview_read, ReadJob,
};
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

fn source_job(n: usize) -> ReadJob {
    ReadJob::Source {
        bytes: vec![0; n],
        len: n as u64,
        nanos: 0,
        sig: 0,
    }
}

fn drain<R: Send + 'static>(pool: &ThumbPipeline<R>, want: usize, timeout: Duration) -> usize {
    let mut got = 0;
    let deadline = Instant::now() + timeout;
    while got < want && Instant::now() < deadline {
        got += pool.poll().len();
        std::thread::sleep(Duration::from_millis(2));
    }
    got
}

#[test]
fn bounded_queue_blocks_past_item_cap() {
    let q = Arc::new(BoundedQueue::<usize>::new(2, 0, |_| 0));
    assert!(q.push(1));
    assert!(q.push(2));
    let q2 = q.clone();
    let t = std::thread::spawn(move || q2.push(3));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!t.is_finished(), "push must block while the queue is full");
    assert_eq!(q.pop_wait(Duration::from_millis(10)), Some(1));
    t.join().unwrap();
    assert_eq!(q.pop_wait(Duration::from_millis(10)), Some(2));
    assert_eq!(q.pop_wait(Duration::from_millis(10)), Some(3));
}

#[test]
fn bounded_queue_blocks_past_byte_cap() {
    let q = Arc::new(BoundedQueue::<Vec<u8>>::new(100, 10, |v| v.len()));
    assert!(q.push(vec![0; 6]));
    assert!(q.push(vec![0; 4]));
    let q2 = q.clone();
    let t = std::thread::spawn(move || q2.push(vec![0; 1]));
    std::thread::sleep(Duration::from_millis(80));
    assert!(!t.is_finished(), "byte cap must block even with items free");
    assert!(q.pop_wait(Duration::from_millis(10)).is_some());
    t.join().unwrap();
}

#[test]
fn bounded_queue_admits_oversized_item_when_empty() {
    let q = BoundedQueue::<Vec<u8>>::new(100, 4, |v| v.len());
    assert!(q.push(vec![0; 64]), "a lone oversized item must never deadlock");
}

#[test]
fn bounded_queue_close_wakes_producers_and_consumers() {
    let q = Arc::new(BoundedQueue::<usize>::new(1, 0, |_| 0));
    assert!(q.push(1));
    let q2 = q.clone();
    let t = std::thread::spawn(move || q2.push(2));
    std::thread::sleep(Duration::from_millis(80));
    q.close();
    assert!(!t.join().unwrap(), "push fails after close");
    assert!(q.pop_wait(Duration::from_millis(10)).is_none());
}

#[test]
fn pipeline_drains_all_jobs_and_dedupes() {
    let pool: ThumbPipeline<u32> =
        ThumbPipeline::new(1, 4, |_| ReadStage::Job(source_job(64)), |_, _| 7);
    let paths: Vec<PathBuf> = (0..200).map(|i| PathBuf::from(format!("/x/{i}"))).collect();
    pool.request_all(paths.iter());
    // Duplicates collapse into the pending entry.
    pool.request(paths[0].clone());
    assert_eq!(drain(&pool, paths.len(), Duration::from_secs(20)), paths.len());
    assert!(pool.workers() >= 1);
    assert_eq!(pool.pending_len(), 0);
}

#[test]
fn read_and_compute_stages_overlap() {
    // Slow read + slow compute: serial execution costs n×(read+compute);
    // the pipeline must finish substantially faster.
    let reads = Arc::new(AtomicUsize::new(0));
    let computes = Arc::new(AtomicUsize::new(0));
    let r = reads.clone();
    let c = computes.clone();
    let pool: ThumbPipeline<()> = ThumbPipeline::new(
        2,
        4,
        move |_| {
            r.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            ReadStage::Job(source_job(8))
        },
        move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
        },
    );
    let paths: Vec<PathBuf> = (0..8).map(|i| PathBuf::from(format!("/o/{i}"))).collect();
    pool.request_all(paths.iter());
    let t0 = Instant::now();
    assert_eq!(drain(&pool, 8, Duration::from_secs(10)), 8);
    // Serial floor: 8×60ms = 480ms. Overlapped stages: ~reads/2 +
    // compute + slack.
    assert!(
        t0.elapsed() < Duration::from_millis(430),
        "stages must overlap, took {:?}",
        t0.elapsed()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 8);
    assert_eq!(computes.load(Ordering::SeqCst), 8);
}

#[test]
fn done_results_skip_the_compute_stage() {
    let computes = Arc::new(AtomicUsize::new(0));
    let c = computes.clone();
    let pool: ThumbPipeline<u32> = ThumbPipeline::new(
        1,
        2,
        |p| {
            if p.ends_with("cached") {
                ReadStage::Done(1)
            } else {
                ReadStage::Job(source_job(4))
            }
        },
        move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
            2
        },
    );
    pool.request(PathBuf::from("/m/cached"));
    pool.request(PathBuf::from("/m/fresh"));
    assert_eq!(drain(&pool, 2, Duration::from_secs(10)), 2);
    assert_eq!(computes.load(Ordering::SeqCst), 1, "only the miss computes");
}

#[test]
fn cancel_stops_workers_and_clears_pending() {
    let barrier = Arc::new(Barrier::new(2));
    let b = barrier.clone();
    let pool: ThumbPipeline<u32> = ThumbPipeline::new(
        1,
        1,
        move |_| {
            b.wait();
            ReadStage::Done(1)
        },
        |_, _| 0,
    );
    let paths: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(format!("/c/{i}"))).collect();
    pool.request_all(paths.iter());
    // Let the reader pick up job #0 (parks on the barrier); the rest sit
    // in backlog/jobs.
    std::thread::sleep(Duration::from_millis(80));
    pool.cancel();
    assert_eq!(pool.workers(), 0, "cancel retires all readers");
    assert_eq!(pool.pending_len(), 0, "queued jobs are dropped");
    barrier.wait(); // release the parked read so its thread can exit
    assert_eq!(drain(&pool, 1, Duration::from_millis(400)), 0);
}

#[test]
fn end_to_end_produces_real_thumbnails() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let paths: Vec<PathBuf> = (0..4)
        .map(|i| {
            let p = root.join(format!("img{i}.png"));
            write_image(&p, &solid_image(64, 32, [i as u8, 0, 255, 255]));
            p
        })
        .collect();

    let pool: ThumbPipeline<Result<(), ()>> = ThumbPipeline::new(
        1,
        4,
        |p| match preview_read(p, None) {
            Err(_) => ReadStage::Done(Err(())),
            Ok(job) => ReadStage::Job(job),
        },
        |p, job| preview_compute_bytes(p, job).map(|_| ()).map_err(|_| ()),
    );
    pool.request_all(paths.iter());
    assert_eq!(drain(&pool, paths.len(), Duration::from_secs(30)), paths.len());
    for p in &paths {
        assert!(
            persisted_thumb_for_test(p).is_some(),
            "pipeline must persist a thumbnail for {}",
            p.display()
        );
    }
}
