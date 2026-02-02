use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Phoenix channel message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PhxMessage {
    pub join_ref: Option<String>,
    pub ref_: Option<String>,
    pub topic: String,
    pub event: String,
    pub payload: Value,
}

impl PhxMessage {
    pub fn new(topic: impl Into<String>, event: impl Into<String>, payload: Value) -> Self {
        Self {
            join_ref: None,
            ref_: None,
            topic: topic.into(),
            event: event.into(),
            payload,
        }
    }

    pub fn with_ref(mut self, ref_: impl Into<String>) -> Self {
        self.ref_ = Some(ref_.into());
        self
    }

    pub fn with_join_ref(mut self, join_ref: impl Into<String>) -> Self {
        self.join_ref = Some(join_ref.into());
        self
    }

    /// Create a reply message for the given request
    pub fn reply(request: &PhxMessage, status: &str, response: Value) -> Self {
        Self {
            join_ref: request.join_ref.clone(),
            ref_: request.ref_.clone(),
            topic: request.topic.clone(),
            event: events::PHX_REPLY.to_string(),
            payload: serde_json::json!({
                "status": status,
                "response": response,
            }),
        }
    }

    /// Create a heartbeat reply
    pub fn heartbeat_reply(request: &PhxMessage) -> Self {
        Self {
            join_ref: None,
            ref_: request.ref_.clone(),
            topic: "phoenix".to_string(),
            event: events::PHX_REPLY.to_string(),
            payload: serde_json::json!({
                "status": "ok",
                "response": {},
            }),
        }
    }
}

/// Phoenix channel event constants
pub mod events {
    pub const PHX_JOIN: &str = "phx_join";
    pub const PHX_LEAVE: &str = "phx_leave";
    pub const PHX_REPLY: &str = "phx_reply";
    pub const PHX_CLOSE: &str = "phx_close";
    pub const PHX_ERROR: &str = "phx_error";
    pub const HEARTBEAT: &str = "heartbeat";
}

/// Reply status constants
pub mod status {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_message_creation() {
        let msg = PhxMessage::new("room:lobby", "new_message", json!({"body": "hello"}));
        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "new_message");
        assert_eq!(msg.payload, json!({"body": "hello"}));
        assert!(msg.join_ref.is_none());
        assert!(msg.ref_.is_none());
    }

    #[test]
    fn test_message_with_refs() {
        let msg = PhxMessage::new("room:lobby", "new_message", json!({}))
            .with_ref("1")
            .with_join_ref("2");

        assert_eq!(msg.ref_, Some("1".to_string()));
        assert_eq!(msg.join_ref, Some("2".to_string()));
    }

    #[test]
    fn test_reply_creation() {
        let request = PhxMessage::new("room:lobby", "phx_join", json!({}))
            .with_ref("5")
            .with_join_ref("3");

        let reply = PhxMessage::reply(&request, "ok", json!({"joined": true}));

        assert_eq!(reply.topic, "room:lobby");
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.ref_, Some("5".to_string()));
        assert_eq!(reply.join_ref, Some("3".to_string()));
        assert_eq!(
            reply.payload,
            json!({"status": "ok", "response": {"joined": true}})
        );
    }

    #[test]
    fn test_heartbeat_reply() {
        let heartbeat = PhxMessage::new("phoenix", events::HEARTBEAT, json!({})).with_ref("42");

        let reply = PhxMessage::heartbeat_reply(&heartbeat);

        assert_eq!(reply.topic, "phoenix");
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.ref_, Some("42".to_string()));
        assert!(reply.join_ref.is_none());
        assert_eq!(reply.payload, json!({"status": "ok", "response": {}}));
    }

    #[test]
    fn test_event_constants() {
        assert_eq!(events::PHX_JOIN, "phx_join");
        assert_eq!(events::PHX_LEAVE, "phx_leave");
        assert_eq!(events::PHX_REPLY, "phx_reply");
        assert_eq!(events::PHX_CLOSE, "phx_close");
        assert_eq!(events::PHX_ERROR, "phx_error");
        assert_eq!(events::HEARTBEAT, "heartbeat");
    }
}
