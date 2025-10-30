use futures::channel::mpsc as futures_mpsc;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tidewave_core::acp_proxy::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

#[tokio::test]
async fn test_websocket_init_request_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpProxyState::with_process_starter(starter);

    let (ws_out_tx, mut ws_out_rx, ws_in_tx, ws_in_rx) = create_fake_websocket();
    let websocket_id = uuid::Uuid::new_v4();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // Send init request from client
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
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

    // Start the handler
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // First send initialize to start the process
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
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

    // Start the handler
    tokio::spawn(async move {
        unit_testable_ws_handler(ws_out_tx, ws_in_rx, state, websocket_id).await;
    });

    // First send initialize to start the process
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
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

#[tokio::test]
async fn test_concurrent_initialize_requests() {
    // Create a barrier to control when the process start completes
    let (barrier_tx, barrier_rx) = tokio::sync::oneshot::channel();
    let barrier = Arc::new(tokio::sync::Mutex::new(Some(barrier_rx)));

    // Counter to track how many times the process starter is called
    let start_count = Arc::new(AtomicUsize::new(0));
    let start_count_clone = start_count.clone();

    // Create a custom process starter that increments counter and waits on barrier
    let (process_stdin, _test_stdin) = tokio::io::duplex(8192);
    let process_stdin = Arc::new(tokio::sync::Mutex::new(Some(process_stdin)));
    let (_test_stdout, process_stdout) = tokio::io::duplex(8192);
    let process_stdout = Arc::new(tokio::sync::Mutex::new(Some(process_stdout)));
    let (started_tx, _started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(tokio::sync::Mutex::new(Some(started_tx)));

    let starter: ProcessStarterFn = Arc::new(move |_spawn_opts: TidewaveSpawnOptions| {
        let process_stdin = process_stdin.clone();
        let process_stdout = process_stdout.clone();
        let barrier = barrier.clone();
        let start_count = start_count_clone.clone();
        let started_tx = started_tx.clone();

        Box::pin(async move {
            // Increment start counter
            start_count.fetch_add(1, Ordering::SeqCst);

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

            // Wait for barrier
            if let Some(rx) = barrier.lock().await.take() {
                let _ = rx.await;
            }

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

    let state = AcpProxyState::with_process_starter(starter);
    let state_clone = state.clone();

    // Create two websockets with the same command and params
    let (ws1_out_tx, mut _ws1_out_rx, ws1_in_tx, ws1_in_rx) = create_fake_websocket();
    let (ws2_out_tx, mut ws2_out_rx, ws2_in_tx, ws2_in_rx) = create_fake_websocket();

    let websocket_id1 = uuid::Uuid::new_v4();
    let websocket_id2 = uuid::Uuid::new_v4();

    // Start both handlers
    tokio::spawn(async move {
        unit_testable_ws_handler(ws1_out_tx, ws1_in_rx, state, websocket_id1).await;
    });

    tokio::spawn(async move {
        unit_testable_ws_handler(ws2_out_tx, ws2_in_rx, state_clone, websocket_id2).await;
    });

    // Send init requests from both clients with SAME params (same process_key)
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
    });

    // Send both requests at nearly the same time
    ws1_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            init_request.to_string().into(),
        )))
        .unwrap();

    ws2_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            init_request.to_string().into(),
        )))
        .unwrap();

    // Wait for the process starter to be called exactly once
    let start_time = tokio::time::Instant::now();
    loop {
        if start_count.load(Ordering::SeqCst) == 1 {
            break;
        }
        if start_time.elapsed() > tokio::time::Duration::from_secs(1) {
            panic!("Timeout waiting for process starter to be called");
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    // At this point, first request should be blocked on barrier
    // Second request should be waiting on the lock
    // Process starter should have been called exactly once
    assert_eq!(start_count.load(Ordering::SeqCst), 1);

    // Release the barrier to complete the first process start
    barrier_tx.send(()).unwrap();

    // First client should receive response (but we need to handle the process side first)
    // For this test, we'll just verify the second client gets the expected error

    // Check second client's response - should be the "Process alive, but no init response" error
    let ws2_msg = tokio::time::timeout(tokio::time::Duration::from_millis(500), ws2_out_rx.next())
        .await
        .expect("Timeout waiting for ws2 response")
        .expect("ws2 channel closed");

    match ws2_msg {
        axum::extract::ws::Message::Text(text) => {
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["jsonrpc"], "2.0");
            assert_eq!(response["id"], 1);
            // Should have an error, not a result
            assert!(response.get("error").is_some());
            let error = &response["error"];
            assert_eq!(error["code"], -32004);
            assert!(error["message"]
                .as_str()
                .unwrap()
                .contains("no init response"));
        }
        _ => panic!("Expected text message"),
    }

    // Process starter should still have been called only once
    assert_eq!(start_count.load(Ordering::SeqCst), 1);

    // Now send a new init request with DIFFERENT params on ws2
    // This should trigger a second process start since the process_key will be different
    let init_request_different = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({
            "clientCapabilities": {
                "experimental": true
            }
        }))
    });

    ws2_in_tx
        .unbounded_send(Ok(axum::extract::ws::Message::Text(
            init_request_different.to_string().into(),
        )))
        .unwrap();

    // Wait for the process starter to be called a second time
    let start_time = tokio::time::Instant::now();
    loop {
        if start_count.load(Ordering::SeqCst) == 2 {
            break;
        }
        if start_time.elapsed() > tokio::time::Duration::from_secs(1) {
            panic!("Timeout waiting for second process starter call");
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    // Process starter should now have been called twice (once for each unique process_key)
    assert_eq!(start_count.load(Ordering::SeqCst), 2);
}

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

    let starter: ProcessStarterFn = Arc::new(move |_spawn_opts: TidewaveSpawnOptions| {
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

/// Helper to create params with spawn options for initialize requests
fn params_with_spawn_opts(additional_params: Value) -> Value {
    let mut params = if additional_params.is_object() {
        additional_params
    } else {
        json!({})
    };

    params["_meta"] = json!({
        "tidewave.ai/spawn": {
            "command": "test_acp",
            "env": {},
            "cwd": "."
        }
    });

    params
}
