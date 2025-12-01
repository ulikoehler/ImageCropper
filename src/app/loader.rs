use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Cursor,
    path::PathBuf,
    sync::{mpsc::{self, Receiver, Sender}, Arc, Mutex},
    thread,
    time::Instant,
};

use fast_image_resize::images::Image;
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use zune_jpeg::JpegDecoder;

use crate::image_utils::PreloadedImage;

pub struct Loader {
    preload_rx: Receiver<PreloadedImage>,
    path_tx: Sender<PathBuf>,
    pub cache: HashMap<PathBuf, PreloadedImage>,
    pub history: VecDeque<PreloadedImage>,
    pub loading_active: bool,
    pub pending: HashSet<PathBuf>,
}

impl Loader {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let (preload_rx, path_tx) = Self::spawn_preloader(device, queue);
        Self {
            preload_rx,
            path_tx,
            cache: HashMap::new(),
            history: VecDeque::with_capacity(10),
            loading_active: false,
            pending: HashSet::new(),
        }
    }

    fn spawn_preloader(device: wgpu::Device, queue: wgpu::Queue) -> (Receiver<PreloadedImage>, Sender<PathBuf>) {
        let (preload_tx, preload_rx) = mpsc::channel();
        let (path_tx, path_rx) = mpsc::channel::<PathBuf>();
        
        let path_rx = Arc::new(Mutex::new(path_rx));
        let device = Arc::new(device);
        let queue = Arc::new(queue);

        for _ in 0..16 {
            let path_rx = path_rx.clone();
            let preload_tx = preload_tx.clone();
            let device = device.clone();
            let queue = queue.clone();

            thread::spawn(move || {
                loop {
                    let path = {
                        let Ok(rx) = path_rx.lock() else { break };
                        match rx.recv() {
                            Ok(p) => p,
                            Err(_) => break,
                        }
                    };

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
                            // Allow incomplete JPEGs to still be rendered
                            let options = zune_jpeg::zune_core::options::DecoderOptions::default()
                                .set_strict_mode(false);
                            let mut decoder = JpegDecoder::new(Cursor::new(&bytes));
                            decoder.set_options(options);

                            match decoder.decode() {
                                Ok(pixels) => {
                                    let info = decoder.info().unwrap();
                                    // zune-jpeg usually returns RGB8
                                    image::RgbImage::from_raw(info.width as u32, info.height as u32, pixels)
                                        .map(image::DynamicImage::ImageRgb8)
                                        .ok_or_else(|| image::ImageError::Decoding(image::error::DecodingError::new(image::error::ImageFormatHint::Exact(image::ImageFormat::Jpeg), "Failed to create buffer")))
                                }
                                Err(_e) => {
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
                                        let mut dst_image = Image::new(new_w, new_h, src_image.pixel_type());
                                        let mut resizer = Resizer::new();
                                        resizer
                                            .resize(&src_image, &mut dst_image, &ResizeOptions::default())
                                            .unwrap();

                                        image = match src_image.pixel_type() {
                                            PixelType::U8x3 => {
                                                image::DynamicImage::ImageRgb8(
                                                    image::RgbImage::from_raw(new_w, new_h, dst_image.into_vec()).unwrap()
                                                )
                                            }
                                            PixelType::U8x4 => {
                                                image::DynamicImage::ImageRgba8(
                                                    image::RgbaImage::from_raw(new_w, new_h, dst_image.into_vec()).unwrap()
                                                )
                                            }
                                            _ => unreachable!("We only created U8x3 or U8x4 images"),
                                        };
                                    }
                                }
                                let resize_duration = resize_start.elapsed();

                                let texture_gen_start = Instant::now();
                                
                                let rgba = image.to_rgba8();
                                let width = rgba.width();
                                let height = rgba.height();
                                
                                let texture_size = wgpu::Extent3d {
                                    width,
                                    height,
                                    depth_or_array_layers: 1,
                                };
                                
                                let texture = device.create_texture(&wgpu::TextureDescriptor {
                                    size: texture_size,
                                    mip_level_count: 1,
                                    sample_count: 1,
                                    dimension: wgpu::TextureDimension::D2,
                                    format: wgpu::TextureFormat::Rgba8Unorm,
                                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                                    label: Some("image_texture"),
                                    view_formats: &[],
                                });
                                
                                queue.write_texture(
                                    wgpu::TexelCopyTextureInfo {
                                        texture: &texture,
                                        mip_level: 0,
                                        origin: wgpu::Origin3d::ZERO,
                                        aspect: wgpu::TextureAspect::All,
                                    },
                                    &rgba,
                                    wgpu::TexelCopyBufferLayout {
                                        offset: 0,
                                        bytes_per_row: Some(4 * width),
                                        rows_per_image: Some(height),
                                    },
                                    texture_size,
                                );

                                let texture_gen_duration = texture_gen_start.elapsed();

                                let load_duration = start.elapsed();
                                if preload_tx
                                    .send(PreloadedImage {
                                        path,
                                        image,
                                        color_image: None,
                                        texture: Some(texture),
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
        }
        (preload_rx, path_tx)
    }

    pub fn load_image(&mut self, path: PathBuf) {
        if self.cache.contains_key(&path) || self.pending.contains(&path) {
            return;
        }
        self.pending.insert(path.clone());
        let _ = self.path_tx.send(path);
    }

    pub fn update(&mut self) {
        while let Ok(entry) = self.preload_rx.try_recv() {
            self.pending.remove(&entry.path);
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

