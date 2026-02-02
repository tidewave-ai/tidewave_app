use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::message::PhxMessage;

/// Per-subscription state storage using type-erased values.
///
/// Assigns allow channels to store arbitrary typed data that persists
/// across the lifetime of a subscription (from join to leave/disconnect).
#[derive(Default)]
pub struct Assigns {
    data: HashMap<String, Box<dyn Any + Send + Sync>>,
}

impl Assigns {
    /// Create a new empty Assigns container
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Insert a value into assigns
    pub fn insert<T: Any + Send + Sync + 'static>(&mut self, key: impl Into<String>, value: T) {
        self.data.insert(key.into(), Box::new(value));
    }

    /// Get an immutable reference to a value
    pub fn get<T: Any + Send + Sync + 'static>(&self, key: &str) -> Option<&T> {
        self.data.get(key).and_then(|v| v.downcast_ref::<T>())
    }

    /// Get a mutable reference to a value
    pub fn get_mut<T: Any + Send + Sync + 'static>(&mut self, key: &str) -> Option<&mut T> {
        self.data.get_mut(key).and_then(|v| v.downcast_mut::<T>())
    }

    /// Remove a value from assigns
    pub fn remove(&mut self, key: &str) -> bool {
        self.data.remove(key).is_some()
    }

    /// Check if a key exists
    pub fn contains(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// Get the number of assigned values
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if assigns is empty
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl std::fmt::Debug for Assigns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assigns")
            .field("keys", &self.data.keys().collect::<Vec<_>>())
            .finish()
    }
}

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

/// Reference to a socket connection for sending messages and managing state.
///
/// Each subscription (socket + topic combination) has its own `SocketRef` with
/// independent assigns that persist across the subscription's lifetime.
pub struct SocketRef {
    /// The topic this socket is subscribed to
    pub topic: String,
    /// The join_ref for this subscription
    pub join_ref: String,
    /// Channel for sending messages to the client
    sender: mpsc::UnboundedSender<PhxMessage>,
    /// Channel for broadcasting to all subscribers of a topic
    broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
    /// Per-subscription state
    assigns: Assigns,
}

impl SocketRef {
    /// Create a new SocketRef
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
            assigns: Assigns::new(),
        }
    }

    /// Create a new SocketRef with existing assigns (used internally)
    pub(crate) fn with_assigns(
        topic: String,
        join_ref: String,
        sender: mpsc::UnboundedSender<PhxMessage>,
        broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
        assigns: Assigns,
    ) -> Self {
        Self {
            topic,
            join_ref,
            sender,
            broadcast_sender,
            assigns,
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
        let mut msg = PhxMessage::new(self.topic.clone(), event, payload);
        msg.join_ref = Some(format!("__exclude:{}", self.join_ref));
        let _ = self.broadcast_sender.send((self.topic.clone(), msg));
    }

    // ==================== Assigns API ====================

    /// Assign a value to this subscription's state
    ///
    /// # Example
    /// ```ignore
    /// socket.assign("user_id", "alice".to_string());
    /// socket.assign("message_count", 0u32);
    /// ```
    pub fn assign<T: Any + Send + Sync + 'static>(&mut self, key: &str, value: T) {
        self.assigns.insert(key, value);
    }

    /// Get an immutable reference to an assigned value
    ///
    /// # Example
    /// ```ignore
    /// let user_id: &String = socket.get_assign("user_id").unwrap();
    /// ```
    pub fn get_assign<T: Any + Send + Sync + 'static>(&self, key: &str) -> Option<&T> {
        self.assigns.get(key)
    }

    /// Get a mutable reference to an assigned value
    ///
    /// # Example
    /// ```ignore
    /// let count: &mut u32 = socket.get_assign_mut("message_count").unwrap();
    /// *count += 1;
    /// ```
    pub fn get_assign_mut<T: Any + Send + Sync + 'static>(&mut self, key: &str) -> Option<&mut T> {
        self.assigns.get_mut(key)
    }

    /// Remove an assigned value
    pub fn remove_assign(&mut self, key: &str) -> bool {
        self.assigns.remove(key)
    }

    /// Check if an assign key exists
    pub fn has_assign(&self, key: &str) -> bool {
        self.assigns.contains(key)
    }

    /// Take ownership of the assigns (used internally for persistence)
    pub(crate) fn take_assigns(&mut self) -> Assigns {
        std::mem::take(&mut self.assigns)
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
            .field("assigns", &self.assigns)
            .finish()
    }
}

/// Trait for implementing Phoenix channel handlers
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Called when a client attempts to join the channel.
    ///
    /// Use `socket.assign()` to store per-subscription state that will be
    /// available in subsequent `handle_in` and `terminate` calls.
    ///
    /// # Arguments
    /// * `topic` - The full topic string (e.g., "room:lobby")
    /// * `payload` - The join payload from the client
    /// * `socket` - Mutable reference to the socket for sending messages and storing state
    ///
    /// # Returns
    /// * `JoinResult::Ok(response)` - Allow the join with the given response
    /// * `JoinResult::Error(reason)` - Reject the join with the given error
    async fn join(&self, topic: &str, payload: Value, socket: &mut SocketRef) -> JoinResult;

    /// Called when a client sends a message to the channel.
    ///
    /// Use `socket.get_assign()` to retrieve state stored during join.
    ///
    /// # Arguments
    /// * `event` - The event name
    /// * `payload` - The message payload
    /// * `socket` - Mutable reference to the socket for sending messages and accessing state
    ///
    /// # Returns
    /// * `HandleResult::Reply { status, response }` - Send a reply
    /// * `HandleResult::NoReply` - No reply needed
    /// * `HandleResult::Stop { reason }` - Close the channel
    async fn handle_in(&self, event: &str, payload: Value, socket: &mut SocketRef) -> HandleResult;

    /// Called when a client leaves the channel or disconnects.
    ///
    /// Assigns are still accessible for cleanup purposes.
    ///
    /// # Arguments
    /// * `reason` - The reason for termination
    /// * `socket` - Mutable reference to the socket for accessing state
    async fn terminate(&self, _reason: &str, _socket: &mut SocketRef) {
        // Default implementation does nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_assigns_basic() {
        let mut assigns = Assigns::new();
        assert!(assigns.is_empty());

        assigns.insert("name", "alice".to_string());
        assigns.insert("count", 42u32);

        assert_eq!(assigns.len(), 2);
        assert!(assigns.contains("name"));
        assert!(!assigns.contains("missing"));

        assert_eq!(assigns.get::<String>("name"), Some(&"alice".to_string()));
        assert_eq!(assigns.get::<u32>("count"), Some(&42u32));

        // Wrong type returns None
        assert_eq!(assigns.get::<u32>("name"), None);
    }

    #[test]
    fn test_assigns_mutable() {
        let mut assigns = Assigns::new();
        assigns.insert("count", 0u32);

        if let Some(count) = assigns.get_mut::<u32>("count") {
            *count += 1;
        }

        assert_eq!(assigns.get::<u32>("count"), Some(&1u32));
    }

    #[test]
    fn test_assigns_remove() {
        let mut assigns = Assigns::new();
        assigns.insert("key", "value".to_string());

        assert!(assigns.remove("key"));
        assert!(!assigns.remove("key")); // Already removed
        assert!(!assigns.contains("key"));
    }

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

    #[test]
    fn test_socket_ref_assigns() {
        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();

        let mut socket = SocketRef::new(
            "room:test".to_string(),
            "join1".to_string(),
            sender,
            broadcast_sender,
        );

        // Assign values
        socket.assign("user_id", "alice".to_string());
        socket.assign("count", 0u32);

        // Get immutable
        assert_eq!(
            socket.get_assign::<String>("user_id"),
            Some(&"alice".to_string())
        );

        // Get mutable and modify
        if let Some(count) = socket.get_assign_mut::<u32>("count") {
            *count += 1;
        }
        assert_eq!(socket.get_assign::<u32>("count"), Some(&1u32));

        // Check existence
        assert!(socket.has_assign("user_id"));
        assert!(!socket.has_assign("missing"));

        // Remove
        assert!(socket.remove_assign("user_id"));
        assert!(!socket.has_assign("user_id"));
    }

    struct TestChannel;

    #[async_trait]
    impl Channel for TestChannel {
        async fn join(&self, topic: &str, _payload: Value, socket: &mut SocketRef) -> JoinResult {
            socket.assign("topic", topic.to_string());
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
                "get_topic" => {
                    let topic = socket.get_assign::<String>("topic").unwrap();
                    HandleResult::ok(json!({"topic": topic}))
                }
                _ => HandleResult::no_reply(),
            }
        }
    }

    #[tokio::test]
    async fn test_channel_trait_join() {
        let channel = TestChannel;
        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();
        let mut socket = SocketRef::new(
            "room:lobby".to_string(),
            "1".to_string(),
            sender,
            broadcast_sender,
        );

        let result = channel.join("room:lobby", json!({}), &mut socket).await;
        assert!(result.is_ok());
        if let JoinResult::Ok(payload) = result {
            assert_eq!(payload["joined"], "room:lobby");
        }

        // Verify assign was set
        assert_eq!(
            socket.get_assign::<String>("topic"),
            Some(&"room:lobby".to_string())
        );
    }

    #[tokio::test]
    async fn test_channel_trait_handle_in() {
        let channel = TestChannel;
        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();
        let mut socket = SocketRef::new(
            "room:lobby".to_string(),
            "1".to_string(),
            sender,
            broadcast_sender,
        );

        // First join to set up assigns
        let _ = channel.join("room:lobby", json!({}), &mut socket).await;

        // Then handle_in can access assigns
        let result = channel.handle_in("get_topic", json!({}), &mut socket).await;
        if let HandleResult::Reply { status, response } = result {
            assert_eq!(status, ReplyStatus::Ok);
            assert_eq!(response["topic"], "room:lobby");
        } else {
            panic!("Expected Reply");
        }
    }
}
