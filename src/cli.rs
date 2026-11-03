//! CLI argument types and file collection/sorting shared by `main` and the
//! headless/server execution paths. Lives in the lib so integration tests
//! can parse arguments and drive the same code the binary uses.

use std::cmp::Ordering;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use rand::seq::SliceRandom;

use crate::fs_utils::{
    collect_images_with_filter, exif_datetime_key, spawn_image_scan, FilterSyntax, PathFilter,
    ScanEvent,
};
use crate::image_utils::OutputFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SortOrder {
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
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Interactively crop images in a fullscreen UI
    Crop(CropArgs),
    /// Browse images as a space-filling album grid of previews
    Album(AlbumArgs),
}

/// Arguments shared by all subcommands: input paths, recursion, filters,
/// sorting and verbosity.
#[derive(Args, Debug, Clone)]
pub struct ScanArgs {
    /// Directories or files to process
    #[arg(value_name = "PATHS", required = true)]
    pub paths: Vec<PathBuf>,

    /// Recurse into subdirectories to find images (disabled by default)
    #[arg(short = 'r', long = "recursive", default_value_t = false)]
    pub recursive: bool,

    /// Pattern syntax for whitelist and blacklist filters
    #[arg(long, value_enum, default_value_t = FilterSyntax::Glob)]
    pub filter_syntax: FilterSyntax,

    /// Only include paths matching this filter (also overrides the blacklist)
    #[arg(long, value_name = "PATTERN")]
    pub whitelist: Vec<String>,

    /// Exclude paths matching this filter unless they also match a whitelist
    #[arg(long, value_name = "PATTERN")]
    pub blacklist: Vec<String>,

    /// Number of parallel image processing threads
    #[arg(short = 'j', long = "parallel", default_value_t = 16)]
    pub parallel: usize,

    /// Skip destructive operations and just print what would happen
    #[arg(short = 'd', long, default_value_t = false)]
    pub dry_run: bool,

    /// Report file sizes and actions as they complete
    #[arg(short = 'v', long, default_value_t = false)]
    pub verbose: bool,

    /// Invert order of processed images (ignored for randomize)
    #[arg(short = 'i', long = "inverse-order", default_value_t = false)]
    pub inverse: bool,

    /// Order in which images are processed
    #[arg(short, long, value_enum, default_value_t = SortOrder::Filename)]
    pub order: SortOrder,
}

/// WebSocket auth mode for `--serve`.
#[derive(clap::ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServeAuth {
    /// Generate a random token; the printed/opened URL carries `?token=`.
    #[default]
    Token,
    /// No token required — any client can connect.
    None,
    /// Fixed password supplied via -p/--password.
    Password,
}

/// How mouse-drag selection behaves in the album grid. Serialized into the
/// manifest so the web client uses the same mode as the native UI.
#[derive(
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
)]
#[serde(rename_all = "kebab-case")]
pub enum MouseSelectMode {
    /// Drag picks the contiguous sequence in list order between the tile
    /// the drag started on and the tile under the cursor.
    #[default]
    Sequence,
    /// Drag selects every tile the drag rectangle intersects.
    RubberBand,
}

/// Headless/web-serving flags shared by `crop` and `album`.
#[derive(Args, Debug, Clone, Default)]
pub struct HeadlessArgs {
    /// No GUI. Alone: run the non-interactive pipeline and exit.
    /// With --serve: start the server without auto-opening a browser.
    #[arg(short = 'G', long = "no-gui", default_value_t = false)]
    pub no_gui: bool,

    /// Serve the web UI at ADDR:PORT instead of opening the native GUI.
    /// Use 127.0.0.1:PORT for local access only, 0.0.0.0:PORT for LAN.
    #[arg(long, value_name = "ADDR:PORT")]
    pub serve: Option<SocketAddr>,

    /// WS auth: `token` (default: random, printed + in URL), `none`, or
    /// `password` (fixed secret from -p/--password).
    #[arg(long, value_enum, default_value_t = ServeAuth::Token)]
    pub auth: ServeAuth,

    /// Fixed password required on WS connect; implies auth regardless of
    /// --auth (required for --auth=password).
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    /// Initial per-client preview push credit in KB
    #[arg(long, default_value_t = 4096)]
    pub push_credit_kb: u32,
}

#[derive(Args, Debug)]
pub struct CropArgs {
    #[command(flatten)]
    pub scan: ScanArgs,

    /// Quality of the output image (1-100)
    #[arg(short, long, default_value_t = 70)]
    pub quality: u8,

    /// Output format for saved images
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Avif)]
    pub format: OutputFormat,

    /// Automatically resave images to the selected format when navigating away
    #[arg(long, default_value_t = false)]
    pub resave: bool,

    /// Show performance diagnostics
    #[arg(long, default_value_t = false)]
    pub benchmark: bool,

    #[command(flatten)]
    pub headless: HeadlessArgs,
}

#[derive(Args, Debug)]
pub struct AlbumArgs {
    #[command(flatten)]
    pub scan: ScanArgs,

    /// Quality of the output image (1-100) when cropping from the album
    #[arg(short, long, default_value_t = 70)]
    pub quality: u8,

    /// Output format for saved images when cropping from the album
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Avif)]
    pub format: OutputFormat,

    /// Directory to move selected files to with the `m` key
    /// (created if missing; must be a directory)
    #[arg(short = 'm', long = "move", value_name = "DIR")]
    pub move_dir: Option<PathBuf>,

    /// Mouse drag selection mode: `sequence` (default) selects the
    /// contiguous run between the drag anchor and the cursor tile;
    /// `rubber-band` selects every tile the drag rectangle intersects.
    #[arg(long = "mouse-select-mode", value_enum, default_value_t = MouseSelectMode::Sequence)]
    pub mouse_select: MouseSelectMode,

    #[command(flatten)]
    pub headless: HeadlessArgs,
}

/// Resolve `-m/--move`: create the directory if missing, then verify it's
/// actually a directory. Returns the canonical path.
pub fn resolve_move_dir(arg: &Option<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(dir) = arg else { return Ok(None) };
    if dir.exists() && !dir.is_dir() {
        return Err(anyhow!("{} exists but is not a directory", dir.display()));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow!("Unable to create move directory {}: {e}", dir.display()))?;
    Ok(Some(dir.canonicalize().unwrap_or_else(|_| dir.clone())))
}

/// A running background directory scan. Paths stream over `rx` in
/// filesystem order — consumers must work with partial lists and apply
/// `order`/`inverse` only once the stream ends.
pub struct ScanStream {
    pub rx: Receiver<ScanEvent>,
    pub order: SortOrder,
    pub inverse: bool,
}

/// Spawn a streaming scan for `scan`'s paths/filters. Errors in the
/// filter patterns fail up front; per-path problems arrive as
/// [`ScanEvent::Error`]s.
pub fn spawn_scan_stream(scan: &ScanArgs) -> Result<ScanStream> {
    let filter = PathFilter::compile(scan.filter_syntax, &scan.whitelist, &scan.blacklist)?;
    Ok(ScanStream {
        rx: spawn_image_scan(scan.paths.clone(), scan.recursive, filter),
        order: scan.order,
        inverse: scan.inverse,
    })
}

/// Collect the input images, apply filters and sort them the same way for
/// every subcommand.
pub fn collect_sorted_files(scan: &ScanArgs) -> Result<Vec<PathBuf>> {
    let file_filter = PathFilter::compile(scan.filter_syntax, &scan.whitelist, &scan.blacklist)?;
    let mut files = collect_images_with_filter(&scan.paths, scan.recursive, file_filter.as_ref())?;
    if files.is_empty() {
        return Err(anyhow!(
            "No supported image files found in the provided paths. Supported formats are: {}",
            crate::fs_utils::SUPPORTED_EXTENSIONS.join(", ")
        ));
    }
    sort_files(&mut files, scan.order, scan.inverse);
    Ok(files)
}

pub fn sort_files(files: &mut Vec<PathBuf>, order: SortOrder, inverse: bool) {
    match order {
        SortOrder::Filename => files.sort(),
        SortOrder::Randomize => files.shuffle(&mut rand::rng()),
        SortOrder::Modified => {
            files.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok())
        }
        SortOrder::Size | SortOrder::SizeAsc => {
            files.sort_by_key(|path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
        }
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
