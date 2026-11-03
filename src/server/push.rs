//! PushScheduler: per-client ordering of preview pushes.
//!
//! Send order: explicit WANT ids (FIFO) → hint-boosted ids by score →
//! manifest-cursor sweep. Sending is gated by the client's credit budget.

use std::collections::{BTreeMap, HashSet, VecDeque};

/// The background sweep preloads only the first `SWEEP_LIMIT` manifest
/// ids — enough thumbnails to scroll through before on-demand wants
/// take over. Deeper items still arrive via explicit wants/boosts.
pub const SWEEP_LIMIT: usize = 2000;

/// One connected client's push state.
pub struct PushQueue {
    /// Explicitly requested ids (highest priority).
    wants: VecDeque<u32>,
    /// Hinted ids (viewport/focus) with a score.
    boosts: BTreeMap<u32, u64>,
    /// Background sweep position (manifest order).
    cursor: usize,
    /// Client-granted byte budget for pushes.
    credit: u64,
    /// Push suspended (hidden tab / scroll fling).
    paused: bool,
    /// Ids already pushed (avoids resending on the sweep).
    pushed: HashSet<u32>,
    /// Total ids in this session's manifest.
    count: usize,
}

impl PushQueue {
    pub fn new(count: usize, initial_credit: u64) -> Self {
        Self {
            wants: VecDeque::new(),
            boosts: BTreeMap::new(),
            cursor: 0,
            credit: initial_credit,
            paused: false,
            pushed: HashSet::new(),
            count,
        }
    }

    pub fn want(&mut self, ids: impl IntoIterator<Item = u32>) {
        for id in ids {
            if self.pushed.contains(&id) {
                continue;
            }
            self.wants.push_back(id);
        }
    }

    /// VIEWPORT hint: boost `first..=last`; `dir` extends the boost one span
    /// in the scroll direction.
    pub fn viewport(&mut self, first: u32, last: u32, dir: i8) {
        let span = last.saturating_sub(first).max(1);
        for id in first..=last {
            self.boosts.insert(id, 1000);
        }
        let (ext_first, ext_last) = if dir > 0 {
            (last + 1, last + span)
        } else if dir < 0 {
            (first.saturating_sub(span), first.saturating_sub(1))
        } else {
            (0, 0)
        };
        for id in ext_first..=ext_last {
            self.boosts.insert(id, 500);
        }
    }

    /// FOCUS hint: strong boost on id and its neighbours.
    pub fn focus(&mut self, id: u32) {
        for n in id.saturating_sub(2)..=id + 2 {
            if (n as usize) < self.count {
                self.boosts.insert(n, 2000);
            }
        }
    }

    /// Update the total id space — a streaming scan appends ids, so the
    /// sweep picks up newly discovered items.
    pub fn set_count(&mut self, count: usize) {
        self.count = count;
    }

    /// A manifest resync reassigned every id — queued wants/boosts and the
    /// pushed-set are stale (they name files that no longer exist at those
    /// ids). Restart the sweep on the new id space; credit is untouched.
    pub fn reset(&mut self, count: usize) {
        self.wants.clear();
        self.boosts.clear();
        self.pushed.clear();
        self.cursor = 0;
        self.count = count;
    }

    pub fn add_credit(&mut self, bytes: u64) {
        self.credit = self.credit.saturating_add(bytes);
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Whether the push pump may send right now (credit available, not paused).
    pub fn may_send(&self) -> bool {
        !self.paused && self.credit > 0
    }

    /// Pop the next id to send (wants → boosts → cursor sweep).
    /// Does not touch credits — the caller produces the payload, then calls
    /// [`commit_send`] with its actual size.
    pub fn next_id(&mut self) -> Option<u32> {
        if self.paused {
            return None;
        }
        self.dequeue()
    }

    /// Record that `id` was sent at `bytes` — decrements the credit budget
    /// and marks the id pushed (or skipped).
    pub fn commit_send(&mut self, id: u32, bytes: u64) {
        self.credit = self.credit.saturating_sub(bytes);
        self.pushed.insert(id);
    }

    /// Mark an id handled without a push (e.g. sent via the WANT fast path,
    /// or failed to produce — avoids retry loops).
    pub fn mark_pushed(&mut self, id: u32) {
        self.pushed.insert(id);
    }

    fn dequeue(&mut self) -> Option<u32> {
        while let Some(id) = self.wants.pop_front() {
            if !self.pushed.contains(&id) {
                return Some(id);
            }
        }
        if let Some((&id, _)) = self
            .boosts
            .iter()
            .filter(|(id, _)| !self.pushed.contains(id))
            .max_by_key(|(_, score)| *score)
        {
            self.boosts.remove(&id);
            return Some(id);
        }
        while self.cursor < self.count.min(SWEEP_LIMIT) {
            let id = self.cursor as u32;
            self.cursor += 1;
            if !self.pushed.contains(&id) {
                return Some(id);
            }
        }
        None
    }

    /// JSON snapshot for `/debugz` — what the pump sees right now.
    pub fn dbg_json(&self) -> serde_json::Value {
        serde_json::json!({
            "wants": self.wants.len(),
            "boosts": self.boosts.len(),
            "cursor": self.cursor,
            "count": self.count,
            "sweep_cap": self.count.min(SWEEP_LIMIT),
            "pushed": self.pushed.len(),
            "credit": self.credit,
            "paused": self.paused,
        })
    }

    /// True when every pushable id has been served (wants and boosts may
    /// still carry ids beyond the sweep limit).
    pub fn is_drained(&self) -> bool {
        self.pushed.len() >= self.count.min(SWEEP_LIMIT)
    }

    pub fn pushed_count(&self) -> usize {
        self.pushed.len()
    }
}
