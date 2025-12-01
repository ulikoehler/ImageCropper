use imagecropper::app::saver::Saver;
use imagecropper::image_utils::{OutputFormat, SaveRequest};
use imagecropper::fs_utils::ORIGINALS_DIR;
use image::{GenericImageView, ImageFormat, ImageReader};
use std::{
    fs,
    io::Read,
    path::Path,
    thread,
    time::{Duration, Instant},
};

mod common;
use common::{solid_image, with_temp_workdir};

fn run_save_test(format: OutputFormat, extension: &str, quality: u8) {
    with_temp_workdir(|cwd| {
        // Use a single saver thread for test determinism
        let mut saver = Saver::new(1);
        let image = solid_image(2, 2, [20, 30, 40, 255]);
        let original_path = cwd.join(format!("source.{extension}"));
        fs::write(&original_path, b"original").unwrap();
        let target_path = cwd.join(format!("output.{extension}"));

        let request = SaveRequest {
            image: image.clone(),
            path: target_path.clone(),
            original_path: original_path.clone(),
            quality,
            format,
        };

        saver.queue_save(request).unwrap();
        let sizes = wait_for_save(&mut saver, &target_path).unwrap();

        assert!(target_path.exists());
        assert_decodable(format, &target_path, image.dimensions());
        assert!(cwd.join(ORIGINALS_DIR).exists());
        // size reporting must yield positive values
        assert!(sizes.0 > 0);
        assert!(sizes.1 > 0);
        assert!(saver.pending_saves.is_empty());
    });
}

fn wait_for_save(saver: &mut Saver, expected_path: &Path) -> Option<(u64, u64)> {
    let start = Instant::now();
    loop {
        for (path, result, sizes) in saver.check_completions() {
            if &path == expected_path {
                result.unwrap();
                return sizes;
            }
        }
        if start.elapsed() > Duration::from_secs(5) {
            panic!("timed out waiting for save");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn saver_writes_jpeg_png_webp_and_avif() {
    run_save_test(OutputFormat::Jpg, "jpg", 75);
    run_save_test(OutputFormat::Png, "png", 100);
    run_save_test(OutputFormat::Webp, "webp", 100);
    run_save_test(OutputFormat::Avif, "avif", 50);
}

fn assert_decodable(format: OutputFormat, path: &Path, expected_dims: (u32, u32)) {
    match format {
        OutputFormat::Avif => {
            let mut header = [0u8; 12];
            fs::File::open(path)
                .unwrap()
                .read_exact(&mut header)
                .unwrap();
            assert_eq!(&header[4..8], b"ftyp");
            let reader = ImageReader::open(path)
                .unwrap()
                .with_guessed_format()
                .unwrap();
            assert_eq!(reader.format(), Some(ImageFormat::Avif));
            assert!(fs::metadata(path).unwrap().len() > 0);
        }
        _ => {
            let decoded = image::open(path).unwrap();
            assert_eq!(decoded.dimensions(), expected_dims);
        }
    }
}
