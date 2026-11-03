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

pub use actions::{Action, ActionDone, ActionEnvelope, ActionQueue, RotateOp};
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
    pub token: Option<String>,
    pub initial_credit: u64,
}

impl ServerState {
    pub fn new(
        files: Vec<PathBuf>,
        roots: Vec<PathBuf>,
        mode: ServeMode,
        parallel: usize,
        token: Option<String>,
        push_credit_kb: u32,
        move_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let manifest = Manifest::build(&files, &roots, mode, 1);
        let previews = Arc::new(PreviewStore::new(parallel, 2048));
        previews.request_all(files.iter());
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            manifest: Arc::new(RwLock::new(manifest)),
            roots,
            previews,
            actions: Arc::new(ActionQueue::new(parallel.max(1).min(4), move_dir)),
            events,
            mode,
            token,
            initial_credit: push_credit_kb as u64 * 1024,
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
    pub fn upsert_files(&self, paths: Vec<PathBuf>) -> usize {
        // Phase 1 (no lock): sniff per-file metadata — this is the I/O part
        // and must not block manifest readers.
        let sniffed: Vec<(PathBuf, u32, u32, u64, u64)> = paths
            .into_iter()
            .map(|p| {
                let (w, h) = image::image_dimensions(&p).unwrap_or((0, 0));
                let bytes = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                let sig = crate::fs_utils::previews::preview_sig_u64(&p).unwrap_or(0);
                (p, w, h, bytes, sig)
            })
            .collect();

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
                    match m.dirs.iter().position(|d| d.as_str() == parent_str.as_ref()) {
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
                if done.ok {
                    use crate::server::actions::ActionKind;
                    match done.kind {
                        ActionKind::Delete | ActionKind::Move => {
                            state.remove_ids(&[done.id]);
                        }
                        ActionKind::Crop | ActionKind::Resave => {
                            if let Some(new_path) = &done.new_path {
                                if new_path != &done.src {
                                    state.remove_ids(&[done.id]);
                                }
                                state.upsert_files(vec![new_path.clone()]);
                            }
                        }
                    }
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
        let version = self.manifest.read().unwrap().version + 1;
        let fresh = Manifest::build(&files, roots, self.mode, version);
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
        let mut batch: Vec<PathBuf> = Vec::new();
        loop {
            let timed_out = match stream.rx.recv_timeout(FLUSH) {
                Ok(ScanEvent::Found(p)) => {
                    batch.push(p);
                    false
                }
                Ok(ScanEvent::Error(e)) => {
                    eprintln!("scan: {e}");
                    false
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if batch.len() >= BATCH || (timed_out && !batch.is_empty()) {
                state.upsert_files(std::mem::take(&mut batch));
            }
        }
        if !batch.is_empty() {
            state.upsert_files(batch);
        }
        state.finish_scan(stream.order, stream.inverse);
        if crate::fs_utils::verbose() {
            eprintln!("scan complete: {} items", state.item_count());
        }
    });
}

/// Build the axum router (public for tests binding ephemeral ports).
pub fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(http::index))
        .route("/app.js", get(http::app_js))
        .route("/healthz", get(http::healthz))
        .route("/data", get(ws_data::handler))
        .route("/bulk", get(ws_bulk::handler))
        .with_state(state)
}

/// Serve on `addr`. Blocks; used by the `--serve` paths in main.rs.
pub fn run_server(addr: SocketAddr, state: Arc<ServerState>, open_browser: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        eprintln!("Serving at http://{bound}/");
        if let Some(token) = &state.token {
            eprintln!("Token: {token}");
        }
        state.spawn_pumps();
        if open_browser {
            let _ = webbrowser::open(&format!("http://{bound}/"));
        }
        axum::serve(listener, build_router(state)).await?;
        Ok(())
    })
}
