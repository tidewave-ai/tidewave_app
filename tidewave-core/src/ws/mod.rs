//! WebSocket module for bidirectional communication.
//!
//! This module provides a unified WebSocket endpoint (`/ws`) that supports
//! multiple features over a single connection:
//! - `watch`: File system watching

pub mod connection;
pub mod watch;

use dashmap::DashMap;
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

/// Outbound message types for WebSocket communication
#[derive(Debug, Clone)]
pub enum WsOutboundMessage {
    Watch(WatchEvent),
    Pong,
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
