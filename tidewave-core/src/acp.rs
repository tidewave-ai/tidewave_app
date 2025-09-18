use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query,
    },
    http::StatusCode,
    response::Response,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::Deserialize;
use std::process::Stdio;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tracing::{debug, error, info};

#[derive(Deserialize)]
pub struct AcpParams {
    command: String,
}

pub async fn acp_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<AcpParams>,
) -> Result<Response, StatusCode> {
    if params.command.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    info!(
        "ACP WebSocket connection requested for command: {}",
        params.command
    );

    Ok(ws.on_upgrade(move |socket| handle_socket(socket, params.command)))
}

async fn handle_socket(socket: WebSocket, command: String) {
    debug!("WebSocket connection established for command: {}", command);

    // Parse the command into program and arguments
    let mut parts = command.split_whitespace();
    let program = match parts.next() {
        Some(p) => p,
        None => {
            error!("Invalid command: {}", command);
            return;
        }
    };
    let args: Vec<&str> = parts.collect();

    // Spawn the command as a subprocess
    let mut child = match Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            error!("Failed to spawn command '{}': {}", command, e);
            return;
        }
    };

    info!("Started subprocess: {}", command);

    let stdin = child.stdin.take().expect("Failed to get stdin");
    let stdout = child.stdout.take().expect("Failed to get stdout");
    let stderr = child.stderr.take().expect("Failed to get stderr");

    let (ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let (process_exit_tx, mut process_exit_rx) = mpsc::unbounded_channel::<()>();

    // Task to read from stdout and send to WebSocket
    let tx_stdout = tx.clone();
    let exit_tx_stdout = process_exit_tx.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            if let Err(e) = tx_stdout.send(Message::Text(line.clone().into())) {
                error!("Failed to send stdout to channel: {}", e);
                break;
            }
            line.clear();
        }
        debug!("Stdout reader finished");
        let _ = exit_tx_stdout.send(());
    });

    // Task to read from stderr and log as errors
    let exit_tx_stderr = process_exit_tx.clone();
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                error!("Process stderr: {}", trimmed);
            }
            line.clear();
        }
        debug!("Stderr reader finished");
        let _ = exit_tx_stderr.send(());
    });

    // Task to send messages to WebSocket and handle process exit
    let mut ws_sender = ws_sender;
    let sender_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(message) => {
                            if ws_sender.send(message).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = process_exit_rx.recv() => {
                    debug!("Process exited, closing WebSocket");
                    let _ = ws_sender.send(Message::Close(None)).await;
                    break;
                }
            }
        }
        debug!("WebSocket sender finished");
    });

    // Task to monitor the process and signal when it exits
    let exit_tx_process = process_exit_tx.clone();
    let process_monitor_task = tokio::spawn(async move {
        let exit_status = child.wait().await;
        match exit_status {
            Ok(status) => {
                if !status.success() {
                    error!("Process exited with status: {}", status);
                } else {
                    info!("Process exited successfully");
                }
            }
            Err(e) => {
                error!("Failed to wait for process: {}", e);
            }
        }
        let _ = exit_tx_process.send(());
        debug!("Process monitor finished");
    });

    // Task to receive messages from WebSocket and write to stdin
    let mut stdin = stdin;
    let receiver_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if text == "ping" {
                        if tx.send(Message::Text("pong".into())).is_err() {
                            break;
                        }
                    } else {
                        // Write to subprocess stdin
                        let mut data = text.to_string();
                        if !data.ends_with('\n') {
                            data.push('\n');
                        }
                        if let Err(e) = stdin.write_all(data.as_bytes()).await {
                            error!("Failed to write to stdin: {}", e);
                            break;
                        }
                        if let Err(e) = stdin.flush().await {
                            error!("Failed to flush stdin: {}", e);
                            break;
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("WebSocket closed by client");
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
        debug!("WebSocket receiver finished");
    });

    // Wait for all tasks to complete
    let _ = tokio::join!(
        stdout_task,
        stderr_task,
        sender_task,
        receiver_task,
        process_monitor_task
    );

    info!("ACP WebSocket connection closed for command: {}", command);
}
