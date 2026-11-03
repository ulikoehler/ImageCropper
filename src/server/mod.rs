//! `imagecropper <subcommand> --serve=ADDR:PORT` — web-UI server.
//!
//! Two WebSocket channels: `/data` (control, events, thumbnail push) and
//! `/bulk` (manifest + full-resolution images). HTTP only serves the embedded
//! client assets.

pub mod actions;
pub mod http;
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
        let previews = Arc::new(PreviewStore::new(parallel, 2048));
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
            actions: Arc::new(ActionQueue::new(
                parallel.max(1).min(4),
                parallel.max(32),
                move_dir,
            )),
            events,
            mode,
            mouse_select,
            token,
            initial_credit: push_credit_kb as u64 * 1024,
            resync_seq: std::sync::atomic::AtomicU64::new(0),
        })
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
            // resurface before the op lands.
            .filter(|(p, _)| !self.actions.is_pending(p))
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
    }

    /// Tombstone the item at `path` — safe to call across id reassignment:
    /// the path resolves to whatever id currently holds it, so a rescan
    /// mid-action can't tombstone the wrong slot.
    pub fn remove_path(&self, path: &Path) {
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
        std::thread::spawn(move || loop {
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
        });
    }

    /// Rebuild the manifest (e.g. after fs changes) and bump the version.
    /// Returns the new manifest for broadcast/patch purposes.
    pub fn rebuild_manifest(&self, files: Vec<PathBuf>, roots: &[PathBuf]) {
        // Paths queued for delete/move are "marked for deletion" — a rebuild
        // must not resurrect them just because the file still exists.
        let files: Vec<PathBuf> = files
            .into_iter()
            .filter(|p| !self.actions.is_pending(p))
            .collect();
        // Reuse stored dims/sig for files whose (size, mtime) signature is
        // unchanged — a rescan then stats each file instead of re-parsing
        // every image header, which matters a lot on slow/remote mounts.
        let (version, prior) = {
            let m = self.manifest.read().unwrap();
            let prior: std::collections::HashMap<PathBuf, (u32, u32, u64, u64)> = (0..m
                .items
                .len())
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
    }
}

/// Spawn the streaming-scan feeder: paths from a [`crate::cli::ScanStream`]
/// are appended to the manifest in batches (so clients see partial results
/// immediately, even on slow mounts), then the final `-o` ordering is
/// applied once the scan completes.
pub fn spawn_scan_feeder(state: &Arc<ServerState>, stream: crate::cli::ScanStream) {
    use crate::fs_utils::ScanEvent;
    const BATCH: usize = 64;
    const FLUSH: Duration = Duration::from_millis(200);

    let state = state.clone();
    std::thread::spawn(move || {
        let _ = state
            .events
            .send(ServerEvent::Json("{\"ev\":\"scan-start\"}".to_string()));
        if crate::fs_utils::verbose() {
            eprintln!("scan: started");
        }
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
            }
        }
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
        .route("/data", get(ws_data::handler))
        .route("/bulk", get(ws_bulk::handler))
        .with_state(state)
}

/// Ctrl-C shutdown: stop preview generation immediately, then wait for
/// queued deletes/moves/etc. to drain — destructive ops are preferential.
/// The wait is bounded well below `ACTION_TIMEOUT`: a wedged op
/// must not hold the process hostage.
async fn shutdown(state: &ServerState) {
    state.previews.stop();
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(10);
    let mut last = usize::MAX;
    while state.actions.pending() > 0 && std::time::Instant::now() < deadline {
        let n = state.actions.pending();
        if n != last {
            eprintln!("shutdown: finishing {n} queued action(s)…");
            last = n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let left = state.actions.pending();
    if left > 0 {
        eprintln!("shutdown: {left} action(s) still unfinished after 10s");
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
    rt.block_on(async move {
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
    })
}
