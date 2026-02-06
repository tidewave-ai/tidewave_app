//! File system watching using Phoenix channels.
//!
//! This module provides a Phoenix channel implementation for file watching.
//! Topic format: `watch:/path/to/directory`
//!
//! # Events
//!
//! Server -> Client:
//! - `created` - File created: `{"path": "relative/path"}`
//! - `modified` - File modified: `{"path": "relative/path"}`
//! - `deleted` - File deleted: `{"path": "relative/path"}`
//! - `renamed` - File renamed: `{"from": "old/path", "to": "new/path"}`
//! - `warning` - Non-fatal warning: `{"message": "..."}`
//! - `error` - Fatal error: `{"reason": "..."}`
//!
//! Note: Event paths are relative to the watched directory.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use notify::{EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::phoenix::{Channel, HandleResult, JoinResult, SocketRef};
use crate::utils::normalize_path;

// ============================================================================
// Event Types
// ============================================================================

/// Watch events broadcast to all subscribers of a path
#[derive(Clone, Debug)]
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
    /// Watcher stopped (path removed or error)
    Stopped {
        error: Option<String>,
    },
}

// ============================================================================
// Shared State
// ============================================================================

/// Active watcher for a directory
pub struct ActiveWatch {
    pub tx: broadcast::Sender<WatchEvent>,
    /// Flag to ensure only one task starts the watcher
    pub started: AtomicBool,
    /// Signals when the watcher is fully active (Some(Ok(()))) or failed (Some(Err(reason)))
    pub ready: tokio::sync::watch::Sender<Option<Result<(), String>>>,
}

/// Watch feature state shared across all channel instances
#[derive(Clone, Default)]
pub struct WatchChannelState {
    /// Active watchers (canonical_path → active_watch)
    pub watchers: Arc<DashMap<String, Arc<ActiveWatch>>>,
    /// Canonical path per channel instance (socket.unique_id → canonical_path)
    pub canonical_paths: Arc<DashMap<uuid::Uuid, String>>,
}

impl WatchChannelState {
    pub fn new() -> Self {
        Self {
            watchers: Arc::new(DashMap::new()),
            canonical_paths: Arc::new(DashMap::new()),
        }
    }
}

// ============================================================================
// Phoenix Channel Implementation
// ============================================================================

/// Phoenix channel for file system watching.
///
/// Topic format: `watch:/path/to/directory`
/// Optional join payload: `{"is_wsl": true}` for WSL path normalization
pub struct WatchChannel {
    state: WatchChannelState,
}

impl WatchChannel {
    pub fn new() -> Self {
        Self {
            state: WatchChannelState::new(),
        }
    }

    pub fn with_state(state: WatchChannelState) -> Self {
        Self { state }
    }

    pub fn state(&self) -> WatchChannelState {
        self.state.clone()
    }
}

impl Default for WatchChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for WatchChannel {
    async fn join(&self, topic: &str, payload: Value, socket: &mut SocketRef) -> JoinResult {
        // Extract path from topic (strip "watch:" prefix)
        let path = match topic.strip_prefix("watch:") {
            Some(p) if !p.is_empty() => p,
            _ => {
                return JoinResult::error(json!({
                    "reason": "invalid topic format, expected 'watch:/path/to/dir'"
                }));
            }
        };

        // Check for WSL path normalization flag
        let is_wsl = payload
            .get("is_wsl")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Normalize path (handles WSL path conversion on Windows)
        let normalized_path = match normalize_path(path, is_wsl).await {
            Ok(p) => p,
            Err(e) => {
                return JoinResult::error(json!({
                    "reason": format!("failed to normalize path: {}", e)
                }));
            }
        };

        // Validate path is absolute
        let path_obj = Path::new(&normalized_path);
        if !path_obj.is_absolute() {
            return JoinResult::error(json!({
                "reason": "path must be absolute"
            }));
        }

        // Check if path exists
        if !path_obj.exists() {
            return JoinResult::error(json!({
                "reason": "path does not exist"
            }));
        }

        // Get canonical path for deduplication
        let canonical_path = match path_obj.canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(e) => {
                return JoinResult::error(json!({
                    "reason": format!("failed to canonicalize path: {}", e)
                }));
            }
        };

        // Store the canonical path for this channel instance
        self.state
            .canonical_paths
            .insert(socket.unique_id, canonical_path.clone());

        // Get or create active watch entry (atomic via entry API)
        let active_watch = self
            .state
            .watchers
            .entry(canonical_path.clone())
            .or_insert_with(|| {
                let (tx, _rx) = broadcast::channel::<WatchEvent>(256);
                let (ready, _) = tokio::sync::watch::channel(None);
                Arc::new(ActiveWatch {
                    tx,
                    started: AtomicBool::new(false),
                    ready,
                })
            })
            .clone();

        // Subscribe this socket to the watch events
        let mut rx = active_watch.tx.subscribe();
        let socket = socket.clone();

        // Spawn task to forward events to this socket
        tokio::spawn(async move {
            let shutdown = socket.shutdown_token();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        break;
                    }
                    result = rx.recv() => {
                        match result {
                            Ok(event) => {
                                let is_stopped = matches!(&event, WatchEvent::Stopped { .. });
                                push_watch_event(&socket, &event);
                                if is_stopped {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                socket.push("error", json!({ "reason": "watcher closed" }));
                                break;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        }
                    }
                }
            }
        });

        // Subscribe to ready signal before spawning
        let mut ready_rx = active_watch.ready.subscribe();

        // Atomically check if we should start the watcher
        let should_start_watcher = active_watch
            .started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();

        if should_start_watcher {
            let active_watch_for_task = active_watch.clone();
            let canonical_path_for_task = canonical_path.clone();
            let state_for_task = self.state.clone();

            // On Windows with WSL, use poll watcher directly since native watcher
            // doesn't work well with WSL paths
            #[cfg(target_os = "windows")]
            let use_poll_watcher = is_wsl;
            #[cfg(not(target_os = "windows"))]
            let use_poll_watcher = false;

            tokio::spawn(async move {
                run_watcher(
                    active_watch_for_task,
                    canonical_path_for_task,
                    state_for_task,
                    use_poll_watcher,
                )
                .await;
            });
        }

        // Wait for watcher to become active before replying
        let _ = ready_rx.wait_for(|v| v.is_some()).await;
        let ready_result = ready_rx.borrow().clone();
        match ready_result {
            Some(Ok(())) => JoinResult::ok(json!({
                "status": "watching",
                "path": canonical_path
            })),
            Some(Err(e)) => JoinResult::error(json!({
                "reason": e
            })),
            None => JoinResult::error(json!({
                "reason": "watcher failed to start"
            })),
        }
    }

    async fn handle_in(
        &self,
        _event: &str,
        _payload: Value,
        _socket: &mut SocketRef,
    ) -> HandleResult {
        // No client->server events needed for watch
        HandleResult::no_reply()
    }

    async fn terminate(&self, reason: &str, socket: &mut SocketRef) {
        let canonical_path = self
            .state
            .canonical_paths
            .remove(&socket.unique_id)
            .map(|(_, cp)| cp);

        debug!(
            path = ?canonical_path,
            reason = %reason,
            socket_id = %socket.unique_id,
            "Watch subscription terminated"
        );
    }
}

// ============================================================================
// File Watcher Task
// ============================================================================

// The watcher uses a broadcast channel to send events to all subscribers.
// We spawn one unique watcher task per canonical path and channel instances
// subscribe to it in join (`active_watch.tx.subscribe()`).
async fn run_watcher(
    active_watch: Arc<ActiveWatch>,
    canonical_path: String,
    state: WatchChannelState,
    use_poll_watcher: bool,
) {
    let tx = &active_watch.tx;
    // Create a channel to receive notify events
    let (notify_tx, mut notify_rx) =
        tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(256);

    // Helper to create poll watcher
    let create_poll_watcher =
        |notify_tx: tokio::sync::mpsc::Sender<notify::Result<notify::Event>>| {
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
                let msg = format!("Failed to create poll watcher: {}", e);
                let _ = active_watch.ready.send(Some(Err(msg.clone())));
                let _ = tx.send(WatchEvent::Stopped { error: Some(msg) });
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
                        let msg = format!(
                            "Failed to create watcher: {} (poll fallback also failed: {})",
                            e, poll_err
                        );
                        let _ = active_watch.ready.send(Some(Err(msg.clone())));
                        let _ = tx.send(WatchEvent::Stopped { error: Some(msg) });
                        state.watchers.remove(&canonical_path);
                        return;
                    }
                }
            }
        }
    };

    if let Err(e) = watcher.watch(Path::new(&canonical_path), RecursiveMode::Recursive) {
        let msg = format!("Failed to watch path: {}", e);
        let _ = active_watch.ready.send(Some(Err(msg.clone())));
        let _ = tx.send(WatchEvent::Stopped { error: Some(msg) });
        state.watchers.remove(&canonical_path);
        return;
    }

    let _ = active_watch.ready.send(Some(Ok(())));
    debug!(path = %canonical_path, "File watcher started");

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
                            &canonical_path,
                            &mut pending_rename_from,
                        );

                        for watch_event in watch_events {
                            let is_stopped = matches!(&watch_event, WatchEvent::Stopped { .. });

                            let _ = tx.send(watch_event);

                            if is_stopped {
                                state.watchers.remove(&canonical_path);
                                return;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(WatchEvent::Stopped {
                            error: Some(format!("Watch error: {}", e)),
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
            // Periodic subscriber check for cleanup (no event sent to clients)
            _ = tokio::time::sleep(cleanup_check_interval) => {
                if tx.receiver_count() == 0 {
                    debug!("No subscribers remaining for watch on {}, cleaning up", canonical_path);
                    state.watchers.remove(&canonical_path);
                    return;
                }
            }
        }
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
                    // Watched directory was removed
                    results.push(WatchEvent::Stopped {
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

fn push_watch_event(socket: &SocketRef, event: &WatchEvent) {
    match event {
        WatchEvent::Created { path } => socket.push("created", json!({ "path": path })),
        WatchEvent::Modified { path } => socket.push("modified", json!({ "path": path })),
        WatchEvent::Deleted { path } => socket.push("deleted", json!({ "path": path })),
        WatchEvent::Renamed { from, to } => {
            socket.push("renamed", json!({ "from": from, "to": to }))
        }
        WatchEvent::Warning { message } => socket.push("warning", json!({ "message": message })),
        WatchEvent::Stopped { error } => {
            socket.push(
                "error",
                json!({ "reason": error.as_deref().unwrap_or("stopped") }),
            );
        }
    }
}
