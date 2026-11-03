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
