//! Persistent thumbnail cache in `.imagecropper-previews`.
//!
//! Preview file names embed the source file's size and modification time, so
//! a changed source automatically produces a fresh preview name and stale
//! previews are cleaned up. Shared by the album UI (`Thumbnailer`) and the
//! web server (`PreviewStore`).

use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use image::DynamicImage;

use crate::{
    fs_utils::{prepare_dir, PREVIEWS_DIR},
    image_utils::{decode_image_bytes, resize_to_fit},
};

/// Maximum dimension of generated previews (pixels).
pub const THUMB_MAX_DIM: u32 = 512;

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

/// Remove every cached preview for a source file (e.g. after it was deleted).
pub fn remove_previews_for(source: &Path) {
    let Some(source_name) = source.file_name() else {
        return;
    };
    let prefix = preview_file_prefix(&source_name.to_string_lossy());
    let dir = previews_dir_for(source);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Load the cached preview or generate, persist and return a fresh one.
/// Returns the decoded preview image (≤ `THUMB_MAX_DIM`).
pub fn produce_preview_image(path: &Path) -> Option<DynamicImage> {
    let (len, nanos) = source_signature(path)?;
    let source_name = path.file_name()?.to_string_lossy().into_owned();
    let expected_name = preview_file_name(&source_name, len, nanos);
    let dir = previews_dir_for(path);

    // Fast path: an up-to-date preview exists on disk.
    if let Some(cached) = find_cached_preview(&dir, path, &expected_name) {
        if let Ok(bytes) = std::fs::read(&cached) {
            if let Ok(img) = decode_image_bytes(&cached, &bytes) {
                return Some(img);
            }
        }
        // Corrupt preview — regenerate.
        let _ = std::fs::remove_file(&cached);
    }

    // Slow path: decode the source, downscale, persist.
    let bytes = std::fs::read(path).ok()?;
    let image = decode_image_bytes(path, &bytes).ok()?;
    drop(bytes);
    let thumb = resize_to_fit(image, THUMB_MAX_DIM, THUMB_MAX_DIM);
    persist_preview(path, &expected_name, &thumb);
    Some(thumb)
}

/// Like [`produce_preview_image`] but returns the persisted JPEG bytes plus
/// the source image's natural dimensions — the form the web server needs.
pub fn produce_preview_bytes(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let (len, nanos) = source_signature(path)?;
    let source_name = path.file_name()?.to_string_lossy().into_owned();
    let expected_name = preview_file_name(&source_name, len, nanos);
    let dir = previews_dir_for(path);

    if let Some(cached) = find_cached_preview(&dir, path, &expected_name) {
        if let Ok(bytes) = std::fs::read(&cached) {
            let dims = image::image_dimensions(path).unwrap_or((0, 0));
            return Some((bytes, dims.0, dims.1));
        }
        let _ = std::fs::remove_file(&cached);
    }

    let bytes = std::fs::read(path).ok()?;
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
    persist_preview_bytes(path, &expected_name, &jpeg);
    Some((jpeg, w, h))
}

fn persist_preview(source: &Path, expected_name: &str, thumb: &DynamicImage) {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    if let Ok(preview_dir) = prepare_dir(parent, PREVIEWS_DIR) {
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
        let _ = std::fs::write(preview_dir.join(expected_name), jpeg);
    }
}
