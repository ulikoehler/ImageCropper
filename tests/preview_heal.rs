//! Corrupt persisted previews must self-heal: a truncated/garbage file in
//! `.imagecropper-previews` (killed mid-write, disk damage) is detected,
//! deleted, and regenerated — never served to clients.

use imagecropper::fs_utils::{
    previews::{invalidate_preview_bytes, previews_dir_for, produce_preview_bytes},
};
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

fn the_preview_file(src: &std::path::Path) -> std::path::PathBuf {
    let dir = previews_dir_for(src);
    std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|x| x == "jpg").unwrap_or(false))
        .expect("a persisted preview must exist")
}

#[test]
fn corrupt_persisted_preview_is_regenerated() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("p.png");
    write_image(&src, &solid_image(300, 200, [10, 200, 30, 255]));

    let (jpeg, w, h) = produce_preview_bytes(&src, None).expect("produce");
    assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    assert_eq!((w, h), (300, 200));
    let persisted = the_preview_file(&src);

    // Simulate a kill mid-write: a truncated, signature-correct file.
    std::fs::write(&persisted, &jpeg[..jpeg.len() / 2]).unwrap();

    // Next read must regenerate rather than serve the truncated bytes.
    let (jpeg2, _, _) = produce_preview_bytes(&src, None).expect("re-produce");
    assert_eq!(&jpeg2[0..2], &[0xFF, 0xD8]);
    assert_eq!(&jpeg2[jpeg2.len() - 2..], &[0xFF, 0xD9], "EOI intact");
    assert_eq!(jpeg2, jpeg, "regeneration restores identical bytes");
    // …and the file itself is whole again for the next reader.
    let on_disk = std::fs::read(&persisted).unwrap();
    assert_eq!(&on_disk[0..2], &[0xFF, 0xD8]);
    assert_eq!(&on_disk[on_disk.len() - 2..], &[0xFF, 0xD9]);
}

#[test]
fn garbage_persisted_preview_is_regenerated() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("g.png");
    write_image(&src, &solid_image(120, 80, [0, 0, 0, 255]));
    produce_preview_bytes(&src, None).expect("produce");
    let persisted = the_preview_file(&src);

    std::fs::write(&persisted, b"not a jpeg at all").unwrap();
    let (jpeg, _, _) = produce_preview_bytes(&src, None).expect("re-produce");
    assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
}

#[test]
fn invalidate_preview_bytes_removes_persisted_file() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("i.png");
    write_image(&src, &solid_image(64, 64, [255, 255, 0, 255]));
    produce_preview_bytes(&src, None).expect("produce");
    let persisted = the_preview_file(&src);

    invalidate_preview_bytes(&src);
    assert!(!persisted.exists(), "preview file must be deleted");

    // Producing again must recreate it (next request regenerates).
    produce_preview_bytes(&src, None).expect("re-produce");
    assert!(persisted.exists());
}

#[test]
fn preview_store_regenerate_replaces_corrupt_bytes() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("s.png");
    write_image(&src, &solid_image(200, 120, [128, 64, 255, 255]));

    let store = imagecropper::server::PreviewStore::new(2, 16);
    let good = store.get_or_produce(&src).expect("produce");
    assert_eq!(&good.jpeg[0..2], &[0xFF, 0xD8]);

    // Corrupt BOTH layers: the persisted file and the RAM entry.
    let persisted = the_preview_file(&src);
    std::fs::write(&persisted, b"").unwrap();
    let corrupt = imagecropper::server::Preview {
        jpeg: std::sync::Arc::new(b"broken".to_vec()),
        w: 1,
        h: 1,
    };
    store.ram_insert_for_test(src.clone(), corrupt);

    let healed = store.regenerate(&src).expect("regenerate");
    assert_eq!(&healed.jpeg[0..2], &[0xFF, 0xD8]);
    assert_eq!(healed.jpeg.len(), good.jpeg.len());
}

#[test]
fn preview_persists_content_hash_sidecar() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("h.png");
    write_image(&src, &solid_image(90, 60, [1, 2, 3, 255]));
    produce_preview_bytes(&src, None).expect("produce");

    let preview = the_preview_file(&src);
    let sidecar = std::path::PathBuf::from(format!("{}.sha256", preview.display()));
    let stored = std::fs::read_to_string(&sidecar).expect("hash sidecar must exist");
    let expected = imagecropper::fs_utils::previews::source_content_hash(&src).unwrap();
    assert_eq!(stored.trim(), expected, "sidecar must hold the source sha256");
    assert_eq!(
        imagecropper::fs_utils::previews::stored_content_hash(&src).as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn stale_signature_ignored_when_content_is_unchanged() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("s.png");
    write_image(&src, &solid_image(120, 90, [9, 9, 9, 255]));
    produce_preview_bytes(&src, None).expect("produce");
    let sig_before = imagecropper::fs_utils::previews::preview_sig_u64(&src).unwrap();

    // Touch the file: mtime (and thus the cheap signature) changes, content
    // does not. Network mounts do this constantly.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    filetime_touch(&src);
    let sig_after = imagecropper::fs_utils::previews::preview_sig_u64(&src).unwrap();
    assert_ne!(sig_before, sig_after, "signature must have changed");

    // The stored content hash proves nothing changed — staleness ignored.
    imagecropper::server::actions::verify_sig(&src, sig_before)
        .expect("same bytes must not be reported as stale");
}

#[test]
fn missing_hash_sidecar_records_one_and_allows_the_action() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("m.png");
    write_image(&src, &solid_image(80, 60, [4, 5, 6, 255]));
    // No preview has ever been produced → no recorded hash.
    assert!(imagecropper::fs_utils::previews::stored_content_hash(&src).is_none());

    std::thread::sleep(std::time::Duration::from_millis(1100));
    filetime_touch(&src);
    let sig_now = imagecropper::fs_utils::previews::preview_sig_u64(&src).unwrap();
    // A stale sig with no hash to compare against must not block the action
    // (the tile would otherwise vanish then reappear when the op is refused).
    imagecropper::server::actions::verify_sig(&src, sig_now.wrapping_sub(1))
        .expect("no recorded hash → action allowed");

    // …and the decision is now exact: the hash was recorded.
    let recorded = imagecropper::fs_utils::previews::stored_content_hash(&src);
    assert!(recorded.is_some(), "verification must record the hash");
    assert_eq!(
        recorded,
        imagecropper::fs_utils::previews::source_content_hash(&src)
    );
}

#[test]
fn stale_signature_enforced_when_content_changed() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("c.png");
    write_image(&src, &solid_image(100, 100, [0, 0, 0, 255]));
    produce_preview_bytes(&src, None).expect("produce");
    let sig_before = imagecropper::fs_utils::previews::preview_sig_u64(&src).unwrap();

    // Different content, same path.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write_image(&src, &solid_image(140, 70, [255, 0, 0, 255]));

    let err = imagecropper::server::actions::verify_sig(&src, sig_before)
        .expect_err("changed content must be rejected");
    assert!(err.to_string().contains("stale signature"), "got: {err}");
}

/// Set the file's mtime to now (cheap, no extra deps).
fn filetime_touch(p: &std::path::Path) {
    let out = std::process::Command::new("touch").arg(p).status();
    assert!(out.map(|s| s.success()).unwrap_or(false), "touch failed");
}

#[test]
fn concurrent_produce_of_same_path_all_succeed() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("c.png");
    write_image(&src, &solid_image(200, 150, [60, 160, 220, 255]));
    let store = std::sync::Arc::new(imagecropper::server::PreviewStore::new(4, 16));

    // 8 threads race the same missing preview — the in-flight slot dedups
    // them onto one production; every caller must still get valid bytes.
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let src = src.clone();
            handles.push(s.spawn(move || store.get_or_produce(&src)));
        }
        let mut first = None;
        for h in handles {
            let p = h.join().unwrap().expect("produce");
            assert_eq!(&p.jpeg[0..2], &[0xFF, 0xD8]);
            if let Some(f) = &first {
                assert_eq!(&p.jpeg, f);
            } else {
                first = Some(p.jpeg);
            }
        }
    });
    assert_eq!(store.inflight_len(), 0, "no leaked in-flight slots");
}
