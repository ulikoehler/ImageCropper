//! Multi-client differential updates: every subscriber sees the same
//! `manifest-patch` events for upserts, tombstone removals and resyncs —
//! the mechanism that keeps simultaneous clients coherent without reloads.

use imagecropper::server::{ServeMode, ServerState};
use imagecropper::server::manifest::ManifestPatch;

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
        2,
        None,
        4096,
        None,
    );

    // Two "clients" subscribed simultaneously.
    let mut rx1 = state.events.subscribe();
    let mut rx2 = state.events.subscribe();

    // Upsert: both subscribers get the identical patch, id 0.
    state.upsert_files(vec![root.join("a.png")]);
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
    state.upsert_files(vec![root.join("b.png")]);
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
        1,
        None,
        4096,
        None,
    );
    let mut rx = state.events.subscribe();

    // Files discovered in reverse alphabetical order (filesystem order).
    state.upsert_files(vec![root.join("z.png"), root.join("a.png")]);
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
