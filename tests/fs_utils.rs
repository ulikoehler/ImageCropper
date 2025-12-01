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

    let mut files = collect_images(&[root.to_path_buf()], false).unwrap();
    files.sort();

    let mut expected: Vec<_> = supported.iter().map(|n| root.join(n)).collect();
    expected.sort();
    assert_eq!(files, expected);
}


#[test]
fn collect_images_respects_recursive_flag() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("subdir")).unwrap();
    fs::write(root.join("subdir/image.png"), []).unwrap();

    // non-recursive should not find the nested file
    let mut nonrec = collect_images(&[root.to_path_buf()], false).unwrap();
    nonrec.sort();
    assert!(nonrec.is_empty());

    // recursive should find it
    let mut rec = collect_images(&[root.to_path_buf()], true).unwrap();
    rec.sort();
    assert_eq!(rec, vec![root.join("subdir/image.png")]);
}

#[test]
fn collect_images_errors_for_missing_directory() {
    let missing = Path::new("/does/not/exist");
    let err = collect_images(&[missing.to_path_buf()], false).unwrap_err();
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn prepare_dir_creates_nested_directories() {
    let tmp = tempdir().unwrap();
    let created = prepare_dir(tmp.path(), "nested/a/b").unwrap();
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

#[test]
fn collect_images_handles_multiple_paths_and_mixed_inputs() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    
    // Create structure:
    // root/
    //   dir1/
    //     img1.png
    //   dir2/
    //     img2.jpg
    //   img3.avif
    
    let dir1 = root.join("dir1");
    fs::create_dir(&dir1).unwrap();
    fs::write(dir1.join("img1.png"), []).unwrap();
    
    let dir2 = root.join("dir2");
    fs::create_dir(&dir2).unwrap();
    fs::write(dir2.join("img2.jpg"), []).unwrap();
    
    let img3 = root.join("img3.avif");
    fs::write(&img3, []).unwrap();
    
    // Test 1: Multiple directories
    let paths = vec![dir1.clone(), dir2.clone()];
    let mut files = collect_images(&paths, false).unwrap();
    files.sort();
    assert_eq!(files, vec![dir1.join("img1.png"), dir2.join("img2.jpg")]);
    
    // Test 2: Mixed directory and file
    let paths = vec![dir1.clone(), img3.clone()];
    let mut files = collect_images(&paths, false).unwrap();
    files.sort();
    assert_eq!(files, vec![dir1.join("img1.png"), img3.clone()]);
    
    // Test 3: Just files
    let paths = vec![dir1.join("img1.png"), img3.clone()];
    let mut files = collect_images(&paths, false).unwrap();
    files.sort();
    assert_eq!(files, vec![dir1.join("img1.png"), img3.clone()]);
}
