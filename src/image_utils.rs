use std::path::PathBuf;

use anyhow::Result;
use clap::ValueEnum;
use eframe::egui;
use image::{DynamicImage, GenericImage, RgbaImage};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
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
    crops.sort_by(|a, b| b.height().cmp(&a.height()));

    // Calculate total area to estimate canvas size
    let total_area: u64 = crops.iter().map(|i| i.width() as u64 * i.height() as u64).sum();
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

