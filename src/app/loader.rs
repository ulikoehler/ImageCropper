use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
};

use crate::image_utils::{to_color_image, PreloadedImage};

pub struct Loader {
    preload_rx: Receiver<PreloadedImage>,
    pub cache: HashMap<PathBuf, PreloadedImage>,
    pub history: VecDeque<PreloadedImage>,
    pub loading_active: bool,
}

impl Loader {
    pub fn new(files: Vec<PathBuf>) -> Self {
        let preload_rx = Self::spawn_preloader(files);
        Self {
            preload_rx,
            cache: HashMap::new(),
            history: VecDeque::with_capacity(10),
            loading_active: false,
        }
    }

    fn spawn_preloader(paths: Vec<PathBuf>) -> Receiver<PreloadedImage> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for path in paths {
                match image::open(&path) {
                    Ok(mut image) => {
                        // Resize if too large to speed up texture upload and save memory
                        // Assuming 4K max dimension is enough for cropping
                        if image.width() > 3840 || image.height() > 2160 {
                            image =
                                image.resize(3840, 2160, image::imageops::FilterType::Lanczos3);
                        }
                        let color_image = to_color_image(&image);
                        if tx
                            .send(PreloadedImage {
                                path,
                                image,
                                color_image,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) => {
                        eprintln!("Failed to preload {}: {err:#}", path.display());
                    }
                }
            }
        });
        rx
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

