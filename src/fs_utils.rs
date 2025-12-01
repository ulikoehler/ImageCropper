use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use walkdir::WalkDir;

pub const TRASH_DIR: &str = ".imagecropper-trash";
pub const ORIGINALS_DIR: &str = ".imagecropper-originals";
pub const TEMP_DIR: &str = ".imagecropper-tmp";

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "bmp", "gif", "webp", "tiff", "tif", "ico", "avif",
];

pub fn collect_images(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        if !path.exists() {
            return Err(anyhow!("{} does not exist", path.display()));
        }

        if path.is_file() {
            if is_supported_image(path) {
                files.push(path.to_path_buf());
            }
        } else if path.is_dir() {
            if recursive {
                for entry in WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.file_type().is_file() && is_supported_image(entry.path()) {
                        files.push(entry.path().to_path_buf());
                    }
                }
            } else {
                for entry in fs::read_dir(path)
                    .with_context(|| format!("Unable to read directory {}", path.display()))?
                {
                    let entry = entry
                        .with_context(|| format!("Unable to read entry in {}", path.display()))?;
                    let p = entry.path();
                    if p.is_file() && is_supported_image(&p) {
                        files.push(p);
                    }
                }
            }
        }
    }
    Ok(files)
}

fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|s| s.to_ascii_lowercase()),
        Some(ref ext) if SUPPORTED_EXTENSIONS.contains(&ext.as_str())
    )
}

pub fn prepare_dir(base: &Path, name: &str) -> Result<PathBuf> {
    let dir = base.join(name);
    fs::create_dir_all(&dir).with_context(|| format!("Unable to create {}", dir.display()))?;
    Ok(dir)
}

pub fn move_with_unique_name(source: &Path, target_dir: &Path) -> Result<()> {
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", source.display()))?;
    let destination = unique_destination(target_dir, file_name);
    fs::rename(source, &destination).with_context(|| {
        format!(
            "Unable to move {} to {}",
            source.display(),
            destination.display()
        )
    })
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

pub fn backup_original(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = prepare_dir(parent, ORIGINALS_DIR)?;
    move_with_unique_name(path, &dir)
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

