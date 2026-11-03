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
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

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
    pub files: Arc<Vec<PathBuf>>,
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
            files: Arc::new(files),
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

    /// Start the background pump: drains preview results + action
    /// completions and forwards them as broadcast events.
    pub fn spawn_pumps(self: &Arc<Self>) {
        let state = self.clone();
        std::thread::spawn(move || loop {
            // Preview completions → per-path ready events.
            for (path, preview) in state.previews.poll() {
                if let Some(id) = state
                    .files
                    .iter()
                    .position(|p| p == &path)
                    .map(|i| i as u32)
                {
                    let dims = preview
                        .as_ref()
                        .map(|p| format!(",\"w\":{},\"h\":{}", p.w, p.h))
                        .unwrap_or_default();
                    let _ = state.events.send(ServerEvent::Json(format!(
                        "{{\"ev\":\"thumb-ready\",\"id\":{id}{dims}}}"
                    )));
                }
            }
            // Action completions → action-done events.
            for done in state.actions.poll() {
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
        eprintln!("Serving {} file(s) at http://{bound}/", state.files.len());
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
