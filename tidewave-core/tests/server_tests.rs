use axum::http::StatusCode;
use tidewave_core::config::Config;
use tidewave_core::server::serve_http_server_with_listener;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

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
        serve_http_server_with_listener(config, listener, async {
            shutdown_rx.await.ok();
        })
        .await
        .unwrap();
    });

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
async fn test_verify_origin_mismatch() {
    let (port, shutdown_tx) = start_test_server(vec!["http://localhost:3000".to_string()]).await;

    let client = reqwest::Client::new();

    // Test 1: Evil origin should be blocked on /stat
    let response = client
        .get(format!("http://127.0.0.1:{}/stat?path=/tmp", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Test 2: Evil origin should work on /check-origin and return valid: false
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], false);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_localhost_with_matching_port() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();

    // Test 1: localhost with matching port should be allowed
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("http://localhost:{}", port))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    // Test 2: 127.0.0.1 with matching port should be allowed
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("http://127.0.0.1:{}", port))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    // Test 3: *.localhost with matching port should be allowed
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("http://myapp.localhost:{}", port))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_localhost_with_wrong_port() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();
    let wrong_port = if port == 8080 { 8081 } else { 8080 };

    // Test 1: localhost with wrong port should be blocked
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("http://localhost:{}", wrong_port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Test 2: 127.0.0.1 with wrong port should be blocked
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("http://127.0.0.1:{}", wrong_port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Test 3: *.localhost with wrong port should be blocked
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("http://myapp.localhost:{}", wrong_port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_localhost_with_wrong_scheme() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();

    // Test 1: HTTPS origin when server is HTTP should be blocked
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("https://localhost:{}", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Test 2: HTTPS with 127.0.0.1 should be blocked
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("https://127.0.0.1:{}", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Test 3: HTTPS with *.localhost should be blocked
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", format!("https://myapp.localhost:{}", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_subdomain_localhost_variants() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();

    // Test various *.localhost subdomains
    let subdomains = vec![
        "app.localhost",
        "my-app.localhost",
        "test.app.localhost",
        "deeply.nested.app.localhost",
    ];

    for subdomain in subdomains {
        let response = client
            .get(format!(
                "http://127.0.0.1:{}/stat?path={}",
                port,
                test_path.display()
            ))
            .header("Origin", format!("http://{}:{}", subdomain, port))
            .send()
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "Subdomain {} should be allowed",
            subdomain
        );
    }

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_non_localhost_domains_blocked() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();

    // Test that non-localhost domains are blocked even with matching port
    let domains = vec![
        format!("http://example.com:{}", port),
        format!("http://evil.com:{}", port),
        format!("http://localhost.com:{}", port), // Note: not *.localhost
        format!("http://notlocalhost:{}", port),
    ];

    for domain in domains {
        let response = client
            .get(format!(
                "http://127.0.0.1:{}/stat?path={}",
                port,
                test_path.display()
            ))
            .header("Origin", &domain)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "Domain {} should be blocked",
            domain
        );
    }

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_allowed_origins_work() {
    let (port, shutdown_tx) = start_test_server(vec![
        "http://example.com:3000".to_string(),
        "https://app.example.com".to_string(),
    ])
    .await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();

    // Test that explicitly allowed origins still work
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", "http://example.com:3000")
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_verify_origin_no_origin_header_allowed() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();
    let test_path = std::env::temp_dir();

    // Test that requests without Origin header are allowed
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            test_path.display()
        ))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    // Test that /check-origin returns valid=true when no Origin header
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], true);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_check_origin_endpoint() {
    let (port, shutdown_tx) = start_test_server(vec!["http://allowed.com".to_string()]).await;

    let client = reqwest::Client::new();

    // Test 1: No origin header should return valid=true
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], true);

    // Test 2: Allowed origin should return valid=true
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .header("Origin", "http://allowed.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], true);

    // Test 3: Localhost with matching port should return valid=true
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .header("Origin", &format!("http://localhost:{}", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], true);

    // Test 4: Evil origin should return valid=false
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], false);

    // Test 5: Localhost with wrong port should return valid=false
    let response = client
        .post(format!("http://127.0.0.1:{}/check-origin", port))
        .header("Origin", &format!("http://localhost:{}", port + 1000))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid"], false);

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

#[tokio::test]
async fn test_shell_echo_command() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/shell", port))
        .json(&serde_json::json!({
            "command": "echo hello"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.bytes().await.unwrap();
    let output = String::from_utf8_lossy(&bytes);
    assert!(output.contains("hello"));

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_read_file() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let readme_path = std::env::current_dir()
        .unwrap()
        .parent()
        .unwrap()
        .join("README.md");

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/read", port))
        .json(&serde_json::json!({
            "path": readme_path.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["content"].is_string());
    assert!(body["mtime"].is_number());

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_read_file_requires_absolute_path() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/read", port))
        .json(&serde_json::json!({
            "path": "relative/path.txt"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_stat_file() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let readme_path = std::env::current_dir()
        .unwrap()
        .parent()
        .unwrap()
        .join("README.md");

    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            readme_path.to_str().unwrap()
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["mtime"].is_number());

    shutdown_tx.send(()).ok();
}

// ============================================================================
// Download Tests
// ============================================================================

/// Parse download chunks from the response
/// Format: Newline-delimited JSON (NDJSON)
fn parse_download_chunks(bytes: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[tokio::test]
async fn test_download_basic() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a simple test file served by the test server
    let test_content = "Hello, World! This is test content for download.";

    // Start a simple file server on a different port
    let file_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let file_port = file_listener.local_addr().unwrap().port();

    let (file_shutdown_tx, file_shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        use axum::{Router, routing::get, body::Body, response::Response};

        let app = Router::new().route("/test.txt", get(move || async move {
            Response::builder()
                .header("content-length", test_content.len().to_string())
                .body(Body::from(test_content))
                .unwrap()
        }));

        axum::serve(file_listener, app)
            .with_graceful_shutdown(async {
                file_shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });

    // Give the file server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client = reqwest::Client::new();

    // Download the file
    let response = client
        .get(format!(
            "http://127.0.0.1:{}/download?key=test_file&url=http://127.0.0.1:{}/test.txt",
            port, file_port
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Parse the chunks
    let bytes = response.bytes().await.unwrap();
    let chunks = parse_download_chunks(&bytes);

    // Should have at least a done chunk
    assert!(!chunks.is_empty(), "Should receive at least one chunk");

    // Last chunk should be "done"
    let last_chunk = chunks.last().unwrap();
    assert_eq!(last_chunk["status"], "done");
    assert!(last_chunk["path"].is_string());

    // Verify the file was downloaded
    let path = last_chunk["path"].as_str().unwrap();
    let downloaded_content = std::fs::read_to_string(path).unwrap();
    assert_eq!(downloaded_content, test_content);

    // Request the same file again - should return immediately with done status
    let response2 = client
        .get(format!(
            "http://127.0.0.1:{}/download?key=test_file&url=http://127.0.0.1:{}/test.txt",
            port, file_port
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);

    // Parse response - should get done message immediately
    let bytes2 = response2.bytes().await.unwrap();
    let chunks2 = parse_download_chunks(&bytes2);
    assert_eq!(chunks2.len(), 1, "Should receive only done message for cached file");
    assert_eq!(chunks2[0]["status"], "done");

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_download_concurrent_with_throttle() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a 100KB test file to ensure we get progress updates at 1% increments
    let test_content = "x".repeat(102400);

    // Use a unique key for this test run to avoid conflicts with cached files
    let test_key = format!("concurrent_test_{}", uuid::Uuid::new_v4());

    // Start a simple file server
    let file_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let file_port = file_listener.local_addr().unwrap().port();

    let (file_shutdown_tx, file_shutdown_rx) = oneshot::channel();

    let test_content_clone = test_content.clone();
    tokio::spawn(async move {
        use axum::{Router, routing::get, body::Body, response::Response};

        let app = Router::new().route("/large.txt", get(|| async move {
            Response::builder()
                .header("content-length", test_content_clone.len().to_string())
                .body(Body::from(test_content_clone))
                .unwrap()
        }));

        axum::serve(file_listener, app)
            .with_graceful_shutdown(async {
                file_shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });

    // Give the file server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let client1 = reqwest::Client::new();
    let client2 = reqwest::Client::new();

    // Start two concurrent downloads with 10KB/s throttle (should take ~10 seconds for 100KB)
    // With 1% progress updates, we expect ~100 chunks
    let port1 = port;
    let port2 = port;
    let file_port1 = file_port;
    let file_port2 = file_port;

    let test_key1 = test_key.clone();
    let handle1 = tokio::spawn(async move {
        let response = client1
            .get(format!(
                "http://127.0.0.1:{}/download?key={}&url=http://127.0.0.1:{}/large.txt&throttle=10240",
                port1, test_key1, file_port1
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Verify chunked transfer encoding is used
        assert_eq!(
            response.headers().get("transfer-encoding").map(|v| v.to_str().unwrap()),
            Some("chunked"),
            "Should use chunked transfer encoding for streaming"
        );

        let bytes = response.bytes().await.unwrap();
        parse_download_chunks(&bytes)
    });

    // Start the second download immediately (no sleep needed - throttling ensures overlap)
    let test_key2 = test_key.clone();
    let handle2 = tokio::spawn(async move {
        let response = client2
            .get(format!(
                "http://127.0.0.1:{}/download?key={}&url=http://127.0.0.1:{}/large.txt&throttle=10240",
                port2, test_key2, file_port2
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.bytes().await.unwrap();
        parse_download_chunks(&bytes)
    });

    let (chunks1, chunks2) = tokio::join!(handle1, handle2);
    let chunks1 = chunks1.unwrap();
    let chunks2 = chunks2.unwrap();

    // With throttling, the first client should receive multiple chunks (progress + done)
    // 100KB file with 10KB/s throttle = ~10 seconds, with 1% updates = ~100 chunks
    assert!(
        chunks1.len() > 1,
        "Client 1 should receive multiple chunks due to throttling. Got: {}",
        chunks1.len()
    );

    // Both should end with "done"
    assert_eq!(chunks1.last().unwrap()["status"], "done");
    assert_eq!(chunks2.last().unwrap()["status"], "done");

    // Both should have the same final path (proving they shared the download)
    assert_eq!(
        chunks1.last().unwrap()["path"],
        chunks2.last().unwrap()["path"]
    );

    // Client 1 should have received progress updates (not just "done")
    let progress_count1 = chunks1.iter().filter(|c| c["status"] == "progress").count();
    assert!(
        progress_count1 > 0,
        "Client 1 should receive progress updates. Got {} progress chunks",
        progress_count1
    );

    // Both clients should have received at least one chunk
    assert!(
        chunks1.len() >= 1 && chunks2.len() >= 1,
        "Both clients should receive chunks"
    );

    // Verify the downloaded file exists and has correct content
    let path = chunks1.last().unwrap()["path"].as_str().unwrap();
    let downloaded_content = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        downloaded_content.len(),
        test_content.len(),
        "Downloaded file should have correct size"
    );

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}
