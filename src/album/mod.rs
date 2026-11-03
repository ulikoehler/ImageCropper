pub mod deleter;
pub mod layout;
pub mod mover;
pub mod thumbnails;

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError},
};

use eframe::{
    egui::{self, Align2, Color32, FontId, Pos2, Rect, ViewportCommand},
    App, Frame,
};

use crate::{
    app::{AppOptions, ImageCropperApp},
    cli::{sort_files, ScanStream, SortOrder},
    fs_utils::{
        format_deletion_summary, format_move_summary, format_signed_size, format_size, ScanEvent,
    },
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
    /// Drag-select behavior: contiguous sequence vs rectangle intersect.
    pub mouse_select: crate::cli::MouseSelectMode,
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
    /// Decoded fullscreen-size preview for hold-`f`.
    full_color: Option<egui::ColorImage>,
    failed: bool,
    /// Byte size from the scan walker (0 when unknown).
    bytes: u64,
}

impl Entry {
    fn new(path: PathBuf, bytes: u64) -> Self {
        Self {
            path,
            aspect: 1.0,
            color: None,
            texture: None,
            full_color: None,
            failed: false,
            bytes,
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
    /// Held `f` — show the focused image fullscreen.
    fullscreen: bool,
}

/// Fullscreen album/tiling view over a set of image files.
pub struct AlbumApp {
    entries: Vec<Entry>,
    positions: HashMap<PathBuf, usize>,
    thumbnailer: Thumbnailer,
    /// Fraction of the panel width the justified grid occupies (w/W
    /// shrinks/grows the viewer on screen; 1.0 = full width).
    view_width: f32,
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
    /// Streaming directory scan (background); `None` once complete.
    scan_rx: Option<Receiver<ScanEvent>>,
    scan_done: bool,
    scan_order: SortOrder,
    scan_inverse: bool,
    scan_errors: usize,
    /// Single-slot fullscreen texture for hold-`f` (path it belongs to).
    fs_tex: Option<egui::TextureHandle>,
    fs_path: Option<PathBuf>,
    /// FIFO of entries holding a decoded fullscreen preview — bounded so
    /// arrowing through many images while holding `f` can't grow memory.
    full_loaded: std::collections::VecDeque<PathBuf>,
    /// Rubber-band drag selection: press origin (screen coords), the
    /// selection snapshot at press, the modifier mode (0 replace, 1 add,
    /// 2 toggle), the sequence-mode anchor tile and whether the drag has
    /// passed the threshold.
    drag_start: Option<Pos2>,
    drag_base: Option<BTreeSet<usize>>,
    drag_mode: u8,
    drag_anchor: Option<usize>,
    drag_active: bool,
    /// Drag-select mode (`sequence` default / `rubber-band`).
    mouse_select: crate::cli::MouseSelectMode,
    /// Screen-space rects of the tiles painted this frame (hit-testing).
    painted: Vec<(usize, Rect)>,
}

impl AlbumApp {
    pub fn new(files: Vec<PathBuf>, options: AlbumOptions, scan: Option<ScanStream>) -> Self {
        let parallel = options.parallel.max(1);
        let (scan_rx, scan_order, scan_inverse) = match scan {
            Some(s) => (Some(s.rx), s.order, s.inverse),
            None => (None, SortOrder::Filename, false),
        };
        Self {
            entries: files.into_iter().map(|p| Entry::new(p, 0)).collect(),
            positions: HashMap::new(),
            thumbnailer: Thumbnailer::new(parallel),
            deleter: Deleter::new(parallel),
            mover: options.move_dir.clone().map(|d| Mover::new(parallel, d)),
            selected: BTreeSet::new(),
            focus: 0,
            anchor: 0,
            base_height: 200.0,
            view_width: 1.0,
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
            scan_done: scan_rx.is_none(),
            scan_rx,
            scan_order,
            scan_inverse,
            scan_errors: 0,
            fs_tex: None,
            fs_path: None,
            full_loaded: std::collections::VecDeque::new(),
            drag_start: None,
            drag_base: None,
            drag_mode: 0,
            drag_anchor: None,
            drag_active: false,
            mouse_select: options.mouse_select,
            painted: Vec::new(),
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

    /// Pull newly discovered paths from the background scan (deduped by
    /// path — overlapping roots or re-scans can't produce double entries).
    /// When the stream ends, the final `-o` ordering is applied once.
    fn drain_scan(&mut self) {
        let mut done = false;
        if let Some(rx) = &self.scan_rx {
            loop {
                match rx.try_recv() {
                    Ok(ScanEvent::Found { path, bytes }) => {
                        if !self.positions.contains_key(&path) {
                            self.positions.insert(path.clone(), self.entries.len());
                            self.entries.push(Entry::new(path, bytes));
                        }
                    }
                    Ok(ScanEvent::Error(e)) => {
                        self.scan_errors += 1;
                        if self.verbose {
                            eprintln!("scan: {e}");
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
        }
        if done {
            self.scan_rx = None;
            self.scan_done = true;
            self.resort_after_scan();
        }
    }

    /// Apply the final sort once scanning completes. Tiles appear in
    /// discovery order during the scan; this reorders them per `-o` while
    /// preserving entry state and remapping selection/focus by path.
    fn resort_after_scan(&mut self) {
        if self.entries.is_empty() {
            self.status = "Scan complete — no images found".into();
            return;
        }
        let sel_paths: BTreeSet<PathBuf> = self
            .selected
            .iter()
            .filter_map(|&i| self.entries.get(i).map(|e| e.path.clone()))
            .collect();
        let focus_path = self.entries.get(self.focus).map(|e| e.path.clone());
        let anchor_path = self.entries.get(self.anchor).map(|e| e.path.clone());

        let mut paths: Vec<PathBuf> = self.entries.iter().map(|e| e.path.clone()).collect();
        sort_files(&mut paths, self.scan_order, self.scan_inverse);

        let mut by_path: HashMap<PathBuf, Entry> = self
            .entries
            .drain(..)
            .map(|e| (e.path.clone(), e))
            .collect();
        self.entries = paths.iter().filter_map(|p| by_path.remove(p)).collect();
        self.rebuild_index();
        self.selected = sel_paths
            .iter()
            .filter_map(|p| self.positions.get(p).copied())
            .collect();
        if let Some(p) = &focus_path {
            self.focus = self.positions.get(p).copied().unwrap_or(0);
        }
        if let Some(p) = &anchor_path {
            self.anchor = self.positions.get(p).copied().unwrap_or(0);
        }
        self.status = format!("Scan complete — {} images", self.entries.len());
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
            fullscreen: input.key_down(egui::Key::F),
        })
    }

    fn drain_results(&mut self) {
        for result in self.thumbnailer.poll_screen() {
            if let Some(&idx) = self.positions.get(&result.path) {
                self.entries[idx].full_color = result.image;
                self.full_loaded.push_back(result.path);
                // Keep at most a handful of decoded fullscreen previews
                // resident (~8 MB each at 1920px).
                while self.full_loaded.len() > 8 {
                    let p = self.full_loaded.pop_front().unwrap();
                    // Keep the focused and the currently displayed
                    // (hold-`f` hover target) previews resident.
                    let keep = self.entries.get(self.focus).map(|e| &e.path) == Some(&p)
                        || self.fs_path.as_ref() == Some(&p);
                    if keep {
                        self.full_loaded.push_back(p);
                        break;
                    }
                    if let Some(&i) = self.positions.get(&p) {
                        self.entries[i].full_color = None;
                    }
                }
            }
        }
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

    /// Painted tile whose center is closest to `p` — the sequence-mode
    /// anchor/cursor when the pointer sits in a gutter.
    fn painted_nearest(&self, p: Pos2) -> Option<usize> {
        self.painted
            .iter()
            .min_by(|(_, a), (_, b)| {
                a.center()
                    .distance_sq(p)
                    .partial_cmp(&b.center().distance_sq(p))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| *i)
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
        let first_gone = targets.iter().min().copied().unwrap_or(0);
        self.entries.retain(|e| !remove.contains(&e.path));
        self.selected.clear();
        self.rebuild_index();
        if self.entries.is_empty() {
            self.focus = 0;
            self.anchor = 0;
        } else {
            // Land on the first surviving tile at the deletion point — or,
            // when the deleted block ran to the end, the last remaining
            // image — and scroll the viewport to it.
            self.focus = first_gone.min(self.entries.len() - 1);
            self.anchor = self.focus;
            self.scroll_to_focus = true;
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
        let first_gone = targets.iter().min().copied().unwrap_or(0);
        self.entries.retain(|e| !remove.contains(&e.path));
        self.selected.clear();
        self.rebuild_index();
        if self.entries.is_empty() {
            self.focus = 0;
            self.anchor = 0;
        } else {
            self.focus = first_gone.min(self.entries.len() - 1);
            self.anchor = self.focus;
            self.scroll_to_focus = true;
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
                let bytes = std::fs::metadata(&new_path).map(|m| m.len()).unwrap_or(0);
                self.entries[idx] = Entry::new(new_path.clone(), bytes);
                // Keep the old aspect as a first guess until the new
                // thumbnail arrives so the layout doesn't jump.
                self.entries[idx].aspect = aspect;
                self.rebuild_index();
            }
            _ => {}
        }
        self.status = format!("Returned from cropping {}", origin.display());
    }

    /// Enter the exit path (Esc / Ctrl-C): stop all preview generation
    /// immediately; queued deletes/moves keep draining and the window
    /// closes once they finish — destructive ops win over previews.
    fn begin_exit(&mut self) {
        self.thumbnailer.cancel();
        self.is_exiting = true;
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
        self.drain_scan();
        self.drain_results();

        // Delete/move worker counts track the preview pool's autotuned
        // concurrency — same storage, same optimal parallelism.
        let preview_workers = self.thumbnailer.workers();
        self.deleter.set_workers(preview_workers);
        if let Some(m) = &self.mover {
            m.set_workers(preview_workers);
        }
        // Keep polling so a Ctrl-C is noticed even on an idle grid.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        if crate::fs_utils::sigint_received() && !self.is_exiting {
            self.begin_exit();
        }

        let pending_moves = self.mover.as_ref().map(|m| m.pending.len()).unwrap_or(0);
        let pending_total = self.deleter.pending.len() + pending_moves;
        if self.is_exiting {
            if pending_total == 0 {
                if !self.summary_printed {
                    println!(
                        "{}",
                        format_deletion_summary(self.deleted_files, self.total_deleted_bytes)
                    );
                    if self.moved_files > 0 {
                        println!(
                            "{}",
                            format_move_summary(self.moved_files, self.total_moved_bytes)
                        );
                    }
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
            // `w`/`W` restrict the viewer: the grid lays out inside
            // `width * view_width` and renders centered with margins.
            let layout_width = width * self.view_width;
            let x_off = (width - layout_width) / 2.0;
            let row_height = (self.base_height * self.zoom).clamp(24.0, 2048.0);
            let aspects: Vec<f32> = self.entries.iter().map(|e| e.aspect).collect();
            let layout = layout_justified(&aspects, layout_width, row_height, TILE_GAP);

            let keys = Self::read_keys(&ctx);
            let modifiers = ctx.input(|i| i.modifiers);

            if keys.escape {
                if !self.selected.is_empty() {
                    self.selected.clear();
                    self.status = "Selection cleared".into();
                } else {
                    self.begin_exit();
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
                self.view_width = (self.view_width * WIDTH_STEP).clamp(0.05, 1.0);
            }
            if keys.width_decrease {
                self.view_width = (self.view_width / WIDTH_STEP).clamp(0.05, 1.0);
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

            // Rubber-band drag selection. Pointer is sampled before the
            // tiles run so `drag_active` also suppresses the stray click
            // egui reports on the release frame.
            let pointer = ctx.input(|i| i.pointer.clone());

            // Hold-`f`: fullscreen loupe. It follows the tile under the
            // mouse; when the pointer isn't over a tile the focus (the
            // last used image) is shown instead.
            let show_fullscreen = keys.fullscreen && !self.entries.is_empty();
            let fs_target = if show_fullscreen {
                pointer
                    .latest_pos()
                    .and_then(|p| {
                        self.painted
                            .iter()
                            .find(|(_, r)| r.contains(p))
                            .map(|(i, _)| *i)
                    })
                    .unwrap_or(self.focus)
                    .min(self.entries.len() - 1)
            } else {
                0
            };
            if show_fullscreen {
                if let Some(entry) = self.entries.get_mut(fs_target) {
                    if entry.full_color.is_none() {
                        self.thumbnailer.request_screen(entry.path.clone());
                    }
                }
            }
            if pointer.primary_pressed() {
                // Only start a band inside the content area, not on the
                // scrollbar strip.
                let inside = pointer
                    .press_origin()
                    .map(|p| p.x < panel_rect.right() - 14.0 && panel_rect.contains(p))
                    .unwrap_or(false);
                if inside {
                    let origin = pointer.press_origin().unwrap();
                    self.drag_start = Some(origin);
                    // Sequence mode anchors at the tile the press landed on
                    // (nearest center when pressing into a gap).
                    self.drag_anchor = self
                        .painted
                        .iter()
                        .find(|(_, r)| r.contains(origin))
                        .map(|(i, _)| *i)
                        .or_else(|| self.painted_nearest(origin));
                    self.drag_base = Some(self.selected.clone());
                    self.drag_mode = if modifiers.command {
                        2
                    } else if modifiers.shift {
                        1
                    } else {
                        0
                    };
                }
            }
            let mut drag_rect: Option<Rect> = None;
            if pointer.primary_down() {
                if let (Some(start), Some(cur)) = (self.drag_start, pointer.latest_pos()) {
                    if self.drag_active || start.distance(cur) > 6.0 {
                        self.drag_active = true;
                        drag_rect = Some(Rect::from_two_pos(start, cur));
                    }
                }
            }

            self.painted.clear();
            let mut new_visible = 0..0;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let (response, painter) = ui.allocate_painter(
                        egui::vec2(width.max(layout.total_size.x), layout.total_size.y),
                        egui::Sense::hover(),
                    );
                    let origin = response.rect.min;
                    let clip = ui.clip_rect().translate(-origin.to_vec2());
                    // Tiles shift right by x_off so a narrowed viewer
                    // renders centered inside the panel.
                    let off = origin.to_vec2() + egui::vec2(x_off, 0.0);

                    new_visible = Self::visible_range(&layout, clip);
                    self.last_visible = new_visible.clone();

                    if self.scroll_to_focus {
                        if let Some(tile) = layout.tiles.get(self.focus) {
                            ui.scroll_to_rect(
                                tile.translate(off),
                                Some(egui::Align::Center),
                            );
                        }
                        self.scroll_to_focus = false;
                    }

                    for i in new_visible.clone() {
                        let tile = layout.tiles[i];
                        let rect = tile.translate(off);
                        self.painted.push((i, rect));

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
                            ui.scope_id().with("tile").with(i),
                            egui::Sense::click_and_drag(),
                        );
                        if tile_response.clicked() && !self.drag_active {
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

            // Apply the drag selection live. Rubber-band mode hits every
            // painted tile the rect intersects; sequence mode selects the
            // contiguous index range between anchor and cursor tile.
            if let Some(rect) = drag_rect {
                // Cursor tile under the drag point (or its nearest neighbor
                // in a gutter) — also drives the focus.
                let cur = self
                    .painted
                    .iter()
                    .find(|(_, r)| r.contains(rect.max))
                    .map(|(i, _)| *i)
                    .or_else(|| self.painted_nearest(rect.max));
                let hits: BTreeSet<usize> = match self.mouse_select {
                    crate::cli::MouseSelectMode::Sequence => {
                        match (self.drag_anchor, cur) {
                            (Some(a), Some(c)) => (a.min(c)..=a.max(c)).collect(),
                            _ => Default::default(),
                        }
                    }
                    crate::cli::MouseSelectMode::RubberBand => self
                        .painted
                        .iter()
                        .filter(|(_, r)| r.intersects(rect))
                        .map(|(i, _)| *i)
                        .collect(),
                };
                let base = self.drag_base.clone().unwrap_or_default();
                self.selected = match self.drag_mode {
                    1 => base.union(&hits).copied().collect(),
                    2 => base.symmetric_difference(&hits).copied().collect(),
                    _ => hits,
                };
                if let Some(i) = cur {
                    self.focus = i;
                }
                let painter = ui.painter().clone();
                painter.rect_filled(
                    rect,
                    0.0,
                    Color32::from_rgba_unmultiplied(78, 161, 255, 38),
                );
                painter.rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(1.5, Color32::from_rgb(78, 161, 255)),
                    egui::StrokeKind::Outside,
                );
            }
            if !pointer.primary_down() {
                self.drag_start = None;
                self.drag_base = None;
                self.drag_anchor = None;
                self.drag_active = false;
            }

            let painter = ui.painter().clone();

            if show_fullscreen {
                // Upload the decoded fullscreen preview once, keep the
                // texture for as long as the same file is targeted.
                if let Some(color) = self
                    .entries
                    .get_mut(fs_target)
                    .and_then(|e| e.full_color.take())
                {
                    self.fs_tex = Some(ctx.load_texture(
                        "album-fullscreen",
                        color,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.fs_path = self.entries.get(fs_target).map(|e| e.path.clone());
                }
                painter.rect_filled(panel_rect, 0.0, Color32::BLACK);
                let ready = self
                    .entries
                    .get(fs_target)
                    .map(|e| self.fs_path.as_ref() == Some(&e.path) && self.fs_tex.is_some())
                    .unwrap_or(false);
                if ready {
                    let tex = self.fs_tex.as_ref().unwrap();
                    let [w, h] = tex.size();
                    let scale = (panel_rect.width() / w as f32)
                        .min(panel_rect.height() / h as f32);
                    let rect = Rect::from_center_size(
                        panel_rect.center(),
                        egui::vec2(w as f32 * scale, h as f32 * scale),
                    );
                    painter.image(
                        tex.id(),
                        rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    painter.text(
                        panel_rect.center(),
                        Align2::CENTER_CENTER,
                        "loading fullscreen preview…",
                        FontId::proportional(22.0),
                        Color32::from_gray(140),
                    );
                }
            } else if self.entries.is_empty() {
                painter.text(
                    panel_rect.center(),
                    Align2::CENTER_CENTER,
                    if self.scan_done {
                        "No images found"
                    } else {
                        "Scanning for images…"
                    },
                    FontId::proportional(24.0),
                    Color32::from_gray(160),
                );
            }

            // Overlay: counts (top-left), status (bottom-left), help (bottom-right)
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
                "Click: Select | Ctrl+Click: Toggle | Shift+Click: Range | Space: Toggle | Arrows: Move | Shift+Arrows: Extend | Del: Trash | m: Move | f(hold): Fullscreen | c/2xClick: Crop | +/-: Zoom | w/W: Viewer width | Esc: Clear/Quit".to_string(),
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
        if !self.scan_done {
            line += " — scanning…";
        }
        if self.scan_errors > 0 {
            line += &format!(" — {} scan error(s)", self.scan_errors);
        }
        if !self.selected.is_empty() {
            let bytes: u64 = self
                .selected
                .iter()
                .filter_map(|&i| self.entries.get(i))
                .map(|e| e.bytes)
                .sum();
            line += &format!(
                " — {} selected ({})",
                self.selected.len(),
                format_size(bytes)
            );
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
    pub fn new(files: Vec<PathBuf>, options: AlbumOptions, scan: Option<ScanStream>) -> Self {
        Self {
            album: AlbumApp::new(files, options, scan),
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
                None,
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
