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
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tracing::{debug, warn};

use crate::phoenix::{InitResult, PhxMessage};
use crate::utils::normalize_path;

// ============================================================================
// Types
// ============================================================================

/// File system event (server → client)
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum FsEvent {
    Created { path: String },
    Modified { path: String },
    Deleted { path: String },
    Renamed { from: String, to: String },
    Warning { message: String },
}

/// Server message (server → client)
#[derive(Clone, Debug)]
pub enum WatchEvent {
    FS(FsEvent),
    /// Internal signal: watcher died or watched path was removed.
    Terminated {
        error: String,
    },
}

impl FsEvent {
    /// Convert into a Phoenix push message on the given topic.
    fn into_phx(self, topic: &str, join_ref: &Option<String>) -> PhxMessage {
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
}

impl WatchFeatureState {
    pub fn new() -> Self {
        Self {
            watchers: Arc::new(DashMap::new()),
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

/// Initialize a `watch:<ref>` channel.
///
/// On success, sends the ok reply via `outgoing_tx` and runs the forwarding
/// loop until the channel is left (incoming_rx closed) or the watcher errors out.
/// Returns `Err(reason)` if the join fails; the caller sends the error reply.
///
/// Intended to be called inside a spawned task.
pub async fn init(
    state: &WatchFeatureState,
    msg: &PhxMessage,
    outgoing_tx: UnboundedSender<PhxMessage>,
    mut incoming_rx: tokio::sync::mpsc::UnboundedReceiver<PhxMessage>,
) -> InitResult {
    let payload: JoinPayload = match serde_json::from_value(msg.payload.clone()) {
        Ok(p) => p,
        Err(e) => return InitResult::Error(e.to_string()),
    };

    // Normalize path (handles WSL path conversion on Windows)
    let normalized_path = match normalize_path(&payload.path, payload.is_wsl).await {
        Ok(p) => p,
        Err(e) => return InitResult::Error(format!("Failed to normalize path: {}", e)),
    };

    // Validate path is absolute
    if !Path::new(&normalized_path).is_absolute() {
        return InitResult::Error("Path must be absolute".to_string());
    }

    // Check if path exists
    let path_obj = Path::new(&normalized_path);
    if !path_obj.exists() {
        return InitResult::Error("Path does not exist".to_string());
    }

    // Get canonical path for deduplication
    let canonical_path = match path_obj.canonicalize() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(e) => return InitResult::Error(format!("Failed to canonicalize path: {}", e)),
    };

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

    // Subscribe to the broadcast channel
    let mut broadcast_rx = active_watch.tx.subscribe();

    // Atomically check if we should start the watcher
    let should_start_watcher = active_watch
        .started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();

    if should_start_watcher {
        init_watcher(&state, &active_watch, &canonical_path, payload.is_wsl);
    }

    // Send success reply
    let _ = outgoing_tx.send(PhxMessage::ok_reply(
        msg,
        serde_json::json!({ "path": canonical_path }),
    ));

    // Run forwarding loop until channel exits
    let topic = &msg.topic;
    let join_ref = &msg.join_ref;
    loop {
        tokio::select! {
            result = broadcast_rx.recv() => {
                match result {
                    Ok(WatchEvent::Terminated { error }) => {
                        return InitResult::Shutdown(error);
                    }
                    Ok(WatchEvent::FS(event)) => {
                        let phx = event.into_phx(topic, join_ref);
                        let _ = outgoing_tx.send(phx);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return InitResult::Shutdown("watcher closed".to_string());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            // When incoming_rx is closed (sender dropped), the channel was left/disconnected
            msg = incoming_rx.recv() => {
                if msg.is_none() {
                    return InitResult::Done;
                }
            }
        }
    }
}

/// Spawn the filesystem watcher task for a canonical path.
/// Broadcasts `WatchEvent`s to all subscribers via the `ActiveWatch` broadcast channel.
/// Cleans itself up from `state.watch.watchers` when done.
fn init_watcher(
    state: &WatchFeatureState,
    active_watch: &Arc<ActiveWatch>,
    canonical_path: &str,
    is_wsl: bool,
) {
    let tx = active_watch.tx.clone();
    let canonical_path = canonical_path.to_string();
    let state = state.clone();

    // On Windows with WSL, use poll watcher directly since native watcher
    // doesn't work well with WSL paths
    #[cfg(target_os = "windows")]
    let use_poll_watcher = is_wsl;
    #[cfg(not(target_os = "windows"))]
    let use_poll_watcher = {
        let _ = is_wsl;
        false
    };

    tokio::spawn(async move {
        // Create a channel to receive notify events
        let (notify_tx, mut notify_rx) =
            tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(256);

        // Helper to create poll watcher
        let create_poll_watcher = |notify_tx: tokio::sync::mpsc::Sender<
            notify::Result<notify::Event>,
        >| {
            let poll_config = notify::Config::default().with_poll_interval(Duration::from_secs(2));
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
                    let _ = tx.send(WatchEvent::Terminated {
                        error: format!("Failed to create poll watcher: {}", e),
                    });
                    state.watchers.remove(&canonical_path);
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
                        canonical_path, e
                    );

                    match create_poll_watcher(notify_tx.clone()) {
                        Ok(w) => {
                            // Send warning to client about poll watcher fallback
                            let _ = tx.send(WatchEvent::FS(FsEvent::Warning {
                                message: format!(
                                    "Using poll-based file watching (native watcher unavailable: {}). \
                                     File change detection may be slower.",
                                    e
                                ),
                            }));
                            Box::new(w)
                        }
                        Err(poll_err) => {
                            let _ = tx.send(WatchEvent::Terminated {
                                error: format!(
                                    "Failed to create watcher: {} (poll fallback also failed: {})",
                                    e, poll_err
                                ),
                            });
                            state.watchers.remove(&canonical_path);
                            return;
                        }
                    }
                }
            }
        };

        if let Err(e) = watcher.watch(Path::new(&canonical_path), RecursiveMode::Recursive) {
            let _ = tx.send(WatchEvent::Terminated {
                error: format!("Failed to watch path: {}", e),
            });
            state.watchers.remove(&canonical_path);
            return;
        }

        let mut pending_rename_from: Option<String> = None;
        let cleanup_check_interval = Duration::from_secs(30);

        loop {
            tokio::select! {
                res = notify_rx.recv() => {
                    match res {
                        Some(Ok(event)) => {
                            let watch_events = convert_notify_event(
                                event,
                                &canonical_path,
                                &mut pending_rename_from,
                            );

                            for watch_event in watch_events {
                                let is_terminated = matches!(&watch_event, WatchEvent::Terminated { .. });

                                let _ = tx.send(watch_event);

                                if is_terminated {
                                    state.watchers.remove(&canonical_path);
                                    return;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let _ = tx.send(WatchEvent::Terminated {
                                error: format!("Watch error: {}", e),
                            });
                            state.watchers.remove(&canonical_path);
                            return;
                        }
                        None => {
                            state.watchers.remove(&canonical_path);
                            return;
                        }
                    }
                }
                _ = tokio::time::sleep(cleanup_check_interval) => {
                    if tx.receiver_count() == 0 {
                        debug!("No subscribers remaining for watch on {}, cleaning up", canonical_path);
                        state.watchers.remove(&canonical_path);
                        return;
                    }
                }
            }
        }
    });
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
                    results.push(WatchEvent::FS(FsEvent::Created {
                        path: relative_path,
                    }));
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
                                results.push(WatchEvent::FS(FsEvent::Renamed {
                                    from: from_relative,
                                    to: to_relative,
                                }));
                            } else {
                                // No pending from, or from was outside watched dir - treat as create
                                results
                                    .push(WatchEvent::FS(FsEvent::Created { path: to_relative }));
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
                                results.push(WatchEvent::FS(FsEvent::Renamed { from, to }));
                            }
                            (Some(from), None) => {
                                // Renamed out of watched directory - treat as delete
                                results.push(WatchEvent::FS(FsEvent::Deleted { path: from }));
                            }
                            (None, Some(to)) => {
                                // Renamed into watched directory - treat as create
                                results.push(WatchEvent::FS(FsEvent::Created { path: to }));
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
                            results.push(WatchEvent::FS(FsEvent::Modified {
                                path: relative_path,
                            }));
                        }
                    }
                }
            },
            _ => {
                for path in event.paths {
                    if let Some(relative_path) = to_relative_path(&path, watched_path) {
                        results.push(WatchEvent::FS(FsEvent::Modified {
                            path: relative_path,
                        }));
                    }
                }
            }
        },
        EventKind::Remove(_) => {
            for path in &event.paths {
                let path_str = path.to_string_lossy().to_string();

                if path_str == watched_path {
                    // Watched directory was removed
                    results.push(WatchEvent::Terminated {
                        error: "watched path was removed".to_string(),
                    });
                } else if let Some(relative_path) = to_relative_path(path, watched_path) {
                    results.push(WatchEvent::FS(FsEvent::Deleted {
                        path: relative_path,
                    }));
                }
            }
        }
        _ => {}
    }

    results
}
