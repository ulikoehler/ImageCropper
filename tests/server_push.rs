use imagecropper::server::PushQueue;

fn drain(q: &mut PushQueue) -> Vec<u32> {
    let mut out = Vec::new();
    while q.may_send() {
        match q.next_id() {
            Some(id) => {
                q.commit_send(id, 1);
                out.push(id);
            }
            None => break,
        }
    }
    out
}

#[test]
fn wants_beat_boosts_beat_cursor() {
    let mut q = PushQueue::new(10, 1_000_000);
    q.viewport(3, 5, 0); // boosts ids 3,4,5
    q.want([8]);
    let order = drain(&mut q);
    assert_eq!(order[0], 8, "want first");
    assert!(
        order[1..4].iter().all(|id| (3..=5).contains(id)),
        "boosts next"
    );
    assert_eq!(
        &order[4..],
        &[0, 1, 2, 6, 7, 9],
        "cursor sweep covers the rest"
    );
}

#[test]
fn credit_gates_sends() {
    let mut q = PushQueue::new(3, 2);
    assert!(q.may_send());
    let a = q.next_id().unwrap();
    q.commit_send(a, 1);
    let b = q.next_id().unwrap();
    q.commit_send(b, 1);
    assert!(!q.may_send(), "credit exhausted → pump stalls");
    q.add_credit(5);
    assert!(q.may_send());
}

#[test]
fn pause_blocks_sends() {
    let mut q = PushQueue::new(3, 100);
    q.set_paused(true);
    assert!(!q.may_send());
    assert_eq!(q.next_id(), None);
    q.set_paused(false);
    assert!(q.may_send());
    assert_eq!(q.next_id(), Some(0));
}

#[test]
fn pushed_ids_not_resent() {
    let mut q = PushQueue::new(5, 100);
    q.mark_pushed(2); // e.g. served via WANT fast path
    q.want([2, 3]); // want includes an already-pushed id
    let order = drain(&mut q);
    assert_eq!(order, vec![3, 0, 1, 4], "id 2 skipped everywhere");
    assert!(q.is_drained());
}

#[test]
fn focus_boosts_neighborhood() {
    let mut q = PushQueue::new(20, 1_000_000);
    q.focus(10);
    let first = q.next_id().unwrap();
    assert!((8..=12).contains(&first));
}

#[test]
fn sweep_preloads_at_most_2000_ids() {
    let mut q = PushQueue::new(5000, 1_000_000_000);
    let order = drain(&mut q);
    assert_eq!(order.len(), 2000, "sweep capped at SWEEP_LIMIT");
    assert!(order.iter().all(|&id| id < 2000));
    assert!(q.is_drained());

    // Explicit wants and boosts still reach ids beyond the limit.
    q.want([4000]);
    q.focus(4500);
    let mut extra = Vec::new();
    while let Some(id) = q.next_id() {
        q.commit_send(id, 1);
        extra.push(id);
    }
    assert_eq!(extra[0], 4000, "explicit want beats boost");
    assert!(extra[1..].iter().all(|&id| (4498..=4502).contains(&id)));
}

#[test]
fn reset_clears_stale_push_state() {
    let mut q = PushQueue::new(10, 1_000_000);
    q.want([1]);
    q.focus(5);
    drain(&mut q);
    assert!(q.is_drained());
    q.reset(10);
    let order = drain(&mut q);
    assert_eq!(order.len(), 10, "reset restarts the sweep on the new ids");
}

#[test]
fn viewport_boosts_span_and_direction() {
    let mut q = PushQueue::new(20, 1_000_000);
    q.viewport(10, 12, 1); // forward scroll → lookahead after 12
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(q.next_id().unwrap());
    }
    assert!(
        seen.iter().all(|i| (10..=14).contains(i)),
        "viewport + forward lookahead before cursor: {seen:?}"
    );
}
