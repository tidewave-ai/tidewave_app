use agent_client_protocol::{self as acp, Agent};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

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

/// Test client implementation for connecting to our demo agent
struct TestClient {
    permission_requests: Arc<AtomicUsize>,
}

impl TestClient {
    fn new(permission_requests: Arc<AtomicUsize>) -> Self {
        Self {
            permission_requests,
        }
    }
}

#[async_trait(?Send)]
impl acp::Client for TestClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        // Track that a permission request was made
        self.permission_requests.fetch_add(1, Ordering::SeqCst);

        // Auto-approve all permissions for testing
        Ok(acp::RequestPermissionResponse {
            outcome: acp::RequestPermissionOutcome::Selected {
                option_id: acp::PermissionOptionId("allow".into()),
            },
            meta: None,
        })
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> Result<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        println!("Received session notification: {:?}", args.update);
        Ok(())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

/// Helper function to spawn the demo agent binary and create a connection
async fn spawn_demo_agent() -> Result<(
    acp::ClientSideConnection,
    tokio::process::Child,
    Arc<AtomicUsize>,
)> {
    let binary_path = get_binary_path("acp-demo-agent");

    let mut child = tokio::process::Command::new(&binary_path)
        .args(&["--mode", "rapid"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let outgoing = child.stdin.take().unwrap().compat_write();
    let incoming = child.stdout.take().unwrap().compat();

    // Create permission request counter
    let permission_requests = Arc::new(AtomicUsize::new(0));

    // Create the client connection
    let (conn, handle_io) = acp::ClientSideConnection::new(
        TestClient::new(permission_requests.clone()),
        outgoing,
        incoming,
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );

    // Handle I/O in the background
    tokio::task::spawn_local(handle_io);

    Ok((conn, child, permission_requests))
}

#[tokio::test]
async fn test_agent_initialization() {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            let (conn, mut child, _) = spawn_demo_agent().await.expect("Failed to spawn agent");

            // Test initialization
            let init_response = timeout(
                Duration::from_secs(5),
                conn.initialize(acp::InitializeRequest {
                    protocol_version: acp::V1,
                    client_capabilities: acp::ClientCapabilities::default(),
                    meta: None,
                }),
            )
            .await
            .expect("Initialization timed out")
            .expect("Initialization failed");

            assert_eq!(init_response.protocol_version, acp::V1);
            println!("✓ Agent initialized successfully");

            // Clean up
            child.kill().await.ok();
        })
        .await;
}

#[tokio::test]
async fn test_session_creation() {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            let (conn, mut child, _) = spawn_demo_agent().await.expect("Failed to spawn agent");

            // Initialize first
            conn.initialize(acp::InitializeRequest {
                protocol_version: acp::V1,
                client_capabilities: acp::ClientCapabilities::default(),
                meta: None,
            })
            .await
            .expect("Initialization failed");

            // Create a new session
            let session_response = timeout(
                Duration::from_secs(5),
                conn.new_session(acp::NewSessionRequest {
                    mcp_servers: Vec::new(),
                    cwd: std::env::current_dir().unwrap(),
                    meta: None,
                }),
            )
            .await
            .expect("Session creation timed out")
            .expect("Session creation failed");

            assert!(!session_response.session_id.0.is_empty());
            println!("✓ Session created: {}", session_response.session_id.0);

            // Clean up
            child.kill().await.ok();
        })
        .await;
}

#[tokio::test]
async fn test_prompt_handling() {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            let (conn, mut child, _) = spawn_demo_agent().await.expect("Failed to spawn agent");

            // Initialize
            conn.initialize(acp::InitializeRequest {
                protocol_version: acp::V1,
                client_capabilities: acp::ClientCapabilities::default(),
                meta: None,
            })
            .await
            .expect("Initialization failed");

            // Create session
            let session_response = conn
                .new_session(acp::NewSessionRequest {
                    mcp_servers: Vec::new(),
                    cwd: std::env::current_dir().unwrap(),
                    meta: None,
                })
                .await
                .expect("Session creation failed");

            // Send a prompt
            let prompt_text = "Hello, demo agent!";
            let prompt_response = timeout(
                Duration::from_secs(10),
                conn.prompt(acp::PromptRequest {
                    session_id: session_response.session_id,
                    prompt: vec![prompt_text.into()],
                    meta: None,
                }),
            )
            .await
            .expect("Prompt timed out")
            .expect("Prompt failed");

            assert_eq!(prompt_response.stop_reason, acp::StopReason::EndTurn);

            // The demo agent sends content through session notifications, not in the response
            println!(
                "✓ Prompt handled successfully with stop reason: {:?}",
                prompt_response.stop_reason
            );

            // Clean up
            child.kill().await.ok();
        })
        .await;
}

#[tokio::test]
async fn test_authentication_flow() {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            let binary_path = get_binary_path("acp-demo-agent");

            let mut child = tokio::process::Command::new(&binary_path)
                .args(&["--mode", "rapid", "--require-auth"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .expect("Failed to spawn agent");

            let outgoing = child.stdin.take().unwrap().compat_write();
            let incoming = child.stdout.take().unwrap().compat();

            // Create permission request counter (not used in this test)
            let permission_requests = Arc::new(AtomicUsize::new(0));

            let (conn, handle_io) = acp::ClientSideConnection::new(
                TestClient::new(permission_requests),
                outgoing,
                incoming,
                |fut| {
                    tokio::task::spawn_local(fut);
                },
            );

            // Handle I/O in the background
            tokio::task::spawn_local(handle_io);

            // Initialize
            conn.initialize(acp::InitializeRequest {
                protocol_version: acp::V1,
                client_capabilities: acp::ClientCapabilities::default(),
                meta: None,
            })
            .await
            .expect("Initialization failed");

            // Try to create session without auth (should fail)
            let session_result = conn
                .new_session(acp::NewSessionRequest {
                    mcp_servers: Vec::new(),
                    cwd: std::env::current_dir().unwrap(),
                    meta: None,
                })
                .await;

            assert!(session_result.is_err());
            println!("✓ Authentication requirement enforced");

            // Clean up
            child.kill().await.ok();
        })
        .await;
}

#[tokio::test]
async fn test_extension_methods() {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            let (conn, mut child, _) = spawn_demo_agent().await.expect("Failed to spawn agent");

            // Initialize
            conn.initialize(acp::InitializeRequest {
                protocol_version: acp::V1,
                client_capabilities: acp::ClientCapabilities::default(),
                meta: None,
            })
            .await
            .expect("Initialization failed");

            // Test extension method (demo agent returns example response)
            let ext_result = conn
                .ext_method(acp::ExtRequest {
                    method: "test_method".into(),
                    params: serde_json::value::to_raw_value(&serde_json::json!({}))
                        .unwrap()
                        .into(),
                })
                .await;

            assert!(ext_result.is_ok(), "Extension method should succeed");
            println!("✓ Extension method handled successfully");

            // Test extension notification (should succeed)
            let ext_notification_result = conn
                .ext_notification(acp::ExtNotification {
                    method: "test_notification".into(),
                    params: serde_json::value::to_raw_value(&serde_json::json!({}))
                        .unwrap()
                        .into(),
                })
                .await;

            assert!(ext_notification_result.is_ok());
            println!("✓ Extension notification handled");

            // Clean up
            child.kill().await.ok();
        })
        .await;
}

#[tokio::test]
async fn test_file_operations_with_permissions() {
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async {
            let (conn, mut child, permission_requests) =
                spawn_demo_agent().await.expect("Failed to spawn agent");

            // Initialize
            conn.initialize(acp::InitializeRequest {
                protocol_version: acp::V1,
                client_capabilities: acp::ClientCapabilities::default(),
                meta: None,
            })
            .await
            .expect("Initialization failed");

            // Create session
            let session = conn
                .new_session(acp::NewSessionRequest {
                    mcp_servers: vec![],
                    cwd: std::env::current_dir().unwrap(),
                    meta: None,
                })
                .await
                .expect("Session creation failed");

            println!("✓ Session created with ID: {:?}", session.session_id);

            // Test write operation (should trigger permission request)
            let write_prompt = acp::PromptRequest {
                session_id: session.session_id.clone(),
                prompt: vec![acp::ContentBlock::Text(acp::TextContent {
                    text: "write path:<<test_file.txt>> content:<<Test content>>".to_string(),
                    annotations: None,
                    meta: None,
                })],
                meta: None,
            };

            let write_response = timeout(Duration::from_secs(5), conn.prompt(write_prompt))
                .await
                .expect("Write prompt timed out")
                .expect("Write prompt failed");

            assert_eq!(write_response.stop_reason, acp::StopReason::EndTurn);
            println!("✓ Write operation handled with permissions");

            // Test read operation (should not require permission)
            let read_prompt = acp::PromptRequest {
                session_id: session.session_id.clone(),
                prompt: vec![acp::ContentBlock::Text(acp::TextContent {
                    text: "read path:<<test_file.txt>>".to_string(),
                    annotations: None,
                    meta: None,
                })],
                meta: None,
            };

            let read_response = timeout(Duration::from_secs(5), conn.prompt(read_prompt))
                .await
                .expect("Read prompt timed out")
                .expect("Read prompt failed");

            assert_eq!(read_response.stop_reason, acp::StopReason::EndTurn);
            println!("✓ Read operation handled without permission request");

            // Test shell command (should trigger permission)
            let shell_prompt = acp::PromptRequest {
                session_id: session.session_id.clone(),
                prompt: vec![acp::ContentBlock::Text(acp::TextContent {
                    text: "shell shell=echo hello".to_string(),
                    annotations: None,
                    meta: None,
                })],
                meta: None,
            };

            let shell_response = timeout(Duration::from_secs(5), conn.prompt(shell_prompt))
                .await
                .expect("Shell prompt timed out")
                .expect("Shell prompt failed");

            assert_eq!(shell_response.stop_reason, acp::StopReason::EndTurn);
            println!("✓ Shell operation handled with permissions");

            // Verify that permission requests were made
            let total_requests = permission_requests.load(Ordering::SeqCst);
            // Should have 2 permission requests: 1 for write, 1 for shell (read doesn't require permission)
            assert_eq!(
                total_requests, 2,
                "Expected 2 permission requests (write + shell), got {}",
                total_requests
            );
            println!(
                "✓ Verified {} permission requests were made as expected",
                total_requests
            );

            // Clean up test file
            std::fs::remove_file("test_file.txt").ok();

            // Clean up
            child.kill().await.ok();
        })
        .await;
}
