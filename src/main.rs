use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Parser;
use eframe::egui;
use rand::seq::SliceRandom;

mod app;
mod fs_utils;
mod image_utils;
mod selection;
mod ui;

use app::ImageCropperApp;
use fs_utils::collect_images;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Fullscreen image cropper with deletion workflow"
)]
struct Args {
    /// Directory that contains images to process
    #[arg(value_name = "DIRECTORY")]
    directory: PathBuf,

    /// Quality of the output image (1-100)
    #[arg(short, long, default_value_t = 60)]
    quality: u8,

    /// Automatically resave images to AVIF when navigating away
    #[arg(long, default_value_t = false)]
    resave: bool,

    /// Skip destructive operations and just print what would happen
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut files = collect_images(&args.directory)?;
    if files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in {}",
            args.directory.display()
        ));
    }
    files.shuffle(&mut rand::thread_rng());
    let dry_run = args.dry_run;
    let quality = args.quality;
    let resave = args.resave;
    let files_for_app = files.clone();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_fullscreen(true),
        ..Default::default()
    };

    eframe::run_native(
        "ImageCropper",
        native_options,
        Box::new(
            move |cc| match ImageCropperApp::new(cc, files_for_app.clone(), dry_run, quality, resave) {
                Ok(app) => Box::new(app) as Box<dyn eframe::App>,
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            },
        ),
    )?;

    Ok(())
}
