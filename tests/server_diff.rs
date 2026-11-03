//! Multi-client differential updates: every subscriber sees the same
//! `manifest-patch` events for upserts, tombstone removals and resyncs —
//! the mechanism that keeps simultaneous clients coherent without reloads.

use imagecropper::server::manifest::ManifestPatch;
use imagecropper::server::{ServeMode, ServerState};

mod common;
use common::{solid_image, write_image};

fn recv_patch(
    rx: &mut tokio::sync::broadcast::Receiver<imagecropper::server::ServerEvent>,
) -> ManifestPatch {
    // Drain until a manifest-patch event arrives (thumb-ready etc. skipped).
    for _ in 0..64 {
        let imagecropper::server::ServerEvent::Json(json) =
            rx.try_recv().expect("expected manifest-patch event");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        if v["ev"] == "manifest-patch" {
            return serde_json::from_value(v["patch"].clone()).unwrap();
        }
    }
    panic!("no manifest-patch in event stream");
}

#[test]
fn upsert_and_remove_broadcast_to_all_subscribers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_image(root.join("a.png"), &solid_image(10, 10, [1, 2, 3, 255]));

    // Empty manifest — as a streaming server starts.
    let state = ServerState::new(
        Vec::new(),
        vec![root.to_path_buf()],
        ServeMode::Album,
        imagecropper::cli::MouseSelectMode::Sequence,
        2,
        None,
        4096,
        None,
    );

    // Two "clients" subscribed simultaneously.
    let mut rx1 = state.events.subscribe();
    let mut rx2 = state.events.subscribe();

    // Upsert: both subscribers get the identical patch, id 0.
    state.upsert_files(vec![(root.join("a.png"), None)]);
    for rx in [&mut rx1, &mut rx2] {
        let patch = recv_patch(rx);
        assert_eq!(patch.upsert.len(), 1);
        assert_eq!(patch.upsert[0].id, 0);
        assert_eq!(patch.upsert[0].name, "a.png");
        assert!(!patch.resync);
    }
    assert_eq!(state.item_count(), 1);
    assert_eq!(state.position_of(&root.join("a.png")), Some(0));

    // Second file appends at id 1 — positional, both clients patched.
    write_image(root.join("b.png"), &solid_image(5, 5, [9, 9, 9, 255]));
    state.upsert_files(vec![(root.join("b.png"), None)]);
    for rx in [&mut rx1, &mut rx2] {
        let patch = recv_patch(rx);
        assert_eq!(patch.upsert.len(), 1);
        assert_eq!(patch.upsert[0].id, 1);
    }

    // Remove: tombstone patch — the id stays positional (items[0] remains).
    state.remove_ids(&[0]);
    for rx in [&mut rx1, &mut rx2] {
        let patch = recv_patch(rx);
        assert_eq!(patch.remove, vec![0]);
    }
    {
        let m = state.manifest.read().unwrap();
        assert_eq!(m.items.len(), 2, "tombstone keeps positional id");
        assert!(m.items[0].removed);
        assert!(m.path_of(0).is_none());
        assert_eq!(m.path_of(1).unwrap(), root.join("b.png"));
    }
}

#[test]
fn marked_for_deletion_stays_out_of_rebuilds_and_upserts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_image(root.join("a.png"), &solid_image(10, 10, [1, 2, 3, 255]));
    let path_a = root.join("a.png");

    let state = ServerState::new(
        vec![path_a.clone()],
        vec![root.to_path_buf()],
        ServeMode::Album,
        imagecropper::cli::MouseSelectMode::Sequence,
        1,
        None,
        4096,
        None,
    );
    let mut rx = state.events.subscribe();

    // Dispatch a delete and eagerly tombstone it, as the /data handler does.
    let sig = imagecropper::fs_utils::previews::preview_sig_u64(&path_a).unwrap();
    state.actions.dispatch(
        1,
        imagecropper::server::Action::Delete { id: 0, sig },
        path_a.clone(),
    );
    state.remove_path(&path_a);

    assert!(state.actions.is_pending(&path_a));
    {
        let m = state.manifest.read().unwrap();
        assert!(m.items[0].removed, "queued delete is tombstoned eagerly");
        assert!(m.path_of(0).is_none());
    }

    // A rescan while the delete is still queued must not resurrect it.
    state.rebuild_manifest(vec![path_a.clone()], &[root.to_path_buf()]);
    {
        let m = state.manifest.read().unwrap();
        assert!(m.items.is_empty(), "pending deletes stay out of rebuilds");
    }
    // Nor may a (stray) upsert re-add it.
    assert_eq!(state.upsert_files(vec![(path_a.clone(), None)]), 0);

    // Once the worker finishes, the path is no longer marked pending.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let done = state.actions.poll();
        if !done.is_empty() {
            assert!(done[0].ok, "delete failed: {:?}", done[0].error);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "action timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!state.actions.is_pending(&path_a));
    assert!(!path_a.exists());
    let _ = rx; // silence: subscription kept so events had a receiver
}

#[test]
fn restore_path_revives_a_failed_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_image(root.join("a.png"), &solid_image(10, 10, [4, 5, 6, 255]));
    let path_a = root.join("a.png");

    let state = ServerState::new(
        vec![path_a.clone()],
        vec![root.to_path_buf()],
        ServeMode::Album,
        imagecropper::cli::MouseSelectMode::Sequence,
        1,
        None,
        4096,
        None,
    );
    let mut rx = state.events.subscribe();
    state.remove_path(&path_a);
    {
        let m = state.manifest.read().unwrap();
        assert!(m.items[0].removed);
    }
    let patch = recv_patch(&mut rx);
    assert_eq!(patch.remove, vec![0], "tombstone broadcasts a remove patch");

    // Failed action → the file is still there → the tombstone is undone
    // and an upsert patch makes the item live again.
    state.restore_path(&path_a);
    {
        let m = state.manifest.read().unwrap();
        assert!(!m.items[0].removed, "restored item is live");
        assert_eq!(m.path_of(0).unwrap(), path_a);
        assert_eq!(m.position_of(&path_a), Some(0));
    }
    let patch = recv_patch(&mut rx);
    assert_eq!(patch.upsert.len(), 1);
    assert_eq!(patch.upsert[0].id, 0);
    assert!(!patch.upsert[0].removed);

    // Restoring a path that was compacted out by a rescan re-appends it.
    state.remove_path(&path_a);
    state.rebuild_manifest(vec![], &[root.to_path_buf()]);
    state.restore_path(&path_a);
    {
        let m = state.manifest.read().unwrap();
        assert_eq!(m.items.len(), 1, "restore re-adds a compacted slot");
        assert!(!m.items[0].removed);
        assert_eq!(m.path_of(0).unwrap(), path_a);
    }
}

#[test]
fn finish_scan_broadcasts_resync_patch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Names order differently under filename sort vs append order.
    write_image(root.join("z.png"), &solid_image(8, 8, [0, 0, 0, 255]));
    write_image(root.join("a.png"), &solid_image(8, 8, [0, 0, 0, 255]));

    let state = ServerState::new(
        Vec::new(),
        vec![root.to_path_buf()],
        ServeMode::Album,
        imagecropper::cli::MouseSelectMode::Sequence,
        1,
        None,
        4096,
        None,
    );
    let mut rx = state.events.subscribe();

    // Files discovered in reverse alphabetical order (filesystem order).
    state.upsert_files(vec![(root.join("z.png"), None), (root.join("a.png"), None)]);
    let _ = recv_patch(&mut rx);

    // Finish: final sort reassigns ids → resync patch carries everything.
    state.finish_scan(imagecropper::cli::SortOrder::Filename, false);
    let patch = recv_patch(&mut rx);
    assert!(patch.resync, "finish_scan must emit a resync patch");
    assert_eq!(patch.upsert.len(), 2);
    assert_eq!(patch.upsert[0].name, "a.png", "sorted order");
    assert_eq!(patch.upsert[1].name, "z.png");
    // Server state agrees.
    let m = state.manifest.read().unwrap();
    assert_eq!(m.items[0].name, "a.png");
    assert_eq!(m.path_of(0).unwrap(), root.join("a.png"));
}

/// A delete that already landed must not come back when a rescan that was
/// started *before* the rename delivers its (stale) listing. This is what
/// made already-deleted files reappear in the web UI.
#[test]
fn a_completed_delete_stays_gone_through_stale_rescans() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let path_a = root.join("a.png");
    write_image(&path_a, &solid_image(10, 10, [7, 8, 9, 255]));

    let state = ServerState::new(
        vec![path_a.clone()],
        vec![root.to_path_buf()],
        ServeMode::Album,
        imagecropper::cli::MouseSelectMode::Sequence,
        1,
        None,
        4096,
        None,
    );
    assert_eq!(state.item_count(), 1);

    // Eager tombstone (as /data does on CL_ACTION), then the rename lands.
    state.remove_path(&path_a);
    std::fs::remove_file(&path_a).unwrap();

    // Stale listing from a scan that enumerated the directory earlier.
    state.rebuild_manifest(vec![path_a.clone()], &[root.to_path_buf()]);
    assert!(
        state.manifest.read().unwrap().items.is_empty(),
        "a departed path must not be re-listed"
    );

    // Nor may a late scan-feeder batch re-add it.
    assert_eq!(state.upsert_files(vec![(path_a.clone(), None)]), 0);
    assert_eq!(state.position_of(&path_a), None);

    // A genuinely new file at the same path is listed again.
    write_image(&path_a, &solid_image(12, 12, [1, 1, 1, 255]));
    assert_eq!(
        state.upsert_files(vec![(path_a.clone(), None)]),
        1,
        "a re-created file is new content, not the deleted one"
    );
    assert!(state.position_of(&path_a).is_some());
}
