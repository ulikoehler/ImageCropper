use imagecropper::app::loader::Loader;
use imagecropper::image_utils::PreloadedImage;
use std::path::PathBuf;
use std::{thread, time::Duration};
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

#[test]
fn loader_populates_cache_from_preloader() {
    let tmp = tempdir().unwrap();
    let img_path = tmp.path().join("sample.png");
    let image = solid_image(4, 4, [10, 20, 30, 255]);
    write_image(&img_path, &image);

    let mut loader = Loader::new(vec![img_path.clone()]);
    for _ in 0..10 {
        loader.update();
        if loader.cache.contains_key(&img_path) {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let cached = loader.get_from_cache(&img_path);
    assert!(cached.is_some());
}

#[test]
fn history_keeps_only_ten_entries() {
    let mut loader = Loader::new(Vec::new());
    for idx in 0..12 {
        let image = solid_image(1, 1, [idx as u8, 0, 0, 255]);
        let color_image = imagecropper::image_utils::to_color_image(&image);
        loader.push_history(PreloadedImage {
            path: PathBuf::from(format!("{idx}.png")),
            image,
            color_image,
        });
    }
    assert_eq!(loader.history.len(), 10);
    assert_eq!(loader.history.front().unwrap().path, PathBuf::from("2.png"));
    assert_eq!(loader.history.back().unwrap().path, PathBuf::from("11.png"));
}
