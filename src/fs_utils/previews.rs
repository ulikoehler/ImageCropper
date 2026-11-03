//! Persistent thumbnail cache in `.imagecropper-previews`.
//!
//! Preview file names embed the source file's size and modification time, so
//! a changed source automatically produces a fresh preview name and stale
//! previews are cleaned up. Shared by the album UI (`Thumbnailer`) and the
//! web server (`PreviewStore`).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use image::DynamicImage;

use crate::{
    fs_utils::{prepare_dir, sigint_received, verbose, PREVIEWS_DIR},
    image_utils::{decode_image_bytes, resize_to_fit},
};

/// Maximum dimension of generated previews (pixels).
pub const THUMB_MAX_DIM: u32 = 512;

/// Maximum dimension of the fullscreen preview tier (fit inside 1920x1080).
pub const SCREEN_MAX_DIM: u32 = 1920;

/// Quality of the JPEG previews stored in `.imagecropper-previews`.
pub const PREVIEW_JPEG_QUALITY: u8 = 80;

/// Signature used to detect that a source file changed: (size, mtime nanos).
pub fn source_signature(path: &Path) -> Option<(u64, u128)> {
    let meta = std::fs::metadata(path).ok()?;
    let nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((meta.len(), nanos))
}

/// Compact u64 signature for protocol use (manifest, action preconditions).
/// Folds the 128-bit nanos into 64 bits; collision-resistant enough for
/// detecting changes to a given file.
pub fn preview_sig_u64(path: &Path) -> Option<u64> {
    let (len, nanos) = source_signature(path)?;
    Some(len ^ nanos as u64 ^ (nanos >> 64) as u64)
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

/// Screen-preview path for a known signature — no I/O at all.
fn screen_path_with_sig(source: &Path, len: u64, nanos: u128) -> PathBuf {
    let sig = len ^ nanos as u64 ^ (nanos >> 64) as u64;
    screen_dir_for(source).join(format!("{sig}-f{SCREEN_MAX_DIM}.jpg"))
}

/// Expected fullscreen-preview path for the source's current signature.
/// Stale variants (different signature) are pruned on write.
pub fn screen_preview_path(source: &Path) -> Option<PathBuf> {
    let (len, nanos) = source_signature(source)?;
    Some(screen_path_with_sig(source, len, nanos))
}

/// Read the cached fullscreen preview, generating + persisting it if
/// missing. Returns (jpeg bytes, preview w, preview h).
pub fn produce_screen_jpeg(source: &Path) -> Option<(Vec<u8>, u32, u32)> {
    // Ctrl-C: no new production — this runs on detached blocking tasks that
    // outlive the pools' cancel.
    if sigint_received() {
        return None;
    }
    let (len, nanos) = source_signature(source)?;
    let path = screen_path_with_sig(source, len, nanos);
    if !path.exists() {
        ensure_screen_preview(source, None, Some((len, nanos)));
    }
    let jpeg = std::fs::read(&path).ok()?;
    // Dimensions from the bytes in hand — never re-open the file.
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&jpeg))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .unwrap_or((0, 0));
    Some((jpeg, w, h))
}

/// Like [`produce_screen_jpeg`] but returns the decoded preview image —
/// the form the native hold-`f` fullscreen view needs.
pub fn produce_screen_image(source: &Path) -> Option<DynamicImage> {
    let (jpeg, _, _) = produce_screen_jpeg(source)?;
    decode_image_bytes(source, &jpeg).ok()
}

/// JPEG-encode the resized `screen` image, prune stale signature
/// variants in the per-source cache dir, and write it to `expected`.
fn persist_screen_jpeg(source: &Path, expected: &Path, screen: &DynamicImage) {
    let mut jpeg = Vec::new();
    {
        let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
        if screen.to_rgb8().write_with_encoder(encoder).is_err() {
            return;
        }
    }
    let dir = screen_dir_for(source);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
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
    if std::fs::write(expected, &jpeg).is_ok() && verbose() {
        eprintln!("screen preview generated: {}", source.display());
    }
}

/// GPU-session variant of [`ensure_screen_preview`]: resizes straight
/// off the session's decoded buffer — the full-res pixels never
/// round-trip through the host.
fn ensure_screen_preview_gpu(
    source: &Path,
    session: &crate::gpujpeg::GpuJpeg,
    sig: Option<(u64, u128)>,
) {
    let Some((len, nanos)) = sig.or_else(|| source_signature(source)) else {
        return;
    };
    let expected = screen_path_with_sig(source, len, nanos);
    if expected.exists() {
        return;
    }
    if let Some(screen) = session.resize_image(SCREEN_MAX_DIM, SCREEN_MAX_DIM) {
        persist_screen_jpeg(source, &expected, &screen);
    }
}

/// Ensure a fullscreen-size preview exists in the global cache. `decoded`
/// may pass an already-decoded source image to avoid a second decode;
/// `sig` carries the caller's `source_signature` to skip a second stat.
fn ensure_screen_preview(source: &Path, decoded: Option<&DynamicImage>, sig: Option<(u64, u128)>) {
    let Some((len, nanos)) = sig.or_else(|| source_signature(source)) else {
        return;
    };
    let expected = screen_path_with_sig(source, len, nanos);
    if expected.exists() {
        return;
    }
    if let Some(img) = decoded {
        let screen = resize_to_fit(img.clone(), SCREEN_MAX_DIM, SCREEN_MAX_DIM);
        persist_screen_jpeg(source, &expected, &screen);
        return;
    }
    let Some(bytes) = std::fs::read(source).ok() else {
        return;
    };
    // GPU session path: decode+resize on the GPU — the full-res image
    // never materializes on the host.
    if let Some(session) = crate::gpujpeg::open(&bytes) {
        if let Some(screen) = session.resize_image(SCREEN_MAX_DIM, SCREEN_MAX_DIM) {
            persist_screen_jpeg(source, &expected, &screen);
            return;
        }
    }
    let Some(img) = decode_image_bytes(source, &bytes).ok() else {
        return;
    };
    let screen = resize_to_fit(img, SCREEN_MAX_DIM, SCREEN_MAX_DIM);
    persist_screen_jpeg(source, &expected, &screen);
}

/// Remove stale previews of `source` inside `dir`, keeping `expected_name`.
/// Returns the cached preview path if an up-to-date preview exists.
pub fn find_cached_preview(dir: &Path, source: &Path, expected_name: &str) -> Option<PathBuf> {
    let source_name = source.file_name()?.to_string_lossy();
    let prefix = preview_file_prefix(&source_name);
    let mut cached = None;

    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        if name == expected_name {
            cached = Some(entry.path());
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
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            if preview_belongs_to(&file_name.to_string_lossy(), &names) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
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

/// Load the cached preview or generate, persist and return a fresh one.
/// Returns the decoded preview image (≤ `THUMB_MAX_DIM`).
///
/// Cache-hit I/O on slow/remote filesystems is kept minimal: one
/// `stat` (signature) plus one `read` of the deterministic preview
/// path — no directory scan, no second stat, no touching the source.
pub fn produce_preview_image(path: &Path) -> Option<DynamicImage> {
    let (len, nanos) = source_signature(path)?;
    let source_name = path.file_name()?.to_string_lossy().into_owned();
    let expected_name = preview_file_name(&source_name, len, nanos);
    let expected = previews_dir_for(path).join(&expected_name);

    // Fast path: the deterministic path exists → read it straight away.
    if let Ok(bytes) = std::fs::read(&expected) {
        if let Ok(img) = decode_image_bytes(&expected, &bytes) {
            ensure_screen_preview(path, None, Some((len, nanos)));
            if verbose() {
                eprintln!("preview cache hit: {}", path.display());
            }
            return Some(img);
        }
        // Corrupt preview — regenerate.
        let _ = std::fs::remove_file(&expected);
    }

    // Slow path: decode the source, downscale, persist. Restart-marker
    // JPEGs go entirely through the GPU session — decode + both resizes
    // run on the GPU and the full-res image never reaches the host.
    let bytes = std::fs::read(path).ok()?;
    let thumb = match crate::gpujpeg::open(&bytes) {
        Some(session) => {
            ensure_screen_preview_gpu(path, &session, Some((len, nanos)));
            session.resize_image(THUMB_MAX_DIM, THUMB_MAX_DIM)?
        }
        None => {
            let image = decode_image_bytes(path, &bytes).ok()?;
            drop(bytes);
            ensure_screen_preview(path, Some(&image), Some((len, nanos)));
            resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM)
        }
    };
    persist_preview(path, &expected_name, &thumb);
    if verbose() {
        eprintln!("preview generated: {}", path.display());
    }
    Some(thumb)
}

/// Like [`produce_preview_image`] but returns the persisted JPEG bytes plus
/// the source image's natural dimensions — the form the web server needs.
/// Also ensures the fullscreen-size preview exists in `~/.cache`, so
/// hold-`f` previews are preloaded alongside thumbnails.
///
/// `dims_hint` (w,h already known — e.g. from the manifest) lets a cache
/// hit skip opening the source file entirely; without it the source's
/// header is read for dimensions, same as before.
pub fn produce_preview_bytes(
    path: &Path,
    dims_hint: Option<(u32, u32)>,
) -> Option<(Vec<u8>, u32, u32)> {
    if sigint_received() {
        return None;
    }
    let (len, nanos) = source_signature(path)?;
    let source_name = path.file_name()?.to_string_lossy().into_owned();
    let expected_name = preview_file_name(&source_name, len, nanos);
    let expected = previews_dir_for(path).join(&expected_name);

    if let Ok(bytes) = std::fs::read(&expected) {
        ensure_screen_preview(path, None, Some((len, nanos)));
        let dims = dims_hint
            .or_else(|| image::image_dimensions(path).ok())
            .unwrap_or((0, 0));
        if verbose() {
            eprintln!("preview cache hit: {}", path.display());
        }
        return Some((bytes, dims.0, dims.1));
    }

    let bytes = std::fs::read(path).ok()?;
    let (thumb, w, h) = match crate::gpujpeg::open(&bytes) {
        Some(session) => {
            let (w, h) = (session.width, session.height);
            ensure_screen_preview_gpu(path, &session, Some((len, nanos)));
            let Some(t) = session.resize_image(THUMB_MAX_DIM, THUMB_MAX_DIM) else {
                return None;
            };
            (t, w, h)
        }
        None => {
            let image = decode_image_bytes(path, &bytes).ok()?;
            drop(bytes);
            let (w, h) = (image.width(), image.height());
            ensure_screen_preview(path, Some(&image), Some((len, nanos)));
            (
                resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM),
                w,
                h,
            )
        }
    };

    let mut jpeg = Vec::new();
    {
        let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
        thumb.to_rgb8().write_with_encoder(encoder).ok()?;
    }
    persist_preview_bytes(path, &expected_name, &jpeg);
    if verbose() {
        eprintln!("preview generated: {}", path.display());
    }
    Some((jpeg, w, h))
}

/// Prune stale signature variants of `source` in `dir`, keeping
/// `expected_name`. Runs on the write path only — cache hits never scan.
fn prune_stale_previews(dir: &Path, source: &Path, expected_name: &str) {
    // find_cached_preview deletes every prefix-matching file that isn't
    // expected_name as it scans — exactly the pruning we need here.
    let _ = find_cached_preview(dir, source, expected_name);
}

fn persist_preview(source: &Path, expected_name: &str, thumb: &DynamicImage) {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(preview_dir) = prepare_dir(parent, PREVIEWS_DIR) {
        prune_stale_previews(&preview_dir, source, expected_name);
        let preview_path = preview_dir.join(expected_name);
        if let Ok(file) = std::fs::File::create(&preview_path) {
            let writer = std::io::BufWriter::new(file);
            let rgb = thumb.to_rgb8();
            let encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
            let _ = rgb.write_with_encoder(encoder);
        }
    }
}

fn persist_preview_bytes(source: &Path, expected_name: &str, jpeg: &[u8]) {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(preview_dir) = prepare_dir(parent, PREVIEWS_DIR) {
        prune_stale_previews(&preview_dir, source, expected_name);
        let _ = std::fs::write(preview_dir.join(expected_name), jpeg);
    }
}
