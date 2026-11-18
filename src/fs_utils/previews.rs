//! Persistent thumbnail cache in `.imagecropper-previews`.
//!
//! Previews live in an append-only `thumbs.pack` per directory (see
//! [`crate::fs_utils::pack`]) — one file per folder instead of one JPEG +
//! sidecar per image. Each record embeds the source file's name, size,
//! mtime signature, sha256 content hash and the thumbnail dimension it was
//! produced at, so changed sources automatically miss the cache.
//! Shared by the album UI (`Thumbnailer`) and the web server (`PreviewStore`).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use image::DynamicImage;

use crate::{
    fs_utils::{prepare_dir, sigint_received, verbose, PREVIEWS_DIR},
    image_utils::{
        decode_image_bytes, decode_image_bytes_tagged_cpu, resize_to_fit,
    },
};

/// Maximum dimension of generated previews (pixels). Pack records carry
/// the dimension they were produced at, so changing this never serves
/// thumbnails encoded for another size.
pub const THUMB_MAX_DIM: u32 = 256;

/// Maximum dimension of the fullscreen preview tier (fit inside 1920x1080).
pub const SCREEN_MAX_DIM: u32 = 1920;

/// Quality of the JPEG previews stored in `.imagecropper-previews`.
pub const PREVIEW_JPEG_QUALITY: u8 = 80;

/// Signature used to detect that a source file changed: (size, mtime nanos).
pub fn source_signature(path: &Path) -> Option<(u64, u128)> {
    source_signature_ex(path).ok()
}

/// [`source_signature`] with the io error preserved — preview production
/// reports the reason to clients instead of collapsing it to `None`.
pub fn source_signature_ex(path: &Path) -> std::io::Result<(u64, u128)> {
    let meta = std::fs::metadata(path)?;
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok((meta.len(), nanos))
}

/// Fold (len, mtime-nanos) into the u64 signature shared by the manifest,
/// `files.pack`/`thumbs.pack` lookups and [`preview_sig_u64`].
pub fn sig_u64(len: u64, nanos: u128) -> u64 {
    len ^ nanos as u64 ^ (nanos >> 64) as u64
}

/// Compact u64 signature for protocol use (manifest, action preconditions).
/// Folds the 128-bit nanos into 64 bits; collision-resistant enough for
/// detecting changes to a given file.
pub fn preview_sig_u64(path: &Path) -> Option<u64> {
    let (len, nanos) = source_signature(path)?;
    Some(sig_u64(len, nanos))
}

/// Why preview production failed. Carried to clients so a corrupt or
/// unreadable source shows an error tile instead of an endless spinner,
/// and counted per `kind` in `/debugz`.
#[derive(Debug, Clone)]
pub struct PreviewFail {
    /// Stable category for counters/protocol: `"io"`, `"decode"`,
    /// `"encode"`, `"cancelled"`, `"internal"`.
    pub kind: &'static str,
    /// Folded signature of the source state that failed (0 = the file
    /// could not even be stat'ed, so no signature is known).
    pub sig: u64,
    /// Short human-readable reason, safe to show in the UI.
    pub msg: String,
}

impl PreviewFail {
    fn new(kind: &'static str, sig: u64, msg: impl Into<String>) -> Self {
        Self {
            kind,
            sig,
            msg: msg.into(),
        }
    }
    /// Filesystem failure (stat/read) — `sig` is the resolved signature
    /// when known, 0 otherwise.
    pub fn io(sig: u64, err: &std::io::Error) -> Self {
        Self::new("io", sig, err.to_string())
    }
    /// Failure touching the *source* file. `NotFound` means it was
    /// deleted outside ImageCropper (manual delete, another client, a
    /// dropped mount); `PermissionDenied` and `NotADirectory` mean the
    /// listing can see it but nobody can read it — for the client that
    /// is the same as gone, so all three become kind `"gone"` and the
    /// server drops the item from the listing (tombstone + patch) instead
    /// of reporting a retryable preview error that leaves a dead tile.
    /// Cache-file reads must keep [`Self::io`]: a missing cache entry is
    /// a miss, not a deleted source.
    pub fn source_io(sig: u64, err: &std::io::Error) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotADirectory => "gone",
            _ => "io",
        };
        Self::new(kind, sig, err.to_string())
    }
    /// Source bytes could not be decoded as an image.
    pub fn decode(sig: u64, err: impl std::fmt::Display) -> Self {
        Self::new("decode", sig, err.to_string())
    }
    /// JPEG re-encode of the thumbnail failed.
    pub fn encode(sig: u64) -> Self {
        Self::new("encode", sig, "jpeg encode failed")
    }
    /// Ctrl-C shutdown — production was asked to stop, not an error.
    pub fn cancelled() -> Self {
        Self::new("cancelled", 0, "interrupted")
    }
}

/// Deterministic preview file name for a source file: embeds the signature so
/// a stale preview simply has a different name.
pub fn preview_file_name(source_file_name: &str, len: u64, mtime_nanos: u128) -> String {
    format!("{source_file_name}.{len}-{mtime_nanos}.jpg")
}

/// Prefix matching all previews (any signature) of a given source file.
pub fn preview_file_prefix(source_file_name: &str) -> String {
    format!("{source_file_name}.")
}

/// Directory holding the previews for a source image.
pub fn previews_dir_for(source: &Path) -> PathBuf {
    source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PREVIEWS_DIR)
}

/// User-global cache root for large previews: `$XDG_CACHE_HOME/imagecropper`
/// or `~/.cache/imagecropper`. A global cache means fullscreen previews
/// work even when the source mount is read-only.
pub fn cache_root() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("imagecropper")
}

/// Stable per-file key for the global cache (avoids collisions between
/// same-named files in different directories).
fn source_key(source: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

/// Per-source directory for fullscreen previews: `f/<path-hash>/`.
pub fn screen_dir_for(source: &Path) -> PathBuf {
    cache_root()
        .join("f")
        .join(format!("{:016x}", source_key(source)))
}

/// Screen-preview path for a known signature — no I/O at all. `sig` is
/// the folded `(len, nanos)` signature `preview_sig_u64` returns.
fn screen_path_with_sig(source: &Path, sig: u64) -> PathBuf {
    screen_dir_for(source).join(format!("{sig}-f{SCREEN_MAX_DIM}.jpg"))
}

/// Expected fullscreen-preview path for the source's current signature.
/// Stale variants (different signature) are pruned on write.
pub fn screen_preview_path(source: &Path) -> Option<PathBuf> {
    let sig = preview_sig_u64(source)?;
    Some(screen_path_with_sig(source, sig))
}

/// Read the cached fullscreen preview, generating + persisting it if
/// missing. `sig_hint` carries a scan-time signature so the `stat` can
/// be skipped entirely on slow mounts. Returns (jpeg bytes, w, h).
pub fn produce_screen_jpeg(
    source: &Path,
    sig_hint: Option<u64>,
) -> Result<(Vec<u8>, u32, u32), PreviewFail> {
    // Ctrl-C: no new production — this runs on detached blocking tasks that
    // outlive the pools' cancel.
    if sigint_received() {
        return Err(PreviewFail::cancelled());
    }
    let sig = match sig_hint {
        Some(s) if s != 0 => s,
        _ => {
            let (len, nanos) = source_signature_ex(source)
                .map_err(|e| PreviewFail::source_io(0, &e))?;
            sig_u64(len, nanos)
        }
    };
    let path = screen_path_with_sig(source, sig);
    if !path.exists() {
        ensure_screen_preview(source, Some(sig));
    }
    let jpeg = std::fs::read(&path).map_err(|e| PreviewFail::io(sig, &e))?;
    // Dimensions from the bytes in hand — never re-open the file.
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&jpeg))
        .with_guessed_format()
        .map_err(|e| PreviewFail::decode(sig, e))?
        .into_dimensions()
        .unwrap_or((0, 0));
    Ok((jpeg, w, h))
}

/// Like [`produce_screen_jpeg`] but returns the decoded preview image —
/// the form the native hold-`f` fullscreen view needs.
pub fn produce_screen_image(
    source: &Path,
    sig_hint: Option<u64>,
) -> Result<DynamicImage, PreviewFail> {
    let (jpeg, _, _) = produce_screen_jpeg(source, sig_hint)?;
    decode_image_bytes(source, &jpeg)
        .map_err(|e| PreviewFail::decode(sig_hint.unwrap_or(0), e))
}

/// Cap for the global fullscreen-preview cache (`~/.cache/imagecropper/f`).
/// These files are pure regenerable cache — the safe thing to delete when
/// the disk fills — so the store is bounded LRU-by-mtime instead of
/// growing forever.
const SCREEN_CACHE_MAX_BYTES: u64 = 4 << 30; // 4 GiB
/// How often the opportunistic GC may run per process.
const SCREEN_GC_INTERVAL: Duration = Duration::from_secs(3600);
/// Per-path backoff after a failed ensure — without it a persistently
/// full disk burns a full decode+resize on every request for the same
/// file.
const SCREEN_FAIL_BACKOFF: Duration = Duration::from_secs(60);

/// Paths whose last fullscreen-preview persist failed, with when.
static SCREEN_FAIL: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Delete oldest fullscreen previews until the global cache fits
/// [`SCREEN_CACHE_MAX_BYTES`]. Returns bytes freed. Every file under
/// `f/` is a regenerable JPEG — nothing here is user data.
fn screen_cache_gc() -> u64 {
    screen_cache_gc_at(&cache_root().join("f"))
}

/// [`screen_cache_gc`] against an explicit cache root — split out so
/// tests don't have to mutate `XDG_CACHE_HOME` process-wide.
fn screen_cache_gc_at(root: &Path) -> u64 {
    let Ok(dirs) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total = 0u64;
    for d in dirs.flatten() {
        let Ok(entries) = std::fs::read_dir(d.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            files.push((e.path(), meta.len(), meta.modified().unwrap_or(UNIX_EPOCH)));
        }
    }
    if total <= SCREEN_CACHE_MAX_BYTES {
        return 0;
    }
    files.sort_by_key(|(_, _, t)| *t);
    let mut freed = 0u64;
    for (path, len, _) in files {
        if total <= SCREEN_CACHE_MAX_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            freed += len;
        }
    }
    // Drop per-source dirs left empty by the sweep.
    if let Ok(dirs) = std::fs::read_dir(root) {
        for d in dirs.flatten() {
            let _ = std::fs::remove_dir(d.path());
        }
    }
    freed
}

/// Opportunistic GC — at most once per [`SCREEN_GC_INTERVAL`] per process.
fn maybe_screen_gc() {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < SCREEN_GC_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    let freed = screen_cache_gc();
    if freed > 0 && verbose() {
        eprintln!(
            "screen cache gc: freed {:.1} GiB",
            freed as f64 / (1u64 << 30) as f64
        );
    }
}

/// Prune stale signature variants in the per-source cache dir and
/// write `jpeg` to `expected`. On ENOSPC the screen cache is GC'd and the
/// write retried once — it is regenerable data, the only safe cleanup.
/// Returns whether the preview is persisted afterwards.
fn persist_screen_bytes(source: &Path, expected: &Path, jpeg: &[u8], engine: &str) -> bool {
    let dir = screen_dir_for(source);
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    // Prune stale variants (older signatures of the same source).
    let expected_name = expected.file_name().unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_name() != expected_name {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let ok = match std::fs::write(expected, jpeg) {
        Ok(()) => true,
        Err(e) if crate::fs_utils::is_no_space(&e) => {
            screen_cache_gc();
            std::fs::write(expected, jpeg).is_ok()
        }
        Err(_) => false,
    };
    if ok && verbose() {
        eprintln!("screen preview generated [{engine}]: {}", source.display());
    }
    ok
}

/// JPEG-encode the resized `screen` image on CPU and persist it.
/// Returns whether the preview is persisted afterwards.
fn persist_screen_jpeg(source: &Path, expected: &Path, screen: &DynamicImage, engine: &str) -> bool {
    let mut jpeg = Vec::new();
    let ok = {
        let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
        screen.to_rgb8().write_with_encoder(encoder).is_ok()
    };
    ok && persist_screen_bytes(source, expected, &jpeg, engine)
}

/// Ensure a fullscreen-size preview exists in the global cache. `sig`
/// carries the caller's folded signature (`preview_sig_u64`) to skip a
/// stat on slow mounts; a stale hint just misses `expected.exists()`
/// and regenerates under the hinted name — the read below is current
/// content either way, and stale variants are pruned on write.
///
/// Failures (full disk, unreadable source, undecodable file) back off
/// per path for [`SCREEN_FAIL_BACKOFF`] so a persistently broken file
/// doesn't burn a decode+resize on every request.
pub(crate) fn ensure_screen_preview(source: &Path, sig: Option<u64>) {
    {
        let fails = SCREEN_FAIL.lock().unwrap();
        if fails.get(source).is_some_and(|t| t.elapsed() < SCREEN_FAIL_BACKOFF) {
            return;
        }
    }
    maybe_screen_gc();
    let ok = ensure_screen_preview_inner(source, sig);
    let mut fails = SCREEN_FAIL.lock().unwrap();
    if ok {
        fails.remove(source);
    } else {
        fails.insert(source.to_path_buf(), Instant::now());
    }
}

/// The ensure body — returns true when the preview exists afterwards
/// (already cached or freshly persisted).
fn ensure_screen_preview_inner(source: &Path, sig: Option<u64>) -> bool {
    let Some(sig) = sig.or_else(|| preview_sig_u64(source)) else {
        return false;
    };
    let expected = screen_path_with_sig(source, sig);
    if expected.exists() {
        return true;
    }
    let Some(bytes) = std::fs::read(source).ok() else {
        return false;
    };
    // GPU session path: decode+resize on the GPU — the full-res image
    // never materializes on the host.
    if let Some(session) = crate::gpujpeg::open(&bytes) {
        if let Some((jpeg, _, _)) =
            session.encode_resized(SCREEN_MAX_DIM, SCREEN_MAX_DIM, PREVIEW_JPEG_QUALITY.into())
        {
            return persist_screen_bytes(source, &expected, &jpeg, "gpu");
        }
    }
    let Some(img) = decode_image_bytes(source, &bytes).ok() else {
        return false;
    };
    let screen = resize_to_fit(img, SCREEN_MAX_DIM, SCREEN_MAX_DIM);
    persist_screen_jpeg(source, &expected, &screen, "cpu")
}

/// Remove stale previews of `source` inside `dir`, keeping `expected_name`.
/// Returns the cached preview path if an up-to-date preview exists.
pub fn find_cached_preview(dir: &Path, source: &Path, expected_name: &str) -> Option<PathBuf> {
    let source_name = source.file_name()?.to_string_lossy();
    let prefix = preview_file_prefix(&source_name);
    let mut cached = None;

    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".tmp-") {
            // Orphaned atomic-write scratch file (crash mid-persist).
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if !name.starts_with(&prefix) {
            continue;
        }
        if name == expected_name {
            cached = Some(entry.path());
        } else if name == format!("{expected_name}.sha256") {
            // Content-hash metadata of the current preview — keep it.
        } else {
            // Stale preview from an older version of the file.
            let _ = std::fs::remove_file(entry.path());
        }
    }
    cached
}

/// Remove every cached preview for a source file (e.g. after it was
/// deleted or moved away): the local `.imagecropper-previews` thumbnails
/// and the whole per-source fullscreen dir in the global cache.
pub fn remove_previews_for(source: &Path) {
    remove_previews_for_many([source]);
}

/// Batched form of [`remove_previews_for`]: purges cached previews for many
/// sources, scanning each shared `.imagecropper-previews` directory once
/// instead of once per source. Every source in a folder shares that
/// directory, so a bulk delete otherwise does O(n²) directory scans.
pub fn remove_previews_for_many<'a, I>(sources: I)
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut groups: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    for source in sources {
        let Some(source_name) = source.file_name() else {
            continue;
        };
        groups
            .entry(previews_dir_for(source))
            .or_default()
            .insert(source_name.to_string_lossy().into_owned());
        // Global fullscreen cache: `f/<path-hash>/` holds only this file's
        // signature variants, so the whole dir goes.
        let _ = std::fs::remove_dir_all(screen_dir_for(source));
    }
    for (dir, names) in groups {
        // Pack records: one tombstone per source name.
        if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
            let mut pack = pack.lock().unwrap();
            for name in &names {
                pack.remove(name);
            }
        }
        // Legacy flat previews from before the pack format (the pack's
        // own open-sweep already removes them, but only for dirs that
        // were opened — sweep here too for certainty).
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            if preview_belongs_to(&file_name.to_string_lossy(), &names) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        // Nothing left at all → the pack file itself goes too, leaving a
        // completely clean `.imagecropper-previews` (or empty) dir.
        crate::fs_utils::pack::remove_pack_if_empty(&dir);
    }
}

/// Does `preview_name` (`<source>.<len>-<mtime>.jpg`) belong to one of
/// `sources`? Parsing the source out of the name keeps the scan O(1) per
/// entry; the prefix fallback covers files from older preview formats.
fn preview_belongs_to(preview_name: &str, sources: &HashSet<String>) -> bool {
    let parsed = preview_name
        .strip_suffix(".jpg")
        .and_then(|stem| stem.rsplit_once('.'))
        .and_then(|(source, sig)| {
            let (len, nanos) = sig.split_once('-')?;
            let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            (numeric(len) && numeric(nanos)).then_some(source)
        });
    match parsed {
        Some(source) => sources.contains(source),
        None => sources.iter().any(|source| {
            preview_name
                .strip_prefix(source.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
        }),
    }
}

/// Bytes the read stage of a thumbnail pipeline hands to the compute
/// stage — the payload of the bounded `mid` queue in
/// [`ThumbPipeline`](crate::fs_utils::pipeline::ThumbPipeline).
pub enum ReadJob {
    /// Pack-hit thumbnail jpeg — decode it (image path) or serve it
    /// as-is (bytes path). No source read happened.
    Cached { jpeg: Vec<u8> },
    /// Freshly read source bytes on a cache miss — the compute stage
    /// runs the full decode/resize/encode/persist. `sig` is the packed
    /// signature (for error labels); `len`/`nanos` key the pack record.
    Source {
        bytes: Vec<u8>,
        len: u64,
        nanos: u128,
        sig: u64,
    },
}

impl ReadJob {
    /// Memory cost of this job for the byte-bounded mid queue.
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Cached { jpeg } => jpeg.len(),
            Self::Source { bytes, .. } => bytes.len(),
        }
    }
}

/// Read stage of the thumbnail pipeline — everything I/O-bound: pack
/// lookups, the signature stat and the source `fs::read`. Produces a
/// [`ReadJob`] the compute stage turns into a result; cache hits come
/// back `Cached` so the pipeline can skip the source read entirely.
///
/// `sig_hint` carries the scan-time signature: a pack hit then needs no
/// `stat` at all — on slow mounts that is one less round-trip per file.
/// A stale hint simply misses and the normal signature path runs.
///
/// Cache-hit I/O on slow/remote filesystems is kept minimal: at most one
/// `stat` (signature) plus one `read` of the pack record — no directory
/// scan, no touching the source.
pub fn preview_read(path: &Path, sig_hint: Option<u64>) -> Result<ReadJob, PreviewFail> {
    if sigint_received() {
        return Err(PreviewFail::cancelled());
    }
    let source_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| PreviewFail::new("io", 0, "invalid path"))?;
    let dir = previews_dir_for(path);

    // Fast path: caller knows the scan-time signature — a pack hit needs
    // no stat at all (one less round-trip per file on slow mounts).
    if let Some(sig) = sig_hint.filter(|s| *s != 0) {
        if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
            let mut pack = pack.lock().unwrap();
            if let Some((jpeg, _hash)) =
                pack.lookup_sig(&source_name, sig, THUMB_MAX_DIM as u16)
            {
                if jpeg_plausible(&jpeg) {
                    if verbose() {
                        eprintln!("preview cache hit [pack-sig]: {}", path.display());
                    }
                    return Ok(ReadJob::Cached { jpeg });
                }
                pack.remove(&source_name);
            }
        }
    }

    let (len, nanos) = source_signature_ex(path)
        .map_err(|e| PreviewFail::source_io(0, &e))?;
    let sig = sig_u64(len, nanos);

    if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
        let mut pack = pack.lock().unwrap();
        if let Some((jpeg, _hash)) =
            pack.lookup(&source_name, len, nanos, THUMB_MAX_DIM as u16)
        {
            if jpeg_plausible(&jpeg) {
                if verbose() {
                    eprintln!("preview cache hit [pack]: {}", path.display());
                }
                return Ok(ReadJob::Cached { jpeg });
            }
            // Corrupt/truncated record (killed mid-write, disk damage) —
            // serving it would wedge clients into decode-fail loops.
            // Tombstone it and fall through to regeneration below.
            if verbose() {
                eprintln!("preview cache corrupt [pack]: {}", path.display());
            }
            pack.remove(&source_name);
        }
    }

    let bytes = std::fs::read(path).map_err(|e| PreviewFail::source_io(sig, &e))?;
    Ok(ReadJob::Source {
        bytes,
        len,
        nanos,
        sig,
    })
}

/// Compute stage for the image form — [`DynamicImage`] thumbnails for the
/// native album UI. `Cached` jpegs decode on CPU (≤256px; a GPU
/// round-trip costs more than zune at this size); `Source` bytes run the
/// full produce: decode, resize, persist.
pub fn preview_compute_image(path: &Path, job: ReadJob) -> Result<DynamicImage, PreviewFail> {
    match job {
        ReadJob::Cached { jpeg } => {
            let (decoded, engine) = decode_image_bytes_tagged_cpu(path, &jpeg);
            match decoded {
                Ok(img) => {
                    if verbose() {
                        eprintln!("preview cache hit [{engine}]: {}", path.display());
                    }
                    Ok(img)
                }
                Err(_) => {
                    // Corrupt record — tombstone so it can never wedge
                    // us, then re-run the read stage for the source.
                    if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
                    {
                        if let Some(pack) =
                            crate::fs_utils::pack::pack_for(&previews_dir_for(path))
                        {
                            pack.lock().unwrap().remove(&name);
                        }
                    }
                    preview_compute_image(path, preview_read(path, None)?)
                }
            }
        }
        ReadJob::Source {
            bytes,
            len,
            nanos,
            sig,
        } => {
            let source_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| PreviewFail::new("io", 0, "invalid path"))?;
            let hash = sha256_hex(&bytes);
            // Restart-marker JPEGs go entirely through the GPU session —
            // decode + the resize run on the GPU and the full-res image
            // never reaches the host. A session whose resample buffers
            // exceed the device limits falls through to the CPU decode.
            let (thumb, engine) = match crate::gpujpeg::open(&bytes)
                .and_then(|s| s.resize_image(THUMB_MAX_DIM, THUMB_MAX_DIM))
            {
                Some(img) => (img, "gpu"),
                None => {
                    let image = decode_image_bytes(path, &bytes)
                        .map_err(|e| PreviewFail::decode(sig, e))?;
                    drop(bytes);
                    (resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM), "cpu")
                }
            };
            persist_preview(path, len, nanos, &source_name, &thumb, &hash);
            if verbose() {
                eprintln!("preview generated [{engine}]: {}", path.display());
            }
            Ok(thumb)
        }
    }
}

/// Compute stage for the bytes form — `(jpeg, w, h)` for the web server
/// and headless pregeneration. `Cached` bytes are served as-is with
/// `(0, 0)` dims — pipeline callers that need natural dims resolve them
/// in the read stage (hint map or header sniff) and never reach this
/// arm. `Source` bytes run decode+resize+encode+persist, GPU-first.
pub fn preview_compute_bytes(
    path: &Path,
    job: ReadJob,
) -> Result<(Vec<u8>, u32, u32), PreviewFail> {
    match job {
        ReadJob::Cached { jpeg } => Ok((jpeg, 0, 0)),
        ReadJob::Source {
            bytes,
            len,
            nanos,
            sig,
        } => {
            let source_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .ok_or_else(|| PreviewFail::new("io", 0, "invalid path"))?;
            // The source is already in memory — the content hash is
            // nearly free and becomes the preview's authoritative
            // metadata.
            let hash = sha256_hex(&bytes);
            if let Some(session) = crate::gpujpeg::open(&bytes) {
                let (w, h) = (session.width, session.height);
                // Encode buffers that exceed the device limits → CPU path.
                if let Some((jpeg, _, _)) = session.encode_resized(
                    THUMB_MAX_DIM,
                    THUMB_MAX_DIM,
                    PREVIEW_JPEG_QUALITY.into(),
                ) {
                    persist_preview_bytes(path, len, nanos, &source_name, &jpeg, &hash);
                    // The fullscreen preview is generated by the store's
                    // deferred screen-fill — never inline on the hot path.
                    if verbose() {
                        eprintln!("preview generated [gpu]: {}", path.display());
                    }
                    return Ok((jpeg, w, h));
                }
            }

            let image =
                decode_image_bytes(path, &bytes).map_err(|e| PreviewFail::decode(sig, e))?;
            drop(bytes);
            let (w, h) = (image.width(), image.height());
            let thumb = resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM);
            let mut jpeg = Vec::new();
            {
                let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
                let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                    writer,
                    PREVIEW_JPEG_QUALITY,
                );
                thumb
                    .to_rgb8()
                    .write_with_encoder(encoder)
                    .map_err(|_| PreviewFail::encode(sig))?;
            }
            persist_preview_bytes(path, len, nanos, &source_name, &jpeg, &hash);
            if verbose() {
                eprintln!("preview generated [cpu]: {}", path.display());
            }
            Ok((jpeg, w, h))
        }
    }
}

/// Load the cached preview or generate, persist and return a fresh one.
/// Returns the decoded preview image (≤ `THUMB_MAX_DIM`) — the sequential
/// composition of [`preview_read`] + [`preview_compute_image`].
///
/// `sig_hint` carries the scan-time signature: a pack hit then needs no
/// `stat` at all — on slow mounts that is one less round-trip per file.
/// A stale hint simply misses and the normal signature path runs.
pub fn produce_preview_image(
    path: &Path,
    sig_hint: Option<u64>,
) -> Result<DynamicImage, PreviewFail> {
    preview_compute_image(path, preview_read(path, sig_hint)?)
}

/// Like [`produce_preview_image`] but returns the persisted JPEG bytes plus
/// the source image's natural dimensions — the form the web server needs.
/// The sequential composition of [`preview_read`] +
/// [`preview_compute_bytes`].
///
/// `dims_hint` (w,h already known — e.g. from the manifest) lets a cache
/// hit skip opening the source file entirely; without it the source's
/// header is read for dimensions, same as before. `sig_hint` carries
/// the scan-time signature: a pack hit then skips the `stat` too —
/// callers that pass `Some((0, 0))` as dims (e.g. headless pregeneration,
/// which discards dimensions) get a pure pack read with zero source I/O.
pub fn produce_preview_bytes(
    path: &Path,
    dims_hint: Option<(u32, u32)>,
    sig_hint: Option<u64>,
) -> Result<(Vec<u8>, u32, u32), PreviewFail> {
    match preview_read(path, sig_hint)? {
        ReadJob::Cached { jpeg } => {
            let dims = dims_hint
                .or_else(|| crate::image_utils::display_dims(path))
                .unwrap_or((0, 0));
            Ok((jpeg, dims.0, dims.1))
        }
        job @ ReadJob::Source { .. } => preview_compute_bytes(path, job),
    }
}

/// sha256 of a source file's bytes — the authoritative "did this file
/// change?" answer. Signatures (len+mtime) are cheap but unstable on
/// network mounts; content is not.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// sha256 of the file's current content (full read — call only when a
/// cheap signature check could not decide).
pub fn source_content_hash(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| sha256_hex(&b))
}

/// Short-lived content-hash cache. Mtime churn makes every signature check
/// fall back to hashing; a bulk delete would otherwise read every source
/// file in full, one after another, on a slow mount.
static HASH_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, String, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// [`source_content_hash`] with a short TTL, keyed by (path, length): a
/// length change always invalidates, a pure mtime change (the network-mount
/// case) reuses the hash for up to 30 s.
pub fn source_content_hash_cached(path: &Path) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    {
        let cache = HASH_CACHE.lock().unwrap();
        if let Some((cached_len, hash, at)) = cache.get(path) {
            if *cached_len == len && at.elapsed() < std::time::Duration::from_secs(30) {
                return Some(hash.clone());
            }
        }
    }
    let hash = source_content_hash(path)?;
    let mut cache = HASH_CACHE.lock().unwrap();
    if cache.len() > 4096 {
        cache.clear();
    }
    cache.insert(
        path.to_path_buf(),
        (len, hash.clone(), std::time::Instant::now()),
    );
    Some(hash)
}

/// Record `hash` as the content hash for `source`'s current signature.
/// Used when a verification found no recorded hash to compare against, so
/// the next check is exact instead of guessing.
pub fn write_stored_hash_for(source: &Path, hash: &str) {
    let Some((len, nanos)) = source_signature(source) else {
        return;
    };
    let Some(name) = source.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    if let Some(pack) = crate::fs_utils::pack::pack_for(&previews_dir_for(source)) {
        pack.lock().unwrap().put_hash(&name, len, nanos, hash);
    }
}

/// The recorded content hash of `source` from any of its persisted
/// previews (signature variants come and go; the newest one wins).
/// Falls back to scanning legacy `.sha256` sidecars for caches written
/// before the pack format existed.
pub fn stored_content_hash(source: &Path) -> Option<String> {
    let name = source.file_name()?.to_string_lossy().into_owned();
    let dir = previews_dir_for(source);
    if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
        if let Some(hash) = pack.lock().unwrap().stored_hash(&name) {
            return Some(hash);
        }
    }
    let prefix = preview_file_prefix(&name);
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        if !fname.starts_with(&prefix) || !fname.ends_with(".sha256") {
            continue;
        }
        let Ok(hash) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let hash = hash.trim().to_string();
        if hash.is_empty() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, hash));
        }
    }
    best.map(|(_, h)| h)
}

/// Test helper: the persisted thumbnail bytes for `source`'s current
/// signature, if the directory pack holds them.
#[doc(hidden)]
pub fn persisted_thumb_for_test(source: &Path) -> Option<Vec<u8>> {
    let (len, nanos) = source_signature(source)?;
    let name = source.file_name()?.to_string_lossy().into_owned();
    let pack = crate::fs_utils::pack::pack_for(&previews_dir_for(source))?;
    let out = pack
        .lock()
        .unwrap()
        .lookup(&name, len, nanos, THUMB_MAX_DIM as u16)
        .map(|(jpeg, _)| jpeg);
    out
}

/// Test helper: absolute offset of `source`'s jpeg inside `thumbs.pack` —
/// tests overwrite bytes there to simulate corruption.
#[doc(hidden)]
pub fn persisted_thumb_offset_for_test(source: &Path) -> Option<u64> {
    let (len, nanos) = source_signature(source)?;
    let name = source.file_name()?.to_string_lossy().into_owned();
    let pack = crate::fs_utils::pack::pack_for(&previews_dir_for(source))?;
    let out = pack.lock().unwrap().jpeg_offset(&name, len, nanos);
    out
}

/// Cheap structural check on cached preview bytes: SOI + EOI markers and a
/// sane minimum length. Anything failing this is corrupt (truncated write,
/// disk damage) and must be regenerated rather than served.
fn jpeg_plausible(bytes: &[u8]) -> bool {
    bytes.len() >= 32
        && bytes[0] == 0xFF
        && bytes[1] == 0xD8
        && bytes[bytes.len() - 2] == 0xFF
        && bytes[bytes.len() - 1] == 0xD9
}

/// Delete the persisted thumbnail for `path`, forcing the next produce to
/// regenerate. Called when a client reports the cached preview as
/// undecodable — the record is presumed corrupt, so the whole name (all
/// signature variants plus the recorded hash) is tombstoned.
pub fn invalidate_preview_bytes(path: &Path) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let dir = previews_dir_for(path);
    if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
        pack.lock().unwrap().remove(&name);
    }
    // Legacy flat file from before the pack format.
    if let Some((len, nanos)) = source_signature(path) {
        let expected = dir.join(preview_file_name(&name, len, nanos));
        let _ = std::fs::remove_file(&expected);
        let _ = std::fs::remove_file(format!("{}.sha256", expected.display()));
    }
}

fn persist_preview(
    source: &Path,
    len: u64,
    nanos: u128,
    source_name: &str,
    thumb: &DynamicImage,
    hash: &str,
) {
    let mut jpeg = Vec::new();
    let ok = {
        let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
        thumb.to_rgb8().write_with_encoder(encoder).is_ok()
    };
    if ok {
        persist_preview_bytes(source, len, nanos, source_name, &jpeg, hash);
    }
}

/// Append the thumbnail record to the directory's pack. A torn append can
/// only corrupt the tail, which replay truncates — no tmp/rename dance
/// needed per record, and concurrent producers serialize on the pack lock.
fn persist_preview_bytes(
    source: &Path,
    len: u64,
    nanos: u128,
    source_name: &str,
    jpeg: &[u8],
    hash: &str,
) {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let Ok(preview_dir) = prepare_dir(parent, PREVIEWS_DIR) else {
        return;
    };
    if let Some(pack) = crate::fs_utils::pack::pack_for(&preview_dir) {
        pack.lock()
            .unwrap()
            .put(source_name, len, nanos, hash, THUMB_MAX_DIM as u16, jpeg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen cache is pure regenerable data: over-cap GC must
    /// delete oldest-first until the total fits, and empty per-source
    /// dirs must be dropped.
    #[test]
    fn screen_cache_gc_evicts_oldest_until_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("f");
        let d1 = root.join("aaaa");
        let d2 = root.join("bbbb");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();

        // Sparse files: logical size counts toward the cap, no real
        // blocks are allocated.
        let big = SCREEN_CACHE_MAX_BYTES + (1 << 30);
        let old = d1.join("old.jpg");
        let new = d2.join("new.jpg");
        std::fs::File::create(&old).unwrap().set_len(big).unwrap();
        std::fs::File::create(&new).unwrap().set_len(1 << 20).unwrap();
        // Age `old` well past `new`.
        let old_f = std::fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap();
        old_f
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();

        let freed = screen_cache_gc_at(&root);
        assert!(freed >= big, "freed {freed}");
        assert!(!old.exists(), "oldest file must be evicted");
        assert!(new.exists(), "younger file must survive");
        assert!(!d1.exists(), "emptied dir must be removed");
        assert!(d2.exists());
    }

    /// Under the cap nothing is touched.
    #[test]
    fn screen_cache_gc_noop_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("f");
        let d = root.join("cccc");
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("keep.jpg");
        std::fs::write(&f, b"jpeg").unwrap();

        assert_eq!(screen_cache_gc_at(&root), 0);
        assert!(f.exists());
    }
}
