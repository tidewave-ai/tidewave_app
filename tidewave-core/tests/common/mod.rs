use axum::extract::ws::Message;
use futures::channel::mpsc as futures_mpsc;
use futures::StreamExt;
use std::time::Duration;
use tidewave_core::phoenix::PhxMessage;

/// Creates fake Phoenix socket streams for testing.
///
/// Returns (out_tx, out_rx, in_tx, in_rx):
/// - `out_tx` / `in_rx` go to `unit_testable_ws_handler` as ws_sender / ws_receiver
/// - `in_tx` / `out_rx` are used by the test to send/receive Phoenix messages
pub fn create_fake_phoenix_socket() -> (
    futures_mpsc::UnboundedSender<Message>,
    futures_mpsc::UnboundedReceiver<Message>,
    futures_mpsc::UnboundedSender<Result<Message, axum::Error>>,
    futures_mpsc::UnboundedReceiver<Result<Message, axum::Error>>,
) {
    let (out_tx, out_rx) = futures_mpsc::unbounded();
    let (in_tx, in_rx) = futures_mpsc::unbounded();
    (out_tx, out_rx, in_tx, in_rx)
}

/// Send a Phoenix message
pub fn send_phoenix_msg(
    tx: &futures_mpsc::UnboundedSender<Result<Message, axum::Error>>,
    msg: &PhxMessage,
) {
    tx.unbounded_send(Ok(Message::Text(msg.clone().encode().into())))
        .unwrap();
}

/// Receive the next Phoenix message (no timeout, no filtering)
#[allow(dead_code)]
pub async fn recv_phoenix_msg(
    rx: &mut futures_mpsc::UnboundedReceiver<Message>,
) -> Option<PhxMessage> {
    let msg = rx.next().await?;
    match msg {
        Message::Text(text) => PhxMessage::decode(&text).ok(),
        _ => None,
    }
}

/// Wait for a specific event type with timeout
#[allow(dead_code)]
pub async fn wait_for_event(
    rx: &mut futures_mpsc::UnboundedReceiver<Message>,
    event_type: &str,
    timeout_ms: u64,
) -> Option<PhxMessage> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }

        match tokio::time::timeout(remaining, rx.next()).await {
            Ok(Some(msg)) => {
                if let Some(phx) = match msg {
                    Message::Text(text) => PhxMessage::decode(&text).ok(),
                    _ => None,
                } {
                    if phx.event == event_type {
                        return Some(phx);
                    }
                    if phx.event == "phx_reply" {
                        continue;
                    }
                }
            }
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}

/// Wait for a phx_reply message
#[allow(dead_code)]
pub async fn wait_for_reply(
    rx: &mut futures_mpsc::UnboundedReceiver<Message>,
    timeout_ms: u64,
) -> Option<PhxMessage> {
    wait_for_event(rx, "phx_reply", timeout_ms).await
}
