//! Phoenix channel implementation for MCP (Model Context Protocol) reverse proxy.
//!
//! This is a port of mcp_remote.rs to use Phoenix channels instead of raw WebSockets.
//! The channel topic format is `mcp:{session_id}` where session_id is the reverse MCP
//! connection identifier.
//!
//! The idea behind this is to provide a "reverse proxy" for ACP agents to connect to
//! the Tidewave Web tools (browser_eval, restart_app_server).
//!
//! When the browser tells the ACP agent what MCP servers to connect to, it provides
//! the session_id. The agent will then send POST requests to the `/acp/mcp-remote-client`
//! endpoint. When receiving such a POST request, we look up the registered browsers
//! and forward the raw MCP message to the browser. The response is routed back to the agent.

use crate::phoenix::{Channel, HandleResult, InfoSender, JoinResult, SocketRef};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use dashmap::DashMap;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

// ============================================================================
// Message Types
// ============================================================================

/// Message sent from background tasks to the channel's handle_info
#[derive(Debug)]
pub enum McpChannelInfo {
    /// A JSON-RPC message to forward to the browser
    JsonRpc {
        session_id: String,
        json_rpc_message: Value,
        response_tx: Option<oneshot::Sender<Value>>,
    },
}

#[derive(Deserialize)]
pub struct McpParams {
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
}

// ============================================================================
// State Types
// ============================================================================

#[derive(Clone)]
pub struct McpChannelState {
    /// Registry mapping session_id to channel sender
    pub sessions: Arc<DashMap<String, InfoSender<McpChannelInfo>>>,
    /// Pending responses waiting for answers from the browser
    pub awaiting_answers: Arc<DashMap<(String, Value), oneshot::Sender<Value>>>,
}

impl McpChannelState {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            awaiting_answers: Arc::new(DashMap::new()),
        }
    }
}

impl Default for McpChannelState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Phoenix Channel Implementation
// ============================================================================

/// Phoenix channel handler for MCP reverse proxy connections.
pub struct McpChannel {
    state: McpChannelState,
}

impl McpChannel {
    pub fn new() -> Self {
        Self {
            state: McpChannelState::new(),
        }
    }

    pub fn with_state(state: McpChannelState) -> Self {
        Self { state }
    }

    /// Get the state for use in HTTP handlers
    pub fn state(&self) -> McpChannelState {
        self.state.clone()
    }
}

impl Default for McpChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for McpChannel {
    async fn join(&self, topic: &str, _payload: Value, socket: &mut SocketRef) -> JoinResult {
        // Extract session_id from topic "mcp:{session_id}"
        let session_id = match topic.strip_prefix("mcp:") {
            Some(id) => id.to_string(),
            None => {
                return JoinResult::error(json!({
                    "reason": "invalid topic format, expected mcp:{session_id}"
                }));
            }
        };

        debug!("MCP channel join for session_id: {}", session_id);

        // Get info sender for forwarding messages from the HTTP handler
        let info_sender = socket.info_sender::<McpChannelInfo>();
        // Register this channel for the session
        self.state.sessions.insert(session_id.clone(), info_sender);

        JoinResult::ok(json!({
            "status": "registered",
            "session_id": session_id
        }))
    }

    async fn handle_in(&self, event: &str, payload: Value, socket: &mut SocketRef) -> HandleResult {
        match event {
            "mcp_message" => {
                // The payload is the raw JSON-RPC message
                let json_rpc_message = payload;

                // Parse session_id from topic "mcp:{session_id}"
                let session_id = socket.topic.strip_prefix("mcp:").unwrap().to_string();

                // Check if this is a reply to a pending request
                if let Some(id) = json_rpc_message.get("id") {
                    let key = (session_id.clone(), id.clone());
                    if let Some((_, response_tx)) = self.state.awaiting_answers.remove(&key) {
                        // This is a reply to a pending request
                        if response_tx.send(json_rpc_message.clone()).is_err() {
                            warn!("Failed to send response to waiting request");
                        }
                        return HandleResult::no_reply();
                    }
                }

                // This is a notification or unexpected message
                if json_rpc_message.get("id").is_some() {
                    error!(
                        "Did not expect a reply (or request) for session {}: {:?}",
                        session_id, json_rpc_message
                    );
                } else {
                    info!("Ignoring notification from browser: {:?}", json_rpc_message);
                }

                HandleResult::no_reply()
            }
            _ => {
                warn!("Unknown event in MCP channel: {}", event);
                HandleResult::no_reply()
            }
        }
    }

    async fn handle_info(&self, message: Box<dyn std::any::Any + Send>, socket: &mut SocketRef) {
        if let Ok(info) = message.downcast::<McpChannelInfo>() {
            match *info {
                McpChannelInfo::JsonRpc {
                    session_id,
                    json_rpc_message,
                    response_tx,
                } => {
                    // If this has a response channel, store it for later
                    if let Some(response_tx) = response_tx {
                        if let Some(id) = json_rpc_message.get("id") {
                            let key = (session_id.clone(), id.clone());
                            self.state.awaiting_answers.insert(key, response_tx);
                        }
                    }

                    // Forward the raw JSON-RPC message to the browser
                    socket.push("mcp_message", json_rpc_message);
                }
            }
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut SocketRef) {
        debug!("MCP channel terminating: {}", reason);

        let session_id = socket.topic.strip_prefix("mcp:").unwrap().to_string();

        // Remove this session from the registry
        self.state.sessions.remove(&session_id);

        // Clean up any pending responses for this session
        let keys_to_remove: Vec<_> = self
            .state
            .awaiting_answers
            .iter()
            .filter(|entry| entry.key().0 == session_id)
            .map(|entry| entry.key().clone())
            .collect();

        for key in keys_to_remove {
            if let Some((_, tx)) = self.state.awaiting_answers.remove(&key) {
                // Send an error response to any waiting requests
                let _ = tx.send(json!({
                    "error": {
                        "code": -32000,
                        "message": "Channel connection closed"
                    }
                }));
            }
        }
    }
}

// ============================================================================
// HTTP Handler for Agent Requests
// ============================================================================

/// HTTP POST handler for MCP remote client requests.
/// This is called by the ACP agent to send MCP messages to the browser.
pub async fn mcp_channel_client_handler(
    Query(params): Query<McpParams>,
    State(state): State<McpChannelState>,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let session_id = params.session_id.ok_or(StatusCode::BAD_REQUEST)?;

    debug!("MCP channel client request for session: {}", session_id);

    // Parse the JSON-RPC message from the request body
    let json_rpc_message: Value = serde_json::from_slice(&body).map_err(|e| {
        error!("Failed to parse JSON-RPC message: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    // Basic validation that this looks like a JSON-RPC message
    if json_rpc_message.get("jsonrpc").is_none() {
        error!("Invalid JSON-RPC message: missing 'jsonrpc' field");
        return Err(StatusCode::BAD_REQUEST);
    }

    // Look up the session in the registry
    let session = match state.sessions.get(&session_id) {
        Some(session) => session.clone(),
        None => {
            error!("Session not found: {}", session_id);
            let error_response = json!({
                "jsonrpc": "2.0",
                "id": json_rpc_message.get("id"),
                "error": {
                    "code": -32000,
                    "message": "Browser is not connected. Abort any generation until a manual user retry."
                }
            });
            return Ok(Json(error_response).into_response());
        }
    };

    // Create response channel if this is a request (has "id")
    let (response_tx, response_rx) = if json_rpc_message.get("id").is_some() {
        let (tx, rx) = oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Send the message to the channel
    let info = McpChannelInfo::JsonRpc {
        session_id: session_id.clone(),
        json_rpc_message: json_rpc_message.clone(),
        response_tx,
    };

    if session.send(info).is_err() {
        error!("Failed to send message to channel");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // If this is a request, wait for the response and return JSON
    if let Some(response_rx) = response_rx {
        match response_rx.await {
            Ok(response) => {
                debug!("Got response: {:?}", response);
                Ok(Json(response).into_response())
            }
            Err(_) => {
                error!("Response channel closed unexpectedly");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        // This is a notification or response - return HTTP 202 Accepted with no body
        Ok(Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(axum::body::Body::empty())
            .unwrap())
    }
}
