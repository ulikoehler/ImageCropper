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
    time::UNIX_EPOCH,
};

use image::DynamicImage;

use crate::{
    fs_utils::{prepare_dir, sigint_received, verbose, PREVIEWS_DIR},
    image_utils::{decode_image_bytes, decode_image_bytes_tagged, resize_to_fit},
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

/// Prune stale signature variants in the per-source cache dir and
/// write `jpeg` to `expected`.
fn persist_screen_bytes(source: &Path, expected: &Path, jpeg: &[u8], engine: &str) {
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
    if std::fs::write(expected, jpeg).is_ok() && verbose() {
        eprintln!("screen preview generated [{engine}]: {}", source.display());
    }
}

/// JPEG-encode the resized `screen` image on CPU and persist it.
fn persist_screen_jpeg(source: &Path, expected: &Path, screen: &DynamicImage, engine: &str) {
    let mut jpeg = Vec::new();
    {
        let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
        if screen.to_rgb8().write_with_encoder(encoder).is_err() {
            return;
        }
    }
    persist_screen_bytes(source, expected, &jpeg, engine);
}

/// Submit the screen-size encode chain off the session's decoded
/// buffer — returns the pending job + target path so the caller can
/// overlap it with other GPU work before `finish`. The full-res
/// pixels never round-trip through the host.
fn screen_encode_start(
    source: &Path,
    session: &crate::gpujpeg::GpuJpeg,
    sig: Option<(u64, u128)>,
) -> Option<(PathBuf, crate::gpujpeg::EncodeJob)> {
    let (len, nanos) = sig.or_else(|| source_signature(source))?;
    let expected = screen_path_with_sig(source, len, nanos);
    if expected.exists() {
        return None;
    }
    let job = session.encode_resized_start(
        SCREEN_MAX_DIM,
        SCREEN_MAX_DIM,
        PREVIEW_JPEG_QUALITY.into(),
    )?;
    Some((expected, job))
}

/// Resolve a [`screen_encode_start`] job and persist the JPEG.
fn screen_encode_finish(job: Option<(PathBuf, crate::gpujpeg::EncodeJob)>, source: &Path) {
    if let Some((expected, job)) = job {
        if let Some((jpeg, _, _)) = job.finish() {
            persist_screen_bytes(source, &expected, &jpeg, "gpu");
        }
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
    let pending = screen_encode_start(source, session, sig);
    screen_encode_finish(pending, source);
}

/// Ensure a fullscreen-size preview exists in the global cache. `decoded`
/// may pass an already-decoded source image to avoid a second decode;
/// `sig` carries the caller's `source_signature` to skip a second stat.
pub(crate) fn ensure_screen_preview(
    source: &Path,
    decoded: Option<&DynamicImage>,
    sig: Option<(u64, u128)>,
) {
    let Some((len, nanos)) = sig.or_else(|| source_signature(source)) else {
        return;
    };
    let expected = screen_path_with_sig(source, len, nanos);
    if expected.exists() {
        return;
    }
    if let Some(img) = decoded {
        let screen = resize_to_fit(img.clone(), SCREEN_MAX_DIM, SCREEN_MAX_DIM);
        persist_screen_jpeg(source, &expected, &screen, "cpu");
        return;
    }
    let Some(bytes) = std::fs::read(source).ok() else {
        return;
    };
    // GPU session path: decode+resize on the GPU — the full-res image
    // never materializes on the host.
    if let Some(session) = crate::gpujpeg::open(&bytes) {
        if let Some((jpeg, _, _)) =
            session.encode_resized(SCREEN_MAX_DIM, SCREEN_MAX_DIM, PREVIEW_JPEG_QUALITY.into())
        {
            persist_screen_bytes(source, &expected, &jpeg, "gpu");
            return;
        }
    }
    let Some(img) = decode_image_bytes(source, &bytes).ok() else {
        return;
    };
    let screen = resize_to_fit(img, SCREEN_MAX_DIM, SCREEN_MAX_DIM);
    persist_screen_jpeg(source, &expected, &screen, "cpu");
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

/// Load the cached preview or generate, persist and return a fresh one.
/// Returns the decoded preview image (≤ `THUMB_MAX_DIM`).
///
/// Cache-hit I/O on slow/remote filesystems is kept minimal: one
/// `stat` (signature) plus one `read` of the deterministic preview
/// path — no directory scan, no second stat, no touching the source.
pub fn produce_preview_image(path: &Path) -> Option<DynamicImage> {
    let (len, nanos) = source_signature(path)?;
    let source_name = path.file_name()?.to_string_lossy().into_owned();
    let dir = previews_dir_for(path);

    // Fast path: the pack has a current-signature thumbnail for this file.
    if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
        let mut pack = pack.lock().unwrap();
        if let Some((jpeg, _)) = pack.lookup(&source_name, len, nanos, THUMB_MAX_DIM as u16) {
            let (decoded, engine) = decode_image_bytes_tagged(path, &jpeg);
            if let Ok(img) = decoded {
                if verbose() {
                    eprintln!("preview cache hit [{engine}]: {}", path.display());
                }
                return Some(img);
            }
            // Corrupt record — tombstone so it can never wedge us.
            pack.remove(&source_name);
        }
    }

    // Slow path: decode the source, downscale, persist. Restart-marker
    // JPEGs go entirely through the GPU session — decode + both resizes
    // run on the GPU and the full-res image never reaches the host.
    let bytes = std::fs::read(path).ok()?;
    let hash = sha256_hex(&bytes);
    let (thumb, engine) = match crate::gpujpeg::open(&bytes) {
        Some(session) => {
            ensure_screen_preview_gpu(path, &session, Some((len, nanos)));
            (session.resize_image(THUMB_MAX_DIM, THUMB_MAX_DIM)?, "gpu")
        }
        None => {
            let image = decode_image_bytes(path, &bytes).ok()?;
            drop(bytes);
            ensure_screen_preview(path, Some(&image), Some((len, nanos)));
            (resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM), "cpu")
        }
    };
    persist_preview(path, len, nanos, &source_name, &thumb, &hash);
    if verbose() {
        eprintln!("preview generated [{engine}]: {}", path.display());
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
    let dir = previews_dir_for(path);

    if let Some(pack) = crate::fs_utils::pack::pack_for(&dir) {
        let mut pack = pack.lock().unwrap();
        if let Some((jpeg, _hash)) =
            pack.lookup(&source_name, len, nanos, THUMB_MAX_DIM as u16)
        {
            if jpeg_plausible(&jpeg) {
                let dims = dims_hint
                    .or_else(|| image::image_dimensions(path).ok())
                    .unwrap_or((0, 0));
                if verbose() {
                    eprintln!("preview cache hit [pack]: {}", path.display());
                }
                return Some((jpeg, dims.0, dims.1));
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

    let bytes = std::fs::read(path).ok()?;
    // The source is already in memory — the content hash is nearly free and
    // becomes the preview's authoritative metadata.
    let hash = sha256_hex(&bytes);
    if let Some(session) = crate::gpujpeg::open(&bytes) {
        let (w, h) = (session.width, session.height);
        let (jpeg, _, _) =
            session.encode_resized(THUMB_MAX_DIM, THUMB_MAX_DIM, PREVIEW_JPEG_QUALITY.into())?;
        persist_preview_bytes(path, len, nanos, &source_name, &jpeg, &hash);
        // The fullscreen preview is generated by the store's deferred
        // screen-fill — never inline on the thumbnail hot path.
        if verbose() {
            eprintln!("preview generated [gpu]: {}", path.display());
        }
        return Some((jpeg, w, h));
    }

    let image = decode_image_bytes(path, &bytes).ok()?;
    drop(bytes);
    let (w, h) = (image.width(), image.height());
    let thumb = resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM);
    let mut jpeg = Vec::new();
    {
        let writer = std::io::BufWriter::new(std::io::Cursor::new(&mut jpeg));
        let encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(writer, PREVIEW_JPEG_QUALITY);
        thumb.to_rgb8().write_with_encoder(encoder).ok()?;
    }
    persist_preview_bytes(path, len, nanos, &source_name, &jpeg, &hash);
    if verbose() {
        eprintln!("preview generated [cpu]: {}", path.display());
    }
    Some((jpeg, w, h))
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
