use imagecropper::selection::*;
use eframe::egui::{self, Rect, Vec2};

#[test]
fn from_points_clamps_to_bounds() {
    let bounds = Vec2::new(100.0, 80.0);
    let selection = Selection::from_points(
        egui::pos2(-10.0, -20.0),
        egui::pos2(120.0, 90.0),
        bounds,
    );
    assert_eq!(selection.rect.min, egui::pos2(0.0, 0.0));
    assert_eq!(selection.rect.max, egui::pos2(100.0, 80.0));
}

#[test]
fn translate_respects_bounds() {
    let bounds = Vec2::new(50.0, 50.0);
    let mut selection = Selection::from_points(
        egui::pos2(10.0, 10.0),
        egui::pos2(20.0, 20.0),
        bounds,
    );
    selection.translate(egui::vec2(100.0, -15.0), bounds);
    assert_eq!(selection.rect.max.x, 50.0);
    assert_eq!(selection.rect.min.y, 0.0);
}

#[test]
fn to_u32_bounds_filters_tiny_selections() {
    let mut selection = Selection::from_points(
        egui::pos2(1.0, 1.0),
        egui::pos2(2.0, 2.0),
        Vec2::new(10.0, 10.0),
    );
    assert_eq!(selection.to_u32_bounds(), Some((1, 1, 1, 1)));
    selection.rect = Rect::from_min_max(egui::pos2(1.0, 1.0), egui::pos2(1.5, 1.5));
    assert_eq!(selection.to_u32_bounds(), None);
}

#[test]
fn adjusted_updates_handles_correctly() {
    let bounds = Vec2::new(100.0, 100.0);
    let selection = Selection::from_points(
        egui::pos2(10.0, 10.0),
        egui::pos2(30.0, 30.0),
        bounds,
    );
    let adjusted = selection
        .clone()
        .adjusted(SelectionHandle::TopLeft, egui::vec2(-5.0, -5.0), bounds);
    assert_eq!(adjusted.rect.min, egui::pos2(5.0, 5.0));
    assert_eq!(adjusted.rect.max, selection.rect.max);
}

#[test]
fn selection_color_varies_with_index() {
    let c0 = selection_color(0);
    let c1 = selection_color(1);
    let c2 = selection_color(2);
    assert_ne!(c0, c1);
    assert_ne!(c1, c2);
}
