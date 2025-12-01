use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Parser;
use eframe::egui;
use rand::seq::SliceRandom;

use imagecropper::app::ImageCropperApp;
use imagecropper::fs_utils::collect_images;
use imagecropper::image_utils::OutputFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SortOrder {
    Filename,
    Randomize,
    Modified,
}

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

    /// Output format for saved images
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Avif)]
    format: OutputFormat,

    /// Automatically resave images to the selected format when navigating away
    #[arg(short, long, default_value_t = false)]
    resave: bool,

    /// Skip destructive operations and just print what would happen
    #[arg(short = 'd', long, default_value_t = false)]
    dry_run: bool,

    /// Number of parallel image processing threads
    #[arg(short = 'j', long = "parallel", default_value_t = 16)]
    parallel: usize,

    /// Recurse into subdirectories to find images (disabled by default)
    #[arg(short = 'r', long = "recursive", default_value_t = false)]
    recursive: bool,

    /// Invert order of processed images (ignored for randomize)
    #[arg(short = 'i', long = "inverse-order", default_value_t = false)]
    inverse: bool,

    /// Order in which images are processed
    #[arg(short = 'o', long, value_enum, default_value_t = SortOrder::Filename)]
    order: SortOrder,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut files = collect_images(&args.directory, args.recursive)?;
    if files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in {}. Supported formats are: {}",
            args.directory.display(),
            imagecropper::fs_utils::SUPPORTED_EXTENSIONS.join(", ")
        ));
    }
    match args.order {
        SortOrder::Filename => files.sort(),
        SortOrder::Randomize => files.shuffle(&mut rand::thread_rng()),
        SortOrder::Modified => files.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
        }),
    }

    // If the inverse flag is set and ordering isn't randomized, invert the order
    if args.inverse && args.order != SortOrder::Randomize {
        files.reverse();
    }
    let dry_run = args.dry_run;
    let quality = args.quality;
    let resave = args.resave;
    let format = args.format;
    let parallel = args.parallel;
    let files_for_app = files.clone();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_fullscreen(true),
        ..Default::default()
    };

    eframe::run_native(
        "ImageCropper",
        native_options,
        Box::new(
            move |cc| match ImageCropperApp::new(cc, files_for_app.clone(), dry_run, quality, resave, format, parallel) {
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

