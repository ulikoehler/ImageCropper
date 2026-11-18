//! `display_dims` must agree with what the decode paths produce: previews
//! are EXIF-rotated, so manifest/tile dims transpose for orientations
//! 5–8. A mismatch leaves tiles sized on raw stored pixels while the
//! thumbnail comes out rotated — visibly stretched images.

use imagecropper::image_utils::{decode_image_bytes, display_dims};
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

/// Wrap `tiff` in a JPEG APP1(Exif) segment.
fn app1(tiff: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, 0xE1];
    out.extend_from_slice(&(2 + 6 + tiff.len() as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(tiff);
    out
}

/// Insert an EXIF APP1 segment carrying `orient` (tag 0x0112, SHORT) into
/// `jpeg`, right after SOI. Little-endian TIFF, one IFD entry.
fn with_orientation(jpeg: &[u8], orient: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "must start with SOI");
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&0x2Au16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&orient.to_le_bytes()); // inline value
    tiff.extend_from_slice(&0u16.to_le_bytes()); // value padding
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&app1(&tiff));
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// A stored-landscape image (120x40) rotated by the given EXIF tag.
fn write_oriented(path: &std::path::Path, orient: u16) {
    let mut jpeg = Vec::new();
    solid_image(120, 40, [200, 30, 30, 255])
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    std::fs::write(path, with_orientation(&jpeg, orient)).unwrap();
}

#[test]
fn display_dims_transposes_for_exif_rotation() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("portrait.jpg");
    write_oriented(&path, 6); // rotate 90° CW to display

    // Raw pixels are 120x40; displayed — and thumbnailed — as 40x120.
    assert_eq!(display_dims(&path), Some((40, 120)));

    // And it must match what the preview pipeline actually produces.
    let bytes = std::fs::read(&path).unwrap();
    let img = decode_image_bytes(&path, &bytes).unwrap();
    assert_eq!((img.width(), img.height()), (40, 120));
}

#[test]
fn display_dims_keeps_unrotated_orientation() {
    let tmp = tempdir().unwrap();
    // Flip-only orientations (2/3/4) don't transpose; 1 is already upright.
    for (i, orient) in [1u16, 2, 3, 4].iter().enumerate() {
        let path = tmp.path().join(format!("o{orient}_{i}.jpg"));
        write_oriented(&path, *orient);
        assert_eq!(display_dims(&path), Some((120, 40)), "orient {orient}");
    }
}

#[test]
fn display_dims_ignores_files_without_exif() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("plain.png");
    write_image(&path, &solid_image(64, 32, [0, 0, 200, 255]));
    assert_eq!(display_dims(&path), Some((64, 32)));
}
