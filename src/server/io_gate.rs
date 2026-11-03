//! Client-first IO gate — three priority tiers:
//!
//!   1. **Client work** (highest): anything serving a live web request —
//!      thumbnail production, full-resolution reads — marks `client_work`
//!      for its duration. Nothing ever waits on it; it only *blocks* lower
//!      tiers.
//!   2. **Scan/loading** (one below client): while the album is still
//!      filling in, `set_hold` is set by the scan feeder. It does not wait
//!      on anything either — it only makes tier 3 defer. Preview
//!      production (the work that fills the screen) ignores the hold: it
//!      yields only to actual client requests via [`yield_for`].
//!   3. **Destructive/background** (lowest): delete/move workers and
//!      crop/resave call [`yield_to_priority`] and wait while *either* a
//!      client request is in flight or the scan hold is set.
//!
//! The wait is always bounded: a continuously busy client must not starve
//! destructive work forever, so a worker resumes after [`CLIENT_PAUSE`]
//! even if higher tiers are still active.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long a background worker holds off while client work is in flight.
pub const CLIENT_PAUSE: Duration = Duration::from_millis(500);

/// How long tier-3 work may be held off by the early-scan hold before it
/// proceeds anyway — a failsafe for a stuck scanner, not the normal path
/// (the hold is released at the priority item count or at scan end).
pub const SCAN_HOLD_PAUSE: Duration = Duration::from_secs(30);

/// Poll interval used while waiting for client work to drain.
const POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
pub struct IoGate {
    client_work: AtomicUsize,
    /// Early-scan hold: while the album is still filling in (first batch
    /// of files not indexed yet), destructive work defers entirely so
    /// scan/preview IO wins — not just a 50/50 share of it.
    hold: AtomicBool,
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

    /// Set/clear the early-scan hold. While held, [`yield_to_priority`]
    /// waiters (deletes, moves, crops) back off; preview production and
    /// client requests ignore it — they ARE the priority work.
    pub fn set_hold(&self, held: bool) {
        self.hold.store(held, Ordering::SeqCst);
    }

    /// True while the early-scan hold is set.
    pub fn held(&self) -> bool {
        self.hold.load(Ordering::SeqCst)
    }

    /// Hold off while client work is in flight, at most [`CLIENT_PAUSE`].
    /// Used by preview production: it must not wait on the scan hold —
    /// during the early load phase previews ARE the priority work.
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

    /// Hold off while the scan hold is set, then while a client request
    /// is in flight — the tier-3 path (deletes, moves, crops).
    ///
    /// `hold_max` bounds the hold wait: the scan-priority phase is finite
    /// (released at a fixed item count or scan end), so tier 3 may defer
    /// on it far longer than the [`CLIENT_PAUSE`] it gives a busy client.
    /// The bound only exists so a pathologically stuck scanner can never
    /// starve the queue.
    pub fn yield_to_priority(&self, hold_max: Duration) {
        let started = Instant::now();
        while self.held() && started.elapsed() < hold_max {
            thread::sleep(POLL);
        }
        self.yield_for(CLIENT_PAUSE);
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

    #[test]
    fn scan_hold_defers_destructive_work_only() {
        let gate = IoGate::new();
        gate.set_hold(true);

        // The preview pool's wait ignores the hold — during the early
        // scan previews ARE the priority work.
        let started = Instant::now();
        gate.yield_to_clients();
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "preview wait must not honor the scan hold"
        );

        // The destructive path waits out its bound while held.
        let started = Instant::now();
        gate.yield_to_priority(Duration::from_millis(80));
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(80),
            "destructive work must hold off while the scan holds: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(2),
            "the hold wait is bounded: {waited:?}"
        );

        // Releasing the hold lets the queue proceed immediately.
        gate.set_hold(false);
        let started = Instant::now();
        gate.yield_to_priority(Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_millis(50));
    }
}
