use std::cmp::Ordering;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use eframe::egui;
use rand::seq::SliceRandom;

use imagecropper::album::{AlbumCropApp, AlbumOptions};
use imagecropper::app::{AppOptions, ImageCropperApp};
use imagecropper::fs_utils::{
    collect_images_with_filter, exif_datetime_key, FilterSyntax, PathFilter,
};
use imagecropper::image_utils::OutputFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SortOrder {
    Filename,
    Randomize,
    Modified,
    /// By file size, smallest first (same as size+)
    Size,
    /// By file size, smallest first
    #[value(name = "size+")]
    SizeAsc,
    /// By file size, largest first
    #[value(name = "size-")]
    SizeDesc,
    /// By EXIF capture time, oldest first (same as exif+)
    Exif,
    /// By EXIF capture time, oldest first
    #[value(name = "exif+")]
    ExifAsc,
    /// By EXIF capture time, newest first
    #[value(name = "exif-")]
    ExifDesc,
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Fullscreen image cropper with deletion workflow"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Interactively crop images in a fullscreen UI
    Crop(CropArgs),
    /// Browse images as a space-filling album grid of previews
    Album(AlbumArgs),
}

/// Arguments shared by all subcommands: input paths, recursion, filters,
/// sorting and verbosity.
#[derive(Args, Debug)]
struct ScanArgs {
    /// Directories or files to process
    #[arg(value_name = "PATHS", required = true)]
    paths: Vec<PathBuf>,

    /// Recurse into subdirectories to find images (disabled by default)
    #[arg(short = 'r', long = "recursive", default_value_t = false)]
    recursive: bool,

    /// Pattern syntax for whitelist and blacklist filters
    #[arg(long, value_enum, default_value_t = FilterSyntax::Glob)]
    filter_syntax: FilterSyntax,

    /// Only include paths matching this filter (also overrides the blacklist)
    #[arg(long, value_name = "PATTERN")]
    whitelist: Vec<String>,

    /// Exclude paths matching this filter unless they also match a whitelist
    #[arg(long, value_name = "PATTERN")]
    blacklist: Vec<String>,

    /// Number of parallel image processing threads
    #[arg(short = 'j', long = "parallel", default_value_t = 16)]
    parallel: usize,

    /// Skip destructive operations and just print what would happen
    #[arg(short = 'd', long, default_value_t = false)]
    dry_run: bool,

    /// Report file sizes and actions as they complete
    #[arg(short = 'v', long, default_value_t = false)]
    verbose: bool,

    /// Invert order of processed images (ignored for randomize)
    #[arg(short = 'i', long = "inverse-order", default_value_t = false)]
    inverse: bool,

    /// Order in which images are processed
    #[arg(short, long, value_enum, default_value_t = SortOrder::Filename)]
    order: SortOrder,
}

#[derive(Args, Debug)]
struct CropArgs {
    #[command(flatten)]
    scan: ScanArgs,

    /// Quality of the output image (1-100)
    #[arg(short, long, default_value_t = 70)]
    quality: u8,

    /// Output format for saved images
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Avif)]
    format: OutputFormat,

    /// Automatically resave images to the selected format when navigating away
    #[arg(long, default_value_t = false)]
    resave: bool,

    /// Show performance diagnostics
    #[arg(long, default_value_t = false)]
    benchmark: bool,
}

#[derive(Args, Debug)]
struct AlbumArgs {
    #[command(flatten)]
    scan: ScanArgs,

    /// Quality of the output image (1-100) when cropping from the album
    #[arg(short, long, default_value_t = 70)]
    quality: u8,

    /// Output format for saved images when cropping from the album
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Avif)]
    format: OutputFormat,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Crop(args) => run_crop(args),
        Commands::Album(args) => run_album(args),
    }
}

/// Collect the input images, apply filters and sort them the same way for
/// every subcommand.
fn collect_sorted_files(scan: &ScanArgs) -> Result<Vec<PathBuf>> {
    let file_filter = PathFilter::compile(
        scan.filter_syntax,
        &scan.whitelist,
        &scan.blacklist,
    )?;
    let mut files =
        collect_images_with_filter(&scan.paths, scan.recursive, file_filter.as_ref())?;
    if files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in the provided paths. Supported formats are: {}",
            imagecropper::fs_utils::SUPPORTED_EXTENSIONS.join(", ")
        ));
    }
    sort_files(&mut files, scan.order, scan.inverse);
    Ok(files)
}

fn sort_files(files: &mut Vec<PathBuf>, order: SortOrder, inverse: bool) {
    match order {
        SortOrder::Filename => files.sort(),
        SortOrder::Randomize => files.shuffle(&mut rand::rng()),
        SortOrder::Modified => files.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
        }),
        SortOrder::Size | SortOrder::SizeAsc => files.sort_by_key(|path| {
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        }),
        SortOrder::SizeDesc => files.sort_by_key(|path| {
            std::cmp::Reverse(std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        }),
        SortOrder::Exif | SortOrder::ExifAsc | SortOrder::ExifDesc => {
            let descending = order == SortOrder::ExifDesc;
            let mut keyed: Vec<_> = files
                .drain(..)
                .map(|path| (exif_datetime_key(&path), path))
                .collect();
            keyed.sort_by(|(a, _), (b, _)| exif_cmp(a, b, descending));
            files.extend(keyed.into_iter().map(|(_, path)| path));
        }
    }

    // If the inverse flag is set and ordering isn't randomized, invert the order
    if inverse && order != SortOrder::Randomize {
        files.reverse();
    }
}

/// Compare two optional EXIF keys; files without EXIF data always sort last.
fn exif_cmp(
    a: &Option<(u32, u32, u32, u32, u32, u32)>,
    b: &Option<(u32, u32, u32, u32, u32, u32)>,
    descending: bool,
) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => {
            if descending {
                y.cmp(x)
            } else {
                x.cmp(y)
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn fullscreen_window() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_fullscreen(true),
        ..Default::default()
    }
}

fn run_crop(args: CropArgs) -> Result<()> {
    let files = collect_sorted_files(&args.scan)?;
    let options = AppOptions {
        dry_run: args.scan.dry_run,
        quality: args.quality,
        resave: args.resave,
        verbose: args.scan.verbose,
        format: args.format,
        parallel: args.scan.parallel,
        benchmark: args.benchmark,
        embedded: false,
    };
    let files_for_app = files.clone();

    eframe::run_native(
        "ImageCropper",
        fullscreen_window(),
        Box::new(
            move |cc| match ImageCropperApp::new(cc, files_for_app.clone(), options.clone()) {
                Ok(app) => Ok(Box::new(app) as Box<dyn eframe::App>),
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            },
        ),
    )?;

    Ok(())
}

fn run_album(args: AlbumArgs) -> Result<()> {
    let files = collect_sorted_files(&args.scan)?;
    let options = AlbumOptions {
        dry_run: args.scan.dry_run,
        verbose: args.scan.verbose,
        parallel: args.scan.parallel,
        crop: AppOptions {
            dry_run: args.scan.dry_run,
            quality: args.quality,
            resave: false,
            verbose: args.scan.verbose,
            format: args.format,
            parallel: args.scan.parallel,
            benchmark: false,
            embedded: true,
        },
    };

    eframe::run_native(
        "ImageCropper Album",
        fullscreen_window(),
        Box::new(move |_cc| {
            Ok(Box::new(AlbumCropApp::new(files, options)) as Box<dyn eframe::App>)
        }),
    )?;

    Ok(())
}
