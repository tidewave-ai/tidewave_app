use futures::channel::mpsc as futures_mpsc;
use futures::StreamExt;
use std::time::Duration;
use tidewave_core::phoenix::PhxMessage;

/// Creates fake Phoenix socket streams for testing
pub fn create_fake_phoenix_socket() -> (
    futures_mpsc::UnboundedSender<String>,
    futures_mpsc::UnboundedReceiver<String>,
    futures_mpsc::UnboundedSender<Result<String, ()>>,
    futures_mpsc::UnboundedReceiver<Result<String, ()>>,
) {
    let (out_tx, out_rx) = futures_mpsc::unbounded();
    let (in_tx, in_rx) = futures_mpsc::unbounded();
    (out_tx, out_rx, in_tx, in_rx)
}

/// Send a Phoenix message
pub fn send_phoenix_msg(tx: &futures_mpsc::UnboundedSender<Result<String, ()>>, msg: &PhxMessage) {
    tx.unbounded_send(Ok(msg.encode())).unwrap();
}

/// Receive the next Phoenix message (no timeout, no filtering)
#[allow(dead_code)]
pub async fn recv_phoenix_msg(
    rx: &mut futures_mpsc::UnboundedReceiver<String>,
) -> Option<PhxMessage> {
    let text = rx.next().await?;
    PhxMessage::decode(&text).ok()
}

/// Wait for a specific event type with timeout
#[allow(dead_code)]
pub async fn wait_for_event(
    rx: &mut futures_mpsc::UnboundedReceiver<String>,
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
            Ok(Some(text)) => {
                if let Ok(msg) = PhxMessage::decode(&text) {
                    if msg.event == event_type {
                        return Some(msg);
                    }
                    // For phx_reply, check if it's our event
                    if msg.event == "phx_reply" {
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
    rx: &mut futures_mpsc::UnboundedReceiver<String>,
    timeout_ms: u64,
) -> Option<PhxMessage> {
    wait_for_event(rx, "phx_reply", timeout_ms).await
}
