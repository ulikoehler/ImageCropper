use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{mpsc::{self, Receiver, Sender}, Arc, Mutex},
    thread,
    time::Instant,
};

use crate::image_utils::{decode_image_bytes, resize_to_fit, PreloadedImage};

pub struct Loader {
    preload_rx: Receiver<PreloadedImage>,
    path_tx: Sender<PathBuf>,
    pub cache: HashMap<PathBuf, PreloadedImage>,
    pub history: VecDeque<PreloadedImage>,
    pub loading_active: bool,
    pub pending: HashSet<PathBuf>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub fn new() -> Self {
        let (preload_rx, path_tx) = Self::spawn_preloader(None, None);
        Self {
            preload_rx,
            path_tx,
            cache: HashMap::new(),
            history: VecDeque::with_capacity(10),
            loading_active: false,
            pending: HashSet::new(),
        }
    }

    pub fn with_wgpu(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let (preload_rx, path_tx) = Self::spawn_preloader(Some(device), Some(queue));
        Self {
            preload_rx,
            path_tx,
            cache: HashMap::new(),
            history: VecDeque::with_capacity(10),
            loading_active: false,
            pending: HashSet::new(),
        }
    }

    fn spawn_preloader(
        device: Option<wgpu::Device>,
        queue: Option<wgpu::Queue>,
    ) -> (Receiver<PreloadedImage>, Sender<PathBuf>) {
        let (preload_tx, preload_rx) = mpsc::channel();
        let (path_tx, path_rx) = mpsc::channel::<PathBuf>();
        
        let path_rx = Arc::new(Mutex::new(path_rx));
        let device = device.map(Arc::new);
        let queue = queue.map(Arc::new);

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
                        let img_result = decode_image_bytes(&path, &bytes);
                        let decode_duration = decode_start.elapsed();
                        drop(bytes); // Free memory early

                        match img_result {
                            Ok(image) => {
                                let resize_start = Instant::now();
                                // Resize if too large to speed up texture upload and save memory
                                // Assuming 4K max dimension is enough for cropping
                                let image = resize_to_fit(image, 3840, 2160);
                                let resize_duration = resize_start.elapsed();

                                let (texture, texture_gen_duration) =
                                    if let (Some(device), Some(queue)) = (&device, &queue) {
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

                                        (Some(texture), texture_gen_start.elapsed())
                                    } else {
                                        (None, std::time::Duration::default())
                                    };

                                let load_duration = start.elapsed();
                                if preload_tx
                                    .send(PreloadedImage {
                                        path,
                                        image,
                                        color_image: None,
                                        texture,
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

