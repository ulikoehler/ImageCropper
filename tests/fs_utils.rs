use imagecropper::fs_utils::*;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

mod common;
use common::with_temp_workdir;

#[test]
fn collect_images_includes_supported_extensions() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let supported = ["image1.png", "photo.jpg", "scan.JPEG", "pic.TiF"]; // mix of cases
    for name in supported {
        fs::write(root.join(name), []).unwrap();
    }
    let unsupported = ["doc.txt", "movie.mp4", "README"]; // should be ignored
    for name in unsupported {
        fs::write(root.join(name), []).unwrap();
    }

    let mut files = collect_images(root).unwrap();
    files.sort();

    let mut expected: Vec<_> = supported.iter().map(|n| root.join(n)).collect();
    expected.sort();
    assert_eq!(files, expected);
}

#[test]
fn collect_images_errors_for_missing_directory() {
    let missing = Path::new("/does/not/exist");
    let err = collect_images(missing).unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn prepare_dir_creates_nested_directories() {
    let tmp = tempdir().unwrap();
    let target = tmp.path().join("nested/a/b");
    let created = prepare_dir(target.to_str().unwrap()).unwrap();
    assert!(created.exists());
    assert!(created.is_dir());
}

#[test]
fn move_with_unique_name_avoids_overwrites() {
    let tmp = tempdir().unwrap();
    let target_dir = tmp.path().join("target");
    fs::create_dir(&target_dir).unwrap();
    let existing = target_dir.join("image.png");
    fs::write(&existing, b"a").unwrap();

    let source = tmp.path().join("image.png");
    fs::write(&source, b"b").unwrap();

    move_with_unique_name(&source, &target_dir).unwrap();

    let moved_files: Vec<_> = fs::read_dir(&target_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(moved_files.len(), 2);
    assert!(moved_files.iter().any(|p| p == &existing));
    assert!(moved_files
        .iter()
        .any(|p| p.file_name().unwrap().to_string_lossy().starts_with("image-")));
}

#[test]
fn split_name_handles_extensions_and_plain_names() {
    let (stem, ext) = split_name(OsStr::new("photo.avif"));
    assert_eq!(stem, "photo");
    assert_eq!(ext.as_deref(), Some("avif"));

    let (stem, ext) = split_name(OsStr::new("archive"));
    assert_eq!(stem, "archive");
    assert!(ext.is_none());
}

#[test]
fn unique_destination_adds_incrementing_suffix() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("image.png"), []).unwrap();
    fs::write(dir.join("image-1.png"), []).unwrap();
    let candidate = unique_destination(dir, OsStr::new("image.png"));
    assert_eq!(candidate.file_name().unwrap(), "image-2.png");
}

#[test]
fn backup_original_moves_file_to_originals_dir() {
    with_temp_workdir(|cwd| {
        let source = cwd.join("sample.png");
        fs::write(&source, b"data").unwrap();
        backup_original(&source).unwrap();
        assert!(!source.exists());
        let originals = cwd.join(ORIGINALS_DIR);
        assert!(originals.exists());
        assert_eq!(fs::read_dir(&originals).unwrap().count(), 1);
    });
}
