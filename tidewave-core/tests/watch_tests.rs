use futures::channel::mpsc as futures_mpsc;
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use tidewave_core::ws::connection::unit_testable_ws_handler;
use tidewave_core::ws::WsState;

// ============================================================================
// Test Helpers
// ============================================================================

/// Creates fake WebSocket streams for testing
fn create_fake_websocket() -> (
    futures_mpsc::UnboundedSender<axum::extract::ws::Message>,
    futures_mpsc::UnboundedReceiver<axum::extract::ws::Message>,
    futures_mpsc::UnboundedSender<Result<axum::extract::ws::Message, axum::Error>>,
    futures_mpsc::UnboundedReceiver<Result<axum::extract::ws::Message, axum::Error>>,
) {
    let (ws_sender_tx, ws_sender_rx) = futures_mpsc::unbounded();
    let (ws_receiver_tx, ws_receiver_rx) = futures_mpsc::unbounded();
    (ws_sender_tx, ws_sender_rx, ws_receiver_tx, ws_receiver_rx)
}

/// Parse a watch event from a WebSocket message
fn parse_watch_event(msg: axum::extract::ws::Message) -> Option<Value> {
    match msg {
        axum::extract::ws::Message::Text(text) => serde_json::from_str(&text).ok(),
        _ => None,
    }
}

/// Wait for a specific event type with timeout
async fn wait_for_event(
    rx: &mut futures_mpsc::UnboundedReceiver<axum::extract::ws::Message>,
    event_type: &str,
    timeout_ms: u64,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }

        match tokio::time::timeout(remaining, rx.next()).await {
            Ok(Some(msg)) => {
                if let Some(event) = parse_watch_event(msg) {
                    if event.get("event").and_then(|e| e.as_str()) == Some(event_type) {
                        return Some(event);
                    }
                }
            }
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn test_watch_subscribe_success() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory for watching
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Send subscribe request
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": watch_path,
        "ref": "watch1"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Should receive subscribed confirmation
    let event = wait_for_event(&mut ws_out_rx, "subscribed", 1000).await;
    assert!(event.is_some(), "Expected subscribed event");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("watch1"));
}

#[tokio::test]
async fn test_watch_subscribe_relative_path_error() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Send subscribe request with relative path
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": "relative/path",
        "ref": "watch1"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Should receive unsubscribed with error
    let event = wait_for_event(&mut ws_out_rx, "unsubscribed", 1000).await;
    assert!(event.is_some(), "Expected unsubscribed event with error");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert!(event["error"]
        .as_str()
        .unwrap()
        .contains("absolute"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("watch1"));
}

#[tokio::test]
async fn test_watch_subscribe_nonexistent_path_error() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Send subscribe request with non-existent path
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": "/nonexistent/path/that/does/not/exist",
        "ref": "watch1"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Should receive unsubscribed with error
    let event = wait_for_event(&mut ws_out_rx, "unsubscribed", 1000).await;
    assert!(event.is_some(), "Expected unsubscribed event with error");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert!(event["error"]
        .as_str()
        .unwrap()
        .contains("does not exist"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("watch1"));
}

#[tokio::test]
async fn test_watch_file_created() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory for watching
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Subscribe to the directory
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": watch_path,
        "ref": "mywatch"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Wait for subscribed confirmation
    let _ = wait_for_event(&mut ws_out_rx, "subscribed", 1000).await;

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file in the watched directory
    let file_path = temp_dir.path().join("test_file.txt");
    tokio::fs::write(&file_path, "hello").await.unwrap();

    // Should receive created event
    let event = wait_for_event(&mut ws_out_rx, "created", 2000).await;
    assert!(event.is_some(), "Expected created event");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert!(event["path"]
        .as_str()
        .unwrap()
        .contains("test_file.txt"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("mywatch"));
}

#[tokio::test]
async fn test_watch_file_modified() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("existing_file.txt");
    tokio::fs::write(&file_path, "initial content").await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Subscribe to the directory
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": watch_path,
        "ref": "modwatch"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Wait for subscribed confirmation
    let _ = wait_for_event(&mut ws_out_rx, "subscribed", 1000).await;

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Modify the file
    tokio::fs::write(&file_path, "modified content").await.unwrap();

    // Should receive modified event
    let event = wait_for_event(&mut ws_out_rx, "modified", 2000).await;
    assert!(event.is_some(), "Expected modified event");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert!(event["path"]
        .as_str()
        .unwrap()
        .contains("existing_file.txt"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("modwatch"));
}

#[tokio::test]
async fn test_watch_file_deleted() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("file_to_delete.txt");
    tokio::fs::write(&file_path, "content").await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Subscribe to the directory
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": watch_path,
        "ref": "delwatch"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Wait for subscribed confirmation
    let _ = wait_for_event(&mut ws_out_rx, "subscribed", 1000).await;

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Delete the file
    tokio::fs::remove_file(&file_path).await.unwrap();

    // Should receive deleted event
    let event = wait_for_event(&mut ws_out_rx, "deleted", 2000).await;
    assert!(event.is_some(), "Expected deleted event");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert!(event["path"]
        .as_str()
        .unwrap()
        .contains("file_to_delete.txt"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("delwatch"));
}

#[tokio::test]
async fn test_watch_unsubscribe() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Subscribe
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": &watch_path,
        "ref": "unsub_test"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Wait for subscribed confirmation
    let _ = wait_for_event(&mut ws_out_rx, "subscribed", 1000).await;

    // Unsubscribe using ref
    let unsubscribe_msg = json!({
        "topic": "watch",
        "action": "unsubscribe",
        "ref": "unsub_test"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            unsubscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Should receive unsubscribed confirmation
    let event = wait_for_event(&mut ws_out_rx, "unsubscribed", 1000).await;
    assert!(event.is_some(), "Expected unsubscribed event");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("unsub_test"));
}

#[tokio::test]
async fn test_watch_ping_pong() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Send ping
    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text("ping".into())))
        .unwrap();

    // Should receive pong
    let msg = tokio::time::timeout(Duration::from_secs(1), ws_out_rx.next())
        .await
        .expect("Timeout waiting for pong")
        .expect("Channel closed");

    match msg {
        axum::extract::ws::Message::Text(text) => {
            assert_eq!(text.as_str(), "pong");
        }
        _ => panic!("Expected text message"),
    }
}

#[tokio::test]
async fn test_watch_concurrent_subscribers() {
    let state = WsState::new();

    // Create a temp directory
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Create two websocket connections
    let (ws1_out_tx, mut ws1_out_rx, ws1_in_tx, ws1_in_rx) = create_fake_websocket();
    let (ws2_out_tx, mut ws2_out_rx, ws2_in_tx, ws2_in_rx) = create_fake_websocket();

    let websocket_id1 = uuid::Uuid::new_v4();
    let websocket_id2 = uuid::Uuid::new_v4();

    let state1 = state.clone();
    let state2 = state.clone();

    // Start both handlers
    tokio::spawn(async move {
        unit_testable_ws_handler(ws1_out_tx, ws1_in_rx, state1, websocket_id1).await;
    });

    tokio::spawn(async move {
        unit_testable_ws_handler(ws2_out_tx, ws2_in_rx, state2, websocket_id2).await;
    });

    // Both subscribe to the same path with different refs
    let subscribe_msg1 = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": &watch_path,
        "ref": "client1_watch"
    });

    let subscribe_msg2 = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": &watch_path,
        "ref": "client2_watch"
    });

    ws1_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg1.to_string().into(),
        )))
        .unwrap();

    ws2_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg2.to_string().into(),
        )))
        .unwrap();

    // Both should receive subscribed confirmations
    let event1 = wait_for_event(&mut ws1_out_rx, "subscribed", 1000).await;
    let event2 = wait_for_event(&mut ws2_out_rx, "subscribed", 1000).await;
    assert!(event1.is_some());
    assert!(event2.is_some());
    assert_eq!(event1.unwrap().get("ref").and_then(|r| r.as_str()), Some("client1_watch"));
    assert_eq!(event2.unwrap().get("ref").and_then(|r| r.as_str()), Some("client2_watch"));

    // Give watchers time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file
    let file_path = temp_dir.path().join("shared_file.txt");
    tokio::fs::write(&file_path, "content").await.unwrap();

    // Both should receive the created event with their own ref
    let event1 = wait_for_event(&mut ws1_out_rx, "created", 2000).await;
    let event2 = wait_for_event(&mut ws2_out_rx, "created", 2000).await;

    assert!(event1.is_some(), "WebSocket 1 should receive created event");
    assert!(event2.is_some(), "WebSocket 2 should receive created event");
    assert_eq!(event1.unwrap().get("ref").and_then(|r| r.as_str()), Some("client1_watch"));
    assert_eq!(event2.unwrap().get("ref").and_then(|r| r.as_str()), Some("client2_watch"));
}

#[tokio::test]
async fn test_watch_subdirectory_events() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory with subdirectory
    let temp_dir = tempfile::tempdir().unwrap();
    let sub_dir = temp_dir.path().join("subdir");
    tokio::fs::create_dir(&sub_dir).await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Subscribe to the parent directory
    let subscribe_msg = json!({
        "topic": "watch",
        "action": "subscribe",
        "path": watch_path,
        "ref": "recursive_watch"
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            subscribe_msg.to_string().into(),
        )))
        .unwrap();

    // Wait for subscribed confirmation
    let _ = wait_for_event(&mut ws_out_rx, "subscribed", 1000).await;

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file in the subdirectory
    let file_path = sub_dir.join("nested_file.txt");
    tokio::fs::write(&file_path, "nested content").await.unwrap();

    // Should receive created event for nested file (recursive watching)
    let event = wait_for_event(&mut ws_out_rx, "created", 2000).await;
    assert!(event.is_some(), "Expected created event for nested file");
    let event = event.unwrap();
    assert_eq!(event.get("topic").and_then(|t| t.as_str()), Some("watch"));
    assert!(event["path"]
        .as_str()
        .unwrap()
        .contains("nested_file.txt"));
    assert_eq!(event.get("ref").and_then(|r| r.as_str()), Some("recursive_watch"));
}
