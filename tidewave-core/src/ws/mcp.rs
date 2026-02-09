//! The idea behind this is to provide a "reverse proxy" for ACP agents to connect to
//! the Tidewave Web tools (browser_eval, restart_app_server).
//!
//! When the browser tells the ACP agent what MCP servers to connect to, it provides
//! the session_id. The agent will then send POST requests to the `/socket/mcp-remote-client`
//! endpoint. When receiving such a POST request, we look up the registered browsers
//! and forward the raw MCP message to the browser. The response is routed back to the agent.

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
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, error, info, warn};

use crate::phoenix::{InitResult, PhxMessage};

use super::ChannelSender;

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
    pub sessions: Arc<DashMap<String, ChannelSender>>,
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
// Channel Init
// ============================================================================

/// Initialize an `mcp:{session_id}` channel.
///
/// Registers the browser connection for the given session and runs a forwarding
/// loop until the channel is left (incoming_rx closed).
pub async fn init(
    state: &McpChannelState,
    msg: &PhxMessage,
    outgoing_tx: UnboundedSender<PhxMessage>,
    mut incoming_rx: tokio::sync::mpsc::UnboundedReceiver<PhxMessage>,
) -> InitResult {
    // Extract session_id from topic "mcp:{session_id}"
    let session_id = match msg.topic.strip_prefix("mcp:") {
        Some(id) => id.to_string(),
        None => {
            return InitResult::Error(
                "invalid topic format, expected mcp:{session_id}".to_string(),
            );
        }
    };

    debug!("MCP channel join for session_id: {}", session_id);

    let sender = ChannelSender {
        tx: outgoing_tx.clone(),
        topic: msg.topic.clone(),
        join_ref: msg.join_ref.clone(),
    };

    // Register this channel's sender for the session
    state.sessions.insert(session_id.clone(), sender);

    // Send success reply
    let _ = outgoing_tx.send(PhxMessage::ok_reply(msg, json!({})));

    // Main loop: handle incoming messages from the browser
    loop {
        match incoming_rx.recv().await {
            Some(phx_msg) => {
                if phx_msg.event == "mcp_message" {
                    let json_rpc_message = phx_msg.payload;

                    // Check if this is a reply to a pending request
                    if let Some(id) = json_rpc_message.get("id") {
                        let key = (session_id.clone(), id.clone());
                        if let Some((_, response_tx)) = state.awaiting_answers.remove(&key) {
                            if response_tx.send(json_rpc_message.clone()).is_err() {
                                warn!("Failed to send response to waiting request");
                            }
                            continue;
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
                } else {
                    warn!("Unknown event in MCP channel: {}", phx_msg.event);
                }
            }
            None => {
                // Channel was left/disconnected
                break;
            }
        }
    }

    debug!("MCP channel terminating for session_id: {}", session_id);
    state.sessions.remove(&session_id);

    // Clean up any pending responses for this session
    let keys_to_remove: Vec<_> = state
        .awaiting_answers
        .iter()
        .filter(|entry| entry.key().0 == session_id)
        .map(|entry| entry.key().clone())
        .collect();

    for key in keys_to_remove {
        if let Some((_, tx)) = state.awaiting_answers.remove(&key) {
            let _ = tx.send(json!({
                "error": {
                    "code": -32000,
                    "message": "Channel connection closed"
                }
            }));
        }
    }

    InitResult::Done
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

    // Look up the session's sender in the registry
    let sender = match state.sessions.get(&session_id) {
        Some(sender) => sender.clone(),
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
    let response_rx = if let Some(id) = json_rpc_message.get("id") {
        let (tx, rx) = oneshot::channel();
        let key = (session_id.clone(), id.clone());
        state.awaiting_answers.insert(key, tx);
        Some(rx)
    } else {
        None
    };

    // Push the message directly to the browser
    sender.push("mcp_message", json_rpc_message.clone());

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
