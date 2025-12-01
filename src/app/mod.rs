pub mod canvas;
pub mod loader;
pub mod saver;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use eframe::{
    egui::{self, Color32, ViewportCommand},
    App, Frame,
};
use image::DynamicImage;

use crate::{
    fs_utils::{move_with_unique_name, prepare_dir, TRASH_DIR},
    image_utils::{combine_crops, to_color_image, OutputFormat, PreloadedImage, SaveRequest},
    ui::{ImageMetrics, KeyboardState},
};

use self::{canvas::Canvas, loader::Loader, saver::Saver};

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
    pub canvas: Canvas,
    pub loader: Loader,
    pub saver: Saver,
    pub status: String,
    pub finished: bool,
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
        parallel: usize,
    ) -> Result<Self> {
        let loader = Loader::new(files.clone());
        let saver = Saver::new(parallel);
        let canvas = Canvas::new();

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
            canvas,
            loader,
            saver,
            status: String::from("Ready"),
            finished: false,
            is_exiting: false,
            exit_attempt_count: 0,
            list_completed: false,
            windowed_mode_set: false,
        };
        app.load_current_image(&cc.egui_ctx)?;
        Ok(app)
    }

    fn current_path(&self) -> Option<&Path> {
        self.files.get(self.current_index).map(|p| p.as_path())
    }

    fn load_current_image(&mut self, ctx: &egui::Context) -> Result<()> {
        self.loader.update();
        let path = self
            .current_path()
            .ok_or_else(|| anyhow!("No images remaining"))?
            .to_path_buf();

        if let Some(preloaded) = self.loader.get_from_cache(&path) {
            self.image_size =
                egui::Vec2::new(preloaded.image.width() as f32, preloaded.image.height() as f32);
            self.canvas.clear();
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
            self.loader.loading_active = false;
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

            if !self.loader.loading_active {
                self.loader.loading_active = true;
            }
        }
        Ok(())
    }

    fn request_shutdown(&mut self, ctx: &egui::Context) {
        self.finished = true;
        if self.saver.pending_saves.is_empty() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
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

                        if let Ok(_) = self.saver.queue_save(request) {
                            if let Some(p) = self.files.get_mut(self.current_index) {
                                *p = output_path.clone();
                            }
                            self.status = format!(
                                "Converting {} to {}...",
                                output_path.display(),
                                self.format.extension().to_uppercase()
                            );
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
            self.loader.push_history(PreloadedImage {
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
        if let Some(entry) = self.loader.pop_history() {
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
                self.canvas.clear();
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
        self.canvas.clear();
        self.loader.cache.remove(&path);
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
        if self.canvas.selections.is_empty() {
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
        for selection in &self.canvas.selections {
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

        if let Err(err) = self.saver.queue_save(request) {
            self.status = format!("Failed to queue save: {err:#}");
            return false;
        }

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
        for selection in &self.canvas.selections {
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
}

impl App for ImageCropperApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut Frame) {
        let _ = frame;

        self.loader.update();

        // Check for save completions
        for (path, result) in self.saver.check_completions() {
            if let Err(err) = result {
                self.status = format!("Error saving {}: {err:#}", path.display());
            }
        }

        if self.exit_attempt_count > 0 && self.saver.pending_saves.is_empty() {
            self.request_shutdown(ctx);
            return;
        }

        // If image is not loaded, check if it arrived in cache
        if self.image.is_none() {
            if let Some(path) = self.current_path().map(Path::to_path_buf) {
                if self.loader.cache.contains_key(&path) {
                    let _ = self.load_current_image(ctx);
                }
            }
        }

        if self.finished {
            self.is_exiting = true;
        }

        if self.is_exiting {
            if self.saver.pending_saves.is_empty() {
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
                            self.saver.pending_saves.len()
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
                        if !self.saver.pending_saves.is_empty() {
                            ui.add_space(10.0);
                            ui.label(format!("Processing {} images...", self.saver.pending_saves.len()));
                        }
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
            if !self.canvas.selections.is_empty() {
                self.canvas.clear();
                self.status = "Selection cleared".into();
                self.exit_attempt_count = 0;
            } else {
                if self.saver.pending_saves.is_empty() {
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
                self.canvas.clear();
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
        self.canvas.handle_arrow_movement(&keys, self.image_size);

        egui::CentralPanel::default().show(ctx, |ui| {
            let (response, painter) =
                ui.allocate_painter(ui.available_size(), egui::Sense::hover());
            painter.rect_filled(response.rect, 0.0, Color32::BLACK);

            let draw_text_with_bg = |pos: egui::Pos2, align: egui::Align2, text: String, font: egui::FontId, color: Color32| {
                let galley = ctx.fonts(|fonts| fonts.layout_no_wrap(text, font, color));
                let rect = align.anchor_size(pos, galley.size());
                painter.rect_filled(rect.expand(4.0), 4.0, Color32::from_black_alpha(178));
                painter.galley(rect.min, galley, Color32::WHITE);
            };

            if keys.preview && !self.canvas.selections.is_empty() {
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

                    draw_text_with_bg(
                        response.rect.left_top() + egui::vec2(10.0, 10.0),
                        egui::Align2::LEFT_TOP,
                        "PREVIEW MODE".to_string(),
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
                    self.canvas.handle_pointer(&image_response, &metrics, self.image_size, ctx);
                    self.canvas.draw(ui, &painter, &metrics, self.image_size);
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
            if !self.saver.pending_saves.is_empty() {
                let text = if self.saver.pending_saves.len() <= 3 {
                    let names: Vec<_> = self.saver.pending_saves.iter()
                        .filter_map(|p| p.file_name().map(|s| s.to_string_lossy()))
                        .collect();
                    format!("Saving: {}", names.join(", "))
                } else {
                    format!("Saving {} images...", self.saver.pending_saves.len())
                };

                draw_text_with_bg(
                    response.rect.right_bottom() + egui::vec2(-12.0, -40.0),
                    egui::Align2::RIGHT_BOTTOM,
                    text,
                    egui::FontId::proportional(16.0),
                    Color32::YELLOW,
                );
            }

            draw_text_with_bg(
                response.rect.left_bottom() + egui::vec2(12.0, -12.0),
                egui::Align2::LEFT_BOTTOM,
                self.status.clone(),
                egui::FontId::monospace(16.0),
                Color32::WHITE,
            );

            draw_text_with_bg(
                response.rect.right_bottom() + egui::vec2(-12.0, -12.0),
                egui::Align2::RIGHT_BOTTOM,
                "Enter: Save | Space: Next | Backspace: Prev | Delete: Trash | P: Preview | Esc: Clear/Quit".to_string(),
                egui::FontId::monospace(16.0),
                Color32::from_gray(200),
            );

            // Image X of Y indicator
            draw_text_with_bg(
                response.rect.left_top() + egui::vec2(12.0, 12.0),
                egui::Align2::LEFT_TOP,
                format!("Image {} of {}", self.current_index + 1, self.files.len()),
                egui::FontId::proportional(20.0),
                Color32::WHITE,
            );
        });

        ctx.request_repaint();
    }
}
