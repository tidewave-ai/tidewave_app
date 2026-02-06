//! Minimal Phoenix.js wire format compatible implementation.
//! It supports only the V2 serializer, does not have per channel state
//! or broadcast support.

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use tokio_util::sync::CancellationToken;

use crate::acp_channel::{AcpChannel, AcpChannelState};
use crate::mcp_channel::{McpChannel, McpChannelState};
use crate::watch_channel::{WatchChannel, WatchChannelState};

// ============================================================================
// Message Types & Wire Format
// ============================================================================

pub mod events {
    pub const PHX_JOIN: &str = "phx_join";
    pub const PHX_LEAVE: &str = "phx_leave";
    pub const PHX_REPLY: &str = "phx_reply";
    pub const PHX_CLOSE: &str = "phx_close";
    pub const HEARTBEAT: &str = "heartbeat";
}

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

    pub fn reply(request: &PhxMessage, status: &str, response: Value) -> Self {
        Self {
            join_ref: request.join_ref.clone(),
            ref_: request.ref_.clone(),
            topic: request.topic.clone(),
            event: events::PHX_REPLY.to_string(),
            payload: serde_json::json!({ "status": status, "response": response }),
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
    pub fn encode(&self) -> String {
        let array: Vec<Value> = vec![
            self.join_ref
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
            self.ref_
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
            Value::String(self.topic.clone()),
            Value::String(self.event.clone()),
            self.payload.clone(),
        ];
        serde_json::to_string(&array).unwrap_or_default()
    }

    /// Decode from V2 JSON array format
    pub fn decode(data: &str) -> Result<Self, String> {
        let arr: Vec<Value> = serde_json::from_str(data).map_err(|e| e.to_string())?;
        if arr.len() != 5 {
            return Err(format!("Expected 5 elements, got {}", arr.len()));
        }

        Ok(Self {
            join_ref: arr[0].as_str().map(String::from),
            ref_: arr[1].as_str().map(String::from),
            topic: arr[2].as_str().ok_or("Invalid topic")?.to_string(),
            event: arr[3].as_str().ok_or("Invalid event")?.to_string(),
            payload: arr[4].clone(),
        })
    }
}

// ============================================================================
// Channel Trait & Types
// ============================================================================

#[derive(Debug, Clone)]
pub struct InfoSendError;

type BoxedInfo = Box<dyn Any + Send>;

#[derive(Debug)]
pub struct InfoSender<M: Send + 'static> {
    sender: mpsc::UnboundedSender<BoxedInfo>,
    _phantom: PhantomData<M>,
}

impl<M: Send + 'static> InfoSender<M> {
    fn new(sender: mpsc::UnboundedSender<BoxedInfo>) -> Self {
        Self {
            sender,
            _phantom: PhantomData,
        }
    }

    pub fn send(&self, message: M) -> Result<(), InfoSendError> {
        self.sender
            .send(Box::new(message))
            .map_err(|_| InfoSendError)
    }
}

impl<M: Send + 'static> Clone for InfoSender<M> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            _phantom: PhantomData,
        }
    }
}

#[derive(Debug, Clone)]
pub enum JoinResult {
    Ok(Value),
    Error(Value),
}

impl JoinResult {
    pub fn ok(response: Value) -> Self {
        JoinResult::Ok(response)
    }
    pub fn error(reason: Value) -> Self {
        JoinResult::Error(reason)
    }
}

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

#[derive(Debug, Clone)]
pub enum HandleResult {
    Reply {
        status: ReplyStatus,
        response: Value,
    },
    NoReply,
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

/// Socket reference passed to channel callbacks
pub struct SocketRef {
    pub topic: String,
    pub join_ref: String,
    /// Unique identifier for this subscription, stable across all callbacks
    pub unique_id: uuid::Uuid,
    sender: mpsc::UnboundedSender<PhxMessage>,
    info_sender: mpsc::UnboundedSender<BoxedInfo>,
    shutdown_token: CancellationToken,
}

impl SocketRef {
    pub fn push(&self, event: &str, payload: Value) {
        let msg = PhxMessage::new(self.topic.clone(), event, payload)
            .with_join_ref(self.join_ref.clone());
        let _ = self.sender.send(msg);
    }

    pub fn info_sender<M: Send + 'static>(&self) -> InfoSender<M> {
        InfoSender::new(self.info_sender.clone())
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }
}

#[async_trait]
pub trait Channel: Send + Sync + 'static {
    async fn join(&self, topic: &str, payload: Value, socket: &mut SocketRef) -> JoinResult;
    async fn handle_in(&self, event: &str, payload: Value, socket: &mut SocketRef) -> HandleResult;
    async fn handle_info(&self, _message: Box<dyn Any + Send>, _socket: &mut SocketRef) {}
    async fn terminate(&self, _reason: &str, _socket: &mut SocketRef) {}
}

// ============================================================================
// Subscription Task
// ============================================================================

enum SubscriptionMsg {
    ClientMessage {
        event: String,
        payload: Value,
        msg_ref: Option<String>,
    },
    Leave {
        msg_ref: Option<String>,
    },
    Terminate {
        reason: String,
    },
}

struct SubscriptionHandle {
    sender: mpsc::UnboundedSender<SubscriptionMsg>,
    task: tokio::task::JoinHandle<()>,
}

impl SubscriptionHandle {
    fn send_message(&self, event: String, payload: Value, msg_ref: Option<String>) {
        let _ = self.sender.send(SubscriptionMsg::ClientMessage {
            event,
            payload,
            msg_ref,
        });
    }

    fn send_leave(&self, msg_ref: Option<String>) {
        let _ = self.sender.send(SubscriptionMsg::Leave { msg_ref });
    }

    fn send_terminate(&self, reason: String) {
        let _ = self.sender.send(SubscriptionMsg::Terminate { reason });
    }
}

async fn spawn_subscription(
    channel: Arc<dyn Channel>,
    topic: String,
    join_ref: String,
    join_payload: Value,
    client_tx: mpsc::UnboundedSender<PhxMessage>,
) -> Result<(SubscriptionHandle, Value), Value> {
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<SubscriptionMsg>();
    let (info_tx, mut info_rx) = mpsc::unbounded_channel::<BoxedInfo>();
    let shutdown = CancellationToken::new();

    let unique_id = uuid::Uuid::new_v4();
    let mut socket = SocketRef {
        topic: topic.clone(),
        join_ref: join_ref.clone(),
        unique_id,
        sender: client_tx.clone(),
        info_sender: info_tx.clone(),
        shutdown_token: shutdown.clone(),
    };

    match channel.join(&topic, join_payload, &mut socket).await {
        JoinResult::Ok(response) => {
            let task = tokio::spawn({
                let channel = channel.clone();
                let topic = topic.clone();
                let join_ref = join_ref.clone();
                async move {
                    loop {
                        let mut socket = SocketRef {
                            topic: topic.clone(),
                            join_ref: join_ref.clone(),
                            unique_id,
                            sender: client_tx.clone(),
                            info_sender: info_tx.clone(),
                            shutdown_token: shutdown.clone(),
                        };

                        tokio::select! {
                            Some(msg) = msg_rx.recv() => {
                                match msg {
                                    SubscriptionMsg::ClientMessage { event, payload, msg_ref } => {
                                        let result = channel.handle_in(&event, payload, &mut socket).await;
                                        match result {
                                            HandleResult::Reply { status, response } => {
                                                let reply = PhxMessage {
                                                    join_ref: Some(join_ref.clone()),
                                                    ref_: msg_ref,
                                                    topic: topic.clone(),
                                                    event: events::PHX_REPLY.to_string(),
                                                    payload: serde_json::json!({
                                                        "status": status.as_str(),
                                                        "response": response,
                                                    }),
                                                };
                                                let _ = client_tx.send(reply);
                                            }
                                            HandleResult::NoReply => {}
                                            HandleResult::Stop { reason } => {
                                                shutdown.cancel();
                                                let close = PhxMessage {
                                                    join_ref: Some(join_ref.clone()),
                                                    ref_: None,
                                                    topic: topic.clone(),
                                                    event: events::PHX_CLOSE.to_string(),
                                                    payload: serde_json::json!({"reason": &reason}),
                                                };
                                                let _ = client_tx.send(close);
                                                channel.terminate(&reason, &mut socket).await;
                                                return;
                                            }
                                        }
                                    }
                                    SubscriptionMsg::Leave { msg_ref } => {
                                        shutdown.cancel();
                                        channel.terminate("leave", &mut socket).await;
                                        let reply = PhxMessage {
                                            join_ref: Some(join_ref.clone()),
                                            ref_: msg_ref,
                                            topic: topic.clone(),
                                            event: events::PHX_REPLY.to_string(),
                                            payload: serde_json::json!({ "status": "ok", "response": {} }),
                                        };
                                        let _ = client_tx.send(reply);
                                        return;
                                    }
                                    SubscriptionMsg::Terminate { reason } => {
                                        shutdown.cancel();
                                        channel.terminate(&reason, &mut socket).await;
                                        return;
                                    }
                                }
                            }
                            Some(info) = info_rx.recv() => {
                                channel.handle_info(info, &mut socket).await;
                            }
                            else => return,
                        }
                    }
                }
            });

            Ok((
                SubscriptionHandle {
                    sender: msg_tx,
                    task,
                },
                response,
            ))
        }
        JoinResult::Error(reason) => Err(reason),
    }
}

// ============================================================================
// WebSocket Handler
// ============================================================================

#[derive(Clone)]
pub struct PhoenixState {
    pub acp_state: AcpChannelState,
    pub mcp_state: McpChannelState,
    pub watch_state: WatchChannelState,
}

impl PhoenixState {
    pub fn new(acp_state: AcpChannelState, mcp_state: McpChannelState) -> Self {
        Self {
            acp_state,
            mcp_state,
            watch_state: WatchChannelState::new(),
        }
    }

    fn create_channel(&self, topic: &str) -> Option<Arc<dyn Channel>> {
        if topic.starts_with("watch:") {
            Some(Arc::new(WatchChannel::with_state(self.watch_state.clone())))
        } else if topic.starts_with("acp:") {
            Some(Arc::new(AcpChannel::with_state(self.acp_state.clone())))
        } else if topic.starts_with("mcp:") {
            Some(Arc::new(McpChannel::with_state(self.mcp_state.clone())))
        } else {
            None
        }
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<PhoenixState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(ws: WebSocket, state: PhoenixState) {
    let (ws_tx, ws_rx) = ws.split();

    // Adapt WebSocket to channel-based interface
    let (out_tx, mut out_rx) = futures::channel::mpsc::unbounded::<String>();
    let (in_tx, in_rx) = futures::channel::mpsc::unbounded::<Result<String, ()>>();

    // Forward WebSocket incoming to channel
    let ws_rx_task = tokio::spawn({
        let in_tx = in_tx.clone();
        async move {
            futures::pin_mut!(ws_rx);
            while let Some(result) = ws_rx.next().await {
                match result {
                    Ok(Message::Text(t)) => {
                        if in_tx.unbounded_send(Ok(t.to_string())).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        let _ = in_tx.unbounded_send(Err(()));
                        break;
                    }
                    _ => continue,
                }
            }
        }
    });

    // Forward channel outgoing to WebSocket
    let ws_tx_task = tokio::spawn(async move {
        futures::pin_mut!(ws_tx);
        while let Some(text) = out_rx.next().await {
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Run the core handler
    handle_socket_core(out_tx, in_rx, state).await;

    ws_rx_task.abort();
    ws_tx_task.abort();
}

/// Unit-testable phoenix socket handler that uses channels instead of WebSocket
pub async fn unit_testable_phoenix_handler(
    out_tx: futures::channel::mpsc::UnboundedSender<String>,
    in_rx: futures::channel::mpsc::UnboundedReceiver<Result<String, ()>>,
    state: PhoenixState,
) {
    handle_socket_core(out_tx, in_rx, state).await;
}

async fn handle_socket_core(
    out_tx: futures::channel::mpsc::UnboundedSender<String>,
    mut in_rx: futures::channel::mpsc::UnboundedReceiver<Result<String, ()>>,
    state: PhoenixState,
) {
    let socket_id = uuid::Uuid::new_v4().to_string();
    info!(socket_id = %socket_id, "Phoenix socket connected");

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<PhxMessage>();
    let mut subscriptions: HashMap<String, SubscriptionHandle> = HashMap::new();

    // Sender task - forward from internal channel to output
    let send_task = tokio::spawn({
        let out_tx = out_tx.clone();
        async move {
            while let Some(msg) = client_rx.recv().await {
                if out_tx.unbounded_send(msg.encode()).is_err() {
                    break;
                }
            }
        }
    });

    // Main receive loop
    while let Some(result) = in_rx.next().await {
        let text = match result {
            Ok(t) => t,
            Err(_) => break,
        };

        let msg = match PhxMessage::decode(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!("Invalid message: {}", e);
                continue;
            }
        };

        // Handle heartbeat
        if msg.topic == "phoenix" && msg.event == events::HEARTBEAT {
            let _ = client_tx.send(PhxMessage::heartbeat_reply(&msg));
            continue;
        }

        match msg.event.as_str() {
            events::PHX_JOIN => {
                let topic = &msg.topic;
                let join_ref = msg
                    .ref_
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

                if subscriptions.contains_key(topic) {
                    let _ = client_tx.send(PhxMessage::reply(
                        &msg,
                        "error",
                        serde_json::json!({"reason": "already joined"}),
                    ));
                    continue;
                }

                let Some(channel) = state.create_channel(topic) else {
                    let _ = client_tx.send(PhxMessage::reply(
                        &msg,
                        "error",
                        serde_json::json!({"reason": "no handler for topic"}),
                    ));
                    continue;
                };

                match spawn_subscription(
                    channel,
                    topic.clone(),
                    join_ref.clone(),
                    msg.payload.clone(),
                    client_tx.clone(),
                )
                .await
                {
                    Ok((handle, response)) => {
                        subscriptions.insert(topic.clone(), handle);
                        let reply = PhxMessage {
                            join_ref: Some(join_ref),
                            ref_: msg.ref_.clone(),
                            topic: topic.clone(),
                            event: events::PHX_REPLY.to_string(),
                            payload: serde_json::json!({ "status": "ok", "response": response }),
                        };
                        let _ = client_tx.send(reply);
                    }
                    Err(reason) => {
                        let _ = client_tx.send(PhxMessage::reply(&msg, "error", reason));
                    }
                }
            }
            events::PHX_LEAVE => {
                if let Some(handle) = subscriptions.remove(&msg.topic) {
                    handle.send_leave(msg.ref_.clone());
                    let _ = handle.task.await;
                } else {
                    let _ = client_tx.send(PhxMessage::reply(&msg, "ok", serde_json::json!({})));
                }
            }
            _ => {
                if let Some(handle) = subscriptions.get(&msg.topic) {
                    handle.send_message(msg.event.clone(), msg.payload.clone(), msg.ref_.clone());
                } else {
                    warn!("Message for unjoined topic: {}", msg.topic);
                }
            }
        }
    }

    // Cleanup
    info!(socket_id = %socket_id, "Phoenix socket disconnecting");
    for (_, handle) in subscriptions.drain() {
        handle.send_terminate("disconnect".to_string());
        let _ = handle.task.await;
    }
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_encode_decode() {
        let msg = PhxMessage::new("room:lobby", "test", serde_json::json!({"x": 1}))
            .with_ref("1")
            .with_join_ref("2");
        let encoded = msg.encode();
        let decoded = PhxMessage::decode(&encoded).unwrap();
        assert_eq!(decoded.topic, "room:lobby");
        assert_eq!(decoded.event, "test");
        assert_eq!(decoded.ref_, Some("1".to_string()));
        assert_eq!(decoded.join_ref, Some("2".to_string()));
    }

    #[test]
    fn test_reply_message() {
        let req = PhxMessage::new("room:lobby", "phx_join", serde_json::json!({}))
            .with_ref("5")
            .with_join_ref("3");
        let reply = PhxMessage::reply(&req, "ok", serde_json::json!({"joined": true}));
        assert_eq!(reply.event, events::PHX_REPLY);
        assert_eq!(reply.payload["status"], "ok");
    }
}
