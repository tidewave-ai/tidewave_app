use axum::http::StatusCode;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tidewave_core::config::Config;
use tidewave_core::server::serve_http_server_with_shutdown;

// ============================================================================
// Test Helpers
// ============================================================================

/// Start a test HTTP server with configurable allowed origins
/// Returns (port, shutdown_sender)
async fn start_test_server(allowed_origins: Vec<String>) -> (u16, oneshot::Sender<()>) {
    let config = Config {
        port: 0, // Random port
        allowed_origins,
        ..Default::default()
    };

    // Bind the server to get the port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    // Create a shutdown channel
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // Start the server in a background task
    tokio::spawn(async move {
        serve_http_server_with_shutdown(config, listener, async {
            shutdown_rx.await.ok();
        })
        .await
        .unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    (port, shutdown_tx)
}

// ============================================================================
// Integration Tests
// ============================================================================

#[tokio::test]
async fn test_localhost_proxy_retry() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create an HTTP client
    let client = reqwest::Client::new();

    // Test 1: Proxy to localhost:PORT/about (should work directly)
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/proxy?url=http://localhost:{}/about",
            port, port
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["name"], "tidewave-cli");

    // Test 2: Proxy to tidewave-app-test.localhost:PORT/about (should retry to 127.0.0.1)
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/proxy?url=http://tidewave-app-test.localhost:{}/about",
            port, port
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["name"], "tidewave-cli");

    // Shutdown the server
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_integration() {
    let (port, shutdown_tx) = start_test_server(vec!["http://localhost:3000".to_string()]).await;

    let client = reqwest::Client::new();

    // Test 1: Allowed origin should pass on /stat
    let response = client
        .get(format!("http://127.0.0.1:{}/stat?path=/tmp", port))
        .header("Origin", "http://localhost:3000")
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    // Test 2: Evil origin should be blocked on /stat
    let response = client
        .get(format!("http://127.0.0.1:{}/stat?path=/tmp", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Test 3: Evil origin should work on /about
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_which_finds_common_command() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/which", port))
        .json(&serde_json::json!({
            "command": "sh"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["path"].is_string());
    let path = body["path"].as_str().unwrap();
    assert!(!path.is_empty());
    assert!(path.contains("sh"));

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_which_nonexistent_command() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/which", port))
        .json(&serde_json::json!({
            "command": "this_command_definitely_does_not_exist_12345"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["path"].is_null());

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_which_respects_custom_path() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let temp_dir = std::env::temp_dir().join(format!("which_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let executable_name = "test_executable_unique_12345";
    let executable_path = temp_dir.join(executable_name);
    fs::write(&executable_path, "#!/bin/sh\necho test").expect("Failed to write executable");

    let mut perms = fs::metadata(&executable_path)
        .expect("Failed to get metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&executable_path, perms).expect("Failed to set permissions");

    let client = reqwest::Client::new();

    // First, verify the binary is NOT found without custom PATH
    let response = client
        .post(format!("http://127.0.0.1:{}/which", port))
        .json(&serde_json::json!({
            "command": executable_name
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body["path"].is_null(),
        "Binary should not be found in system PATH"
    );

    // Now verify the binary IS found with custom PATH
    let mut env = std::collections::HashMap::new();
    env.insert("PATH".to_string(), temp_dir.to_string_lossy().to_string());

    let response = client
        .post(format!("http://127.0.0.1:{}/which", port))
        .json(&serde_json::json!({
            "command": executable_name,
            "cwd": temp_dir.to_string_lossy(),
            "env": env
        }))
        .send()
        .await
        .unwrap();

    fs::remove_dir_all(&temp_dir).ok();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["path"].is_string());
    let path = body["path"].as_str().unwrap();
    assert!(path.contains(executable_name));
    assert!(path.contains(temp_dir.to_string_lossy().as_ref()));

    shutdown_tx.send(()).ok();
}
