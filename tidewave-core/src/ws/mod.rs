//! WebSocket module for bidirectional communication using Phoenix V2 wire format.
//!
//! This module provides a unified WebSocket endpoint (`/socket/websocket`) that supports
//! multiple features over a single connection:
//! - `watch`: File system watching (topics: `watch:<ref>`)
//! - `acp`: ACP (Agent Client Protocol) proxy (topics: `acp:<id>`)
//! - `mcp`: MCP (Model Context Protocol) reverse proxy (topics: `mcp:<session_id>`)
//!
//! # Protocol
//!
//! Messages use the Phoenix V2 JSON array format: `[join_ref, ref, topic, event, payload]`
//!
//! Client → Server:
//! ```json
//! ["1", "1", "watch:watch1", "phx_join", {"path": "/foo/bar"}]
//! ["1", "2", "watch:watch1", "phx_leave", {}]
//! [null, "3", "phoenix", "heartbeat", {}]
//! ```
//!
//! Server → Client:
//! ```json
//! ["1", "1", "watch:watch1", "phx_reply", {"status": "ok", "response": {"path": "/canonical/path"}}]
//! ["1", null, "watch:watch1", "modified", {"path": "file.txt"}]
//! [null, "3", "phoenix", "phx_reply", {"status": "ok", "response": {}}]
//! ```

pub mod acp;
pub mod connection;
pub mod mcp;
pub mod terminal;
pub mod watch;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
use uuid::Uuid;

use crate::phoenix::PhxMessage;
pub use connection::ws_handler;

// ============================================================================
// Types
// ============================================================================

pub type WebSocketId = Uuid;

/// Lightweight handle for pushing messages to a channel's client.
///
/// Wraps an `UnboundedSender<PhxMessage>` with topic and join_ref so that
/// channel handlers and HTTP handlers can push events without constructing
/// full `PhxMessage` structs every time.
#[derive(Clone)]
pub struct ChannelSender {
    pub tx: UnboundedSender<PhxMessage>,
    pub topic: String,
    pub join_ref: Option<String>,
}

impl ChannelSender {
    pub fn push(&self, event: impl Into<String>, payload: Value) {
        let mut phx = PhxMessage::new(&self.topic, event, payload);
        phx.join_ref = self.join_ref.clone();
        let _ = self.tx.send(phx);
    }
}

/// Global WebSocket state
#[derive(Clone)]
pub struct WsState {
    /// Watch feature state
    pub watch: watch::WatchFeatureState,
    /// ACP channel state
    pub acp: acp::AcpChannelState,
    /// MCP channel state
    pub mcp: mcp::McpChannelState,
}

impl WsState {
    pub fn new() -> Self {
        Self {
            watch: watch::WatchFeatureState::new(),
            acp: acp::AcpChannelState::new(),
            mcp: mcp::McpChannelState::new(),
        }
    }

    /// Use a custom ACP channel state (e.g. with a test process starter).
    pub fn with_acp_state(mut self, acp: acp::AcpChannelState) -> Self {
        self.acp = acp;
        self
    }

    /// Use a custom MCP channel state.
    pub fn with_mcp_state(mut self, mcp: mcp::McpChannelState) -> Self {
        self.mcp = mcp;
        self
    }

    /// Clear all state with logging, including ACP process cleanup.
    pub async fn clear(&self) {
        // Clean up ACP channel processes
        let process_count = self.acp.processes.len();
        debug!("Found {} ACP channel processes to clean up", process_count);
        for entry in self.acp.processes.iter() {
            let process_state = entry.value();
            if let Some(exit_tx) = process_state.exit_tx.read().await.as_ref() {
                let _ = exit_tx.send(());
            }
        }

        // Clean up file watchers
        let watch_count = self.watch.watchers.len();
        debug!("Found {} file watchers to clean up", watch_count);
        self.watch.watchers.clear();
    }
}
