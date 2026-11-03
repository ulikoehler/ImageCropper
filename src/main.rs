use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use eframe::egui;

use imagecropper::album::{AlbumCropApp, AlbumOptions};
use imagecropper::app::{AppOptions, ImageCropperApp};
use imagecropper::cli::{collect_sorted_files, AlbumArgs, Cli, Commands, CropArgs, HeadlessArgs};
use imagecropper::fs_utils::previews::produce_preview_bytes;
use imagecropper::image_utils::decode_image_bytes;
use imagecropper::server::{run_server, ServeMode, ServerState};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Crop(args) => run_crop(args),
        Commands::Album(args) => run_album(args),
    }
}

fn fullscreen_window() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_fullscreen(true),
        ..Default::default()
    }
}

fn run_crop(args: CropArgs) -> Result<()> {
    if let Some(addr) = args.headless.serve {
        return serve(args.scan, addr, &args.headless, ServeMode::Crop, None);
    }
    let files = collect_sorted_files(&args.scan)?;
    if args.headless.no_gui {
        return headless_crop(&args, files);
    }
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
        Box::new(move |cc| {
            match ImageCropperApp::new(cc, files_for_app.clone(), options.clone()) {
                Ok(app) => Ok(Box::new(app) as Box<dyn eframe::App>),
                Err(err) => {
                    eprintln!("{err:#}");
                    std::process::exit(1);
                }
            }
        }),
    )?;

    Ok(())
}

fn run_album(args: AlbumArgs) -> Result<()> {
    let move_dir = imagecropper::cli::resolve_move_dir(&args.move_dir)?;
    if let Some(addr) = args.headless.serve {
        return serve(args.scan, addr, &args.headless, ServeMode::Album, move_dir);
    }
    let files = collect_sorted_files(&args.scan)?;
    if args.headless.no_gui {
        return headless_album(&args, files);
    }
    let options = AlbumOptions {
        dry_run: args.scan.dry_run,
        verbose: args.scan.verbose,
        parallel: args.scan.parallel,
        move_dir,
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
        Box::new(
            move |_cc| Ok(Box::new(AlbumCropApp::new(files, options)) as Box<dyn eframe::App>),
        ),
    )?;

    Ok(())
}

/// Shared `--serve` path: collect files, build server state, run.
fn serve(
    scan: imagecropper::cli::ScanArgs,
    addr: std::net::SocketAddr,
    headless: &HeadlessArgs,
    mode: ServeMode,
    move_dir: Option<PathBuf>,
) -> Result<()> {
    let files = collect_sorted_files(&scan)?;
    let state = ServerState::new(
        files,
        scan.paths.clone(),
        mode,
        scan.parallel,
        headless.token.clone().or_else(|| {
            // Keep it usable without a token on loopback; generate one when
            // binding a non-loopback address.
            if addr.ip().is_loopback() {
                None
            } else {
                Some(random_token())
            }
        }),
        headless.push_credit_kb,
        move_dir,
    );
    let _watcher = imagecropper::server::watch::spawn(state.clone(), scan.clone());
    run_server(addr, state, !headless.no_gui)
}

/// `album -G`: pregenerate every preview, then exit.
fn headless_album(args: &AlbumArgs, files: Vec<PathBuf>) -> Result<()> {
    pregenerate_all(&files, args.scan.parallel);
    Ok(())
}

/// Worker-pool pregeneration of all previews (shared by `album -G` and
/// usable by tests). Concurrency self-tunes to the storage device.
fn pregenerate_all(files: &[PathBuf], parallel: usize) {
    use imagecropper::fs_utils::adaptive_pool::AdaptivePool;
    use std::time::Duration;

    let total = files.len();
    let pool = AdaptivePool::new(2, parallel.max(1), |p| produce_preview_bytes(p));
    pool.request_all(files.iter());

    let mut done = 0usize;
    while done < total {
        for _ in pool.poll() {
            done += 1;
            if done % 25 == 0 || done == total {
                eprint!("\rPregenerated previews {done}/{total}");
            }
        }
        if done < total {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    if total > 0 {
        eprintln!(" ({} worker(s))", pool.workers());
    }
}

/// `crop -G`: batch resave all files to the selected format/quality.
fn headless_crop(args: &CropArgs, files: Vec<PathBuf>) -> Result<()> {
    if !args.resave && args.format == imagecropper::image_utils::OutputFormat::Avif {
        eprintln!("crop -G without --resave does nothing (pass --resave and/or -f).");
        return Ok(());
    }
    let mut saver = imagecropper::app::saver::Saver::new(args.scan.parallel.max(1));
    let mut queued = 0usize;
    for f in &files {
        let Ok(bytes) = std::fs::read(f) else {
            continue;
        };
        let Ok(img) = decode_image_bytes(f, &bytes) else {
            continue;
        };
        let out = f.with_extension(args.format.extension());
        saver.queue_save(imagecropper::image_utils::SaveRequest {
            image: img,
            path: out,
            original_path: f.clone(),
            quality: args.quality,
            format: args.format,
        })?;
        queued += 1;
    }
    eprintln!("Queued {queued} resaves; waiting for completion…");
    loop {
        let done = saver.check_completions();
        if saver.pending_saves.is_empty() && done.is_empty() {
            break;
        }
        for (path, result, _) in done {
            if let Err(e) = result {
                eprintln!("{}: {e}", path.display());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

fn random_token() -> String {
    format!("{:032x}", rand::random::<u128>())
}
