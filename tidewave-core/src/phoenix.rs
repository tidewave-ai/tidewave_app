//! Minimal Phoenix.js V2 wire format serialization.

use serde_json::Value;

/// Result of a channel init function.
///
/// - `Done` — clean exit (client left or disconnected).
/// - `Error(reason)` — join validation failure (before entering the main loop).
/// - `Shutdown(reason)` — runtime error (watcher died, watched path removed, etc.).
pub enum InitResult {
    Done,
    Error(String),
    Shutdown(String),
}

// ============================================================================
// Message Types & Wire Format
// ============================================================================

pub mod events {
    pub const PHX_JOIN: &str = "phx_join";
    pub const PHX_LEAVE: &str = "phx_leave";
    pub const PHX_REPLY: &str = "phx_reply";
    pub const PHX_ERROR: &str = "phx_error";
    pub const PHX_CLOSE: &str = "phx_close";
    pub const HEARTBEAT: &str = "heartbeat";
}

#[derive(Debug, Clone, PartialEq)]
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

    pub fn error(
        topic: impl Into<String>,
        join_ref: Option<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            join_ref,
            ref_: None,
            topic: topic.into(),
            event: events::PHX_ERROR.to_string(),
            payload: serde_json::json!({ "reason": reason.into() }),
        }
    }

    pub fn ok_reply(request: &PhxMessage, response: Value) -> Self {
        Self::reply(request, "ok", response)
    }

    pub fn error_reply(request: &PhxMessage, reason: impl Into<String>) -> Self {
        Self::reply(
            request,
            "error",
            serde_json::json!({ "reason": reason.into() }),
        )
    }

    pub fn reply(request: &PhxMessage, status: &str, response: Value) -> Self {
        Self {
            join_ref: request.join_ref.clone(),
            ref_: request.ref_.clone(),
            topic: request.topic.clone(),
            event: events::PHX_REPLY.to_string(),
            payload: serde_json::json!({ "status": status, "response": response }),
        }
    }

    pub fn close(topic: impl Into<String>, join_ref: Option<String>) -> Self {
        Self {
            join_ref,
            ref_: None,
            topic: topic.into(),
            event: events::PHX_CLOSE.to_string(),
            payload: serde_json::json!({}),
        }
    }

    pub fn heartbeat_reply(request: &PhxMessage) -> Self {
        Self {
            join_ref: None,
            ref_: request.ref_.clone(),
            topic: "phoenix".to_string(),
            event: events::PHX_REPLY.to_string(),
            payload: serde_json::json!({ "status": "ok", "response": {} }),
        }
    }

    /// Encode to V2 JSON array format: [join_ref, ref, topic, event, payload]
    pub fn encode(self) -> String {
        let array: Vec<Value> = vec![
            self.join_ref.map(Value::String).unwrap_or(Value::Null),
            self.ref_.map(Value::String).unwrap_or(Value::Null),
            Value::String(self.topic),
            Value::String(self.event),
            self.payload,
        ];
        serde_json::to_string(&array).unwrap_or_default()
    }

    /// Decode from V2 JSON array format
    pub fn decode(data: &str) -> Result<Self, String> {
        let mut arr: Vec<Value> = serde_json::from_str(data).map_err(|e| e.to_string())?;
        if arr.len() != 5 {
            return Err(format!("Expected 5 elements, got {}", arr.len()));
        }

        let payload = arr.pop().unwrap();
        let event = arr[3].as_str().ok_or("Invalid event")?.to_string();
        let topic = arr[2].as_str().ok_or("Invalid topic")?.to_string();

        Ok(Self {
            join_ref: arr[0].as_str().map(String::from),
            ref_: arr[1].as_str().map(String::from),
            topic,
            event,
            payload,
        })
    }
}
