mod common;

use common::{create_fake_phoenix_socket, send_phoenix_msg, wait_for_event, wait_for_reply};
use serde_json::json;
use tidewave_core::channels::acp_channel::AcpChannelState;
use tidewave_core::channels::mcp_channel::McpChannelState;
use tidewave_core::phoenix::{unit_testable_phoenix_handler, PhoenixState, PhxMessage};

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn test_watch_subscribe_success() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Create a temp directory for watching
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Send join request
    let join_msg = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Should receive join reply
    let reply = wait_for_reply(&mut out_rx, 1000).await;
    assert!(reply.is_some(), "Expected join reply");
    let reply = reply.unwrap();
    assert_eq!(reply.payload["status"], "ok");
    assert!(reply.payload["response"]["path"].is_string());
}

#[tokio::test]
async fn test_watch_subscribe_relative_path_error() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Send join request with relative path
    let join_msg = PhxMessage::new("watch:relative/path", "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Should receive error reply
    let reply = wait_for_reply(&mut out_rx, 1000).await;
    assert!(reply.is_some(), "Expected join reply");
    let reply = reply.unwrap();
    assert_eq!(reply.payload["status"], "error");
    assert!(reply.payload["response"]["reason"]
        .as_str()
        .unwrap()
        .contains("absolute"));
}

#[tokio::test]
async fn test_watch_subscribe_nonexistent_path_error() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Send join request with non-existent path
    let join_msg = PhxMessage::new(
        "watch:/nonexistent/path/that/does/not/exist",
        "phx_join",
        json!({}),
    )
    .with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Should receive error reply
    let reply = wait_for_reply(&mut out_rx, 1000).await;
    assert!(reply.is_some(), "Expected join reply");
    let reply = reply.unwrap();
    assert_eq!(reply.payload["status"], "error");
    assert!(reply.payload["response"]["reason"]
        .as_str()
        .unwrap()
        .contains("does not exist"));
}

#[tokio::test]
async fn test_watch_file_created() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Create a temp directory for watching
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Subscribe to the directory
    let join_msg = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let _ = wait_for_reply(&mut out_rx, 1000).await;

    // Create a file in the watched directory
    let file_path = temp_dir.path().join("test_file.txt");
    tokio::fs::write(&file_path, "hello").await.unwrap();

    // Should receive created event with relative path
    let event = wait_for_event(&mut out_rx, "created", 2000).await;
    assert!(event.is_some(), "Expected created event");
    let event = event.unwrap();
    assert_eq!(event.payload["path"].as_str().unwrap(), "test_file.txt");
}

#[tokio::test]
async fn test_watch_file_modified() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("existing_file.txt");
    tokio::fs::write(&file_path, "initial content")
        .await
        .unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Subscribe to the directory
    let join_msg = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let _ = wait_for_reply(&mut out_rx, 1000).await;

    // Modify the file
    tokio::fs::write(&file_path, "modified content")
        .await
        .unwrap();

    // Should receive modified event with relative path
    let event = wait_for_event(&mut out_rx, "modified", 2000).await;
    assert!(event.is_some(), "Expected modified event");
    let event = event.unwrap();
    assert_eq!(event.payload["path"].as_str().unwrap(), "existing_file.txt");
}

#[tokio::test]
async fn test_watch_file_deleted() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Create a temp directory and file
    let temp_dir = tempfile::tempdir().unwrap();
    let file_path = temp_dir.path().join("file_to_delete.txt");
    tokio::fs::write(&file_path, "content").await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Subscribe to the directory
    let join_msg = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let _ = wait_for_reply(&mut out_rx, 1000).await;

    // Delete the file
    tokio::fs::remove_file(&file_path).await.unwrap();

    // Should receive deleted event with relative path
    let event = wait_for_event(&mut out_rx, "deleted", 2000).await;
    assert!(event.is_some(), "Expected deleted event");
    let event = event.unwrap();
    assert_eq!(
        event.payload["path"].as_str().unwrap(),
        "file_to_delete.txt"
    );
}

#[tokio::test]
async fn test_watch_leave() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Create a temp directory
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Subscribe
    let join_msg = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let _ = wait_for_reply(&mut out_rx, 1000).await;

    // Leave
    let leave_msg = PhxMessage::new(&topic, "phx_leave", json!({})).with_ref("2");
    send_phoenix_msg(&in_tx, &leave_msg);

    // Should receive leave reply
    let reply = wait_for_reply(&mut out_rx, 1000).await;
    assert!(reply.is_some(), "Expected leave reply");
    let reply = reply.unwrap();
    assert_eq!(reply.payload["status"], "ok");
}

#[tokio::test]
async fn test_watch_concurrent_subscribers() {
    // Use a single shared state
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());

    // Create a temp directory
    let temp_dir = tempfile::tempdir().unwrap();
    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Create two socket connections
    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    let state1 = state.clone();
    let state2 = state.clone();

    // Start both handlers
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx1, in_rx1, state1).await;
    });

    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx2, in_rx2, state2).await;
    });

    // Both subscribe to the same path
    let join_msg1 = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    let join_msg2 = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");

    send_phoenix_msg(&in_tx1, &join_msg1);
    send_phoenix_msg(&in_tx2, &join_msg2);

    // Both should receive join confirmations
    let reply1 = wait_for_reply(&mut out_rx1, 1000).await;
    let reply2 = wait_for_reply(&mut out_rx2, 1000).await;
    assert!(reply1.is_some());
    assert!(reply2.is_some());
    assert_eq!(reply1.unwrap().payload["status"], "ok");
    assert_eq!(reply2.unwrap().payload["status"], "ok");

    // Verify only one watcher exists (through shared state)
    assert_eq!(
        state.watch_state.watchers.len(),
        1,
        "Should have exactly one watcher for the same path"
    );

    // Create a file
    let file_path = temp_dir.path().join("shared_file.txt");
    tokio::fs::write(&file_path, "content").await.unwrap();

    // Both should receive the created event
    let event1 = wait_for_event(&mut out_rx1, "created", 2000).await;
    let event2 = wait_for_event(&mut out_rx2, "created", 2000).await;

    assert!(event1.is_some(), "Socket 1 should receive created event");
    assert!(event2.is_some(), "Socket 2 should receive created event");
    assert_eq!(
        event1.unwrap().payload["path"].as_str().unwrap(),
        "shared_file.txt"
    );
    assert_eq!(
        event2.unwrap().payload["path"].as_str().unwrap(),
        "shared_file.txt"
    );
}

#[tokio::test]
async fn test_watch_subdirectory_events() {
    let state = PhoenixState::new(AcpChannelState::new(), McpChannelState::new());
    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Create a temp directory with subdirectory
    let temp_dir = tempfile::tempdir().unwrap();
    let sub_dir = temp_dir.path().join("subdir");
    tokio::fs::create_dir(&sub_dir).await.unwrap();

    let watch_path = temp_dir.path().to_string_lossy().to_string();
    let topic = format!("watch:{}", watch_path);

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_phoenix_handler(out_tx, in_rx, state).await;
    });

    // Subscribe to the parent directory
    let join_msg = PhxMessage::new(&topic, "phx_join", json!({})).with_ref("1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let _ = wait_for_reply(&mut out_rx, 1000).await;

    // Create a file in the subdirectory
    let file_path = sub_dir.join("nested_file.txt");
    tokio::fs::write(&file_path, "nested content")
        .await
        .unwrap();

    // Should receive created event for nested file with relative path (recursive watching)
    let event = wait_for_event(&mut out_rx, "created", 2000).await;
    assert!(event.is_some(), "Expected created event for nested file");
    let event = event.unwrap();
    // Relative path includes subdirectory
    assert_eq!(
        event.payload["path"].as_str().unwrap(),
        "subdir/nested_file.txt"
    );
}
