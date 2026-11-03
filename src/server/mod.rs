//! `imagecropper <subcommand> --serve=ADDR:PORT` — web-UI server.
//!
//! Two WebSocket channels: `/data` (control, events, thumbnail push) and
//! `/bulk` (manifest + full-resolution images). HTTP only serves the embedded
//! client assets.

pub mod actions;
pub mod http;
pub mod io_gate;
pub mod manifest;
pub mod preview_store;
pub mod proto;
pub mod push;
pub mod watch;
mod ws_bulk;
mod ws_data;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use axum::{routing::get, Router};
use tokio::sync::broadcast;

pub use actions::{Action, ActionDone, ActionEnvelope, ActionKind, ActionQueue, RotateOp};
pub use io_gate::IoGate;
pub use manifest::{Manifest, ManifestItem, ManifestPatch, ServeMode};
pub use preview_store::{Preview, PreviewStore};
pub use proto::*;
pub use push::PushQueue;

/// Server-wide events broadcast to every connected /data client.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// JSON-encoded client event (`{"ev": …}`).
    Json(String),
}

pub struct ServerState {
    pub manifest: Arc<RwLock<Manifest>>,
    /// Scan roots (also embedded in the manifest for clients).
    pub roots: Vec<PathBuf>,
    pub previews: Arc<PreviewStore>,
    pub actions: Arc<ActionQueue>,
    pub events: broadcast::Sender<ServerEvent>,
    pub mode: ServeMode,
    pub mouse_select: crate::cli::MouseSelectMode,
    pub token: Option<String>,
    pub initial_credit: u64,
    /// Bumped on every resync broadcast — per-client push queues reset on it
    /// because the resync reassigned all manifest ids.
    pub resync_seq: std::sync::atomic::AtomicU64,
    /// Paths that left the album (deleted/moved) with the signature they had
    /// when they left. A rescan that listed the directory *before* the op
    /// landed re-inserts them otherwise — that is how already-deleted files
    /// used to come back.
    gone: std::sync::Mutex<std::collections::HashMap<PathBuf, (u64, std::time::Instant)>>,
    /// Client-first IO priority: live client requests mark it, background
    /// work (deletes/moves, crops, speculative preload) yields to it.
    pub io_gate: Arc<IoGate>,
    /// Set when the manifest changed — the pinned (RAM-resident) thumbnail
    /// set has to be recomputed.
    pins_dirty: std::sync::atomic::AtomicBool,
    /// Counters behind `/debugz` — the web client only sees its own state;
    /// this answers "is the server still scanning/pushing/producing?".
    pub dbg: DbgStats,
}

/// Lightweight instrumentation for `/debugz`. Everything is cheap (atomics
/// or a small mutexed map) — it must never gate the hot path.
#[derive(Default)]
pub struct DbgStats {
    /// Thumbnails pushed by the sweep path / WANT replies, and their bytes.
    pub sweep_sent: std::sync::atomic::AtomicU64,
    pub sweep_bytes: std::sync::atomic::AtomicU64,
    pub want_sent: std::sync::atomic::AtomicU64,
    /// CL_WANT ids received across all connections.
    pub want_recv: std::sync::atomic::AtomicU64,
    /// Produces that returned `None` (unsupported/corrupt/stopped).
    pub produce_none: std::sync::atomic::AtomicU64,
    /// Produces abandoned by the 30 s wait bound (hung reads).
    pub produce_timeout: std::sync::atomic::AtomicU64,
    /// Pushes dropped by the stale-id guard (resync/upsert mid-produce).
    pub stale_skip: std::sync::atomic::AtomicU64,
    /// CL_CREDIT bytes granted by clients.
    pub credit_recv: std::sync::atomic::AtomicU64,
    /// Scan feeder progress (mirrors the stderr counters).
    pub scan_found: std::sync::atomic::AtomicU64,
    pub scan_done: std::sync::atomic::AtomicBool,
    /// Per-connection PushQueue snapshots keyed by connection id.
    pub queues: std::sync::Mutex<std::collections::HashMap<u64, serde_json::Value>>,
    /// Connection id allocator (debug output only).
    pub next_conn: std::sync::atomic::AtomicU64,
}

impl ServerState {
    pub fn new(
        files: Vec<PathBuf>,
        roots: Vec<PathBuf>,
        mode: ServeMode,
        mouse_select: crate::cli::MouseSelectMode,
        parallel: usize,
        token: Option<String>,
        push_credit_kb: u32,
        move_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let manifest = Manifest::build(&files, &roots, mode, mouse_select, 1);
        // One gate for the whole server: preview production marks live
        // client requests on it, deletes/crops/preload yield to them.
        let io_gate = IoGate::new();
        let previews = Arc::new(PreviewStore::with_gate(
            parallel,
            2048,
            Arc::clone(&io_gate),
        ));
        for item in &manifest.items {
            if let Some(path) = manifest.path_of(item.id) {
                previews.note_dims(&path, (item.w, item.h));
            }
        }
        previews.request_all(files.iter());
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            manifest: Arc::new(RwLock::new(manifest)),
            roots,
            previews,
            actions: Arc::new(ActionQueue::with_gate(
                parallel.max(1).min(4),
                parallel.max(32),
                move_dir,
                Arc::clone(&io_gate),
            )),
            events,
            mode,
            mouse_select,
            token,
            initial_credit: push_credit_kb as u64 * 1024,
            resync_seq: std::sync::atomic::AtomicU64::new(0),
            gone: std::sync::Mutex::new(std::collections::HashMap::new()),
            io_gate,
            pins_dirty: std::sync::atomic::AtomicBool::new(true),
            dbg: DbgStats::default(),
        })
    }

    /// Recompute which thumbnails must stay resident in RAM: the first
    /// [`preview_store::PIN_LIMIT`] live items, skipping anything queued for
    /// delete/move or already departed. Cheap enough to run on every manifest
    /// change, but the pump throttles it.
    pub fn refresh_pins(&self) {
        self.pins_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let pins: Vec<PathBuf> = {
            let m = self.manifest.read().unwrap();
            let mut pins = Vec::with_capacity(preview_store::PIN_LIMIT);
            for i in 0..m.items.len() {
                if pins.len() >= preview_store::PIN_LIMIT {
                    break;
                }
                if m.items[i].removed {
                    continue;
                }
                let Some(path) = m.path_of(i as u32) else {
                    continue;
                };
                if self.actions.is_pending(&path) || self.is_gone(&path) {
                    continue;
                }
                pins.push(path);
            }
            pins
        };
        self.previews.set_pins(pins);
    }

    /// Mark the pin set stale (manifest changed).
    fn pins_changed(&self) {
        self.pins_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// How long a departed path stays suppressed. Long enough to cover a
    /// rescan that started before the delete landed; a file re-created with
    /// different content is let straight through.
    const GONE_TTL: Duration = Duration::from_secs(300);

    /// Remember that `path` left the album so a stale directory listing can't
    /// put it back. The signature is kept so a *re-created* file (different
    /// size/mtime) is still accepted.
    fn note_gone(&self, path: &Path) {
        let sig = {
            let m = self.manifest.read().unwrap();
            m.position_of(path)
                .and_then(|id| m.items.get(id as usize).map(|i| i.sig))
                .unwrap_or(0)
        };
        let mut gone = self.gone.lock().unwrap();
        if gone.len() > 4096 {
            let ttl = Self::GONE_TTL;
            gone.retain(|_, (_, at)| at.elapsed() < ttl);
        }
        let entry = gone
            .entry(path.to_path_buf())
            .or_insert((sig, std::time::Instant::now()));
        // A later call (post-success) can no longer read the item's sig —
        // keep the one recorded while it was still live.
        if entry.0 == 0 {
            entry.0 = sig;
        }
    }

    /// True while `path` must stay out of the manifest: it left the album and
    /// either no longer exists or still holds the content it had when it left.
    fn is_gone(&self, path: &Path) -> bool {
        let mut gone = self.gone.lock().unwrap();
        let Some(&(sig, at)) = gone.get(path) else {
            return false;
        };
        if at.elapsed() >= Self::GONE_TTL {
            gone.remove(path);
            return false;
        }
        match crate::fs_utils::previews::preview_sig_u64(path) {
            // Gone from disk (or unreadable) — keep it out of the listing.
            None => true,
            // Same content as when it left: a stale listing, not a new file.
            Some(now) if sig != 0 && now == sig => true,
            // Re-created with different content — it is a new file.
            Some(_) => {
                gone.remove(path);
                false
            }
        }
    }

    /// Manifest id → absolute path.
    pub fn path_of(&self, id: u32) -> Option<PathBuf> {
        self.manifest.read().unwrap().path_of(id)
    }

    /// Number of items currently in the manifest (grows while scanning).
    pub fn item_count(&self) -> usize {
        self.manifest.read().unwrap().items.len()
    }

    /// Absolute path → item id.
    pub fn position_of(&self, path: &Path) -> Option<u32> {
        self.manifest.read().unwrap().position_of(path)
    }

    /// Insert newly discovered files or update existing entries in place
    /// (streaming scan + post-action upserts). Keeps the positional id of
    /// known paths so client indexing stays stable; unknown paths append.
    /// Queues previews and broadcasts a `manifest-patch` event so clients
    /// upsert differentially instead of re-pulling the manifest.
    /// Returns how many items were inserted or updated.
    /// `bytes` = walker-supplied size when known (`None` → sniffed here).
    pub fn upsert_files(&self, files: Vec<(PathBuf, Option<u64>)>) -> usize {
        // Phase 1 (no lock): sniff per-file metadata — this is the I/O part
        // and must not block manifest readers.
        let sniffed: Vec<(PathBuf, u32, u32, u64, u64)> = files
            .into_iter()
            // Queued for delete/move — "marked for deletion", must not
            // resurface before the op lands. Already departed (a scan batch
            // enumerated the directory before the rename) — must not come
            // back either.
            .filter(|(p, _)| !self.actions.is_pending(p) && !self.is_gone(p))
            .map(|(p, known_bytes)| {
                let (w, h) = image::image_dimensions(&p).unwrap_or((0, 0));
                let bytes = known_bytes
                    .unwrap_or_else(|| std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0));
                let sig = crate::fs_utils::previews::preview_sig_u64(&p).unwrap_or(0);
                (p, w, h, bytes, sig)
            })
            .collect();
        // Feed the dims hint map before queueing — manifest sniffing already
        // paid for this information, so preview cache hits won't pay again.
        for (p, w, h, _, _) in &sniffed {
            self.previews.note_dims(p, (*w, *h));
        }

        let (added, patch_json) = {
            let mut m = self.manifest.write().unwrap();
            let mut patch = ManifestPatch {
                version: m.version + 1,
                ..Default::default()
            };
            let mut dir_index = std::collections::HashMap::<PathBuf, u32>::new();
            let mut added = Vec::new();
            for (path, w, h, bytes, sig) in sniffed {
                if let Some(&id) = m.positions.get(&path) {
                    // In-place upsert: keep the id so client tiles stay put.
                    let item = &mut m.items[id as usize];
                    item.w = w;
                    item.h = h;
                    item.bytes = bytes;
                    item.sig = sig;
                    item.removed = false;
                    patch.upsert.push(item.clone());
                    added.push(path);
                    continue;
                }
                let parent = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                let dir_idx = *dir_index.entry(parent.clone()).or_insert_with(|| {
                    // Intern into the manifest's dir table (existing or new).
                    let parent_str = parent.to_string_lossy();
                    match m
                        .dirs
                        .iter()
                        .position(|d| d.as_str() == parent_str.as_ref())
                    {
                        Some(i) => i as u32,
                        None => {
                            m.dirs.push(parent.to_string_lossy().into_owned());
                            patch.dirs_added.push(parent.to_string_lossy().into_owned());
                            (m.dirs.len() - 1) as u32
                        }
                    }
                });
                let id = m.items.len() as u32;
                let item = ManifestItem {
                    id,
                    name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    dir_index: dir_idx,
                    w,
                    h,
                    bytes,
                    sig,
                    removed: false,
                };
                m.positions.insert(path.clone(), id);
                m.items.push(item.clone());
                patch.upsert.push(item);
                added.push(path);
            }
            if patch.upsert.is_empty() {
                return 0;
            }
            m.version = patch.version;
            let json = serde_json::to_string(&serde_json::json!({
                "ev": "manifest-patch",
                "patch": patch,
            }))
            .ok();
            (added, json)
        };

        self.previews.request_all(added.iter());
        if let Some(json) = patch_json {
            let _ = self.events.send(ServerEvent::Json(json));
        }
        if !added.is_empty() {
            self.pins_changed();
        }
        added.len()
    }

    /// Tombstone items (deleted/moved files). Broadcasts a `manifest-patch`
    /// with `remove` so every connected client updates differentially —
    /// ids stay positional, so no reload is needed.
    pub fn remove_ids(&self, ids: &[u32]) {
        let json = {
            let mut m = self.manifest.write().unwrap();
            let mut touched = Vec::new();
            for &id in ids {
                let Some(path) = m.path_of(id) else {
                    continue;
                };
                if let Some(item) = m.items.get_mut(id as usize) {
                    item.removed = true;
                }
                m.positions.remove(&path);
                touched.push(id);
            }
            if touched.is_empty() {
                return;
            }
            m.version += 1;
            serde_json::to_string(&serde_json::json!({
                "ev": "manifest-patch",
                "patch": {
                    "version": m.version,
                    "upsert": [],
                    "remove": touched,
                    "dirs_added": [],
                },
            }))
            .ok()
        };
        if let Some(json) = json {
            let _ = self.events.send(ServerEvent::Json(json));
        }
        // The pinned set is "first N live items" — tombstones change it.
        self.pins_changed();
    }

    /// Tombstone the item at `path` — safe to call across id reassignment:
    /// the path resolves to whatever id currently holds it, so a rescan
    /// mid-action can't tombstone the wrong slot.
    pub fn remove_path(&self, path: &Path) {
        // Remember the departure *before* the slot is tombstoned: a rescan
        // that already listed this directory must not put it back.
        self.note_gone(path);
        let id = self.manifest.read().unwrap().position_of(path);
        if let Some(id) = id {
            self.remove_ids(&[id]);
        }
    }

    /// Undo a tombstone after a failed delete/move — the file is still on
    /// disk, so the item goes live again (broadcast as an `upsert` patch).
    /// Falls back to [`Self::upsert_files`] when a rescan already compacted
    /// the slot away.
    pub fn restore_path(&self, path: &Path) {
        // The op failed, so the file never left: forget the departure or the
        // item could never come back.
        self.gone.lock().unwrap().remove(path);
        enum Out {
            Live,
            Missing,
            Restored(Option<String>),
        }
        let out = {
            let mut m = self.manifest.write().unwrap();
            if m.positions.contains_key(path) {
                Out::Live
            } else {
                // positions only tracks live items — scan for a tombstoned
                // slot that still resolves to `path`.
                let id = (0..m.items.len()).find(|&i| {
                    let item = &m.items[i];
                    m.dirs
                        .get(item.dir_index as usize)
                        .map(|d| Path::new(d).join(&item.name) == path)
                        .unwrap_or(false)
                });
                match id {
                    Some(i) => {
                        m.items[i].removed = false;
                        m.positions.insert(path.to_path_buf(), i as u32);
                        let item = m.items[i].clone();
                        m.version += 1;
                        let json = serde_json::to_string(&serde_json::json!({
                            "ev": "manifest-patch",
                            "patch": {
                                "version": m.version,
                                "upsert": [item],
                                "remove": [],
                                "dirs_added": [],
                            },
                        }))
                        .ok();
                        Out::Restored(json)
                    }
                    None => Out::Missing,
                }
            }
        };
        match out {
            Out::Live => {}
            Out::Restored(json) => {
                if let Some(json) = json {
                    let _ = self.events.send(ServerEvent::Json(json));
                }
            }
            Out::Missing => {
                self.upsert_files(vec![(path.to_path_buf(), None)]);
            }
        }
    }

    /// Scan finished: apply the final `-o` ordering, rebuild the manifest
    /// (ids are reassigned) and broadcast a `resync` patch so clients
    /// replace their lists in place — no reconnect or manifest re-pull.
    pub fn finish_scan(&self, order: crate::cli::SortOrder, inverse: bool) {
        let mut files: Vec<PathBuf> = {
            let m = self.manifest.read().unwrap();
            (0..m.items.len())
                .filter_map(|i| m.path_of(i as u32))
                .collect()
        };
        crate::cli::sort_files(&mut files, order, inverse);
        self.rebuild_manifest(files, &self.roots);
        self.broadcast_resync();
    }

    /// Broadcast the current manifest as a `resync` patch event.
    pub fn broadcast_resync(&self) {
        self.resync_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let patch = self.manifest.read().unwrap().full_patch();
        if let Ok(json) = serde_json::to_string(&serde_json::json!({
            "ev": "manifest-patch",
            "patch": patch,
        })) {
            let _ = self.events.send(ServerEvent::Json(json));
        }
    }

    /// Start the background pump: drains preview results + action
    /// completions and forwards them as broadcast events.
    pub fn spawn_pumps(self: &Arc<Self>) {
        let state = self.clone();
        std::thread::spawn(move || {
            let mut last_pin = std::time::Instant::now() - std::time::Duration::from_secs(1);
            let mut last_fill = std::time::Instant::now();
            loop {
                // Keep the top of the album resident in RAM. The pin set is
                // recomputed when the manifest changed; missing pinned
                // previews are re-queued periodically so the set converges
                // (a failed/evicted preview must not leave a hole).
                if state.pins_dirty.load(std::sync::atomic::Ordering::Relaxed)
                    && last_pin.elapsed() >= std::time::Duration::from_millis(250)
                {
                    state.refresh_pins();
                    last_pin = std::time::Instant::now();
                }
                if last_fill.elapsed() >= std::time::Duration::from_secs(2) {
                    for path in state.previews.missing_pins() {
                        state.previews.request(path);
                    }
                    last_fill = std::time::Instant::now();
                }
                // Preview completions → per-path ready events.
                for (path, preview) in state.previews.poll() {
                    if let Some(id) = state.position_of(&path) {
                        let dims = preview
                            .as_ref()
                            .map(|p| format!(",\"w\":{},\"h\":{}", p.w, p.h))
                            .unwrap_or_default();
                        let _ = state.events.send(ServerEvent::Json(format!(
                            "{{\"ev\":\"thumb-ready\",\"id\":{id}{dims}}}"
                        )));
                    }
                }
                // Action completions → differential manifest updates + events.
                for done in state.actions.poll() {
                    use crate::server::actions::ActionKind;
                    match (done.ok, done.kind) {
                        // Path-based: ids may have been reassigned by a rescan
                        // while the action was in flight — done.src is stable.
                        (true, ActionKind::Delete | ActionKind::Move) => {
                            state.remove_path(&done.src);
                        }
                        // Undo the eager tombstone: the file never left.
                        (false, ActionKind::Delete | ActionKind::Move) => {
                            state.restore_path(&done.src);
                        }
                        (true, ActionKind::Crop | ActionKind::Resave) => {
                            if let Some(new_path) = &done.new_path {
                                if new_path != &done.src {
                                    state.remove_path(&done.src);
                                }
                                state.upsert_files(vec![(new_path.clone(), None)]);
                            }
                        }
                        (false, _) => {}
                    }
                    if crate::fs_utils::verbose() {
                        eprintln!(
                            "action {:?} id={} ok={} err={:?}",
                            done.kind, done.id, done.ok, done.error
                        );
                    }
                    if let Ok(json) = serde_json::to_string(&serde_json::json!({
                        "ev": "action-done",
                        "tag": done.tag,
                        "ok": done.ok,
                        "error": done.error,
                        "new_sig": done.new_sig,
                        "bytes": done.bytes,
                    })) {
                        let _ = state.events.send(ServerEvent::Json(json));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
    }

    /// Rebuild the manifest (e.g. after fs changes) and bump the version.
    /// Returns the new manifest for broadcast/patch purposes.
    pub fn rebuild_manifest(&self, files: Vec<PathBuf>, roots: &[PathBuf]) {
        // Paths queued for delete/move are "marked for deletion" — a rebuild
        // must not resurrect them just because the file still exists. Paths
        // that already left (the listing predates the rename) must not come
        // back either.
        let files: Vec<PathBuf> = files
            .into_iter()
            .filter(|p| !self.actions.is_pending(p) && !self.is_gone(p))
            .collect();
        // Reuse stored dims/sig for files whose (size, mtime) signature is
        // unchanged — a rescan then stats each file instead of re-parsing
        // every image header, which matters a lot on slow/remote mounts.
        let (version, prior) = {
            let m = self.manifest.read().unwrap();
            let prior: std::collections::HashMap<PathBuf, (u32, u32, u64, u64)> =
                (0..m.items.len())
                    .filter_map(|i| {
                        let it = &m.items[i];
                        m.path_of(i as u32)
                            .map(|p| (p, (it.w, it.h, it.bytes, it.sig)))
                    })
                    .collect();
            (m.version + 1, prior)
        };
        let sniff = move |path: &Path| {
            let sig = crate::fs_utils::previews::preview_sig_u64(path).unwrap_or(0);
            if let Some(&(w, h, bytes, old_sig)) = prior.get(path) {
                if sig == old_sig && sig != 0 {
                    return (w, h, bytes, sig);
                }
            }
            let (w, h) = image::image_dimensions(path).unwrap_or((0, 0));
            let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            (w, h, bytes, sig)
        };
        let fresh =
            Manifest::build_with(&files, roots, self.mode, self.mouse_select, version, sniff);
        *self.manifest.write().unwrap() = fresh;
        // The album order/contents changed — re-pin the top of it.
        self.pins_changed();
    }
}

/// Spawn the streaming-scan feeder: paths from a [`crate::cli::ScanStream`]
/// are appended to the manifest in batches (so clients see partial results
/// immediately, even on slow mounts), then the final `-o` ordering is
/// applied once the scan completes.
pub fn spawn_scan_feeder(state: &Arc<ServerState>, stream: crate::cli::ScanStream) {
    use crate::fs_utils::ScanEvent;
    // Small batches so the client sees items appear smoothly instead of in
    // big steps — a flush still caps the patch rate at FLUSH intervals.
    const BATCH: usize = 8;
    const FLUSH: Duration = Duration::from_millis(200);
    // While fewer than this many items are indexed (and the scan is still
    // running), destructive work defers entirely: the album filling in is
    // the priority, deletes/moves can wait.
    const PRIORITY_UNTIL: usize = 2000;

    let state = state.clone();
    std::thread::spawn(move || {
        let _ = state
            .events
            .send(ServerEvent::Json("{\"ev\":\"scan-start\"}".to_string()));
        if crate::fs_utils::verbose() {
            eprintln!("scan: started");
        }
        // RAII: the flag clears itself if this thread exits by any path —
        // a stuck hold would tax every action job for the rest of the run.
        let mut hold = Some(state.io_gate.scan_hold());
        let mut batch: Vec<(PathBuf, Option<u64>)> = Vec::new();
        let mut found = 0usize;
        let t0 = std::time::Instant::now();
        let mut last_report = t0;
        loop {
            let timed_out = match stream.rx.recv_timeout(FLUSH) {
                Ok(ScanEvent::Found { path, bytes }) => {
                    batch.push((path, Some(bytes)));
                    found += 1;
                    false
                }
                Ok(ScanEvent::Error(e)) => {
                    eprintln!("scan: {e}");
                    // Surface scan errors to clients — a directory that
                    // can't be read must not silently show "0 images".
                    if let Ok(json) = serde_json::to_string(
                        &serde_json::json!({"ev": "scan-error", "msg": e}),
                    ) {
                        let _ = state.events.send(ServerEvent::Json(json));
                    }
                    false
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // Slow mounts can take minutes to enumerate — say we're alive.
            if crate::fs_utils::verbose() && last_report.elapsed().as_secs() >= 2 {
                eprintln!("scan: {found} files discovered so far…");
                last_report = std::time::Instant::now();
            }
            // Ctrl-C: stop feeding upserts/preview requests into a state
            // that's already shutting down.
            if crate::fs_utils::sigint_received() {
                break;
            }
            if batch.len() >= BATCH || (timed_out && !batch.is_empty()) {
                state.upsert_files(std::mem::take(&mut batch));
                state
                    .dbg
                    .scan_found
                    .store(found as u64, std::sync::atomic::Ordering::Relaxed);
                if hold.is_some() && state.item_count() >= PRIORITY_UNTIL {
                    hold.take(); // drops the guard → flag cleared
                    if crate::fs_utils::verbose() {
                        eprintln!("scan: {PRIORITY_UNTIL} items indexed — background work resumed");
                    }
                }
            }
        }
        drop(hold);
        if !batch.is_empty() {
            state.upsert_files(batch);
        }
        if crate::fs_utils::verbose() {
            eprintln!(
                "scan: finished — {found} files discovered in {:?}, indexing…",
                t0.elapsed()
            );
        }
        state.finish_scan(stream.order, stream.inverse);
        if crate::fs_utils::verbose() {
            eprintln!("scan: {} items indexed", state.item_count());
        }
        // Let clients distinguish "still scanning" from "done, found
        // nothing" — the manifest resync alone can't express that.
        let _ = state.events.send(ServerEvent::Json(format!(
            "{{\"ev\":\"scan-end\",\"count\":{found}}}"
        )));
        state
            .dbg
            .scan_found
            .store(found as u64, std::sync::atomic::Ordering::Relaxed);
        state
            .dbg
            .scan_done
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Token check at WS upgrade time (`?token=` query param). Returns `true`
/// when no token is configured or the supplied token matches. Both sockets
/// enforce it — `/bulk` must never be a backdoor around `/data`'s HELLO.
fn ws_authed(state: &ServerState, query: &std::collections::HashMap<String, String>) -> bool {
    state.token.is_none() || query.get("token").map(String::as_str) == state.token.as_deref()
}

/// Build the axum router (public for tests binding ephemeral ports).
pub fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(http::index))
        .route("/app.js", get(http::app_js))
        .route("/favicon.ico", get(http::favicon))
        .route("/healthz", get(http::healthz))
        .route("/debugz", get(http::debugz))
        .route("/data", get(ws_data::handler))
        .route("/bulk", get(ws_bulk::handler))
        .with_state(state)
}

/// Ctrl-C shutdown: stop preview generation immediately, then finish every
/// queued delete/move/crop. There is deliberately no deadline — the queue is
/// the user's work and it must land. A second Ctrl-C abandons the drain.
async fn shutdown(state: &ServerState) {
    state.previews.stop();
    // Refuse new GPU work too: in-flight decodes/resizes still finish,
    // but nothing submits after this point — required for a quiescent
    // device at process exit.
    crate::gpujpeg::stop();
    let mut last = usize::MAX;
    let mut last_report = std::time::Instant::now();
    loop {
        let n = state.actions.pending();
        if n == 0 {
            break;
        }
        if n != last {
            eprintln!("shutdown: finishing {n} queued action(s)…");
            last = n;
            last_report = std::time::Instant::now();
        } else if last_report.elapsed() >= std::time::Duration::from_secs(5) {
            eprintln!("shutdown: still finishing {n} queued action(s)… (Ctrl-C again to quit)");
            last_report = std::time::Instant::now();
        }
        if crate::fs_utils::sigint_count() > 1 {
            eprintln!("shutdown: {n} action(s) left unfinished");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // No GPU call may be in flight when the process exits: shared-library
    // destructors (the Vulkan driver) run during exit() and segfault if a
    // detached worker is still inside a driver call. Wait for the
    // counted GPU scopes to drain — bounded: a genuinely wedged driver
    // call must not hang shutdown forever.
    let gpu_t0 = std::time::Instant::now();
    loop {
        if crate::gpujpeg::wait_idle(std::time::Duration::from_millis(200)) {
            break;
        }
        if crate::fs_utils::sigint_count() > 1 {
            break;
        }
        if gpu_t0.elapsed() >= std::time::Duration::from_secs(30) {
            eprintln!("shutdown: GPU work did not drain in 30s — exiting anyway");
            break;
        }
    }
}

/// Serve on `addr`. Blocks; used by the `--serve` paths in main.rs.
/// `on_ready` runs after the socket is bound and the URL printed — slow
/// startup work (scan feeding, FS watcher registration on remote mounts)
/// belongs there so the server is reachable first.
pub fn run_server(
    addr: SocketAddr,
    state: Arc<ServerState>,
    open_browser: bool,
    on_ready: impl FnOnce() + Send + 'static,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        // A wildcard bind (0.0.0.0) isn't a useful browser address — open
        // loopback instead. The token goes straight into the URL so the
        // client's WS connects carry `?token=` automatically.
        let host = if bound.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            bound.ip().to_string()
        };
        let url = match &state.token {
            Some(t) => format!("http://{host}:{}/?token={t}", bound.port()),
            None => format!("http://{host}:{}/", bound.port()),
        };
        eprintln!("Serving at {url}");
        state.spawn_pumps();
        if open_browser {
            let _ = webbrowser::open(&url);
        }
        on_ready();
        tokio::select! {
            r = axum::serve(listener, build_router(state.clone())) => { r?; }
            r = tokio::signal::ctrl_c() => {
                r?;
                eprintln!("Ctrl-C: stopping preview generation, finishing queued actions…");
                shutdown(&state).await;
            }
        }
        Ok(())
    });
    // Bound the teardown wait: a `spawn_blocking` task stuck on a
    // dead-mount read must not hold process exit forever. Detached
    // leftovers are safe: gpujpeg::stop() means they can no longer start
    // a driver call, and a normal exit() is then race-free — shutdown()
    // already waited for GPU_ACTIVE == 0.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}
