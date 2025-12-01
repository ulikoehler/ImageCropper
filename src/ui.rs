use eframe::egui::{self, Pos2, Rect, Vec2};

use crate::selection::Selection;

pub const ARROW_MOVE_STEP: f32 = 2.0;

pub struct ImageMetrics {
    pub image_rect: Rect,
    pub image_size: Vec2,
    pub scale: f32,
}

impl ImageMetrics {
    pub fn new(canvas: Rect, image_size: Vec2) -> Self {
        let (display, scale) = fit_within(image_size, canvas.size());
        let offset = (canvas.size() - display) * 0.5;
        let image_rect = Rect::from_min_size(canvas.min + offset, display);
        Self {
            image_rect,
            image_size,
            scale,
        }
    }

    pub fn screen_to_image(&self, pos: Pos2) -> Pos2 {
        let rel = pos - self.image_rect.min;
        egui::pos2(
            (rel.x / self.scale).clamp(0.0, self.image_size.x),
            (rel.y / self.scale).clamp(0.0, self.image_size.y),
        )
    }

    pub fn selection_rect(&self, selection: &Selection) -> Rect {
        let min = egui::pos2(
            self.image_rect.min.x + selection.rect.min.x * self.scale,
            self.image_rect.min.y + selection.rect.min.y * self.scale,
        );
        let max = egui::pos2(
            self.image_rect.min.x + selection.rect.max.x * self.scale,
            self.image_rect.min.y + selection.rect.max.y * self.scale,
        );
        Rect::from_min_max(min, max)
    }
}

pub fn fit_within(image_size: Vec2, available: Vec2) -> (Vec2, f32) {
    let safe_size = egui::vec2(image_size.x.max(1.0), image_size.y.max(1.0));
    let scale = (available.x / safe_size.x)
        .min(available.y / safe_size.y)
        .max(0.01);
    (safe_size * scale, scale)
}

pub struct KeyboardState {
    pub next_image: bool,
    pub prev_image: bool,
    pub save_selection: bool,
    pub delete: bool,
    pub escape: bool,
    pub move_up: bool,
    pub move_down: bool,
    pub move_left: bool,
    pub move_right: bool,
    pub preview: bool,
    pub rotate_cw: bool,
    pub rotate_ccw: bool,
}

