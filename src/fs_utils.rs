use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::RegexSet;
use walkdir::WalkDir;

pub const TRASH_DIR: &str = ".imagecropper-trash";
pub const ORIGINALS_DIR: &str = ".imagecropper-originals";
pub const TEMP_DIR: &str = ".imagecropper-tmp";

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

        !self.blacklist.matches(path)
    }
}

fn normalize_filter_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub fn collect_images(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
    collect_images_with_filter(paths, recursive, None)
}

pub fn collect_images_with_filter(
    paths: &[PathBuf],
    recursive: bool,
    filter: Option<&PathFilter>,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        if !path.exists() {
            return Err(anyhow!("{} does not exist", path.display()));
        }

        if path.is_file() {
            if is_supported_image(path) && filter.map_or(true, |f| f.matches(path)) {
                files.push(path.to_path_buf());
            }
        } else if path.is_dir() {
            if recursive {
                for entry in WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.file_type().is_file()
                        && is_supported_image(entry.path())
                        && filter.map_or(true, |f| f.matches(entry.path()))
                    {
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
                    if p.is_file()
                        && is_supported_image(&p)
                        && filter.map_or(true, |f| f.matches(&p))
                    {
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

pub fn move_with_unique_name(source: &Path, target_dir: &Path) -> Result<PathBuf> {
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
    })?;
    Ok(destination)
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

pub fn format_savings_summary(original_bytes: u64, new_bytes: u64) -> String {
    if original_bytes >= new_bytes {
        format!(
            "Total conversion savings: {} ({} -> {})",
            format_size(original_bytes - new_bytes),
            format_size(original_bytes),
            format_size(new_bytes)
        )
    } else {
        format!(
            "Total conversion size increase: {} ({} -> {})",
            format_size(new_bytes - original_bytes),
            format_size(original_bytes),
            format_size(new_bytes)
        )
    }
}

/// Format a summary of bytes deleted (i.e. moved to trash).
pub fn format_deletion_summary(deleted_bytes: u64) -> String {
    format!("Total deleted file size: {}", format_size(deleted_bytes))
}

/// Combined summary string suitable for printing at exit.
pub fn format_overall_summary(original_bytes: u64, new_bytes: u64, deleted_bytes: u64) -> String {
    let mut parts = Vec::new();
    if original_bytes > 0 || new_bytes > 0 {
        parts.push(format_savings_summary(original_bytes, new_bytes));
    }
    if deleted_bytes > 0 {
        parts.push(format_deletion_summary(deleted_bytes));
    }
    if parts.is_empty() {
        "No operations performed".to_string()
    } else {
        parts.join(" | ")
    }
}

