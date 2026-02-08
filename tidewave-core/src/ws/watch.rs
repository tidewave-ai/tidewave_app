//! File system watch feature for WebSocket using Phoenix channel protocol.
//!
//! Topics: `watch:<ref>` where `<ref>` is a client-chosen identifier.
//!
//! On error (watcher died, watched path removed), a `phx_error` event is sent
//! which causes Phoenix clients to automatically attempt to rejoin.
//!
//! Event paths (created, modified, deleted, renamed) are relative to the watched directory.

use dashmap::DashMap;
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
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::phoenix::PhxMessage;
use crate::utils::normalize_path;

use super::{WebSocketId, WsState};

// ============================================================================
// Types
// ============================================================================

/// Server message (server → client)
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum WatchEvent {
    Created {
        path: String,
    },
    Modified {
        path: String,
    },
    Deleted {
        path: String,
    },
    Renamed {
        from: String,
        to: String,
    },
    Warning {
        message: String,
    },
    /// Internal signal: watcher died or watched path was removed.
    /// Converted to `phx_error` in `into_phx` to trigger client rejoin.
    Unsubscribed {
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

impl WatchEvent {
    /// Convert into a Phoenix push message on the given topic.
    fn into_phx(self, topic: &str, join_ref: &Option<String>) -> PhxMessage {
        if let WatchEvent::Unsubscribed { error, .. } = &self {
            let payload = match error {
                Some(reason) => serde_json::json!({"reason": reason}),
                None => serde_json::json!({}),
            };
            let mut phx = PhxMessage::new(topic, crate::phoenix::events::PHX_ERROR, payload);
            phx.join_ref = join_ref.clone();
            return phx;
        }

        let mut val = serde_json::to_value(&self).unwrap_or_default();
        let event_name = val
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        if let Some(map) = val.as_object_mut() {
            map.remove("event");
        }
        let mut phx = PhxMessage::new(topic, event_name, val);
        phx.join_ref = join_ref.clone();
        phx
    }
}

/// Active watcher for a directory
pub struct ActiveWatch {
    pub tx: broadcast::Sender<WatchEvent>,
    /// Flag to ensure only one task starts the watcher
    pub started: AtomicBool,
}

/// Watch feature state
#[derive(Clone, Default)]
pub struct WatchFeatureState {
    /// Active watchers (canonical_path → active_watch)
    pub watchers: Arc<DashMap<String, Arc<ActiveWatch>>>,
    /// Subscriptions per WebSocket (websocket_id → (topic → canonical_path))
    pub subscriptions: Arc<DashMap<WebSocketId, HashMap<String, String>>>,
}

impl WatchFeatureState {
    pub fn new() -> Self {
        Self {
            watchers: Arc::new(DashMap::new()),
            subscriptions: Arc::new(DashMap::new()),
        }
    }
}

// ============================================================================
// Message Handler
// ============================================================================

#[derive(Deserialize)]
struct JoinPayload {
    path: String,
    #[serde(default)]
    is_wsl: bool,
}

/// Handle a phx_join on a `watch:<ref>` topic.
pub async fn handle_join(
    state: &WsState,
    websocket_id: WebSocketId,
    msg: &PhxMessage,
) -> PhxMessage {
    let payload: JoinPayload = match serde_json::from_value(msg.payload.clone()) {
        Ok(p) => p,
        Err(e) => {
            return PhxMessage::reply(msg, "error", serde_json::json!({ "reason": e.to_string() }));
        }
    };

    match handle_subscribe(
        state,
        websocket_id,
        &payload.path,
        payload.is_wsl,
        msg.topic.clone(),
        msg.join_ref.clone(),
    )
    .await
    {
        Ok(canonical_path) => {
            PhxMessage::reply(msg, "ok", serde_json::json!({ "path": canonical_path }))
        }
        Err(reason) => PhxMessage::reply(msg, "error", serde_json::json!({ "reason": reason })),
    }
}

/// Handle a phx_leave on a `watch:<ref>` topic.
pub async fn handle_leave(
    state: &WsState,
    websocket_id: WebSocketId,
    msg: &PhxMessage,
) -> PhxMessage {
    handle_unsubscribe(state, websocket_id, msg.topic.clone()).await;
    PhxMessage::reply(msg, "ok", serde_json::json!({}))
}

/// Subscribe to file watching. Returns Ok(canonical_path) on success.
async fn handle_subscribe(
    state: &WsState,
    websocket_id: WebSocketId,
    path: &str,
    is_wsl: bool,
    topic: String,
    join_ref: Option<String>,
) -> Result<String, String> {
    // Normalize path (handles WSL path conversion on Windows)
    let normalized_path = normalize_path(path, is_wsl)
        .await
        .map_err(|e| format!("Failed to normalize path: {}", e))?;

    // Validate path is absolute
    if !Path::new(&normalized_path).is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    // Check if path exists
    let path_obj = Path::new(&normalized_path);
    if !path_obj.exists() {
        return Err("Path does not exist".to_string());
    }

    // Get canonical path for deduplication
    let canonical_path = path_obj
        .canonicalize()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| format!("Failed to canonicalize path: {}", e))?;

    // Add to this websocket's subscriptions (topic -> canonical_path)
    if let Some(mut subs) = state.watch.subscriptions.get_mut(&websocket_id) {
        subs.insert(topic.clone(), canonical_path.clone());
    }

    // Get or create active watch entry (atomic via entry API)
    let active_watch = state
        .watch
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
    let topic_for_forward = topic.clone();

    // Spawn task to forward events to this websocket
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let is_unsubscribed = matches!(&event, WatchEvent::Unsubscribed { .. });

                    // Check if this websocket is still subscribed with this ref
                    let still_subscribed = state_for_forward
                        .watch
                        .subscriptions
                        .get(&websocket_id)
                        .map(|subs| {
                            subs.get(&topic_for_forward) == Some(&canonical_path_for_forward)
                        })
                        .unwrap_or(false);

                    if still_subscribed {
                        let phx = event.into_phx(&topic, &join_ref);
                        if let Some(tx) = state_for_forward.websocket_senders.get(&websocket_id) {
                            let _ = tx.send(phx);
                        }
                    }

                    if is_unsubscribed || !still_subscribed {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    // Watcher died - notify client so they can resubscribe if needed
                    let phx = WatchEvent::Unsubscribed {
                        error: Some("watcher closed".to_string()),
                    }
                    .into_phx(&topic, &join_ref);
                    if let Some(tx) = state_for_forward.websocket_senders.get(&websocket_id) {
                        let _ = tx.send(phx);
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

        // On Windows with WSL, use poll watcher directly since native watcher
        // doesn't work well with WSL paths
        #[cfg(target_os = "windows")]
        let use_poll_watcher = is_wsl;
        #[cfg(not(target_os = "windows"))]
        let use_poll_watcher = false;

        tokio::spawn(async move {
            // Create a channel to receive notify events
            let (notify_tx, mut notify_rx) =
                tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(256);

            // Helper to create poll watcher
            let create_poll_watcher =
                |notify_tx: tokio::sync::mpsc::Sender<notify::Result<notify::Event>>| {
                    let poll_config =
                        notify::Config::default().with_poll_interval(Duration::from_secs(2));
                    PollWatcher::new(
                        move |res| {
                            let _ = notify_tx.blocking_send(res);
                        },
                        poll_config,
                    )
                };

            // Box the watcher to allow different types
            let mut watcher: Box<dyn Watcher + Send> = if use_poll_watcher {
                // Use poll watcher directly (e.g., for WSL on Windows)
                match create_poll_watcher(notify_tx.clone()) {
                    Ok(w) => Box::new(w),
                    Err(e) => {
                        let _ = tx.send(WatchEvent::Unsubscribed {
                            error: Some(format!("Failed to create poll watcher: {}", e)),
                        });
                        state_for_task
                            .watch
                            .watchers
                            .remove(&canonical_path_for_task);
                        return;
                    }
                }
            } else {
                // Try native watcher first, fall back to poll watcher if it fails
                let notify_tx_clone = notify_tx.clone();
                let watcher_result = RecommendedWatcher::new(
                    move |res| {
                        let _ = notify_tx_clone.blocking_send(res);
                    },
                    notify::Config::default(),
                );

                match watcher_result {
                    Ok(w) => Box::new(w),
                    Err(e) => {
                        // Native watcher failed, try poll watcher as fallback
                        warn!(
                            "Native file watcher failed for {}: {}. Falling back to poll watcher.",
                            canonical_path_for_task, e
                        );

                        match create_poll_watcher(notify_tx.clone()) {
                            Ok(w) => {
                                // Send warning to client about poll watcher fallback
                                let _ = tx.send(WatchEvent::Warning {
                                    message: format!(
                                        "Using poll-based file watching (native watcher unavailable: {}). \
                                         File change detection may be slower.",
                                        e
                                    ),
                                });
                                Box::new(w)
                            }
                            Err(poll_err) => {
                                let _ = tx.send(WatchEvent::Unsubscribed {
                                    error: Some(format!(
                                        "Failed to create watcher: {} (poll fallback also failed: {})",
                                        e, poll_err
                                    )),
                                });
                                state_for_task
                                    .watch
                                    .watchers
                                    .remove(&canonical_path_for_task);
                                return;
                            }
                        }
                    }
                }
            };

            if let Err(e) = watcher.watch(
                Path::new(&canonical_path_for_task),
                RecursiveMode::Recursive,
            ) {
                let _ = tx.send(WatchEvent::Unsubscribed {
                    error: Some(format!("Failed to watch path: {}", e)),
                });
                state_for_task
                    .watch
                    .watchers
                    .remove(&canonical_path_for_task);
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
                                        state_for_task.watch.watchers.remove(&canonical_path_for_task);
                                        return;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                let _ = tx.send(WatchEvent::Unsubscribed {
                                    error: Some(format!("Watch error: {}", e)),
                                });
                                state_for_task.watch.watchers.remove(&canonical_path_for_task);
                                return;
                            }
                            None => {
                                state_for_task.watch.watchers.remove(&canonical_path_for_task);
                                return;
                            }
                        }
                    }
                    // Periodic subscriber check for cleanup (no event sent to clients)
                    _ = tokio::time::sleep(cleanup_check_interval) => {
                        if tx.receiver_count() == 0 {
                            debug!("No subscribers remaining for watch on {}, cleaning up", canonical_path_for_task);
                            state_for_task.watch.watchers.remove(&canonical_path_for_task);
                            return;
                        }
                    }
                }
            }
        });
    }

    Ok(canonical_path)
}

async fn handle_unsubscribe(state: &WsState, websocket_id: WebSocketId, topic: String) {
    // Remove from this websocket's subscriptions
    if let Some(mut subs) = state.watch.subscriptions.get_mut(&websocket_id) {
        subs.remove(&topic);
    }
}

/// Convert an absolute path to a relative path (relative to watched_path).
/// Returns None if the path is not under watched_path.
fn to_relative_path(absolute_path: &Path, watched_path: &str) -> Option<String> {
    let watched = Path::new(watched_path);
    absolute_path
        .strip_prefix(watched)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
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
                if let Some(relative_path) = to_relative_path(&path, watched_path) {
                    results.push(WatchEvent::Created {
                        path: relative_path,
                    });
                }
            }
        }
        EventKind::Modify(modify_kind) => match modify_kind {
            notify::event::ModifyKind::Name(rename_mode) => match rename_mode {
                notify::event::RenameMode::From => {
                    if let Some(path) = event.paths.first() {
                        // Store the relative path for rename, or None if not relative
                        *pending_rename_from = to_relative_path(path, watched_path);
                    }
                }
                notify::event::RenameMode::To => {
                    if let Some(to_path) = event.paths.first() {
                        if let Some(to_relative) = to_relative_path(to_path, watched_path) {
                            if let Some(from_relative) = pending_rename_from.take() {
                                results.push(WatchEvent::Renamed {
                                    from: from_relative,
                                    to: to_relative,
                                });
                            } else {
                                // No pending from, or from was outside watched dir - treat as create
                                results.push(WatchEvent::Created { path: to_relative });
                            }
                        } else {
                            // to_path is outside watched dir, clear any pending rename
                            pending_rename_from.take();
                        }
                    }
                }
                notify::event::RenameMode::Both => {
                    if event.paths.len() >= 2 {
                        let from_relative = to_relative_path(&event.paths[0], watched_path);
                        let to_relative = to_relative_path(&event.paths[1], watched_path);
                        match (from_relative, to_relative) {
                            (Some(from), Some(to)) => {
                                results.push(WatchEvent::Renamed { from, to });
                            }
                            (Some(from), None) => {
                                // Renamed out of watched directory - treat as delete
                                results.push(WatchEvent::Deleted { path: from });
                            }
                            (None, Some(to)) => {
                                // Renamed into watched directory - treat as create
                                results.push(WatchEvent::Created { path: to });
                            }
                            (None, None) => {
                                // Both outside watched dir - ignore
                            }
                        }
                    }
                }
                _ => {
                    for path in event.paths {
                        if let Some(relative_path) = to_relative_path(&path, watched_path) {
                            results.push(WatchEvent::Modified {
                                path: relative_path,
                            });
                        }
                    }
                }
            },
            _ => {
                for path in event.paths {
                    if let Some(relative_path) = to_relative_path(&path, watched_path) {
                        results.push(WatchEvent::Modified {
                            path: relative_path,
                        });
                    }
                }
            }
        },
        EventKind::Remove(_) => {
            for path in &event.paths {
                let path_str = path.to_string_lossy().to_string();

                if path_str == watched_path {
                    // Watched directory was removed - send unsubscribed
                    results.push(WatchEvent::Unsubscribed {
                        error: Some("watched path was removed".to_string()),
                    });
                } else if let Some(relative_path) = to_relative_path(path, watched_path) {
                    results.push(WatchEvent::Deleted {
                        path: relative_path,
                    });
                }
            }
        }
        _ => {}
    }

    results
}
