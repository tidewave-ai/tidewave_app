//! Upload channel for receiving binary chunks over WebSocket.
//!
//! Topics: `upload:<ref>` where `<ref>` is a client-chosen identifier.
//!
//! Join payload: `{"path": "/absolute/path/to/file.ext"}`
//! Client sends `"chunk"` events with `Payload::Binary` data.
//! Client sends `"done"` event with `{}` payload; reply contains `{path, size}`.

use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error};

use crate::phoenix::{InitResult, Payload, PhxMessage};

#[derive(Deserialize)]
struct JoinPayload {
    path: String,
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

    let file_path = PathBuf::from(&join_payload.path);

    // Create parent directories if needed
    if let Some(parent) = file_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return InitResult::Error(format!("Failed to create parent directories: {}", e));
        }
    }

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

    debug!("Upload channel opened: {}", file_path.display());

    // Send success reply
    let _ = outgoing_tx.send(PhxMessage::ok_reply(
        msg,
        json!({"path": join_payload.path}),
    ));

    let mut size: u64 = 0;

    // Main loop: receive chunks and "done"
    loop {
        match incoming_rx.recv().await {
            Some(phx_msg) => match phx_msg.event.as_str() {
                "chunk" => {
                    if let Payload::Binary(data) = phx_msg.payload {
                        size += data.len() as u64;
                        if let Err(e) = file.write_all(&data).await {
                            error!("Upload write error: {}", e);
                            return InitResult::Shutdown(format!("Write error: {}", e));
                        }
                    }
                }
                "done" => {
                    if let Err(e) = file.flush().await {
                        error!("Upload flush error: {}", e);
                        let _ = outgoing_tx.send(PhxMessage::error_reply(
                            &phx_msg,
                            format!("Flush error: {}", e),
                        ));
                    } else {
                        let _ = outgoing_tx.send(PhxMessage::ok_reply(
                            &phx_msg,
                            json!({
                                "path": file_path.to_string_lossy().into_owned(),
                                "size": size,
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
