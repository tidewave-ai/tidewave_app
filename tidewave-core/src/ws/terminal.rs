//! Interactive terminal (PTY) feature for WebSocket using Phoenix channel protocol.
//!
//! Topics: `terminal:<ref>` where `<ref>` is a client-chosen identifier.
//!
//! Events:
//! - `input`  (client → server): `{ "data": "<string>" }` — keyboard input
//! - `resize` (client → server): `{ "cols": 80, "rows": 24 }` — terminal resize
//! - `output` (server → client): `{ "data": "<string>" }` — terminal output
//! - `exit`   (server → client): `{ "code": <i64> }` — process exited
//!
//! ## Future: Multi-client & Persistence
//!
//! Currently each join spawns a new PTY and disconnect kills it. The code is
//! structured so that persistence (PTY outlives a single channel, keyed by
//! terminal ID in shared state) and multi-client support can be added later.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;
use std::io::{Read, Write};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use crate::phoenix::{InitResult, PhxMessage};

// ============================================================================
// Types
// ============================================================================

#[derive(Deserialize)]
struct ResizePayload {
    cols: u16,
    rows: u16,
}

#[derive(Deserialize)]
struct InputPayload {
    data: String,
}

#[derive(Deserialize)]
struct JoinPayload {
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

// ============================================================================
// Channel Handler
// ============================================================================

/// Initialize a `terminal:<ref>` channel.
///
/// Spawns a PTY with the user's default shell, wires input/output/resize,
/// and runs until the process exits or the client disconnects.
pub async fn init(
    msg: &PhxMessage,
    outgoing_tx: UnboundedSender<PhxMessage>,
    mut incoming_rx: tokio::sync::mpsc::UnboundedReceiver<PhxMessage>,
) -> InitResult {
    let topic = msg.topic.clone();
    let join_ref = msg.join_ref.clone();

    let join_payload: JoinPayload = match serde_json::from_value(msg.payload.clone()) {
        Ok(p) => p,
        Err(e) => return InitResult::Error(format!("Invalid join payload: {}", e)),
    };

    // Open PTY
    let pty_system = native_pty_system();
    let initial_size = PtySize {
        rows: join_payload.rows,
        cols: join_payload.cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = match pty_system.openpty(initial_size) {
        Ok(pair) => pair,
        Err(e) => return InitResult::Error(format!("Failed to open PTY: {}", e)),
    };

    // Spawn default shell
    let cmd = CommandBuilder::new_default_prog();
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(e) => return InitResult::Error(format!("Failed to spawn shell: {}", e)),
    };

    // Drop slave — we only need the master side
    drop(pair.slave);

    // Get reader and writer from master
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => return InitResult::Error(format!("Failed to clone PTY reader: {}", e)),
    };

    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => return InitResult::Error(format!("Failed to take PTY writer: {}", e)),
    };

    // Send success reply
    let _ = outgoing_tx.send(PhxMessage::ok_reply(msg, serde_json::json!({})));

    // Reader task: blocking reads from PTY → async channel
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let reader_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    debug!("PTY read error: {}", e);
                    break;
                }
            }
        }
    });

    // Main event loop
    loop {
        tokio::select! {
            // PTY output → client
            data = output_rx.recv() => {
                match data {
                    Some(bytes) => {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        let mut phx = PhxMessage::new(
                            &topic,
                            "output",
                            serde_json::json!({ "data": text }),
                        );
                        phx.join_ref = join_ref.clone();
                        let _ = outgoing_tx.send(phx);
                    }
                    None => {
                        // Reader ended — process exited
                        let exit_code = match child.try_wait() {
                            Ok(Some(status)) => {
                                if status.success() { 0 } else { 1 }
                            }
                            _ => {
                                // Wait for the child to actually exit
                                match child.wait() {
                                    Ok(status) => if status.success() { 0 } else { 1 },
                                    Err(_) => 1,
                                }
                            }
                        };
                        let mut phx = PhxMessage::new(
                            &topic,
                            "exit",
                            serde_json::json!({ "code": exit_code }),
                        );
                        phx.join_ref = join_ref.clone();
                        let _ = outgoing_tx.send(phx);
                        return InitResult::Done;
                    }
                }
            }

            // Client messages → PTY
            msg = incoming_rx.recv() => {
                match msg {
                    Some(msg) => {
                        match msg.event.as_str() {
                            "input" => {
                                if let Ok(payload) = serde_json::from_value::<InputPayload>(msg.payload) {
                                    if let Err(e) = writer.write_all(payload.data.as_bytes()) {
                                        warn!("PTY write error: {}", e);
                                    }
                                }
                            }
                            "resize" => {
                                if let Ok(payload) = serde_json::from_value::<ResizePayload>(msg.payload) {
                                    let size = PtySize {
                                        rows: payload.rows,
                                        cols: payload.cols,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    };
                                    if let Err(e) = pair.master.resize(size) {
                                        warn!("PTY resize error: {}", e);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    None => {
                        // Client disconnected — kill the process
                        debug!("Terminal channel closed, killing PTY process");
                        let _ = child.kill();
                        reader_handle.abort();
                        return InitResult::Done;
                    }
                }
            }
        }
    }
}
