use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use phoenix_rs::{
    ChannelRegistry, PhxMessage,
    channel::{Channel, HandleResult, JoinResult, SocketRef},
    events, phoenix_router,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Test channel that echoes messages
struct EchoChannel;

#[async_trait]
impl Channel for EchoChannel {
    async fn join(&self, topic: &str, payload: Value, _socket: &mut SocketRef) -> JoinResult {
        // Extract user_id from payload if present
        let user_id = payload.get("user_id").and_then(|v| v.as_str());
        JoinResult::ok(json!({
            "joined": topic,
            "user_id": user_id,
        }))
    }

    async fn handle_in(&self, event: &str, payload: Value, socket: &mut SocketRef) -> HandleResult {
        match event {
            "ping" => HandleResult::ok(json!({"pong": true})),
            "echo" => HandleResult::ok(payload),
            "broadcast" => {
                socket.broadcast("broadcasted", payload.clone());
                HandleResult::ok(json!({}))
            }
            "push_to_me" => {
                socket.push("pushed", json!({"from": "server"}));
                HandleResult::ok(json!({}))
            }
            "error" => HandleResult::error(json!({"reason": "test error"})),
            "no_reply" => HandleResult::no_reply(),
            _ => HandleResult::no_reply(),
        }
    }
}

/// Test channel that rejects joins
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

/// Start a test server and return its address
async fn start_test_server() -> SocketAddr {
    let mut registry = ChannelRegistry::new();
    registry.register("room:*", EchoChannel);
    registry.register("private:*", RejectChannel);

    let app = phoenix_router(registry);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    addr
}

/// Helper to send a Phoenix message and receive the response
async fn send_and_receive(
    ws: &mut (
        impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
        impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    ),
    msg: PhxMessage,
) -> Option<PhxMessage> {
    let encoded = phoenix_rs::serializer::encode(&msg).unwrap();
    ws.0.send(Message::Text(encoded.into())).await.ok()?;

    // Wait for response
    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), ws.1.next())
        .await
        .ok()??;

    match response {
        Ok(Message::Text(text)) => phoenix_rs::serializer::decode(&text).ok(),
        _ => None,
    }
}

#[tokio::test]
async fn test_connect_and_heartbeat() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let (mut write, mut read) = ws_stream.split();

    // Send heartbeat
    let heartbeat = PhxMessage::new("phoenix", events::HEARTBEAT, json!({})).with_ref("1");
    let encoded = phoenix_rs::serializer::encode(&heartbeat).unwrap();
    write.send(Message::Text(encoded.into())).await.unwrap();

    // Receive heartbeat reply
    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), read.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    if let Message::Text(text) = response {
        let reply = phoenix_rs::serializer::decode(&text).unwrap();
        assert_eq!(reply.topic, "phoenix");
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.ref_, Some("1".to_string()));
        assert_eq!(reply.payload["status"], "ok");
    } else {
        panic!("Expected text message");
    }
}

#[tokio::test]
async fn test_join_channel() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join channel
    let join_msg =
        PhxMessage::new("room:lobby", events::PHX_JOIN, json!({"user_id": "alice"})).with_ref("1");

    let reply = send_and_receive(&mut ws, join_msg).await.unwrap();

    assert_eq!(reply.topic, "room:lobby");
    assert_eq!(reply.event, events::PHX_REPLY);
    assert_eq!(reply.ref_, Some("1".to_string()));
    assert_eq!(reply.payload["status"], "ok");
    assert_eq!(reply.payload["response"]["joined"], "room:lobby");
    assert_eq!(reply.payload["response"]["user_id"], "alice");
}

#[tokio::test]
async fn test_join_rejected() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Try to join private channel (should be rejected)
    let join_msg = PhxMessage::new("private:secret", events::PHX_JOIN, json!({})).with_ref("1");

    let reply = send_and_receive(&mut ws, join_msg).await.unwrap();

    assert_eq!(reply.payload["status"], "error");
    assert_eq!(reply.payload["response"]["reason"], "unauthorized");
}

#[tokio::test]
async fn test_join_unknown_topic() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Try to join unknown topic
    let join_msg = PhxMessage::new("unknown:topic", events::PHX_JOIN, json!({})).with_ref("1");

    let reply = send_and_receive(&mut ws, join_msg).await.unwrap();

    assert_eq!(reply.payload["status"], "error");
    assert_eq!(reply.payload["response"]["reason"], "no handler for topic");
}

#[tokio::test]
async fn test_push_and_reply() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join first
    let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let _ = send_and_receive(&mut ws, join_msg).await.unwrap();

    // Send ping
    let ping_msg = PhxMessage::new("room:lobby", "ping", json!({})).with_ref("2");
    let reply = send_and_receive(&mut ws, ping_msg).await.unwrap();

    assert_eq!(reply.event, events::PHX_REPLY);
    assert_eq!(reply.ref_, Some("2".to_string()));
    assert_eq!(reply.payload["status"], "ok");
    assert_eq!(reply.payload["response"]["pong"], true);
}

#[tokio::test]
async fn test_echo() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join first
    let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let _ = send_and_receive(&mut ws, join_msg).await.unwrap();

    // Send echo
    let echo_msg = PhxMessage::new(
        "room:lobby",
        "echo",
        json!({"message": "hello", "number": 42}),
    )
    .with_ref("2");
    let reply = send_and_receive(&mut ws, echo_msg).await.unwrap();

    assert_eq!(reply.payload["status"], "ok");
    assert_eq!(reply.payload["response"]["message"], "hello");
    assert_eq!(reply.payload["response"]["number"], 42);
}

#[tokio::test]
async fn test_error_reply() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join first
    let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let _ = send_and_receive(&mut ws, join_msg).await.unwrap();

    // Send error event
    let error_msg = PhxMessage::new("room:lobby", "error", json!({})).with_ref("2");
    let reply = send_and_receive(&mut ws, error_msg).await.unwrap();

    assert_eq!(reply.payload["status"], "error");
    assert_eq!(reply.payload["response"]["reason"], "test error");
}

#[tokio::test]
async fn test_leave_channel() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join first
    let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let _ = send_and_receive(&mut ws, join_msg).await.unwrap();

    // Leave channel
    let leave_msg = PhxMessage::new("room:lobby", events::PHX_LEAVE, json!({})).with_ref("2");
    let reply = send_and_receive(&mut ws, leave_msg).await.unwrap();

    assert_eq!(reply.payload["status"], "ok");
}

#[tokio::test]
async fn test_leave_without_join() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Leave without joining - Phoenix returns ok for stale phx_leave
    let leave_msg = PhxMessage::new("room:lobby", events::PHX_LEAVE, json!({})).with_ref("1");
    let reply = send_and_receive(&mut ws, leave_msg).await.unwrap();

    assert_eq!(reply.payload["status"], "ok");
}

#[tokio::test]
async fn test_server_push() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let (mut write, mut read) = ws_stream.split();

    // Join first
    let join_msg = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let encoded = phoenix_rs::serializer::encode(&join_msg).unwrap();
    write.send(Message::Text(encoded.into())).await.unwrap();
    let _ = read.next().await; // consume join reply

    // Request server push
    let push_msg = PhxMessage::new("room:lobby", "push_to_me", json!({})).with_ref("2");
    let encoded = phoenix_rs::serializer::encode(&push_msg).unwrap();
    write.send(Message::Text(encoded.into())).await.unwrap();

    // We should receive two messages: the reply and the push
    let mut received_reply = false;
    let mut received_push = false;

    for _ in 0..2 {
        let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), read.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        if let Message::Text(text) = response {
            let msg = phoenix_rs::serializer::decode(&text).unwrap();
            if msg.event == events::PHX_REPLY {
                received_reply = true;
                assert_eq!(msg.ref_, Some("2".to_string()));
            } else if msg.event == "pushed" {
                received_push = true;
                assert_eq!(msg.payload["from"], "server");
            }
        }
    }

    assert!(received_reply, "Should receive reply");
    assert!(received_push, "Should receive server push");
}

#[tokio::test]
async fn test_multiple_channels() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join first channel
    let join1 = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let reply1 = send_and_receive(&mut ws, join1).await.unwrap();
    assert_eq!(reply1.payload["status"], "ok");

    // Join second channel
    let join2 = PhxMessage::new("room:game", events::PHX_JOIN, json!({})).with_ref("2");
    let reply2 = send_and_receive(&mut ws, join2).await.unwrap();
    assert_eq!(reply2.payload["status"], "ok");

    // Send to first channel
    let msg1 = PhxMessage::new("room:lobby", "echo", json!({"room": "lobby"})).with_ref("3");
    let reply3 = send_and_receive(&mut ws, msg1).await.unwrap();
    assert_eq!(reply3.payload["response"]["room"], "lobby");

    // Send to second channel
    let msg2 = PhxMessage::new("room:game", "echo", json!({"room": "game"})).with_ref("4");
    let reply4 = send_and_receive(&mut ws, msg2).await.unwrap();
    assert_eq!(reply4.payload["response"]["room"], "game");
}

#[tokio::test]
async fn test_double_join_same_topic() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let mut ws = ws_stream.split();

    // Join first time
    let join1 = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("1");
    let reply1 = send_and_receive(&mut ws, join1).await.unwrap();
    assert_eq!(reply1.payload["status"], "ok");

    // Try to join again
    let join2 = PhxMessage::new("room:lobby", events::PHX_JOIN, json!({})).with_ref("2");
    let reply2 = send_and_receive(&mut ws, join2).await.unwrap();
    assert_eq!(reply2.payload["status"], "error");
    assert_eq!(reply2.payload["response"]["reason"], "already joined");
}

#[tokio::test]
async fn test_v1_map_format_support() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    let (ws_stream, _) = connect_async(&url).await.unwrap();
    let (mut write, mut read) = ws_stream.split();

    // Send V1 map format (for backwards compatibility)
    let v1_join = r#"{"join_ref": "1", "ref": "1", "topic": "room:lobby", "event": "phx_join", "payload": {}}"#;
    write
        .send(Message::Text(v1_join.to_string().into()))
        .await
        .unwrap();

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), read.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    if let Message::Text(text) = response {
        let reply = phoenix_rs::serializer::decode(&text).unwrap();
        assert_eq!(reply.payload["status"], "ok");
        assert_eq!(reply.payload["response"]["joined"], "room:lobby");
    } else {
        panic!("Expected text message");
    }
}

#[tokio::test]
async fn test_broadcast_to_multiple_clients() {
    let addr = start_test_server().await;
    let url = format!("ws://{}/socket/websocket", addr);

    // Connect two clients
    let (ws1, _) = connect_async(&url).await.unwrap();
    let (mut write1, mut read1) = ws1.split();

    let (ws2, _) = connect_async(&url).await.unwrap();
    let (mut write2, mut read2) = ws2.split();

    // Both join the same room
    let join1 = PhxMessage::new("room:broadcast", events::PHX_JOIN, json!({})).with_ref("1");
    let encoded = phoenix_rs::serializer::encode(&join1).unwrap();
    write1.send(Message::Text(encoded.into())).await.unwrap();
    let _ = read1.next().await; // consume join reply

    let join2 = PhxMessage::new("room:broadcast", events::PHX_JOIN, json!({})).with_ref("1");
    let encoded = phoenix_rs::serializer::encode(&join2).unwrap();
    write2.send(Message::Text(encoded.into())).await.unwrap();
    let _ = read2.next().await; // consume join reply

    // Client 1 broadcasts a message
    let broadcast_msg = PhxMessage::new(
        "room:broadcast",
        "broadcast",
        json!({"message": "hello everyone"}),
    )
    .with_ref("2");
    let encoded = phoenix_rs::serializer::encode(&broadcast_msg).unwrap();
    write1.send(Message::Text(encoded.into())).await.unwrap();

    // Client 1 should receive the reply
    let response1 = tokio::time::timeout(tokio::time::Duration::from_secs(5), read1.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    if let Message::Text(text) = response1 {
        let msg = phoenix_rs::serializer::decode(&text).unwrap();
        assert_eq!(msg.event, events::PHX_REPLY);
        assert_eq!(msg.payload["status"], "ok");
    }

    // Client 2 should receive the broadcast
    let response2 = tokio::time::timeout(tokio::time::Duration::from_secs(5), read2.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    if let Message::Text(text) = response2 {
        let msg = phoenix_rs::serializer::decode(&text).unwrap();
        assert_eq!(msg.topic, "room:broadcast");
        assert_eq!(msg.event, "broadcasted");
        assert_eq!(msg.payload["message"], "hello everyone");
    } else {
        panic!("Expected text message");
    }
}
