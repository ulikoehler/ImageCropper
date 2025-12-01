use std::{
    collections::{HashMap, VecDeque},
    io::Cursor,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Instant,
};

use fast_image_resize::images::Image;
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use zune_jpeg::JpegDecoder;

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
        let (path_tx, path_rx) = mpsc::channel::<PathBuf>();
        
        thread::spawn(move || {
            while let Ok(path) = path_rx.recv() {
                let start = Instant::now();
                
                let read_start = Instant::now();
                let file_bytes = std::fs::read(&path);
                let read_duration = read_start.elapsed();

                match file_bytes {
                    Ok(bytes) => {
                        let decode_start = Instant::now();
                        
                        // Try zune-jpeg first for JPEGs
                        let is_jpeg = path.extension()
                            .and_then(|e| e.to_str())
                            .map(|s| s.eq_ignore_ascii_case("jpg") || s.eq_ignore_ascii_case("jpeg"))
                            .unwrap_or(false);

                        let img_result = if is_jpeg {
                            let mut decoder = JpegDecoder::new(Cursor::new(&bytes));
                            match decoder.decode() {
                                Ok(pixels) => {
                                    let info = decoder.info().unwrap();
                                    // zune-jpeg usually returns RGB8
                                    image::RgbImage::from_raw(info.width as u32, info.height as u32, pixels)
                                        .map(image::DynamicImage::ImageRgb8)
                                        .ok_or_else(|| image::ImageError::Decoding(image::error::DecodingError::new(image::error::ImageFormatHint::Exact(image::ImageFormat::Jpeg), "Failed to create buffer")))
                                }
                                Err(e) => {
                                    // Fallback to standard loader if zune fails
                                    image::load_from_memory(&bytes)
                                }
                            }
                        } else {
                            image::load_from_memory(&bytes)
                        };

                        let decode_duration = decode_start.elapsed();
                        drop(bytes); // Free memory early

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

                                    // Use fast_image_resize to convert to RGBA8 and resize in one go if possible
                                    // or just resize.
                                    // We want the result to be RGBA8 for egui.
                                    
                                    let src_image = match image {
                                        image::DynamicImage::ImageRgb8(ref rgb) => {
                                            Image::from_vec_u8(
                                                rgb.width(),
                                                rgb.height(),
                                                rgb.as_raw().clone(),
                                                PixelType::U8x3,
                                            ).ok()
                                        }
                                        image::DynamicImage::ImageRgba8(ref rgba) => {
                                            Image::from_vec_u8(
                                                rgba.width(),
                                                rgba.height(),
                                                rgba.as_raw().clone(),
                                                PixelType::U8x4,
                                            ).ok()
                                        }
                                        _ => {
                                            // Fallback for other types
                                            let rgba = image.to_rgba8();
                                            Image::from_vec_u8(
                                                rgba.width(),
                                                rgba.height(),
                                                rgba.into_raw(),
                                                PixelType::U8x4,
                                            ).ok()
                                        }
                                    };

                                    if let Some(src_image) = src_image {
                                        let mut dst_image = Image::new(new_w, new_h, PixelType::U8x4);
                                        let mut resizer = Resizer::new();
                                        resizer
                                            .resize(&src_image, &mut dst_image, &ResizeOptions::default())
                                            .unwrap();

                                        image = image::DynamicImage::ImageRgba8(
                                            image::RgbaImage::from_raw(new_w, new_h, dst_image.into_vec())
                                                .unwrap(),
                                        );
                                    }
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
                                        read_duration,
                                        decode_duration,
                                        resize_duration,
                                        texture_gen_duration,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(err) => {
                                eprintln!("Failed to decode {}: {err:#}", path.display());
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("Failed to read {}: {err:#}", path.display());
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

