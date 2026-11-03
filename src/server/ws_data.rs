//! /data WebSocket: control, events, and thumbnail push.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};

use super::{proto::*, ActionEnvelope, PushQueue, ServerEvent, ServerState};

pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<ServerState>>) -> Response {
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

    // Outbound channel: push/ev/event producers → sink.
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
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

    // Broadcast events → EVENT frames.
    let mut events_rx = state.events.subscribe();
    let events_task = {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            while let Ok(ServerEvent::Json(json)) = events_rx.recv().await {
                if out_tx
                    .send(Frame::new(EV_EVENT, 0, json.into_bytes()))
                    .await
                    .is_err()
                {
                    break;
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
            loop {
                let id = {
                    let mut q = queue.lock().await;
                    // The manifest may still be growing (streaming scan) —
                    // let the sweep pick up newly appended ids.
                    q.set_count(state.item_count());
                    if !q.may_send() {
                        None
                    } else {
                        q.next_id()
                    }
                };
                let Some(id) = id else {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                };
                let Some(path) = state.path_of(id) else {
                    queue.lock().await.mark_pushed(id);
                    continue;
                };
                let previews = state.previews.clone();
                let produced = tokio::task::spawn_blocking(move || previews.get_or_produce(&path))
                    .await
                    .ok()
                    .flatten();
                let mut q = queue.lock().await;
                match produced {
                    Some(p) => {
                        let bytes = p.jpeg.len() as u64;
                        let sig = state
                            .manifest
                            .read()
                            .unwrap()
                            .items
                            .get(id as usize)
                            .map(|i| i.sig)
                            .unwrap_or(0);
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
                        let Some(path) = state.path_of(id) else {
                            continue;
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
                                        thumb_payload(id, 0, &p.jpeg),
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
                    if let Some(path) = state.path_of(env.action.id()) {
                        state.actions.dispatch(env.tag, env.action, path);
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
