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

    // A preview file must have been persisted next to the source.
    let dir = root.join(PREVIEWS_DIR);
    assert!(dir.is_dir());
    let previews: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(previews.len(), 1);
    assert!(previews[0].starts_with(&preview_file_prefix("photo.png")));
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

    // Stale preview must be gone, a fresh one must exist.
    assert!(!stale.exists());
    let remaining: Vec<_> = std::fs::read_dir(&preview_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0], stale.file_name().unwrap().to_string_lossy());
}

#[test]
fn remove_previews_for_deletes_cached_files() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let source = root.join("gone.png");
    std::fs::write(&source, b"x").unwrap();

    let preview_dir = root.join(PREVIEWS_DIR);
    std::fs::create_dir(&preview_dir).unwrap();
    let p1 = preview_dir.join(preview_file_name("gone.png", 1, 1));
    let p2 = preview_dir.join(preview_file_name("gone.png", 2, 2));
    let other = preview_dir.join(preview_file_name("other.png", 1, 1));
    for p in [&p1, &p2, &other] {
        std::fs::write(p, b"x").unwrap();
    }

    remove_previews_for(&source);

    assert!(!p1.exists() && !p2.exists());
    assert!(other.exists(), "unrelated previews must be kept");
}

#[test]
fn remove_previews_for_many_purges_only_listed_sources() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let preview_dir = root.join(PREVIEWS_DIR);
    std::fs::create_dir(&preview_dir).unwrap();

    // Dotted source names exercise the signature-suffix parser.
    let sources: Vec<_> = ["a.b.png", "b.png", "c.v2.png"]
        .iter()
        .map(|name| {
            let p = root.join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        })
        .collect();
    let previews: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p.file_name().unwrap().to_string_lossy();
            let preview = preview_dir.join(preview_file_name(&name, i as u64, i as u128));
            std::fs::write(&preview, b"x").unwrap();
            preview
        })
        .collect();

    remove_previews_for_many([sources[0].as_path(), sources[1].as_path()]);

    assert!(!previews[0].exists() && !previews[1].exists());
    assert!(previews[2].exists(), "unlisted source keeps its preview");
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
