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

/// Bound on how long the pump waits for one thumbnail produce — a wedged
/// read (network mount) must not stall pushes for every other image.
const PRODUCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    let conn_id = state
        .dbg
        .next_conn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;

    // Background push pump: drains queue while credit lasts.
    let pump = {
        let out_tx = out_tx.clone();
        let queue = queue.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let mut last_resync = state.resync_seq.load(std::sync::atomic::Ordering::Relaxed);
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
                    // Thumbnails keep flowing no matter what actions are in
                    // flight — a wedged delete/move must never freeze image
                    // loading. Paths queued for deletion are skipped
                    // individually below instead of pausing the sweep.
                    if q.may_send() {
                        while ids.len() < PUMP_WIDTH {
                            match q.next_id() {
                                Some(id) => ids.push(id),
                                None => break,
                            }
                        }
                    }
                    // /debugz snapshot: what the pump sees right now.
                    let mut snap = q.dbg_json();
                    snap["idle"] = ids.is_empty().into();
                    snap["t_ms"] = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| serde_json::json!(d.as_millis() as u64))
                        .unwrap_or_default();
                    state
                        .dbg
                        .queues
                        .lock()
                        .unwrap()
                        .insert(conn_id, snap);
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
                        // Queued for delete/move: the file is about to
                        // vanish, so skip this pass. Deliberately not marked
                        // pushed — if the action fails (or times out) the
                        // item is restored and must still get a thumbnail.
                        Some((path, _)) if state.actions.is_pending(&path) => {}
                        Some((path, sig)) => jobs.push((id, path, sig)),
                        None => queue.lock().await.mark_pushed(id),
                    }
                }
                // Start every produce before awaiting any of them, then
                // send in COMPLETION order — awaiting in spawn order lets
                // one cold produce stall a batch of instant cache hits.
                let previews = state.previews.clone();
                let mut handles = futures_util::stream::FuturesUnordered::new();
                for (id, path, sig) in jobs {
                    let previews = previews.clone();
                    let produce_path = path.clone();
                    let state = state.clone();
                    // One hung produce (wedged mount read, poisoned file)
                    // must not stall the whole sweep: the drain below waits
                    // for every handle. The detached blocking call keeps
                    // running — only the wait is bounded.
                    handles.push(tokio::spawn(async move {
                        let res = tokio::time::timeout(
                            PRODUCE_TIMEOUT,
                            tokio::task::spawn_blocking(move || {
                                previews.get_or_produce(&produce_path)
                            }),
                        )
                        .await;
                        if res.is_err() {
                            state.dbg.produce_timeout.fetch_add(
                                1,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                        (id, path, sig, res.ok().and_then(|r| r.ok()).flatten())
                    }));
                }
                if handles.is_empty() {
                    // Every id in this batch is queued for delete/move — give
                    // the workers a moment rather than spinning on the queue.
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
                // Thumbs go out in EV_THUMB_BATCH frames — one WS message
                // per group instead of one per thumb cuts message dispatch
                // and lets the client decode them in parallel off one Blob.
                let mut batch: Vec<(u32, u64, Vec<u8>)> = Vec::new();
                while let Some(done) = handles.next().await {
                    let Ok((id, path, sig, produced)) = done else {
                        continue;
                    };
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
                                state.dbg.stale_skip.fetch_add(
                                    1,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                continue;
                            }
                            let bytes = p.jpeg.len() as u64;
                            q.commit_send(id, bytes);
                            state
                                .dbg
                                .sweep_sent
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            state
                                .dbg
                                .sweep_bytes
                                .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
                            drop(q);
                            batch.push((id, sig, p.jpeg.as_ref().clone()));
                            if batch.len() >= 4 {
                                let _ = out_tx
                                    .send(Frame::new(
                                        EV_THUMB_BATCH,
                                        0,
                                        thumb_batch_payload(&batch),
                                    ))
                                    .await;
                                batch.clear();
                            }
                        }
                        None => {
                            q.mark_pushed(id);
                            state.dbg.produce_none.fetch_add(
                                1,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                        }
                    }
                }
                if !batch.is_empty() {
                    let _ = out_tx
                        .send(Frame::new(EV_THUMB_BATCH, 0, thumb_batch_payload(&batch)))
                        .await;
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
                    state
                        .dbg
                        .want_recv
                        .fetch_add(ids.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    for id in ids {
                        queue.lock().await.mark_pushed(id);
                        // Path + sig under one read — the frame must be
                        // labelled consistently (ids shift on resync).
                        let (path, sig) = {
                            let m = state.manifest.read().unwrap();
                            match m.path_of(id) {
                                Some(p) => {
                                    (p, m.items.get(id as usize).map(|i| i.sig).unwrap_or(0))
                                }
                                None => continue,
                            }
                        };
                        let previews = state.previews.clone();
                        let out_tx = out_tx.clone();
                        let state = state.clone();
                        tokio::spawn(async move {
                            let res = tokio::time::timeout(
                                PRODUCE_TIMEOUT,
                                tokio::task::spawn_blocking(move || {
                                    previews.get_or_produce(&path)
                                }),
                            )
                            .await;
                            if res.is_err() {
                                state.dbg.produce_timeout.fetch_add(
                                    1,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            let produced =
                                res.ok().and_then(|r| r.ok()).flatten();
                            if let Some(p) = produced {
                                state
                                    .dbg
                                    .want_sent
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let _ = out_tx
                                    .send(Frame::new(
                                        EV_THUMB,
                                        frame.req_id,
                                        thumb_payload(id, sig, &p.jpeg),
                                    ))
                                    .await;
                            } else {
                                state.dbg.produce_none.fetch_add(
                                    1,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
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
                            // Ack the *enqueue* right away: the op is now in
                            // the server's queue and will be finished (also
                            // across Ctrl-C), so the client must stop
                            // counting it against any local deadline.
                            if let Ok(json) = serde_json::to_string(&serde_json::json!({
                                "ev": "action-queued",
                                "tag": env.tag,
                                "id": id,
                                "kind": kind,
                            })) {
                                let _ =
                                    ev_tx.send(Frame::new(EV_EVENT, 0, json.into_bytes())).await;
                            }
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
                                let _ =
                                    ev_tx.send(Frame::new(EV_EVENT, 0, json.into_bytes())).await;
                            }
                        }
                    }
                }
            }
            CL_CREDIT => {
                if let Some(bytes) = decode_credit(&frame.payload) {
                    state
                        .dbg
                        .credit_recv
                        .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
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
    state.dbg.queues.lock().unwrap().remove(&conn_id);
}
