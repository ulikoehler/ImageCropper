//! Client-first IO gate.
//!
//! The web client is latency-sensitive: a thumbnail it is waiting for must
//! not queue behind bulk background IO. Anything that serves a live client
//! request (thumbnail production, full-resolution reads) marks itself as
//! client work while it runs; background writers — delete/move workers,
//! crop/resave, and the speculative preview preload — wait out a short window
//! whenever that marker is set, so they yield IO instead of competing.
//!
//! The wait is always bounded: a continuously busy client must not starve
//! destructive work forever, so a worker resumes after [`CLIENT_PAUSE`] even
//! if the client is still loading.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long a background worker holds off while client work is in flight.
pub const CLIENT_PAUSE: Duration = Duration::from_millis(500);

/// Poll interval used while waiting for client work to drain.
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
pub struct IoGate {
    client_work: AtomicUsize,
}

impl IoGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Mark the current operation as client-servicing. The marker is released
    /// when the returned guard is dropped.
    pub fn client_work(self: &Arc<Self>) -> ClientWork {
        self.client_work.fetch_add(1, Ordering::SeqCst);
        ClientWork {
            gate: Arc::clone(self),
        }
    }

    /// True while at least one client request is being served.
    pub fn busy(&self) -> bool {
        self.client_work.load(Ordering::SeqCst) > 0
    }

    /// Hold off while client work is in flight, at most [`CLIENT_PAUSE`].
    pub fn yield_to_clients(&self) {
        self.yield_for(CLIENT_PAUSE);
    }

    /// Hold off while client work is in flight, at most `max`.
    pub fn yield_for(&self, max: Duration) {
        if !self.busy() {
            return;
        }
        let started = Instant::now();
        while self.busy() && started.elapsed() < max {
            thread::sleep(POLL);
        }
    }
}

/// RAII marker: dropping it releases the client-work slot.
#[derive(Debug)]
pub struct ClientWork {
    gate: Arc<IoGate>,
}

impl Drop for ClientWork {
    fn drop(&mut self) {
        self.gate.client_work.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_gate_does_not_wait() {
        let gate = IoGate::new();
        assert!(!gate.busy());
        let started = Instant::now();
        gate.yield_for(Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn client_work_marks_busy_until_released() {
        let gate = IoGate::new();
        let guard = gate.client_work();
        assert!(gate.busy());
        drop(guard);
        assert!(!gate.busy(), "guard release clears the marker");
    }

    #[test]
    fn wait_is_bounded_while_client_is_busy() {
        let gate = IoGate::new();
        let _guard = gate.client_work();
        let started = Instant::now();
        gate.yield_for(Duration::from_millis(100));
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(100),
            "waits while client work is in flight: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "the wait is bounded: {waited:?}"
        );
    }

    #[test]
    fn wait_ends_as_soon_as_client_work_drains() {
        let gate = IoGate::new();
        let guard = gate.client_work();
        let waiter = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let started = Instant::now();
                gate.yield_for(Duration::from_secs(30));
                started.elapsed()
            })
        };
        thread::sleep(Duration::from_millis(50));
        drop(guard);
        let waited = waiter.join().unwrap();
        assert!(
            waited < Duration::from_secs(2),
            "returns once the client is served: {waited:?}"
        );
    }

    #[test]
    fn concurrent_client_work_counts_are_tracked() {
        let gate = IoGate::new();
        let a = gate.client_work();
        let b = gate.client_work();
        drop(a);
        assert!(gate.busy(), "one request still in flight");
        drop(b);
        assert!(!gate.busy());
    }
}
