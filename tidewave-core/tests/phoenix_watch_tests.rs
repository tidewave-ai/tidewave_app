//! Integration tests for Phoenix-based file watching.
//!
//! These tests verify the WatchChannel implementation using the Phoenix protocol.

use futures::SinkExt;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

async fn start_test_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        // Keep the shutdown sender alive
        std::mem::forget(shutdown_tx);

        let config = tidewave_core::Config::default();
        tidewave_core::serve_http_server_with_listener(config, listener, async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    format!("ws://127.0.0.1:{}/socket/websocket", port)
}

/// Encode a Phoenix message
fn encode_phoenix_msg(join_ref: Option<&str>, msg_ref: &str, topic: &str, event: &str, payload: Value) -> String {
    json!([join_ref, msg_ref, topic, event, payload]).to_string()
}

/// Decode a Phoenix message
fn decode_phoenix_msg(text: &str) -> Option<(Option<String>, Option<String>, String, String, Value)> {
    let arr: Vec<Value> = serde_json::from_str(text).ok()?;
    if arr.len() != 5 {
        return None;
    }
    Some((
        arr[0].as_str().map(String::from),
        arr[1].as_str().map(String::from),
        arr[2].as_str()?.to_string(),
        arr[3].as_str()?.to_string(),
        arr[4].clone(),
    ))
}

/// Wait for a message matching a predicate
async fn wait_for_msg<F>(
    stream: &mut (impl futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    predicate: F,
    timeout_ms: u64,
) -> Option<(Option<String>, Option<String>, String, String, Value)>
where
    F: Fn(&str, &str, &Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }

        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Some((join_ref, msg_ref, topic, event, payload)) = decode_phoenix_msg(&text) {
                    if predicate(&topic, &event, &payload) {
                        return Some((join_ref, msg_ref, topic, event, payload));
                    }
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn test_phoenix_watch_join_success() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Create a temp directory for watching
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Send join message
    let join_msg = encode_phoenix_msg(Some("1"), "1", &topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for phx_reply with ok status
    let result = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await;

    assert!(result.is_some(), "Expected successful join reply");
    let (_, _, _, _, payload) = result.unwrap();
    assert_eq!(payload["response"]["status"], "watching");
}

#[tokio::test]
async fn test_phoenix_watch_join_invalid_topic() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Send join with invalid topic (missing path)
    let topic = "watch:";
    let join_msg = encode_phoenix_msg(Some("1"), "1", topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for phx_reply with error status
    let result = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("error")
    }, 2000).await;

    assert!(result.is_some(), "Expected error join reply");
    let (_, _, _, _, payload) = result.unwrap();
    assert!(payload["response"]["reason"].as_str().unwrap().contains("invalid topic"));
}

#[tokio::test]
async fn test_phoenix_watch_join_nonexistent_path() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    let topic = "watch:/nonexistent/path/that/does/not/exist";
    let join_msg = encode_phoenix_msg(Some("1"), "1", topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for phx_reply with error status
    let result = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("error")
    }, 2000).await;

    assert!(result.is_some(), "Expected error join reply");
    let (_, _, _, _, payload) = result.unwrap();
    assert!(payload["response"]["reason"].as_str().unwrap().contains("does not exist"));
}

#[tokio::test]
async fn test_phoenix_watch_file_created() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Create a temp directory for watching
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Join the watch channel
    let join_msg = encode_phoenix_msg(Some("1"), "1", &topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for successful join
    let _ = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await.expect("Join should succeed");

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file in the watched directory
    let file_path = temp_dir.path().join("test_file.txt");
    tokio::fs::write(&file_path, "hello").await.unwrap();

    // Wait for created event
    let result = wait_for_msg(&mut read, |t, e, _| t == topic && e == "created", 3000).await;

    assert!(result.is_some(), "Expected created event");
    let (_, _, _, _, payload) = result.unwrap();
    assert!(payload["path"].as_str().unwrap().contains("test_file.txt"));
}

#[tokio::test]
async fn test_phoenix_watch_file_modified() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("existing_file.txt");
    tokio::fs::write(&file_path, "initial content").await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Join the watch channel
    let join_msg = encode_phoenix_msg(Some("1"), "1", &topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for successful join
    let _ = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await.expect("Join should succeed");

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Modify the file
    tokio::fs::write(&file_path, "modified content").await.unwrap();

    // Wait for modified event
    let result = wait_for_msg(&mut read, |t, e, _| t == topic && e == "modified", 3000).await;

    assert!(result.is_some(), "Expected modified event");
    let (_, _, _, _, payload) = result.unwrap();
    assert!(payload["path"].as_str().unwrap().contains("existing_file.txt"));
}

#[tokio::test]
async fn test_phoenix_watch_file_deleted() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("file_to_delete.txt");
    tokio::fs::write(&file_path, "content").await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Join the watch channel
    let join_msg = encode_phoenix_msg(Some("1"), "1", &topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for successful join
    let _ = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await.expect("Join should succeed");

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Delete the file
    tokio::fs::remove_file(&file_path).await.unwrap();

    // Wait for deleted event
    let result = wait_for_msg(&mut read, |t, e, _| t == topic && e == "deleted", 3000).await;

    assert!(result.is_some(), "Expected deleted event");
    let (_, _, _, _, payload) = result.unwrap();
    assert!(payload["path"].as_str().unwrap().contains("file_to_delete.txt"));
}

#[tokio::test]
async fn test_phoenix_watch_leave() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Create a temp directory
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Join the watch channel
    let join_msg = encode_phoenix_msg(Some("1"), "1", &topic, "phx_join", json!({}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Wait for successful join
    let _ = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await.expect("Join should succeed");

    // Send leave message
    let leave_msg = encode_phoenix_msg(Some("1"), "2", &topic, "phx_leave", json!({}));
    write.send(Message::Text(leave_msg.into())).await.unwrap();

    // Wait for leave reply
    let result = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await;

    assert!(result.is_some(), "Expected successful leave reply");
}

#[tokio::test]
async fn test_phoenix_watch_wsl_param() {
    let url = start_test_server().await;
    let (ws_stream, _) = connect_async(&url).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();

    // Create a temp directory
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Join with is_wsl parameter
    let join_msg = encode_phoenix_msg(Some("1"), "1", &topic, "phx_join", json!({"is_wsl": false}));
    write.send(Message::Text(join_msg.into())).await.unwrap();

    // Should still succeed (is_wsl=false has no effect on non-WSL systems)
    let result = wait_for_msg(&mut read, |t, e, p| {
        t == topic && e == "phx_reply" && p.get("status").and_then(|s| s.as_str()) == Some("ok")
    }, 2000).await;

    assert!(result.is_some(), "Expected successful join reply");
}
