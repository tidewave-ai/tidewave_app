use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{Json, Response},
};
use dashmap::DashMap;
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    time::timeout,
};
use tracing::{debug, error, info, warn};

// Global registry for MCP sessions
pub type McpRegistry = Arc<DashMap<String, McpSession>>;

#[derive(Clone, Debug)]
pub struct McpSession {
    pub sender: mpsc::UnboundedSender<McpMessage>,
}

#[derive(Debug)]
pub enum McpMessage {
    System {
        message: String,
    },
    JsonRpc {
        session_id: String,
        json_rpc_message: Value,
        response_tx: Option<oneshot::Sender<Value>>,
    },
}

#[derive(Clone)]
pub struct McpRemoteState {
    pub registry: McpRegistry,
    pub awaiting_answers: Arc<RwLock<HashMap<(String, Value), oneshot::Sender<Value>>>>,
}

impl McpRemoteState {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(DashMap::new()),
            awaiting_answers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Deserialize)]
pub struct McpParams {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Deserialize)]
pub struct McpRemoteMessage {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "jsonRpcMessage")]
    json_rpc_message: Value,
}

#[derive(Serialize)]
#[serde(tag = "type")]
pub enum McpOutboundMessage {
    #[serde(rename = "system")]
    System { message: String },
    #[serde(rename = "mcp-jsonrpc")]
    McpJsonRpc {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "jsonRpcMessage")]
        json_rpc_message: Value,
    },
}

// WebSocket handler for MCP remote connections
pub async fn mcp_remote_ws_handler(
    ws: WebSocketUpgrade,
    Query(_params): Query<McpParams>,
    State(state): State<McpRemoteState>,
) -> Response {
    info!("MCP Remote WebSocket connection requested");
    ws.on_upgrade(move |socket| handle_mcp_remote_socket(socket, state))
}

async fn handle_mcp_remote_socket(socket: WebSocket, state: McpRemoteState) {
    debug!("MCP Remote WebSocket connection established");

    let (ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<McpMessage>();

    // Track registered session IDs for cleanup
    let registered_sessions = Arc::new(RwLock::new(Vec::<String>::new()));

    // Task to send messages to WebSocket
    let sender_task = {
        let mut ws_sender = ws_sender;
        let awaiting_answers = state.awaiting_answers.clone();

        tokio::spawn(async move {
            while let Some(mcp_msg) = rx.recv().await {
                match mcp_msg {
                    McpMessage::System { message } => {
                        let outbound = McpOutboundMessage::System { message };

                        let json_str = match serde_json::to_string(&outbound) {
                            Ok(s) => s,
                            Err(e) => {
                                error!("Failed to serialize outbound message: {}", e);
                                continue;
                            }
                        };

                        if ws_sender
                            .send(Message::Text(json_str.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    McpMessage::JsonRpc {
                        session_id,
                        json_rpc_message,
                        response_tx,
                    } => {
                        let outbound = McpOutboundMessage::McpJsonRpc {
                            session_id: session_id.clone(),
                            json_rpc_message: json_rpc_message.clone(),
                        };

                        let json_str = match serde_json::to_string(&outbound) {
                            Ok(s) => s,
                            Err(e) => {
                                error!("Failed to serialize outbound message: {}", e);
                                continue;
                            }
                        };

                        // If this has a response channel, store it for later
                        if let Some(response_tx) = response_tx {
                            if let Some(id) = json_rpc_message.get("id") {
                                let key = (session_id, id.clone());
                                awaiting_answers.write().await.insert(key, response_tx);
                            }
                        }

                        if ws_sender
                            .send(Message::Text(json_str.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            debug!("MCP Remote WebSocket sender finished");
        })
    };

    // Task to receive messages from WebSocket
    let receiver_task = {
        let awaiting_answers = state.awaiting_answers.clone();
        let registry = state.registry.clone();
        let registered_sessions = registered_sessions.clone();

        tokio::spawn(async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if text == "ping" {
                            // Handle ping-pong
                            if let Err(e) = tx.send(McpMessage::System {
                                message: "pong".to_string(),
                            }) {
                                error!("Failed to send pong: {}", e);
                                break;
                            }
                            continue;
                        }

                        // Handle registration
                        if let Some(session_id) = text.strip_prefix("register-") {
                            let session = McpSession { sender: tx.clone() };
                            registry.insert(session_id.to_string(), session);

                            // Track this session for cleanup
                            registered_sessions
                                .write()
                                .await
                                .push(session_id.to_string());

                            // Send registration confirmation
                            if let Err(e) = tx.send(McpMessage::System {
                                message: format!("registered-{}", session_id),
                            }) {
                                error!("Failed to send registration confirmation: {}", e);
                                break;
                            }
                            continue;
                        }

                        // Handle JSON-RPC messages
                        let message: McpRemoteMessage = match serde_json::from_str(&text) {
                            Ok(msg) => msg,
                            Err(e) => {
                                error!("Failed to parse incoming message: {}", e);
                                continue;
                            }
                        };

                        // Check if this is a reply to a pending request
                        if let Some(id) = message.json_rpc_message.get("id") {
                            let key = (message.session_id.clone(), id.clone());
                            if let Some(response_tx) = awaiting_answers.write().await.remove(&key) {
                                // This is a reply to a pending request
                                if let Err(_) = response_tx.send(message.json_rpc_message.clone()) {
                                    warn!("Failed to send response to waiting request");
                                }
                                continue;
                            }
                        }

                        // This is a notification or unexpected message
                        if message.json_rpc_message.get("id").is_some() {
                            error!(
                                "Did not expect a reply (or request) for session {}: {:?}",
                                message.session_id, message.json_rpc_message
                            );
                        } else {
                            info!(
                                "Ignoring notification from browser: {:?}",
                                message.json_rpc_message
                            );
                        }
                    }
                    Ok(Message::Close(_)) => {
                        debug!("MCP Remote WebSocket closed by client");
                        break;
                    }
                    Err(e) => {
                        error!("MCP Remote WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            debug!("MCP Remote WebSocket receiver finished");
        })
    };

    // Wait for tasks to complete
    let _ = tokio::join!(sender_task, receiver_task);

    // Cleanup: Remove all registered sessions from registry
    let sessions_to_cleanup = registered_sessions.read().await.clone();
    for session_id in sessions_to_cleanup {
        debug!("Cleaning up session: {}", session_id);
        state.registry.remove(&session_id);

        // Clean up any pending responses for this session and notify waiters
        let mut awaiting = state.awaiting_answers.write().await;
        let mut to_remove = Vec::new();

        for (key, _) in awaiting.iter() {
            if key.0 == session_id {
                to_remove.push(key.clone());
            }
        }

        for key in to_remove {
            if let Some(tx) = awaiting.remove(&key) {
                // Send an error response to any waiting requests
                let _ = tx.send(serde_json::json!({
                    "error": {
                        "code": -32000,
                        "message": "WebSocket connection closed"
                    }
                }));
            }
        }
    }

    info!("MCP Remote WebSocket connection closed");
}

// HTTP POST handler for MCP remote client requests
pub async fn mcp_remote_client_handler(
    Query(params): Query<McpParams>,
    State(state): State<McpRemoteState>,
    body: Bytes,
) -> Result<Json<Value>, StatusCode> {
    let session_id = params.session_id.ok_or(StatusCode::BAD_REQUEST)?;

    debug!("MCP Remote client request for session: {}", session_id);

    // Parse the JSON-RPC message from the request body
    let json_rpc_message: Value = serde_json::from_slice(&body).map_err(|e| {
        error!("Failed to parse JSON-RPC message: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    // Look up the session in the registry
    let session = state.registry.get(&session_id).ok_or_else(|| {
        error!("Session not found: {}", session_id);
        StatusCode::NOT_FOUND
    })?;

    // Create response channel if this is a request (has "id")
    let (response_tx, response_rx) = if json_rpc_message.get("id").is_some() {
        let (tx, rx) = oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Send the message to the WebSocket
    let mcp_message = McpMessage::JsonRpc {
        session_id: session_id.clone(),
        json_rpc_message: json_rpc_message.clone(),
        response_tx,
    };

    session.sender.send(mcp_message).map_err(|e| {
        error!("Failed to send message to WebSocket: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // If this is a request, wait for the response
    if let Some(response_rx) = response_rx {
        match timeout(Duration::from_secs(30), response_rx).await {
            Ok(Ok(response)) => Ok(Json(response)),
            Ok(Err(_)) => {
                error!("Response channel closed unexpectedly");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
            Err(_) => {
                error!("Timeout waiting for response");
                Err(StatusCode::REQUEST_TIMEOUT)
            }
        }
    } else {
        // This is a notification, no response expected
        Ok(Json(serde_json::json!({"status": "ok"})))
    }
}
