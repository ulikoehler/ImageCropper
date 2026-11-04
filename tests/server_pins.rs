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

/// Once every RAM slot is pinned, further previews are still produced and
/// persisted to disk — they just aren't kept resident. This is the
/// "generate ALL, keep the first 2k in RAM" contract at small scale.
#[test]
fn unpinned_previews_produce_without_ram_retention() {
    use imagecropper::fs_utils::previews::{preview_file_prefix, previews_dir_for};
    use std::time::Duration;
    with_temp_workdir(|root| {
        let mut paths = Vec::new();
        for i in 0..4u32 {
            let p = root.join(format!("b{i}.png"));
            write_image(&p, &solid_image(16, 16, [0, 0, i as u8, 255]));
            paths.push(p);
        }
        // RAM = the pinned set only (the server runs 0 unpinned + PIN_LIMIT).
        let store = store(0, 2);
        store.set_pins(vec![paths[0].clone(), paths[1].clone()]);
        assert!(store.get_or_produce(&paths[0]).is_some());
        assert!(store.get_or_produce(&paths[1]).is_some());

        // RAM is now entirely pinned — a third preview still produces and
        // persists, but is not kept resident.
        assert!(
            store.get_or_produce(&paths[2]).is_some(),
            "produce must not stop at the RAM cap"
        );
        assert!(
            store.cached(&paths[2]).is_none(),
            "unpinned over-cap entry kept in RAM"
        );

        // The background pool path behaves the same: the job completes and
        // the bytes are delivered, they just aren't retained.
        store.request(paths[3].clone());
        let mut got = None;
        for _ in 0..200 {
            for (p, prev) in store.poll() {
                if p == paths[3] {
                    got = Some(prev);
                }
            }
            if got.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(got, Some(Some(_))), "pool produce did not finish");
        assert!(store.cached(&paths[3]).is_none());

        // Over-cap work was still persisted: a later want is a disk hit,
        // not a re-encode.
        for p in &paths[2..] {
            let dir = previews_dir_for(p);
            let prefix = preview_file_prefix(p.file_name().unwrap().to_str().unwrap());
            let on_disk = std::fs::read_dir(&dir).unwrap().any(|e| {
                let n = e.unwrap().file_name().into_string().unwrap();
                n.starts_with(&prefix) && n.ends_with(".jpg")
            });
            assert!(on_disk, "no persisted preview for {}", p.display());
        }
    });
}
