use thiserror::Error;

#[derive(Error, Debug)]
pub enum PhoenixError {
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid message: {0}")]
    InvalidMessage(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Channel error: {0}")]
    Channel(String),

    #[error("Topic not found: {0}")]
    TopicNotFound(String),

    #[error("Not joined: {0}")]
    NotJoined(String),

    #[error("Connection closed")]
    ConnectionClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = PhoenixError::InvalidMessage("test error".to_string());
        assert_eq!(err.to_string(), "Invalid message: test error");
    }

    #[test]
    fn test_channel_error() {
        let err = PhoenixError::Channel("join failed".to_string());
        assert_eq!(err.to_string(), "Channel error: join failed");
    }

    #[test]
    fn test_topic_not_found() {
        let err = PhoenixError::TopicNotFound("room:unknown".to_string());
        assert_eq!(err.to_string(), "Topic not found: room:unknown");
    }
}
