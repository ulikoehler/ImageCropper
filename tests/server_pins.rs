//! The top of the album stays resident in RAM: pinned previews are never
//! evicted and are re-queued when missing, so the browser can load them
//! without touching the disk.

use imagecropper::fs_utils::previews::produce_preview_bytes;
use imagecropper::server::{IoGate, PreviewStore, ServeMode, ServerState};

mod common;
use common::{solid_image, with_temp_workdir, write_image};

fn store(ram_cache: usize, pin_limit: usize) -> PreviewStore {
    PreviewStore::with_limits(1, ram_cache, pin_limit, IoGate::new())
}

/// Pinned previews survive cache pressure from unpinned ones.
#[test]
fn pinned_previews_are_never_evicted() {
    with_temp_workdir(|root| {
        let mut paths = Vec::new();
        for i in 0..8u32 {
            let p = root.join(format!("img{i}.png"));
            write_image(&p, &solid_image(16, 16, [i as u8, 0, 0, 255]));
            produce_preview_bytes(&p, None).unwrap();
            paths.push(p);
        }

        // Room for 2 unpinned entries plus 2 pinned.
        let store = store(2, 2);
        let missing = store.set_pins(vec![paths[0].clone(), paths[1].clone()]);
        assert_eq!(missing, 2, "nothing produced yet");
        assert_eq!(store.pinned_count(), 2);
        assert!(store.is_pinned(&paths[0]));

        // Fill the RAM cache: the pinned two first, then pressure.
        for p in &paths {
            assert!(store.get_or_produce(p).is_some());
        }

        assert!(
            store.cached(&paths[0]).is_some(),
            "pinned preview evicted by unpinned pressure"
        );
        assert!(store.cached(&paths[1]).is_some(), "pinned preview evicted");
        assert_eq!(store.missing_pins().len(), 0, "pins are resident");
    });
}

/// Unpinned entries are still evicted, so the cache stays bounded.
#[test]
fn unpinned_previews_still_rotate() {
    with_temp_workdir(|root| {
        let mut paths = Vec::new();
        for i in 0..6u32 {
            let p = root.join(format!("u{i}.png"));
            write_image(&p, &solid_image(16, 16, [0, i as u8, 0, 255]));
            produce_preview_bytes(&p, None).unwrap();
            paths.push(p);
        }
        let store = store(2, 0);
        for p in &paths {
            assert!(store.get_or_produce(p).is_some());
        }
        let cached: Vec<_> = paths.iter().filter(|p| store.cached(p).is_some()).collect();
        assert_eq!(cached.len(), 2, "capacity is respected");
        assert!(
            store.cached(&paths[0]).is_none(),
            "oldest unpinned entry was evicted"
        );
    });
}

/// The server pins the first live items, skipping tombstones.
#[test]
fn server_pins_the_first_live_items() {
    with_temp_workdir(|root| {
        let mut files = Vec::new();
        for i in 0..5u32 {
            let p = root.join(format!("p{i}.png"));
            write_image(&p, &solid_image(8, 8, [i as u8, 2, 2, 255]));
            files.push(p);
        }
        let state = ServerState::new(
            files.clone(),
            vec![root.to_path_buf()],
            ServeMode::Album,
            imagecropper::cli::MouseSelectMode::Sequence,
            1,
            None,
            4096,
            None,
        );
        state.refresh_pins();
        assert_eq!(state.previews.pinned_count(), 5);
        for f in &files {
            assert!(state.previews.is_pinned(f), "{} not pinned", f.display());
        }

        // A tombstoned item is no longer part of the pinned top.
        state.remove_path(&files[1]);
        state.refresh_pins();
        assert_eq!(state.previews.pinned_count(), 4);
        assert!(!state.previews.is_pinned(&files[1]));
        assert!(state.previews.is_pinned(&files[2]));
    });
}
