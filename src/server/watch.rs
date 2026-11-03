//! Filesystem watcher: scans roots for changes, rebuilds the manifest, and
//! broadcasts `fs-changed` so clients re-pull. Debounced 250 ms; reserved
//! dirs are excluded by the rescan itself (RESERVED_DIRS).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use super::ServerState;
use crate::cli::{collect_sorted_files, ScanArgs};

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Spawn a watcher over the scan roots. The whole setup — watcher
/// construction AND recursive watch registration — runs on the spawned
/// thread, because on remote/slow mounts `watch(root, Recursive)` can take
/// a very long time (per-directory watches over the whole tree) and must
/// never block server startup.
pub fn spawn(state: Arc<ServerState>, scan: ScanArgs) {
    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel::<()>();
        if crate::fs_utils::verbose() {
            eprintln!(
                "watch: registering {} root(s){}…",
                scan.paths.len(),
                if scan.recursive { " recursively" } else { "" }
            );
        }
        let Ok(mut watcher) =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    // Ignore churn inside our reserved dirs (trash/previews/tmp).
                    let relevant = event.paths.iter().any(|p| !inside_reserved(p));
                    if relevant {
                        let _ = tx.send(());
                    }
                }
            })
        else {
            eprintln!("watch: could not create a filesystem watcher — live updates off");
            return;
        };
        for root in &scan.paths {
            let mode = if scan.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if watcher.watch(Path::new(root), mode).is_err() && crate::fs_utils::verbose() {
                eprintln!("watch: cannot watch {}", root.display());
            }
        }
        if crate::fs_utils::verbose() {
            eprintln!("watch: active");
        }

        // Debounce: drain bursts, rescan when quiet for DEBOUNCE. `watcher`
        // is owned by this thread — it stays alive for the process lifetime.
        while rx.recv().is_ok() {
            while rx.recv_timeout(DEBOUNCE).is_ok() {}
            if let Ok(files) = collect_sorted_files(&scan) {
                state.rebuild_manifest(files, &scan.paths);
                // Full resync patch — ids were reassigned wholesale.
                state.broadcast_resync();
            }
        }
    });
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
