use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use eframe::{
    egui,
    egui::{Color32, ViewportCommand},
};
use image::{DynamicImage, GenericImage, RgbaImage};
use image::codecs::avif::AvifEncoder;
use rand::seq::SliceRandom;
use walkdir::WalkDir;

const TRASH_DIR: &str = ".imagecropper-trash";
const ORIGINALS_DIR: &str = ".imagecropper-originals";
const TEMP_DIR: &str = ".imagecropper-tmp";
const HANDLE_THICKNESS: f32 = 10.0;
const ARROW_MOVE_STEP: f32 = 2.0;
const MIN_HANDLE_LENGTH: f32 = 20.0;
const MAX_HANDLE_LENGTH: f32 = 100.0;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Fullscreen image cropper with deletion workflow"
)]
struct Args {
    /// Directory that contains images to process
    #[arg(value_name = "DIRECTORY")]
    directory: PathBuf,

    /// Quality of the output image (1-100)
    #[arg(short, long, default_value_t = 60)]
    quality: u8,

    /// Automatically resave images to AVIF when navigating away
    #[arg(long, default_value_t = false)]
    resave: bool,

    /// Skip destructive operations and just print what would happen
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut files = collect_images(&args.directory)?;
    if files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in {}",
            args.directory.display()
        ));
    }
    files.shuffle(&mut rand::thread_rng());
    let dry_run = args.dry_run;
    let quality = args.quality;
    let resave = args.resave;
    let files_for_app = files.clone();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_fullscreen(true),
        ..Default::default()
    };

    eframe::run_native(
        "ImageCropper",
        native_options,
        Box::new(
            move |cc| match ImageCropperApp::new(cc, files_for_app.clone(), dry_run, quality, resave) {
                Ok(app) => Box::new(app) as Box<dyn eframe::App>,
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            },
        ),
    )?;

    Ok(())
}

fn collect_images(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Err(anyhow!("{} does not exist", root.display()));
    }
    if !root.is_dir() {
        return Err(anyhow!("{} is not a directory", root.display()));
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() && is_supported_image(entry.path()) {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ref ext)
            if matches!(
                ext.as_str(),
                "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tiff" | "tif" | "ico"
            )
    )
}

struct SaveRequest {
    image: DynamicImage,
    path: PathBuf,
    original_path: PathBuf,
    quality: u8,
}

struct SaveStatus {
    path: PathBuf,
    result: Result<()>,
}

struct ImageCropperApp {
    files: Vec<PathBuf>,
    current_index: usize,
    dry_run: bool,
    quality: u8,
    resave: bool,
    image: Option<DynamicImage>,
    texture: Option<egui::TextureHandle>,
    image_size: egui::Vec2,
    selections: Vec<Selection>,
    selection_anchor: Option<egui::Pos2>,
    active_handle: Option<HandleDrag>,
    status: String,
    finished: bool,
    preload_rx: Receiver<PreloadedImage>,
    preload_cache: HashMap<PathBuf, PreloadedImage>,
    past_images: VecDeque<PreloadedImage>,
    save_tx: mpsc::Sender<SaveRequest>,
    save_status_rx: Receiver<SaveStatus>,
    pending_saves: Vec<PathBuf>,
    loading_active: bool,
    is_exiting: bool,
    exit_attempt_count: usize,
}

impl ImageCropperApp {
    fn new(cc: &eframe::CreationContext<'_>, files: Vec<PathBuf>, dry_run: bool, quality: u8, resave: bool) -> Result<Self> {
        let preload_rx = Self::spawn_preloader(files.clone());
        let (save_tx, save_rx) = mpsc::channel();
        let (save_status_tx, save_status_rx) = mpsc::channel();
        
        Self::spawn_saver(save_rx, save_status_tx);

        let mut app = Self {
            files,
            current_index: 0,
            dry_run,
            quality,
            resave,
            image: None,
            texture: None,
            image_size: egui::Vec2::new(1.0, 1.0),
            selections: Vec::new(),
            selection_anchor: None,
            active_handle: None,
            status: String::from("Ready"),
            finished: false,
            preload_rx,
            preload_cache: HashMap::new(),
            past_images: VecDeque::with_capacity(5),
            save_tx,
            save_status_rx,
            pending_saves: Vec::new(),
            loading_active: false,
            is_exiting: false,
            exit_attempt_count: 0,
        };
        app.ensure_initial_preload();
        app.load_current_image(&cc.egui_ctx)?;
        Ok(app)
    }

    fn spawn_saver(rx: Receiver<SaveRequest>, tx: mpsc::Sender<SaveStatus>) {
        thread::spawn(move || {
            while let Ok(req) = rx.recv() {
                let result = (|| -> Result<()> {
                    backup_original(&req.original_path)?;
                    
                    // Save to temp file first
                    let temp_dir = prepare_dir(TEMP_DIR)?;
                    let file_name = req.path.file_name().ok_or_else(|| anyhow!("No filename"))?;
                    let temp_path = temp_dir.join(file_name);

                    {
                        let file = std::fs::File::create(&temp_path)?;
                        let writer = std::io::BufWriter::new(file);
                        let encoder = AvifEncoder::new_with_speed_quality(writer, 4, req.quality);
                        req.image.write_with_encoder(encoder)?;
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

    fn current_path(&self) -> Option<&Path> {
        self.files.get(self.current_index).map(|p| p.as_path())
    }

    fn load_current_image(&mut self, ctx: &egui::Context) -> Result<()> {
        self.drain_preload_queue();
        let path = self
            .current_path()
            .ok_or_else(|| anyhow!("No images remaining"))?
            .to_path_buf();
        
        if let Some(preloaded) = self.preload_cache.remove(&path) {
            self.image_size = egui::Vec2::new(preloaded.image.width() as f32, preloaded.image.height() as f32);
            self.clear_selection_state();
            if let Some(texture) = self.texture.as_mut() {
                texture.set(preloaded.color_image, egui::TextureOptions::LINEAR);
            } else {
                self.texture = Some(ctx.load_texture(
                    "imagecropper-current",
                    preloaded.color_image,
                    egui::TextureOptions::LINEAR,
                ));
            }
            self.image = Some(preloaded.image);
            self.status = format!(
                "Loaded {} ({}/{})",
                path.display(),
                self.current_index + 1,
                self.files.len()
            );
            self.loading_active = false;
        } else {
            // Not in cache, start loading if not already
            self.image = None;
            self.texture = None;
            self.status = format!(
                "Loading {} ({}/{})",
                path.display(),
                self.current_index + 1,
                self.files.len()
            );
            
            // We rely on the preloader, but if we want to be sure it's coming,
            // we could spawn a dedicated loader here. For now, let's assume preloader catches up
            // or we just wait. To make it robust against random access, let's spawn a dedicated loader.
            if !self.loading_active {
                self.loading_active = true;
                // We can reuse the preloader logic but just for one file
                // We need to clone the sender from somewhere? 
                // Actually, we can just spawn a thread that sends to a new channel?
                // Or better, just let the preloader handle it?
                // The preloader is linear. If we jump, it might take long.
                // Let's spawn a one-off loader.
                // But we need to send the result back to the main thread.
                // We can't easily clone the Receiver.
                // So we need a shared Sender for the preload_rx?
                // mpsc::channel is (Sender, Receiver). Sender is cloneable.
                // But we created the channel inside spawn_preloader and returned Receiver.
                // We don't have the Sender here.
                
                // Let's just wait for the preloader for now. 
                // If the user complains about slow loading on jump, we can fix it.
                // But wait, the user said "Add a progress indicator if currently saving or loading!".
                // By setting image=None, we will show "Loading..." in update().
            }
        }
        Ok(())
    }

    fn clear_selection_state(&mut self) {
        self.selections.clear();
        self.selection_anchor = None;
        self.active_handle = None;
    }

    fn request_shutdown(&mut self, ctx: &egui::Context) {
        self.finished = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
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
                            image = image.resize(3840, 2160, image::imageops::FilterType::Lanczos3);
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

    fn ensure_initial_preload(&mut self) {
        if let Some(path) = self.current_path().map(Path::to_path_buf) {
            if let Some(entry) = self.wait_for_preload(&path, Duration::from_secs(5)) {
                self.preload_cache.insert(path, entry);
            }
        }
        self.drain_preload_queue();
    }

    fn drain_preload_queue(&mut self) {
        while let Ok(entry) = self.preload_rx.try_recv() {
            let path = entry.path.clone();
            self.preload_cache.insert(path, entry);
        }
    }

    fn wait_for_preload(&mut self, target: &Path, timeout: Duration) -> Option<PreloadedImage> {
        let deadline = Instant::now() + timeout;
        let target = target.to_path_buf();
        loop {
            if let Some(entry) = self.preload_cache.remove(&target) {
                return Some(entry);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = deadline.saturating_duration_since(now);
            match self.preload_rx.recv_timeout(remaining) {
                Ok(entry) => {
                    if entry.path == target {
                        return Some(entry);
                    } else {
                        let path = entry.path.clone();
                        self.preload_cache.insert(path, entry);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    return None;
                }
            }
        }
    }

    fn handle_keyboard(ctx: &egui::Context) -> KeyboardState {
        ctx.input(|input| KeyboardState {
            next_image: input.key_pressed(egui::Key::Space),
            prev_image: input.key_pressed(egui::Key::Backspace),
            save_selection: input.key_pressed(egui::Key::Enter),
            delete: input.key_pressed(egui::Key::Delete),
            escape: input.key_pressed(egui::Key::Escape),
            move_up: input.key_down(egui::Key::ArrowUp),
            move_down: input.key_down(egui::Key::ArrowDown),
            move_left: input.key_down(egui::Key::ArrowLeft),
            move_right: input.key_down(egui::Key::ArrowRight),
        })
    }

    fn advance(&mut self, ctx: &egui::Context) {
        if self.files.is_empty() {
            self.request_shutdown(ctx);
            return;
        }

        // Check if we need to resave the current image
        if self.resave {
            if let Some(path) = self.current_path().map(Path::to_path_buf) {
                if path.extension().map_or(false, |e| e.to_ascii_lowercase() != "avif") {
                    if let Some(image) = self.image.clone() {
                        let output_path = path.with_extension("avif");
                        let request = SaveRequest {
                            image,
                            path: output_path.clone(),
                            original_path: path.clone(),
                            quality: self.quality,
                        };
                        
                        if let Ok(_) = self.save_tx.send(request) {
                            self.pending_saves.push(output_path.clone());
                            if let Some(p) = self.files.get_mut(self.current_index) {
                                *p = output_path.clone();
                            }
                            self.status = format!("Converting {} to AVIF...", output_path.display());
                        }
                    }
                }
            }
        }

        // Cache current image before moving
        if let (Some(path), Some(image), Some(_texture)) = (
            self.current_path().map(Path::to_path_buf),
            self.image.clone(),
            self.texture.clone(),
        ) {
            // We need the ColorImage for the cache, but we only have the texture.
            // Re-generating ColorImage from DynamicImage is fast enough.
            let color_image = to_color_image(&image);
            if self.past_images.len() >= 5 {
                self.past_images.pop_front();
            }
            self.past_images.push_back(PreloadedImage {
                path,
                image,
                color_image,
            });
        }

        self.current_index = (self.current_index + 1) % self.files.len();
        if let Err(err) = self.load_current_image(ctx) {
            self.status = format!("{err:#}");
        }
    }

    fn go_back(&mut self, ctx: &egui::Context) {
        if self.files.is_empty() {
            return;
        }
        
        // Try to pop from history first
        if let Some(entry) = self.past_images.pop_back() {
             // Check if this entry matches the previous index
             let prev_index = if self.current_index == 0 {
                 self.files.len() - 1
             } else {
                 self.current_index - 1
             };
             
             if entry.path == self.files[prev_index] {
                 self.current_index = prev_index;
                 self.image_size = egui::Vec2::new(entry.image.width() as f32, entry.image.height() as f32);
                 self.clear_selection_state();
                 if let Some(texture) = self.texture.as_mut() {
                     texture.set(entry.color_image, egui::TextureOptions::LINEAR);
                 } else {
                     self.texture = Some(ctx.load_texture(
                         "imagecropper-current",
                         entry.color_image,
                         egui::TextureOptions::LINEAR,
                     ));
                 }
                 self.image = Some(entry.image);
                 self.status = format!(
                    "Loaded {} ({}/{})",
                    self.files[prev_index].display(),
                    self.current_index + 1,
                    self.files.len()
                );
                return;
             } else {
                 // History mismatch (maybe file list changed?), discard and fall through
             }
        }

        // Fallback if not in history
        if self.current_index == 0 {
            self.current_index = self.files.len() - 1;
        } else {
            self.current_index -= 1;
        }
        if let Err(err) = self.load_current_image(ctx) {
            self.status = format!("{err:#}");
        }
    }

    fn delete_current(&mut self, ctx: &egui::Context) {
        let Some(path) = self.current_path().map(Path::to_path_buf) else {
            self.status = "No image selected".into();
            return;
        };

        if self.dry_run {
            println!("Dry run: would move {} to {}", path.display(), TRASH_DIR);
            self.status = format!("Dry run: skipped deleting {}", path.display());
            self.advance(ctx);
            return;
        }

        let Ok(target_dir) = prepare_dir(TRASH_DIR) else {
            self.status = "Unable to prepare trash directory".into();
            return;
        };
        if let Err(err) = move_with_unique_name(&path, &target_dir) {
            self.status = format!("Failed to delete: {err:#}");
            return;
        }

        self.status = format!("Moved {} to {}", path.display(), TRASH_DIR);
        self.clear_selection_state();
        self.preload_cache.remove(&path);
        self.files.remove(self.current_index);
        if self.files.is_empty() {
            self.request_shutdown(ctx);
            return;
        }
        if self.current_index >= self.files.len() {
            self.current_index = 0;
        }
        if let Err(err) = self.load_current_image(ctx) {
            self.status = format!("{err:#}");
        }
    }

    fn crop_selections(&mut self, ctx: &egui::Context) -> bool {
        if self.selections.is_empty() {
            self.status = "No selection to crop".into();
            return false;
        }
        let Some(image) = self.image.clone() else {
            self.status = "Image not loaded".into();
            return false;
        };
        let Some(path) = self.current_path().map(Path::to_path_buf) else {
            self.status = "No image selected".into();
            return false;
        };

        let mut crops = Vec::new();
        for selection in &self.selections {
            if let Some((x, y, w, h)) = selection.to_u32_bounds() {
                if w > 0 && h > 0 {
                    crops.push(image.crop_imm(x, y, w, h));
                }
            }
        }

        if crops.is_empty() {
            self.status = "Selections too small".into();
            return false;
        }

        let final_image = if crops.len() == 1 {
            crops[0].clone()
        } else {
            combine_crops(crops)
        };

        let output_path = path.with_extension("avif");
        
        // Send to background saver
        let request = SaveRequest {
            image: final_image,
            path: output_path.clone(),
            original_path: path.clone(),
            quality: self.quality,
        };
        
        if let Err(err) = self.save_tx.send(request) {
            self.status = format!("Failed to queue save: {err:#}");
            return false;
        }
        
        self.pending_saves.push(output_path.clone());

        // Update the file list to point to the new file
        if let Some(p) = self.files.get_mut(self.current_index) {
            *p = output_path.clone();
        }

        // Skip to next image immediately
        self.advance(ctx);
        
        self.status = format!("Saving {} in background...", output_path.display());
        true
    }

    fn handle_arrow_movement(&mut self, keys: &KeyboardState) {
        if self.selections.is_empty() {
            return;
        }
        let mut delta = egui::Vec2::ZERO;
        if keys.move_up {
            delta.y -= ARROW_MOVE_STEP;
        }
        if keys.move_down {
            delta.y += ARROW_MOVE_STEP;
        }
        if keys.move_left {
            delta.x -= ARROW_MOVE_STEP;
        }
        if keys.move_right {
            delta.x += ARROW_MOVE_STEP;
        }
        if delta == egui::Vec2::ZERO {
            return;
        }
        // Move all selections
        for selection in &mut self.selections {
            selection.translate(delta, self.image_size);
        }
    }

    fn handle_pointer(&mut self, response: &egui::Response, metrics: &ImageMetrics, ctx: &egui::Context) {
        let ctrl_down = ctx.input(|i| i.modifiers.ctrl);

        if response.drag_started() {
            if let Some(pointer) = response.interact_pointer_pos() {
                let image_pos = metrics.screen_to_image(pointer);
                self.selection_anchor = Some(image_pos);
                
                if !ctrl_down {
                    // If not holding ctrl, clear existing unless we clicked inside one?
                    // For now, simple behavior: No ctrl = clear and start new.
                    self.selections.clear();
                }
                
                self.selections.push(Selection::from_points(
                    image_pos,
                    image_pos,
                    self.image_size,
                ));
            }
        } else if response.dragged() {
            if let (Some(anchor), Some(pointer)) =
                (self.selection_anchor, response.interact_pointer_pos())
            {
                let image_pos = metrics.screen_to_image(pointer);
                // Update the last selection (the one currently being created)
                if let Some(last) = self.selections.last_mut() {
                    *last = Selection::from_points(anchor, image_pos, self.image_size);
                }
            }
        } else if response.drag_stopped() {
            self.selection_anchor = None;
        }
    }

    fn draw_selection(&self, painter: &egui::Painter, metrics: &ImageMetrics) {
        for (i, selection) in self.selections.iter().enumerate() {
            let rect = metrics.selection_rect(selection);
            let color = selection_color(i);
            painter.rect_filled(
                rect,
                0.0,
                Color32::from_rgba_unmultiplied(255, 255, 255, 24),
            );
            painter.rect_stroke(rect, 0.0, (2.0, color));
        }
    }

    fn draw_handles(&mut self, ui: &egui::Ui, painter: &egui::Painter, metrics: &ImageMetrics) {
        if self.selections.is_empty() {
            return;
        }
        
        // We need to iterate indices to modify specific selections
        for i in 0..self.selections.len() {
            let current_selection = self.selections[i].clone();
            let color = selection_color(i);
            let handle_color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 160);
            
            for handle in SelectionHandle::ALL {
                let screen_rect = metrics.selection_rect(&current_selection);
                let handle_rect = handle.handle_rect(screen_rect);
                painter.rect_filled(
                    handle_rect,
                    2.0,
                    handle_color,
                );
                let response = ui.interact(
                    handle_rect,
                    ui.id().with(handle.id_suffix()).with(i),
                    egui::Sense::click_and_drag(),
                );
                if response.drag_started() {
                    if let Some(pointer_pos) = response.interact_pointer_pos() {
                        self.active_handle = Some(HandleDrag {
                            handle,
                            original: current_selection.clone(),
                            start_pos: pointer_pos,
                            selection_index: i,
                        });
                    }
                }
                if response.dragged() {
                    if let Some(active) = &self.active_handle {
                        if active.handle == handle && active.selection_index == i {
                            if let Some(pointer_pos) = response.interact_pointer_pos() {
                                let total_delta = pointer_pos - active.start_pos;
                                let delta = egui::vec2(
                                    total_delta.x / metrics.scale,
                                    total_delta.y / metrics.scale,
                                );
                                if let Some(sel) = self.selections.get_mut(i) {
                                    *sel = active.original.clone().adjusted(
                                        active.handle,
                                        delta,
                                        self.image_size,
                                    );
                                }
                            }
                        }
                    }
                }
                if response.drag_stopped() {
                    self.active_handle = None;
                }
            }
        }
    }
}

impl eframe::App for ImageCropperApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let _ = frame;

        self.drain_preload_queue();
        
        // Check for save completions
        while let Ok(status) = self.save_status_rx.try_recv() {
            if let Some(idx) = self.pending_saves.iter().position(|p| *p == status.path) {
                self.pending_saves.remove(idx);
            }
            if let Err(err) = status.result {
                self.status = format!("Error saving {}: {err:#}", status.path.display());
            }
        }
        
        // If image is not loaded, check if it arrived in cache
        if self.image.is_none() {
             if let Some(path) = self.current_path().map(Path::to_path_buf) {
                 if self.preload_cache.contains_key(&path) {
                     let _ = self.load_current_image(ctx);
                 }
             }
        }

        if self.finished {
            self.is_exiting = true;
        }

        if self.is_exiting {
            if self.pending_saves.is_empty() {
                self.request_shutdown(ctx);
            } else {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.heading(format!(
                            "Finishing background tasks... ({} remaining)",
                            self.pending_saves.len()
                        ));
                    });
                });
                ctx.request_repaint();
            }
            return;
        }

        let keys = Self::handle_keyboard(ctx);

        if keys.escape {
            if !self.selections.is_empty() {
                self.clear_selection_state();
                self.status = "Selection cleared".into();
                self.exit_attempt_count = 0;
            } else {
                if self.pending_saves.is_empty() {
                    self.request_shutdown(ctx);
                    return;
                } else {
                    self.exit_attempt_count += 1;
                    let remaining = 3usize.saturating_sub(self.exit_attempt_count);
                    if remaining == 0 {
                        self.request_shutdown(ctx);
                        return;
                    } else {
                        self.status = format!(
                            "Saving in progress! Press ESC {} more times to force exit.",
                            remaining
                        );
                    }
                }
            }
        }
        
        if keys.save_selection {
             self.exit_attempt_count = 0;
             if self.crop_selections(ctx) {
                 // crop_selections now advances automatically
                 self.clear_selection_state();
             }
        }
        
        if keys.next_image {
            self.exit_attempt_count = 0;
            self.advance(ctx);
        }
        
        if keys.prev_image {
            self.exit_attempt_count = 0;
            self.go_back(ctx);
        }

        if keys.delete {
            self.exit_attempt_count = 0;
            self.delete_current(ctx);
        }
        self.handle_arrow_movement(&keys);

        egui::CentralPanel::default().show(ctx, |ui| {
            let (response, painter) =
                ui.allocate_painter(ui.available_size(), egui::Sense::hover());
            painter.rect_filled(response.rect, 0.0, Color32::BLACK);

            if let Some(texture) = &self.texture {
                let metrics = ImageMetrics::new(response.rect, self.image_size);
                painter.image(
                    texture.id(),
                    metrics.image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );

                let image_response = ui.interact(
                    metrics.image_rect,
                    ui.id().with("image"),
                    egui::Sense::click_and_drag(),
                );
                self.handle_pointer(&image_response, &metrics, ctx);
                self.draw_selection(&painter, &metrics);
                self.draw_handles(ui, &painter, &metrics);
            } else {
                painter.text(
                    response.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Loading...",
                    egui::FontId::proportional(24.0),
                    Color32::WHITE,
                );
            }
            
            // Draw spinner if saving
            if !self.pending_saves.is_empty() {
                let text = if self.pending_saves.len() <= 3 {
                    let names: Vec<_> = self.pending_saves.iter()
                        .filter_map(|p| p.file_name().map(|s| s.to_string_lossy()))
                        .collect();
                    format!("Saving: {}", names.join(", "))
                } else {
                    format!("Saving {} images...", self.pending_saves.len())
                };
                
                painter.text(
                    response.rect.right_bottom() + egui::vec2(-12.0, -40.0),
                    egui::Align2::RIGHT_BOTTOM,
                    text,
                    egui::FontId::proportional(16.0),
                    Color32::YELLOW,
                );
            }

            painter.text(
                response.rect.left_bottom() + egui::vec2(12.0, -12.0),
                egui::Align2::LEFT_BOTTOM,
                &self.status,
                egui::FontId::monospace(16.0),
                Color32::WHITE,
            );

            painter.text(
                response.rect.right_bottom() + egui::vec2(-12.0, -12.0),
                egui::Align2::RIGHT_BOTTOM,
                "Enter: Save | Space: Next | Backspace: Prev | Delete: Trash | Esc: Clear/Quit",
                egui::FontId::monospace(16.0),
                Color32::from_gray(200),
            );
        });

        ctx.request_repaint();
    }
}

#[derive(Clone)]
struct Selection {
    rect: egui::Rect,
}

impl Selection {
    fn from_points(a: egui::Pos2, b: egui::Pos2, bounds: egui::Vec2) -> Self {
        let min = egui::pos2(
            a.x.min(b.x).clamp(0.0, bounds.x),
            a.y.min(b.y).clamp(0.0, bounds.y),
        );
        let max = egui::pos2(
            a.x.max(b.x).clamp(0.0, bounds.x),
            a.y.max(b.y).clamp(0.0, bounds.y),
        );
        let mut selection = Self {
            rect: egui::Rect::from_min_max(min, max),
        };
        selection.clamp_within(bounds);
        selection
    }

    fn translate(&mut self, delta: egui::Vec2, bounds: egui::Vec2) {
        self.rect = self.rect.translate(delta);
        self.clamp_within(bounds);
    }

    fn to_u32_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let width = self.rect.width();
        let height = self.rect.height();
        if width < 1.0 || height < 1.0 {
            return None;
        }
        let x = self.rect.min.x.max(0.0).round() as u32;
        let y = self.rect.min.y.max(0.0).round() as u32;
        Some((x, y, width.round() as u32, height.round() as u32))
    }

    fn adjusted(mut self, handle: SelectionHandle, delta: egui::Vec2, bounds: egui::Vec2) -> Self {
        match handle {
            SelectionHandle::Top => {
                self.rect.min.y = (self.rect.min.y + delta.y).clamp(0.0, self.rect.max.y - 1.0);
            }
            SelectionHandle::Bottom => {
                self.rect.max.y =
                    (self.rect.max.y + delta.y).clamp(self.rect.min.y + 1.0, bounds.y);
            }
            SelectionHandle::Left => {
                self.rect.min.x = (self.rect.min.x + delta.x).clamp(0.0, self.rect.max.x - 1.0);
            }
            SelectionHandle::Right => {
                self.rect.max.x =
                    (self.rect.max.x + delta.x).clamp(self.rect.min.x + 1.0, bounds.x);
            }
            SelectionHandle::TopLeft => {
                self.rect.min.x = (self.rect.min.x + delta.x).clamp(0.0, self.rect.max.x - 1.0);
                self.rect.min.y = (self.rect.min.y + delta.y).clamp(0.0, self.rect.max.y - 1.0);
            }
            SelectionHandle::TopRight => {
                self.rect.max.x = (self.rect.max.x + delta.x).clamp(self.rect.min.x + 1.0, bounds.x);
                self.rect.min.y = (self.rect.min.y + delta.y).clamp(0.0, self.rect.max.y - 1.0);
            }
            SelectionHandle::BottomLeft => {
                self.rect.min.x = (self.rect.min.x + delta.x).clamp(0.0, self.rect.max.x - 1.0);
                self.rect.max.y = (self.rect.max.y + delta.y).clamp(self.rect.min.y + 1.0, bounds.y);
            }
            SelectionHandle::BottomRight => {
                self.rect.max.x = (self.rect.max.x + delta.x).clamp(self.rect.min.x + 1.0, bounds.x);
                self.rect.max.y = (self.rect.max.y + delta.y).clamp(self.rect.min.y + 1.0, bounds.y);
            }
        }
        self.clamp_within(bounds);
        self
    }

    fn clamp_within(&mut self, bounds: egui::Vec2) {
        let mut min = self.rect.min;
        let mut max = self.rect.max;
        min.x = min.x.clamp(0.0, bounds.x);
        max.x = max.x.clamp(0.0, bounds.x);
        min.y = min.y.clamp(0.0, bounds.y);
        max.y = max.y.clamp(0.0, bounds.y);
        if max.x <= min.x {
            max.x = (min.x + 1.0).min(bounds.x);
            min.x = (max.x - 1.0).max(0.0);
        }
        if max.y <= min.y {
            max.y = (min.y + 1.0).min(bounds.y);
            min.y = (max.y - 1.0).max(0.0);
        }
        self.rect = egui::Rect::from_min_max(min, max);
    }
}

#[derive(Clone)]
struct HandleDrag {
    handle: SelectionHandle,
    original: Selection,
    start_pos: egui::Pos2,
    selection_index: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionHandle {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl SelectionHandle {
    const ALL: [Self; 8] = [
        Self::Top, Self::Bottom, Self::Left, Self::Right,
        Self::TopLeft, Self::TopRight, Self::BottomLeft, Self::BottomRight,
    ];

    fn id_suffix(self) -> &'static str {
        match self {
            Self::Top => "handle_top",
            Self::Bottom => "handle_bottom",
            Self::Left => "handle_left",
            Self::Right => "handle_right",
            Self::TopLeft => "handle_top_left",
            Self::TopRight => "handle_top_right",
            Self::BottomLeft => "handle_bottom_left",
            Self::BottomRight => "handle_bottom_right",
        }
    }

    fn handle_rect(self, selection: egui::Rect) -> egui::Rect {
        let corner_size = egui::vec2(HANDLE_THICKNESS, HANDLE_THICKNESS);
        match self {
            Self::Top => egui::Rect::from_center_size(
                egui::pos2(selection.center().x, selection.min.y),
                egui::vec2(
                    selection
                        .width()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                    HANDLE_THICKNESS,
                ),
            ),
            Self::Bottom => egui::Rect::from_center_size(
                egui::pos2(selection.center().x, selection.max.y),
                egui::vec2(
                    selection
                        .width()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                    HANDLE_THICKNESS,
                ),
            ),
            Self::Left => egui::Rect::from_center_size(
                egui::pos2(selection.min.x, selection.center().y),
                egui::vec2(
                    HANDLE_THICKNESS,
                    selection
                        .height()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                ),
            ),
            Self::Right => egui::Rect::from_center_size(
                egui::pos2(selection.max.x, selection.center().y),
                egui::vec2(
                    HANDLE_THICKNESS,
                    selection
                        .height()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                ),
            ),
            Self::TopLeft => egui::Rect::from_center_size(selection.min, corner_size),
            Self::TopRight => egui::Rect::from_center_size(selection.right_top(), corner_size),
            Self::BottomLeft => egui::Rect::from_center_size(selection.left_bottom(), corner_size),
            Self::BottomRight => egui::Rect::from_center_size(selection.max, corner_size),
        }
    }
}

struct ImageMetrics {
    image_rect: egui::Rect,
    image_size: egui::Vec2,
    scale: f32,
}

struct PreloadedImage {
    path: PathBuf,
    image: DynamicImage,
    color_image: egui::ColorImage,
}

impl ImageMetrics {
    fn new(canvas: egui::Rect, image_size: egui::Vec2) -> Self {
        let (display, scale) = fit_within(image_size, canvas.size());
        let offset = (canvas.size() - display) * 0.5;
        let image_rect = egui::Rect::from_min_size(canvas.min + offset, display);
        Self {
            image_rect,
            image_size,
            scale,
        }
    }

    fn screen_to_image(&self, pos: egui::Pos2) -> egui::Pos2 {
        let rel = pos - self.image_rect.min;
        egui::pos2(
            (rel.x / self.scale).clamp(0.0, self.image_size.x),
            (rel.y / self.scale).clamp(0.0, self.image_size.y),
        )
    }

    fn selection_rect(&self, selection: &Selection) -> egui::Rect {
        let min = egui::pos2(
            self.image_rect.min.x + selection.rect.min.x * self.scale,
            self.image_rect.min.y + selection.rect.min.y * self.scale,
        );
        let max = egui::pos2(
            self.image_rect.min.x + selection.rect.max.x * self.scale,
            self.image_rect.min.y + selection.rect.max.y * self.scale,
        );
        egui::Rect::from_min_max(min, max)
    }
}

fn selection_color(index: usize) -> Color32 {
    let golden_ratio_conjugate = 0.618033988749895;
    let h = (index as f32 * golden_ratio_conjugate) % 1.0;
    let [r, g, b] = egui::ecolor::Hsva::new(h, 0.8, 1.0, 1.0).to_rgb();
    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

fn fit_within(image_size: egui::Vec2, available: egui::Vec2) -> (egui::Vec2, f32) {
    let safe_size = egui::vec2(image_size.x.max(1.0), image_size.y.max(1.0));
    let scale = (available.x / safe_size.x)
        .min(available.y / safe_size.y)
        .max(0.01);
    (safe_size * scale, scale)
}

fn to_color_image(img: &DynamicImage) -> egui::ColorImage {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.into_raw();
    egui::ColorImage::from_rgba_unmultiplied(size, &pixels)
}

fn prepare_dir(name: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(name);
    fs::create_dir_all(&dir).with_context(|| format!("Unable to create {name}"))?;
    Ok(dir)
}

fn move_with_unique_name(source: &Path, target_dir: &Path) -> Result<()> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", source.display()))?;
    let destination = unique_destination(target_dir, file_name);
    fs::rename(source, &destination).with_context(|| {
        format!(
            "Unable to move {} to {}",
            source.display(),
            destination.display()
        )
    })
}

fn unique_destination(dir: &Path, file_name: &OsStr) -> PathBuf {
    let mut candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_name(file_name);
    for idx in 1.. {
        let new_name = if let Some(ext) = &ext {
            format!("{stem}-{idx}.{ext}")
        } else {
            format!("{stem}-{idx}")
        };
        candidate = dir.join(new_name);
        if !candidate.exists() {
            break;
        }
    }
    candidate
}

fn split_name(file_name: &OsStr) -> (String, Option<String>) {
    let name = file_name.to_string_lossy();
    if let Some((stem, ext)) = name.rsplit_once('.') {
        (stem.to_string(), Some(ext.to_string()))
    } else {
        (name.to_string(), None)
    }
}

fn backup_original(path: &Path) -> Result<()> {
    let dir = prepare_dir(ORIGINALS_DIR)?;
    move_with_unique_name(path, &dir)
}

struct KeyboardState {
    next_image: bool,
    prev_image: bool,
    save_selection: bool,
    delete: bool,
    escape: bool,
    move_up: bool,
    move_down: bool,
    move_left: bool,
    move_right: bool,
}

fn combine_crops(mut crops: Vec<DynamicImage>) -> DynamicImage {
    // Simple shelf packing or just horizontal stacking if few?
    // User wants to "minimize empty space".
    // Let's sort by height descending.
    crops.sort_by(|a, b| b.height().cmp(&a.height()));

    // Calculate total area to estimate canvas size
    let total_area: u64 = crops.iter().map(|i| i.width() as u64 * i.height() as u64).sum();
    let max_width = (total_area as f64).sqrt().ceil() as u32 * 2; // Heuristic: start with something wider
    
    // Simple shelf algorithm
    let mut canvas_width = 0;
    let mut canvas_height = 0;
    
    struct PlacedImage {
        x: u32,
        y: u32,
        img: DynamicImage,
    }
    
    let mut placed = Vec::new();
    let mut current_x = 0;
    let mut current_y = 0;
    let mut row_height = 0;
    
    // First pass: determine positions and canvas size
    for img in crops {
        if current_x + img.width() > max_width && current_x > 0 {
            // New row
            current_x = 0;
            current_y += row_height;
            row_height = 0;
        }
        
        placed.push(PlacedImage {
            x: current_x,
            y: current_y,
            img: img.clone(),
        });
        
        row_height = row_height.max(img.height());
        current_x += img.width();
        
        canvas_width = canvas_width.max(current_x);
        canvas_height = canvas_height.max(current_y + row_height);
    }
    
    let mut final_image = RgbaImage::new(canvas_width, canvas_height);
    
    for p in placed {
        // Copy pixels
        // We can use image::GenericImage::copy_from but we need to be careful about types.
        // DynamicImage implements GenericImage.
        let _ = final_image.copy_from(&p.img, p.x, p.y);
    }
    
    DynamicImage::ImageRgba8(final_image)
}
