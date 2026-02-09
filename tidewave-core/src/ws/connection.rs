//! WebSocket connection management using Phoenix V2 wire format.
//!
//! # Channel abstraction
//!
//! Each Phoenix topic (e.g. `watch:<ref>`) is handled by a **channel** — an async
//! function spawned in its own task when the client sends `phx_join`. A channel
//! receives four arguments:
//!
//! - `state: &WsState` — shared server state.
//! - `msg: &PhxMessage` — the original `phx_join` message (contains topic, join_ref,
//!   and the join payload sent by the client).
//! - `outgoing_tx: UnboundedSender<PhxMessage>` — send messages to the client.
//!   The channel should use this to send the `ok_reply` for the join and any
//!   subsequent push events.
//! - `incoming_rx: UnboundedReceiver<PhxMessage>` — receive messages from the client
//!   for this topic. When the client sends `phx_leave` or disconnects, this channel
//!   closes (`recv()` returns `None`), signaling the handler to exit.
//!
//! The return type is `Result<(), String>`:
//!
//! - `Ok(())` — clean exit. The connection sends `phx_close` to the client.
//! - `Err(reason)` — error. The connection sends `phx_reply` with error status.
//!   Use this for join validation failures (bad path, missing params) by returning
//!   early with `?` or `return Err(...)` before entering the main loop.
//!
//! To add a new channel, add a match arm in [`dispatch_join`] for your topic prefix.
//! See [`super::watch::init`] for a complete example.

use std::collections::HashMap;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::debug;
use uuid::Uuid;

use super::{WebSocketId, WsState};
use crate::phoenix::{events, PhxMessage};

// ============================================================================
// WebSocket Handler
// ============================================================================

struct ChannelExit {
    topic: String,
    join_ref: Option<String>,
    panicked: bool,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsState>,
) -> Result<impl IntoResponse, StatusCode> {
    let websocket_id = Uuid::new_v4();
    debug!("New WebSocket connection: {}", websocket_id);

    Ok(ws.on_upgrade(move |socket| handle_connection(socket, state, websocket_id)))
}

async fn handle_connection(socket: WebSocket, state: WsState, websocket_id: WebSocketId) {
    let (ws_sender, ws_receiver) = socket.split();
    unit_testable_ws_handler(ws_sender, ws_receiver, state, websocket_id).await;
}

pub async fn unit_testable_ws_handler<W, R>(
    mut ws_sender: W,
    mut ws_receiver: R,
    state: WsState,
    websocket_id: WebSocketId,
) where
    W: Sink<Message> + Unpin + Send + 'static,
    R: Stream<Item = Result<Message, axum::Error>> + Unpin + Send + 'static,
{
    // Channel for sending messages to this WebSocket (shared with channel handlers)
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<PhxMessage>();

    // Task to forward outgoing messages to the WebSocket sink
    let websocket_id_tx = websocket_id;
    let mut tx_task = tokio::spawn(async move {
        while let Some(phx) = outgoing_rx.recv().await {
            let ws_message = Message::Text(phx.encode().into());
            if ws_sender.send(ws_message).await.is_err() {
                debug!("WebSocket send failed for: {}", websocket_id_tx);
                break;
            }
        }
    });

    // Local map of joined topics → channel handle (incoming sender)
    let mut channels: HashMap<String, UnboundedSender<PhxMessage>> = HashMap::new();

    // Tracks spawned channel tasks; detects unexpected exits (panics).
    let mut channel_tasks = tokio::task::JoinSet::<ChannelExit>::new();

    // Receive loop runs directly; select on tx_task to detect send failures
    loop {
        tokio::select! {
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_incoming_message(
                            &state,
                            &outgoing_tx,
                            &mut channels,
                            &mut channel_tasks,
                            &text,
                        ).await;
                    }
                    Some(Ok(Message::Close(_))) => {
                        debug!("WebSocket closed for: {}", websocket_id);
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!("WebSocket error for {}: {}", websocket_id, e);
                        break;
                    }
                    None => break,
                }
            }
            Some(Ok(exit)) = channel_tasks.join_next() => {
                if channels.remove(&exit.topic).is_some() && exit.panicked {
                    let _ = outgoing_tx.send(PhxMessage::error(exit.topic, exit.join_ref, "channel exited unexpectedly"));
                }
            }
            _ = &mut tx_task => {
                // Outgoing task ended (WebSocket sink closed)
                break;
            }
        }
    }

    // Cleanup: dropping `channels` closes all incoming senders,
    // signaling each channel handler to exit.
    drop(channels);
}

async fn handle_incoming_message(
    state: &WsState,
    outgoing_tx: &UnboundedSender<PhxMessage>,
    channels: &mut HashMap<String, UnboundedSender<PhxMessage>>,
    channel_tasks: &mut tokio::task::JoinSet<ChannelExit>,
    text: &str,
) {
    let msg = match PhxMessage::decode(text) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Handle heartbeat
    if msg.topic == "phoenix" && msg.event == events::HEARTBEAT {
        let _ = outgoing_tx.send(PhxMessage::heartbeat_reply(&msg));
        return;
    }

    match msg.event.as_str() {
        events::PHX_JOIN => {
            let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
            let topic = msg.topic.clone();
            let join_ref = msg.join_ref.clone();
            if let Some(handle) = dispatch_join(state, msg, outgoing_tx.clone(), incoming_rx) {
                channels.insert(topic.clone(), incoming_tx);
                channel_tasks.spawn(async move {
                    let panicked = handle.await.is_err();
                    ChannelExit { topic, join_ref, panicked }
                });
            }
        }
        events::PHX_LEAVE => {
            // Drop the channel handle to signal cleanup
            channels.remove(&msg.topic);
            let _ = outgoing_tx.send(PhxMessage::ok_reply(&msg, serde_json::json!({})));
        }
        _ => {
            // Forward to the channel handler if joined
            if let Some(tx) = channels.get(&msg.topic) {
                let _ = tx.send(msg);
            }
        }
    }
}

/// Dispatch a phx_join to the appropriate channel handler based on topic prefix.
/// Spawns the channel handler task and returns its JoinHandle, or None for unknown topics.
fn dispatch_join(
    state: &WsState,
    msg: PhxMessage,
    outgoing_tx: UnboundedSender<PhxMessage>,
    incoming_rx: mpsc::UnboundedReceiver<PhxMessage>,
) -> Option<tokio::task::JoinHandle<()>> {
    if msg.topic.starts_with("watch:") {
        let state = state.clone();
        Some(tokio::spawn(async move {
            match super::watch::init(&state, &msg, outgoing_tx.clone(), incoming_rx).await {
                Ok(()) => {
                    let _ = outgoing_tx.send(PhxMessage::close(msg.topic, msg.join_ref));
                }
                Err(reason) => {
                    let _ = outgoing_tx.send(PhxMessage::error(msg.topic, msg.join_ref, reason));
                }
            }
        }))
    } else {
        let _ = outgoing_tx.send(PhxMessage::error(msg.topic, msg.join_ref, "unknown topic"));
        None
    }
}
