mod common;

use common::{create_fake_phoenix_socket, recv_phoenix_msg, send_phoenix_msg, wait_for_event};
use serde_json::{json, Value};
use std::sync::Arc;
use tidewave_core::phoenix::PhxMessage;
use tidewave_core::ws::acp::*;
use tidewave_core::ws::connection::unit_testable_ws_handler;
use tidewave_core::ws::WsState;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn test_channel_init_request_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);
    let ws_state = WsState::new().with_acp_state(state);

    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Start the handler in background
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx, in_rx, ws_state, uuid::Uuid::new_v4()).await;
    });

    // Join the channel
    let join_msg = PhxMessage::new("acp:test", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let reply = recv_phoenix_msg(&mut out_rx)
        .await
        .expect("Expected join reply");
    assert_eq!(reply.event, "phx_reply");
    assert_eq!(reply.payload.as_json()["status"], "ok");

    // Wait for process to actually start
    process_started.await.expect("Process failed to start");

    // Send init request via jsonrpc event
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    let push_msg = PhxMessage::new("acp:test", "jsonrpc", init_request)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &push_msg);

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
    let msg = recv_phoenix_msg(&mut out_rx)
        .await
        .expect("Expected response message");
    assert_eq!(msg.event, "jsonrpc");
    let response = msg.payload.as_json();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
}

#[tokio::test]
async fn test_channel_session_new_flow() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);
    let ws_state = WsState::new().with_acp_state(state);

    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Start the handler
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx, in_rx, ws_state, uuid::Uuid::new_v4()).await;
    });

    // Join the channel
    let join_msg = PhxMessage::new("acp:test", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &join_msg);

    // Wait for join reply
    let _ = recv_phoenix_msg(&mut out_rx).await;

    // Wait for process to start
    process_started.await.expect("Process failed to start");

    // First send initialize
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {}
    });
    let push_msg = PhxMessage::new("acp:test", "jsonrpc", init_request)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &push_msg);

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
    let _ = recv_phoenix_msg(&mut out_rx).await;

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
    let push_msg = PhxMessage::new("acp:test", "jsonrpc", session_new_request)
        .with_ref("3")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &push_msg);

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
    let msg = recv_phoenix_msg(&mut out_rx)
        .await
        .expect("Expected response message");
    assert_eq!(msg.event, "jsonrpc");
    let response = msg.payload.as_json();
    assert_eq!(response["id"], "new_123");
    assert_eq!(response["result"]["sessionId"], "sess_xyz_789");
}

#[tokio::test]
async fn test_channel_notification_forwarding() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);
    let ws_state = WsState::new().with_acp_state(state);

    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    // Start the handler
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx, in_rx, ws_state, uuid::Uuid::new_v4()).await;
    });

    // Join the channel
    let join_msg = PhxMessage::new("acp:test", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &join_msg);
    let _ = recv_phoenix_msg(&mut out_rx).await;

    // Wait for process to start
    process_started.await.expect("Process failed to start");

    // First send initialize
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {}
    });
    let push_msg = PhxMessage::new("acp:test", "jsonrpc", init_request)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &push_msg);

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
    let _ = recv_phoenix_msg(&mut out_rx).await;

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
    let push_msg = PhxMessage::new("acp:test", "jsonrpc", session_new)
        .with_ref("3")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &push_msg);

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
    let _ = recv_phoenix_msg(&mut out_rx).await;

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
    let msg = recv_phoenix_msg(&mut out_rx)
        .await
        .expect("Expected notification message");
    assert_eq!(msg.event, "jsonrpc");
    let notif = msg.payload.as_json();
    assert_eq!(notif["method"], "session/update");
    assert_eq!(notif["params"]["sessionId"], "sess_123");
}

/// Test that concurrent channel joins with same spawn options only start one process
#[tokio::test]
async fn test_concurrent_channel_joins() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Create a barrier to control when the process start completes
    let (barrier_tx, barrier_rx) = tokio::sync::oneshot::channel();
    let barrier = Arc::new(tokio::sync::Mutex::new(Some(barrier_rx)));

    // Counter to track how many times the process starter is called
    let start_count = Arc::new(AtomicUsize::new(0));
    let start_count_clone = start_count.clone();

    // Create a custom process starter that increments counter and waits on barrier
    let starter: ProcessStarterFn = Arc::new(move |_spawn_opts: TidewaveSpawnOptions| {
        let barrier = barrier.clone();
        let start_count = start_count_clone.clone();

        Box::pin(async move {
            // Increment start counter
            start_count.fetch_add(1, Ordering::SeqCst);

            // Wait for barrier - this simulates slow process startup
            if let Some(rx) = barrier.lock().await.take() {
                let _ = rx.await;
            }

            // Create duplex streams for the process
            let (process_stdin, _test_stdin) = tokio::io::duplex(8192);
            let (_test_stdout, process_stdout) = tokio::io::duplex(8192);

            // Create stderr (unused)
            let (stderr_write, stderr_read) = tokio::io::duplex(1024);
            drop(stderr_write);

            Ok::<ProcessIo, anyhow::Error>((
                Box::new(process_stdout),
                Box::new(BufReader::new(process_stdin)),
                Box::new(BufReader::new(stderr_read)),
                None,
            ))
        })
    });

    let state = AcpChannelState::with_process_starter(starter);

    // Two ws handlers sharing the same AcpChannelState (same process)
    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state);

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    // Start both handlers
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    // Create futures for both joins
    let join1_future = async {
        let join_msg = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
            .with_ref("1")
            .with_join_ref("j1");
        send_phoenix_msg(&in_tx1, &join_msg);
        recv_phoenix_msg(&mut out_rx1).await
    };

    let join2_future = async {
        let join_msg = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
            .with_ref("1")
            .with_join_ref("j1");
        send_phoenix_msg(&in_tx2, &join_msg);
        recv_phoenix_msg(&mut out_rx2).await
    };

    // Run both joins concurrently, plus a task to release the barrier
    let barrier_release = async {
        // Wait for the process starter to be called
        let start_time = tokio::time::Instant::now();
        loop {
            if start_count.load(Ordering::SeqCst) >= 1 {
                break;
            }
            if start_time.elapsed() > tokio::time::Duration::from_secs(1) {
                panic!("Timeout waiting for process starter to be called");
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        // Process starter should have been called exactly once
        assert_eq!(start_count.load(Ordering::SeqCst), 1);

        // Release the barrier to complete the first process start
        barrier_tx.send(()).unwrap();
    };

    // Run all three concurrently
    let (result1, result2, ()): (Option<PhxMessage>, Option<PhxMessage>, ()) =
        tokio::join!(join1_future, join2_future, barrier_release);

    // Both joins should succeed
    assert!(result1.is_some(), "First join should succeed");
    assert!(result2.is_some(), "Second join should succeed");
    assert_eq!(result1.unwrap().payload.as_json()["status"], "ok");
    assert_eq!(result2.unwrap().payload.as_json()["status"], "ok");

    // Process starter should have been called only once
    assert_eq!(start_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_process_lifecycle_lock_is_not_removed_after_join() {
    let (starter, _test_stdin, _test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);
    let ws_state = WsState::new().with_acp_state(state.clone());

    let (out_tx, mut out_rx, in_tx, in_rx) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx, in_rx, ws_state, uuid::Uuid::new_v4()).await;
    });

    let join_msg = PhxMessage::new("acp:test", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx, &join_msg);

    let reply = recv_phoenix_msg(&mut out_rx)
        .await
        .expect("Expected join reply");
    assert_eq!(reply.payload.as_json()["status"], "ok");

    process_started.await.expect("Process failed to start");
    assert!(state.process_lifecycle_locks.contains_key("test_acp:."));
}

/// Test that concurrent init requests from two clients only send one init to the process
#[tokio::test]
async fn test_concurrent_init_requests() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    // Two ws handlers sharing the same AcpChannelState (same process)
    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state);

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    // Start both handlers
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    // Join client 1
    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");

    // Wait for process to start
    process_started.await.expect("Process failed to start");

    // Join client 2 (same spawn opts = same process)
    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");

    // Both clients send init requests concurrently
    let init_request1 = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    let init_request2 = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "initialize",
        "params": {}
    });
    let push_msg1 = PhxMessage::new("acp:test1", "jsonrpc", init_request1)
        .with_ref("2")
        .with_join_ref("j1");
    let push_msg2 = PhxMessage::new("acp:test2", "jsonrpc", init_request2)
        .with_ref("2")
        .with_join_ref("j2");

    // Send both init requests as close together as possible
    send_phoenix_msg(&in_tx1, &push_msg1);
    send_phoenix_msg(&in_tx2, &push_msg2);

    // The process should receive exactly ONE init request
    let received_request = read_json_line(&mut test_stdout).await;
    assert_eq!(received_request["method"], "initialize");
    let proxy_id = received_request["id"].clone();

    // Respond from the process
    let init_response = json!({
        "jsonrpc": "2.0",
        "id": proxy_id,
        "result": {
            "protocolVersion": 1,
            "agentCapabilities": {}
        }
    });
    write_json_line(&mut test_stdin, &init_response).await;

    // Both clients should receive their init responses (with their original IDs)
    let msg1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected response for client 1");
    assert_eq!(msg1.event, "jsonrpc");
    assert_eq!(msg1.payload.as_json()["id"], 1);
    assert_eq!(msg1.payload.as_json()["result"]["protocolVersion"], 1);

    let msg2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected response for client 2");
    assert_eq!(msg2.event, "jsonrpc");
    assert_eq!(msg2.payload.as_json()["id"], 2);
    assert_eq!(msg2.payload.as_json()["result"]["protocolVersion"], 1);

    // Verify no additional init request was sent to the process by sending another
    // request and checking it's not an init
    let session_new = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", session_new)
        .with_ref("3")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);

    let next_request = read_json_line(&mut test_stdout).await;
    assert_eq!(
        next_request["method"], "session/new",
        "Next request to process should be session/new, not a duplicate initialize"
    );
}

#[tokio::test]
async fn test_session_resume_rejects_active_session() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state);

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");

    process_started.await.expect("Process failed to start");

    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");

    let session_new = json!({
        "jsonrpc": "2.0",
        "id": "new_1",
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", session_new)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);

    let new_req = read_json_line(&mut test_stdout).await;
    assert_eq!(new_req["method"], "session/new");
    let new_proxy_id = new_req["id"].clone();
    let new_response = json!({
        "jsonrpc": "2.0",
        "id": new_proxy_id,
        "result": {
            "sessionId": "sess_active"
        }
    });
    write_json_line(&mut test_stdin, &new_response).await;
    let _ = recv_phoenix_msg(&mut out_rx1).await;

    let resume = json!({
        "jsonrpc": "2.0",
        "id": "resume_1",
        "method": "session/resume",
        "params": {
            "sessionId": "sess_active"
        }
    });
    let push_msg = PhxMessage::new("acp:test2", "jsonrpc", resume)
        .with_ref("2")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &push_msg);

    let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
        .await
        .expect("Expected session/resume to be rejected");
    let response = msg.payload.as_json();
    assert_eq!(response["id"], "resume_1");
    assert_eq!(response["error"]["code"], -32003);
    assert_eq!(
        response["error"]["message"],
        "Session already has an active connection"
    );
}

#[tokio::test]
async fn test_session_resume_rejects_in_flight_resume_for_inactive_session() {
    let (starter, _test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state.clone());

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");

    process_started.await.expect("Process failed to start");

    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");

    let process_key = "test_acp:.".to_string();
    let process_state = state
        .processes
        .get(&process_key)
        .expect("Expected process to exist")
        .clone();
    let session_id = "sess_inactive".to_string();
    let session = Arc::new(SessionState::new(process_key));
    process_state
        .insert_session(session_id.clone(), session, None)
        .await;

    let resume1 = json!({
        "jsonrpc": "2.0",
        "id": "resume_1",
        "method": "session/resume",
        "params": {
            "sessionId": session_id
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", resume1)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);

    let first_resume = read_json_line(&mut test_stdout).await;
    assert_eq!(first_resume["method"], "session/resume");

    let resume2 = json!({
        "jsonrpc": "2.0",
        "id": "resume_2",
        "method": "session/resume",
        "params": {
            "sessionId": "sess_inactive"
        }
    });
    let push_msg = PhxMessage::new("acp:test2", "jsonrpc", resume2)
        .with_ref("2")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &push_msg);

    let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
        .await
        .expect("Expected second session/resume to be rejected");
    let response = msg.payload.as_json();
    assert_eq!(response["id"], "resume_2");
    assert_eq!(response["error"]["code"], -32003);
    assert_eq!(
        response["error"]["message"],
        "Session already has an active connection"
    );
}

#[tokio::test]
async fn test_late_resume_response_routes_to_reconnected_session() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state.clone());

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");

    process_started.await.expect("Process failed to start");

    let process_key = "test_acp:.".to_string();
    let process_state = state
        .processes
        .get(&process_key)
        .expect("Expected process to exist")
        .clone();
    let session_id = "sess_late_resume".to_string();
    let session = Arc::new(SessionState::new(process_key));
    process_state
        .insert_session(session_id.clone(), session, None)
        .await;

    let resume = json!({
        "jsonrpc": "2.0",
        "id": "resume_1",
        "method": "session/resume",
        "params": {
            "sessionId": session_id
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", resume)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);

    let resume_request = read_json_line(&mut test_stdout).await;
    assert_eq!(resume_request["method"], "session/resume");
    let resume_proxy_id = resume_request["id"].clone();

    drop(in_tx1);
    assert!(
        wait_for_session_channel(&process_state, &session_id, None).await,
        "Expected disconnect cleanup to unmap the session before reconnect"
    );

    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");

    let load = json!({
        "jsonrpc": "2.0",
        "id": "load_1",
        "method": "_tidewave.ai/session/load",
        "params": {
            "sessionId": "sess_late_resume",
            "latestId": ""
        }
    });
    let push_msg = PhxMessage::new("acp:test2", "jsonrpc", load)
        .with_ref("2")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &push_msg);

    let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
        .await
        .expect("Expected Tidewave session/load response");
    let response = msg.payload.as_json();
    assert_eq!(response["id"], "load_1");
    assert_eq!(response["result"]["cancelled"], false);

    write_json_line(
        &mut test_stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": resume_proxy_id,
            "result": {
                "sessionId": "sess_late_resume"
            }
        }),
    )
    .await;

    let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
        .await
        .expect("Expected late session/resume response on reconnected channel");
    let response = msg.payload.as_json();
    assert_eq!(response["id"], "resume_1");
    assert_eq!(response["result"]["sessionId"], "sess_late_resume");
}

#[tokio::test]
async fn test_late_failed_resume_response_does_not_remove_reconnected_session() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state.clone());

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");

    process_started.await.expect("Process failed to start");

    let process_key = "test_acp:.".to_string();
    let process_state = state
        .processes
        .get(&process_key)
        .expect("Expected process to exist")
        .clone();
    let session_id = "sess_failed_late_resume".to_string();
    let session = Arc::new(SessionState::new(process_key));
    process_state
        .insert_session(session_id.clone(), session, None)
        .await;

    let resume = json!({
        "jsonrpc": "2.0",
        "id": "resume_1",
        "method": "session/resume",
        "params": {
            "sessionId": session_id
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", resume)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);

    let resume_request = read_json_line(&mut test_stdout).await;
    assert_eq!(resume_request["method"], "session/resume");
    let resume_proxy_id = resume_request["id"].clone();

    drop(in_tx1);
    assert!(
        wait_for_session_channel(&process_state, &session_id, None).await,
        "Expected disconnect cleanup to unmap the session before reconnect"
    );

    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");

    let load = json!({
        "jsonrpc": "2.0",
        "id": "load_1",
        "method": "_tidewave.ai/session/load",
        "params": {
            "sessionId": "sess_failed_late_resume",
            "latestId": ""
        }
    });
    let push_msg = PhxMessage::new("acp:test2", "jsonrpc", load)
        .with_ref("2")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &push_msg);

    let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
        .await
        .expect("Expected Tidewave session/load response");
    assert_eq!(msg.payload.as_json()["id"], "load_1");

    write_json_line(
        &mut test_stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": resume_proxy_id,
            "error": {
                "code": -32002,
                "message": "Session not found"
            }
        }),
    )
    .await;

    write_json_line(
        &mut test_stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_failed_late_resume",
                "update": "still_connected"
            }
        }),
    )
    .await;

    let mut received_update = false;
    for _ in 0..2 {
        let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
            .await
            .expect("Expected JSON-RPC message on reconnected channel");
        if msg.payload.as_json()["params"]["update"] == "still_connected" {
            received_update = true;
            break;
        }
    }

    assert!(
        received_update,
        "Expected reconnected session to keep receiving updates"
    );
}

#[tokio::test]
async fn test_late_session_new_response_does_not_claim_disconnected_channel() {
    let (starter, mut test_stdin, mut test_stdout, process_started) = create_fake_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state.clone());

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");

    process_started.await.expect("Process failed to start");

    let process_key = "test_acp:.".to_string();
    let process_state = state
        .processes
        .get(&process_key)
        .expect("Expected process to exist")
        .clone();

    let session_new = json!({
        "jsonrpc": "2.0",
        "id": "new_1",
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", session_new)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);

    let new_request = read_json_line(&mut test_stdout).await;
    assert_eq!(new_request["method"], "session/new");
    let new_proxy_id = new_request["id"].clone();

    drop(in_tx1);
    assert!(
        wait_for_event(&mut out_rx1, "jsonrpc", 100).await.is_none(),
        "Disconnected client should not receive the late session/new response"
    );

    write_json_line(
        &mut test_stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": new_proxy_id,
            "result": {
                "sessionId": "sess_late_new"
            }
        }),
    )
    .await;

    assert!(
        wait_for_inactive_session(&process_state, "sess_late_new").await,
        "Late session/new response should keep the session available for reconnect"
    );

    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts())
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");

    let load = json!({
        "jsonrpc": "2.0",
        "id": "load_1",
        "method": "_tidewave.ai/session/load",
        "params": {
            "sessionId": "sess_late_new",
            "latestId": ""
        }
    });
    let push_msg = PhxMessage::new("acp:test2", "jsonrpc", load)
        .with_ref("2")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &push_msg);

    let mut load_response = None;
    for _ in 0..2 {
        let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
            .await
            .expect("Expected JSON-RPC message after session/load");
        if msg.payload.as_json()["id"] == "load_1" {
            load_response = Some(msg);
            break;
        }
    }

    let load_response = load_response.expect("Expected Tidewave session/load response");
    let response = load_response.payload.as_json();
    assert_eq!(response["id"], "load_1");
    assert_eq!(response["result"]["cancelled"], false);
}

#[tokio::test]
async fn test_same_session_id_routes_within_each_process() {
    let (starter, mut processes_rx) = create_multi_process_starter();
    let state = AcpChannelState::with_process_starter(starter);

    let ws_state1 = WsState::new().with_acp_state(state.clone());
    let ws_state2 = WsState::new().with_acp_state(state);

    let (out_tx1, mut out_rx1, in_tx1, in_rx1) = create_fake_phoenix_socket();
    let (out_tx2, mut out_rx2, in_tx2, in_rx2) = create_fake_phoenix_socket();

    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx1, in_rx1, ws_state1, uuid::Uuid::new_v4()).await;
    });
    tokio::spawn(async move {
        unit_testable_ws_handler(out_tx2, in_rx2, ws_state2, uuid::Uuid::new_v4()).await;
    });

    let join_msg1 = PhxMessage::new("acp:test1", "phx_join", spawn_opts_with_command("agent_a"))
        .with_ref("1")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &join_msg1);
    let reply1 = recv_phoenix_msg(&mut out_rx1)
        .await
        .expect("Expected join reply for client 1");
    assert_eq!(reply1.payload.as_json()["status"], "ok");
    let mut process_a = processes_rx.recv().await.expect("Expected agent_a process");
    assert_eq!(process_a.command, "agent_a");

    let join_msg2 = PhxMessage::new("acp:test2", "phx_join", spawn_opts_with_command("agent_b"))
        .with_ref("1")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &join_msg2);
    let reply2 = recv_phoenix_msg(&mut out_rx2)
        .await
        .expect("Expected join reply for client 2");
    assert_eq!(reply2.payload.as_json()["status"], "ok");
    let mut process_b = processes_rx.recv().await.expect("Expected agent_b process");
    assert_eq!(process_b.command, "agent_b");

    let session_new = json!({
        "jsonrpc": "2.0",
        "id": "new_a",
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });
    let push_msg = PhxMessage::new("acp:test1", "jsonrpc", session_new)
        .with_ref("2")
        .with_join_ref("j1");
    send_phoenix_msg(&in_tx1, &push_msg);
    let new_req = read_json_line(&mut process_a.stdout).await;
    let new_proxy_id = new_req["id"].clone();
    write_json_line(
        &mut process_a.stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": new_proxy_id,
            "result": {
                "sessionId": "sess_shared"
            }
        }),
    )
    .await;
    let _ = recv_phoenix_msg(&mut out_rx1).await;

    let session_new = json!({
        "jsonrpc": "2.0",
        "id": "new_b",
        "method": "session/new",
        "params": {
            "cwd": "/tmp",
            "mcpServers": []
        }
    });
    let push_msg = PhxMessage::new("acp:test2", "jsonrpc", session_new)
        .with_ref("2")
        .with_join_ref("j2");
    send_phoenix_msg(&in_tx2, &push_msg);
    let new_req = read_json_line(&mut process_b.stdout).await;
    let new_proxy_id = new_req["id"].clone();
    write_json_line(
        &mut process_b.stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": new_proxy_id,
            "result": {
                "sessionId": "sess_shared"
            }
        }),
    )
    .await;
    let _ = recv_phoenix_msg(&mut out_rx2).await;

    write_json_line(
        &mut process_a.stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_shared",
                "update": "from_a"
            }
        }),
    )
    .await;

    let msg = wait_for_event(&mut out_rx1, "jsonrpc", 250)
        .await
        .expect("Expected client 1 to receive agent_a update");
    assert_eq!(msg.payload.as_json()["params"]["update"], "from_a");
    assert!(
        wait_for_event(&mut out_rx2, "jsonrpc", 100).await.is_none(),
        "client 2 should not receive agent_a update"
    );

    write_json_line(
        &mut process_b.stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_shared",
                "update": "from_b"
            }
        }),
    )
    .await;

    let msg = wait_for_event(&mut out_rx2, "jsonrpc", 250)
        .await
        .expect("Expected client 2 to receive agent_b update");
    assert_eq!(msg.payload.as_json()["params"]["update"], "from_b");
}

// ============================================================================
// Helpers
// ============================================================================

struct FakeProcess {
    command: String,
    stdin: DuplexStream,
    stdout: DuplexStream,
}

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

fn create_multi_process_starter() -> (ProcessStarterFn, mpsc::UnboundedReceiver<FakeProcess>) {
    let (process_tx, process_rx) = mpsc::unbounded_channel();

    let starter: ProcessStarterFn = Arc::new(move |spawn_opts: TidewaveSpawnOptions| {
        let process_tx = process_tx.clone();

        Box::pin(async move {
            let (process_stdin, test_stdin) = tokio::io::duplex(8192);
            let (test_stdout, process_stdout) = tokio::io::duplex(8192);

            process_tx
                .send(FakeProcess {
                    command: spawn_opts.command,
                    stdin: test_stdin,
                    stdout: test_stdout,
                })
                .expect("Failed to send fake process handles");

            let (stderr_write, stderr_read) = tokio::io::duplex(1024);
            drop(stderr_write);

            Ok::<ProcessIo, anyhow::Error>((
                Box::new(process_stdout),
                Box::new(BufReader::new(process_stdin)),
                Box::new(BufReader::new(stderr_read)),
                None,
            ))
        })
    });

    (starter, process_rx)
}

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

async fn wait_for_session_channel(
    process_state: &ProcessState,
    session_id: &str,
    expected: Option<ChannelId>,
) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);

    loop {
        if process_state.session_channel(session_id).await == expected {
            return true;
        }

        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

async fn wait_for_inactive_session(process_state: &ProcessState, session_id: &str) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);

    loop {
        if process_state.session_state(session_id).await.is_some()
            && process_state.session_channel(session_id).await.is_none()
        {
            return true;
        }

        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

/// Helper to create spawn options for channel join
fn spawn_opts() -> Value {
    spawn_opts_with_command("test_acp")
}

fn spawn_opts_with_command(command: &str) -> Value {
    json!({
        "command": command,
        "env": {},
        "cwd": "."
    })
}
