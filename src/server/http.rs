//! Embedded client assets — HTTP only ever serves these.

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;

use super::ServerState;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");

pub async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

pub async fn app_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        APP_JS,
    )
}

pub async fn healthz(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let n = state.manifest.read().unwrap().items.len();
    (StatusCode::OK, format!("ok {n}"))
}

/// Debug counters: scan progress, push-pump activity, per-connection queue
/// snapshots, preview backlog. Read by the client's `icServer()` helper.
pub async fn debugz(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    use std::sync::atomic::Ordering::Relaxed;
    let d = &state.dbg;
    // Sample entries: 0x0 dims here mean tiles render square/stretched;
    // compare `sig` against the client's ic().sample to spot a stale entry.
    let (items, version, sample) = {
        let m = state.manifest.read().unwrap();
        let sample: Vec<serde_json::Value> = (0..m.items.len().min(3))
            .filter_map(|i| {
                let it = m.items.get(i)?;
                Some(serde_json::json!({
                    "id": i,
                    "name": it.name,
                    "w": it.w,
                    "h": it.h,
                    "sig": it.sig.to_string(),
                    "dir": m.dirs.get(it.dir_index as usize),
                    "removed": it.removed,
                }))
            })
            .collect();
        (m.items.len(), m.version, sample)
    };
    let body = serde_json::json!({
        "items": items,
        "version": version,
        "sample": sample,
        "resync_seq": state.resync_seq.load(Relaxed),
        "scan": {
            "found": d.scan_found.load(Relaxed),
            "done": d.scan_done.load(Relaxed),
        },
        "push": {
            "sweep_sent": d.sweep_sent.load(Relaxed),
            "sweep_bytes": d.sweep_bytes.load(Relaxed),
            "want_sent": d.want_sent.load(Relaxed),
            "want_recv": d.want_recv.load(Relaxed),
            "bad_recv": d.bad_recv.load(Relaxed),
            "produce_none": d.produce_none.load(Relaxed),
            "produce_timeout": d.produce_timeout.load(Relaxed),
            "stale_skip": d.stale_skip.load(Relaxed),
            "credit_recv": d.credit_recv.load(Relaxed),
        },
        "queues": d.queues.lock().unwrap().clone(),
        "previews": {
            "pending": state.previews.pending.lock().unwrap().len(),
            "pinned": state.previews.pinned_count(),
            "stopped": state.previews.stopped(),
        },
        "event_subs": state.events.receiver_count(),
    });
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body.to_string(),
    )
}

/// No favicon shipped — answer 204 instead of a 404 in the console.
pub async fn favicon() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}
