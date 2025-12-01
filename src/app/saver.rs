use std::{
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

use anyhow::{anyhow, Result};
use image::codecs::avif::AvifEncoder;

use crate::{
    fs_utils::{backup_original, prepare_dir, TEMP_DIR},
    image_utils::{OutputFormat, SaveRequest, SaveStatus},
};

pub struct Saver {
    save_tx: Sender<SaveRequest>,
    save_status_rx: Receiver<SaveStatus>,
    pub pending_saves: Vec<PathBuf>,
}

impl Saver {
    pub fn new(concurrency: usize) -> Self {
        let (save_tx, save_rx) = mpsc::channel();
        let (save_status_tx, save_status_rx) = mpsc::channel();

        let rx = Arc::new(Mutex::new(save_rx));

        for _ in 0..concurrency {
            Self::spawn_saver_thread(rx.clone(), save_status_tx.clone());
        }

        Self {
            save_tx,
            save_status_rx,
            pending_saves: Vec::new(),
        }
    }

    fn spawn_saver_thread(rx: Arc<Mutex<Receiver<SaveRequest>>>, tx: Sender<SaveStatus>) {
        thread::spawn(move || {
            loop {
                let req = {
                    let Ok(lock) = rx.lock() else { break };
                    match lock.recv() {
                        Ok(req) => req,
                        Err(_) => break,
                    }
                };

                let result = (|| -> Result<()> {
                    backup_original(&req.original_path)?;

                    // Save to temp file first
                    let temp_dir = prepare_dir(TEMP_DIR)?;
                    let file_name = req
                        .path
                        .file_name()
                        .ok_or_else(|| anyhow!("No filename"))?;
                    let temp_path = temp_dir.join(file_name);

                    {
                        let file = std::fs::File::create(&temp_path)?;
                        let writer = std::io::BufWriter::new(file);
                        match req.format {
                            OutputFormat::Jpg => {
                                let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                                    writer,
                                    req.quality,
                                );
                                req.image.write_with_encoder(encoder)?;
                            }
                            OutputFormat::Png => {
                                let encoder = image::codecs::png::PngEncoder::new(writer);
                                req.image.write_with_encoder(encoder)?;
                            }
                            OutputFormat::Webp => {
                                let encoder = image::codecs::webp::WebPEncoder::new_lossless(writer);
                                req.image.write_with_encoder(encoder)?;
                            }
                            OutputFormat::Avif => {
                                let encoder =
                                    AvifEncoder::new_with_speed_quality(writer, 4, req.quality);
                                req.image.write_with_encoder(encoder)?;
                            }
                        }
                    } // Close file

                    // Move to final destination
                    std::fs::rename(&temp_path, &req.path)?;
                    Ok(())
                })();
                let _ = tx.send(SaveStatus {
                    path: req.path,
                    result,
                });
            }
        });
    }

    pub fn queue_save(&mut self, request: SaveRequest) -> Result<()> {
        self.pending_saves.push(request.path.clone());
        self.save_tx
            .send(request)
            .map_err(|e| anyhow!("Failed to send save request: {}", e))
    }

    pub fn check_completions(&mut self) -> Vec<(PathBuf, Result<()>)> {
        let mut completed = Vec::new();
        while let Ok(status) = self.save_status_rx.try_recv() {
            if let Some(idx) = self.pending_saves.iter().position(|p| *p == status.path) {
                self.pending_saves.remove(idx);
            }
            completed.push((status.path, status.result));
        }
        completed
    }
}

