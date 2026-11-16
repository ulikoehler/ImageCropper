//! Filesystem watcher: scans roots for changes, rebuilds the manifest, and
//! broadcasts `fs-changed` so clients re-pull. Debounced 250 ms; reserved
//! dirs are excluded by the rescan itself (RESERVED_DIRS).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};

use super::ServerState;
use crate::cli::ScanArgs;

/// Coalesce bursts into one rescan. Bulk deletes fire hundreds of events in
/// seconds; a short window turns that into a rescan per delete batch.
const DEBOUNCE: Duration = Duration::from_millis(1000);

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
                    let relevant = event_affects_listing(&event.kind)
                        && event.paths.iter().any(|p| !inside_reserved(p));
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
            // Seeded rescan: files.pack prunes unchanged leaf dirs and the
            // walker skips stats for covered files — on slow mounts a full
            // re-walk + re-stat per fs event is the dominant cost. The
            // scan events carry (bytes, sig, dims) so the manifest rebuild
            // needs no per-file stat either.
            let mut seeds = Vec::new();
            let mut skip = std::collections::HashSet::new();
            for root in &scan.paths {
                if let Some((dirs, s)) =
                    crate::fs_utils::files_pack::load(root, scan.recursive)
                {
                    skip.extend(s);
                    for (dir, rec) in dirs {
                        for f in rec.files {
                            seeds.push(crate::fs_utils::ScanEvent::Cached {
                                path: dir.join(&f.name),
                                w: f.w,
                                h: f.h,
                                bytes: f.len,
                                sig: f.sig,
                            });
                        }
                    }
                }
            }
            let filter = match crate::fs_utils::PathFilter::compile(
                scan.filter_syntax,
                &scan.whitelist,
                &scan.blacklist,
            ) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("watch: {e}");
                    continue;
                }
            };
            let mut files = Vec::new();
            let mut sigs = HashMap::new();
            let mut hints = HashMap::new();
            let mut saw_error = false;
            crate::fs_utils::walk_images_seeded(
                &scan.paths,
                scan.recursive,
                filter.as_ref(),
                scan.max_files,
                Some((seeds, skip)),
                &mut |ev| match ev {
                    crate::fs_utils::ScanEvent::Found { path, bytes, sig } => {
                        files.push(path.clone());
                        sigs.insert(path.clone(), sig);
                        hints.insert(path, (0, 0, bytes, sig));
                    }
                    crate::fs_utils::ScanEvent::Cached {
                        path,
                        w,
                        h,
                        bytes,
                        sig,
                    } => {
                        files.push(path.clone());
                        sigs.insert(path.clone(), sig);
                        hints.insert(path, (w, h, bytes, sig));
                    }
                    crate::fs_utils::ScanEvent::Error(e) => {
                        eprintln!("watch: scan: {e}");
                        saw_error = true;
                    }
                },
            );
            // A scan error aborts the rescan (same as the old
            // collect_files → Err path): a partial listing would
            // tombstone files that merely failed to stat. An empty
            // result is likewise not a valid rescan.
            if saw_error || files.is_empty() {
                continue;
            }
            // The listing arrives unsorted; apply the `-o` order
            // through the shared signature-keyed cache — scan sigs make
            // the per-file validation stat unnecessary.
            state.sort_with_order(&mut files, scan.order, scan.inverse, Some(&sigs));
            // Differential rebuild: surviving files keep their ids, so
            // clients keep thumbnails/wants/selection. A full resync
            // here renumbers everything and — during a delete storm —
            // keeps wiping the thumbnails that just arrived. It only
            // happens when the sorted order itself changed (e.g. a new
            // file inserting mid-list), never for plain deletes.
            // Randomize is exempt — a shuffled listing must not force
            // a reshuffle on every rescan.
            let reorder = scan.order != crate::cli::SortOrder::Randomize;
            state.rebuild_manifest_diff_hinted(files, &hints, &scan.paths, reorder);
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

/// Whether an fs event can change what the rescan would list. Read-side
/// accesses are filtered out — the rescan itself opens and reads every
/// image (dimension sniffing, signatures), so without this each rescan
/// would emit enough inotify traffic to retrigger itself forever.
pub fn event_affects_listing(kind: &notify::event::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
    match kind {
        // A finished write can add/complete a file — relevant. Everything
        // else under Access is read-side noise (read, open, close-read).
        EventKind::Access(AccessKind::Close(mode)) => *mode != AccessMode::Read,
        EventKind::Access(_) => false,
        // chmod/chown/utime don't change the listing or file content.
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        // Creates, deletes, renames, content changes, unknown kinds.
        _ => true,
    }
}

/// Expose PathBuf helper for tests.
#[allow(dead_code)]
pub fn is_reserved_path(p: &PathBuf) -> bool {
    inside_reserved(p)
}
