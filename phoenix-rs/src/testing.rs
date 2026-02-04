//! Test utilities for Phoenix channels.
//!
//! Provides a high-level API for testing channel implementations without
//! starting a real HTTP/WebSocket server.
//!
//! # Example
//!
//! ```ignore
//! use phoenix_rs::testing::TestClient;
//! use phoenix_rs::ChannelRegistry;
//! use serde_json::json;
//!
//! let mut registry = ChannelRegistry::new();
//! registry.register("room:*", MyRoomChannel);
//!
//! let mut client = TestClient::new(registry);
//! let mut channel = client.join("room:lobby", json!({})).await.unwrap();
//!
//! let response = channel.call("echo", json!({"msg": "hello"})).await;
//! assert_eq!(response.payload["response"]["msg"], "hello");
//!
//! channel.leave().await;
//! ```

use crate::message::{PhxMessage, events};
use crate::registry::ChannelRegistry;
use crate::socket::Socket;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Test client simulating a WebSocket connection to a Phoenix server.
///
/// Use this to test channel implementations without running an HTTP server.
pub struct TestClient {
    socket: Socket,
    receiver: mpsc::UnboundedReceiver<PhxMessage>,
    ref_counter: AtomicU64,
}

impl TestClient {
    /// Create a new test client with the given channel registry.
    pub fn new(registry: ChannelRegistry) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (broadcast_sender, _broadcast_receiver) = mpsc::unbounded_channel();
        let all_sockets = Arc::new(DashMap::new());
        let topic_subscribers = Arc::new(DashMap::new());
        let socket_id = format!("test-socket-{}", uuid::Uuid::new_v4());

        all_sockets.insert(socket_id.clone(), sender.clone());

        let socket = Socket::new(
            socket_id,
            sender,
            broadcast_sender,
            Arc::new(registry),
            all_sockets,
            topic_subscribers,
        );

        Self {
            socket,
            receiver,
            ref_counter: AtomicU64::new(1),
        }
    }

    /// Join a channel, returns a handle for interacting with it.
    ///
    /// # Arguments
    /// * `topic` - The topic to join (e.g., "room:lobby", "acp:test")
    /// * `payload` - The join payload
    ///
    /// # Returns
    /// * `Ok(JoinedChannel)` if join succeeded
    /// * `Err(PhxMessage)` if join failed (contains the error reply)
    pub async fn join(
        &mut self,
        topic: &str,
        payload: Value,
    ) -> Result<JoinedChannel<'_>, PhxMessage> {
        let ref_id = self.next_ref();
        self.join_with_ref(topic, payload, &ref_id).await
    }

    /// Join a channel with explicit join_ref (for testing reconnection scenarios).
    pub async fn join_with_ref(
        &mut self,
        topic: &str,
        payload: Value,
        join_ref: &str,
    ) -> Result<JoinedChannel<'_>, PhxMessage> {
        let join_msg = PhxMessage::new(topic, events::PHX_JOIN, payload)
            .with_ref(join_ref)
            .with_join_ref(join_ref);

        self.socket
            .handle_message(join_msg)
            .await
            .expect("Failed to send join message");

        // Wait for join reply
        let reply = self
            .receiver
            .recv()
            .await
            .expect("Channel closed while waiting for join reply");

        if reply.payload.get("status").and_then(|s| s.as_str()) == Some("ok") {
            Ok(JoinedChannel {
                client: self,
                topic: topic.to_string(),
                join_ref: join_ref.to_string(),
            })
        } else {
            Err(reply)
        }
    }

    /// Receive the next message from the server (with default timeout).
    ///
    /// Returns `None` if the channel is closed or timeout expires.
    pub async fn recv(&mut self) -> Option<PhxMessage> {
        self.recv_timeout(Duration::from_secs(5)).await
    }

    /// Receive the next message with custom timeout.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> Option<PhxMessage> {
        tokio::time::timeout(timeout, self.receiver.recv())
            .await
            .ok()
            .flatten()
    }

    /// Clean up the socket (call when done testing).
    pub async fn cleanup(&mut self) {
        self.socket.cleanup().await;
    }

    fn next_ref(&self) -> String {
        self.ref_counter.fetch_add(1, Ordering::SeqCst).to_string()
    }
}

/// Handle for an active channel subscription.
pub struct JoinedChannel<'a> {
    client: &'a mut TestClient,
    topic: String,
    join_ref: String,
}

impl<'a> JoinedChannel<'a> {
    /// Push an event without waiting for reply.
    pub async fn push(&mut self, event: &str, payload: Value) {
        let ref_id = self.client.next_ref();
        let msg = PhxMessage::new(&self.topic, event, payload)
            .with_ref(&ref_id)
            .with_join_ref(&self.join_ref);

        self.client
            .socket
            .handle_message(msg)
            .await
            .expect("Failed to send message");
    }

    /// Push an event and wait for reply (matches by ref).
    ///
    /// # Returns
    /// The reply message from the server.
    pub async fn call(&mut self, event: &str, payload: Value) -> PhxMessage {
        self.call_timeout(event, payload, Duration::from_secs(5))
            .await
    }

    /// Push an event and wait for reply with custom timeout.
    pub async fn call_timeout(
        &mut self,
        event: &str,
        payload: Value,
        timeout: Duration,
    ) -> PhxMessage {
        let ref_id = self.client.next_ref();
        let msg = PhxMessage::new(&self.topic, event, payload)
            .with_ref(&ref_id)
            .with_join_ref(&self.join_ref);

        self.client
            .socket
            .handle_message(msg)
            .await
            .expect("Failed to send message");

        // Wait for reply matching our ref
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("Timeout waiting for reply to ref {}", ref_id);
            }

            match tokio::time::timeout(remaining, self.client.receiver.recv()).await {
                Ok(Some(reply)) => {
                    if reply.ref_.as_deref() == Some(&ref_id) {
                        return reply;
                    }
                    // Not our reply, could be a server push - for now just continue
                    // In a more complete implementation, we'd queue these
                }
                Ok(None) => panic!("Channel closed while waiting for reply"),
                Err(_) => panic!("Timeout waiting for reply to ref {}", ref_id),
            }
        }
    }

    /// Receive next server-pushed message (with default timeout).
    ///
    /// Use this to receive notifications or broadcasts from the server.
    pub async fn recv(&mut self) -> Option<PhxMessage> {
        self.recv_timeout(Duration::from_secs(5)).await
    }

    /// Receive with custom timeout.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> Option<PhxMessage> {
        self.client.recv_timeout(timeout).await
    }

    /// Leave the channel.
    pub async fn leave(self) -> PhxMessage {
        let ref_id = self.client.next_ref();
        let msg = PhxMessage::new(&self.topic, events::PHX_LEAVE, serde_json::json!({}))
            .with_ref(&ref_id)
            .with_join_ref(&self.join_ref);

        self.client
            .socket
            .handle_message(msg)
            .await
            .expect("Failed to send leave message");

        // Wait for leave reply
        self.client
            .receiver
            .recv()
            .await
            .expect("Channel closed while waiting for leave reply")
    }

    /// Get the topic.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Get the join_ref.
    pub fn join_ref(&self) -> &str {
        &self.join_ref
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Channel, HandleResult, JoinResult, SocketRef};
    use async_trait::async_trait;
    use serde_json::json;
    use std::any::Any;

    struct EchoChannel;

    #[async_trait]
    impl Channel for EchoChannel {
        async fn join(&self, topic: &str, _payload: Value, _socket: &mut SocketRef) -> JoinResult {
            JoinResult::ok(json!({"joined": topic}))
        }

        async fn handle_in(
            &self,
            event: &str,
            payload: Value,
            _socket: &mut SocketRef,
        ) -> HandleResult {
            match event {
                "echo" => HandleResult::ok(payload),
                "error" => HandleResult::error(json!({"reason": "test error"})),
                _ => HandleResult::no_reply(),
            }
        }

        async fn handle_info(&self, _message: Box<dyn Any + Send>, _socket: &mut SocketRef) {}

        async fn terminate(&self, _reason: &str, _socket: &mut SocketRef) {}
    }

    struct RejectChannel;

    #[async_trait]
    impl Channel for RejectChannel {
        async fn join(&self, _topic: &str, _payload: Value, _socket: &mut SocketRef) -> JoinResult {
            JoinResult::error(json!({"reason": "unauthorized"}))
        }

        async fn handle_in(
            &self,
            _event: &str,
            _payload: Value,
            _socket: &mut SocketRef,
        ) -> HandleResult {
            HandleResult::no_reply()
        }
    }

    #[tokio::test]
    async fn test_client_join_success() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);

        let mut client = TestClient::new(registry);
        let channel = client.join("room:lobby", json!({})).await;

        assert!(channel.is_ok());
        let channel = channel.unwrap();
        assert_eq!(channel.topic(), "room:lobby");
    }

    #[tokio::test]
    async fn test_client_join_error() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", RejectChannel);

        let mut client = TestClient::new(registry);
        let result = client.join("room:lobby", json!({})).await;

        assert!(result.is_err());
        let error = result.err().unwrap();
        assert_eq!(error.payload["status"], "error");
        assert_eq!(error.payload["response"]["reason"], "unauthorized");
    }

    #[tokio::test]
    async fn test_client_call() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);

        let mut client = TestClient::new(registry);
        let mut channel = client.join("room:lobby", json!({})).await.unwrap();

        let response = channel.call("echo", json!({"message": "hello"})).await;
        assert_eq!(response.payload["status"], "ok");
        assert_eq!(response.payload["response"]["message"], "hello");
    }

    #[tokio::test]
    async fn test_client_call_error() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);

        let mut client = TestClient::new(registry);
        let mut channel = client.join("room:lobby", json!({})).await.unwrap();

        let response = channel.call("error", json!({})).await;
        assert_eq!(response.payload["status"], "error");
        assert_eq!(response.payload["response"]["reason"], "test error");
    }

    #[tokio::test]
    async fn test_client_leave() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);

        let mut client = TestClient::new(registry);
        let channel = client.join("room:lobby", json!({})).await.unwrap();

        let reply = channel.leave().await;
        assert_eq!(reply.payload["status"], "ok");
    }
}
