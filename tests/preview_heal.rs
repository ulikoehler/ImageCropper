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
