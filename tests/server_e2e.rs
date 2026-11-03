//! End-to-end: real server on an ephemeral port, real WS client driving the
//! binary protocol: HELLO → manifest → WANT/thumbs → ACTION delete.

use futures_util::{SinkExt, StreamExt};
use imagecropper::fs_utils::TRASH_DIR;
use imagecropper::server::proto::*;
use imagecropper::server::{build_router, ServeMode, ServerState};
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod common;
use common::{solid_image, write_image};

fn frame_bytes(msg_type: u8, req_id: u32, payload: Vec<u8>) -> Vec<u8> {
    Frame::new(msg_type, req_id, payload).encode()
}

fn make_want(ids: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    for id in ids {
        p.extend_from_slice(&id.to_le_bytes());
    }
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_hello_manifest_thumbs_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for i in 0..3 {
        write_image(
            root.join(format!("img{i}.png")),
            &solid_image(30 + i as u32 * 10, 20, [10, 20, 30, 255]),
        );
    }
    let files: Vec<_> = (0..3).map(|i| root.join(format!("img{i}.png"))).collect();
    let state = ServerState::new(
        files.clone(),
        vec![root.to_path_buf()],
        ServeMode::Album,
        2,
        None,
        4096,
        None,
    );
    state.spawn_pumps();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });

    // ---- /data channel ----
    let url = format!("ws://{addr}/data");
    let (mut data, _) = connect_async(&url).await.expect("connect /data");
    data.send(Message::Binary(
        frame_bytes(
            CL_HELLO,
            0,
            serde_json::to_vec(&json!({"token": null, "manifest_version": 0})).unwrap(),
        )
        .into(),
    ))
    .await
    .unwrap();

    // ---- /bulk channel: manifest ----
    let url = format!("ws://{addr}/bulk");
    let (mut bulk, _) = connect_async(&url).await.expect("connect /bulk");
    bulk.send(Message::Binary(
        frame_bytes(BK_GET_MANIFEST, 7, Vec::new()).into(),
    ))
    .await
    .unwrap();

    let mut manifest_bytes = Vec::new();
    let manifest = loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), bulk.next())
            .await
            .expect("manifest timeout")
            .unwrap()
            .unwrap();
        let Message::Binary(b) = msg else { continue };
        let f = Frame::decode(&b).unwrap();
        assert_eq!(f.msg_type, BK_MANIFEST);
        manifest_bytes.extend_from_slice(&f.payload);
        if f.flags & FLAG_LAST != 0 {
            break serde_json::from_slice::<serde_json::Value>(&manifest_bytes).unwrap();
        }
    };
    assert_eq!(manifest["items"].as_array().unwrap().len(), 3);
    assert_eq!(manifest["mode"], "Album");
    assert_eq!(manifest["items"][0]["w"], 30);
    let sig0 = manifest["items"][0]["sig"].as_str().unwrap().to_string();

    // ---- /data: WANT → THUMB frames ----
    data.send(Message::Binary(
        frame_bytes(CL_WANT, 0, make_want(&[0, 1, 2])).into(),
    ))
    .await
    .unwrap();
    let mut got_ids = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while got_ids.len() < 3 && std::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), data.next())
            .await
            .expect("thumb timeout")
            .unwrap()
            .unwrap();
        let Message::Binary(b) = msg else { continue };
        let f = Frame::decode(&b).unwrap();
        if f.msg_type == EV_THUMB {
            let id = u32::from_le_bytes(f.payload[0..4].try_into().unwrap());
            let jpeg = &f.payload[12..];
            assert_eq!(&jpeg[0..2], &[0xFF, 0xD8], "JPEG magic");
            got_ids.push(id);
        }
    }
    got_ids.sort();
    assert_eq!(got_ids, vec![0, 1, 2]);

    // ---- /data: ACTION delete id=0 → action-done + file trashed ----
    let action = json!({"tag": 99, "kind": "delete", "id": 0, "sig": sig0});
    data.send(Message::Binary(
        frame_bytes(CL_ACTION, 0, serde_json::to_vec(&action).unwrap()).into(),
    ))
    .await
    .unwrap();
    let mut done_ok = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done_ok && std::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), data.next())
            .await
            .expect("action-done timeout")
            .unwrap()
            .unwrap();
        let Message::Binary(b) = msg else { continue };
        let f = Frame::decode(&b).unwrap();
        if f.msg_type == EV_EVENT {
            let ev: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
            if ev["ev"] == "action-done" && ev["tag"] == 99 {
                assert_eq!(ev["ok"], true, "delete failed: {ev}");
                done_ok = true;
            }
        }
    }
    assert!(done_ok);
    assert!(!files[0].exists());
    assert!(root.join(TRASH_DIR).join("img0.png").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_healthz_and_static() {
    let tmp = tempfile::tempdir().unwrap();
    write_image(tmp.path().join("a.png"), &solid_image(8, 8, [0, 0, 0, 255]));
    let state = ServerState::new(
        vec![tmp.path().join("a.png")],
        vec![tmp.path().to_path_buf()],
        ServeMode::Crop,
        1,
        None,
        4096,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move { axum::serve(listener, router).await });

    let resp = reqwest_get(&format!("http://{addr}/healthz")).await;
    assert!(resp.contains("ok 1"), "{resp}");
    let page = reqwest_get(&format!("http://{addr}/")).await;
    assert!(page.contains("/app.js"));
    let js = reqwest_get(&format!("http://{addr}/app.js")).await;
    assert!(js.contains("WebSocket"));
}

/// Minimal HTTP/1.1 GET over a raw TcpStream (no reqwest dep).
async fn reqwest_get(url: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let url = url.strip_prefix("http://").unwrap();
    let (host, path) = url.split_once('/').unwrap();
    let mut s = tokio::net::TcpStream::connect(host).await.unwrap();
    s.write_all(
        format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    buf
}
