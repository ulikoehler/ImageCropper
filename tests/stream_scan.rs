//! Streaming scan: `walk_images`/`spawn_image_scan` emit `Found` events
//! incrementally in filesystem order (never a sorted `ls`), `Error` events
//! for bad roots, and respect filters.

use imagecropper::cli::{spawn_scan_stream, ScanArgs, SortOrder};
use imagecropper::fs_utils::{
    spawn_image_scan, FilterSyntax, PathFilter, ScanEvent,
};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;

fn drain(rx: std::sync::mpsc::Receiver<ScanEvent>) -> (Vec<PathBuf>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for ev in rx {
        match ev {
            ScanEvent::Found(p) => found.push(p),
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
    let mut sorted = found.clone();
    sorted.sort();
    assert_eq!(sorted, vec![root.join("a.jpg"), root.join("b.png")]);
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
    let filter = PathFilter::compile(
        FilterSyntax::Glob,
        &["**/keep.png".to_string()],
        &[],
    )
    .unwrap();
    let (found, errors) = drain(spawn_image_scan(vec![root.to_path_buf()], false, filter));
    assert!(errors.is_empty());
    assert_eq!(found, vec![root.join("keep.png")]);
}

#[test]
fn stream_recurses_and_skips_reserved() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("sub/deep.png"), []).unwrap();
    fs::create_dir(root.join(imagecropper::fs_utils::TRASH_DIR)).unwrap();
    fs::write(root.join(imagecropper::fs_utils::TRASH_DIR).join("x.png"), []).unwrap();

    let (found, _) = drain(spawn_image_scan(vec![root.to_path_buf()], true, None));
    assert_eq!(found, vec![root.join("sub/deep.png")]);
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
    assert_eq!(found, vec![root.join("x.png")]);
}
