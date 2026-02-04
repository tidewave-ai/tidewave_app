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

        // Create a channel to receive ready signal from the watcher task
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        // Spawn the file watcher background task
        tokio::spawn(file_watcher_task(
            canonical_path.clone(),
            info_sender,
            shutdown_token,
            use_poll_watcher,
            ready_tx,
        ));

        // Wait for the watcher to be ready before returning
        match ready_rx.await {
            Ok(Ok(())) => JoinResult::ok(json!({
                "status": "watching",
                "path": canonical_path
            })),
            Ok(Err(e)) => JoinResult::error(json!({
                "reason": e
            })),
            Err(_) => JoinResult::error(json!({
                "reason": "watcher task failed to start"
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
                    socket.push("error", json!({ "reason": "watched path was removed" }));
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
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
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
                let msg = format!("Failed to create poll watcher: {}", e);
                let _ = ready_tx.send(Err(msg));
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
                        let msg = format!(
                            "Failed to create watcher: {} (poll fallback also failed: {})",
                            e, poll_err
                        );
                        let _ = ready_tx.send(Err(msg));
                        return;
                    }
                }
            }
        }
    };

    if let Err(e) = watcher.watch(Path::new(&canonical_path), RecursiveMode::Recursive) {
        let msg = format!("Failed to watch path: {}", e);
        let _ = ready_tx.send(Err(msg));
        return;
    }

    // Signal that the watcher is ready
    let _ = ready_tx.send(Ok(()));
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
                                results.push(WatchInfo::Created { path: to_relative });
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
    use phoenix_rs::{ChannelRegistry, TestClient};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn test_watch_join_success() {
        let temp_dir = tempfile::tempdir().unwrap();
        let watch_path = temp_dir.path().to_string_lossy().to_string();
        let topic = format!("watch:{}", watch_path);

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);

        let mut client = TestClient::new(registry);
        let channel = client.join(&topic, json!({})).await;
        assert!(channel.is_ok(), "Expected successful join");
    }

    #[tokio::test]
    async fn test_watch_join_invalid_topic() {
        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);

        let result = client.join("watch:", json!({})).await;
        assert!(result.is_err());
        let error = result.err().unwrap();
        assert_eq!(error.payload["status"], "error");
        assert!(error.payload["response"]["reason"]
            .as_str()
            .unwrap()
            .contains("invalid topic"));
    }

    #[tokio::test]
    async fn test_watch_join_nonexistent_path() {
        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);

        let result = client
            .join("watch:/nonexistent/path/that/does/not/exist", json!({}))
            .await;
        assert!(result.is_err());
        let error = result.err().unwrap();
        assert_eq!(error.payload["status"], "error");
        assert!(error.payload["response"]["reason"]
            .as_str()
            .unwrap()
            .contains("does not exist"));
    }

    #[tokio::test]
    async fn test_watch_join_relative_path() {
        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);

        let result = client.join("watch:relative/path", json!({})).await;
        assert!(result.is_err());
        let error = result.err().unwrap();
        assert_eq!(error.payload["status"], "error");
        assert!(error.payload["response"]["reason"]
            .as_str()
            .unwrap()
            .contains("absolute"));
    }

    #[tokio::test]
    async fn test_watch_file_created() {
        let temp_dir = tempfile::tempdir().unwrap();
        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);
        let mut channel = client.join(&topic, json!({})).await.unwrap();

        // Create file
        tokio::fs::write(temp_dir.path().join("test_file.txt"), "hello")
            .await
            .unwrap();

        // Wait for event
        let msg = channel.recv_timeout(Duration::from_secs(3)).await;
        assert!(msg.is_some(), "Expected created event");
        let msg = msg.unwrap();
        assert_eq!(msg.event, "created");
        assert_eq!(msg.payload["path"].as_str().unwrap(), "test_file.txt");
    }

    #[tokio::test]
    async fn test_watch_file_modified() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("existing_file.txt");
        tokio::fs::write(&file_path, "initial content")
            .await
            .unwrap();

        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);
        let mut channel = client.join(&topic, json!({})).await.unwrap();

        // Modify the file
        tokio::fs::write(&file_path, "modified content")
            .await
            .unwrap();

        // Wait for event
        let msg = channel.recv_timeout(Duration::from_secs(3)).await;
        assert!(msg.is_some(), "Expected modified event");
        let msg = msg.unwrap();
        assert_eq!(msg.event, "modified");
        assert_eq!(msg.payload["path"].as_str().unwrap(), "existing_file.txt");
    }

    #[tokio::test]
    async fn test_watch_file_deleted() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("file_to_delete.txt");
        tokio::fs::write(&file_path, "content").await.unwrap();

        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);
        let mut channel = client.join(&topic, json!({})).await.unwrap();

        // Delete the file
        tokio::fs::remove_file(&file_path).await.unwrap();

        // Wait for event
        let msg = channel.recv_timeout(Duration::from_secs(3)).await;
        assert!(msg.is_some(), "Expected deleted event");
        let msg = msg.unwrap();
        assert_eq!(msg.event, "deleted");
        assert_eq!(msg.payload["path"].as_str().unwrap(), "file_to_delete.txt");
    }

    #[tokio::test]
    async fn test_watch_leave() {
        let temp_dir = tempfile::tempdir().unwrap();
        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);
        let channel = client.join(&topic, json!({})).await.unwrap();

        // Leave the channel - leave() returns PhxMessage with ok status on success
        let result = channel.leave().await;
        assert_eq!(
            result.payload.get("status").and_then(|s| s.as_str()),
            Some("ok")
        );
    }

    #[tokio::test]
    async fn test_watch_wsl_param() {
        let temp_dir = tempfile::tempdir().unwrap();
        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);

        // Join with is_wsl parameter (false has no effect on non-WSL systems)
        let channel = client.join(&topic, json!({"is_wsl": false})).await;
        assert!(channel.is_ok());
    }

    #[tokio::test]
    async fn test_watch_subdirectory_events() {
        let temp_dir = tempfile::tempdir().unwrap();
        let sub_dir = temp_dir.path().join("subdir");
        tokio::fs::create_dir(&sub_dir).await.unwrap();

        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        let mut registry = ChannelRegistry::new();
        registry.register("watch:*", WatchChannel);
        let mut client = TestClient::new(registry);
        let mut channel = client.join(&topic, json!({})).await.unwrap();

        // Create a file in the subdirectory
        tokio::fs::write(sub_dir.join("nested_file.txt"), "nested content")
            .await
            .unwrap();

        // Should receive created event with relative path including subdirectory
        let msg = channel.recv_timeout(Duration::from_secs(3)).await;
        assert!(msg.is_some(), "Expected created event for nested file");
        let msg = msg.unwrap();
        assert_eq!(msg.event, "created");
        assert_eq!(
            msg.payload["path"].as_str().unwrap(),
            "subdir/nested_file.txt"
        );
    }

    #[tokio::test]
    async fn test_watch_concurrent_subscribers() {
        let temp_dir = tempfile::tempdir().unwrap();
        let topic = format!("watch:{}", temp_dir.path().to_string_lossy());

        // Create two clients watching the same directory
        let mut registry1 = ChannelRegistry::new();
        registry1.register("watch:*", WatchChannel);
        let mut client1 = TestClient::new(registry1);

        let mut registry2 = ChannelRegistry::new();
        registry2.register("watch:*", WatchChannel);
        let mut client2 = TestClient::new(registry2);

        // Both join the same topic
        let mut channel1 = client1.join(&topic, json!({})).await.unwrap();
        let mut channel2 = client2.join(&topic, json!({})).await.unwrap();

        // Create a file
        tokio::fs::write(temp_dir.path().join("shared_file.txt"), "content")
            .await
            .unwrap();

        // Both should receive the created event
        let msg1 = channel1.recv_timeout(Duration::from_secs(3)).await;
        let msg2 = channel2.recv_timeout(Duration::from_secs(3)).await;

        assert!(msg1.is_some(), "Client 1 should receive created event");
        assert!(msg2.is_some(), "Client 2 should receive created event");
        assert_eq!(msg1.unwrap().event, "created");
        assert_eq!(msg2.unwrap().event, "created");
    }
}
