use imagecropper::image_utils::*;

mod common;
use common::solid_image;

#[test]
fn output_format_extensions_match_expectations() {
    assert_eq!(OutputFormat::Jpg.extension(), "jpg");
    assert_eq!(OutputFormat::Png.extension(), "png");
    assert_eq!(OutputFormat::Webp.extension(), "webp");
    assert_eq!(OutputFormat::Avif.extension(), "avif");
}

#[test]
fn to_color_image_matches_input_dimensions() {
    let img = solid_image(3, 5, [10, 20, 30, 255]);
    let color = to_color_image(&img);
    assert_eq!(color.size, [3, 5]);
    assert_eq!(color.pixels.len(), (img.width() * img.height()) as usize);
    assert_eq!(color.pixels[0].r(), 10);
    assert_eq!(color.pixels[0].g(), 20);
    assert_eq!(color.pixels[0].b(), 30);
}

#[test]
fn combine_crops_keeps_all_pixels() {
    let red = solid_image(2, 2, [255, 0, 0, 255]);
    let blue = solid_image(1, 3, [0, 0, 255, 255]);
    let combined = combine_crops(vec![red.clone(), blue.clone()]).to_rgba8();
    let mut red_count = 0;
    let mut blue_count = 0;
    for chunk in combined.chunks_exact(4) {
        match chunk {
            [255, 0, 0, 255] => red_count += 1,
            [0, 0, 255, 255] => blue_count += 1,
            _ => {}
        }
    }
    assert_eq!(red_count, (red.width() * red.height()) as usize);
    assert_eq!(blue_count, (blue.width() * blue.height()) as usize);
}
