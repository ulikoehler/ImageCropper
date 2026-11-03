//! /bulk WebSocket: manifest + full-resolution image transfer (chunked).

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::image_utils::{decode_image_bytes, resize_to_fit};

use super::{proto::*, ServerState};

/// Cap on the decoded full image (same policy as the native Loader).
const FULL_MAX_DIM: u32 = 3840;

/// Requested encoding for GET_FULL payloads.
const FMT_ORIGINAL: u8 = 0;
/// Fullscreen-size preview JPEG (≤1920) from the shared `~/.cache` tier —
/// used by hold-`f` in the web client.
const FMT_SCREEN: u8 = 2;

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if !super::ws_authed(&state, &query) {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    ws.max_message_size(1024)
        .on_upgrade(move |socket| handle(socket, state))
}

async fn handle(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(64);
    let send_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink
                .send(Message::Binary(frame.encode().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Binary(bytes) = msg else {
            continue;
        };
        let Some(frame) = Frame::decode(&bytes) else {
            continue;
        };
        match frame.msg_type {
            BK_GET_MANIFEST => {
                let manifest = state.manifest.read().unwrap().clone();
                // JSON so any client (JS stub now, wasm later) can decode.
                let payload = serde_json::to_vec(&manifest).unwrap_or_default();
                send_chunked(&out_tx, frame.req_id, BK_MANIFEST, payload).await;
            }
            BK_GET_FULL => {
                if let Some((id, _sig, want_fmt, q)) = decode_get_full(&frame.payload) {
                    let state = state.clone();
                    let out_tx = out_tx.clone();
                    let req_id = frame.req_id;
                    tokio::spawn(async move {
                        let Some(path) = state.path_of(id) else {
                            return;
                        };
                        // Shutting down — no new production, and a client
                        // about to lose its socket won't miss the reply.
                        if state.previews.stopped() {
                            return;
                        }
                        let item = state
                            .manifest
                            .read()
                            .unwrap()
                            .items
                            .get(id as usize)
                            .cloned();
                        let produced =
                            tokio::task::spawn_blocking(move || produce_full(&path, want_fmt, q))
                                .await
                                .ok()
                                .flatten();
                        if let Some(((bytes, w, h), sig)) = produced.zip(item.map(|i| i.sig)) {
                            let _ = out_tx
                                .send(Frame::new(
                                    BK_FULL_HEAD,
                                    req_id,
                                    full_head_payload(sig, w, h, want_fmt, bytes.len() as u64),
                                ))
                                .await;
                            send_chunked(&out_tx, req_id, BK_FULL_CHUNK, bytes).await;
                        }
                    });
                }
            }
            _ => {}
        }
    }

    send_task.abort();
}

/// Split `payload` into BULK_CHUNK frames, LAST flag on the final chunk.
async fn send_chunked(out_tx: &mpsc::Sender<Frame>, req_id: u32, msg_type: u8, payload: Vec<u8>) {
    if payload.is_empty() {
        let _ = out_tx
            .send(Frame::new(msg_type, req_id, Vec::new()).last())
            .await;
        return;
    }
    let n = payload.len().div_ceil(BULK_CHUNK);
    for (i, chunk) in payload.chunks(BULK_CHUNK).enumerate() {
        let mut f = Frame::new(msg_type, req_id, chunk.to_vec());
        if i == n - 1 {
            f.flags |= FLAG_LAST;
        }
        f.flags |= (i as u32) << 8; // seq in high bits
        if out_tx.send(f).await.is_err() {
            return;
        }
    }
}

/// Produce a full-image payload: original bytes, or a JPEG re-encode of the
/// resized (≤FULL_MAX_DIM) decode. Returns (bytes, w, h) of the *decoded*
/// image for layout purposes.
fn produce_full(path: &std::path::Path, fmt: u8, quality: u8) -> Option<(Vec<u8>, u32, u32)> {
    match fmt {
        FMT_SCREEN => crate::fs_utils::previews::produce_screen_jpeg(path),
        FMT_ORIGINAL => {
            let bytes = std::fs::read(path).ok()?;
            let (w, h) = image::image_dimensions(path).unwrap_or((0, 0));
            Some((bytes, w, h))
        }
        _ => {
            let bytes = std::fs::read(path).ok()?;
            // Restart-marker JPEGs: GPU decode + resize in one session —
            // the full-res image never reaches the host.
            let img = match crate::gpujpeg::open(&bytes) {
                Some(session) => session.resize_image(FULL_MAX_DIM, FULL_MAX_DIM)?,
                None => {
                    let img = decode_image_bytes(path, &bytes).ok()?;
                    resize_to_fit(img, FULL_MAX_DIM, FULL_MAX_DIM)
                }
            };
            let (w, h) = (img.width(), img.height());
            let mut out = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                std::io::BufWriter::new(std::io::Cursor::new(&mut out)),
                if quality == 0 { 80 } else { quality },
            );
            img.to_rgb8().write_with_encoder(encoder).ok()?;
            Some((out, w, h))
        }
    }
}
