use std::sync::Arc;

use crate::channel::Channel;

/// Pattern for matching topic strings
#[derive(Debug, Clone)]
pub struct TopicPattern {
    /// The pattern string (e.g., "room:*" or "room:lobby")
    pattern: String,
    /// Whether this is a wildcard pattern
    is_wildcard: bool,
    /// The prefix for wildcard patterns (e.g., "room:" for "room:*")
    prefix: String,
}

impl TopicPattern {
    /// Create a new topic pattern
    ///
    /// Supports exact matches ("room:lobby") and wildcard matches ("room:*")
    pub fn new(pattern: impl Into<String>) -> Self {
        let pattern = pattern.into();
        let is_wildcard = pattern.ends_with(":*");
        let prefix = if is_wildcard {
            pattern.trim_end_matches('*').to_string()
        } else {
            pattern.clone()
        };

        Self {
            pattern,
            is_wildcard,
            prefix,
        }
    }

    /// Check if this pattern matches the given topic
    pub fn matches(&self, topic: &str) -> bool {
        if self.is_wildcard {
            topic.starts_with(&self.prefix)
        } else {
            topic == self.pattern
        }
    }

    /// Get the pattern string
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Check if this is a wildcard pattern
    pub fn is_wildcard(&self) -> bool {
        self.is_wildcard
    }
}

/// Registry for mapping topic patterns to channel handlers
pub struct ChannelRegistry {
    handlers: Vec<(TopicPattern, Arc<dyn Channel>)>,
}

impl ChannelRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Register a channel handler for a topic pattern
    ///
    /// # Arguments
    /// * `pattern` - The topic pattern (e.g., "room:*" or "chat:lobby")
    /// * `channel` - The channel handler implementation
    ///
    /// # Example
    /// ```ignore
    /// let mut registry = ChannelRegistry::new();
    /// registry.register("room:*", MyRoomChannel::new());
    /// registry.register("presence:lobby", PresenceChannel::new());
    /// ```
    pub fn register<C: Channel>(&mut self, pattern: impl Into<String>, channel: C) {
        let pattern = TopicPattern::new(pattern);
        self.handlers.push((pattern, Arc::new(channel)));
    }

    /// Find a channel handler for the given topic
    ///
    /// Returns the first matching handler. Exact matches are preferred over wildcards.
    pub fn find(&self, topic: &str) -> Option<Arc<dyn Channel>> {
        // First, look for exact matches
        for (pattern, channel) in &self.handlers {
            if !pattern.is_wildcard() && pattern.matches(topic) {
                return Some(Arc::clone(channel));
            }
        }

        // Then, look for wildcard matches
        for (pattern, channel) in &self.handlers {
            if pattern.is_wildcard() && pattern.matches(topic) {
                return Some(Arc::clone(channel));
            }
        }

        None
    }

    /// Check if a topic has a registered handler
    pub fn has_handler(&self, topic: &str) -> bool {
        self.find(topic).is_some()
    }

    /// Get the number of registered handlers
    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let patterns: Vec<&str> = self.handlers.iter().map(|(p, _)| p.pattern()).collect();
        f.debug_struct("ChannelRegistry")
            .field("patterns", &patterns)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{HandleResult, JoinResult, SocketRef};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use tokio::sync::mpsc;

    #[test]
    fn test_topic_pattern_exact_match() {
        let pattern = TopicPattern::new("room:lobby");

        assert!(!pattern.is_wildcard());
        assert!(pattern.matches("room:lobby"));
        assert!(!pattern.matches("room:other"));
        assert!(!pattern.matches("room:lobby:nested"));
    }

    #[test]
    fn test_topic_pattern_wildcard_match() {
        let pattern = TopicPattern::new("room:*");

        assert!(pattern.is_wildcard());
        assert!(pattern.matches("room:lobby"));
        assert!(pattern.matches("room:other"));
        assert!(pattern.matches("room:123"));
        assert!(!pattern.matches("chat:lobby"));
        assert!(!pattern.matches("room")); // No colon after prefix
    }

    #[test]
    fn test_topic_pattern_phoenix_special() {
        // The "phoenix" topic for heartbeats
        let pattern = TopicPattern::new("phoenix");
        assert!(!pattern.is_wildcard());
        assert!(pattern.matches("phoenix"));
        assert!(!pattern.matches("phoenix:other"));
    }

    struct TestRoomChannel;
    struct TestChatChannel;
    struct TestLobbyChannel;

    #[async_trait]
    impl Channel for TestRoomChannel {
        async fn join(&self, _topic: &str, _payload: Value, _socket: &mut SocketRef) -> JoinResult {
            JoinResult::ok(json!({"channel": "room"}))
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

    #[async_trait]
    impl Channel for TestChatChannel {
        async fn join(&self, _topic: &str, _payload: Value, _socket: &mut SocketRef) -> JoinResult {
            JoinResult::ok(json!({"channel": "chat"}))
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

    #[async_trait]
    impl Channel for TestLobbyChannel {
        async fn join(&self, _topic: &str, _payload: Value, _socket: &mut SocketRef) -> JoinResult {
            JoinResult::ok(json!({"channel": "lobby"}))
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

    #[test]
    fn test_registry_empty() {
        let registry = ChannelRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(!registry.has_handler("room:lobby"));
    }

    #[test]
    fn test_registry_register_and_find() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestRoomChannel);
        registry.register("chat:*", TestChatChannel);

        assert_eq!(registry.len(), 2);
        assert!(registry.has_handler("room:lobby"));
        assert!(registry.has_handler("room:123"));
        assert!(registry.has_handler("chat:general"));
        assert!(!registry.has_handler("unknown:topic"));
    }

    #[test]
    fn test_registry_exact_match_preferred() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestRoomChannel); // Wildcard
        registry.register("room:lobby", TestLobbyChannel); // Exact

        // Exact match should be preferred over wildcard
        let _handler = registry.find("room:lobby").unwrap();

        // We verify the correct handler is returned in the async test below
    }

    #[tokio::test]
    async fn test_registry_exact_match_behavior() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestRoomChannel);
        registry.register("room:lobby", TestLobbyChannel);

        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();
        let mut socket = SocketRef::new(
            "room:lobby".to_string(),
            "1".to_string(),
            sender,
            broadcast_sender,
        );

        // Should use the exact match (lobby channel)
        let handler = registry.find("room:lobby").unwrap();
        let result = handler.join("room:lobby", json!({}), &mut socket).await;

        if let JoinResult::Ok(payload) = result {
            assert_eq!(payload["channel"], "lobby");
        } else {
            panic!("Expected Ok result");
        }

        // Should use wildcard for other room topics
        let (sender, _) = mpsc::unbounded_channel();
        let (broadcast_sender, _) = mpsc::unbounded_channel();
        let mut socket = SocketRef::new(
            "room:other".to_string(),
            "2".to_string(),
            sender,
            broadcast_sender,
        );

        let handler = registry.find("room:other").unwrap();
        let result = handler.join("room:other", json!({}), &mut socket).await;

        if let JoinResult::Ok(payload) = result {
            assert_eq!(payload["channel"], "room");
        } else {
            panic!("Expected Ok result");
        }
    }

    #[test]
    fn test_registry_no_match() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestRoomChannel);

        assert!(registry.find("chat:lobby").is_none());
        assert!(!registry.has_handler("chat:lobby"));
    }

    #[test]
    fn test_registry_debug() {
        let mut registry = ChannelRegistry::new();
        registry.register("room:*", TestRoomChannel);
        registry.register("chat:lobby", TestChatChannel);

        let debug_str = format!("{:?}", registry);
        assert!(debug_str.contains("room:*"));
        assert!(debug_str.contains("chat:lobby"));
    }
}
