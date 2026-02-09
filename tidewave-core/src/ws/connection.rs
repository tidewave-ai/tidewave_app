//! WebSocket connection management using Phoenix V2 wire format.

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
            let (reply, channel_tx) = dispatch_join(state, &msg, outgoing_tx.clone()).await;
            let _ = outgoing_tx.send(reply);
            if let Some(tx) = channel_tx {
                channels.insert(msg.topic.clone(), tx);
            }
        }
        events::PHX_LEAVE => {
            // Drop the channel handle to signal cleanup
            channels.remove(&msg.topic);
            let reply = PhxMessage::reply(&msg, "ok", serde_json::json!({}));
            let _ = outgoing_tx.send(reply);
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
async fn dispatch_join(
    state: &WsState,
    msg: &PhxMessage,
    outgoing_tx: UnboundedSender<PhxMessage>,
) -> (PhxMessage, Option<UnboundedSender<PhxMessage>>) {
    if msg.topic.starts_with("watch:") {
        super::watch::handle_join(state, msg, outgoing_tx).await
    } else {
        (
            PhxMessage::reply(
                msg,
                "error",
                serde_json::json!({"reason": "unknown topic"}),
            ),
            None,
        )
    }
}
