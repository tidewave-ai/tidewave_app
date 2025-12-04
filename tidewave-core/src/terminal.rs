use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

/// WebSocket upgrade handler for terminal connections
pub async fn terminal_ws_handler(ws: WebSocketUpgrade) -> Result<impl IntoResponse, StatusCode> {
    Ok(ws.on_upgrade(handle_terminal_socket))
}

/// Handle WebSocket connection for terminal
async fn handle_terminal_socket(socket: WebSocket) {
    info!("New terminal WebSocket connection established");

    // Split the WebSocket into sender and receiver
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Spawn PTY process
    let pty_system = native_pty_system();

    let pty_pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(e) => {
            error!("Failed to create PTY: {}", e);
            let error_msg = format!("Error: Failed to create terminal: {}\r\n", e);
            let _ = ws_sender.send(Message::Text(error_msg.into())).await;
            return;
        }
    };

    // Determine shell command based on platform
    #[cfg(unix)]
    let shell_cmd = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    #[cfg(windows)]
    let shell_cmd = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());

    let mut cmd = CommandBuilder::new(&shell_cmd);
    cmd.env("TERM", "xterm-256color");

    // Spawn child process
    let mut child = match pty_pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(e) => {
            error!("Failed to spawn shell: {}", e);
            let error_msg = format!("Error: Failed to spawn shell: {}\r\n", e);
            let _ = ws_sender.send(Message::Text(error_msg.into())).await;
            return;
        }
    };

    // Get reader and writer from PTY master
    let pty_reader = Arc::new(Mutex::new(pty_pair.master.try_clone_reader().unwrap()));
    let pty_writer = Arc::new(Mutex::new(pty_pair.master.take_writer().unwrap()));

    info!("Shell process spawned successfully");

    // Create channels for coordinating shutdown
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let shutdown_tx_clone = shutdown_tx.clone();

    // Task 1: Read from PTY and send to WebSocket
    let reader_clone = pty_reader.clone();
    let pty_to_ws = tokio::spawn(async move {
        loop {
            // Read from PTY in blocking IO (spawn_blocking for non-blocking async)
            let reader = reader_clone.clone();
            let read_result = tokio::task::spawn_blocking(move || {
                let mut buffer = [0u8; 8192];
                let mut reader = reader.lock().unwrap();
                match reader.read(&mut buffer) {
                    Ok(n) => Ok((n, buffer)),
                    Err(e) => Err(e),
                }
            })
            .await;

            match read_result {
                Ok(Ok((n, buffer))) if n > 0 => {
                    let data = &buffer[..n];
                    debug!("Read {} bytes from PTY", n);

                    // Send to WebSocket as binary data
                    if let Err(e) = ws_sender
                        .send(Message::Binary(Bytes::copy_from_slice(data)))
                        .await
                    {
                        warn!("Failed to send to WebSocket: {}", e);
                        break;
                    }
                }
                Ok(Ok(_)) => {
                    // EOF - child process exited
                    info!("PTY reached EOF");
                    break;
                }
                Ok(Err(e)) => {
                    error!("Error reading from PTY: {}", e);
                    break;
                }
                Err(e) => {
                    error!("Task join error: {}", e);
                    break;
                }
            }
        }
        debug!("PTY-to-WS task exiting");
        let _ = shutdown_tx.send(()).await;
    });

    // Task 2: Read from WebSocket and write to PTY
    let writer_clone = pty_writer.clone();
    let ws_to_pty = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    debug!("Received text from WebSocket: {} bytes", text.len());

                    // Write to PTY
                    let writer = writer_clone.clone();
                    let text = text.to_string();
                    if let Err(e) = tokio::task::spawn_blocking(move || {
                        let mut writer = writer.lock().unwrap();
                        writer.write_all(text.as_bytes())
                    })
                    .await
                    {
                        error!("Failed to write to PTY: {}", e);
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    debug!("Received binary from WebSocket: {} bytes", data.len());

                    // Write to PTY
                    let writer = writer_clone.clone();
                    if let Err(e) = tokio::task::spawn_blocking(move || {
                        let mut writer = writer.lock().unwrap();
                        writer.write_all(&data)
                    })
                    .await
                    {
                        error!("Failed to write to PTY: {}", e);
                        break;
                    }
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket close message received");
                    break;
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    // Axum handles ping/pong automatically
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
            }
        }
        debug!("WS-to-PTY task exiting");
        let _ = shutdown_tx_clone.send(()).await;
    });

    // Wait for either task to complete (which means connection is done)
    tokio::select! {
        _ = shutdown_rx.recv() => {
            info!("Shutdown signal received");
        }
    }

    // Clean up: kill child process
    match child.wait() {
        Ok(status) => {
            info!("Child process exited with status: {:?}", status);
        }
        Err(e) => {
            warn!("Error waiting for child process: {}", e);
            // Try to kill it
            let _ = child.kill();
        }
    }

    // Abort tasks
    pty_to_ws.abort();
    ws_to_pty.abort();

    info!("Terminal WebSocket connection closed");
}
