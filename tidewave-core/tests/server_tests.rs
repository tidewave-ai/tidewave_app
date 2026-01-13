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

    // Test 2: Evil origin should work on /about but valid_origin should be false
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["name"], "tidewave-cli");
    assert_eq!(body["valid_origin"], false);

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

    // Test that /about returns valid_origin=true when no Origin header
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid_origin"], true);

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn test_about_valid_origin_field() {
    let (port, shutdown_tx) = start_test_server(vec!["http://allowed.com".to_string()]).await;

    let client = reqwest::Client::new();

    // Test 1: No origin header should return valid_origin=true
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid_origin"], true);

    // Test 2: Allowed origin should return valid_origin=true
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .header("Origin", "http://allowed.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid_origin"], true);

    // Test 3: Localhost with matching port should return valid_origin=true
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .header("Origin", &format!("http://localhost:{}", port))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid_origin"], true);

    // Test 4: Evil origin should return valid_origin=false
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .header("Origin", "http://evil.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid_origin"], false);

    // Test 5: Localhost with wrong port should return valid_origin=false
    let response = client
        .get(format!("http://127.0.0.1:{}/about", port))
        .header("Origin", &format!("http://localhost:{}", port + 1000))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["valid_origin"], false);

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
