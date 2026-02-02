//! File system watching using Phoenix channels.
//!
//! This module provides a Phoenix channel implementation for file watching.
//! Topic format: `watch:/path/to/directory`
//!
//! # Events
//!
//! Server → Client:
//! - `created` - File created: `{"path": "/full/path"}`
//! - `modified` - File modified: `{"path": "/full/path"}`
//! - `deleted` - File deleted: `{"path": "/full/path"}`
//! - `renamed` - File renamed: `{"from": "/old/path", "to": "/new/path"}`
//! - `warning` - Non-fatal warning: `{"message": "..."}`

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

        // Spawn the file watcher background task
        tokio::spawn(file_watcher_task(
            canonical_path.clone(),
            info_sender,
            shutdown_token,
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
        // No client→server events needed for watch
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

/// Background task that watches the file system and sends events via InfoSender.
/// Automatically stops when the subscription terminates (via shutdown token).
async fn file_watcher_task(
    canonical_path: String,
    info_sender: InfoSender<WatchInfo>,
    shutdown: CancellationToken,
) {
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
                canonical_path, e
            );

            let notify_tx_poll = notify_tx.clone();
            let poll_config = notify::Config::default().with_poll_interval(Duration::from_secs(2));

            match PollWatcher::new(
                move |res| {
                    let _ = notify_tx_poll.blocking_send(res);
                },
                poll_config,
            ) {
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
                results.push(WatchInfo::Created {
                    path: path.to_string_lossy().to_string(),
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
                            results.push(WatchInfo::Renamed {
                                from: from_path,
                                to: to_path.to_string_lossy().to_string(),
                            });
                        } else {
                            results.push(WatchInfo::Created {
                                path: to_path.to_string_lossy().to_string(),
                            });
                        }
                    }
                }
                notify::event::RenameMode::Both => {
                    if event.paths.len() >= 2 {
                        results.push(WatchInfo::Renamed {
                            from: event.paths[0].to_string_lossy().to_string(),
                            to: event.paths[1].to_string_lossy().to_string(),
                        });
                    }
                }
                _ => {
                    for path in event.paths {
                        results.push(WatchInfo::Modified {
                            path: path.to_string_lossy().to_string(),
                        });
                    }
                }
            },
            _ => {
                for path in event.paths {
                    results.push(WatchInfo::Modified {
                        path: path.to_string_lossy().to_string(),
                    });
                }
            }
        },
        EventKind::Remove(_) => {
            for path in &event.paths {
                let path_str = path.to_string_lossy().to_string();

                if path_str == watched_path {
                    // Watched directory was removed
                    results.push(WatchInfo::WatchedPathRemoved);
                } else {
                    results.push(WatchInfo::Deleted { path: path_str });
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
    use std::sync::Arc;

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
            WatchInfo::Created { path } => assert_eq!(path, "/tmp/test.txt"),
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
                assert_eq!(from, "/tmp/old.txt");
                assert_eq!(to, "/tmp/new.txt");
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
}
