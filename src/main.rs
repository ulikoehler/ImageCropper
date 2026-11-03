use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use eframe::egui;

use imagecropper::album::{AlbumCropApp, AlbumOptions};
use imagecropper::app::{AppOptions, ImageCropperApp};
use imagecropper::cli::{
    spawn_scan_stream, AlbumArgs, Cli, Commands, CropArgs, HeadlessArgs, ScanStream,
};
use imagecropper::fs_utils::previews::produce_preview_bytes;
use imagecropper::fs_utils::ScanEvent;
use imagecropper::image_utils::decode_image_bytes;
use imagecropper::server::{run_server, ServeMode, ServerState};

fn main() -> Result<()> {
    imagecropper::fs_utils::install_sigint_handler();
    let cli = Cli::parse();
    match cli.command {
        Commands::Crop(args) => run_crop(args),
        Commands::Album(args) => run_album(args),
        Commands::GpuCheck => run_gpucheck(),
    }
}

/// `gpucheck`: exercise every GPU path on synthetic input and report what
/// actually works on this machine — the presence of `gpujpeg::available()`
/// alone doesn't prove decode/resize/encode all succeed.
fn run_gpucheck() -> Result<()> {
    use imagecropper::gpujpeg;
    println!("gpujpeg capabilities:");
    println!(
        "  IC_NO_GPUJPEG:      {}",
        if std::env::var("IC_NO_GPUJPEG").is_ok() {
            "set (GPU disabled)"
        } else {
            "unset"
        }
    );
    let avail = gpujpeg::available();
    println!("  wgpu adapter:       {}", if avail { "found (IMMEDIATES ok)" } else { "NOT FOUND — all GPU paths fall back to CPU" });
    if !avail {
        return Ok(());
    }

    // A small synthetic JPEG through the normal encoder (typically DRI=0 —
    // exactly the class the survey/virtual-interval path must handle).
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(64, 48, |x, y| {
        image::Rgb([(x * 4) as u8, (y * 4) as u8, 128])
    }));
    let mut jpeg = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut jpeg),
        image::ImageFormat::Jpeg,
    )?;

    match gpujpeg::probe(&jpeg) {
        Some((w, h, dri)) => println!("  probe test jpeg:    {w}x{h} dri={dri}"),
        None => println!("  probe test jpeg:    unsupported format"),
    }
    match gpujpeg::decode_rgba(&jpeg) {
        Some((rgba, w, h)) => println!("  GPU decode:         ok ({w}x{h}, {} bytes)", rgba.len()),
        None => println!("  GPU decode:         FAILS → CPU fallback"),
    }
    match gpujpeg::open(&jpeg).and_then(|s| s.resize_rgb(32, 24)) {
        Some((rgb, w, h)) => println!("  GPU decode+resize:  ok ({w}x{h}, {} bytes)", rgb.len()),
        None => println!("  GPU decode+resize:  FAILS → CPU fallback"),
    }
    let rgba = vec![0x80u8; 32 * 32 * 4];
    match gpujpeg::encode_rgba(&rgba, 32, 32, 80) {
        Some(j) => {
            // Round-trip check: the GPU-encoded bytes must decode again.
            let ok = image::load_from_memory(&j).is_ok();
            println!("  GPU jpeg encode:    ok ({} bytes, decodes: {ok})", j.len());
        }
        None => println!("  GPU jpeg encode:    unavailable"),
    }
    Ok(())
}

fn fullscreen_window() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_fullscreen(true),
        ..Default::default()
    }
}

fn run_crop(args: CropArgs) -> Result<()> {
    imagecropper::fs_utils::set_verbose(args.scan.verbose);
    if let Some(addr) = args.headless.serve {
        return serve(
            args.scan,
            addr,
            &args.headless,
            ServeMode::Crop,
            imagecropper::cli::MouseSelectMode::Sequence,
            None,
        );
    }
    let scan = spawn_scan_stream(&args.scan)?;
    if args.headless.no_gui {
        return headless_crop(&args, scan);
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
    let mut scan_slot = Some(scan);

    eframe::run_native(
        "ImageCropper",
        fullscreen_window(),
        Box::new(move |cc| {
            match ImageCropperApp::new(cc, Vec::new(), options.clone(), scan_slot.take()) {
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
    imagecropper::fs_utils::set_verbose(args.scan.verbose);
    let move_dir = imagecropper::cli::resolve_move_dir(&args.move_dir)?;
    if let Some(addr) = args.headless.serve {
        return serve(
            args.scan,
            addr,
            &args.headless,
            ServeMode::Album,
            args.mouse_select,
            move_dir,
        );
    }
    let scan = spawn_scan_stream(&args.scan)?;
    if args.headless.no_gui {
        return headless_album(&args, scan);
    }
    let options = AlbumOptions {
        dry_run: args.scan.dry_run,
        verbose: args.scan.verbose,
        parallel: args.scan.parallel,
        move_dir,
        mouse_select: args.mouse_select,
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

    let mut scan_slot = Some(scan);
    eframe::run_native(
        "ImageCropper Album",
        fullscreen_window(),
        Box::new(move |_cc| {
            Ok(
                Box::new(AlbumCropApp::new(Vec::new(), options, scan_slot.take()))
                    as Box<dyn eframe::App>,
            )
        }),
    )?;

    Ok(())
}

/// Shared `--serve` path: start the server with an empty manifest, then
/// stream scan results in as they are discovered (never blocks on listing).
fn serve(
    scan: imagecropper::cli::ScanArgs,
    addr: std::net::SocketAddr,
    headless: &HeadlessArgs,
    mode: ServeMode,
    mouse_select: imagecropper::cli::MouseSelectMode,
    move_dir: Option<PathBuf>,
) -> Result<()> {
    let stream = spawn_scan_stream(&scan)?;
    // -p/--password pins the secret outright; --auth=password requires it.
    let token = match (&headless.password, headless.auth) {
        (Some(p), _) => Some(p.clone()),
        (None, imagecropper::cli::ServeAuth::Token) => Some(random_token()),
        (None, imagecropper::cli::ServeAuth::Password) => {
            anyhow::bail!("--auth=password requires -p/--password <SECRET>");
        }
        (None, imagecropper::cli::ServeAuth::None) => None,
    };
    let state = ServerState::new(
        Vec::new(),
        scan.paths.clone(),
        mode,
        mouse_select,
        scan.parallel,
        token,
        headless.push_credit_kb,
        move_dir,
    );
    // The server binds and prints its URL first; scan feeding and the FS
    // watcher (recursive registration can take a long time on remote
    // mounts) start once it is reachable.
    let start_state = state.clone();
    run_server(addr, state, !headless.no_gui, move || {
        imagecropper::server::spawn_scan_feeder(&start_state, stream);
        imagecropper::server::watch::spawn(start_state, scan);
    })
}

/// `album -G`: pregenerate every preview as scan results stream in.
fn headless_album(args: &AlbumArgs, stream: ScanStream) -> Result<()> {
    pregenerate_stream(stream, args.scan.parallel);
    Ok(())
}

/// Worker-pool pregeneration fed by a streaming scan — preview work starts
/// while the directory listing is still being produced (pipelined; matters
/// on slow mounts). Concurrency self-tunes to the storage device.
fn pregenerate_stream(stream: ScanStream, parallel: usize) {
    use imagecropper::fs_utils::adaptive_pool::AdaptivePool;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    let pool = AdaptivePool::new(2, parallel.max(1), |p| produce_preview_bytes(p, None));
    let mut queued = 0usize;
    let mut done = 0usize;
    let mut scanning = true;
    while scanning || pool.pending_len() > 0 {
        // Ctrl-C: stop preview generation outright — nothing queued is
        // worth blocking the exit for.
        if imagecropper::fs_utils::sigint_received() {
            pool.cancel();
            eprintln!("\nCtrl-C: stopped preview generation");
            break;
        }
        if scanning {
            match stream.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(ScanEvent::Found { path, .. }) => {
                    pool.request(path);
                    queued += 1;
                }
                Ok(ScanEvent::Error(e)) => eprintln!("scan: {e}"),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => scanning = false,
            }
        }
        for _ in pool.poll() {
            done += 1;
            if done % 25 == 0 {
                eprint!("\rPregenerated previews {done}/{queued}…");
            }
        }
        if !scanning && pool.pending_len() == 0 {
            break;
        }
    }
    if queued > 0 {
        eprintln!(
            "\rPregenerated previews {done}/{queued} ({} worker(s))",
            pool.workers()
        );
    }
}

/// `crop -G`: batch resave all files to the selected format/quality as the
/// scan streams them in.
fn headless_crop(args: &CropArgs, stream: ScanStream) -> Result<()> {
    if !args.resave && args.format == imagecropper::image_utils::OutputFormat::Avif {
        eprintln!("crop -G without --resave does nothing (pass --resave and/or -f).");
        return Ok(());
    }
    let mut saver = imagecropper::app::saver::Saver::new(args.scan.parallel.max(1));
    let mut queued = 0usize;
    for ev in stream.rx {
        // Ctrl-C: stop queueing new work; saves already in flight finish.
        if imagecropper::fs_utils::sigint_received() {
            break;
        }
        match ev {
            ScanEvent::Found { path: f, .. } => {
                let Ok(bytes) = std::fs::read(&f) else {
                    continue;
                };
                let Ok(img) = decode_image_bytes(&f, &bytes) else {
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
            ScanEvent::Error(e) => eprintln!("scan: {e}"),
        }
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
