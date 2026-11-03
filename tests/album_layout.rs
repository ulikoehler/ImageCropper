use imagecropper::album::layout::layout_justified;

#[test]
fn completed_rows_fill_the_width_exactly() {
    // 8 square images; two rows of 4.
    let aspects = vec![1.0; 8];
    let width = 424.0;
    let gap = 4.0;
    let layout = layout_justified(&aspects, width, 100.0, gap);

    assert_eq!(layout.rows.len(), 2);
    assert_eq!(layout.rows[0].start, 0);
    assert_eq!(layout.rows[0].end, 4);
    assert_eq!(layout.rows[1].start, 4);
    assert_eq!(layout.rows[1].end, 8);

    // The completed first row must fill the width exactly: 4 tiles + 3 gaps.
    let row0_width: f32 = (0..4).map(|i| layout.tiles[i].width()).sum::<f32>() + 3.0 * gap;
    assert!((row0_width - width).abs() < 0.01, "got {row0_width}");

    // Completed rows may stretch beyond the target height to fill the space.
    assert!(layout.tiles[0].height() >= 100.0);

    // The last row stays at the target height so leftovers are not blown up.
    assert!((layout.tiles[4].height() - 100.0).abs() < 0.01);
}

#[test]
fn mixed_aspects_fill_width() {
    let aspects = vec![2.0, 1.0, 1.0, 2.0];
    let width = 424.0;
    let gap = 4.0;
    let layout = layout_justified(&aspects, width, 100.0, gap);

    // First three fit at target height (aspects 2+1+1 -> 408 <= 424), the
    // fourth would overflow, so row 0 = items 0..3 and is justified exactly.
    assert_eq!(layout.rows[0].end, 3);
    let row0_width: f32 = (0..3).map(|i| layout.tiles[i].width()).sum::<f32>() + 2.0 * gap;
    assert!((row0_width - width).abs() < 0.01, "got {row0_width}");
}

#[test]
fn a_single_wide_item_still_forms_a_row() {
    // One 4:1 panorama in a 200px wide view: it fills the row on its own.
    let layout = layout_justified(&[4.0], 200.0, 100.0, 4.0);
    assert_eq!(layout.rows.len(), 1);
    assert!(layout.tiles[0].width() <= 200.0 + 0.01);
}

#[test]
fn row_of_reports_the_row_index() {
    let aspects = vec![1.0; 8];
    let layout = layout_justified(&aspects, 424.0, 100.0, 4.0);
    assert_eq!(layout.row_of(0), Some(0));
    assert_eq!(layout.row_of(3), Some(0));
    assert_eq!(layout.row_of(4), Some(1));
    assert_eq!(layout.row_of(7), Some(1));
    assert_eq!(layout.row_of(8), None);
}

#[test]
fn empty_input_produces_empty_layout() {
    let layout = layout_justified(&[], 800.0, 100.0, 4.0);
    assert!(layout.tiles.is_empty());
    assert!(layout.rows.is_empty());
    assert_eq!(layout.total_size, eframe::egui::Vec2::ZERO);
}

#[test]
fn total_height_accounts_for_rows_and_gaps() {
    let aspects = vec![1.0; 8];
    let layout = layout_justified(&aspects, 424.0, 100.0, 4.0);
    let expected = layout.rows[0].rect.height() + 4.0 + 100.0;
    assert!((layout.total_size.y - expected).abs() < 0.01);
}
