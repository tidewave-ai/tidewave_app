//! Recording channel for receiving binary media chunks over WebSocket.
//!
//! Topics: `recording:<ref>` where `<ref>` is a client-chosen identifier.
//!
//! Join payload: `{"name": "filename.webm"}`
//! Client sends `"chunk"` events with `Payload::Binary` data.
//! Client sends `"done"` event with `{}` payload; reply contains `{path}`.

use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error};

use crate::phoenix::{InitResult, Payload, PhxMessage};
use crate::utils::recordings_dir;

#[derive(Deserialize)]
struct JoinPayload {
    name: String,
}

pub async fn init(
    msg: &PhxMessage,
    outgoing_tx: UnboundedSender<PhxMessage>,
    mut incoming_rx: tokio::sync::mpsc::UnboundedReceiver<PhxMessage>,
) -> InitResult {
    let join_payload: JoinPayload = match serde_json::from_value(msg.payload.clone().into_json()) {
        Ok(p) => p,
        Err(e) => return InitResult::Error(format!("Invalid join payload: {}", e)),
    };

    let name = &join_payload.name;

    // Validate name to prevent path traversal
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return InitResult::Error(format!("Invalid name: {}", name));
    }

    let recordings_dir = recordings_dir();

    if let Err(e) = tokio::fs::create_dir_all(&recordings_dir).await {
        return InitResult::Error(format!("Failed to create recordings dir: {}", e));
    }

    let file_path = recordings_dir.join(name);

    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&file_path)
        .await
    {
        Ok(f) => f,
        Err(e) => return InitResult::Error(format!("Failed to open file: {}", e)),
    };

    debug!("Recording channel opened: {}", file_path.display());

    // Send success reply with the filename
    let _ = outgoing_tx.send(PhxMessage::ok_reply(msg, json!({"name": name})));

    // Main loop: receive chunks and "done"
    loop {
        match incoming_rx.recv().await {
            Some(phx_msg) => match phx_msg.event.as_str() {
                "chunk" => {
                    if let Payload::Binary(data) = phx_msg.payload {
                        if let Err(e) = file.write_all(&data).await {
                            error!("Recording write error: {}", e);
                            return InitResult::Shutdown(format!("Write error: {}", e));
                        }
                    }
                }
                "done" => {
                    if let Err(e) = file.flush().await {
                        error!("Recording flush error: {}", e);
                        let _ = outgoing_tx.send(PhxMessage::error_reply(
                            &phx_msg,
                            format!("Flush error: {}", e),
                        ));
                    } else {
                        // Drop the file handle before remuxing
                        drop(file);
                        // remux_with_ffmpeg(&file_path).await;

                        let _ = outgoing_tx.send(PhxMessage::ok_reply(
                            &phx_msg,
                            json!({
                                "path": file_path.display().to_string(),
                            }),
                        ));
                    }
                    break;
                }
                _ => {}
            },
            None => break,
        }
    }

    InitResult::Done
}
