use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Instant,
};

use fast_image_resize::images::Image;
use fast_image_resize::{ResizeOptions, Resizer};

use crate::image_utils::{to_color_image, PreloadedImage};

pub struct Loader {
    preload_rx: Receiver<PreloadedImage>,
    path_tx: Sender<PathBuf>,
    pub cache: HashMap<PathBuf, PreloadedImage>,
    pub history: VecDeque<PreloadedImage>,
    pub loading_active: bool,
}

impl Loader {
    pub fn new() -> Self {
        let (preload_rx, path_tx) = Self::spawn_preloader();
        Self {
            preload_rx,
            path_tx,
            cache: HashMap::new(),
            history: VecDeque::with_capacity(10),
            loading_active: false,
        }
    }

    fn spawn_preloader() -> (Receiver<PreloadedImage>, Sender<PathBuf>) {
        let (preload_tx, preload_rx) = mpsc::channel();
        let (path_tx, path_rx) = mpsc::channel();
        
        thread::spawn(move || {
            while let Ok(path) = path_rx.recv() {
                let start = Instant::now();
                
                let io_start = Instant::now();
                let img_result = image::open(&path);
                let io_duration = io_start.elapsed();

                match img_result {
                    Ok(mut image) => {
                        let resize_start = Instant::now();
                        // Resize if too large to speed up texture upload and save memory
                        // Assuming 4K max dimension is enough for cropping
                        if image.width() > 3840 || image.height() > 2160 {
                            let (nwidth, nheight) = (3840, 2160);
                            let ratio = image.width() as f64 / image.height() as f64;
                            let (new_w, new_h) = if ratio > nwidth as f64 / nheight as f64 {
                                (nwidth, (nwidth as f64 / ratio) as u32)
                            } else {
                                ((nheight as f64 * ratio) as u32, nheight)
                            };

                            let src_image = Image::from_vec_u8(
                                image.width(),
                                image.height(),
                                image.to_rgba8().into_raw(),
                                fast_image_resize::PixelType::U8x4,
                            )
                            .unwrap();

                            let mut dst_image = Image::new(new_w, new_h, src_image.pixel_type());

                            let mut resizer = Resizer::new();
                            resizer
                                .resize(&src_image, &mut dst_image, &ResizeOptions::default())
                                .unwrap();

                            image = image::DynamicImage::ImageRgba8(
                                image::RgbaImage::from_raw(new_w, new_h, dst_image.into_vec())
                                    .unwrap(),
                            );
                        }
                        let resize_duration = resize_start.elapsed();

                        let texture_gen_start = Instant::now();
                        let color_image = to_color_image(&image);
                        let texture_gen_duration = texture_gen_start.elapsed();

                        let load_duration = start.elapsed();
                        if preload_tx
                            .send(PreloadedImage {
                                path,
                                image,
                                color_image,
                                load_duration,
                                io_duration,
                                resize_duration,
                                texture_gen_duration,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        // Give a clearer hint when AVIF decoding isn't available in the build
                        if path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|s| s.eq_ignore_ascii_case("avif"))
                            .unwrap_or(false)
                            && err.to_string().contains("Avif")
                        {
                            eprintln!(
                                "Failed to preload {}: AVIF decoding not available in this build.\n\tHint: build with image crate's `avif-native` feature (enables dav1d/mp4parse) to decode AVIF files.",
                                path.display()
                            );
                        } else {
                            eprintln!("Failed to preload {}: {err:#}", path.display());
                        }
                    }
                }
            }
        });
        (preload_rx, path_tx)
    }

    pub fn load_image(&self, path: PathBuf) {
        let _ = self.path_tx.send(path);
    }

    pub fn update(&mut self) {
        while let Ok(entry) = self.preload_rx.try_recv() {
            self.cache.insert(entry.path.clone(), entry);
        }
    }

    pub fn get_from_cache(&mut self, path: &PathBuf) -> Option<PreloadedImage> {
        self.cache.remove(path)
    }

    pub fn push_history(&mut self, image: PreloadedImage) {
        if self.history.len() >= 10 {
            self.history.pop_front();
        }
        self.history.push_back(image);
    }

    pub fn pop_history(&mut self) -> Option<PreloadedImage> {
        self.history.pop_back()
    }
}

