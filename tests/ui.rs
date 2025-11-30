use imagecropper::ui::*;
use imagecropper::selection::Selection;
use eframe::egui::{self, Rect, Vec2};

#[test]
fn image_metrics_center_image_and_compute_scale() {
    let canvas = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 100.0));
    let metrics = ImageMetrics::new(canvas, Vec2::new(50.0, 50.0));
    assert!(metrics.scale > 0.0);
    assert_eq!(metrics.image_size, Vec2::new(50.0, 50.0));
    assert!((metrics.image_rect.center() - canvas.center()).length_sq() < 1.0);
}

#[test]
fn screen_to_image_inverts_selection_rect() {
    let canvas = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 200.0));
    let metrics = ImageMetrics::new(canvas, Vec2::new(100.0, 100.0));
    let point = metrics.image_rect.center();
    let image_pos = metrics.screen_to_image(point);
    assert_eq!(image_pos, egui::pos2(50.0, 50.0));
}

#[test]
fn selection_rect_scales_with_metrics() {
    let canvas = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 200.0));
    let metrics = ImageMetrics::new(canvas, Vec2::new(100.0, 100.0));
    let selection = Selection {
        rect: Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(30.0, 40.0)),
    };
    let rect = metrics.selection_rect(&selection);
    assert!(rect.width() > 0.0);
    assert!(rect.height() > 0.0);
}

#[test]
fn fit_within_respects_available_bounds() {
    let (display, scale) = fit_within(Vec2::new(400.0, 100.0), Vec2::new(200.0, 200.0));
    assert_eq!(display.x, 200.0);
    assert!(display.y <= 200.0);
    assert_eq!(scale, 0.5);
}
