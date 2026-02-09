//! WebSocket module for bidirectional communication using Phoenix V2 wire format.
//!
//! This module provides a unified WebSocket endpoint (`/socket/websocket`) that supports
//! multiple features over a single connection:
//! - `watch`: File system watching (topics: `watch:<ref>`)
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

pub mod connection;
pub mod watch;

use tracing::debug;
use uuid::Uuid;

pub use connection::ws_handler;
pub use watch::WatchFeatureState;

// ============================================================================
// Types
// ============================================================================

pub type WebSocketId = Uuid;

/// Global WebSocket state
#[derive(Clone, Default)]
pub struct WsState {
    /// Watch feature state
    pub watch: WatchFeatureState,
}

impl WsState {
    pub fn new() -> Self {
        Self {
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
