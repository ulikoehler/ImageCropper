//! Filesystem watcher: scans roots for changes, rebuilds the manifest, and
//! broadcasts `fs-changed` so clients re-pull. Debounced 250 ms; reserved
//! dirs are excluded by the rescan itself (RESERVED_DIRS).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use super::{ServerEvent, ServerState};
use crate::cli::{collect_sorted_files, ScanArgs};

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Spawn a watcher over the scan roots. Returns the watcher — keep it alive
/// for the server lifetime.
pub fn spawn(state: Arc<ServerState>, scan: ScanArgs) -> Option<notify::RecommendedWatcher> {
    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Ignore churn inside our reserved dirs (trash/previews/tmp).
            let relevant = event.paths.iter().any(|p| !inside_reserved(p));
            if relevant {
                let _ = tx.send(());
            }
        }
    })
    .ok()?;

    for root in &scan.paths {
        let mode = if scan.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let _ = watcher.watch(Path::new(root), mode);
    }

    let roots = scan.paths.clone();
    std::thread::spawn(move || {
        // Debounce: drain bursts, rescan when quiet for DEBOUNCE.
        while rx.recv().is_ok() {
            while rx.recv_timeout(DEBOUNCE).is_ok() {}
            if let Ok(files) = collect_sorted_files(&scan) {
                state.rebuild_manifest(files, &roots);
                let _ = state
                    .events
                    .send(ServerEvent::Json("{\"ev\":\"fs-changed\"}".to_string()));
            }
        }
    });
    Some(watcher)
}

fn inside_reserved(p: &Path) -> bool {
    p.components().any(|c| {
        let s = c.as_os_str();
        s == crate::fs_utils::TRASH_DIR
            || s == crate::fs_utils::PREVIEWS_DIR
            || s == crate::fs_utils::TEMP_DIR
            || s == crate::fs_utils::ORIGINALS_DIR
    })
}

/// Expose PathBuf helper for tests.
#[allow(dead_code)]
pub fn is_reserved_path(p: &PathBuf) -> bool {
    inside_reserved(p)
}
