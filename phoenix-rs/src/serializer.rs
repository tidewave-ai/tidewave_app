use crate::error::PhoenixError;
use crate::message::PhxMessage;
use serde_json::Value;

/// Encode a PhxMessage to V2 JSON array format: [join_ref, ref, topic, event, payload]
pub fn encode(msg: &PhxMessage) -> Result<String, PhoenixError> {
    let array: Vec<Value> = vec![
        msg.join_ref
            .as_ref()
            .map(|s| Value::String(s.clone()))
            .unwrap_or(Value::Null),
        msg.ref_
            .as_ref()
            .map(|s| Value::String(s.clone()))
            .unwrap_or(Value::Null),
        Value::String(msg.topic.clone()),
        Value::String(msg.event.clone()),
        msg.payload.clone(),
    ];

    serde_json::to_string(&array).map_err(PhoenixError::Serialization)
}

/// Decode a message from JSON (supports both V1 map and V2 array formats)
pub fn decode(data: &str) -> Result<PhxMessage, PhoenixError> {
    // Try to parse as JSON
    let value: Value = serde_json::from_str(data).map_err(PhoenixError::Serialization)?;

    match value {
        Value::Array(arr) => decode_array_format(arr),
        Value::Object(_) => decode_map_format(value),
        _ => Err(PhoenixError::InvalidMessage(
            "Expected array or object".to_string(),
        )),
    }
}

/// Decode V2 array format: [join_ref, ref, topic, event, payload]
fn decode_array_format(arr: Vec<Value>) -> Result<PhxMessage, PhoenixError> {
    if arr.len() != 5 {
        return Err(PhoenixError::InvalidMessage(format!(
            "Expected 5 elements, got {}",
            arr.len()
        )));
    }

    let join_ref = match &arr[0] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => {
            return Err(PhoenixError::InvalidMessage(
                "Invalid join_ref type".to_string(),
            ));
        }
    };

    let ref_ = match &arr[1] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => return Err(PhoenixError::InvalidMessage("Invalid ref type".to_string())),
    };

    let topic = match &arr[2] {
        Value::String(s) => s.clone(),
        _ => {
            return Err(PhoenixError::InvalidMessage(
                "Invalid topic type".to_string(),
            ));
        }
    };

    let event = match &arr[3] {
        Value::String(s) => s.clone(),
        _ => {
            return Err(PhoenixError::InvalidMessage(
                "Invalid event type".to_string(),
            ));
        }
    };

    let payload = arr[4].clone();

    Ok(PhxMessage {
        join_ref,
        ref_,
        topic,
        event,
        payload,
    })
}

/// Decode V1 map format: {join_ref, ref, topic, event, payload}
fn decode_map_format(value: Value) -> Result<PhxMessage, PhoenixError> {
    let obj = value
        .as_object()
        .ok_or_else(|| PhoenixError::InvalidMessage("Expected object".to_string()))?;

    let join_ref = obj.get("join_ref").and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => None,
    });

    let ref_ = obj.get("ref").and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => None,
    });

    let topic = obj
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PhoenixError::InvalidMessage("Missing or invalid topic".to_string()))?
        .to_string();

    let event = obj
        .get("event")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PhoenixError::InvalidMessage("Missing or invalid event".to_string()))?
        .to_string();

    let payload = obj.get("payload").cloned().unwrap_or(Value::Null);

    Ok(PhxMessage {
        join_ref,
        ref_,
        topic,
        event,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_encode_message_with_refs() {
        let msg = PhxMessage {
            join_ref: Some("1".to_string()),
            ref_: Some("2".to_string()),
            topic: "room:lobby".to_string(),
            event: "new_message".to_string(),
            payload: json!({"body": "Hello World", "user": "john"}),
        };

        let encoded = encode(&msg).unwrap();
        let decoded: Vec<Value> = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.len(), 5);
        assert_eq!(decoded[0], json!("1")); // join_ref
        assert_eq!(decoded[1], json!("2")); // ref
        assert_eq!(decoded[2], json!("room:lobby")); // topic
        assert_eq!(decoded[3], json!("new_message")); // event
        assert_eq!(decoded[4], json!({"body": "Hello World", "user": "john"})); // payload
    }

    #[test]
    fn test_encode_message_without_refs() {
        let msg = PhxMessage {
            join_ref: None,
            ref_: None,
            topic: "room:lobby".to_string(),
            event: "broadcast".to_string(),
            payload: json!({"message": "hello"}),
        };

        let encoded = encode(&msg).unwrap();
        let decoded: Vec<Value> = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.len(), 5);
        assert_eq!(decoded[0], Value::Null); // join_ref is null
        assert_eq!(decoded[1], Value::Null); // ref is null
        assert_eq!(decoded[2], json!("room:lobby"));
        assert_eq!(decoded[3], json!("broadcast"));
    }

    #[test]
    fn test_decode_v2_array_format() {
        let data =
            r#"["1", "2", "room:lobby", "new_message", {"body": "Hello World", "user": "john"}]"#;

        let msg = decode(data).unwrap();

        assert_eq!(msg.join_ref, Some("1".to_string()));
        assert_eq!(msg.ref_, Some("2".to_string()));
        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "new_message");
        assert_eq!(msg.payload["body"], "Hello World");
        assert_eq!(msg.payload["user"], "john");
    }

    #[test]
    fn test_decode_v2_array_format_with_nulls() {
        let data = r#"[null, null, "room:lobby", "broadcast", {"message": "hello"}]"#;

        let msg = decode(data).unwrap();

        assert!(msg.join_ref.is_none());
        assert!(msg.ref_.is_none());
        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "broadcast");
    }

    #[test]
    fn test_decode_v1_map_format() {
        let data = r#"{"join_ref": "1", "ref": "2", "topic": "room:lobby", "event": "new_message", "payload": {"body": "hello"}}"#;

        let msg = decode(data).unwrap();

        assert_eq!(msg.join_ref, Some("1".to_string()));
        assert_eq!(msg.ref_, Some("2".to_string()));
        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "new_message");
        assert_eq!(msg.payload["body"], "hello");
    }

    #[test]
    fn test_decode_v1_map_format_with_nulls() {
        let data = r#"{"join_ref": null, "ref": null, "topic": "room:test", "event": "test", "payload": {}}"#;

        let msg = decode(data).unwrap();

        assert!(msg.join_ref.is_none());
        assert!(msg.ref_.is_none());
        assert_eq!(msg.topic, "room:test");
        assert_eq!(msg.event, "test");
    }

    #[test]
    fn test_roundtrip_encode_decode() {
        let original = PhxMessage {
            join_ref: Some("join1".to_string()),
            ref_: Some("ref1".to_string()),
            topic: "room:test".to_string(),
            event: "test_event".to_string(),
            payload: json!({"key": "value", "number": 42}),
        };

        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded.join_ref, original.join_ref);
        assert_eq!(decoded.ref_, original.ref_);
        assert_eq!(decoded.topic, original.topic);
        assert_eq!(decoded.event, original.event);
        assert_eq!(decoded.payload, original.payload);
    }

    #[test]
    fn test_decode_invalid_json() {
        let result = decode("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_invalid_array_length() {
        let data = r#"["one", "two", "three"]"#;
        let result = decode(data);
        assert!(result.is_err());
        assert!(matches!(result, Err(PhoenixError::InvalidMessage(_))));
    }

    #[test]
    fn test_decode_missing_topic() {
        let data = r#"{"event": "test", "payload": {}}"#;
        let result = decode(data);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_missing_event() {
        let data = r#"{"topic": "test", "payload": {}}"#;
        let result = decode(data);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_heartbeat() {
        let data = r#"[null, "42", "phoenix", "heartbeat", {}]"#;

        let msg = decode(data).unwrap();

        assert!(msg.join_ref.is_none());
        assert_eq!(msg.ref_, Some("42".to_string()));
        assert_eq!(msg.topic, "phoenix");
        assert_eq!(msg.event, "heartbeat");
    }

    #[test]
    fn test_decode_join_message() {
        let data = r#"["1", "1", "room:lobby", "phx_join", {"user_id": "123"}]"#;

        let msg = decode(data).unwrap();

        assert_eq!(msg.join_ref, Some("1".to_string()));
        assert_eq!(msg.ref_, Some("1".to_string()));
        assert_eq!(msg.topic, "room:lobby");
        assert_eq!(msg.event, "phx_join");
        assert_eq!(msg.payload["user_id"], "123");
    }
}
