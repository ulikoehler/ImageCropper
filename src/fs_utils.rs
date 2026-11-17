use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::RegexSet;
use walkdir::WalkDir;

pub mod adaptive_pool;
pub mod files_pack;
pub mod pack;
pub mod pipeline;
pub mod previews;

static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enable `-v/--verbose` output across scan/preview/action paths. Set once
/// at startup from the CLI flag.
pub fn set_verbose(v: bool) {
    VERBOSE.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(std::sync::atomic::Ordering::Relaxed)
}

static SIGINT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Install a SIGINT (Ctrl-C) handler that only raises a flag — callers
/// poll [`sigint_received`] and run their own graceful shutdown (stop
/// preview generation, finish queued deletes). A second Ctrl-C bumps
/// [`sigint_count`] so a drain loop can be abandoned.
pub fn install_sigint_handler() {
    let _ = ctrlc::set_handler(|| {
        SIGINT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Whether Ctrl-C has been pressed since startup.
pub fn sigint_received() -> bool {
    sigint_count() > 0
}

/// How many times Ctrl-C was pressed. `> 1` means "stop draining, quit now".
pub fn sigint_count() -> usize {
    SIGINT_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

pub const TRASH_DIR: &str = ".imagecropper-trash";
/// Fixed worker count for deletions — trash moves and `--force-delete`
/// unlinks alike — independent of `-j` because delete throughput is bound
/// by the filesystem, not decode CPU.
pub const DELETE_WORKERS: usize = 32;

/// True when `err` is ENOSPC — the filesystem has no room for the write.
/// Preview caches are pure regenerable data, so callers use this to
/// trigger cleanup and retry rather than failing the operation.
pub fn is_no_space(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::StorageFull
}
pub const ORIGINALS_DIR: &str = ".imagecropper-originals";
pub const TEMP_DIR: &str = ".imagecropper-tmp";
pub const PREVIEWS_DIR: &str = ".imagecropper-previews";

const RESERVED_DIRS: &[&str] = &[TRASH_DIR, ORIGINALS_DIR, TEMP_DIR, PREVIEWS_DIR];

fn is_reserved_dir_name(name: &OsStr) -> bool {
    RESERVED_DIRS.iter().any(|reserved| name == *reserved)
}

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "bmp", "gif", "webp", "tiff", "tif", "ico", "avif",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FilterSyntax {
    Glob,
    Regex,
}

enum PatternMatcher {
    None,
    Glob(GlobSet),
    Regex(RegexSet),
}

impl PatternMatcher {
    fn compile(syntax: FilterSyntax, patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self::None);
        }

        match syntax {
            FilterSyntax::Glob => {
                let mut builder = GlobSetBuilder::new();
                for pattern in patterns {
                    let glob = Glob::new(pattern)
                        .with_context(|| format!("Invalid glob pattern: {pattern}"))?;
                    builder.add(glob);
                }
                Ok(Self::Glob(
                    builder
                        .build()
                        .context("Failed to compile glob filter patterns")?,
                ))
            }
            FilterSyntax::Regex => Ok(Self::Regex(
                RegexSet::new(patterns).context("Failed to compile regex filter patterns")?,
            )),
        }
    }

    fn matches(&self, path: &Path) -> bool {
        let candidate = normalize_filter_path(path);
        match self {
            Self::None => false,
            Self::Glob(set) => set.is_match(&candidate),
            Self::Regex(set) => set.is_match(&candidate),
        }
    }
}

pub struct PathFilter {
    whitelist: PatternMatcher,
    blacklist: PatternMatcher,
}

impl PathFilter {
    pub fn compile(
        syntax: FilterSyntax,
        whitelist_patterns: &[String],
        blacklist_patterns: &[String],
    ) -> Result<Option<Self>> {
        if whitelist_patterns.is_empty() && blacklist_patterns.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self {
            whitelist: PatternMatcher::compile(syntax, whitelist_patterns)?,
            blacklist: PatternMatcher::compile(syntax, blacklist_patterns)?,
        }))
    }

    pub fn matches(&self, path: &Path) -> bool {
        if self.whitelist.matches(path) {
            return true;
        }

        if self.blacklist.matches(path) {
            return false;
        }

        // A whitelist also restricts the allowed set: if one was provided
        // and the path didn't match it, the file is excluded.
        matches!(self.whitelist, PatternMatcher::None)
    }
}

fn normalize_filter_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// One event from a streaming directory scan.
#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// An image file matched — emitted the moment it is discovered, with
    /// its byte size and folded content signature (both from the one
    /// metadata call the walker already makes — consumers never need to
    /// re-stat the file for listing purposes).
    Found { path: PathBuf, bytes: u64, sig: u64 },
    /// A cached entry from `files.pack` (dir mtime unchanged): the consumer
    /// gets the recorded dims + signature, no re-sniff needed.
    Cached {
        path: PathBuf,
        w: u32,
        h: u32,
        bytes: u64,
        sig: u64,
    },
    /// Non-fatal problem (missing root, unreadable dir); the scan continues.
    Error(String),
}

/// Walk `paths` in filesystem order, calling `emit` for every match —
/// never collects, sorts, or waits, so consumers can work on partial
/// results as soon as the first entries arrive.
pub fn walk_images(
    paths: &[PathBuf],
    recursive: bool,
    filter: Option<&PathFilter>,
    emit: &mut dyn FnMut(ScanEvent),
) {
    walk_images_limited(paths, recursive, filter, None, emit)
}

/// [`walk_images`] with an optional cap on discovered files: the walk stops
/// as soon as `max` images were emitted (0 or `None` = unlimited). Stopping
/// the walk itself — not just the events — is the point: it keeps a huge or
/// slow tree from being traversed when only the first N images are wanted.
pub fn walk_images_limited(
    paths: &[PathBuf],
    recursive: bool,
    filter: Option<&PathFilter>,
    max: Option<usize>,
    emit: &mut dyn FnMut(ScanEvent),
) {
    walk_images_seeded(paths, recursive, filter, max, None, emit)
}

/// [`walk_images_limited`] with a `files.pack` seed: `seeds` are emitted
/// first (they count toward the limit), and dirs in `skip` are pruned —
/// their contents arrive via the seeds instead of a re-walk.
pub fn walk_images_seeded(
    paths: &[PathBuf],
    recursive: bool,
    filter: Option<&PathFilter>,
    max: Option<usize>,
    plan: Option<(Vec<ScanEvent>, std::collections::HashSet<PathBuf>)>,
    emit: &mut dyn FnMut(ScanEvent),
) {
    let limit = max.filter(|n| *n > 0).unwrap_or(usize::MAX);
    let mut emitted = 0usize;
    let (seeds, skip) = plan.unwrap_or_default();
    // Parent dirs of the Cached seeds: their direct files already
    // arrived, so Found events for them are suppressed — but the walker
    // still descends (only leaf dirs are in `skip`; a non-leaf dir's
    // mtime says nothing about files inside its subdirectories).
    let seeded: std::collections::HashSet<PathBuf> = seeds
        .iter()
        .filter_map(|ev| match ev {
            ScanEvent::Cached { path, .. } => path.parent().map(Path::to_path_buf),
            _ => None,
        })
        .collect();
    let covered = |p: &Path| p.parent().is_some_and(|d| seeded.contains(d));
    for ev in seeds {
        if emitted >= limit {
            return;
        }
        emit(ev);
        emitted += 1;
    }
    for path in paths {
        if emitted >= limit {
            return;
        }
        // One stat covers exists/is_file/is_dir and the signature — the
        // old code stat'ed the root up to three times.
        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    format!("{} does not exist", path.display())
                } else {
                    format!("Unable to access {}: {e}", path.display())
                };
                emit(ScanEvent::Error(msg));
                continue;
            }
        };

        if meta.is_file() {
            if !covered(path)
                && is_supported_image(path)
                && filter.is_none_or(|f| f.matches(path))
            {
                emit(ScanEvent::Found {
                    path: path.to_path_buf(),
                    bytes: meta.len(),
                    sig: meta_sig(&meta),
                });
                emitted += 1;
            }
        } else if meta.is_dir() {
            if recursive {
                for entry in WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_entry(|e| {
                        (e.depth() == 0
                            || !e.file_type().is_dir()
                            || !is_reserved_dir_name(e.file_name()))
                            && !(e.file_type().is_dir() && skip.contains(e.path()))
                    })
                    .filter_map(|e| e.ok())
                {
                    if covered(entry.path()) {
                        continue;
                    }
                    if entry.file_type().is_file()
                        && is_supported_image(entry.path())
                        && filter.is_none_or(|f| f.matches(entry.path()))
                    {
                        let meta = entry.metadata().ok();
                        emit(ScanEvent::Found {
                            path: entry.path().to_path_buf(),
                            bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                            sig: meta.as_ref().map(meta_sig).unwrap_or(0),
                        });
                        emitted += 1;
                        if emitted >= limit {
                            return;
                        }
                    }
                }
            } else if skip.contains(path) {
                // Non-recursive root whose dir mtime is unchanged — its
                // contents already arrived via the seed events.
            } else {
                match fs::read_dir(path) {
                    Err(e) => emit(ScanEvent::Error(format!(
                        "Unable to read directory {}: {e}",
                        path.display()
                    ))),
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            // d_type from the dirent is free; only
                            // symlinks need a stat (they're followed,
                            // matching the old p.is_file() semantics).
                            let file_type = entry.file_type().ok();
                            let is_symlink = file_type
                                .as_ref()
                                .map(|t| t.is_symlink())
                                .unwrap_or(false);
                            let is_file = file_type
                                .as_ref()
                                .map(|t| t.is_file() || (t.is_symlink() && p.is_file()))
                                .unwrap_or_else(|| p.is_file());
                            if covered(&p)
                                || !is_file
                                || !is_supported_image(&p)
                                || !filter.is_none_or(|f| f.matches(&p))
                            {
                                continue;
                            }
                            // One stat for size + mtime signature.
                            // DirEntry::metadata doesn't follow links —
                            // identical for regular files; symlinks (or
                            // unknown d_type) re-stat with follow, like
                            // the old p.metadata() did.
                            let meta = if is_symlink || file_type.is_none() {
                                fs::metadata(&p).ok()
                            } else {
                                entry.metadata().ok()
                            };
                            emit(ScanEvent::Found {
                                bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                                sig: meta.as_ref().map(meta_sig).unwrap_or(0),
                                path: p,
                            });
                            emitted += 1;
                            if emitted >= limit {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Run [`walk_images`] on a background thread. Events stream over the
/// returned channel in filesystem order; the sender drops (and the
/// receiver sees `Disconnected`) when the scan completes.
pub fn spawn_image_scan(
    paths: Vec<PathBuf>,
    recursive: bool,
    filter: Option<PathFilter>,
) -> std::sync::mpsc::Receiver<ScanEvent> {
    spawn_image_scan_limited(paths, recursive, filter, None)
}

/// [`spawn_image_scan`] that stops walking once `max` images were found.
pub fn spawn_image_scan_limited(
    paths: Vec<PathBuf>,
    recursive: bool,
    filter: Option<PathFilter>,
    max: Option<usize>,
) -> std::sync::mpsc::Receiver<ScanEvent> {
    spawn_image_scan_seeded(paths, recursive, filter, max, None)
}

/// Bound on the scan event channel — the listing stage of the thumbnail
/// pipeline. The walker blocks once consumers fall this far behind
/// instead of building an unbounded in-memory listing ahead of reads.
pub const SCAN_QUEUE_CAP: usize = 4096;

/// [`spawn_image_scan_limited`] with a `files.pack` seed plan. The event
/// channel is bounded ([`SCAN_QUEUE_CAP`]) so listing backpressures on
/// the read/compute stages rather than queuing unboundedly.
pub fn spawn_image_scan_seeded(
    paths: Vec<PathBuf>,
    recursive: bool,
    filter: Option<PathFilter>,
    max: Option<usize>,
    plan: Option<(Vec<ScanEvent>, std::collections::HashSet<PathBuf>)>,
) -> std::sync::mpsc::Receiver<ScanEvent> {
    let (tx, rx) = std::sync::mpsc::sync_channel(SCAN_QUEUE_CAP);
    std::thread::spawn(move || {
        walk_images_seeded(&paths, recursive, filter.as_ref(), max, plan, &mut |ev| {
            let _ = tx.send(ev);
        });
    });
    rx
}

pub fn collect_images(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
    collect_images_with_filter(paths, recursive, None)
}

pub fn collect_images_with_filter(
    paths: &[PathBuf],
    recursive: bool,
    filter: Option<&PathFilter>,
) -> Result<Vec<PathBuf>> {
    collect_images_limited(paths, recursive, filter, None)
}

/// [`collect_images_with_filter`] capped at `max` discovered files.
pub fn collect_images_limited(
    paths: &[PathBuf],
    recursive: bool,
    filter: Option<&PathFilter>,
    max: Option<usize>,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut error = None;
    walk_images_limited(paths, recursive, filter, max, &mut |ev| match ev {
        ScanEvent::Found { path, .. } | ScanEvent::Cached { path, .. } => files.push(path),
        ScanEvent::Error(e) => {
            if error.is_none() {
                error = Some(e);
            }
        }
    });
    if let Some(e) = error {
        return Err(anyhow!("{e}"));
    }
    Ok(files)
}

/// Folded content signature from already-fetched metadata — identical to
/// `previews::preview_sig_u64` so scan events and preview lookups agree.
fn meta_sig(meta: &fs::Metadata) -> u64 {
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    meta.len() ^ nanos as u64 ^ (nanos >> 64) as u64
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|s| ext.eq_ignore_ascii_case(s))
        })
}

/// Map `items` through `f` preserving order — serial for small inputs, a
/// few scoped threads for larger ones. For batches of independent
/// filesystem calls (dir stats, per-file header sniffs) where a handful
/// of workers hides per-op latency without unbounded thread counts.
pub(crate) fn par_map<T, R>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    const PAR: usize = 8;
    const MIN_PAR: usize = 16;
    if items.len() < MIN_PAR {
        return items.iter().map(f).collect();
    }
    let chunk = items.len().div_ceil(items.len().min(PAR));
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("par_map worker panicked"))
            .collect()
    })
}

pub fn prepare_dir(base: &Path, name: &str) -> Result<PathBuf> {
    let dir = base.join(name);
    fs::create_dir_all(&dir).with_context(|| format!("Unable to create {}", dir.display()))?;
    Ok(dir)
}

pub fn move_with_unique_name(source: &Path, target_dir: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", source.display()))?;
    let destination = unique_destination(target_dir, file_name);
    match fs::rename(source, &destination) {
        Ok(()) => Ok(destination),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            // rename(2) can't span filesystems (e.g. the /nastmp CIFS
            // mount → local disk): copy the bytes, keep the mtime like
            // mv does, then remove the source.
            let mtime = fs::metadata(source).and_then(|m| m.modified()).ok();
            if let Err(e) = fs::copy(source, &destination) {
                let _ = fs::remove_file(&destination); // no truncated leftovers
                return Err(anyhow!(e).context(format!(
                    "Unable to copy {} to {}",
                    source.display(),
                    destination.display()
                )));
            }
            if let Some(t) = mtime {
                if let Ok(f) = fs::File::options().write(true).open(&destination) {
                    let _ = f.set_modified(t);
                }
            }
            fs::remove_file(source).with_context(|| {
                format!(
                    "Copied {} to {} but unable to remove the source",
                    source.display(),
                    destination.display()
                )
            })?;
            Ok(destination)
        }
        Err(e) => Err(anyhow!(e).context(format!(
            "Unable to move {} to {}",
            source.display(),
            destination.display()
        ))),
    }
}

pub fn unique_destination(dir: &Path, file_name: &OsStr) -> PathBuf {
    let mut candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_name(file_name);
    for idx in 1.. {
        let new_name = if let Some(ext) = &ext {
            format!("{stem}-{idx}.{ext}")
        } else {
            format!("{stem}-{idx}")
        };
        candidate = dir.join(new_name);
        if !candidate.exists() {
            break;
        }
    }
    candidate
}

pub fn split_name(file_name: &OsStr) -> (String, Option<String>) {
    let name = file_name.to_string_lossy();
    if let Some((stem, ext)) = name.rsplit_once('.') {
        (stem.to_string(), Some(ext.to_string()))
    } else {
        (name.to_string(), None)
    }
}

pub fn backup_original(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = prepare_dir(parent, ORIGINALS_DIR)?;
    move_with_unique_name(path, &dir)
}

/// EXIF capture time as a sortable (year, month, day, hour, minute, second)
/// tuple. Checks DateTimeOriginal, then DateTimeDigitized, then DateTime.
/// Returns `None` for files without usable EXIF metadata.
pub fn exif_datetime_key(path: &Path) -> Option<(u32, u32, u32, u32, u32, u32)> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    // Tag numbers: 0x9003 = DateTimeOriginal, 0x9004 = DateTimeDigitized,
    // 0x0132 = DateTime. Match by number so fields are found regardless of
    // which IFD a camera stored them in.
    let mut fallback = None;
    for field in exif.fields() {
        let tag = field.tag.number();
        if tag == 0x9003 {
            if let Some(key) = parse_exif_datetime(&field.display_value().to_string()) {
                return Some(key);
            }
        } else if fallback.is_none() && matches!(tag, 0x9004 | 0x0132) {
            fallback = parse_exif_datetime(&field.display_value().to_string());
        }
    }
    fallback
}

/// Parse an EXIF datetime string ("YYYY:MM:DD HH:MM:SS") into a tuple that
/// sorts chronologically.
fn parse_exif_datetime(s: &str) -> Option<(u32, u32, u32, u32, u32, u32)> {
    let s = s.trim().trim_matches('"');
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split(':').map(|p| p.parse::<u32>().ok());
    let mut t = time.split(':').map(|p| p.parse::<u32>().ok());
    Some((
        d.next()??,
        d.next()??,
        d.next()??,
        t.next()??,
        t.next()??,
        t.next()??,
    ))
}

/// Format bytes into a short human readable string using 1024-based units.
///
/// Examples: 0 -> "0 B", 512 -> "512 B", 2048 -> "2.0 KB", 1_500_000 -> "1.4 MB"
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if bytes == 0 {
        return "0 B".to_string();
    }

    if b < KB {
        format!("{} B", bytes)
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.2} GB", b / GB)
    }
}

/// Like `format_size`, but keeps the sign for deltas (e.g. "-1.0 KB").
pub fn format_signed_size(bytes: i64) -> String {
    if bytes < 0 {
        format!("-{}", format_size(bytes.unsigned_abs()))
    } else {
        format_size(bytes as u64)
    }
}

/// Running statistics for files that were (re)saved/converted.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConversionStats {
    pub files: usize,
    pub original_bytes: u64,
    pub new_bytes: u64,
    min_saving: Option<i64>,
    max_saving: Option<i64>,
}

impl ConversionStats {
    pub fn record(&mut self, original: u64, new: u64) {
        self.files += 1;
        self.original_bytes = self.original_bytes.saturating_add(original);
        self.new_bytes = self.new_bytes.saturating_add(new);
        let saving = original as i64 - new as i64;
        self.min_saving = Some(self.min_saving.map_or(saving, |m| m.min(saving)));
        self.max_saving = Some(self.max_saving.map_or(saving, |m| m.max(saving)));
    }

    pub fn min_saving(&self) -> i64 {
        self.min_saving.unwrap_or(0)
    }

    pub fn max_saving(&self) -> i64 {
        self.max_saving.unwrap_or(0)
    }

    pub fn avg_saving(&self) -> i64 {
        if self.files == 0 {
            0
        } else {
            (self.original_bytes as i64 - self.new_bytes as i64) / self.files as i64
        }
    }
}

pub fn format_savings_summary(stats: &ConversionStats) -> String {
    let per_file = if stats.files > 0 {
        format!(
            " in {} file(s) (re)saved; per file min/avg/max: {} / {} / {}",
            stats.files,
            format_signed_size(stats.min_saving()),
            format_signed_size(stats.avg_saving()),
            format_signed_size(stats.max_saving())
        )
    } else {
        String::new()
    };
    if stats.original_bytes >= stats.new_bytes {
        format!(
            "Total conversion savings: {} ({} -> {}){}",
            format_size(stats.original_bytes - stats.new_bytes),
            format_size(stats.original_bytes),
            format_size(stats.new_bytes),
            per_file
        )
    } else {
        format!(
            "Total conversion size increase: {} ({} -> {}){}",
            format_size(stats.new_bytes - stats.original_bytes),
            format_size(stats.original_bytes),
            format_size(stats.new_bytes),
            per_file
        )
    }
}

/// Format a summary of deleted files (i.e. moved to trash): count + bytes.
pub fn format_deletion_summary(deleted_files: usize, deleted_bytes: u64) -> String {
    format!(
        "Total deleted: {} file(s) ({})",
        deleted_files,
        format_size(deleted_bytes)
    )
}

/// Same for files moved to the `-m/--move` directory.
pub fn format_move_summary(moved_files: usize, moved_bytes: u64) -> String {
    format!(
        "Total moved: {} file(s) ({})",
        moved_files,
        format_size(moved_bytes)
    )
}

/// Combined summary string suitable for printing at exit.
pub fn format_overall_summary(
    stats: &ConversionStats,
    deleted_files: usize,
    deleted_bytes: u64,
) -> String {
    let mut parts = Vec::new();
    if stats.files > 0 || stats.original_bytes > 0 || stats.new_bytes > 0 {
        parts.push(format_savings_summary(stats));
    }
    if deleted_bytes > 0 || deleted_files > 0 {
        parts.push(format_deletion_summary(deleted_files, deleted_bytes));
    }
    if parts.is_empty() {
        "No operations performed".to_string()
    } else {
        parts.join(" | ")
    }
}
