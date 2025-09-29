mod test_helpers;

use agent_client_protocol::{self as acp, Agent, ContentBlock, SessionId, TextContent};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use test_helpers::*;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn test_custom_session_load_protocol() {
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
    let result: Result<SessionId, Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            let mut wrapper = create_acp_connection(&ws_url).await?;

            // Create session first to get a session ID
            let session_response = wrapper
                .conn
                .new_session(acp::NewSessionRequest {
                    cwd: std::env::current_dir().unwrap(),
                    mcp_servers: vec![],
                    meta: None,
                })
                .await?;
            let session_id = session_response.session_id.clone();
            println!("Created session: {}", session_id);

            // Send some message to generate notifications in the buffer
            let _prompt_response = wrapper
                .conn
                .prompt(acp::PromptRequest {
                    session_id: session_id.clone(),
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: "Hello".to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                })
                .await?;

            // Collect some notifications
            let mut collected_notifications = 0;
            while let Ok(Some(_notification)) =
                tokio::time::timeout(Duration::from_millis(100), wrapper.notification_rx.recv())
                    .await
            {
                collected_notifications += 1;
                if collected_notifications >= 3 {
                    break;
                }
            }

            Ok(session_id)
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
    let session_id = result.unwrap();

    let result2: Result<(), Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            // Test custom session/load protocol - try to load with a non-existent session ID first
            let ws_url2 = format!(
                "ws://127.0.0.1:{}/acp/ws?command={}",
                port,
                urlencoding::encode(&command)
            );
            let (ws_stream, _) = connect_async(&ws_url2).await?;
            let (mut ws_sender, mut ws_receiver) = ws_stream.split();

            // Send initialize first
            let init_msg = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "1",
                    "clientCapabilities": {}
                }
            });
            ws_sender.send(Message::Text(init_msg.to_string())).await?;

            // Wait for initialize response
            if let Some(Ok(Message::Text(text))) = ws_receiver.next().await {
                let response: Value = serde_json::from_str(&text)?;
                assert_eq!(response["jsonrpc"], "2.0");
                assert!(response["result"].is_object());
            }

            // Try to load non-existent session (should fail)
            let load_msg = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "_tidewave.ai/session/load",
                "params": {
                    "sessionId": "non-existent-session",
                    "latestId": "0"
                }
            });
            ws_sender.send(Message::Text(load_msg.to_string())).await?;

            // Should get error response
            if let Some(Ok(Message::Text(text))) = ws_receiver.next().await {
                let response: Value = serde_json::from_str(&text)?;
                println!(
                    "Non-existent session response: {}",
                    serde_json::to_string_pretty(&response)?
                );
                assert_eq!(response["id"], 2);
                assert!(response["error"].is_object());
                assert_eq!(response["error"]["code"], -32002);
                assert!(response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("Session not found"));
            }

            // Try to load existing session (should succeed and replay buffered messages)
            let load_existing_msg = json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "_tidewave.ai/session/load",
                "params": {
                    "sessionId": session_id,
                    "latestId": "0"
                }
            });
            ws_sender
                .send(Message::Text(load_existing_msg.to_string()))
                .await?;

            // Should get buffered messages followed by success response
            let mut message_count = 0;
            let mut got_success_response = false;

            while let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(2), ws_receiver.next()).await
            {
                let message: Value = serde_json::from_str(&text)?;

                if message.get("id") == Some(&json!(3)) {
                    // This is our response
                    println!(
                        "Session load response: {}",
                        serde_json::to_string_pretty(&message)?
                    );
                    assert!(message["result"].is_object());
                    got_success_response = true;
                    break;
                } else if message.get("method").is_some() {
                    // This is a buffered notification/request
                    message_count += 1;
                }
            }

            assert!(
                got_success_response,
                "Should receive success response for session load"
            );
            assert!(message_count > 0, "Should replay buffered messages");

            Ok(())
        })
        .await;

    assert!(result2.is_ok(), "Test should succeed: {:?}", result2);
}

#[tokio::test]
async fn test_tidewave_ack_notification() {
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

            // Create session
            let session_response = wrapper
                .conn
                .new_session(acp::NewSessionRequest {
                    cwd: std::env::current_dir().unwrap(),
                    mcp_servers: vec![],
                    meta: None,
                })
                .await?;
            let session_id = session_response.session_id.clone();

            // Generate some notifications
            let _prompt_response = wrapper
                .conn
                .prompt(acp::PromptRequest {
                    session_id: session_id.clone(),
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: "Generate some notifications".to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                })
                .await?;

            // Collect some notifications
            let mut notification_count = 0;
            while let Ok(Some(_notification)) =
                tokio::time::timeout(Duration::from_millis(100), wrapper.notification_rx.recv())
                    .await
            {
                notification_count += 1;
                if notification_count >= 2 {
                    break;
                }
            }

            assert!(
                notification_count > 0,
                "Should have collected some notifications"
            );

            // Create a second WebSocket connection to test ACK
            let ws_url2 = format!(
                "ws://127.0.0.1:{}/acp/ws?command={}",
                port,
                urlencoding::encode(&command)
            );
            let (ws_stream, _) = connect_async(&ws_url2).await?;
            let (mut ws_sender, mut ws_receiver) = ws_stream.split();

            // Initialize the connection
            let init_msg = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "1",
                    "clientCapabilities": {}
                }
            });
            ws_sender.send(Message::Text(init_msg.to_string())).await?;

            // Read initialize response
            if let Some(Ok(Message::Text(_text))) = ws_receiver.next().await {
                // Successfully initialized
            }

            // Load the session
            let load_msg = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "_tidewave.ai/session/load",
                "params": {
                    "sessionId": session_id.0,
                    "latestId": "0"
                }
            });
            ws_sender.send(Message::Text(load_msg.to_string())).await?;

            // Skip through the replayed messages until we get the success response
            while let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(2), ws_receiver.next()).await
            {
                let message: Value = serde_json::from_str(&text)?;
                if message.get("id") == Some(&json!(2)) {
                    println!(
                        "ACK test session load response: {}",
                        serde_json::to_string_pretty(&message)?
                    );
                    assert!(message["result"].is_object());
                    break;
                }
            }

            // Send ACK notification to acknowledge some notifications
            let ack_msg = json!({
                "jsonrpc": "2.0",
                "method": "_tidewave.ai/ack",
                "params": {
                    "sessionId": session_id.0,
                    "latestId": "notif_2"
                }
            });
            ws_sender.send(Message::Text(ack_msg.to_string())).await?;

            // ACK is a notification, so no response expected
            // But the server should prune the buffer internally

            println!("Successfully sent ACK notification");

            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}

#[tokio::test]
async fn test_session_reconnection_and_buffer_replay() {
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    let demo_agent_path = get_demo_agent_path();
    let command = format!("{} --mode rapid", demo_agent_path.to_string_lossy());
    let ws_url = format!(
        "ws://127.0.0.1:{}/acp/ws?command={}",
        port,
        urlencoding::encode(&command)
    );

    // First, run the session creation in a completely separate LocalSet
    let session_id = {
        let local_set1 = tokio::task::LocalSet::new();
        let session_id: SessionId = local_set1
            .run_until(async move {
                let mut wrapper = create_acp_connection(&ws_url).await?;

                let session_response = wrapper
                    .conn
                    .new_session(acp::NewSessionRequest {
                        cwd: std::env::current_dir().unwrap(),
                        mcp_servers: vec![],
                        meta: None,
                    })
                    .await?;
                let session_id = session_response.session_id.clone();
                println!("Created session: {}", session_id.0);

                // Generate notifications
                let _prompt_response = wrapper
                    .conn
                    .prompt(acp::PromptRequest {
                        session_id: session_id.clone(),
                        prompt: vec![ContentBlock::Text(TextContent {
                            text: "Create notifications for buffer test".to_string(),
                            annotations: None,
                            meta: None,
                        })],
                        meta: None,
                    })
                    .await?;

                // Collect some notifications
                let mut notification_count = 0;
                while let Ok(Some(_notification)) =
                    tokio::time::timeout(Duration::from_millis(100), wrapper.notification_rx.recv())
                        .await
                {
                    notification_count += 1;
                    if notification_count >= 3 {
                        break;
                    }
                }

                println!("Generated {} notifications", notification_count);

                Ok::<SessionId, Box<dyn std::error::Error>>(session_id)
                // Connection drops here, simulating disconnect
            })
            .await
            .unwrap();
        session_id
    };

    // Add a small delay to ensure cleanup
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second connection: reconnect and load session in a separate LocalSet
    let local_set2 = tokio::task::LocalSet::new();
    let result: Result<(), Box<dyn std::error::Error>> = local_set2
        .run_until(async move {
            let ws_url2 = format!(
                "ws://127.0.0.1:{}/acp/ws?command={}",
                port,
                urlencoding::encode(&command)
            );
            let (ws_stream, _) = connect_async(&ws_url2).await?;
            let (mut ws_sender, mut ws_receiver) = ws_stream.split();

            // Initialize
            let init_msg = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "1",
                    "clientCapabilities": {}
                }
            });
            ws_sender.send(Message::Text(init_msg.to_string())).await?;

            // Read init response
            if let Some(Ok(Message::Text(_text))) = ws_receiver.next().await {
                // Successfully initialized
            }

            // Load session from the beginning
            let load_msg = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "_tidewave.ai/session/load",
                "params": {
                    "sessionId": session_id.0,
                    "latestId": "0"
                }
            });
            ws_sender.send(Message::Text(load_msg.to_string())).await?;

            // Count replayed messages
            let mut replayed_count = 0;
            let mut got_success = false;

            while let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(3), ws_receiver.next()).await
            {
                let message: Value = serde_json::from_str(&text)?;

                if message.get("id") == Some(&json!(2)) {
                    // Success response
                    println!(
                        "Reconnection test session load response: {}",
                        serde_json::to_string_pretty(&message)?
                    );
                    assert!(message["result"].is_object());
                    got_success = true;
                    break;
                } else if message.get("method").is_some() {
                    // Replayed notification
                    replayed_count += 1;
                }
            }

            assert!(got_success, "Should get success response");
            assert!(replayed_count > 0, "Should replay buffered messages");
            println!("Replayed {} messages on reconnection", replayed_count);

            // Test partial replay - load from a middle point
            let partial_load_msg = json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "_tidewave.ai/session/load",
                "params": {
                    "sessionId": session_id.0,
                    "latestId": "notif_1"
                }
            });
            ws_sender
                .send(Message::Text(partial_load_msg.to_string()))
                .await?;

            let mut partial_replayed_count = 0;
            let mut got_partial_success = false;

            while let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(2), ws_receiver.next()).await
            {
                let message: Value = serde_json::from_str(&text)?;

                if message.get("id") == Some(&json!(3)) {
                    got_partial_success = true;
                    break;
                } else if message.get("method").is_some() {
                    partial_replayed_count += 1;
                }
            }

            assert!(
                got_partial_success,
                "Should get success response for partial load"
            );
            println!("Partial replay: {} messages", partial_replayed_count);

            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}

#[tokio::test]
async fn test_multiple_websocket_connections_same_process() {
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    let demo_agent_path = get_demo_agent_path();
    let command = format!("{} --mode rapid", demo_agent_path.to_string_lossy());

    let local_set = tokio::task::LocalSet::new();
    let result: Result<(), Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            // Create two connections with the same command (should reuse same process)
            let ws_url1 = format!(
                "ws://127.0.0.1:{}/acp/ws?command={}",
                port,
                urlencoding::encode(&command)
            );
            let ws_url2 = format!(
                "ws://127.0.0.1:{}/acp/ws?command={}",
                port,
                urlencoding::encode(&command)
            );

            let wrapper1 = create_acp_connection(&ws_url1).await?;
            let wrapper2 = create_acp_connection(&ws_url2).await?;

            // Create sessions on both connections
            let session1 = wrapper1
                .conn
                .new_session(acp::NewSessionRequest {
                    cwd: std::env::current_dir().unwrap(),
                    mcp_servers: vec![],
                    meta: None,
                })
                .await?;

            let session2 = wrapper2
                .conn
                .new_session(acp::NewSessionRequest {
                    cwd: std::env::current_dir().unwrap(),
                    mcp_servers: vec![],
                    meta: None,
                })
                .await?;

            // Both sessions should be created successfully
            assert!(!session1.session_id.0.is_empty());
            assert!(!session2.session_id.0.is_empty());
            assert_ne!(
                session1.session_id, session2.session_id,
                "Sessions should have different IDs"
            );

            // Both connections should be able to send prompts
            let response1 = tokio::time::timeout(
                Duration::from_secs(5),
                wrapper1.conn.prompt(acp::PromptRequest {
                    session_id: session1.session_id,
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: "Connection 1 test".to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                }),
            )
            .await??;

            let response2 = tokio::time::timeout(
                Duration::from_secs(5),
                wrapper2.conn.prompt(acp::PromptRequest {
                    session_id: session2.session_id,
                    prompt: vec![ContentBlock::Text(TextContent {
                        text: "Connection 2 test".to_string(),
                        annotations: None,
                        meta: None,
                    })],
                    meta: None,
                }),
            )
            .await??;

            assert_eq!(response1.stop_reason, acp::StopReason::EndTurn);
            assert_eq!(response2.stop_reason, acp::StopReason::EndTurn);

            println!("Successfully tested multiple WebSocket connections to same process");
            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}

#[tokio::test]
async fn test_process_exit_notifications() {
    let port = find_available_port().await;
    let _guard = spawn_tidewave_server(port).await;

    // Use a command that doesn't exist to trigger process start failure
    let invalid_command = "nonexistent-command-that-should-fail";
    let ws_url = format!(
        "ws://127.0.0.1:{}/acp/ws?command={}",
        port,
        urlencoding::encode(invalid_command)
    );

    let local_set = tokio::task::LocalSet::new();
    let result: Result<(), Box<dyn std::error::Error>> = local_set
        .run_until(async move {
            let (ws_stream, _) = connect_async(&ws_url).await?;
            let (mut ws_sender, mut ws_receiver) = ws_stream.split();

            // Send initialize request
            let init_msg = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "1",
                    "clientCapabilities": {}
                }
            });
            ws_sender.send(Message::Text(init_msg.to_string())).await?;

            // Should receive exit notification due to process start failure
            let mut got_exit_notification = false;
            while let Ok(Some(Ok(Message::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(5), ws_receiver.next()).await
            {
                let message: Value = serde_json::from_str(&text)?;

                if let Some(method) = message.get("method") {
                    if method.as_str() == Some("_tidewave.ai/exit") {
                        got_exit_notification = true;

                        // Verify exit notification structure
                        if let Some(params) = message.get("params") {
                            assert!(
                                params.get("error").is_some(),
                                "Exit notification should have error field"
                            );
                            assert!(
                                params.get("message").is_some(),
                                "Exit notification should have message field"
                            );

                            let error = params.get("error").unwrap().as_str().unwrap();
                            assert_eq!(
                                error, "process_start_failed",
                                "Should indicate process start failure"
                            );

                            println!("Received expected exit notification: {}", params);
                        }
                        break;
                    }
                }
            }

            assert!(
                got_exit_notification,
                "Should receive _tidewave.ai/exit notification on process failure"
            );
            Ok(())
        })
        .await;

    assert!(result.is_ok(), "Test should succeed: {:?}", result);
}
