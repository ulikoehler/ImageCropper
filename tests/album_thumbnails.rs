use std::path::Path;
use std::time::{Duration, Instant};

use imagecropper::album::thumbnails::{
    preview_file_name, preview_file_prefix, previews_dir_for, remove_previews_for,
    remove_previews_for_many, Thumbnailer, THUMB_MAX_DIM,
};
use imagecropper::fs_utils::PREVIEWS_DIR;
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

#[test]
fn preview_names_embed_the_source_signature() {
    let name = preview_file_name("img.png", 1234, 5678);
    assert_eq!(name, "img.png.1234-5678.jpg");
    assert!(name.starts_with(&preview_file_prefix("img.png")));
    // A different signature produces a different name -> automatic staleness.
    assert_ne!(name, preview_file_name("img.png", 1234, 9999));
}

#[test]
fn thumbnailer_generates_and_persists_previews() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let image_path = root.join("photo.png");
    write_image(&image_path, &solid_image(2000, 1000, [255, 0, 0, 255]));

    let mut thumbnailer = Thumbnailer::new(2);
    thumbnailer.request(image_path.clone());

    // Wait for the worker to deliver a thumbnail.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut result = None;
    while Instant::now() < deadline {
        if let Some(r) = thumbnailer.poll().pop() {
            result = Some(r);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = result.expect("thumbnail job did not finish");
    let thumb = result.image.expect("thumbnail generation failed");

    // Aspect preserved, downscaled to THUMB_MAX_DIM.
    assert_eq!(thumb.size[0] * 2, thumb.size[1] * 4);
    assert!(thumb.size[0] <= THUMB_MAX_DIM as usize);
    assert_eq!(thumb.size[0], THUMB_MAX_DIM as usize);

    // The thumbnail must be persisted in the directory pack — one
    // `thumbs.pack`, not per-image files.
    let dir = root.join(PREVIEWS_DIR);
    assert!(dir.is_dir());
    assert!(dir.join("thumbs.pack").is_file());
    let persisted = imagecropper::fs_utils::previews::persisted_thumb_for_test(&image_path)
        .expect("pack must hold the thumbnail");
    assert_eq!(&persisted[0..2], &[0xFF, 0xD8]);
    // …and its sha256 metadata is a pack record, queryable by source.
    assert!(
        imagecropper::fs_utils::previews::stored_content_hash(&image_path).is_some(),
        "content hash must be recorded in the pack"
    );
    assert_eq!(previews_dir_for(&image_path), dir);
}

#[test]
fn changed_source_produces_fresh_preview() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let image_path = root.join("photo.png");

    // Seed a stale preview file with a signature that no longer matches.
    let preview_dir = root.join(PREVIEWS_DIR);
    std::fs::create_dir(&preview_dir).unwrap();
    let stale = preview_dir.join(preview_file_name("photo.png", 1, 1));
    std::fs::write(&stale, b"stale").unwrap();

    write_image(&image_path, &solid_image(64, 64, [0, 255, 0, 255]));

    let mut thumbnailer = Thumbnailer::new(1);
    thumbnailer.request(image_path.clone());
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && thumbnailer.poll().is_empty() {
        std::thread::sleep(Duration::from_millis(20));
    }

    // The stale flat file is swept on pack open, and the fresh thumbnail
    // lands in the pack.
    assert!(!stale.exists());
    assert!(
        imagecropper::fs_utils::previews::persisted_thumb_for_test(&image_path).is_some(),
        "fresh preview must be in the pack"
    );
}

#[test]
fn remove_previews_for_deletes_cached_files() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let source = root.join("gone.png");
    let other = root.join("other.png");
    write_image(&source, &solid_image(16, 16, [1, 1, 1, 255]));
    write_image(&other, &solid_image(16, 16, [2, 2, 2, 255]));
    imagecropper::fs_utils::previews::produce_preview_bytes(&source, None).unwrap();
    imagecropper::fs_utils::previews::produce_preview_bytes(&other, None).unwrap();
    assert!(imagecropper::fs_utils::previews::persisted_thumb_for_test(&source).is_some());
    assert!(imagecropper::fs_utils::previews::persisted_thumb_for_test(&other).is_some());

    remove_previews_for(&source);

    assert!(
        imagecropper::fs_utils::previews::persisted_thumb_for_test(&source).is_none(),
        "tombstoned record must not resolve"
    );
    assert!(
        imagecropper::fs_utils::previews::persisted_thumb_for_test(&other).is_some(),
        "unrelated previews must be kept"
    );
}

#[test]
fn remove_previews_for_many_purges_only_listed_sources() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Dotted source names exercise the record key handling.
    let sources: Vec<_> = ["a.b.png", "b.png", "c.v2.png"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let p = root.join(name);
            write_image(&p, &solid_image(12, 12, [i as u8, 0, 0, 255]));
            imagecropper::fs_utils::previews::produce_preview_bytes(&p, None).unwrap();
            p
        })
        .collect();

    remove_previews_for_many([sources[0].as_path(), sources[1].as_path()]);

    for p in &sources[..2] {
        assert!(
            imagecropper::fs_utils::previews::persisted_thumb_for_test(p).is_none(),
            "{} must be tombstoned",
            p.display()
        );
    }
    assert!(
        imagecropper::fs_utils::previews::persisted_thumb_for_test(&sources[2]).is_some(),
        "unlisted source keeps its preview"
    );
}

#[test]
fn thumbnailer_reports_failures_for_undecodable_files() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("broken.png");
    std::fs::write(&path, b"not an image").unwrap();

    let mut thumbnailer = Thumbnailer::new(1);
    thumbnailer.request(path);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut result = None;
    while Instant::now() < deadline {
        if let Some(r) = thumbnailer.poll().pop() {
            result = Some(r);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(result.unwrap().image.is_none());
}

#[test]
fn previews_dir_is_next_to_the_source() {
    let p = Path::new("/some/dir/pic.jpg");
    assert_eq!(
        previews_dir_for(p),
        Path::new("/some/dir").join(PREVIEWS_DIR)
    );
}
