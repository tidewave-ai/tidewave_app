//! File system watch handler using WebSocket for bidirectional communication.
//!
//! # Protocol
//!
//! Client → Server:
//! ```json
//! {"action": "subscribe", "path": "/foo/bar", "ref": "watch1"}
//! {"action": "unsubscribe", "ref": "watch1"}
//! ```
//!
//! Server → Client:
//! ```json
//! {"event": "subscribed", "ref": "watch1"}
//! {"event": "unsubscribed", "ref": "watch1"}
//! {"event": "unsubscribed", "ref": "watch1", "error": "..."}
//! {"event": "created", "path": "/foo/bar/file.txt", "ref": "watch1"}
//! {"event": "modified", "path": "/foo/bar/file.txt", "ref": "watch1"}
//! {"event": "deleted", "path": "/foo/bar/file.txt", "ref": "watch1"}
//! {"event": "renamed", "from": "/foo/bar/old.txt", "to": "/foo/bar/new.txt", "ref": "watch1"}
//! {"event": "warning", "message": "...", "ref": "watch1"}
//! ```

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use dashmap::DashMap;
use futures::{Sink, SinkExt, Stream, StreamExt};
use notify::{EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::utils::normalize_path;
use uuid::Uuid;

// ============================================================================
// Types
// ============================================================================

pub type WebSocketId = Uuid;

/// Client message (client → server)
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum WatchClientMessage {
    Subscribe {
        path: String,
        #[serde(rename = "ref")]
        reference: String,
        #[serde(default)]
        is_wsl: bool,
    },
    Unsubscribe {
        #[serde(rename = "ref")]
        reference: String,
    },
}

/// Server message (server → client)
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum WatchEvent {
    Created {
        path: String,
        #[serde(rename = "ref")]
        reference: String,
    },
    Modified {
        path: String,
        #[serde(rename = "ref")]
        reference: String,
    },
    Deleted {
        path: String,
        #[serde(rename = "ref")]
        reference: String,
    },
    Renamed {
        from: String,
        to: String,
        #[serde(rename = "ref")]
        reference: String,
    },
    Warning {
        message: String,
        #[serde(rename = "ref")]
        reference: String,
    },
    Subscribed {
        #[serde(rename = "ref")]
        reference: String,
    },
    Unsubscribed {
        #[serde(rename = "ref")]
        reference: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl WatchEvent {
    /// Create a new event with a different reference
    fn with_reference(&self, new_ref: String) -> Self {
        match self {
            WatchEvent::Created { path, .. } => WatchEvent::Created {
                path: path.clone(),
                reference: new_ref,
            },
            WatchEvent::Modified { path, .. } => WatchEvent::Modified {
                path: path.clone(),
                reference: new_ref,
            },
            WatchEvent::Deleted { path, .. } => WatchEvent::Deleted {
                path: path.clone(),
                reference: new_ref,
            },
            WatchEvent::Renamed { from, to, .. } => WatchEvent::Renamed {
                from: from.clone(),
                to: to.clone(),
                reference: new_ref,
            },
            WatchEvent::Warning { message, .. } => WatchEvent::Warning {
                message: message.clone(),
                reference: new_ref,
            },
            WatchEvent::Subscribed { .. } => WatchEvent::Subscribed { reference: new_ref },
            WatchEvent::Unsubscribed { error, .. } => WatchEvent::Unsubscribed {
                reference: new_ref,
                error: error.clone(),
            },
        }
    }
}

/// Internal message for WebSocket communication
#[derive(Debug, Clone)]
pub enum WatchWebSocketMessage {
    Event(WatchEvent),
    Pong,
}

/// Active watcher for a directory
pub struct ActiveWatch {
    pub tx: broadcast::Sender<WatchEvent>,
    /// Flag to ensure only one task starts the watcher
    pub started: AtomicBool,
}

/// Global watch state
#[derive(Clone, Default)]
pub struct WatchState {
    /// Active watchers (canonical_path → active_watch)
    pub watchers: Arc<DashMap<String, Arc<ActiveWatch>>>,
    /// WebSocket connections (websocket_id → sender)
    pub websocket_senders: Arc<DashMap<WebSocketId, mpsc::UnboundedSender<WatchWebSocketMessage>>>,
    /// Subscriptions per WebSocket (websocket_id → (ref → canonical_path))
    pub subscriptions: Arc<DashMap<WebSocketId, HashMap<String, String>>>,
}

impl WatchState {
    pub fn new() -> Self {
        Self {
            watchers: Arc::new(DashMap::new()),
            websocket_senders: Arc::new(DashMap::new()),
            subscriptions: Arc::new(DashMap::new()),
        }
    }
}

// ============================================================================
// WebSocket Handler
// ============================================================================

pub async fn watch_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<WatchState>,
) -> Result<impl IntoResponse, StatusCode> {
    let websocket_id = Uuid::new_v4();
    debug!("New watch WebSocket connection: {}", websocket_id);

    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state, websocket_id)))
}

async fn handle_websocket(socket: WebSocket, state: WatchState, websocket_id: WebSocketId) {
    let (ws_sender, ws_receiver) = socket.split();
    unit_testable_ws_handler(ws_sender, ws_receiver, state, websocket_id).await;
}

pub async fn unit_testable_ws_handler<W, R>(
    mut ws_sender: W,
    mut ws_receiver: R,
    state: WatchState,
    websocket_id: WebSocketId,
) where
    W: Sink<Message> + Unpin + Send + 'static,
    R: Stream<Item = Result<Message, axum::Error>> + Unpin + Send + 'static,
{
    // Create channel for sending messages to this WebSocket
    let (tx, mut rx) = mpsc::unbounded_channel::<WatchWebSocketMessage>();

    // Register WebSocket sender and initialize empty subscriptions
    state.websocket_senders.insert(websocket_id, tx.clone());
    state.subscriptions.insert(websocket_id, HashMap::new());

    // Task to handle outgoing messages (server → client)
    let websocket_id_tx = websocket_id;
    let tx_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let ws_message = match message {
                WatchWebSocketMessage::Event(event) => match serde_json::to_string(&event) {
                    Ok(json_str) => Message::Text(format!("{}\n", json_str).into()),
                    Err(_) => continue,
                },
                WatchWebSocketMessage::Pong => Message::Text("pong".into()),
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
                    if text.trim() == "ping" {
                        if let Some(tx) = state_rx.websocket_senders.get(&websocket_id) {
                            let _ = tx.send(WatchWebSocketMessage::Pong);
                        }
                    } else if let Ok(client_msg) =
                        serde_json::from_str::<WatchClientMessage>(&text)
                    {
                        handle_client_message(&state_rx, websocket_id, client_msg).await;
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("Watch WebSocket closed for: {}", websocket_id);
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    debug!("Watch WebSocket error for {}: {}", websocket_id, e);
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
    state.subscriptions.remove(&websocket_id);
}

async fn handle_client_message(
    state: &WatchState,
    websocket_id: WebSocketId,
    message: WatchClientMessage,
) {
    match message {
        WatchClientMessage::Subscribe { path, reference, is_wsl } => {
            handle_subscribe(state, websocket_id, &path, &reference, is_wsl).await;
        }
        WatchClientMessage::Unsubscribe { reference } => {
            handle_unsubscribe(state, websocket_id, &reference).await;
        }
    }
}

async fn handle_subscribe(state: &WatchState, websocket_id: WebSocketId, path: &str, reference: &str, is_wsl: bool) {
    // Normalize path (handles WSL path conversion on Windows)
    let normalized_path = match normalize_path(path, is_wsl).await {
        Ok(p) => p,
        Err(e) => {
            send_unsubscribed_with_error(state, websocket_id, reference, &format!("Failed to normalize path: {}", e));
            return;
        }
    };

    // Validate path is absolute
    if !Path::new(&normalized_path).is_absolute() {
        send_unsubscribed_with_error(state, websocket_id, reference, "Path must be absolute");
        return;
    }

    // Check if path exists
    let path_obj = Path::new(&normalized_path);
    if !path_obj.exists() {
        send_unsubscribed_with_error(state, websocket_id, reference, "Path does not exist");
        return;
    }

    // Get canonical path for deduplication
    let canonical_path = match path_obj.canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => {
            send_unsubscribed_with_error(state, websocket_id, reference, &format!("Failed to canonicalize path: {}", e));
            return;
        }
    };

    // Add to this websocket's subscriptions (ref -> canonical_path)
    if let Some(mut subs) = state.subscriptions.get_mut(&websocket_id) {
        subs.insert(reference.to_string(), canonical_path.clone());
    }

    // Get or create active watch entry (atomic via entry API)
    let active_watch = state
        .watchers
        .entry(canonical_path.clone())
        .or_insert_with(|| {
            let (tx, _rx) = broadcast::channel::<WatchEvent>(256);
            Arc::new(ActiveWatch {
                tx,
                started: AtomicBool::new(false),
            })
        })
        .clone();

    // Subscribe this websocket to the watch events
    let mut rx = active_watch.tx.subscribe();
    let state_for_forward = state.clone();
    let canonical_path_for_forward = canonical_path.clone();
    let reference_for_forward = reference.to_string();

    // Spawn task to forward events to this websocket
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let is_unsubscribed = matches!(&event, WatchEvent::Unsubscribed { .. });

                    // Check if this websocket is still subscribed with this ref
                    let still_subscribed = state_for_forward
                        .subscriptions
                        .get(&websocket_id)
                        .map(|subs| subs.get(&reference_for_forward) == Some(&canonical_path_for_forward))
                        .unwrap_or(false);

                    if still_subscribed {
                        // Transform event to use client's ref instead of canonical path
                        let client_event = event.with_reference(reference_for_forward.clone());
                        if let Some(tx) = state_for_forward.websocket_senders.get(&websocket_id) {
                            let _ = tx.send(WatchWebSocketMessage::Event(client_event));
                        }
                    }

                    if is_unsubscribed || !still_subscribed {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Watcher died - notify client so they can resubscribe if needed
                    if let Some(tx) = state_for_forward.websocket_senders.get(&websocket_id) {
                        let _ = tx.send(WatchWebSocketMessage::Event(WatchEvent::Unsubscribed {
                            reference: reference_for_forward.clone(),
                            error: Some("watcher closed".to_string()),
                        }));
                    }
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Atomically check if we should start the watcher
    let should_start_watcher = active_watch
        .started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();

    if should_start_watcher {
        let tx = active_watch.tx.clone();
        let canonical_path_for_task = canonical_path.clone();
        let state_for_task = state.clone();

        tokio::spawn(async move {
            // Create a channel to receive notify events
            let (notify_tx, mut notify_rx) =
                tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(256);

            // Try to create the native watcher first, fall back to poll watcher if it fails
            let notify_tx_clone = notify_tx.clone();
            let watcher_result = RecommendedWatcher::new(
                move |res| {
                    let _ = notify_tx_clone.blocking_send(res);
                },
                notify::Config::default(),
            );

            // Box the watcher to allow different types
            let mut watcher: Box<dyn Watcher + Send> = match watcher_result {
                Ok(w) => Box::new(w),
                Err(e) => {
                    // Native watcher failed, try poll watcher as fallback
                    warn!(
                        "Native file watcher failed for {}: {}. Falling back to poll watcher.",
                        canonical_path_for_task, e
                    );

                    let notify_tx_poll = notify_tx.clone();
                    let poll_config = notify::Config::default()
                        .with_poll_interval(Duration::from_secs(2));

                    match PollWatcher::new(
                        move |res| {
                            let _ = notify_tx_poll.blocking_send(res);
                        },
                        poll_config,
                    ) {
                        Ok(w) => {
                            // Send warning to client about poll watcher fallback
                            let _ = tx.send(WatchEvent::Warning {
                                message: format!(
                                    "Using poll-based file watching (native watcher unavailable: {}). \
                                     File change detection may be slower.",
                                    e
                                ),
                                reference: canonical_path_for_task.clone(),
                            });
                            Box::new(w)
                        }
                        Err(poll_err) => {
                            let _ = tx.send(WatchEvent::Unsubscribed {
                                reference: canonical_path_for_task.clone(),
                                error: Some(format!(
                                    "Failed to create watcher: {} (poll fallback also failed: {})",
                                    e, poll_err
                                )),
                            });
                            state_for_task.watchers.remove(&canonical_path_for_task);
                            return;
                        }
                    }
                }
            };

            if let Err(e) =
                watcher.watch(Path::new(&canonical_path_for_task), RecursiveMode::Recursive)
            {
                let _ = tx.send(WatchEvent::Unsubscribed {
                    reference: canonical_path_for_task.clone(),
                    error: Some(format!("Failed to watch path: {}", e)),
                });
                state_for_task.watchers.remove(&canonical_path_for_task);
                return;
            }

            let mut pending_rename_from: Option<String> = None;
            let cleanup_check_interval = Duration::from_secs(30);

            loop {
                tokio::select! {
                    // Handle notify events
                    res = notify_rx.recv() => {
                        match res {
                            Some(Ok(event)) => {
                                let watch_events = convert_notify_event(
                                    event,
                                    &canonical_path_for_task,
                                    &mut pending_rename_from,
                                );

                                for watch_event in watch_events {
                                    let is_unsubscribed = matches!(&watch_event, WatchEvent::Unsubscribed { .. });

                                    let _ = tx.send(watch_event);

                                    if is_unsubscribed {
                                        state_for_task.watchers.remove(&canonical_path_for_task);
                                        return;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                let _ = tx.send(WatchEvent::Unsubscribed {
                                    reference: canonical_path_for_task.clone(),
                                    error: Some(format!("Watch error: {}", e)),
                                });
                                state_for_task.watchers.remove(&canonical_path_for_task);
                                return;
                            }
                            None => {
                                state_for_task.watchers.remove(&canonical_path_for_task);
                                return;
                            }
                        }
                    }
                    // Periodic subscriber check for cleanup (no event sent to clients)
                    _ = tokio::time::sleep(cleanup_check_interval) => {
                        if tx.receiver_count() == 0 {
                            debug!("No subscribers remaining for watch on {}, cleaning up", canonical_path_for_task);
                            state_for_task.watchers.remove(&canonical_path_for_task);
                            return;
                        }
                    }
                }
            }
        });
    }

    // Send subscribed confirmation
    if let Some(tx) = state.websocket_senders.get(&websocket_id) {
        let _ = tx.send(WatchWebSocketMessage::Event(WatchEvent::Subscribed {
            reference: reference.to_string(),
        }));
    }
}

async fn handle_unsubscribe(state: &WatchState, websocket_id: WebSocketId, reference: &str) {
    // Remove from this websocket's subscriptions
    if let Some(mut subs) = state.subscriptions.get_mut(&websocket_id) {
        subs.remove(reference);
    }

    // Send unsubscribed confirmation
    if let Some(tx) = state.websocket_senders.get(&websocket_id) {
        let _ = tx.send(WatchWebSocketMessage::Event(WatchEvent::Unsubscribed {
            reference: reference.to_string(),
            error: None,
        }));
    }
}

fn send_unsubscribed_with_error(
    state: &WatchState,
    websocket_id: WebSocketId,
    reference: &str,
    message: &str,
) {
    if let Some(tx) = state.websocket_senders.get(&websocket_id) {
        let _ = tx.send(WatchWebSocketMessage::Event(WatchEvent::Unsubscribed {
            reference: reference.to_string(),
            error: Some(message.to_string()),
        }));
    }
}

fn convert_notify_event(
    event: notify::Event,
    watched_path: &str,
    pending_rename_from: &mut Option<String>,
) -> Vec<WatchEvent> {
    let mut results = Vec::new();

    match event.kind {
        EventKind::Create(_) => {
            for path in event.paths {
                results.push(WatchEvent::Created {
                    path: path.to_string_lossy().to_string(),
                    reference: watched_path.to_string(),
                });
            }
        }
        EventKind::Modify(modify_kind) => match modify_kind {
            notify::event::ModifyKind::Name(rename_mode) => match rename_mode {
                notify::event::RenameMode::From => {
                    if let Some(path) = event.paths.first() {
                        *pending_rename_from = Some(path.to_string_lossy().to_string());
                    }
                }
                notify::event::RenameMode::To => {
                    if let Some(to_path) = event.paths.first() {
                        if let Some(from_path) = pending_rename_from.take() {
                            results.push(WatchEvent::Renamed {
                                from: from_path,
                                to: to_path.to_string_lossy().to_string(),
                                reference: watched_path.to_string(),
                            });
                        } else {
                            results.push(WatchEvent::Created {
                                path: to_path.to_string_lossy().to_string(),
                                reference: watched_path.to_string(),
                            });
                        }
                    }
                }
                notify::event::RenameMode::Both => {
                    if event.paths.len() >= 2 {
                        results.push(WatchEvent::Renamed {
                            from: event.paths[0].to_string_lossy().to_string(),
                            to: event.paths[1].to_string_lossy().to_string(),
                            reference: watched_path.to_string(),
                        });
                    }
                }
                _ => {
                    for path in event.paths {
                        results.push(WatchEvent::Modified {
                            path: path.to_string_lossy().to_string(),
                            reference: watched_path.to_string(),
                        });
                    }
                }
            },
            _ => {
                for path in event.paths {
                    results.push(WatchEvent::Modified {
                        path: path.to_string_lossy().to_string(),
                        reference: watched_path.to_string(),
                    });
                }
            }
        },
        EventKind::Remove(_) => {
            for path in &event.paths {
                let path_str = path.to_string_lossy().to_string();

                if path_str == watched_path {
                    // Watched directory was removed - send unsubscribed
                    results.push(WatchEvent::Unsubscribed {
                        reference: watched_path.to_string(),
                        error: Some("watched path was removed".to_string()),
                    });
                } else {
                    results.push(WatchEvent::Deleted {
                        path: path_str,
                        reference: watched_path.to_string(),
                    });
                }
            }
        }
        _ => {}
    }

    results
}
