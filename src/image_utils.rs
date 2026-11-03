use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::ValueEnum;
use eframe::egui;
use fast_image_resize::images::Image;
use fast_image_resize::{PixelType, ResizeOptions, Resizer};
use image::{DynamicImage, GenericImage, RgbaImage};
use serde::{Deserialize, Serialize};
use zune_jpeg::JpegDecoder;

use crate::selection::Selection;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug, Serialize, Deserialize)]
pub enum OutputFormat {
    Jpg,
    Png,
    Webp,
    Avif,
}

impl OutputFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Jpg => "jpg",
            OutputFormat::Png => "png",
            OutputFormat::Webp => "webp",
            OutputFormat::Avif => "avif",
        }
    }
}

pub struct PreloadedImage {
    pub path: PathBuf,
    pub image: DynamicImage,
    pub color_image: Option<egui::ColorImage>,
    pub texture: Option<wgpu::Texture>,
    pub load_duration: std::time::Duration,
    pub read_duration: std::time::Duration,
    pub decode_duration: std::time::Duration,
    pub resize_duration: std::time::Duration,
    pub texture_gen_duration: std::time::Duration,
}

pub struct SaveRequest {
    pub image: DynamicImage,
    pub path: PathBuf,
    pub original_path: PathBuf,
    pub quality: u8,
    pub format: OutputFormat,
}

pub struct SaveStatus {
    pub path: PathBuf,
    pub result: Result<()>,
    /// Size of the original file (in bytes) before moving/backup, if available
    pub original_size: Option<u64>,
    /// Size of the newly-written file (in bytes), if available
    pub new_size: Option<u64>,
}

/// Decode image bytes, using the fast zune-jpeg decoder for JPEG files
/// (with fallback to the standard loader for incomplete/failed JPEGs).
pub fn decode_image_bytes(path: &Path, bytes: &[u8]) -> image::ImageResult<DynamicImage> {
    decode_image_bytes_tagged(path, bytes).0
}

/// Like [`decode_image_bytes`], also reporting which engine decoded:
/// `"gpu"` for the wgpu restart-interval JPEG path, `"cpu"` otherwise.
pub fn decode_image_bytes_tagged(
    path: &Path,
    bytes: &[u8],
) -> (image::ImageResult<DynamicImage>, &'static str) {
    let is_jpeg = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("jpg") || s.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false);

    if is_jpeg {
        // GPU decode: baseline + progressive, restart markers or not
        // (DRI=0 gets virtual intervals via the survey pass) —
        // ~10x libjpeg; unsupported inputs fall through to zune.
        if let Some((rgba, w, h)) = crate::gpujpeg::decode_rgba(bytes) {
            if let Some(img) = image::RgbaImage::from_raw(w, h, rgba) {
                return (Ok(image::DynamicImage::ImageRgba8(img)), "gpu");
            }
        }
        // Allow incomplete JPEGs to still be rendered
        let options =
            zune_jpeg::zune_core::options::DecoderOptions::default().set_strict_mode(false);
        let mut decoder = JpegDecoder::new(Cursor::new(bytes));
        decoder.set_options(options);

        let res = match decoder.decode() {
            Ok(pixels) => {
                let info = decoder.info().unwrap();
                image::RgbImage::from_raw(info.width as u32, info.height as u32, pixels)
                    .map(image::DynamicImage::ImageRgb8)
                    .ok_or_else(|| {
                        image::ImageError::Decoding(image::error::DecodingError::new(
                            image::error::ImageFormatHint::Exact(image::ImageFormat::Jpeg),
                            "Failed to create buffer",
                        ))
                    })
            }
            Err(_e) => image::load_from_memory(bytes),
        };
        (res.map(|mut i| {
            apply_exif_orientation(&mut i, bytes);
            i
        }), "cpu")
    } else {
        (image::load_from_memory(bytes), "cpu")
    }
}

/// Apply the EXIF orientation tag (matching the GPU JPEG path, which
/// orients in the upsample shader). No-op without a valid tag.
fn apply_exif_orientation(img: &mut DynamicImage, bytes: &[u8]) {
    let orient = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
                .map(|v| v as u8)
        });
    if let Some(o) = orient.and_then(image::metadata::Orientation::from_exif) {
        img.apply_orientation(o);
    }
}

/// Target dims for a fit-resize — shared by [`resize_to_fit`] and the
/// GPU resize path (`gpujpeg::GpuJpeg`) so both produce identical
/// output sizes.
pub(crate) fn fit_dims(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let ratio = w as f64 / h as f64;
    if ratio > max_w as f64 / max_h as f64 {
        (max_w, ((max_w as f64 / ratio) as u32).max(1))
    } else {
        (((max_h as f64 * ratio) as u32).max(1), max_h)
    }
}

/// Resize the image so it fits within `max_w` x `max_h`, preserving aspect
/// ratio. Returns the image unchanged if it already fits.
pub fn resize_to_fit(image: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (new_w, new_h) = fit_dims(image.width(), image.height(), max_w, max_h);
    if new_w == image.width() && new_h == image.height() {
        return image;
    }

    let src_image = match image {
        image::DynamicImage::ImageRgb8(ref rgb) => Image::from_vec_u8(
            rgb.width(),
            rgb.height(),
            rgb.as_raw().clone(),
            PixelType::U8x3,
        )
        .ok(),
        image::DynamicImage::ImageRgba8(ref rgba) => Image::from_vec_u8(
            rgba.width(),
            rgba.height(),
            rgba.as_raw().clone(),
            PixelType::U8x4,
        )
        .ok(),
        _ => {
            let rgba = image.to_rgba8();
            Image::from_vec_u8(
                rgba.width(),
                rgba.height(),
                rgba.into_raw(),
                PixelType::U8x4,
            )
            .ok()
        }
    };

    let Some(src_image) = src_image else {
        return image;
    };

    let mut dst_image = Image::new(new_w, new_h, src_image.pixel_type());
    let mut resizer = Resizer::new();
    if resizer
        .resize(&src_image, &mut dst_image, &ResizeOptions::default())
        .is_err()
    {
        return image;
    }

    match src_image.pixel_type() {
        PixelType::U8x3 => image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(new_w, new_h, dst_image.into_vec()).unwrap(),
        ),
        PixelType::U8x4 => image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(new_w, new_h, dst_image.into_vec()).unwrap(),
        ),
        _ => unreachable!("We only created U8x3 or U8x4 images"),
    }
}

pub fn to_color_image(img: &DynamicImage) -> egui::ColorImage {
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = rgba.into_raw();
    egui::ColorImage::from_rgba_unmultiplied(size, &pixels)
}

pub fn combine_crops(mut crops: Vec<DynamicImage>) -> DynamicImage {
    // Simple shelf packing or just horizontal stacking if few?
    // User wants to "minimize empty space".
    // Let's sort by height descending.
    crops.sort_by_key(|i| std::cmp::Reverse(i.height()));

    // Calculate total area to estimate canvas size
    let total_area: u64 = crops
        .iter()
        .map(|i| i.width() as u64 * i.height() as u64)
        .sum();
    let max_width = (total_area as f64).sqrt().ceil() as u32 * 2; // Heuristic: start with something wider

    // Simple shelf algorithm
    let mut canvas_width = 0;
    let mut canvas_height = 0;

    struct PlacedImage {
        x: u32,
        y: u32,
        img: DynamicImage,
    }

    let mut placed = Vec::new();
    let mut current_x = 0;
    let mut current_y = 0;
    let mut row_height = 0;

    // First pass: determine positions and canvas size
    for img in crops {
        if current_x + img.width() > max_width && current_x > 0 {
            // New row
            current_x = 0;
            current_y += row_height;
            row_height = 0;
        }

        placed.push(PlacedImage {
            x: current_x,
            y: current_y,
            img: img.clone(),
        });

        row_height = row_height.max(img.height());
        current_x += img.width();

        canvas_width = canvas_width.max(current_x);
        canvas_height = canvas_height.max(current_y + row_height);
    }

    let mut final_image = RgbaImage::new(canvas_width, canvas_height);

    for p in placed {
        // Copy pixels
        // We can use image::GenericImage::copy_from but we need to be careful about types.
        // DynamicImage implements GenericImage.
        let _ = final_image.copy_from(&p.img, p.x, p.y);
    }

    DynamicImage::ImageRgba8(final_image)
}

pub fn build_output_image(image: &DynamicImage, selections: &[Selection]) -> Option<DynamicImage> {
    if selections.is_empty() {
        return Some(image.clone());
    }

    let mut crops = Vec::new();
    for selection in selections {
        if let Some((x, y, w, h)) = selection.to_u32_bounds() {
            if w > 0 && h > 0 {
                crops.push(image.crop_imm(x, y, w, h));
            }
        }
    }

    if crops.is_empty() {
        None
    } else if crops.len() == 1 {
        Some(crops.remove(0))
    } else {
        Some(combine_crops(crops))
    }
}
