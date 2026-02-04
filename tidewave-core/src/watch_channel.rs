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
//!
//! Note: Event paths are relative to the watched directory.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use notify::{EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use phoenix_rs::{CancellationToken, Channel, HandleResult, InfoSender, JoinResult, SocketRef};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::utils::normalize_path;

/// Info messages sent from the file watcher background task to the channel
enum WatchInfo {
    /// File created
    Created { path: String },
    /// File modified
    Modified { path: String },
    /// File deleted
    Deleted { path: String },
    /// File renamed
    Renamed { from: String, to: String },
    /// Warning (non-fatal)
    Warning { message: String },
    /// Watched path was removed (fatal)
    WatchedPathRemoved,
    /// Watcher error (fatal)
    WatchError { message: String },
}

/// Phoenix channel for file system watching.
///
/// Topic format: `watch:/path/to/directory`
/// Optional join payload: `{"is_wsl": true}` for WSL path normalization
pub struct WatchChannel;

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

        // Get canonical path
        let canonical_path = match path_obj.canonicalize() {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(e) => {
                return JoinResult::error(json!({
                    "reason": format!("failed to canonicalize path: {}", e)
                }));
            }
        };

        // Store canonical path in assigns for terminate callback
        socket.assign("canonical_path", canonical_path.clone());

        // Get info sender and shutdown token for the watcher task
        let info_sender = match socket.info_sender::<WatchInfo>() {
            Some(s) => s,
            None => {
                return JoinResult::error(json!({
                    "reason": "internal error: info sender not available"
                }));
            }
        };

        let shutdown_token = match socket.shutdown_token() {
            Some(t) => t,
            None => {
                return JoinResult::error(json!({
                    "reason": "internal error: shutdown token not available"
                }));
            }
        };

        // On Windows with WSL, use poll watcher directly since native watcher
        // doesn't work well with WSL paths
        #[cfg(target_os = "windows")]
        let use_poll_watcher = is_wsl;
        #[cfg(not(target_os = "windows"))]
        let use_poll_watcher = false;

        // Spawn the file watcher background task
        tokio::spawn(file_watcher_task(
            canonical_path.clone(),
            info_sender,
            shutdown_token,
            use_poll_watcher,
        ));

        JoinResult::ok(json!({
            "status": "watching",
            "path": canonical_path
        }))
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

    async fn handle_info(&self, message: Box<dyn std::any::Any + Send>, socket: &mut SocketRef) {
        // Downcast to our expected type
        if let Ok(info) = message.downcast::<WatchInfo>() {
            match *info {
                WatchInfo::Created { path } => {
                    socket.push("created", json!({ "path": path }));
                }
                WatchInfo::Modified { path } => {
                    socket.push("modified", json!({ "path": path }));
                }
                WatchInfo::Deleted { path } => {
                    socket.push("deleted", json!({ "path": path }));
                }
                WatchInfo::Renamed { from, to } => {
                    socket.push("renamed", json!({ "from": from, "to": to }));
                }
                WatchInfo::Warning { message } => {
                    socket.push("warning", json!({ "message": message }));
                }
                WatchInfo::WatchedPathRemoved => {
                    // Push error and stop - the subscription will be terminated
                    socket.push(
                        "error",
                        json!({ "reason": "watched path was removed" }),
                    );
                }
                WatchInfo::WatchError { message } => {
                    socket.push("error", json!({ "reason": message }));
                }
            }
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut SocketRef) {
        let path = socket
            .get_assign::<String>("canonical_path")
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        debug!(path = %path, reason = %reason, "Watch subscription terminated");
        // Shutdown token is automatically cancelled by the subscription cleanup
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

/// Background task that watches the file system and sends events via InfoSender.
/// Automatically stops when the subscription terminates (via shutdown token).
async fn file_watcher_task(
    canonical_path: String,
    info_sender: InfoSender<WatchInfo>,
    shutdown: CancellationToken,
    use_poll_watcher: bool,
) {
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
                let _ = info_sender.send(WatchInfo::WatchError {
                    message: format!("Failed to create poll watcher: {}", e),
                });
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
                        let _ = info_sender.send(WatchInfo::Warning {
                            message: format!(
                                "Using poll-based file watching (native watcher unavailable: {}). \
                                 File change detection may be slower.",
                                e
                            ),
                        });
                        Box::new(w)
                    }
                    Err(poll_err) => {
                        let _ = info_sender.send(WatchInfo::WatchError {
                            message: format!(
                                "Failed to create watcher: {} (poll fallback also failed: {})",
                                e, poll_err
                            ),
                        });
                        return;
                    }
                }
            }
        }
    };

    if let Err(e) = watcher.watch(Path::new(&canonical_path), RecursiveMode::Recursive) {
        let _ = info_sender.send(WatchInfo::WatchError {
            message: format!("Failed to watch path: {}", e),
        });
        return;
    }

    debug!(path = %canonical_path, "File watcher started");

    let mut pending_rename_from: Option<String> = None;

    loop {
        tokio::select! {
            // Clean exit when subscription terminates
            _ = shutdown.cancelled() => {
                debug!(path = %canonical_path, "File watcher shutting down");
                break;
            }
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
                            let is_fatal = matches!(
                                &watch_event,
                                WatchInfo::WatchedPathRemoved | WatchInfo::WatchError { .. }
                            );

                            let _ = info_sender.send(watch_event);

                            if is_fatal {
                                return;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let _ = info_sender.send(WatchInfo::WatchError {
                            message: format!("Watch error: {}", e),
                        });
                        return;
                    }
                    None => {
                        // Channel closed unexpectedly
                        return;
                    }
                }
            }
        }
    }
}

fn convert_notify_event(
    event: notify::Event,
    watched_path: &str,
    pending_rename_from: &mut Option<String>,
) -> Vec<WatchInfo> {
    let mut results = Vec::new();

    match event.kind {
        EventKind::Create(_) => {
            for path in event.paths {
                if let Some(relative_path) = to_relative_path(&path, watched_path) {
                    results.push(WatchInfo::Created {
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
                                results.push(WatchInfo::Renamed {
                                    from: from_relative,
                                    to: to_relative,
                                });
                            } else {
                                // No pending from, or from was outside watched dir - treat as create
                                results.push(WatchInfo::Created {
                                    path: to_relative,
                                });
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
                                results.push(WatchInfo::Renamed { from, to });
                            }
                            (Some(from), None) => {
                                // Renamed out of watched directory - treat as delete
                                results.push(WatchInfo::Deleted { path: from });
                            }
                            (None, Some(to)) => {
                                // Renamed into watched directory - treat as create
                                results.push(WatchInfo::Created { path: to });
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
                            results.push(WatchInfo::Modified {
                                path: relative_path,
                            });
                        }
                    }
                }
            },
            _ => {
                for path in event.paths {
                    if let Some(relative_path) = to_relative_path(&path, watched_path) {
                        results.push(WatchInfo::Modified {
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
                    results.push(WatchInfo::WatchedPathRemoved);
                } else if let Some(relative_path) = to_relative_path(path, watched_path) {
                    results.push(WatchInfo::Deleted {
                        path: relative_path,
                    });
                }
            }
        }
        _ => {}
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_rs::ChannelRegistry;

    #[test]
    fn test_watch_channel_registration() {
        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        assert!(registry.find("watch:/tmp/test").is_some());
        assert!(registry.find("other:/tmp/test").is_none());
    }

    #[tokio::test]
    async fn test_convert_notify_event_create() {
        let event = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec!["/tmp/test.txt".into()],
            attrs: Default::default(),
        };

        let mut pending = None;
        let results = convert_notify_event(event, "/tmp", &mut pending);

        assert_eq!(results.len(), 1);
        match &results[0] {
            WatchInfo::Created { path } => assert_eq!(path, "test.txt"),
            _ => panic!("Expected Created event"),
        }
    }

    #[tokio::test]
    async fn test_convert_notify_event_rename() {
        // Rename comes as two events: From then To
        let from_event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::From,
            )),
            paths: vec!["/tmp/old.txt".into()],
            attrs: Default::default(),
        };

        let to_event = notify::Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To,
            )),
            paths: vec!["/tmp/new.txt".into()],
            attrs: Default::default(),
        };

        let mut pending = None;
        let results1 = convert_notify_event(from_event, "/tmp", &mut pending);
        assert!(results1.is_empty()); // From event just stores state
        assert!(pending.is_some());

        let results2 = convert_notify_event(to_event, "/tmp", &mut pending);
        assert_eq!(results2.len(), 1);
        match &results2[0] {
            WatchInfo::Renamed { from, to } => {
                assert_eq!(from, "old.txt");
                assert_eq!(to, "new.txt");
            }
            _ => panic!("Expected Renamed event"),
        }
    }

    #[tokio::test]
    async fn test_convert_notify_event_watched_path_removed() {
        let event = notify::Event {
            kind: EventKind::Remove(notify::event::RemoveKind::Folder),
            paths: vec!["/tmp/watched".into()],
            attrs: Default::default(),
        };

        let mut pending = None;
        let results = convert_notify_event(event, "/tmp/watched", &mut pending);

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], WatchInfo::WatchedPathRemoved));
    }

    #[tokio::test]
    async fn test_to_relative_path() {
        let abs_path = Path::new("/watched/dir/subdir/file.txt");
        let result = to_relative_path(abs_path, "/watched/dir");
        assert_eq!(result, Some("subdir/file.txt".to_string()));

        // Path not under watched dir
        let outside_path = Path::new("/other/file.txt");
        let result = to_relative_path(outside_path, "/watched/dir");
        assert_eq!(result, None);
    }
}
