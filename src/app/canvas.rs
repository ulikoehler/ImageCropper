use eframe::egui::{self, Color32};

use crate::{
    selection::{selection_color, HandleDrag, Selection, SelectionHandle},
    ui::{ImageMetrics, KeyboardState, ARROW_MOVE_STEP},
};

pub struct Canvas {
    pub selections: Vec<Selection>,
    pub selection_anchor: Option<egui::Pos2>,
    pub active_handle: Option<HandleDrag>,
}

impl Canvas {
    pub fn new() -> Self {
        Self {
            selections: Vec::new(),
            selection_anchor: None,
            active_handle: None,
        }
    }

    pub fn clear(&mut self) {
        self.selections.clear();
        self.selection_anchor = None;
        self.active_handle = None;
    }

    pub fn handle_pointer(
        &mut self,
        response: &egui::Response,
        metrics: &ImageMetrics,
        image_size: egui::Vec2,
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
                    image_size,
                ));
            }
        } else if response.dragged() {
            if let (Some(anchor), Some(pointer)) =
                (self.selection_anchor, response.interact_pointer_pos())
            {
                let image_pos = metrics.screen_to_image(pointer);
                // Update the last selection (the one currently being created)
                if let Some(last) = self.selections.last_mut() {
                    *last = Selection::from_points(anchor, image_pos, image_size);
                }
            }
        } else if response.drag_stopped() {
            self.selection_anchor = None;
        }
    }

    pub fn handle_arrow_movement(&mut self, keys: &KeyboardState, image_size: egui::Vec2) {
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
            selection.translate(delta, image_size);
        }
    }

    pub fn draw(&mut self, ui: &egui::Ui, painter: &egui::Painter, metrics: &ImageMetrics, image_size: egui::Vec2) {
        self.draw_selection(painter, metrics);
        self.draw_handles(ui, painter, metrics, image_size);
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

    fn draw_handles(&mut self, ui: &egui::Ui, painter: &egui::Painter, metrics: &ImageMetrics, image_size: egui::Vec2) {
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
                                        image_size,
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

