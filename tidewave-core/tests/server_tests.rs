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

/// Start a file server serving the given data at the specified path
/// Returns (port, shutdown_sender)
async fn start_file_server(path: &'static str, data: Vec<u8>) -> (u16, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        use axum::body::Body;
        use axum::response::Response;
        use axum::{routing::get, Router};

        let app = Router::new().route(
            path,
            get(move || {
                let data = data.clone();
                async move {
                    Response::builder()
                        .header("content-length", data.len().to_string())
                        .body(Body::from(data))
                        .unwrap()
                }
            }),
        );

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                shutdown_rx.await.ok();
            })
            .await
            .unwrap();
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    (port, shutdown_tx)
}

/// Create a tar.gz archive in memory with a single file
fn create_test_tarball(path: &str, content: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use tar::Builder;

    let mut archive_data = Vec::new();
    let encoder = GzEncoder::new(&mut archive_data, Compression::default());
    let mut builder = Builder::new(encoder);

    let mut header = tar::Header::new_gnu();
    header.set_path(path).unwrap();
    header.set_size(content.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();

    builder.append(&header, content).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    archive_data
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
async fn test_about_includes_system_info() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();

    assert_eq!(body["name"], "tidewave-cli");
    assert!(body["version"].is_string());

    // Verify system info is present
    let system = &body["system"];
    assert!(system.is_object());
    assert!(system["os"].is_string());
    assert!(system["arch"].is_string());
    assert!(system["family"].is_string());
    assert!(system["target"].is_string());

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
async fn test_write_file_exclusive_succeeds_if_not_exists() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let temp_dir =
        std::env::temp_dir().join(format!("write_exclusive_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
    let file_path = temp_dir.join("new_file.txt");

    let client = reqwest::Client::new();

    // Write with exclusive=true to a new file - should succeed
    let response = client
        .post(format!("http://127.0.0.1:{}/write", port))
        .json(&serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "content": "new content",
            "exclusive": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["mtime"].is_number());
    assert_eq!(body["bytes_written"], 11);

    // Verify the file was created with correct content
    let content = fs::read_to_string(&file_path).expect("Failed to read file");
    assert_eq!(content, "new content");

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_write_file_exclusive_fails_if_exists() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a temp file that already exists
    let temp_dir =
        std::env::temp_dir().join(format!("write_exclusive_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
    let file_path = temp_dir.join("existing_file.txt");
    fs::write(&file_path, "original content").expect("Failed to write file");

    let client = reqwest::Client::new();

    // Try to write with exclusive=true - should fail atomically
    let response = client
        .post(format!("http://127.0.0.1:{}/write", port))
        .json(&serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "content": "new content",
            "exclusive": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);

    // Verify the original content is unchanged
    let content = fs::read_to_string(&file_path).expect("Failed to read file");
    assert_eq!(content, "original content");

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

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
async fn test_delete_file() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a temp file to delete
    let temp_dir = std::env::temp_dir().join(format!("delete_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
    let file_path = temp_dir.join("file_to_delete.txt");
    fs::write(&file_path, "content to delete").expect("Failed to write file");

    // Verify the file exists
    assert!(file_path.exists());

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/delete", port))
        .json(&serde_json::json!({
            "path": file_path.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    // Verify the file was deleted
    assert!(!file_path.exists());

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_delete_file_requires_absolute_path() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/delete", port))
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
async fn test_delete_file_nonexistent() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/delete", port))
        .json(&serde_json::json!({
            "path": "/nonexistent/path/that/does/not/exist.txt"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].is_string());

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_delete_file_cannot_delete_directory() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a temp directory
    let temp_dir = std::env::temp_dir().join(format!("delete_dir_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/delete", port))
        .json(&serde_json::json!({
            "path": temp_dir.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].is_string());

    // Verify the directory still exists
    assert!(temp_dir.exists());

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

    shutdown_tx.send(()).ok();
}

#[tokio::test]
#[cfg(unix)]
async fn test_delete_symlink_removes_link_not_target() {
    use std::fs;
    use std::os::unix::fs::symlink;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a temp directory with a file and a symlink to it
    let temp_dir =
        std::env::temp_dir().join(format!("delete_symlink_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let target_file = temp_dir.join("target.txt");
    fs::write(&target_file, "target content").expect("Failed to write target file");

    let symlink_path = temp_dir.join("symlink.txt");
    symlink(&target_file, &symlink_path).expect("Failed to create symlink");

    // Verify both exist
    assert!(target_file.exists());
    assert!(symlink_path.exists());

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/delete", port))
        .json(&serde_json::json!({
            "path": symlink_path.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    // Verify the symlink was deleted but the target still exists
    assert!(!symlink_path.exists());
    assert!(target_file.exists());

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

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
    assert_eq!(body["type"], "file");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_stat_directory() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let dir_path = std::env::current_dir()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "http://127.0.0.1:{}/stat?path={}",
            port,
            dir_path.to_str().unwrap()
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["mtime"].is_number());
    assert_eq!(body["type"], "directory");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_listdir() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    // Create a temp directory with known contents
    let temp_dir = std::env::temp_dir().join(format!("listdir_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    // Create test files and directories
    fs::write(temp_dir.join("file1.txt"), "content1").expect("Failed to write file1");
    fs::write(temp_dir.join("file2.rs"), "content2").expect("Failed to write file2");
    fs::create_dir(temp_dir.join("subdir")).expect("Failed to create subdir");
    fs::create_dir(temp_dir.join("another_dir")).expect("Failed to create another_dir");

    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "http://127.0.0.1:{}/listdir?path={}",
            port,
            temp_dir.to_str().unwrap()
        ))
        .send()
        .await
        .unwrap();

    // Clean up before assertions
    fs::remove_dir_all(&temp_dir).ok();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["entries"].is_array());

    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 4);

    // Check specific files exist with correct types
    let file1 = entries
        .iter()
        .find(|e| e["name"] == "file1.txt")
        .expect("file1.txt should exist");
    assert_eq!(file1["type"], "file");

    let file2 = entries
        .iter()
        .find(|e| e["name"] == "file2.rs")
        .expect("file2.rs should exist");
    assert_eq!(file2["type"], "file");

    let subdir = entries
        .iter()
        .find(|e| e["name"] == "subdir")
        .expect("subdir should exist");
    assert_eq!(subdir["type"], "directory");

    let another_dir = entries
        .iter()
        .find(|e| e["name"] == "another_dir")
        .expect("another_dir should exist");
    assert_eq!(another_dir["type"], "directory");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_listdir_requires_absolute_path() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "http://127.0.0.1:{}/listdir?path=relative/path",
            port
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_listdir_nonexistent() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .get(format!(
            "http://127.0.0.1:{}/listdir?path=/nonexistent/path/that/does/not/exist",
            port
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], false);
    assert!(body["error"].is_string());

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_mkdir() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let temp_dir = std::env::temp_dir().join(format!("mkdir_test_{}", uuid::Uuid::new_v4()));
    let nested_dir = temp_dir.join("nested").join("deep").join("directory");

    // Ensure the directory doesn't exist
    fs::remove_dir_all(&temp_dir).ok();

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/mkdir", port))
        .json(&serde_json::json!({
            "path": nested_dir.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    // Verify the directory was created
    assert!(nested_dir.exists());
    assert!(nested_dir.is_dir());

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_mkdir_requires_absolute_path() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://127.0.0.1:{}/mkdir", port))
        .json(&serde_json::json!({
            "path": "relative/path"
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_mkdir_already_exists() {
    use std::fs;

    let (port, shutdown_tx) = start_test_server(vec![]).await;

    let temp_dir = std::env::temp_dir().join(format!("mkdir_exists_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

    let client = reqwest::Client::new();

    // mkdir on an existing directory should succeed (create_dir_all behavior)
    let response = client
        .post(format!("http://127.0.0.1:{}/mkdir", port))
        .json(&serde_json::json!({
            "path": temp_dir.to_str().unwrap()
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    // Clean up
    fs::remove_dir_all(&temp_dir).ok();

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
    let test_content = b"Hello, World! This is test content for download.";
    let (file_port, file_shutdown_tx) = start_file_server("/test.txt", test_content.to_vec()).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!(
            "http://127.0.0.1:{port}/download?key=test_file&url=http://127.0.0.1:{file_port}/test.txt"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let chunks = parse_download_chunks(&response.bytes().await.unwrap());
    assert!(!chunks.is_empty(), "Should receive at least one chunk");

    let last_chunk = chunks.last().unwrap();
    assert_eq!(last_chunk["status"], "done");

    let path = last_chunk["path"].as_str().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), test_content);

    // Request again - should return immediately with done status (cached)
    let response2 = client
        .get(format!(
            "http://127.0.0.1:{port}/download?key=test_file&url=http://127.0.0.1:{file_port}/test.txt"
        ))
        .send()
        .await
        .unwrap();

    let chunks2 = parse_download_chunks(&response2.bytes().await.unwrap());
    assert_eq!(
        chunks2.len(),
        1,
        "Should receive only done message for cached file"
    );
    assert_eq!(chunks2[0]["status"], "done");

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_download_concurrent_with_throttle() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;
    let test_content: Vec<u8> = vec![b'x'; 102400]; // 100KB
    let test_key = format!("concurrent_test_{}", uuid::Uuid::new_v4());
    let (file_port, file_shutdown_tx) = start_file_server("/large.txt", test_content.clone()).await;

    let test_key1 = test_key.clone();
    let test_key2 = test_key.clone();

    let handle1 = tokio::spawn(async move {
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/download?key={test_key1}&url=http://127.0.0.1:{file_port}/large.txt&throttle=10240"
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("transfer-encoding")
                .map(|v| v.to_str().unwrap()),
            Some("chunked")
        );
        parse_download_chunks(&response.bytes().await.unwrap())
    });

    let handle2 = tokio::spawn(async move {
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/download?key={test_key2}&url=http://127.0.0.1:{file_port}/large.txt&throttle=10240"
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        parse_download_chunks(&response.bytes().await.unwrap())
    });

    let (chunks1, chunks2) = tokio::join!(handle1, handle2);
    let chunks1 = chunks1.unwrap();
    let chunks2 = chunks2.unwrap();

    assert!(
        chunks1.len() > 1,
        "Client 1 should receive multiple chunks. Got: {}",
        chunks1.len()
    );
    assert_eq!(chunks1.last().unwrap()["status"], "done");
    assert_eq!(chunks2.last().unwrap()["status"], "done");
    assert_eq!(
        chunks1.last().unwrap()["path"],
        chunks2.last().unwrap()["path"]
    );

    let progress_count = chunks1.iter().filter(|c| c["status"] == "progress").count();
    assert!(
        progress_count > 0,
        "Should receive progress updates. Got {}",
        progress_count
    );

    let path = chunks1.last().unwrap()["path"].as_str().unwrap();
    assert_eq!(std::fs::read(path).unwrap().len(), test_content.len());

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
#[cfg(unix)]
async fn test_download_with_executable_flag() {
    use std::os::unix::fs::PermissionsExt;

    let (port, shutdown_tx) = start_test_server(vec![]).await;
    let test_content = b"#!/bin/sh\necho 'Hello from script'";
    let (file_port, file_shutdown_tx) =
        start_file_server("/script.sh", test_content.to_vec()).await;

    let response = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/download?key=test_executable&url=http://127.0.0.1:{file_port}/script.sh&executable=true"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let chunks = parse_download_chunks(&response.bytes().await.unwrap());
    let last_chunk = chunks.last().unwrap();
    assert_eq!(last_chunk["status"], "done");

    let path = last_chunk["path"].as_str().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), test_content);

    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "File should have executable permissions. Mode: {:o}",
        mode
    );

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
#[cfg(unix)]
async fn test_download_with_extract_from_tarball() {
    use std::os::unix::fs::PermissionsExt;

    let (port, shutdown_tx) = start_test_server(vec![]).await;
    let test_content = b"#!/bin/sh\necho 'Hello'";
    let archive = create_test_tarball("package/bin/myexe", test_content);
    let (file_port, file_shutdown_tx) = start_file_server("/archive.tar.gz", archive).await;

    let test_key = format!("extract_test_{}", uuid::Uuid::new_v4());
    let response = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/download?key={test_key}&url=http://127.0.0.1:{file_port}/archive.tar.gz&extract=package/bin/myexe&executable=true"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let chunks = parse_download_chunks(&response.bytes().await.unwrap());
    assert!(!chunks.is_empty(), "Expected at least one chunk");

    let last_chunk = chunks.last().unwrap();
    assert_eq!(last_chunk["status"], "done", "Got: {:?}", last_chunk);

    let path = last_chunk["path"].as_str().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), test_content);
    assert!(std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0);

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_download_with_extract_file_not_found() {
    let (port, shutdown_tx) = start_test_server(vec![]).await;
    let archive = create_test_tarball("other/path/file.txt", b"content");
    let (file_port, file_shutdown_tx) = start_file_server("/archive.tar.gz", archive).await;

    let test_key = format!("extract_notfound_{}", uuid::Uuid::new_v4());
    let response = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/download?key={test_key}&url=http://127.0.0.1:{file_port}/archive.tar.gz&extract=nonexistent/path"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let chunks = parse_download_chunks(&response.bytes().await.unwrap());
    assert!(!chunks.is_empty(), "Expected at least one chunk");

    let last_chunk = chunks.last().unwrap();
    assert_eq!(last_chunk["status"], "error", "Got: {:?}", last_chunk);

    let message = last_chunk["message"].as_str().unwrap();
    assert!(message.contains("not found"), "Error: {}", message);
    assert!(
        message.contains("other/path/file.txt"),
        "Error: {}",
        message
    );

    file_shutdown_tx.send(()).ok();
    shutdown_tx.send(()).ok();
}
