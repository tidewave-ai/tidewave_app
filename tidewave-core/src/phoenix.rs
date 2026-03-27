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
pub enum Payload {
    Json(Value),
    Binary(Vec<u8>),
}

impl Payload {
    /// Returns the inner JSON value, or panics if this is a binary payload.
    pub fn as_json(&self) -> &Value {
        match self {
            Payload::Json(v) => v,
            Payload::Binary(_) => panic!("expected JSON payload, got binary"),
        }
    }

    /// Consumes self and returns the inner JSON value, or panics if binary.
    pub fn into_json(self) -> Value {
        match self {
            Payload::Json(v) => v,
            Payload::Binary(_) => panic!("expected JSON payload, got binary"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhxMessage {
    pub join_ref: Option<String>,
    pub ref_: Option<String>,
    pub topic: String,
    pub event: String,
    pub payload: Payload,
}

impl PhxMessage {
    pub fn new(topic: impl Into<String>, event: impl Into<String>, payload: Value) -> Self {
        Self {
            join_ref: None,
            ref_: None,
            topic: topic.into(),
            event: event.into(),
            payload: Payload::Json(payload),
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
            payload: Payload::Json(serde_json::json!({ "reason": reason.into() })),
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
            payload: Payload::Json(serde_json::json!({ "status": status, "response": response })),
        }
    }

    pub fn close(topic: impl Into<String>, join_ref: Option<String>) -> Self {
        Self {
            join_ref,
            ref_: None,
            topic: topic.into(),
            event: events::PHX_CLOSE.to_string(),
            payload: Payload::Json(serde_json::json!({})),
        }
    }

    pub fn heartbeat_reply(request: &PhxMessage) -> Self {
        Self {
            join_ref: None,
            ref_: request.ref_.clone(),
            topic: "phoenix".to_string(),
            event: events::PHX_REPLY.to_string(),
            payload: Payload::Json(serde_json::json!({ "status": "ok", "response": {} })),
        }
    }

    /// Encode to V2 JSON array format: [join_ref, ref, topic, event, payload]
    ///
    /// NOTE: Only JSON text encoding is supported for outgoing messages for now.
    /// Panics if the payload is binary.
    pub fn encode(self) -> String {
        let json_payload = match self.payload {
            Payload::Json(v) => v,
            Payload::Binary(_) => panic!("binary payload encoding is not supported yet"),
        };
        let array: Vec<Value> = vec![
            self.join_ref.map(Value::String).unwrap_or(Value::Null),
            self.ref_.map(Value::String).unwrap_or(Value::Null),
            Value::String(self.topic),
            Value::String(self.event),
            json_payload,
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
            payload: Payload::Json(payload),
        })
    }

    /// Decode from Phoenix V2 binary wire format (client → server push only).
    ///
    /// Wire format: [kind=0, join_ref_size, ref_size, topic_size, event_size,
    ///               join_ref, ref, topic, event, data]
    pub fn decode_binary(buf: &[u8]) -> Result<Self, String> {
        if buf.len() < 5 {
            return Err("binary message too short".into());
        }

        let kind = buf[0];
        if kind != 0 {
            return Err(format!("expected push (kind 0), got kind {}", kind));
        }

        let join_ref_size = buf[1] as usize;
        let ref_size = buf[2] as usize;
        let topic_size = buf[3] as usize;
        let event_size = buf[4] as usize;

        let mut offset = 5;
        let total_meta = join_ref_size + ref_size + topic_size + event_size;
        if buf.len() < offset + total_meta {
            return Err("binary message truncated".into());
        }

        let join_ref =
            std::str::from_utf8(&buf[offset..offset + join_ref_size]).map_err(|e| e.to_string())?;
        offset += join_ref_size;

        let ref_ =
            std::str::from_utf8(&buf[offset..offset + ref_size]).map_err(|e| e.to_string())?;
        offset += ref_size;

        let topic =
            std::str::from_utf8(&buf[offset..offset + topic_size]).map_err(|e| e.to_string())?;
        offset += topic_size;

        let event =
            std::str::from_utf8(&buf[offset..offset + event_size]).map_err(|e| e.to_string())?;
        offset += event_size;

        fn non_empty(s: &str) -> Option<String> {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }

        Ok(Self {
            join_ref: non_empty(join_ref),
            ref_: non_empty(ref_),
            topic: topic.to_string(),
            event: event.to_string(),
            payload: Payload::Binary(buf[offset..].to_vec()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_decode() {
        // Build a binary push as the JS client would send it
        let mut buf = Vec::new();
        buf.push(0); // KIND_PUSH
        buf.push(1); // join_ref_size
        buf.push(1); // ref_size
        buf.push(13); // topic_size
        buf.push(5); // event_size
        buf.extend_from_slice(b"1"); // join_ref
        buf.extend_from_slice(b"3"); // ref
        buf.extend_from_slice(b"recording:abc"); // topic
        buf.extend_from_slice(b"chunk"); // event
        buf.extend_from_slice(b"\xDE\xAD\xBE\xEF"); // binary payload

        let msg = PhxMessage::decode_binary(&buf).unwrap();
        assert_eq!(msg.join_ref.as_deref(), Some("1"));
        assert_eq!(msg.ref_.as_deref(), Some("3"));
        assert_eq!(msg.topic, "recording:abc");
        assert_eq!(msg.event, "chunk");
        assert_eq!(msg.payload, Payload::Binary(b"\xDE\xAD\xBE\xEF".to_vec()));
    }

    #[test]
    fn test_binary_decode_empty_payload() {
        let mut buf = Vec::new();
        buf.push(0);
        buf.push(0); // no join_ref
        buf.push(1); // ref_size
        buf.push(5); // topic_size
        buf.push(4); // event_size
        buf.extend_from_slice(b"2"); // ref
        buf.extend_from_slice(b"topic"); // topic
        buf.extend_from_slice(b"done"); // event

        let msg = PhxMessage::decode_binary(&buf).unwrap();
        assert_eq!(msg.join_ref, None);
        assert_eq!(msg.ref_.as_deref(), Some("2"));
        assert_eq!(msg.topic, "topic");
        assert_eq!(msg.event, "done");
        assert_eq!(msg.payload, Payload::Binary(vec![]));
    }

    #[test]
    fn test_binary_decode_wrong_kind() {
        let buf = vec![1, 0, 0, 0, 0]; // KIND_REPLY
        assert!(PhxMessage::decode_binary(&buf).is_err());
    }

    #[test]
    fn test_binary_decode_truncated() {
        let buf = vec![0, 5, 0, 0, 0]; // claims join_ref is 5 bytes but there are none
        assert!(PhxMessage::decode_binary(&buf).is_err());
    }
}
