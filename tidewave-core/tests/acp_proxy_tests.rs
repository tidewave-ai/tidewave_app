mod test_helpers;

use agent_client_protocol::{self as acp, Agent, ContentBlock, TextContent};
use std::time::Duration;
use test_helpers::*;

#[tokio::test]
async fn test_acp_client_connection() {
    // Start the tidewave server
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    // Get the demo agent binary path
    let demo_agent_path = get_demo_agent_path();
    let command = format!("{} --mode rapid", demo_agent_path.to_string_lossy());

    // Connect to WebSocket endpoint with the demo agent command
    let ws_url = format!(
        "ws://127.0.0.1:{}/acp/ws?command={}",
        port,
        urlencoding::encode(&command)
    );

    // Create ACP client connection using helper
    let local_set = tokio::task::LocalSet::new();
    let result: Result<(), Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            let _wrapper = create_acp_connection(&ws_url).await?;
            println!("ACP client connection established successfully");
            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}

#[tokio::test]
async fn test_acp_agent_prompt_response() {
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    let demo_agent_path = get_demo_agent_path();
    let command = format!("{} --mode rapid", demo_agent_path.to_string_lossy());
    let ws_url = format!(
        "ws://127.0.0.1:{}/acp/ws?command={}",
        port,
        urlencoding::encode(&command)
    );

    let local_set = tokio::task::LocalSet::new();
    let result: Result<(), Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            let mut wrapper = create_acp_connection(&ws_url).await?;

            // First create a session (agents need session_id for prompts)
            let session_response = wrapper
                .conn
                .new_session(acp::NewSessionRequest {
                    cwd: std::env::current_dir().unwrap(),
                    mcp_servers: vec![],
                    meta: None,
                })
                .await?;
            println!("Created session: {:?}", session_response.session_id);

            // Send a simple prompt
            println!("Sending prompt to agent...");
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                wrapper.conn.prompt(acp::PromptRequest {
                    session_id: session_response.session_id.clone(),
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: "Hello! Please respond briefly.".to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                }),
            )
            .await??;

            println!("Received response: {:?}", response);

            // Collect session notifications
            let mut notification_count = 0;
            while let Ok(notification) =
                tokio::time::timeout(Duration::from_millis(100), wrapper.notification_rx.recv())
                    .await
            {
                if let Some(notif) = notification {
                    println!("Received notification: {:?}", notif.update);
                    notification_count += 1;
                } else {
                    break;
                }
            }

            println!("Received {} notifications", notification_count);

            // Verify we got a response and notifications
            assert_eq!(
                response.stop_reason,
                acp::StopReason::EndTurn,
                "Should end turn successfully"
            );
            assert!(
                notification_count > 0,
                "Should receive session notifications"
            );

            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}

#[tokio::test]
async fn test_acp_agent_permission_request() {
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    let demo_agent_path = get_demo_agent_path();
    let command = format!("{} --mode rapid", demo_agent_path.to_string_lossy());
    let ws_url = format!(
        "ws://127.0.0.1:{}/acp/ws?command={}",
        port,
        urlencoding::encode(&command)
    );

    let local_set = tokio::task::LocalSet::new();
    let result: Result<(), Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            let mut wrapper = create_acp_connection(&ws_url).await?;

            // First create a session
            let session_response = wrapper
                .conn
                .new_session(acp::NewSessionRequest {
                    cwd: std::env::current_dir().unwrap(),
                    mcp_servers: vec![],
                    meta: None,
                })
                .await?;
            println!("Created session: {:?}", session_response.session_id);

            // Send a prompt that should trigger a permission request (e.g., "write")
            println!("Sending write prompt to trigger permission request...");
            let response = tokio::time::timeout(
                Duration::from_secs(10),
                wrapper.conn.prompt(acp::PromptRequest {
                    session_id: session_response.session_id.clone(),
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: "write".to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                }),
            )
            .await??;

            println!("Received response: {:?}", response);

            // Collect session notifications
            let mut notification_count = 0;
            while let Ok(notification) =
                tokio::time::timeout(Duration::from_millis(100), wrapper.notification_rx.recv())
                    .await
            {
                if let Some(notif) = notification {
                    println!("Received notification: {:?}", notif.update);
                    notification_count += 1;
                } else {
                    break;
                }
            }

            println!("Received {} notifications", notification_count);

            // The response should indicate that permission was denied (our TestClient always denies)
            assert_eq!(
                response.stop_reason,
                acp::StopReason::EndTurn,
                "Should end turn successfully"
            );
            assert!(
                notification_count > 0,
                "Should receive session notifications"
            );

            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}
