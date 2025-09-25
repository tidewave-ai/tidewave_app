use axum::http::StatusCode;
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// A guard that ensures processes are killed on drop, especially on test failures (panics)
struct TestGuard {
    server_process: Option<tokio::process::Child>,
    stderr_buffer: Arc<Mutex<Vec<String>>>,
}

impl TestGuard {
    fn new(server_process: tokio::process::Child, stderr_buffer: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            server_process: Some(server_process),
            stderr_buffer,
        }
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        // If we're dropping because of a panic, print the stderr content
        if std::thread::panicking() {
            eprintln!("Test failed! Server stderr output:");
            for line in self.stderr_buffer.lock().unwrap().iter() {
                eprintln!("{}", line);
            }
        }

        // Force kill the server process
        if let Some(mut server_process) = self.server_process.take() {
            let _ = server_process.start_kill();
        }
    }
}

/// Collects stderr lines in the background
fn collect_stderr(
    mut stderr_reader: BufReader<tokio::process::ChildStderr>,
) -> Arc<Mutex<Vec<String>>> {
    let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = stderr_buffer.clone();

    tokio::spawn(async move {
        let mut line = String::new();
        while let Ok(bytes_read) = stderr_reader.read_line(&mut line).await {
            if bytes_read == 0 {
                break;
            }
            buffer_clone.lock().unwrap().push(line.clone());
            line.clear();
        }
    });

    stderr_buffer
}

/// Helper to spawn tidewave server as external process
async fn spawn_tidewave_server(port: u16) -> TestGuard {
    let mut server_cmd = tokio::process::Command::new(get_binary_path("tidewave"));
    server_cmd
        .arg("--port")
        .arg(port.to_string())
        .arg("--debug")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut server_process = server_cmd.spawn().expect("Failed to start tidewave server");

    // Collect stderr for debugging
    let stderr = server_process.stderr.take().unwrap();
    let stderr_reader = BufReader::new(stderr);
    let stderr_buffer = collect_stderr(stderr_reader);

    // Wait for server to be ready
    wait_for_server(port).await.unwrap();

    TestGuard::new(server_process, stderr_buffer)
}

// Helper to find an available port
async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// Helper to get a binary path using CARGO_MANIFEST_DIR
fn get_binary_path(bin_name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // Go up to workspace root
    path.push("target");
    path.push("debug");
    path.push(bin_name);

    if !path.exists() {
        panic!(
            "{} binary not found at {:?}. Run 'cargo build --bin {}' first.",
            bin_name, path, bin_name
        );
    }

    path
}

// Helper to get the demo agent binary path
fn get_demo_agent_path() -> PathBuf {
    get_binary_path("acp-demo-agent")
}

// Helper to wait for server to be ready
async fn wait_for_server(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();
    let url = format!("http://127.0.0.1:{}/", port);

    for _ in 0..30 {
        // Try for 3 seconds
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err("Server did not start in time".into())
}

#[derive(Debug, Default)]
struct MessageCollector {
    session_notifications: usize,
    prompt_responses: usize,
    permission_requests: Vec<u64>, // Store the IDs for later response
    exit_messages: usize,
}

/// Helper function to collect WebSocket messages until first timeout
async fn collect_messages(
    ws_receiver: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    timeout_ms: u64,
) -> MessageCollector {
    let mut collector = MessageCollector::default();

    loop {
        let timeout_duration = Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout_duration, ws_receiver.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                    match msg.get("type").and_then(|t| t.as_str()) {
                        Some("session-notification") => collector.session_notifications += 1,
                        Some("prompt-response") => collector.prompt_responses += 1,
                        Some("permission-request") => {
                            if let Some(id) = msg.get("id").and_then(|id| id.as_u64()) {
                                collector.permission_requests.push(id);
                            }
                        }
                        Some("exit") => collector.exit_messages += 1,
                        _ => {}
                    }
                }
            }
            Ok(Some(Ok(_))) => {} // Other WebSocket message types - continue collecting
            Ok(Some(Err(_))) => break, // WebSocket error - stop collecting
            Ok(None) => break,    // WebSocket closed - stop collecting
            Err(_) => break,      // Timeout - stop collecting
        }
    }

    collector
}

#[tokio::test]
async fn test_full_acp_integration() {
    // Find available port and start server
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    let client = Client::new();
    let base_url = format!("http://127.0.0.1:{}", port);

    let demo_agent_path = get_demo_agent_path();

    // Test 1: Create ACP connection
    let create_connections_payload = json!([
        {
            "id": "demo-agent-1",
            "name": "Demo Agent",
            "command": format!("{} --mode rapid", demo_agent_path.to_string_lossy())
        }
    ]);

    let response = client
        .post(&format!("{}/acp/connections", base_url))
        .json(&create_connections_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let connections: Vec<Value> = response.json().await.unwrap();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0]["id"], "demo-agent-1");

    // The connection should start as loading
    assert_eq!(connections[0]["status"], "loading");

    // Test 2: Wait for connection to become ready by polling
    let mut connection_ready = false;
    for _ in 0..50 {
        // Wait up to 5 seconds
        let response = client
            .get(&format!("{}/acp/connections", base_url))
            .send()
            .await
            .unwrap();

        let connections: Vec<Value> = response.json().await.unwrap();
        if !connections.is_empty() && connections[0]["status"] == "connected" {
            connection_ready = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(connection_ready, "Connection did not become ready in time");

    // Test 3: Create a session
    let create_session_payload = json!({
        "acpId": "demo-agent-1"
    });

    let response = client
        .post(&format!("{}/acp/sessions", base_url))
        .json(&create_session_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let session_response: Value = response.json().await.unwrap();
    assert_eq!(session_response["status"], "created");

    let session_id = session_response["sessionId"].as_str().unwrap();
    assert!(!session_id.is_empty());

    // Test 4: List sessions
    let response = client
        .get(&format!("{}/acp/sessions", base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let sessions: Vec<Value> = response.json().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], session_id);
    assert_eq!(sessions[0]["acpId"], "demo-agent-1");

    // Test 5: Test WebSocket endpoint
    let ws_url = format!("ws://127.0.0.1:{}/acp/{}/ws", port, session_id);

    let connect_result = tokio::time::timeout(Duration::from_secs(5), connect_async(&ws_url)).await;
    let (ws_stream, _) = connect_result
        .expect("WebSocket connection timed out")
        .expect("WebSocket connection failed");

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Test WebSocket interaction in three phases:
    // Phase 1: Simple prompt -> expect notifications + prompt response
    // Phase 2: Write prompt -> expect notifications + permission request
    // Phase 3: Permission response -> expect notifications + prompt response

    // Phase 1: Send initial prompt and collect responses
    let prompt_msg = json!({
        "type": "prompt",
        "id": 0,
        "messages": [{"type": "text", "text": "Hey"}]
    });
    ws_sender
        .send(Message::Text(prompt_msg.to_string()))
        .await
        .unwrap();

    let phase1_messages = collect_messages(&mut ws_receiver, 100).await;
    assert!(
        phase1_messages.session_notifications >= 10,
        "Phase 1: Expected at least 10 session notifications, got {}",
        phase1_messages.session_notifications
    );
    assert_eq!(
        phase1_messages.prompt_responses, 1,
        "Phase 1: Expected exactly 1 prompt response, got {}",
        phase1_messages.prompt_responses
    );
    assert!(
        phase1_messages.permission_requests.is_empty(),
        "Phase 1: Expected no permission requests, got {}",
        phase1_messages.permission_requests.len()
    );

    // Phase 2: Send write prompt and collect permission request
    let write_prompt = json!({
        "type": "prompt",
        "id": 1,
        "messages": [{"type": "text", "text": "write"}]
    });
    ws_sender
        .send(Message::Text(write_prompt.to_string()))
        .await
        .unwrap();

    let phase2_messages = collect_messages(&mut ws_receiver, 100).await;
    assert!(
        !phase2_messages.permission_requests.is_empty(),
        "Phase 2: Expected at least one permission request, got {}",
        phase2_messages.permission_requests.len()
    );
    let perm_id = phase2_messages.permission_requests[0];

    // Phase 3: Send permission response and collect final messages
    let permission_response = json!({
        "type": "permission-response",
        "id": perm_id,
        "response": {
            "outcome": {
                "outcome": "selected",
                "optionId": "deny"
            }
        }
    });
    ws_sender
        .send(Message::Text(permission_response.to_string()))
        .await
        .unwrap();

    let phase3_messages = collect_messages(&mut ws_receiver, 100).await;
    assert!(
        phase3_messages.session_notifications > 0,
        "Phase 3: Expected more notifications after permission response, got {}",
        phase3_messages.session_notifications
    );
    assert_eq!(
        phase3_messages.prompt_responses, 1,
        "Phase 3: Expected exactly 1 prompt response to write request, got {}",
        phase3_messages.prompt_responses
    );

    // Close WebSocket
    ws_sender.close().await.unwrap()
}

#[tokio::test]
async fn test_acp_connection_error_handling() {
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    let client = Client::new();
    let base_url = format!("http://127.0.0.1:{}", port);

    // Test creating connection with invalid command
    let create_connections_payload = json!([
        {
            "id": "invalid-agent",
            "name": "Invalid Agent",
            "command": "definitely-does-not-exist-command-12345"
        }
    ]);

    let response = client
        .post(&format!("{}/acp/connections", base_url))
        .json(&create_connections_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let connections: Vec<Value> = response.json().await.unwrap();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0]["id"], "invalid-agent");

    // Should be in error state
    assert!(connections[0]["status"].is_object());
    if let Some(status_obj) = connections[0]["status"].as_object() {
        assert!(status_obj.contains_key("error"));
    }
}

#[tokio::test]
async fn test_session_creation_without_connection() {
    let port = find_available_port().await;

    let _guard = spawn_tidewave_server(port).await;

    let client = Client::new();
    let base_url = format!("http://127.0.0.1:{}", port);

    // Try to create session with nonexistent connection
    let create_session_payload = json!({
        "acpId": "nonexistent-connection"
    });

    let response = client
        .post(&format!("{}/acp/sessions", base_url))
        .json(&create_session_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_session_update_without_session() {
    let port = find_available_port().await;

    let _guard = spawn_tidewave_server(port).await;

    // Try to connect to nonexistent session WebSocket
    let ws_url = format!("ws://127.0.0.1:{}/acp/nonexistent-session/ws", port);
    let result = connect_async(&ws_url).await;

    // Should fail to connect
    assert!(result.is_err());
}

#[tokio::test]
async fn test_multiple_sessions_same_connection() {
    let port = find_available_port().await;

    let _guard = spawn_tidewave_server(port).await;

    let client = Client::new();
    let base_url = format!("http://127.0.0.1:{}", port);

    let demo_agent_path = get_demo_agent_path();

    // Create connection
    let create_connections_payload = json!([
        {
            "id": "multi-session-agent",
            "name": "Multi Session Agent",
            "command": format!("{} --mode rapid", demo_agent_path.to_string_lossy())
        }
    ]);

    let response = client
        .post(&format!("{}/acp/connections", base_url))
        .json(&create_connections_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Wait for connection to be ready
    for _ in 0..50 {
        let response = client
            .get(&format!("{}/acp/connections", base_url))
            .send()
            .await
            .unwrap();

        let connections: Vec<Value> = response.json().await.unwrap();
        if !connections.is_empty() && connections[0]["status"] == "connected" {
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Create multiple sessions
    let mut session_ids = Vec::new();
    for _i in 0..3 {
        let create_session_payload = json!({
            "acpId": "multi-session-agent"
        });

        let response = client
            .post(&format!("{}/acp/sessions", base_url))
            .json(&create_session_payload)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let session_response: Value = response.json().await.unwrap();
        assert_eq!(session_response["status"], "created");

        let session_id = session_response["sessionId"].as_str().unwrap().to_string();
        session_ids.push(session_id);
    }

    // Verify all sessions exist
    let response = client
        .get(&format!("{}/acp/sessions", base_url))
        .send()
        .await
        .unwrap();

    let sessions: Vec<Value> = response.json().await.unwrap();
    assert_eq!(sessions.len(), 3);

    // All sessions should be associated with the same connection
    for session in &sessions {
        assert_eq!(session["acpId"], "multi-session-agent");
    }
}
