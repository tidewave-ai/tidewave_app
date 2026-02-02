use phoenix_rs::{ChannelRegistry, TestClient};
use serde_json::{json, Value};
use std::sync::Arc;
use tidewave_core::acp_channel::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

#[tokio::test]
async fn test_channel_init_request_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let mut registry = ChannelRegistry::new();
    registry.register("acp:*", AcpChannel::with_state(state));

    let mut client = TestClient::new(registry);
    let mut channel = client.join("acp:test", json!({})).await.unwrap();

    // Send init request via jsonrpc event
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
    });

    channel.push("jsonrpc", init_request).await;

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

    // Client should receive response via jsonrpc event with original ID
    let msg = channel.recv().await.expect("Expected response message");
    assert_eq!(msg.event, "jsonrpc");
    let response = &msg.payload;
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
}

#[tokio::test]
async fn test_channel_session_new_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let mut registry = ChannelRegistry::new();
    registry.register("acp:*", AcpChannel::with_state(state));

    let mut client = TestClient::new(registry);
    let mut channel = client.join("acp:test", json!({})).await.unwrap();

    // First send initialize to start the process
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
    });

    channel.push("jsonrpc", init_request).await;

    // Wait for process to start
    process_started.await.expect("Process failed to start");

    // Read and respond to init request
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

    // Consume init response
    let _ = channel.recv().await;

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

    channel.push("jsonrpc", session_new_request).await;

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
    let msg = channel.recv().await.expect("Expected response message");
    assert_eq!(msg.event, "jsonrpc");
    let response = &msg.payload;
    assert_eq!(response["id"], "new_123");
    assert_eq!(response["result"]["sessionId"], "sess_xyz_789");
}

#[tokio::test]
async fn test_channel_notification_forwarding() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let mut registry = ChannelRegistry::new();
    registry.register("acp:*", AcpChannel::with_state(state));

    let mut client = TestClient::new(registry);
    let mut channel = client.join("acp:test", json!({})).await.unwrap();

    // First send initialize to start the process
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
    });

    channel.push("jsonrpc", init_request).await;

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

    // Consume init response
    let _ = channel.recv().await;

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

    channel.push("jsonrpc", session_new).await;

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
    let _ = channel.recv().await;

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
    let msg = channel.recv().await.expect("Expected notification message");
    assert_eq!(msg.event, "jsonrpc");
    let notif = &msg.payload;
    assert_eq!(notif["method"], "session/update");
    assert_eq!(notif["params"]["sessionId"], "sess_123");
}

#[tokio::test]
async fn test_concurrent_initialize_requests() {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    let state = AcpChannelState::with_process_starter(starter);

    // Create two clients with the same shared state
    let mut registry1 = ChannelRegistry::new();
    registry1.register("acp:*", AcpChannel::with_state(state.clone()));

    let mut registry2 = ChannelRegistry::new();
    registry2.register("acp:*", AcpChannel::with_state(state));

    let mut client1 = TestClient::new(registry1);
    let mut client2 = TestClient::new(registry2);

    let mut channel1 = client1.join("acp:test1", json!({})).await.unwrap();
    let mut channel2 = client2.join("acp:test2", json!({})).await.unwrap();

    // Send init requests from both clients with SAME params (same process_key)
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params_with_spawn_opts(json!({}))
    });

    // Send both requests at nearly the same time
    channel1.push("jsonrpc", init_request.clone()).await;
    channel2.push("jsonrpc", init_request).await;

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

    // Check second client's response - should be the "Process alive, but no init response" error
    let msg = tokio::time::timeout(
        tokio::time::Duration::from_millis(500),
        channel2.recv(),
    )
    .await
    .expect("Timeout waiting for response")
    .expect("Channel closed");

    assert_eq!(msg.event, "jsonrpc");
    let response = &msg.payload;
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

    // Process starter should still have been called only once
    assert_eq!(start_count.load(Ordering::SeqCst), 1);
}

// ============================================================================
// Fake Process Infrastructure
// ============================================================================

/// Creates a fake process starter that returns duplex stream pairs
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
