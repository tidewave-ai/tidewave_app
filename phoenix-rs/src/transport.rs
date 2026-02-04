use std::sync::Arc;

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
    routing::get,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::message::PhxMessage;
use crate::registry::ChannelRegistry;
use crate::serializer;
use crate::socket::Socket;

/// Shared state for the Phoenix WebSocket server
#[derive(Clone)]
pub struct PhoenixState {
    /// Channel registry for routing messages to handlers
    registry: Arc<ChannelRegistry>,
    /// All connected sockets: socket_id -> sender
    sockets: Arc<DashMap<String, mpsc::UnboundedSender<PhxMessage>>>,
    /// Topic subscriptions: topic -> set of socket_ids
    topic_subscribers: Arc<DashMap<String, dashmap::DashSet<String>>>,
}

impl PhoenixState {
    /// Create a new Phoenix state with the given registry
    pub fn new(registry: ChannelRegistry) -> Self {
        Self {
            registry: Arc::new(registry),
            sockets: Arc::new(DashMap::new()),
            topic_subscribers: Arc::new(DashMap::new()),
        }
    }

    /// Get the number of connected sockets
    pub fn socket_count(&self) -> usize {
        self.sockets.len()
    }

    /// Get the number of subscribers for a topic
    pub fn topic_subscriber_count(&self, topic: &str) -> usize {
        self.topic_subscribers
            .get(topic)
            .map(|s| s.len())
            .unwrap_or(0)
    }
}

/// Create a router for Phoenix WebSocket connections
///
/// # Arguments
/// * `registry` - The channel registry with topic handlers
///
/// # Example
/// ```ignore
/// let mut registry = ChannelRegistry::new();
/// registry.register("room:*", MyRoomChannel);
///
/// let app = phoenix_router(registry)
///     .merge(other_routes);
///
/// axum::serve(listener, app).await?;
/// ```
pub fn phoenix_router(registry: ChannelRegistry) -> Router {
    let state = PhoenixState::new(registry);

    Router::new()
        .route("/socket/websocket", get(ws_handler))
        .with_state(state)
}

/// Create a router with a custom path prefix
///
/// The WebSocket endpoint will be at `{path}/websocket` to match Phoenix convention.
pub fn phoenix_router_at(path: &str, registry: ChannelRegistry) -> Router {
    let state = PhoenixState::new(registry);
    let ws_path = format!("{}/websocket", path.trim_end_matches('/'));

    Router::new()
        .route(&ws_path, get(ws_handler))
        .with_state(state)
}

/// Get the Phoenix state for external access
pub fn phoenix_state(registry: ChannelRegistry) -> PhoenixState {
    PhoenixState::new(registry)
}

/// WebSocket upgrade handler
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<PhoenixState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handle a single WebSocket connection
async fn handle_socket(ws: WebSocket, state: PhoenixState) {
    let socket_id = uuid::Uuid::new_v4().to_string();
    info!(socket_id = %socket_id, "New WebSocket connection");

    // Split the WebSocket
    let (mut ws_sender, mut ws_receiver) = ws.split();

    // Create channels for communication
    let (tx, mut rx) = mpsc::unbounded_channel::<PhxMessage>();
    let (broadcast_tx, mut broadcast_rx) = mpsc::unbounded_channel::<(String, PhxMessage)>();

    // Register this socket
    state.sockets.insert(socket_id.clone(), tx.clone());

    // Create socket handler
    let mut socket = Socket::new(
        socket_id.clone(),
        tx,
        broadcast_tx,
        Arc::clone(&state.registry),
        Arc::clone(&state.sockets),
        Arc::clone(&state.topic_subscribers),
    );

    // Task for sending messages to the client
    let send_socket_id = socket_id.clone();
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match serializer::encode(&msg) {
                Ok(data) => {
                    if ws_sender.send(Message::Text(data.into())).await.is_err() {
                        debug!(socket_id = %send_socket_id, "Failed to send message, connection closed");
                        break;
                    }
                }
                Err(e) => {
                    error!(socket_id = %send_socket_id, error = %e, "Failed to encode message");
                }
            }
        }
    });

    // Task for handling broadcasts
    let broadcast_sockets = Arc::clone(&state.sockets);
    let broadcast_subscribers = Arc::clone(&state.topic_subscribers);
    let broadcast_socket_id = socket_id.clone();
    let broadcast_task = tokio::spawn(async move {
        while let Some((topic, msg)) = broadcast_rx.recv().await {
            // Check if this is a broadcast_from (exclude sender)
            let exclude_join_ref = if let Some(ref jr) = msg.join_ref {
                if jr.starts_with("__exclude:") {
                    Some(jr.trim_start_matches("__exclude:").to_string())
                } else {
                    None
                }
            } else {
                None
            };

            // Clean up the message for actual broadcast
            let broadcast_msg = if exclude_join_ref.is_some() {
                PhxMessage {
                    join_ref: None,
                    ..msg
                }
            } else {
                msg
            };

            // Send to all subscribers
            if let Some(subscribers) = broadcast_subscribers.get(&topic) {
                for socket_id_ref in subscribers.iter() {
                    let subscriber_id = socket_id_ref.key().clone();

                    // Skip the sender for broadcast_from
                    if exclude_join_ref.is_some() {
                        if subscriber_id == broadcast_socket_id {
                            continue;
                        }
                    }

                    if let Some(sender) = broadcast_sockets.get(&subscriber_id) {
                        let _ = sender.send(broadcast_msg.clone());
                    }
                }
            }
        }
    });

    // Main loop for receiving messages from client
    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => match serializer::decode(&text) {
                Ok(msg) => {
                    if let Err(e) = socket.handle_message(msg).await {
                        warn!(socket_id = %socket_id, error = %e, "Error handling message");
                    }
                }
                Err(e) => {
                    warn!(socket_id = %socket_id, error = %e, "Failed to decode message");
                }
            },
            Ok(Message::Binary(_)) => {
                // Binary messages not supported yet
                warn!(socket_id = %socket_id, "Received binary message, not supported");
            }
            Ok(Message::Ping(_)) => {
                // Pong is handled automatically by axum
                debug!(socket_id = %socket_id, "Received ping");
            }
            Ok(Message::Pong(_)) => {
                debug!(socket_id = %socket_id, "Received pong");
            }
            Ok(Message::Close(_)) => {
                info!(socket_id = %socket_id, "Client closed connection");
                break;
            }
            Err(e) => {
                error!(socket_id = %socket_id, error = %e, "WebSocket error");
                break;
            }
        }
    }

    // Cleanup
    info!(socket_id = %socket_id, "Cleaning up socket");
    socket.cleanup().await;
    state.sockets.remove(&socket_id);

    // Cancel the send and broadcast tasks
    send_task.abort();
    broadcast_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Channel, HandleResult, JoinResult, SocketRef};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    struct TestChannel;

    #[async_trait]
    impl Channel for TestChannel {
        async fn join(&self, topic: &str, _payload: Value, _socket: &mut SocketRef) -> JoinResult {
            JoinResult::ok(json!({"joined": topic}))
        }

        async fn handle_in(
            &self,
            event: &str,
            payload: Value,
            socket: &mut SocketRef,
        ) -> HandleResult {
            match event {
                "ping" => HandleResult::ok(json!({"pong": true})),
                "echo" => HandleResult::ok(payload),
                "broadcast" => {
                    socket.broadcast("message", payload.clone());
                    HandleResult::ok(json!({}))
                }
                _ => HandleResult::no_reply(),
            }
        }
    }

    #[test]
    fn test_phoenix_state_creation() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestChannel);

        let state = PhoenixState::new(registry);
        assert_eq!(state.socket_count(), 0);
        assert_eq!(state.topic_subscriber_count("room:lobby"), 0);
    }

    #[test]
    fn test_phoenix_router_creation() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestChannel);

        // This just tests that the router can be created
        let _router = phoenix_router(registry);
    }

    #[test]
    fn test_phoenix_router_at_custom_path() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestChannel);

        let _router = phoenix_router_at("/ws", registry);
    }

    // Integration tests would require a running server
    // Those are better done in a separate integration test file
}
