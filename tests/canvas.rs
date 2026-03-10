use imagecropper::app::canvas::Canvas;
use imagecropper::selection::Selection;
use imagecropper::ui::{ImageMetrics, KeyboardState, ARROW_MOVE_STEP};
use eframe::egui;

fn selection_from_coords(min: (f32, f32), max: (f32, f32)) -> Selection {
    Selection {
        rect: egui::Rect::from_min_max(egui::pos2(min.0, min.1), egui::pos2(max.0, max.1)),
    }
}

#[test]
fn handle_arrow_movement_translates_selection() {
    let mut canvas = Canvas::new();
    canvas.selections.push(selection_from_coords((10.0, 10.0), (20.0, 20.0)));
    let keys = KeyboardState {
        next_image: false,
        prev_image: false,
        save_selection: false,
        delete: false,
        escape: false,
        move_up: false,
        move_down: false,
        move_left: false,
        move_right: true,
        preview: false,
        rotate_cw: false,
        rotate_ccw: false,
    };
    canvas.handle_arrow_movement(&keys, egui::vec2(100.0, 100.0));
    let selection = &canvas.selections[0];
    assert_eq!(selection.rect.min.x, 10.0 + ARROW_MOVE_STEP);
    assert_eq!(selection.rect.max.x, 20.0 + ARROW_MOVE_STEP);
}

#[test]
fn begin_selection_outside_image_clamps_to_image_border() {
    let mut canvas = Canvas::new();
    let canvas_rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 200.0));
    let metrics = ImageMetrics::new(canvas_rect, egui::vec2(100.0, 100.0));

    canvas.begin_selection(egui::pos2(-40.0, 80.0), &metrics, egui::vec2(100.0, 100.0), false);

    let selection = canvas.selections.last().unwrap();
    assert_eq!(selection.rect.min.x, 0.0);
    assert_eq!(selection.rect.max.x, 1.0);
}
