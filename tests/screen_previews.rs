//! Fullscreen-tier previews: generated under `~/.cache/imagecropper/f/…`
//! (or `$XDG_CACHE_HOME`), signature-named, capped at 1920px.

use imagecropper::fs_utils::previews::{
    cache_root, produce_screen_jpeg, remove_previews_for, screen_dir_for, screen_preview_path,
    SCREEN_MAX_DIM,
};
use std::sync::Mutex;
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

/// XDG_CACHE_HOME is process-global — serialize the tests that set it.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_xdg_cache<T>(f: impl FnOnce() -> T) -> T {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    let out = f();
    std::env::remove_var("XDG_CACHE_HOME");
    out
}

#[test]
fn screen_preview_path_lives_under_cache_root() {
    with_xdg_cache(|| {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("p.png");
        write_image(&src, &solid_image(40, 30, [255, 0, 0, 255]));
        let p = screen_preview_path(&src).unwrap();
        assert!(p.starts_with(cache_root().join("f")), "path: {p:?}");
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&format!("-f{SCREEN_MAX_DIM}.jpg")));
    });
}

#[test]
fn produce_screen_jpeg_generates_and_caches() {
    with_xdg_cache(|| {
        let tmp = tempdir().unwrap();
        // 3000x1000 source → preview must fit inside 1920.
        let src = tmp.path().join("big.png");
        write_image(&src, &solid_image(3000, 1000, [0, 0, 255, 255]));

        let (jpeg, w, h) = produce_screen_jpeg(&src).expect("screen jpeg");
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "JPEG magic");
        assert!(w <= SCREEN_MAX_DIM && h <= SCREEN_MAX_DIM);
        assert_eq!((w, h), (1920, 640), "aspect preserved");

        // Persisted under the global cache — second call is a cache hit.
        let path = screen_preview_path(&src).unwrap();
        assert!(path.exists());
        let (jpeg2, w2, h2) = produce_screen_jpeg(&src).unwrap();
        assert_eq!(jpeg2, jpeg);
        assert_eq!((w2, h2), (w, h));
    });
}

#[test]
fn screen_preview_sig_names_invalidate_on_change() {
    with_xdg_cache(|| {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("p.png");
        write_image(&src, &solid_image(64, 64, [0, 255, 0, 255]));
        let first = screen_preview_path(&src).unwrap();

        // Replace the file: signature changes → different cache filename.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_image(&src, &solid_image(128, 128, [255, 0, 255, 255]));
        let second = screen_preview_path(&src).unwrap();
        assert_ne!(first, second);
    });
}

#[test]
fn remove_previews_for_purges_fullscreen_cache() {
    with_xdg_cache(|| {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("p.png");
        write_image(&src, &solid_image(64, 64, [0, 0, 255, 255]));

        // Generate the cached fullscreen preview, then delete the source's
        // previews as the delete/move queues do.
        let (jpeg, _, _) = produce_screen_jpeg(&src).expect("screen jpeg");
        assert!(!jpeg.is_empty());
        assert!(screen_dir_for(&src).exists());

        remove_previews_for(&src);

        assert!(
            !screen_dir_for(&src).exists(),
            "fullscreen cache dir must be removed with the previews"
        );
    });
}
