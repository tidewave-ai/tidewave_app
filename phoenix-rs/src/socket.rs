use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::channel::{Assigns, HandleResult, JoinResult, SocketRef};
use crate::error::PhoenixError;
use crate::message::{events, PhxMessage};
use crate::registry::ChannelRegistry;

/// Per-subscription data including join_ref and assigns
struct Subscription {
    join_ref: String,
    assigns: Assigns,
}

/// Represents a single WebSocket connection
pub struct Socket {
    /// Unique identifier for this socket
    id: String,
    /// Channel for sending messages to the client
    sender: mpsc::UnboundedSender<PhxMessage>,
    /// Receiver for broadcast messages
    broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
    /// Active channel subscriptions: topic -> subscription data
    subscriptions: HashMap<String, Subscription>,
    /// Reference to the channel registry
    registry: Arc<ChannelRegistry>,
    /// Reference to all sockets for broadcast
    all_sockets: Arc<DashMap<String, mpsc::UnboundedSender<PhxMessage>>>,
    /// Topic subscriptions across all sockets: topic -> set of socket_ids
    topic_subscribers: Arc<DashMap<String, dashmap::DashSet<String>>>,
}

impl Socket {
    /// Create a new socket
    pub fn new(
        id: String,
        sender: mpsc::UnboundedSender<PhxMessage>,
        broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
        registry: Arc<ChannelRegistry>,
        all_sockets: Arc<DashMap<String, mpsc::UnboundedSender<PhxMessage>>>,
        topic_subscribers: Arc<DashMap<String, dashmap::DashSet<String>>>,
    ) -> Self {
        Self {
            id,
            sender,
            broadcast_sender,
            subscriptions: HashMap::new(),
            registry,
            all_sockets,
            topic_subscribers,
        }
    }

    /// Get the socket ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Handle an incoming message from the client
    pub async fn handle_message(&mut self, msg: PhxMessage) -> Result<(), PhoenixError> {
        debug!(
            socket_id = %self.id,
            topic = %msg.topic,
            event = %msg.event,
            "Handling message"
        );

        // Handle heartbeat
        if msg.topic == "phoenix" && msg.event == events::HEARTBEAT {
            return self.handle_heartbeat(&msg);
        }

        // Handle channel events
        match msg.event.as_str() {
            events::PHX_JOIN => self.handle_join(msg).await,
            events::PHX_LEAVE => self.handle_leave(msg).await,
            _ => self.handle_channel_message(msg).await,
        }
    }

    /// Handle heartbeat messages
    fn handle_heartbeat(&self, msg: &PhxMessage) -> Result<(), PhoenixError> {
        let reply = PhxMessage::heartbeat_reply(msg);
        self.send(reply)
    }

    /// Handle channel join requests
    async fn handle_join(&mut self, msg: PhxMessage) -> Result<(), PhoenixError> {
        let topic = &msg.topic;
        let join_ref = msg
            .ref_
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // Check if already joined
        if self.subscriptions.contains_key(topic) {
            let reply = PhxMessage::reply(
                &msg,
                "error",
                serde_json::json!({"reason": "already joined"}),
            );
            return self.send(reply);
        }

        // Find channel handler
        let channel = match self.registry.find(topic) {
            Some(ch) => ch,
            None => {
                let reply = PhxMessage::reply(
                    &msg,
                    "error",
                    serde_json::json!({"reason": "no handler for topic"}),
                );
                return self.send(reply);
            }
        };

        // Create socket reference for the channel (with fresh assigns)
        let mut socket_ref = SocketRef::new(
            topic.clone(),
            join_ref.clone(),
            self.sender.clone(),
            self.broadcast_sender.clone(),
        );

        // Call channel's join handler
        let result = channel
            .join(topic, msg.payload.clone(), &mut socket_ref)
            .await;

        match result {
            JoinResult::Ok(response) => {
                // Take the assigns from socket_ref and store them
                let assigns = socket_ref.take_assigns();

                // Add subscription with assigns
                self.subscriptions.insert(
                    topic.clone(),
                    Subscription {
                        join_ref: join_ref.clone(),
                        assigns,
                    },
                );

                // Add to topic subscribers
                self.topic_subscribers
                    .entry(topic.clone())
                    .or_insert_with(dashmap::DashSet::new)
                    .insert(self.id.clone());

                // Send success reply
                let reply = PhxMessage {
                    join_ref: Some(join_ref),
                    ref_: msg.ref_.clone(),
                    topic: topic.clone(),
                    event: events::PHX_REPLY.to_string(),
                    payload: serde_json::json!({
                        "status": "ok",
                        "response": response,
                    }),
                };
                self.send(reply)
            }
            JoinResult::Error(reason) => {
                let reply = PhxMessage::reply(&msg, "error", reason);
                self.send(reply)
            }
        }
    }

    /// Handle channel leave requests
    async fn handle_leave(&mut self, msg: PhxMessage) -> Result<(), PhoenixError> {
        let topic = &msg.topic;

        // Check if joined and remove subscription
        if let Some(subscription) = self.subscriptions.remove(topic) {
            // Remove from topic subscribers
            if let Some(subscribers) = self.topic_subscribers.get(topic) {
                subscribers.remove(&self.id);
            }

            // Find channel handler and call terminate with socket_ref
            if let Some(channel) = self.registry.find(topic) {
                let mut socket_ref = SocketRef::with_assigns(
                    topic.clone(),
                    subscription.join_ref.clone(),
                    self.sender.clone(),
                    self.broadcast_sender.clone(),
                    subscription.assigns,
                );
                channel.terminate("leave", &mut socket_ref).await;
            }

            // Send success reply
            let reply = PhxMessage {
                join_ref: Some(subscription.join_ref),
                ref_: msg.ref_.clone(),
                topic: topic.clone(),
                event: events::PHX_REPLY.to_string(),
                payload: serde_json::json!({
                    "status": "ok",
                    "response": {},
                }),
            };
            self.send(reply)
        } else {
            let reply =
                PhxMessage::reply(&msg, "error", serde_json::json!({"reason": "not joined"}));
            self.send(reply)
        }
    }

    /// Handle regular channel messages
    async fn handle_channel_message(&mut self, msg: PhxMessage) -> Result<(), PhoenixError> {
        let topic = &msg.topic;

        // Check if joined and get subscription
        let subscription = match self.subscriptions.get_mut(topic) {
            Some(sub) => sub,
            None => {
                warn!(topic = %topic, "Received message for unjoined topic");
                return Err(PhoenixError::NotJoined(topic.clone()));
            }
        };

        let join_ref = subscription.join_ref.clone();

        // Find channel handler
        let channel = match self.registry.find(topic) {
            Some(ch) => ch,
            None => {
                return Err(PhoenixError::TopicNotFound(topic.clone()));
            }
        };

        // Create socket reference with the subscription's assigns
        // We need to temporarily take assigns out, use them, then put them back
        let assigns = std::mem::take(&mut subscription.assigns);
        let mut socket_ref = SocketRef::with_assigns(
            topic.clone(),
            join_ref.clone(),
            self.sender.clone(),
            self.broadcast_sender.clone(),
            assigns,
        );

        // Call channel's handle_in
        let result = channel
            .handle_in(&msg.event, msg.payload.clone(), &mut socket_ref)
            .await;

        // Put the (possibly modified) assigns back
        if let Some(sub) = self.subscriptions.get_mut(topic) {
            sub.assigns = socket_ref.take_assigns();
        }

        match result {
            HandleResult::Reply { status, response } => {
                let reply = PhxMessage {
                    join_ref: Some(join_ref),
                    ref_: msg.ref_.clone(),
                    topic: topic.clone(),
                    event: events::PHX_REPLY.to_string(),
                    payload: serde_json::json!({
                        "status": status.as_str(),
                        "response": response,
                    }),
                };
                self.send(reply)
            }
            HandleResult::NoReply => Ok(()),
            HandleResult::Stop { reason } => {
                // Send close message
                let close_msg = PhxMessage {
                    join_ref: Some(join_ref),
                    ref_: None,
                    topic: topic.clone(),
                    event: events::PHX_CLOSE.to_string(),
                    payload: serde_json::json!({"reason": reason}),
                };
                self.send(close_msg)?;

                // Remove subscription
                self.subscriptions.remove(topic);
                if let Some(subscribers) = self.topic_subscribers.get(topic) {
                    subscribers.remove(&self.id);
                }

                Ok(())
            }
        }
    }

    /// Send a message to the client
    fn send(&self, msg: PhxMessage) -> Result<(), PhoenixError> {
        self.sender
            .send(msg)
            .map_err(|_| PhoenixError::ConnectionClosed)
    }

    /// Clean up when socket disconnects
    pub async fn cleanup(&mut self) {
        // Remove from all topic subscriptions and call terminate
        for (topic, subscription) in self.subscriptions.drain() {
            if let Some(subscribers) = self.topic_subscribers.get(&topic) {
                subscribers.remove(&self.id);
            }

            // Call terminate on channel with the socket_ref
            if let Some(channel) = self.registry.find(&topic) {
                let mut socket_ref = SocketRef::with_assigns(
                    topic.clone(),
                    subscription.join_ref,
                    self.sender.clone(),
                    self.broadcast_sender.clone(),
                    subscription.assigns,
                );
                channel.terminate("disconnect", &mut socket_ref).await;
            }
        }

        // Remove from all_sockets
        self.all_sockets.remove(&self.id);
    }

    /// Broadcast a message to all sockets subscribed to a topic
    pub fn broadcast_to_topic(&self, topic: &str, msg: PhxMessage, exclude_join_ref: Option<&str>) {
        if let Some(subscribers) = self.topic_subscribers.get(topic) {
            for socket_id in subscribers.iter() {
                // Skip if this is the sender (for broadcast_from)
                if let Some(exclude) = exclude_join_ref {
                    if let Some(sub) = self.subscriptions.get(topic) {
                        if socket_id.as_str() == self.id && sub.join_ref == exclude {
                            continue;
                        }
                    }
                }

                if let Some(sender) = self.all_sockets.get(socket_id.as_str()) {
                    let _ = sender.send(msg.clone());
                }
            }
        }
    }

    /// Check if this socket is subscribed to a topic
    pub fn is_subscribed(&self, topic: &str) -> bool {
        self.subscriptions.contains_key(topic)
    }

    /// Get the join_ref for a topic subscription
    pub fn get_join_ref(&self, topic: &str) -> Option<&String> {
        self.subscriptions.get(topic).map(|s| &s.join_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;
    use async_trait::async_trait;
    use serde_json::{json, Value};

    struct EchoChannel;

    #[async_trait]
    impl Channel for EchoChannel {
        async fn join(&self, topic: &str, _payload: Value, socket: &mut SocketRef) -> JoinResult {
            // Store some state on join
            socket.assign("topic", topic.to_string());
            socket.assign("message_count", 0u32);
            JoinResult::ok(json!({"joined": topic}))
        }

        async fn handle_in(
            &self,
            event: &str,
            payload: Value,
            socket: &mut SocketRef,
        ) -> HandleResult {
            match event {
                "echo" => HandleResult::ok(payload),
                "broadcast" => {
                    socket.broadcast("broadcasted", payload.clone());
                    HandleResult::ok(json!({}))
                }
                "error" => HandleResult::error(json!({"reason": "test error"})),
                "get_count" => {
                    let count = socket.get_assign::<u32>("message_count").unwrap_or(&0);
                    HandleResult::ok(json!({"count": *count}))
                }
                "increment" => {
                    if let Some(count) = socket.get_assign_mut::<u32>("message_count") {
                        *count += 1;
                        HandleResult::ok(json!({"count": *count}))
                    } else {
                        HandleResult::error(json!({"reason": "no count"}))
                    }
                }
                _ => HandleResult::no_reply(),
            }
        }

        async fn terminate(&self, _reason: &str, socket: &mut SocketRef) {
            // Can access assigns during terminate
            let _topic = socket.get_assign::<String>("topic");
        }
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

    fn create_test_socket(
        registry: ChannelRegistry,
    ) -> (Socket, mpsc::UnboundedReceiver<PhxMessage>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (broadcast_sender, _broadcast_receiver) = mpsc::unbounded_channel();
        let all_sockets = Arc::new(DashMap::new());
        let topic_subscribers = Arc::new(DashMap::new());
        let socket_id = "test-socket-1".to_string();

        all_sockets.insert(socket_id.clone(), sender.clone());

        let socket = Socket::new(
            socket_id,
            sender,
            broadcast_sender,
            Arc::new(registry),
            all_sockets,
            topic_subscribers,
        );

        (socket, receiver)
    }

    #[tokio::test]
    async fn test_heartbeat() {
        let registry = ChannelRegistry::new();
        let (mut socket, mut receiver) = create_test_socket(registry);

        let heartbeat = PhxMessage::new("phoenix", events::HEARTBEAT, json!({})).with_ref("42");

        socket.handle_message(heartbeat).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.topic, "phoenix");
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.ref_, Some("42".to_string()));
        assert_eq!(reply.payload["status"], "ok");
    }

    #[tokio::test]
    async fn test_join_success() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({"user": "alice"}))
            .with_ref("1")
            .with_join_ref("1");

        socket.handle_message(join_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.topic, "room:lobby");
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.payload["status"], "ok");
        assert_eq!(reply.payload["response"]["joined"], "room:lobby");

        assert!(socket.is_subscribed("room:lobby"));
    }

    #[tokio::test]
    async fn test_join_error() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", RejectChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");

        socket.handle_message(join_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["status"], "error");
        assert_eq!(reply.payload["response"]["reason"], "unauthorized");

        assert!(!socket.is_subscribed("room:lobby"));
    }

    #[tokio::test]
    async fn test_join_no_handler() {
        let registry = ChannelRegistry::new();
        let (mut socket, mut receiver) = create_test_socket(registry);

        let join_msg = PhxMessage::new("unknown:topic", events::PHX_JOIN, json!({})).with_ref("1");

        socket.handle_message(join_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["status"], "error");
        assert_eq!(reply.payload["response"]["reason"], "no handler for topic");
    }

    #[tokio::test]
    async fn test_join_already_joined() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // First join
        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join_msg).await.unwrap();
        let _ = receiver.try_recv().unwrap(); // consume first reply

        // Second join attempt
        let join_msg2 = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("2");
        socket.handle_message(join_msg2).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["status"], "error");
        assert_eq!(reply.payload["response"]["reason"], "already joined");
    }

    #[tokio::test]
    async fn test_leave() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // Join first
        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join_msg).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        assert!(socket.is_subscribed("room:lobby"));

        // Leave
        let leave_msg = PhxMessage::new("room:lobby", events::PHX_LEAVE, json!({})).with_ref("2");
        socket.handle_message(leave_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["status"], "ok");

        assert!(!socket.is_subscribed("room:lobby"));
    }

    #[tokio::test]
    async fn test_leave_not_joined() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        let leave_msg = PhxMessage::new("room:lobby", events::PHX_LEAVE, json!({})).with_ref("1");
        socket.handle_message(leave_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["status"], "error");
        assert_eq!(reply.payload["response"]["reason"], "not joined");
    }

    #[tokio::test]
    async fn test_channel_message() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // Join first
        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join_msg).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        // Send echo message
        let echo_msg =
            PhxMessage::new("room:lobby", "echo", json!({"message": "hello"})).with_ref("2");
        socket.handle_message(echo_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.payload["status"], "ok");
        assert_eq!(reply.payload["response"]["message"], "hello");
    }

    #[tokio::test]
    async fn test_channel_message_not_joined() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, _receiver) = create_test_socket(registry);

        // Try to send without joining
        let msg = PhxMessage::new("room:lobby", "echo", json!({})).with_ref("1");
        let result = socket.handle_message(msg).await;

        assert!(result.is_err());
        assert!(matches!(result, Err(PhoenixError::NotJoined(_))));
    }

    #[tokio::test]
    async fn test_channel_error_response() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // Join first
        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join_msg).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        // Send error message
        let error_msg = PhxMessage::new("room:lobby", "error", json!({})).with_ref("2");
        socket.handle_message(error_msg).await.unwrap();

        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["status"], "error");
        assert_eq!(reply.payload["response"]["reason"], "test error");
    }

    #[tokio::test]
    async fn test_cleanup() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // Join
        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join_msg).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        assert!(socket.is_subscribed("room:lobby"));

        // Cleanup
        socket.cleanup().await;

        assert!(!socket.is_subscribed("room:lobby"));
    }

    #[tokio::test]
    async fn test_assigns_persist_across_messages() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // Join (sets message_count to 0)
        let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join_msg).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        // Get initial count
        let get_msg = PhxMessage::new("room:lobby", "get_count", json!({})).with_ref("2");
        socket.handle_message(get_msg).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 0);

        // Increment
        let inc_msg = PhxMessage::new("room:lobby", "increment", json!({})).with_ref("3");
        socket.handle_message(inc_msg).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 1);

        // Increment again
        let inc_msg2 = PhxMessage::new("room:lobby", "increment", json!({})).with_ref("4");
        socket.handle_message(inc_msg2).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 2);

        // Get final count
        let get_msg2 = PhxMessage::new("room:lobby", "get_count", json!({})).with_ref("5");
        socket.handle_message(get_msg2).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 2);
    }

    #[tokio::test]
    async fn test_assigns_isolated_between_topics() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", EchoChannel);
        let (mut socket, mut receiver) = create_test_socket(registry);

        // Join first room
        let join1 = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
        socket.handle_message(join1).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        // Join second room
        let join2 = PhxMessage::new("room:game", events::PHX_JOIN, json!({})).with_ref("2");
        socket.handle_message(join2).await.unwrap();
        let _ = receiver.try_recv().unwrap();

        // Increment in first room
        let inc1 = PhxMessage::new("room:lobby", "increment", json!({})).with_ref("3");
        socket.handle_message(inc1).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 1);

        // Check second room still at 0
        let get2 = PhxMessage::new("room:game", "get_count", json!({})).with_ref("4");
        socket.handle_message(get2).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 0);

        // Increment first room again
        let inc1b = PhxMessage::new("room:lobby", "increment", json!({})).with_ref("5");
        socket.handle_message(inc1b).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 2);

        // Second room still at 0
        let get2b = PhxMessage::new("room:game", "get_count", json!({})).with_ref("6");
        socket.handle_message(get2b).await.unwrap();
        let reply = receiver.try_recv().unwrap();
        assert_eq!(reply.payload["response"]["count"], 0);
    }
}
