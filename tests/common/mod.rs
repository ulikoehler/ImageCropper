use image::{DynamicImage, Rgba, RgbaImage};
use once_cell::sync::Lazy;
use std::{
    env,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tempfile::tempdir;

pub static WORKDIR_GUARD: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub fn with_temp_workdir<F: FnOnce(&Path)>(func: F) {
    let guard = WORKDIR_GUARD
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let temp = tempdir().expect("tempdir");
    let previous = env::current_dir().expect("cwd");
    env::set_current_dir(temp.path()).expect("set cwd");

    struct RestoreCwd {
        previous: PathBuf,
    }

    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    let _restore = RestoreCwd { previous };
    func(temp.path());
    drop(guard);
    // tempdir drops here
}

pub fn solid_image(width: u32, height: u32, color: [u8; 4]) -> DynamicImage {
    let pixel = Rgba(color);
    let buffer = RgbaImage::from_pixel(width, height, pixel);
    DynamicImage::ImageRgba8(buffer)
}

pub fn write_image(path: impl Into<PathBuf>, image: &DynamicImage) {
    image
        .save(path.into())
        .expect("failed to write image to disk");
}
