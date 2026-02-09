use futures::channel::mpsc as futures_mpsc;
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tidewave_core::phoenix::{events, PhxMessage};
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

fn send_phx(
    tx: &futures_mpsc::UnboundedSender<Result<axum::extract::ws::Message, axum::Error>>,
    msg: PhxMessage,
) {
    tx.unbounded_send(Ok(axum::extract::ws::Message::Text(msg.encode().into())))
        .unwrap();
}

fn parse_phx(msg: axum::extract::ws::Message) -> Option<PhxMessage> {
    match msg {
        axum::extract::ws::Message::Text(text) => PhxMessage::decode(&text).ok(),
        _ => None,
    }
}

/// Wait for a Phoenix message with a specific event type
async fn wait_for_phx_event(
    rx: &mut futures_mpsc::UnboundedReceiver<axum::extract::ws::Message>,
    event_type: &str,
    timeout_ms: u64,
) -> Option<PhxMessage> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }

        match tokio::time::timeout(remaining, rx.next()).await {
            Ok(Some(msg)) => {
                if let Some(phx) = parse_phx(msg) {
                    if phx.event == event_type {
                        return Some(phx);
                    }
                }
            }
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

/// Wait for a phx_reply message
async fn wait_for_reply(
    rx: &mut futures_mpsc::UnboundedReceiver<axum::extract::ws::Message>,
    timeout_ms: u64,
) -> Option<PhxMessage> {
    wait_for_phx_event(rx, events::PHX_REPLY, timeout_ms).await
}

/// Send a phx_join for a watch topic and wait for the reply
async fn join_watch(
    ws_in_tx: &futures_mpsc::UnboundedSender<Result<axum::extract::ws::Message, axum::Error>>,
    ws_out_rx: &mut futures_mpsc::UnboundedReceiver<axum::extract::ws::Message>,
    reference: &str,
    path: &str,
) -> PhxMessage {
    send_phx(
        ws_in_tx,
        PhxMessage::new(
            format!("watch:{}", reference),
            events::PHX_JOIN,
            json!({"path": path}),
        )
        .with_ref("1"),
    );

    wait_for_reply(ws_out_rx, 1000)
        .await
        .expect("Expected phx_reply for join")
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

    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "watch1", &watch_path).await;

    assert_eq!(reply.topic, "watch:watch1");
    assert_eq!(reply.payload["status"], "ok");
    assert!(reply.payload["response"]["path"].as_str().is_some());
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

    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "watch1", "relative/path").await;

    assert_eq!(reply.payload["status"], "error");
    assert!(reply.payload["response"]["reason"]
        .as_str()
        .unwrap()
        .contains("absolute"));
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
    let reply = join_watch(
        &ws_in_tx,
        &mut ws_out_rx,
        "watch1",
        "/nonexistent/path/that/does/not/exist",
    )
    .await;

    assert_eq!(reply.payload["status"], "error");
    assert!(reply.payload["response"]["reason"]
        .as_str()
        .unwrap()
        .contains("does not exist"));
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
    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "mywatch", &watch_path).await;
    assert_eq!(reply.payload["status"], "ok");

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file in the watched directory
    let file_path = temp_dir.path().join("test_file.txt");
    tokio::fs::write(&file_path, "hello").await.unwrap();

    // Should receive created event as Phoenix push
    let event = wait_for_phx_event(&mut ws_out_rx, "created", 2000).await;
    assert!(event.is_some(), "Expected created event");
    let event = event.unwrap();
    assert_eq!(event.topic, "watch:mywatch");
    assert_eq!(event.payload["path"].as_str().unwrap(), "test_file.txt");
}

#[tokio::test]
async fn test_watch_file_modified() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("existing_file.txt");
    tokio::fs::write(&file_path, "initial content")
        .await
        .unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Subscribe to the directory
    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "modwatch", &watch_path).await;
    assert_eq!(reply.payload["status"], "ok");

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Modify the file
    tokio::fs::write(&file_path, "modified content")
        .await
        .unwrap();

    // Should receive modified event with relative path
    let event = wait_for_phx_event(&mut ws_out_rx, "modified", 2000).await;
    assert!(event.is_some(), "Expected modified event");
    let event = event.unwrap();
    assert_eq!(event.topic, "watch:modwatch");
    assert_eq!(event.payload["path"].as_str().unwrap(), "existing_file.txt");
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
    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "delwatch", &watch_path).await;
    assert_eq!(reply.payload["status"], "ok");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Delete the file
    tokio::fs::remove_file(&file_path).await.unwrap();

    // Should receive deleted event with relative path
    let event = wait_for_phx_event(&mut ws_out_rx, "deleted", 2000).await;
    assert!(event.is_some(), "Expected deleted event");
    let event = event.unwrap();
    assert_eq!(event.topic, "watch:delwatch");
    assert_eq!(
        event.payload["path"].as_str().unwrap(),
        "file_to_delete.txt"
    );
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

    // Join
    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "unsub_test", &watch_path).await;
    assert_eq!(reply.payload["status"], "ok");

    // Leave
    send_phx(
        &ws_in_tx,
        PhxMessage::new("watch:unsub_test", events::PHX_LEAVE, json!({})).with_ref("2"),
    );

    let reply = wait_for_reply(&mut ws_out_rx, 1000).await;
    assert!(reply.is_some(), "Expected phx_reply for leave");
    let reply = reply.unwrap();
    assert_eq!(reply.topic, "watch:unsub_test");
    assert_eq!(reply.payload["status"], "ok");
}

#[tokio::test]
async fn test_watch_heartbeat() {
    let state = WsState::new();
    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Send heartbeat
    send_phx(
        &ws_in_tx,
        PhxMessage::new("phoenix", events::HEARTBEAT, json!({})).with_ref("3"),
    );

    let reply = wait_for_reply(&mut ws_out_rx, 1000).await;
    assert!(reply.is_some(), "Expected heartbeat reply");
    let reply = reply.unwrap();
    assert_eq!(reply.topic, "phoenix");
    assert_eq!(reply.ref_, Some("3".to_string()));
    assert_eq!(reply.payload["status"], "ok");
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

    // Both join with different refs
    let reply1 = join_watch(&ws1_in_tx, &mut ws1_out_rx, "client1_watch", &watch_path).await;
    let reply2 = join_watch(&ws2_in_tx, &mut ws2_out_rx, "client2_watch", &watch_path).await;
    assert_eq!(reply1.payload["status"], "ok");
    assert_eq!(reply2.payload["status"], "ok");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file
    let file_path = temp_dir.path().join("shared_file.txt");
    tokio::fs::write(&file_path, "content").await.unwrap();

    // Both should receive the created event on their respective topics
    let event1 = wait_for_phx_event(&mut ws1_out_rx, "created", 2000).await;
    let event2 = wait_for_phx_event(&mut ws2_out_rx, "created", 2000).await;

    assert!(event1.is_some(), "WebSocket 1 should receive created event");
    assert!(event2.is_some(), "WebSocket 2 should receive created event");
    assert_eq!(event1.unwrap().topic, "watch:client1_watch");
    assert_eq!(event2.unwrap().topic, "watch:client2_watch");
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
    let reply = join_watch(&ws_in_tx, &mut ws_out_rx, "recursive_watch", &watch_path).await;
    assert_eq!(reply.payload["status"], "ok");

    // Give the watcher time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a file in the subdirectory
    let file_path = sub_dir.join("nested_file.txt");
    tokio::fs::write(&file_path, "nested content")
        .await
        .unwrap();

    // Should receive created event for nested file with relative path (recursive watching)
    let event = wait_for_phx_event(&mut ws_out_rx, "created", 2000).await;
    assert!(event.is_some(), "Expected created event for nested file");
    let event = event.unwrap();
    assert_eq!(event.topic, "watch:recursive_watch");
    assert_eq!(
        event.payload["path"].as_str().unwrap(),
        "subdir/nested_file.txt"
    );
}
