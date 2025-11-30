use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use eframe::{
    egui::{self, Color32, ViewportCommand},
    App, Frame,
};
use image::{codecs::avif::AvifEncoder, DynamicImage};

use crate::{
    fs_utils::{backup_original, move_with_unique_name, prepare_dir, TEMP_DIR, TRASH_DIR},
    image_utils::{
        combine_crops, to_color_image, OutputFormat, PreloadedImage, SaveRequest, SaveStatus,
    },
    selection::{selection_color, HandleDrag, Selection, SelectionHandle},
    ui::{ImageMetrics, KeyboardState, ARROW_MOVE_STEP},
};

pub struct ImageCropperApp {
    pub files: Vec<PathBuf>,
    pub current_index: usize,
    pub dry_run: bool,
    pub quality: u8,
    pub resave: bool,
    pub format: OutputFormat,
    pub image: Option<DynamicImage>,
    pub texture: Option<egui::TextureHandle>,
    pub preview_texture: Option<egui::TextureHandle>,
    pub image_size: egui::Vec2,
    pub selections: Vec<Selection>,
    pub selection_anchor: Option<egui::Pos2>,
    pub active_handle: Option<HandleDrag>,
    pub status: String,
    pub finished: bool,
    pub preload_rx: Receiver<PreloadedImage>,
    pub preload_cache: HashMap<PathBuf, PreloadedImage>,
    pub past_images: VecDeque<PreloadedImage>,
    pub save_tx: mpsc::Sender<SaveRequest>,
    pub save_status_rx: Receiver<SaveStatus>,
    pub pending_saves: Vec<PathBuf>,
    pub loading_active: bool,
    pub is_exiting: bool,
    pub exit_attempt_count: usize,
    pub list_completed: bool,
    pub windowed_mode_set: bool,
}

impl ImageCropperApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        files: Vec<PathBuf>,
        dry_run: bool,
        quality: u8,
        resave: bool,
        format: OutputFormat,
    ) -> Result<Self> {
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
            format,
            image: None,
            texture: None,
            preview_texture: None,
            image_size: egui::Vec2::new(1.0, 1.0),
            selections: Vec::new(),
            selection_anchor: None,
            active_handle: None,
            status: String::from("Ready"),
            finished: false,
            preload_rx,
            preload_cache: HashMap::new(),
            past_images: VecDeque::with_capacity(10),
            save_tx,
            save_status_rx,
            pending_saves: Vec::new(),
            loading_active: false,
            is_exiting: false,
            exit_attempt_count: 0,
            list_completed: false,
            windowed_mode_set: false,
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
                                let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(writer, req.quality);
                                req.image.write_with_encoder(encoder)?;
                            }
                            OutputFormat::Png => {
                                let encoder = image::codecs::png::PngEncoder::new(writer);
                                req.image.write_with_encoder(encoder)?;
                            }
                            OutputFormat::Webp => {
                                // WebP encoder in image crate is currently lossless only or limited?
                                // Using new_lossless for now.
                                let encoder = image::codecs::webp::WebPEncoder::new_lossless(writer);
                                req.image.write_with_encoder(encoder)?;
                            }
                            OutputFormat::Avif => {
                                let encoder = AvifEncoder::new_with_speed_quality(writer, 4, req.quality);
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
            self.image_size =
                egui::Vec2::new(preloaded.image.width() as f32, preloaded.image.height() as f32);
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

            if !self.loading_active {
                self.loading_active = true;
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
        if self.pending_saves.is_empty() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
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
            preview: input.key_down(egui::Key::P),
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
                if path
                    .extension()
                    .map_or(false, |e| e.to_ascii_lowercase() != self.format.extension())
                {
                    if let Some(image) = self.image.clone() {
                        let output_path = path.with_extension(self.format.extension());
                        let request = SaveRequest {
                            image,
                            path: output_path.clone(),
                            original_path: path.clone(),
                            quality: self.quality,
                            format: self.format,
                        };

                        if let Ok(_) = self.save_tx.send(request) {
                            self.pending_saves.push(output_path.clone());
                            if let Some(p) = self.files.get_mut(self.current_index) {
                                *p = output_path.clone();
                            }
                            self.status =
                                format!("Converting {} to {}...", output_path.display(), self.format.extension().to_uppercase());
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
            if self.past_images.len() >= 10 {
                self.past_images.pop_front();
            }
            self.past_images.push_back(PreloadedImage {
                path,
                image,
                color_image,
            });
        }

        if self.current_index + 1 >= self.files.len() {
            self.list_completed = true;
            self.status = "All images processed".into();
            return;
        }

        self.current_index += 1;
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
                self.image_size =
                    egui::Vec2::new(entry.image.width() as f32, entry.image.height() as f32);
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
            self.list_completed = true;
            self.status = "No images remaining".into();
            return;
        }
        if self.current_index >= self.files.len() {
            self.list_completed = true;
            self.status = "All images processed".into();
            return;
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

        let output_path = path.with_extension(self.format.extension());

        // Send to background saver
        let request = SaveRequest {
            image: final_image,
            path: output_path.clone(),
            original_path: path.clone(),
            quality: self.quality,
            format: self.format,
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

    fn generate_preview(&mut self, ctx: &egui::Context) {
        let Some(image) = self.image.clone() else { return };

        let mut crops = Vec::new();
        for selection in &self.selections {
            if let Some((x, y, w, h)) = selection.to_u32_bounds() {
                if w > 0 && h > 0 {
                    crops.push(image.crop_imm(x, y, w, h));
                }
            }
        }

        if crops.is_empty() {
            return;
        }

        let final_image = if crops.len() == 1 {
            crops[0].clone()
        } else {
            combine_crops(crops)
        };

        let color_image = to_color_image(&final_image);
        self.preview_texture = Some(ctx.load_texture(
            "preview-texture",
            color_image,
            egui::TextureOptions::LINEAR,
        ));
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

    fn handle_pointer(
        &mut self,
        response: &egui::Response,
        metrics: &ImageMetrics,
        ctx: &egui::Context,
    ) {
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
            let handle_color =
                Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 160);

            for handle in SelectionHandle::ALL {
                let screen_rect = metrics.selection_rect(&current_selection);
                let handle_rect = handle.handle_rect(screen_rect);
                painter.rect_filled(handle_rect, 2.0, handle_color);
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

impl App for ImageCropperApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut Frame) {
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
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                if !self.windowed_mode_set {
                    ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(400.0, 200.0)));
                    self.windowed_mode_set = true;
                }
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

        if self.list_completed {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("All images processed!");
                        ui.add_space(20.0);
                        if ui.button("Start Over").clicked() {
                            self.list_completed = false;
                            self.current_index = 0;
                            if let Err(err) = self.load_current_image(ctx) {
                                self.status = format!("{err:#}");
                            }
                        }
                        ui.add_space(10.0);
                        if ui.button("Quit").clicked() {
                            self.finished = true;
                        }
                    });
                });
            });
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

            if keys.preview && !self.selections.is_empty() {
                if self.preview_texture.is_none() {
                    self.generate_preview(ctx);
                }

                if let Some(texture) = &self.preview_texture {
                    let metrics = ImageMetrics::new(response.rect, texture.size_vec2());
                    painter.image(
                        texture.id(),
                        metrics.image_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );

                    painter.text(
                        response.rect.left_top() + egui::vec2(10.0, 10.0),
                        egui::Align2::LEFT_TOP,
                        "PREVIEW MODE",
                        egui::FontId::proportional(20.0),
                        Color32::YELLOW,
                    );
                }
            } else {
                self.preview_texture = None;

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
                "Enter: Save | Space: Next | Backspace: Prev | Delete: Trash | P: Preview | Esc: Clear/Quit",
                egui::FontId::monospace(16.0),
                Color32::from_gray(200),
            );
        });

        ctx.request_repaint();
    }
}
