//! Streaming scan: `walk_images`/`spawn_image_scan` emit `Found` events
//! incrementally in filesystem order (never a sorted `ls`), `Error` events
//! for bad roots, and respect filters.

use imagecropper::cli::{spawn_scan_stream, ScanArgs, SortOrder};
use imagecropper::fs_utils::{
    spawn_image_scan, spawn_image_scan_limited, FilterSyntax, PathFilter, ScanEvent,
};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

fn drain(rx: std::sync::mpsc::Receiver<ScanEvent>) -> (Vec<(PathBuf, u64)>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for ev in rx {
        match ev {
            ScanEvent::Found { path, bytes } => found.push((path, bytes)),
            ScanEvent::Error(e) => errors.push(e),
        }
    }
    (found, errors)
}

fn scan_args(root: &std::path::Path) -> ScanArgs {
    ScanArgs {
        paths: vec![root.to_path_buf()],
        recursive: false,
        filter_syntax: FilterSyntax::Glob,
        whitelist: vec![],
        blacklist: vec![],
        parallel: 4,
        dry_run: false,
        verbose: false,
        inverse: false,
        order: SortOrder::Filename,
        resort: imagecropper::cli::ResortMode::OnScanFinish,
        no_gpu: false,
        max_files: None,
    }
}

#[test]
fn stream_emits_all_supported_files() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("b.png"), []).unwrap();
    fs::write(root.join("a.jpg"), []).unwrap();
    fs::write(root.join("c.txt"), []).unwrap();

    let (found, errors) = drain(spawn_image_scan(vec![root.to_path_buf()], false, None));
    assert!(errors.is_empty());
    let mut sorted: Vec<PathBuf> = found.iter().map(|(p, _)| p.clone()).collect();
    sorted.sort();
    assert_eq!(sorted, vec![root.join("a.jpg"), root.join("b.png")]);
}

#[test]
fn stream_found_carries_byte_size() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("sized.png"), [0u8; 123]).unwrap();

    let (found, errors) = drain(spawn_image_scan(vec![root.to_path_buf()], false, None));
    assert!(errors.is_empty());
    assert_eq!(found, vec![(root.join("sized.png"), 123)]);
}

#[test]
fn stream_reports_missing_root_as_error_event() {
    let (_, errors) = drain(spawn_image_scan(
        vec![PathBuf::from("/definitely/not/here")],
        false,
        None,
    ));
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("does not exist"));
}

#[test]
fn stream_respects_filters() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("keep.png"), []).unwrap();
    fs::write(root.join("drop.png"), []).unwrap();
    let filter =
        PathFilter::compile(FilterSyntax::Glob, &["**/keep.png".to_string()], &[]).unwrap();
    let (found, errors) = drain(spawn_image_scan(vec![root.to_path_buf()], false, filter));
    assert!(errors.is_empty());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, root.join("keep.png"));
}

#[test]
fn stream_recurses_and_skips_reserved() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/deep.png"), []).unwrap();
    fs::create_dir(root.join(imagecropper::fs_utils::TRASH_DIR)).unwrap();
    fs::write(
        root.join(imagecropper::fs_utils::TRASH_DIR).join("x.png"),
        [],
    )
    .unwrap();

    let (found, _) = drain(spawn_image_scan(vec![root.to_path_buf()], true, None));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, root.join("sub/deep.png"));
}

#[test]
fn max_files_stops_the_stream_after_n_images() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    for i in 0..8 {
        fs::write(root.join(format!("img{i}.png")), []).unwrap();
    }
    let (found, errors) = drain(spawn_image_scan_limited(
        vec![root.to_path_buf()],
        false,
        None,
        Some(3),
    ));
    assert!(errors.is_empty());
    assert_eq!(found.len(), 3, "must stop after N images");
}

#[test]
fn max_files_zero_means_unlimited() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    for i in 0..5 {
        fs::write(root.join(format!("z{i}.png")), []).unwrap();
    }
    let (found, _) = drain(spawn_image_scan_limited(
        vec![root.to_path_buf()],
        false,
        None,
        Some(0),
    ));
    assert_eq!(found.len(), 5);
}

#[test]
fn max_files_caps_recursive_walks_and_collect() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("sub")).unwrap();
    for i in 0..4 {
        fs::write(root.join(format!("a{i}.png")), []).unwrap();
        fs::write(root.join("sub").join(format!("b{i}.png")), []).unwrap();
    }
    let (found, _) = drain(spawn_image_scan_limited(
        vec![root.to_path_buf()],
        true,
        None,
        Some(2),
    ));
    assert_eq!(found.len(), 2, "recursive walk must stop at N");

    let files = imagecropper::fs_utils::collect_images_limited(
        &[root.to_path_buf()],
        true,
        None,
        Some(3),
    )
    .unwrap();
    assert_eq!(files.len(), 3, "collect path must honour the cap");
}

#[test]
fn scan_stream_carries_order_settings() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("x.png"), []).unwrap();
    let mut args = scan_args(root);
    args.inverse = true;
    let stream = spawn_scan_stream(&args).unwrap();
    assert!(stream.inverse);
    assert_eq!(stream.order, SortOrder::Filename);
    let (found, errors) = drain(stream.rx);
    assert!(errors.is_empty());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, root.join("x.png"));
}
