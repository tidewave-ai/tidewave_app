use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::message::PhxMessage;

/// Result of a channel join operation
#[derive(Debug, Clone)]
pub enum JoinResult {
    /// Join succeeded with response payload
    Ok(Value),
    /// Join failed with error payload
    Error(Value),
}

impl JoinResult {
    pub fn ok(response: Value) -> Self {
        JoinResult::Ok(response)
    }

    pub fn error(reason: Value) -> Self {
        JoinResult::Error(reason)
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, JoinResult::Ok(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self, JoinResult::Error(_))
    }
}

/// Status for reply messages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyStatus {
    Ok,
    Error,
}

impl ReplyStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReplyStatus::Ok => "ok",
            ReplyStatus::Error => "error",
        }
    }
}

/// Result of handling an incoming message
#[derive(Debug, Clone)]
pub enum HandleResult {
    /// Send a reply to the client
    Reply {
        status: ReplyStatus,
        response: Value,
    },
    /// No reply needed
    NoReply,
    /// Stop the channel (will send phx_close)
    Stop {
        reason: String,
    },
}

impl HandleResult {
    pub fn ok(response: Value) -> Self {
        HandleResult::Reply {
            status: ReplyStatus::Ok,
            response,
        }
    }

    pub fn error(response: Value) -> Self {
        HandleResult::Reply {
            status: ReplyStatus::Error,
            response,
        }
    }

    pub fn no_reply() -> Self {
        HandleResult::NoReply
    }

    pub fn stop(reason: impl Into<String>) -> Self {
        HandleResult::Stop {
            reason: reason.into(),
        }
    }
}

/// Reference to a socket connection for sending messages
#[derive(Clone)]
pub struct SocketRef {
    /// The topic this socket is subscribed to
    pub topic: String,
    /// The join_ref for this subscription
    pub join_ref: String,
    /// Channel for sending messages to the client
    sender: mpsc::UnboundedSender<PhxMessage>,
    /// Channel for broadcasting to all subscribers of a topic
    broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
}

impl SocketRef {
    pub fn new(
        topic: String,
        join_ref: String,
        sender: mpsc::UnboundedSender<PhxMessage>,
        broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
    ) -> Self {
        Self {
            topic,
            join_ref,
            sender,
            broadcast_sender,
        }
    }

    /// Send a message directly to this client
    pub fn push(&self, event: &str, payload: Value) {
        let msg = PhxMessage::new(self.topic.clone(), event, payload)
            .with_join_ref(self.join_ref.clone());
        let _ = self.sender.send(msg);
    }

    /// Broadcast a message to all clients subscribed to the same topic
    pub fn broadcast(&self, event: &str, payload: Value) {
        let msg = PhxMessage::new(self.topic.clone(), event, payload);
        let _ = self.broadcast_sender.send((self.topic.clone(), msg));
    }

    /// Broadcast a message to all clients subscribed to the same topic, except this one
    pub fn broadcast_from(&self, event: &str, payload: Value) {
        // For broadcast_from, we need to include the sender's join_ref so the socket
        // can filter it out. We'll use a special marker in the message.
        let mut msg = PhxMessage::new(self.topic.clone(), event, payload);
        // Store the sender's join_ref so we can exclude them
        msg.join_ref = Some(format!("__exclude:{}", self.join_ref));
        let _ = self.broadcast_sender.send((self.topic.clone(), msg));
    }

    /// Get a reference to the underlying sender for testing
    #[cfg(test)]
    pub fn sender(&self) -> &mpsc::UnboundedSender<PhxMessage> {
        &self.sender
    }
}

impl std::fmt::Debug for SocketRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketRef")
            .field("topic", &self.topic)
            .field("join_ref", &self.join_ref)
            .finish()
    }
}

/// Trait for implementing Phoenix channel handlers
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Called when a client attempts to join the channel.
    ///
    /// # Arguments
    /// * `topic` - The full topic string (e.g., "room:lobby")
    /// * `payload` - The join payload from the client
    /// * `socket` - Reference to the socket for sending messages
    ///
    /// # Returns
    /// * `JoinResult::Ok(response)` - Allow the join with the given response
    /// * `JoinResult::Error(reason)` - Reject the join with the given error
    async fn join(&self, topic: &str, payload: Value, socket: &SocketRef) -> JoinResult;

    /// Called when a client sends a message to the channel.
    ///
    /// # Arguments
    /// * `event` - The event name
    /// * `payload` - The message payload
    /// * `socket` - Reference to the socket for sending messages
    ///
    /// # Returns
    /// * `HandleResult::Reply { status, response }` - Send a reply
    /// * `HandleResult::NoReply` - No reply needed
    /// * `HandleResult::Stop { reason }` - Close the channel
    async fn handle_in(&self, event: &str, payload: Value, socket: &SocketRef) -> HandleResult;

    /// Called when a client leaves the channel or disconnects.
    ///
    /// # Arguments
    /// * `reason` - The reason for termination
    async fn terminate(&self, _reason: &str) {
        // Default implementation does nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_join_result_ok() {
        let result = JoinResult::ok(json!({"status": "joined"}));
        assert!(result.is_ok());
        assert!(!result.is_error());

        if let JoinResult::Ok(payload) = result {
            assert_eq!(payload["status"], "joined");
        } else {
            panic!("Expected Ok variant");
        }
    }

    #[test]
    fn test_join_result_error() {
        let result = JoinResult::error(json!({"reason": "unauthorized"}));
        assert!(result.is_error());
        assert!(!result.is_ok());

        if let JoinResult::Error(payload) = result {
            assert_eq!(payload["reason"], "unauthorized");
        } else {
            panic!("Expected Error variant");
        }
    }

    #[test]
    fn test_reply_status() {
        assert_eq!(ReplyStatus::Ok.as_str(), "ok");
        assert_eq!(ReplyStatus::Error.as_str(), "error");
    }

    #[test]
    fn test_handle_result_ok() {
        let result = HandleResult::ok(json!({"data": "test"}));
        if let HandleResult::Reply { status, response } = result {
            assert_eq!(status, ReplyStatus::Ok);
            assert_eq!(response["data"], "test");
        } else {
            panic!("Expected Reply variant");
        }
    }

    #[test]
    fn test_handle_result_error() {
        let result = HandleResult::error(json!({"error": "failed"}));
        if let HandleResult::Reply { status, response } = result {
            assert_eq!(status, ReplyStatus::Error);
            assert_eq!(response["error"], "failed");
        } else {
            panic!("Expected Reply variant");
        }
    }

    #[test]
    fn test_handle_result_no_reply() {
        let result = HandleResult::no_reply();
        assert!(matches!(result, HandleResult::NoReply));
    }

    #[test]
    fn test_handle_result_stop() {
        let result = HandleResult::stop("test reason");
        if let HandleResult::Stop { reason } = result {
            assert_eq!(reason, "test reason");
        } else {
            panic!("Expected Stop variant");
        }
    }

    #[test]
    fn test_socket_ref_push() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (broadcast_sender, _broadcast_receiver) = mpsc::unbounded_channel();

        let socket_ref = SocketRef::new(
            "room:test".to_string(),
            "join1".to_string(),
            sender,
            broadcast_sender,
        );

        socket_ref.push("test_event", json!({"message": "hello"}));

        let msg = receiver.try_recv().unwrap();
        assert_eq!(msg.topic, "room:test");
        assert_eq!(msg.event, "test_event");
        assert_eq!(msg.join_ref, Some("join1".to_string()));
        assert_eq!(msg.payload["message"], "hello");
    }

    #[test]
    fn test_socket_ref_broadcast() {
        let (sender, _receiver) = mpsc::unbounded_channel();
        let (broadcast_sender, mut broadcast_receiver) = mpsc::unbounded_channel();

        let socket_ref = SocketRef::new(
            "room:test".to_string(),
            "join1".to_string(),
            sender,
            broadcast_sender,
        );

        socket_ref.broadcast("broadcast_event", json!({"data": "broadcast"}));

        let (topic, msg) = broadcast_receiver.try_recv().unwrap();
        assert_eq!(topic, "room:test");
        assert_eq!(msg.event, "broadcast_event");
        assert_eq!(msg.payload["data"], "broadcast");
    }

    struct TestChannel;

    #[async_trait]
    impl Channel for TestChannel {
        async fn join(&self, topic: &str, _payload: Value, _socket: &SocketRef) -> JoinResult {
            JoinResult::ok(json!({"joined": topic}))
        }

        async fn handle_in(
            &self,
            event: &str,
            payload: Value,
            _socket: &SocketRef,
        ) -> HandleResult {
            match event {
                "ping" => HandleResult::ok(json!({"pong": true})),
                "echo" => HandleResult::ok(payload),
                _ => HandleResult::no_reply(),
            }
        }
    }

    #[tokio::test]
    async fn test_channel_trait_join() {
        let channel = TestChannel;
        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();
        let socket = SocketRef::new(
            "room:lobby".to_string(),
            "1".to_string(),
            sender,
            broadcast_sender,
        );

        let result = channel.join("room:lobby", json!({}), &socket).await;
        assert!(result.is_ok());
        if let JoinResult::Ok(payload) = result {
            assert_eq!(payload["joined"], "room:lobby");
        }
    }

    #[tokio::test]
    async fn test_channel_trait_handle_in() {
        let channel = TestChannel;
        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();
        let socket = SocketRef::new(
            "room:lobby".to_string(),
            "1".to_string(),
            sender,
            broadcast_sender,
        );

        let result = channel.handle_in("ping", json!({}), &socket).await;
        if let HandleResult::Reply { status, response } = result {
            assert_eq!(status, ReplyStatus::Ok);
            assert_eq!(response["pong"], true);
        } else {
            panic!("Expected Reply");
        }
    }
}
