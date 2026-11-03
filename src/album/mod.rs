pub mod deleter;
pub mod layout;
pub mod mover;
pub mod thumbnails;

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::{Path, PathBuf},
};

use eframe::{
    egui::{self, Align2, Color32, FontId, Pos2, Rect, ViewportCommand},
    App, Frame,
};

use crate::{
    app::{AppOptions, ImageCropperApp},
    fs_utils::{format_deletion_summary, format_signed_size, format_size},
};

use self::{
    deleter::Deleter,
    layout::{layout_justified, JustifiedLayout},
    mover::Mover,
    thumbnails::Thumbnailer,
};

/// Gap between tiles and rows in the justified layout (points).
const TILE_GAP: f32 = 4.0;
/// Maximum number of GPU textures kept for previews; off-screen previews are
/// evicted beyond this and reloaded from the on-disk cache when scrolled back.
const MAX_TEXTURES: usize = 1500;
/// How many thumbnail jobs may be in flight at once per worker thread.
const IN_FLIGHT_PER_THREAD: usize = 8;

const ZOOM_STEP: f32 = 1.1;
const WIDTH_STEP: f32 = 1.25;

/// Options for the album view itself plus the embedded crop session.
pub struct AlbumOptions {
    pub dry_run: bool,
    pub verbose: bool,
    pub parallel: usize,
    /// Target dir for `m` moves; `None` disables the move key.
    pub move_dir: Option<PathBuf>,
    /// Options used when launching the embedded crop UI via `c`.
    pub crop: AppOptions,
}

struct Entry {
    path: PathBuf,
    /// width / height; 1.0 until the thumbnail arrives.
    aspect: f32,
    /// Decoded preview waiting to be uploaded to the GPU.
    color: Option<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
    failed: bool,
}

impl Entry {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            aspect: 1.0,
            color: None,
            texture: None,
            failed: false,
        }
    }
}

struct AlbumKeys {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    home: bool,
    end: bool,
    toggle: bool,
    select_all: bool,
    delete: bool,
    move_sel: bool,
    crop: bool,
    escape: bool,
    zoom_in: bool,
    zoom_out: bool,
    width_increase: bool,
    width_decrease: bool,
}

/// Fullscreen album/tiling view over a set of image files.
pub struct AlbumApp {
    entries: Vec<Entry>,
    positions: HashMap<PathBuf, usize>,
    thumbnailer: Thumbnailer,
    deleter: Deleter,
    mover: Option<Mover>,
    selected: BTreeSet<usize>,
    focus: usize,
    anchor: usize,
    base_height: f32,
    zoom: f32,
    pub status: String,
    dry_run: bool,
    verbose: bool,
    parallel: usize,
    pub crop_options: AppOptions,
    crop_request: Option<PathBuf>,
    is_exiting: bool,
    windowed_mode_set: bool,
    deleted_files: usize,
    moved_files: usize,
    total_deleted_bytes: u64,
    total_moved_bytes: u64,
    summary_printed: bool,
    scroll_to_focus: bool,
    enqueue_cursor: usize,
    last_visible: Range<usize>,
}

impl AlbumApp {
    pub fn new(files: Vec<PathBuf>, options: AlbumOptions) -> Self {
        let parallel = options.parallel.max(1);
        Self {
            entries: files.into_iter().map(Entry::new).collect(),
            positions: HashMap::new(),
            thumbnailer: Thumbnailer::new(parallel),
            deleter: Deleter::new(parallel),
            mover: options.move_dir.clone().map(|d| Mover::new(parallel, d)),
            selected: BTreeSet::new(),
            focus: 0,
            anchor: 0,
            base_height: 200.0,
            zoom: 1.0,
            status: "Ready".into(),
            dry_run: options.dry_run,
            verbose: options.verbose,
            parallel,
            crop_options: options.crop,
            crop_request: None,
            is_exiting: false,
            windowed_mode_set: false,
            deleted_files: 0,
            moved_files: 0,
            total_deleted_bytes: 0,
            total_moved_bytes: 0,
            summary_printed: false,
            scroll_to_focus: false,
            enqueue_cursor: 0,
            last_visible: 0..0,
        }
        .reindex()
    }

    fn reindex(mut self) -> Self {
        self.rebuild_index();
        self
    }

    fn rebuild_index(&mut self) {
        self.positions = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.path.clone(), i))
            .collect();
    }

    fn read_keys(ctx: &egui::Context) -> AlbumKeys {
        ctx.input(|input| AlbumKeys {
            left: input.key_pressed(egui::Key::ArrowLeft),
            right: input.key_pressed(egui::Key::ArrowRight),
            up: input.key_pressed(egui::Key::ArrowUp),
            down: input.key_pressed(egui::Key::ArrowDown),
            home: input.key_pressed(egui::Key::Home),
            end: input.key_pressed(egui::Key::End),
            toggle: input.key_pressed(egui::Key::Space),
            select_all: input.key_pressed(egui::Key::A) && input.modifiers.command,
            delete: input.key_pressed(egui::Key::Delete),
            move_sel: input.key_pressed(egui::Key::M) && !input.modifiers.command,
            crop: input.key_pressed(egui::Key::C) && !input.modifiers.command,
            escape: input.key_pressed(egui::Key::Escape),
            zoom_in: input.key_pressed(egui::Key::Plus) || input.key_pressed(egui::Key::Equals),
            zoom_out: input.key_pressed(egui::Key::Minus),
            width_increase: input.key_pressed(egui::Key::W) && input.modifiers.shift,
            width_decrease: input.key_pressed(egui::Key::W) && !input.modifiers.shift,
        })
    }

    fn drain_results(&mut self) {
        for result in self.thumbnailer.poll() {
            let Some(&idx) = self.positions.get(&result.path) else {
                continue; // File was removed from the album meanwhile.
            };
            let entry = &mut self.entries[idx];
            match result.image {
                Some(image) => {
                    let [w, h] = image.size;
                    if h > 0 {
                        entry.aspect = w as f32 / h as f32;
                    }
                    entry.color = Some(image);
                }
                None => entry.failed = true,
            }
        }

        for result in self.deleter.poll() {
            match result.result {
                Ok(bytes) => {
                    self.deleted_files += 1;
                    self.total_deleted_bytes += bytes;
                    if self.verbose {
                        println!("Deleted {} ({})", result.path.display(), format_size(bytes));
                    }
                }
                Err(err) => {
                    let msg = format!("Failed to delete {}: {err:#}", result.path.display());
                    eprintln!("{}", msg);
                    self.status = msg;
                }
            }
        }

        if let Some(mover) = self.mover.as_mut() {
            for result in mover.poll() {
                match result.result {
                    Ok(bytes) => {
                        self.moved_files += 1;
                        self.total_moved_bytes += bytes;
                        if self.verbose {
                            println!("Moved {} ({})", result.path.display(), format_size(bytes));
                        }
                    }
                    Err(err) => {
                        let msg = format!("Failed to move {}: {err:#}", result.path.display());
                        eprintln!("{}", msg);
                        self.status = msg;
                    }
                }
            }
        }
    }

    /// Queue thumbnails for `priority` indices first (the visible tiles),
    /// then keep filling the worker queue sequentially so every image gets
    /// preloaded.
    fn enqueue_previews(&mut self, priority: Range<usize>) {
        for i in priority {
            self.request_index(i);
        }
        let n = self.entries.len();
        let cap = self.parallel * IN_FLIGHT_PER_THREAD;
        let mut steps = 0;
        while steps < n && self.thumbnailer.pending.len() < cap {
            if self.enqueue_cursor >= n {
                self.enqueue_cursor = 0;
            }
            let idx = self.enqueue_cursor;
            self.enqueue_cursor += 1;
            self.request_index(idx);
            steps += 1;
        }
    }

    fn request_index(&mut self, i: usize) {
        let Some(entry) = self.entries.get(i) else {
            return;
        };
        if entry.color.is_some() || entry.texture.is_some() || entry.failed {
            return;
        }
        self.thumbnailer.request(entry.path.clone());
    }

    fn select_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        self.selected = (lo..=hi).collect();
    }

    fn move_focus(&mut self, new_focus: usize, extend: bool, keep_selection: bool) {
        if self.entries.is_empty() {
            return;
        }
        self.focus = new_focus.min(self.entries.len() - 1);
        self.scroll_to_focus = true;
        if extend {
            self.select_range(self.anchor, self.focus);
        } else if !keep_selection {
            self.selected.clear();
            self.selected.insert(self.focus);
            self.anchor = self.focus;
        }
    }

    /// Find the tile index in the row above/below `focus` closest to its
    /// horizontal center (justified rows have variable item counts).
    fn vertical_neighbor(&self, layout: &JustifiedLayout, up: bool) -> Option<usize> {
        let row_idx = layout.row_of(self.focus)?;
        let target_row = if up {
            row_idx.checked_sub(1)?
        } else {
            if row_idx + 1 >= layout.rows.len() {
                return None;
            }
            row_idx + 1
        };
        let center_x = layout.tiles.get(self.focus)?.center().x;
        let row = layout.rows[target_row];
        (row.start..row.end).min_by(|&a, &b| {
            let da = (layout.tiles[a].center().x - center_x).abs();
            let db = (layout.tiles[b].center().x - center_x).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    fn delete_targets(&self) -> Vec<usize> {
        if self.selected.is_empty() {
            if self.focus < self.entries.len() {
                vec![self.focus]
            } else {
                Vec::new()
            }
        } else {
            self.selected.iter().copied().collect()
        }
    }

    fn delete_selected(&mut self) {
        let targets = self.delete_targets();
        if targets.is_empty() {
            self.status = "Nothing selected".into();
            return;
        }
        let paths: Vec<PathBuf> = targets
            .iter()
            .map(|&i| self.entries[i].path.clone())
            .collect();

        if self.dry_run {
            for path in &paths {
                println!("Dry run: would move {} to trash", path.display());
            }
            self.status = format!("Dry run: skipped deleting {} image(s)", paths.len());
        } else {
            for path in &paths {
                self.deleter.queue(path.clone());
            }
            self.status = format!("Queued {} image(s) for deletion", paths.len());
        }

        let remove: std::collections::HashSet<&PathBuf> = paths.iter().collect();
        self.entries.retain(|e| !remove.contains(&e.path));
        self.selected.clear();
        self.rebuild_index();
        if self.entries.is_empty() {
            self.focus = 0;
            self.anchor = 0;
        } else {
            self.focus = self.focus.min(self.entries.len() - 1);
            self.anchor = self.focus;
        }
    }

    fn move_selected(&mut self) {
        if self.mover.is_none() {
            self.status = "No -m/--move directory configured".into();
            return;
        }
        let targets = self.delete_targets();
        let mover = self.mover.as_mut().unwrap();
        if targets.is_empty() {
            self.status = "Nothing selected".into();
            return;
        }
        let paths: Vec<PathBuf> = targets
            .iter()
            .map(|&i| self.entries[i].path.clone())
            .collect();

        if self.dry_run {
            for path in &paths {
                println!("Dry run: would move {}", path.display());
            }
            self.status = format!("Dry run: skipped moving {} image(s)", paths.len());
        } else {
            for path in &paths {
                mover.queue(path.clone());
            }
            self.status = format!("Queued {} image(s) for moving", paths.len());
        }

        let remove: std::collections::HashSet<&PathBuf> = paths.iter().collect();
        self.entries.retain(|e| !remove.contains(&e.path));
        self.selected.clear();
        self.rebuild_index();
        if self.entries.is_empty() {
            self.focus = 0;
            self.anchor = 0;
        } else {
            self.focus = self.focus.min(self.entries.len() - 1);
            self.anchor = self.focus;
        }
    }

    pub fn take_crop_request(&mut self) -> Option<PathBuf> {
        self.crop_request.take()
    }

    /// Re-integrate the results of an embedded crop session. `resulting` is
    /// the crop app's file list (the origin may have been renamed to the
    /// output format, or removed entirely if deleted).
    pub fn sync_after_crop(&mut self, origin: &Path, resulting: Vec<PathBuf>) {
        let Some(&idx) = self.positions.get(origin) else {
            return;
        };
        match resulting.first() {
            None => {
                self.entries.remove(idx);
                self.selected.clear();
                self.rebuild_index();
                if !self.entries.is_empty() {
                    self.focus = self.focus.min(self.entries.len() - 1);
                    self.anchor = self.focus;
                } else {
                    self.focus = 0;
                    self.anchor = 0;
                }
            }
            Some(new_path) if new_path != origin => {
                let aspect = self.entries[idx].aspect;
                self.entries[idx] = Entry::new(new_path.clone());
                // Keep the old aspect as a first guess until the new
                // thumbnail arrives so the layout doesn't jump.
                self.entries[idx].aspect = aspect;
                self.rebuild_index();
            }
            _ => {}
        }
        self.status = format!("Returned from cropping {}", origin.display());
    }

    /// Indices of tiles intersecting `clip` — the visible range is contiguous
    /// because the layout fills rows left-to-right, top-to-bottom.
    fn visible_range(layout: &JustifiedLayout, clip: Rect) -> Range<usize> {
        let mut start = layout.tiles.len();
        let mut end = 0usize;
        for (i, tile) in layout.tiles.iter().enumerate() {
            if tile.intersects(clip) {
                start = start.min(i);
                end = i + 1;
            }
        }
        if end < start {
            0..0
        } else {
            start..end
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        let ctx = ui.ctx().clone();
        self.drain_results();

        let pending_moves = self.mover.as_ref().map(|m| m.pending.len()).unwrap_or(0);
        let pending_total = self.deleter.pending.len() + pending_moves;
        if self.is_exiting {
            if pending_total == 0 {
                if !self.summary_printed {
                    println!("{}", format_deletion_summary(self.total_deleted_bytes));
                    self.summary_printed = true;
                }
                ctx.send_viewport_cmd(ViewportCommand::Close);
                return;
            }
            if !self.windowed_mode_set {
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(400.0, 200.0)));
                self.windowed_mode_set = true;
            }
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.heading(format!(
                        "Finishing deletions/moves... ({} remaining)",
                        pending_total
                    ));
                });
            });
            ctx.request_repaint();
            return;
        }

        egui::CentralPanel::default().show(ui, |ui| {
            let panel_rect = ui.max_rect();
            let width = ui.available_width();
            let row_height = (self.base_height * self.zoom).clamp(24.0, 2048.0);
            let aspects: Vec<f32> = self.entries.iter().map(|e| e.aspect).collect();
            let layout = layout_justified(&aspects, width, row_height, TILE_GAP);

            let keys = Self::read_keys(&ctx);
            let modifiers = ctx.input(|i| i.modifiers);

            if keys.escape {
                if !self.selected.is_empty() {
                    self.selected.clear();
                    self.status = "Selection cleared".into();
                } else {
                    self.is_exiting = true;
                    return;
                }
            }

            if keys.zoom_in {
                self.zoom = (self.zoom * ZOOM_STEP).clamp(0.1, 12.0);
            }
            if keys.zoom_out {
                self.zoom = (self.zoom / ZOOM_STEP).clamp(0.1, 12.0);
            }
            if keys.width_increase {
                self.base_height = (self.base_height * WIDTH_STEP).clamp(32.0, 1024.0);
            }
            if keys.width_decrease {
                self.base_height = (self.base_height / WIDTH_STEP).clamp(32.0, 1024.0);
            }

            if keys.select_all {
                self.selected = (0..self.entries.len()).collect();
                self.status = format!("Selected all {} images", self.entries.len());
            }

            let extend = modifiers.shift;
            let keep = modifiers.command;
            if keys.left && self.focus > 0 {
                self.move_focus(self.focus - 1, extend, keep);
            }
            if keys.right && self.focus + 1 < self.entries.len() {
                self.move_focus(self.focus + 1, extend, keep);
            }
            if keys.up {
                if let Some(i) = self.vertical_neighbor(&layout, true) {
                    self.move_focus(i, extend, keep);
                }
            }
            if keys.down {
                if let Some(i) = self.vertical_neighbor(&layout, false) {
                    self.move_focus(i, extend, keep);
                }
            }
            if keys.home {
                self.move_focus(0, extend, keep);
            }
            if keys.end && !self.entries.is_empty() {
                self.move_focus(self.entries.len() - 1, extend, keep);
            }
            if keys.toggle && !self.entries.is_empty() {
                if self.selected.contains(&self.focus) {
                    self.selected.remove(&self.focus);
                } else {
                    self.selected.insert(self.focus);
                }
                self.anchor = self.focus;
            }

            if keys.delete {
                self.delete_selected();
            }
            if keys.move_sel {
                self.move_selected();
            }

            if keys.crop {
                if let Some(entry) = self.entries.get(self.focus) {
                    self.crop_request = Some(entry.path.clone());
                }
            }

            let mut new_visible = 0..0;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let (response, painter) =
                        ui.allocate_painter(layout.total_size, egui::Sense::hover());
                    let origin = response.rect.min;
                    let clip = ui.clip_rect().translate(-origin.to_vec2());

                    new_visible = Self::visible_range(&layout, clip);
                    self.last_visible = new_visible.clone();

                    if self.scroll_to_focus {
                        if let Some(tile) = layout.tiles.get(self.focus) {
                            ui.scroll_to_rect(
                                tile.translate(origin.to_vec2()),
                                Some(egui::Align::Center),
                            );
                        }
                        self.scroll_to_focus = false;
                    }

                    for i in new_visible.clone() {
                        let tile = layout.tiles[i];
                        let rect = tile.translate(origin.to_vec2());

                        let texture_id = {
                            let entry = &mut self.entries[i];
                            if entry.texture.is_none() {
                                if let Some(color) = entry.color.take() {
                                    entry.texture = Some(ctx.load_texture(
                                        format!("thumb-{}", i),
                                        color,
                                        egui::TextureOptions::LINEAR,
                                    ));
                                }
                            }
                            entry.texture.as_ref().map(|t| t.id())
                        };
                        let failed = self.entries[i].failed;

                        painter.rect_filled(rect, 0.0, Color32::from_gray(28));
                        if let Some(texture_id) = texture_id {
                            painter.image(
                                texture_id,
                                rect,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        } else if failed {
                            painter.rect_filled(rect, 0.0, Color32::from_rgb(60, 20, 20));
                            if rect.height() > 40.0 {
                                painter.text(
                                    rect.center(),
                                    Align2::CENTER_CENTER,
                                    "failed",
                                    FontId::proportional(14.0),
                                    Color32::from_rgb(220, 120, 120),
                                );
                            }
                        } else if rect.height() > 40.0 {
                            painter.text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                "…",
                                FontId::proportional(18.0),
                                Color32::from_gray(120),
                            );
                        }

                        if self.selected.contains(&i) {
                            painter.rect_stroke(
                                rect.shrink(2.0),
                                0.0,
                                egui::Stroke::new(4.0, Color32::YELLOW),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if i == self.focus {
                            painter.rect_stroke(
                                rect.shrink(0.5),
                                0.0,
                                egui::Stroke::new(1.5, Color32::WHITE),
                                egui::StrokeKind::Inside,
                            );
                        }

                        let tile_response = ui.interact(
                            rect,
                            ui.id().with("tile").with(i),
                            egui::Sense::click_and_drag(),
                        );
                        if tile_response.clicked() {
                            if modifiers.shift {
                                self.select_range(self.anchor, i);
                            } else if modifiers.command {
                                if self.selected.contains(&i) {
                                    self.selected.remove(&i);
                                } else {
                                    self.selected.insert(i);
                                }
                                self.anchor = i;
                            } else {
                                self.selected.clear();
                                self.selected.insert(i);
                                self.anchor = i;
                            }
                            self.focus = i;
                        }
                        if tile_response.double_clicked() {
                            self.crop_request =
                                self.entries.get(i).map(|e| e.path.clone());
                        }
                    }

                    // Evict off-screen textures once the budget is exceeded;
                    // they reload from the on-disk cache when scrolled back.
                    let texture_count =
                        self.entries.iter().filter(|e| e.texture.is_some()).count();
                    if texture_count > MAX_TEXTURES {
                        for (i, entry) in self.entries.iter_mut().enumerate() {
                            if !new_visible.contains(&i) {
                                entry.texture = None;
                            }
                        }
                    }
                });

            // Overlay: counts (top-left), status (bottom-left), help (bottom-right)
            let painter = ui.painter().clone();
            draw_text_with_bg(
                &painter,
                &ctx,
                panel_rect.left_top() + egui::vec2(12.0, 12.0),
                Align2::LEFT_TOP,
                self.top_line(),
                FontId::proportional(20.0),
                Color32::WHITE,
            );
            draw_text_with_bg(
                &painter,
                &ctx,
                panel_rect.left_bottom() + egui::vec2(12.0, -12.0),
                Align2::LEFT_BOTTOM,
                self.status.clone(),
                FontId::monospace(16.0),
                Color32::WHITE,
            );
            draw_text_with_bg(
                &painter,
                &ctx,
                panel_rect.right_bottom() + egui::vec2(-12.0, -12.0),
                Align2::RIGHT_BOTTOM,
                "Click: Select | Ctrl+Click: Toggle | Shift+Click: Range | Space: Toggle | Arrows: Move | Shift+Arrows: Extend | Del: Trash | m: Move | c/2xClick: Crop | +/-: Zoom | w/W: Width | Esc: Clear/Quit".to_string(),
                FontId::monospace(16.0),
                Color32::from_gray(200),
            );
        });

        // Priority: whatever is visible now, then everything else.
        self.enqueue_previews(self.last_visible.clone());
        ctx.request_repaint();
    }

    fn top_line(&self) -> String {
        let total = self.entries.len();
        let loaded = self
            .entries
            .iter()
            .filter(|e| e.texture.is_some() || e.color.is_some() || e.failed)
            .count();
        let mut line = format!("{total} images — previews {loaded}/{total}");
        if !self.selected.is_empty() {
            line += &format!(" — {} selected", self.selected.len());
        }
        if !self.deleter.pending.is_empty() {
            line += &format!(" — deleting {}", self.deleter.pending.len());
        }
        if let Some(mover) = &self.mover {
            if !mover.pending.is_empty() {
                line += &format!(" — moving {}", mover.pending.len());
            }
        }
        if self.total_deleted_bytes > 0 {
            line += &format!(
                " — deleted {}",
                format_signed_size(self.total_deleted_bytes as i64)
            );
        }
        if self.total_moved_bytes > 0 {
            line += &format!(
                " — moved {}",
                format_signed_size(self.total_moved_bytes as i64)
            );
        }
        line
    }
}

fn draw_text_with_bg(
    painter: &egui::Painter,
    ctx: &egui::Context,
    pos: Pos2,
    align: Align2,
    text: String,
    font: FontId,
    color: Color32,
) {
    let galley = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(text, font, color));
    let rect = align.anchor_size(pos, galley.size());
    painter.rect_filled(rect.expand(4.0), 4.0, Color32::from_black_alpha(178));
    painter.galley(rect.min, galley, Color32::WHITE);
}

/// Root app for `imagecropper album`: the album grid plus an embedded
/// single-image crop session toggled with `c` / `Escape`.
pub struct AlbumCropApp {
    album: AlbumApp,
    crop: Option<ImageCropperApp>,
    crop_origin: Option<PathBuf>,
}

impl AlbumCropApp {
    pub fn new(files: Vec<PathBuf>, options: AlbumOptions) -> Self {
        Self {
            album: AlbumApp::new(files, options),
            crop: None,
            crop_origin: None,
        }
    }
}

impl App for AlbumCropApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame) {
        if let Some(crop) = self.crop.as_mut() {
            <ImageCropperApp as App>::ui(crop, ui, frame);
            if crop.exit_to_album || crop.list_completed {
                let mut crop = self.crop.take().unwrap();
                let origin = self.crop_origin.take().unwrap();
                let resulting = std::mem::take(&mut crop.files);
                self.album.sync_after_crop(&origin, resulting);
                // The crop UI may have shrunk the window while waiting for
                // background saves — restore fullscreen for the album grid.
                if crop.windowed_mode_set {
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::Fullscreen(true));
                }
            }
            return;
        }

        self.album.update(ui, frame);

        if let Some(path) = self.album.take_crop_request() {
            let ctx = ui.ctx().clone();
            let options = self.album.crop_options.clone();
            match ImageCropperApp::create(
                &ctx,
                frame.wgpu_render_state(),
                vec![path.clone()],
                options,
            ) {
                Ok(app) => {
                    self.crop = Some(app);
                    self.crop_origin = Some(path);
                }
                Err(err) => {
                    self.album.status = format!("Failed to open crop mode: {err:#}");
                }
            }
        }
    }
}
