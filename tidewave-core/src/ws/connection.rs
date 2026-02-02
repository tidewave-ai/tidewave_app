//! WebSocket connection management.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use super::{WatchClientMessage, WebSocketId, WsOutboundMessage, WsState};

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
    // Create channel for sending messages to this WebSocket
    let (tx, mut rx) = mpsc::unbounded_channel::<WsOutboundMessage>();

    // Register WebSocket sender and initialize empty subscriptions
    state.websocket_senders.insert(websocket_id, tx.clone());
    state
        .watch
        .subscriptions
        .insert(websocket_id, std::collections::HashMap::new());

    // Task to handle outgoing messages (server → client)
    let websocket_id_tx = websocket_id;
    let tx_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let ws_message = serialize_outbound(message);
            let Some(ws_message) = ws_message else {
                continue;
            };

            if ws_sender.send(ws_message).await.is_err() {
                debug!("WebSocket send failed for: {}", websocket_id_tx);
                break;
            }
        }
    });

    // Task to handle incoming messages (client → server)
    let state_rx = state.clone();
    let rx_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    handle_incoming_message(&state_rx, websocket_id, &text).await;
                }
                Ok(Message::Close(_)) => {
                    debug!("WebSocket closed for: {}", websocket_id);
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!("WebSocket error for {}: {}", websocket_id, e);
                    break;
                }
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = tx_task => {},
        _ = rx_task => {},
    }

    // Cleanup on disconnect
    state.websocket_senders.remove(&websocket_id);
    state.watch.subscriptions.remove(&websocket_id);
}

async fn handle_incoming_message(state: &WsState, websocket_id: WebSocketId, text: &str) {
    // Handle ping first
    if text.trim() == "ping" {
        if let Some(tx) = state.websocket_senders.get(&websocket_id) {
            let _ = tx.send(WsOutboundMessage::Pong);
        }
        return;
    }

    // Try to deserialize as WatchClientMessage
    if let Ok(watch_msg) = serde_json::from_str::<WatchClientMessage>(text) {
        super::watch::handle_watch_message(state, websocket_id, watch_msg).await;
        return;
    }

    // Future: try other message types (Terminal, etc.)
}

fn serialize_outbound(message: WsOutboundMessage) -> Option<Message> {
    match message {
        WsOutboundMessage::Watch(event) => match serde_json::to_string(&event) {
            Ok(json_str) => Some(Message::Text(format!("{}\n", json_str).into())),
            Err(_) => None,
        },
        WsOutboundMessage::Pong => Some(Message::Text("pong".into())),
    }
}
