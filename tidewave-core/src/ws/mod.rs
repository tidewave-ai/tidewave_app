//! WebSocket module for bidirectional communication.
//!
//! This module provides a unified WebSocket endpoint (`/ws`) that supports
//! multiple features over a single connection:
//! - `watch`: File system watching
//!
//! # Protocol
//!
//! All messages (except ping/pong) include a `topic` field for routing:
//!
//! Client → Server:
//! ```json
//! {"topic": "watch", "action": "subscribe", "path": "/foo/bar", "ref": "watch1"}
//! {"topic": "watch", "action": "unsubscribe", "ref": "watch1"}
//! ```
//!
//! Server → Client:
//! ```json
//! {"topic": "watch", "event": "subscribed", "ref": "watch1"}
//! {"topic": "watch", "event": "modified", "path": "/foo/bar/file.txt", "ref": "watch1"}
//! ```

pub mod connection;
pub mod watch;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
use uuid::Uuid;

pub use connection::ws_handler;
pub use watch::{WatchClientMessage, WatchEvent, WatchFeatureState};

// ============================================================================
// Types
// ============================================================================

pub type WebSocketId = Uuid;

/// Inbound message with topic-based routing (client → server)
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "topic")]
pub enum ClientMessage {
    #[serde(rename = "watch")]
    Watch {
        #[serde(flatten)]
        message: WatchClientMessage,
    },
}

/// Outbound message types for WebSocket communication (server → client)
#[derive(Debug, Clone)]
pub enum WsOutboundMessage {
    Watch(WatchEvent),
    Pong,
}

/// Helper struct for serializing outbound messages with topic field
#[derive(Serialize)]
struct TopicMessage<'a, T: Serialize> {
    topic: &'a str,
    #[serde(flatten)]
    inner: T,
}

/// Global WebSocket state
#[derive(Clone, Default)]
pub struct WsState {
    /// WebSocket connections (websocket_id → sender)
    pub websocket_senders: Arc<DashMap<WebSocketId, UnboundedSender<WsOutboundMessage>>>,
    /// Watch feature state
    pub watch: WatchFeatureState,
}

impl WsState {
    pub fn new() -> Self {
        Self {
            websocket_senders: Arc::new(DashMap::new()),
            watch: WatchFeatureState::new(),
        }
    }

    /// Clear all state with logging
    pub fn clear(&self) {
        let watch_count = self.watch.watchers.len();
        debug!("Found {} file watchers to clean up", watch_count);
        self.watch.watchers.clear();
    }
}
