//! /data WebSocket: control, events, and thumbnail push.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::sync::{mpsc, Mutex};

use super::{proto::*, ActionEnvelope, PushQueue, ServerEvent, ServerState};

pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !super::ws_authed(&state, &query) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| handle(socket, state))
}

async fn handle(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sink, mut stream) = socket.split();

    // First message must be HELLO with the token.
    let hello_ok = match stream.next().await {
        Some(Ok(Message::Binary(bytes))) => Frame::decode(&bytes)
            .filter(|f| f.msg_type == CL_HELLO)
            .and_then(|f| serde_json::from_slice::<serde_json::Value>(&f.payload).ok())
            .map(|v| {
                v.get("token")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .map(|tok| tok.as_deref() == state.token.as_deref() || state.token.is_none())
            .unwrap_or(false),
        _ => false,
    };
    if !hello_ok {
        let _ = sink.send(Message::Close(None)).await;
        return;
    }

    // Two outbound channels: events (small, latency-critical) and thumbs
    // (bulk). The send task drains events first — a flood of thumbnail
    // pushes must not delay action-done acks or manifest patches.
    let (ev_tx, mut ev_rx) = mpsc::channel::<Frame>(256);
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let send_task = tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                biased;
                f = ev_rx.recv() => f,
                f = out_rx.recv() => f,
            };
            let Some(frame) = frame else { break };
            if sink
                .send(Message::Binary(frame.encode().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Broadcast events → EVENT frames.
    let mut events_rx = state.events.subscribe();
    let events_task = {
        let out_tx = ev_tx.clone();
        tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    Ok(ServerEvent::Json(json)) => {
                        if out_tx
                            .send(Frame::new(EV_EVENT, 0, json.into_bytes()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // The ring overflowed (delete storms, big rescans) —
                    // events were dropped. Stay alive and tell the client
                    // to re-pull the manifest so missed tombstones/upserts
                    // can't leave it permanently inconsistent.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if out_tx
                            .send(Frame::new(EV_EVENT, 0, b"{\"ev\":\"fs-changed\"}".to_vec()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    let queue = Arc::new(Mutex::new(PushQueue::new(
        state.manifest.read().unwrap().items.len(),
        state.initial_credit,
    )));

    // Background push pump: drains queue while credit lasts.
    let pump = {
        let out_tx = out_tx.clone();
        let queue = queue.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut last_resync = state
                .resync_seq
                .load(std::sync::atomic::Ordering::Relaxed);
            // Produce up to PUMP_WIDTH thumbs concurrently per batch — a
            // serial produce+send loop caps cold-cache delivery at ~20/s.
            const PUMP_WIDTH: usize = 8;
            loop {
                let mut ids = Vec::new();
                {
                    let mut q = queue.lock().await;
                    // A resync reassigned every id — queued wants/boosts and
                    // the pushed-set now name different files. Restart fresh.
                    let seq = state.resync_seq.load(std::sync::atomic::Ordering::Relaxed);
                    if seq != last_resync {
                        last_resync = seq;
                        q.reset(state.item_count());
                    }
                    // While destructive actions drain, the listing is in flux
                    // — sweeping thumbs for about-to-vanish files is wasted
                    // work competing with action-done delivery.
                    if state.actions.pending() == 0 && q.may_send() {
                        while ids.len() < PUMP_WIDTH {
                            match q.next_id() {
                                Some(id) => ids.push(id),
                                None => break,
                            }
                        }
                    }
                }
                if ids.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
                // Path + sig under one read so pushed frames are labelled
                // consistently — ids shift on resync.
                let mut jobs = Vec::new();
                for id in ids {
                    let found = {
                        let m = state.manifest.read().unwrap();
                        m.path_of(id)
                            .map(|p| (p, m.items.get(id as usize).map(|i| i.sig).unwrap_or(0)))
                    };
                    match found {
                        Some((path, sig)) => jobs.push((id, path, sig)),
                        None => queue.lock().await.mark_pushed(id),
                    }
                }
                // Start every produce before awaiting any of them.
                let previews = state.previews.clone();
                let mut handles = Vec::new();
                for (id, path, sig) in jobs {
                    let previews = previews.clone();
                    let produce_path = path.clone();
                    handles.push((
                        id,
                        path,
                        sig,
                        tokio::task::spawn_blocking(move || {
                            previews.get_or_produce(&produce_path)
                        }),
                    ));
                }
                for (id, path, sig, handle) in handles {
                    let produced = handle.await.ok().flatten();
                    let mut q = queue.lock().await;
                    match produced {
                        Some(p) => {
                            // Re-verify the id still names the produced file —
                            // a resync/upsert mid-produce would make this push
                            // a wrong thumbnail for the current item.
                            let still_current = {
                                let m = state.manifest.read().unwrap();
                                m.path_of(id).as_deref() == Some(path.as_path())
                                    && m.items.get(id as usize).map(|i| i.sig) == Some(sig)
                            };
                            if !still_current {
                                continue;
                            }
                            let bytes = p.jpeg.len() as u64;
                            q.commit_send(id, bytes);
                            drop(q);
                            let _ = out_tx
                                .send(Frame::new(EV_THUMB, 0, thumb_payload(id, sig, &p.jpeg)))
                                .await;
                        }
                        None => {
                            q.mark_pushed(id);
                        }
                    }
                }
            }
        })
    };

    // Inbound control loop.
    while let Some(Ok(msg)) = stream.next().await {
        let Message::Binary(bytes) = msg else {
            continue;
        };
        let Some(frame) = Frame::decode(&bytes) else {
            continue;
        };
        match frame.msg_type {
            CL_WANT => {
                if let Some(ids) = decode_want(&frame.payload) {
                    for id in ids {
                        queue.lock().await.mark_pushed(id);
                        // Path + sig under one read — the frame must be
                        // labelled consistently (ids shift on resync).
                        let (path, sig) = {
                            let m = state.manifest.read().unwrap();
                            match m.path_of(id) {
                                Some(p) => (
                                    p,
                                    m.items.get(id as usize).map(|i| i.sig).unwrap_or(0),
                                ),
                                None => continue,
                            }
                        };
                        let previews = state.previews.clone();
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let produced =
                                tokio::task::spawn_blocking(move || previews.get_or_produce(&path))
                                    .await
                                    .ok()
                                    .flatten();
                            if let Some(p) = produced {
                                let _ = out_tx
                                    .send(Frame::new(
                                        EV_THUMB,
                                        frame.req_id,
                                        thumb_payload(id, sig, &p.jpeg),
                                    ))
                                    .await;
                            }
                        });
                    }
                }
            }
            CL_VIEWPORT => {
                if let Some((first, last, dir)) = decode_viewport(&frame.payload) {
                    queue.lock().await.viewport(first, last, dir);
                }
            }
            CL_FOCUS => {
                if let Some(id) = decode_focus(&frame.payload) {
                    queue.lock().await.focus(id);
                }
            }
            CL_ACTION => {
                if let Ok(env) = serde_json::from_slice::<ActionEnvelope>(&frame.payload) {
                    let kind = env.action.kind();
                    let id = env.action.id();
                    match state.path_of(id) {
                        Some(path) => {
                            state.actions.dispatch(env.tag, env.action, path.clone());
                            // "Marked for deletion": tombstone queued
                            // delete/move ops immediately so client reloads
                            // and fs-watch rescans can't resurrect the file
                            // before the worker lands the op. A failure
                            // restores it via the completion path.
                            if matches!(
                                kind,
                                super::actions::ActionKind::Delete
                                    | super::actions::ActionKind::Move
                            ) {
                                state.remove_path(&path);
                            }
                        }
                        // Gone/tombstoned — still ack, otherwise the
                        // client's pending-action counter never clears.
                        None => {
                            if let Ok(json) = serde_json::to_string(&serde_json::json!({
                                "ev": "action-done",
                                "tag": env.tag,
                                "ok": false,
                                "id": id,
                                "kind": kind,
                                "error": "item no longer in manifest",
                            })) {
                                let _ = ev_tx
                                    .send(Frame::new(EV_EVENT, 0, json.into_bytes()))
                                    .await;
                            }
                        }
                    }
                }
            }
            CL_CREDIT => {
                if let Some(bytes) = decode_credit(&frame.payload) {
                    queue.lock().await.add_credit(bytes);
                }
            }
            CL_PAUSE => queue.lock().await.set_paused(true),
            CL_RESUME => queue.lock().await.set_paused(false),
            CL_PING => {
                let _ = out_tx
                    .send(Frame::new(EV_PONG, frame.req_id, frame.payload))
                    .await;
            }
            _ => {}
        }
    }

    pump.abort();
    events_task.abort();
    send_task.abort();
}
