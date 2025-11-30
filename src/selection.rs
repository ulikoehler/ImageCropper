use eframe::egui::{self, Color32, Rect, Vec2};

pub const HANDLE_THICKNESS: f32 = 10.0;
pub const MIN_HANDLE_LENGTH: f32 = 20.0;
pub const MAX_HANDLE_LENGTH: f32 = 100.0;

#[derive(Clone)]
pub struct Selection {
    pub rect: Rect,
}

impl Selection {
    pub fn from_points(a: egui::Pos2, b: egui::Pos2, bounds: Vec2) -> Self {
        let min = egui::pos2(
            a.x.min(b.x).clamp(0.0, bounds.x),
            a.y.min(b.y).clamp(0.0, bounds.y),
        );
        let max = egui::pos2(
            a.x.max(b.x).clamp(0.0, bounds.x),
            a.y.max(b.y).clamp(0.0, bounds.y),
        );
        let mut selection = Self {
            rect: Rect::from_min_max(min, max),
        };
        selection.clamp_within(bounds);
        selection
    }

    pub fn translate(&mut self, delta: Vec2, bounds: Vec2) {
        self.rect = self.rect.translate(delta);
        self.clamp_within(bounds);
    }

    pub fn to_u32_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let width = self.rect.width();
        let height = self.rect.height();
        if width < 1.0 || height < 1.0 {
            return None;
        }
        let x = self.rect.min.x.max(0.0).round() as u32;
        let y = self.rect.min.y.max(0.0).round() as u32;
        Some((x, y, width.round() as u32, height.round() as u32))
    }

    pub fn adjusted(mut self, handle: SelectionHandle, delta: Vec2, bounds: Vec2) -> Self {
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

    fn clamp_within(&mut self, bounds: Vec2) {
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
        self.rect = Rect::from_min_max(min, max);
    }
}

#[derive(Clone)]
pub struct HandleDrag {
    pub handle: SelectionHandle,
    pub original: Selection,
    pub start_pos: egui::Pos2,
    pub selection_index: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelectionHandle {
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
    pub const ALL: [Self; 8] = [
        Self::Top, Self::Bottom, Self::Left, Self::Right,
        Self::TopLeft, Self::TopRight, Self::BottomLeft, Self::BottomRight,
    ];

    pub fn id_suffix(self) -> &'static str {
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

    pub fn handle_rect(self, selection: Rect) -> Rect {
        let corner_size = egui::vec2(HANDLE_THICKNESS, HANDLE_THICKNESS);
        match self {
            Self::Top => Rect::from_center_size(
                egui::pos2(selection.center().x, selection.min.y),
                egui::vec2(
                    selection
                        .width()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                    HANDLE_THICKNESS,
                ),
            ),
            Self::Bottom => Rect::from_center_size(
                egui::pos2(selection.center().x, selection.max.y),
                egui::vec2(
                    selection
                        .width()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                    HANDLE_THICKNESS,
                ),
            ),
            Self::Left => Rect::from_center_size(
                egui::pos2(selection.min.x, selection.center().y),
                egui::vec2(
                    HANDLE_THICKNESS,
                    selection
                        .height()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                ),
            ),
            Self::Right => Rect::from_center_size(
                egui::pos2(selection.max.x, selection.center().y),
                egui::vec2(
                    HANDLE_THICKNESS,
                    selection
                        .height()
                        .clamp(MIN_HANDLE_LENGTH, MAX_HANDLE_LENGTH),
                ),
            ),
            Self::TopLeft => Rect::from_center_size(selection.min, corner_size),
            Self::TopRight => Rect::from_center_size(selection.right_top(), corner_size),
            Self::BottomLeft => Rect::from_center_size(selection.left_bottom(), corner_size),
            Self::BottomRight => Rect::from_center_size(selection.max, corner_size),
        }
    }
}

pub fn selection_color(index: usize) -> Color32 {
    let golden_ratio_conjugate = 0.618033988749895;
    let h = (index as f32 * golden_ratio_conjugate) % 1.0;
    let [r, g, b] = egui::ecolor::Hsva::new(h, 0.8, 1.0, 1.0).to_rgb();
    Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}
