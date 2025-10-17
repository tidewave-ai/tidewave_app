use futures::channel::mpsc as futures_mpsc;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tidewave_core::acp_proxy::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

// ============================================================================
// Fake Process Infrastructure
// ============================================================================

/// Creates a fake process starter that returns duplex stream pairs
/// The starter will be called when the WebSocket handler starts the process
/// Returns:
/// - ProcessStarterFn: the starter function
/// - DuplexStream: test writes here, process reads from stdin
/// - DuplexStream: test reads here, process writes to stdout
/// - Receiver that signals when process has started
fn create_fake_process_starter() -> (
    ProcessStarterFn,
    DuplexStream,
    DuplexStream,
    tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::sync::Mutex;

    // Create paired streams for stdin
    let (process_stdin, test_stdin) = tokio::io::duplex(8192);
    let process_stdin = Arc::new(Mutex::new(Some(process_stdin)));

    // Create paired streams for stdout
    let (test_stdout, process_stdout) = tokio::io::duplex(8192);
    let process_stdout = Arc::new(Mutex::new(Some(process_stdout)));

    // Channel to signal when process starts
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));

    let starter: ProcessStarterFn = Arc::new(move |command: String| {
        let process_stdin = process_stdin.clone();
        let process_stdout = process_stdout.clone();
        let started_tx = started_tx.clone();

        Box::pin(async move {
            let stdin = process_stdin
                .lock()
                .await
                .take()
                .expect("process already started");
            let stdout = process_stdout
                .lock()
                .await
                .take()
                .expect("process already started");

            // Signal that process has started
            if let Some(tx) = started_tx.lock().await.take() {
                let _ = tx.send(());
            }

            // Create stderr (unused)
            let (stderr_write, stderr_read) = tokio::io::duplex(1024);
            drop(stderr_write);

            Ok::<ProcessIo, anyhow::Error>((
                Box::new(stdout),
                Box::new(BufReader::new(stdin)),
                Box::new(BufReader::new(stderr_read)),
                None,
            ))
        })
    });

    (starter, test_stdin, test_stdout, started_rx)
}

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

// ============================================================================
// Test Helper Functions
// ============================================================================

/// Reads a JSON line from a stream (expects newline-terminated JSON)
async fn read_json_line(stream: &mut DuplexStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .expect("Failed to read line");
    serde_json::from_str(&line).expect("Failed to parse JSON")
}

/// Writes a JSON value to a stream with a trailing newline and flushes
async fn write_json_line(stream: &mut DuplexStream, value: &Value) {
    stream
        .write_all(value.to_string().as_bytes())
        .await
        .expect("Failed to write JSON");
    stream
        .write_all(b"\n")
        .await
        .expect("Failed to write newline");
    stream.flush().await.expect("Failed to flush");
}

// ============================================================================
// Integration Tests
// ============================================================================

#[tokio::test]
async fn test_websocket_init_request_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpProxyState::with_process_starter(starter);

    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();
    let command = "test_acp".to_string();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id, command).await;
    });

    // Send init request from client
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            init_request.to_string().into(),
        )))
        .unwrap();

    // Wait for process to actually start
    process_started.await.expect("Process failed to start");

    // Read what the process received on stdin
    let received_request = read_json_line(&mut test_stdout).await;
    assert_eq!(received_request["method"], "initialize");
    let proxy_id = received_request["id"].clone();

    // Write process response to stdout
    let init_response = json!({
        "jsonrpc": "2.0",
        "id": proxy_id,
        "result": {
            "protocolVersion": 1,
            "agentCapabilities": {}
        }
    });
    write_json_line(&mut test_stdin, &init_response).await;

    // Client should receive response with original ID
    let ws_msg = ws_out_rx.next().await.unwrap();
    match ws_msg {
        axum::extract::ws::Message::Text(text) => {
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["id"], 1);
            assert_eq!(response["result"]["protocolVersion"], 1);
        }
        _ => panic!("Expected text message"),
    }
}

#[tokio::test]
async fn test_websocket_session_new_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpProxyState::with_process_starter(starter);

    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();
    let command = "test_acp".to_string();

    // Start the handler
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id, command).await;
    });

    // First send initialize to start the process
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {}
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            init_request.to_string().into(),
        )))
        .unwrap();

    // Wait for process to start
    process_started.await.expect("Process failed to start");

    // Read and ignore init request from process
    let _ = read_json_line(&mut test_stdout).await;

    // Send session/new request
    let session_new_request = json!({
        "jsonrpc": "2.0",
        "id": "new_123",
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            session_new_request.to_string().into(),
        )))
        .unwrap();

    // Read session/new from process
    let received_request = read_json_line(&mut test_stdout).await;
    assert_eq!(received_request["method"], "session/new");
    assert_eq!(received_request["params"]["cwd"], "/tmp");
    let proxy_id = received_request["id"].clone();

    // Write process response
    let session_response = json!({
        "jsonrpc": "2.0",
        "id": proxy_id,
        "result": {
            "sessionId": "sess_xyz_789"
        }
    });
    write_json_line(&mut test_stdin, &session_response).await;

    // Client receives response
    let ws_msg = ws_out_rx.next().await.unwrap();
    match ws_msg {
        axum::extract::ws::Message::Text(text) => {
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["id"], "new_123");
            assert_eq!(response["result"]["sessionId"], "sess_xyz_789");
        }
        _ => panic!("Expected text message"),
    }
}

#[tokio::test]
async fn test_websocket_notification_forwarding() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpProxyState::with_process_starter(starter);

    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();
    let command = "test_acp".to_string();

    // Start the handler
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id, command).await;
    });

    // First send initialize to start the process
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {}
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            init_request.to_string().into(),
        )))
        .unwrap();

    // Wait for process to start
    process_started.await.expect("Process failed to start");

    // Read init request and send response
    let init_req = read_json_line(&mut test_stdout).await;
    let init_proxy_id = init_req["id"].clone();
    let init_response = json!({
        "jsonrpc": "2.0",
        "id": init_proxy_id,
        "result": {
            "protocolVersion": 1,
            "agentCapabilities": {}
        }
    });
    write_json_line(&mut test_stdin, &init_response).await;

    // Consume init response on websocket
    let _ = ws_out_rx.next().await.unwrap();

    // Create a session by sending session/new
    let session_new = json!({
        "jsonrpc": "2.0",
        "id": "new_1",
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });

    ws_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            session_new.to_string().into(),
        )))
        .unwrap();

    // Read session/new and respond
    let new_req = read_json_line(&mut test_stdout).await;
    let new_proxy_id = new_req["id"].clone();
    let new_response = json!({
        "jsonrpc": "2.0",
        "id": new_proxy_id,
        "result": {
            "sessionId": "sess_123"
        }
    });
    write_json_line(&mut test_stdin, &new_response).await;

    // Consume session/new response
    let _ = ws_out_rx.next().await.unwrap();

    // Write notification from process
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "sess_123",
            "update": "some data"
        }
    });
    write_json_line(&mut test_stdin, &notification).await;

    // Client receives notification
    let ws_msg = ws_out_rx.next().await.unwrap();
    match ws_msg {
        axum::extract::ws::Message::Text(text) => {
            let notif: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(notif["method"], "session/update");
            assert_eq!(notif["params"]["sessionId"], "sess_123");
        }
        _ => panic!("Expected text message"),
    }
}
