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

use img_parts::{ImageEXIF, ImageICC};
use img_parts::jpeg::Jpeg;
use img_parts::png::Png;
use img_parts::webp::WebP;

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

                let mut original_size: Option<u64> = None;
                let mut new_size: Option<u64> = None;

                let result = (|| -> Result<()> {
                    // capture original size if possible before backup moves the file
                    if let Ok(meta) = std::fs::metadata(&req.original_path) {
                        original_size = Some(meta.len());
                    }

                    let backed_up_path = backup_original(&req.original_path)?;

                    // Save to temp file first
                    let parent = req.path.parent().unwrap_or_else(|| std::path::Path::new("."));
                    let temp_dir = prepare_dir(parent, TEMP_DIR)?;
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
                    // std::fs::rename(&temp_path, &req.path)?; // We do this later now

                    // Try to copy EXIF/ICC from original to new file
                    // We read the temp file, inject metadata, and write to final path.
                    // If injection fails, we just move the temp file.
                    
                    let copy_metadata = || -> Result<()> {
                        let input_data = std::fs::read(&backed_up_path)?;
                        let temp_data = std::fs::read(&temp_path)?;
                        
                        // Detect input format and extract metadata
                        let (exif, icc) = if let Ok(input_jpeg) = Jpeg::from_bytes(input_data.clone().into()) {
                            (input_jpeg.exif(), input_jpeg.icc_profile())
                        } else if let Ok(input_png) = Png::from_bytes(input_data.clone().into()) {
                            (input_png.exif(), input_png.icc_profile())
                        } else if let Ok(input_webp) = WebP::from_bytes(input_data.clone().into()) {
                            (input_webp.exif(), input_webp.icc_profile())
                        } else {
                            (None, None)
                        };

                        if exif.is_none() && icc.is_none() {
                            // No metadata to copy, just move file
                            std::fs::rename(&temp_path, &req.path)?;
                            return Ok(());
                        }

                        // Inject into output
                        let output_bytes = match req.format {
                            OutputFormat::Jpg => {
                                if let Ok(mut out_jpeg) = Jpeg::from_bytes(temp_data.into()) {
                                    if let Some(exif) = exif { out_jpeg.set_exif(Some(exif)); }
                                    if let Some(icc) = icc { out_jpeg.set_icc_profile(Some(icc)); }
                                    let mut out = Vec::new();
                                    out_jpeg.encoder().write_to(&mut out)?;
                                    Some(out)
                                } else { None }
                            }
                            OutputFormat::Png => {
                                if let Ok(mut out_png) = Png::from_bytes(temp_data.into()) {
                                    if let Some(exif) = exif { out_png.set_exif(Some(exif)); }
                                    if let Some(icc) = icc { out_png.set_icc_profile(Some(icc)); }
                                    let mut out = Vec::new();
                                    out_png.encoder().write_to(&mut out)?;
                                    Some(out)
                                } else { None }
                            }
                            OutputFormat::Webp => {
                                if let Ok(mut out_webp) = WebP::from_bytes(temp_data.into()) {
                                    if let Some(exif) = exif { out_webp.set_exif(Some(exif)); }
                                    if let Some(icc) = icc { out_webp.set_icc_profile(Some(icc)); }
                                    let mut out = Vec::new();
                                    out_webp.encoder().write_to(&mut out)?;
                                    Some(out)
                                } else { None }
                            }
                            OutputFormat::Avif => {
                                // img-parts doesn't support AVIF yet?
                                // AVIF is based on ISOBMFF (HEIF). img-parts has some support?
                                // It seems img-parts 0.3 doesn't have explicit AVIF support.
                                // So we skip AVIF metadata copy for now.
                                None
                            }
                        };

                        if let Some(bytes) = output_bytes {
                            std::fs::write(&req.path, bytes)?;
                            std::fs::remove_file(&temp_path)?;
                        } else {
                            std::fs::rename(&temp_path, &req.path)?;
                        }
                        Ok(())
                    };

                    if let Err(e) = copy_metadata() {
                        eprintln!("Failed to copy metadata: {}", e);
                        // Fallback: just move the file if it hasn't been moved yet
                        if temp_path.exists() {
                            std::fs::rename(&temp_path, &req.path)?;
                        }
                    }

                    // capture new file size if possible

                    // capture new file size if possible
                    if let Ok(meta) = std::fs::metadata(&req.path) {
                        new_size = Some(meta.len());
                    }
                    Ok(())
                })();
                let _ = tx.send(SaveStatus {
                    path: req.path,
                    result,
                    original_size,
                    new_size,
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

    pub fn check_completions(&mut self) -> Vec<(PathBuf, Result<()>, Option<(u64, u64)>)> {
        let mut completed = Vec::new();
        while let Ok(status) = self.save_status_rx.try_recv() {
            if let Some(idx) = self.pending_saves.iter().position(|p| *p == status.path) {
                self.pending_saves.remove(idx);
            }
            let sizes = match (status.original_size, status.new_size) {
                (Some(original), Some(new)) => Some((original, new)),
                _ => None,
            };
            completed.push((status.path, status.result, sizes));
        }
        completed
    }
}

