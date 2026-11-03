use std::io::Write;

use imagecropper::fs_utils::exif_datetime_key;
use tempfile::tempdir;

mod common;
use common::{solid_image, write_image};

/// Build a minimal JPEG file carrying an EXIF APP1 segment with
/// DateTimeOriginal = "2020:01:02 03:04:05".
fn jpeg_with_exif(datetime: &str) -> Vec<u8> {
    assert_eq!(datetime.len() + 1, 20, "EXIF datetime must be 19 chars");

    // --- TIFF payload (little endian) ---
    let datetime_bytes = format!("{datetime}\0").into_bytes();
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II"); // little endian
    tiff.extend_from_slice(&0x2Au16.to_le_bytes()); // magic
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD at offset 8
    tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 IFD entry
    tiff.extend_from_slice(&0x9003u16.to_le_bytes()); // DateTimeOriginal
    tiff.extend_from_slice(&2u16.to_le_bytes()); // type ASCII
    tiff.extend_from_slice(&(datetime_bytes.len() as u32).to_le_bytes());
    let value_offset = 8 + 2 + 12 + 4; // header + count + entry + next-ifd
    tiff.extend_from_slice(&(value_offset as u32).to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD
    tiff.extend_from_slice(&datetime_bytes);

    // --- JPEG: SOI + APP1(Exif) + EOI ---
    let mut jpeg = Vec::new();
    jpeg.extend_from_slice(&[0xFF, 0xD8]);
    jpeg.extend_from_slice(&[0xFF, 0xE1]);
    let app1_len = 2 + 6 + tiff.len() as u16; // includes its own length field
    jpeg.extend_from_slice(&app1_len.to_be_bytes());
    jpeg.extend_from_slice(b"Exif\0\0");
    jpeg.extend_from_slice(&tiff);
    jpeg.extend_from_slice(&[0xFF, 0xD9]);
    jpeg
}

#[test]
fn exif_datetime_key_reads_datetime_original() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("with_exif.jpg");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(&jpeg_with_exif("2020:01:02 03:04:05")).unwrap();
    drop(file);

    assert_eq!(exif_datetime_key(&path), Some((2020, 1, 2, 3, 4, 5)));
}

#[test]
fn exif_datetime_key_sorts_chronologically() {
    let tmp = tempdir().unwrap();
    let older = tmp.path().join("older.jpg");
    let newer = tmp.path().join("newer.jpg");
    std::fs::write(&older, jpeg_with_exif("2001:06:07 08:09:10")).unwrap();
    std::fs::write(&newer, jpeg_with_exif("2022:11:12 13:14:15")).unwrap();

    assert!(exif_datetime_key(&older) < exif_datetime_key(&newer));
}

#[test]
fn exif_datetime_key_returns_none_without_exif() {
    let tmp = tempdir().unwrap();
    let png = tmp.path().join("plain.png");
    write_image(&png, &solid_image(16, 16, [0, 0, 0, 255]));
    assert_eq!(exif_datetime_key(&png), None);
}
