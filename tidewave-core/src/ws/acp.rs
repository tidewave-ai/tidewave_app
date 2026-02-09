/**
Phoenix channel implementation for ACP (Agent Client Protocol).

The idea behind the ACP Proxy is similar to our MCP Proxy
(https://github.com/tidewave-ai/mcp_proxy_rust):
We keep the ACP session alive for the agent side, and allow the
client (the browser) to reconnect when necessary.

We do this by handling the minimal subset of ACP messages we need to
handle (init, new sessions) to keep track of active sessions and
add our own custom messages using the protocol extensibility features
(https://agentclientprotocol.com/protocol/extensibility)
to support a "_tidewave.ai/session/load" request for loading chats that
are still active on the agent side.

We start an agent when joining the channel - checking if there's not
already an existing process for the given command/cwd combination.

An overview of our protocol extensions:

1. Custom load session request

    {
      "jsonrpc": "2.0",
      "id": 1,
      "method": "_tidewave.ai/session/load",
      "params": {
        "sessionId": "sess_123456",
        "latestId": "foo",
      }
    }

    The client needs to pass the sessionId, as well as the "latestId", which
    is the most recent ID it successfully processed.

    In case the session cannot be loaded, we respond with a JSON-RPC error, reason
    "Session not found".

    {
      "jsonrpc": "2.0",
      "id": "1",
      "result": {}
    }

2. Custom agentCapabilities

    We inject a `tidewave.ai` meta key into the agent capabilities:

    {
      "jsonrpc": "2.0",
      "id": 0,
      "result": {
        "protocolVersion": 1,
        "agentCapabilities": {
          "loadSession": true,
          "_meta": {
            "tidewave.ai": {
              "session/load": true
            }
          }
        }
      }
    }

3. tidewave.ai/notificationId injection

    We inject a custom `tidewave.ai/notificationId` meta property into notifications.
    This ID is used for acknowledgements/pruning below.

    {
      "jsonrpc": "2.0",
      "method": "session/update",
      "params": {
        "sessionId": "sess_abc123def456",
        "update": {
          "sessionUpdate": "agent_message_chunk",
          "content": {
            "type": "text",
            "text": "I'll analyze your code for potential issues. Let me examine it..."
          }
        },
        "_meta": {
          "tidewave.ai/notificationId": "notif_12"
        }
      }
    }

4. Client acknowledgements

    A client can acknowledge messages (so we can prune the buffer):

    {
      "jsonrpc": "2.0",
      "method": "_tidewave.ai/ack",
      "params": {
        "latestId": "notif_12"
      }
    }

The proxy keeps a mapping of sessionId to the active socket connection.
There is a bit of nuance on how to do this. Imagine the following situation:

1. A client connects to the socket and starts an ACP session.
2. The client starts a prompt (JSON-RPC request with - let's assume - ID 1)
3. The client reconnects (browser reload).
4. The client load the session and receives buffered notifications.
5. The agent finishes and sends the response to the initial request with ID 1.

Now, we have an issue, because the browser did not send that original request.
Because of this, we don't use the ACP SDK in the browser, but instead handle raw
JSON-RPC messages and use the ACP-SDK for types. The proxy will continue to forward
any requests to the new connection.
*/
use crate::command::{create_shell_command, spawn_command, ChildProcess};
use crate::phoenix::{InitResult, PhxMessage};
use anyhow::{anyhow, Result};
use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc, Mutex, Notify, RwLock},
};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use super::ChannelSender;

// ============================================================================
// JSON-RPC Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ============================================================================
// Custom Tidewave Protocol Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveSpawnOptions {
    pub command: String,
    pub env: HashMap<String, String>,
    pub cwd: String,
    #[serde(default)]
    pub is_wsl: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveSessionLoadRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "latestId")]
    pub latest_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveSessionLoadResponse {
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveAckNotification {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "latestId")]
    pub latest_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveExitParams {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentExitEvent {
    pub error: String,
    pub message: String,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

// ============================================================================
// Regular ACP Message Types
// ============================================================================

// we also have a type for the session response, which we try to parse
// to see if we need to track a new session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionResponse {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

// ============================================================================
// Process Starter Abstraction
// ============================================================================

/// Type alias for process I/O streams
/// Returns (stdin_writer, stdout_reader, stderr_reader, optional_child_process)
pub type ProcessIo = (
    Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    Option<ChildProcess>,
);

/// Function type for starting a process and returning its I/O streams
pub type ProcessStarterFn = Arc<
    dyn Fn(
            TidewaveSpawnOptions,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ProcessIo>> + Send>>
        + Send
        + Sync,
>;

// ============================================================================
// Server State Types
// ============================================================================

pub type ChannelId = Uuid;
pub type ProcessKey = String; // command + cwd
pub type SessionId = String;
pub type NotificationId = String;

#[derive(Clone)]
pub struct AcpChannelState {
    /// Active ACP processes (process_key -> process_state)
    pub processes: Arc<DashMap<ProcessKey, Arc<ProcessState>>>,
    /// Channel senders (channel_id -> sender)
    pub channel_senders: Arc<DashMap<ChannelId, ChannelSender>>,
    /// Sessions (session_id -> session_state)
    pub sessions: Arc<DashMap<SessionId, Arc<SessionState>>>,
    /// Session to Channel mapping (session_id -> channel_id)
    /// Used when we need to forward an ACP message to the correct channel.
    pub session_to_channel: Arc<DashMap<SessionId, ChannelId>>,
    /// Channel to Process mapping (channel_id -> process_key)
    /// Used when we need to forward a channel message to the ACP process.
    pub channel_to_process: Arc<DashMap<ChannelId, ProcessKey>>,
    /// Process starter function for creating new ACP processes
    pub process_starter: ProcessStarterFn,
    /// Locks to prevent multiple concurrent process starts for the same process_key
    pub process_start_locks: Arc<DashMap<ProcessKey, Arc<Mutex<()>>>>,
}

impl AcpChannelState {
    pub fn new() -> Self {
        Self::with_process_starter(real_process_starter())
    }

    pub fn with_process_starter(process_starter: ProcessStarterFn) -> Self {
        Self {
            processes: Arc::new(DashMap::new()),
            channel_senders: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            session_to_channel: Arc::new(DashMap::new()),
            channel_to_process: Arc::new(DashMap::new()),
            process_starter,
            process_start_locks: Arc::new(DashMap::new()),
        }
    }
}

impl Default for AcpChannelState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ProcessState {
    pub key: ProcessKey,
    pub spawn_opts: TidewaveSpawnOptions,
    pub child: Arc<RwLock<Option<ChildProcess>>>,
    /// Channel used to forward a message to the ACP process.
    pub stdin_tx: Arc<RwLock<Option<mpsc::UnboundedSender<JsonRpcMessage>>>>,
    /// Channel used to signal the exit monitor to kill the process.
    pub exit_tx: Arc<RwLock<Option<mpsc::UnboundedSender<()>>>>,
    /// Broadcast channel used to notify all subscribed init loops about process exit.
    pub exit_broadcast: broadcast::Sender<AgentExitEvent>,
    pub next_proxy_id: Arc<AtomicU64>,

    // ID mapping for multiplexed connections
    pub client_to_proxy_ids: Arc<DashMap<(ChannelId, Value), Value>>,
    pub proxy_to_client_ids: Arc<DashMap<Value, (ChannelId, Value)>>,
    /// In case the client disconnected, the original client for a request ID
    /// does not exist any more, so we also store the a mapping to the session ID.
    pub proxy_to_session_ids: Arc<DashMap<Value, (SessionId, Value)>>,

    /// The cached init response we resend when a client reconnects.
    pub cached_init_response: Arc<RwLock<Option<JsonRpcResponse>>>,
    /// Whether an init request has already been sent to the process.
    pub init_sent: AtomicBool,
    /// Notified when the init response is cached, so waiters can grab it.
    pub init_complete: Arc<Notify>,

    /// Buffers for stdout/stderr output before init completes.
    /// Used to provide detailed error messages when process exits before init.
    pub stdout_buffer: Arc<RwLock<Vec<String>>>,
    pub stderr_buffer: Arc<RwLock<Vec<String>>>,

    // We store the request ID of init, session/new and session/load
    // because we need to handle their responses in a special way.
    pub init_request_id: Arc<RwLock<Option<Value>>>,
    pub new_request_ids: Arc<DashSet<Value>>,
    pub load_request_ids: Arc<DashMap<Value, SessionId>>,
    pub resume_request_ids: Arc<DashMap<Value, SessionId>>,
    pub fork_request_ids: Arc<DashSet<Value>>,
}

pub struct SessionState {
    pub process_key: ProcessKey,
    pub message_buffer: Arc<RwLock<Vec<BufferedMessage>>>,
    pub notification_id_counter: Arc<AtomicU64>,
    pub cancelled: Arc<AtomicBool>,
    pub cancel_counter: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferedMessage {
    pub id: NotificationId,
    pub message: JsonRpcMessage,
}

impl ProcessState {
    pub fn new(key: ProcessKey, spawn_opts: TidewaveSpawnOptions) -> Self {
        let (exit_broadcast, _) = broadcast::channel::<AgentExitEvent>(1);
        Self {
            key,
            spawn_opts,
            child: Arc::new(RwLock::new(None)),
            stdin_tx: Arc::new(RwLock::new(None)),
            exit_tx: Arc::new(RwLock::new(None)),
            exit_broadcast,
            next_proxy_id: Arc::new(AtomicU64::new(1)),
            client_to_proxy_ids: Arc::new(DashMap::new()),
            proxy_to_client_ids: Arc::new(DashMap::new()),
            proxy_to_session_ids: Arc::new(DashMap::new()),
            cached_init_response: Arc::new(RwLock::new(None)),
            init_sent: AtomicBool::new(false),
            init_complete: Arc::new(Notify::new()),
            stdout_buffer: Arc::new(RwLock::new(Vec::new())),
            stderr_buffer: Arc::new(RwLock::new(Vec::new())),
            init_request_id: Arc::new(RwLock::new(None)),
            new_request_ids: Arc::new(DashSet::<Value>::new()),
            load_request_ids: Arc::new(DashMap::new()),
            resume_request_ids: Arc::new(DashMap::new()),
            fork_request_ids: Arc::new(DashSet::<Value>::new()),
        }
    }

    pub async fn send_to_process(&self, message: JsonRpcMessage) -> Result<()> {
        if let Some(tx) = self.stdin_tx.read().await.as_ref() {
            tx.send(message)
                .map_err(|e| anyhow!("Failed to send message to process: {}", e))?;
        } else {
            return Err(anyhow!("Process stdin channel not available"));
        }
        Ok(())
    }

    pub fn generate_proxy_id(&self) -> Value {
        // Claude decided to use this ordering, we could also use UUIDs instead
        let id = self.next_proxy_id.fetch_add(1, Ordering::SeqCst);
        Value::Number(serde_json::Number::from(id))
    }

    pub fn map_client_id_to_proxy(
        &self,
        channel_id: ChannelId,
        client_id: Value,
        session_id: Option<SessionId>,
    ) -> Value {
        let proxy_id = self.generate_proxy_id();
        self.client_to_proxy_ids
            .insert((channel_id, client_id.clone()), proxy_id.clone());
        self.proxy_to_client_ids
            .insert(proxy_id.clone(), (channel_id, client_id.clone()));

        // If this request has a session_id, also store the session mapping
        if let Some(session_id) = session_id {
            self.proxy_to_session_ids
                .insert(proxy_id.clone(), (session_id, client_id));
        }
        proxy_id
    }

    pub fn resolve_proxy_id_to_client(&self, proxy_id: &Value) -> Option<(ChannelId, Value)> {
        self.proxy_to_client_ids
            .get(proxy_id)
            .map(|entry| entry.value().clone())
    }

    pub fn cleanup_id_mappings(&self, proxy_id: &Value) {
        if let Some((_, (channel_id, client_id))) = self.proxy_to_client_ids.remove(proxy_id) {
            self.client_to_proxy_ids.remove(&(channel_id, client_id));
        }
        self.proxy_to_session_ids.remove(proxy_id);
    }
}

impl SessionState {
    pub fn new(process_key: ProcessKey) -> Self {
        Self {
            process_key,
            message_buffer: Arc::new(RwLock::new(Vec::new())),
            notification_id_counter: Arc::new(AtomicU64::new(1)),
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn generate_notification_id(&self) -> NotificationId {
        let id = self.notification_id_counter.fetch_add(1, Ordering::SeqCst);
        format!("notif_{}", id)
    }

    pub async fn add_to_buffer(
        &self,
        message: JsonRpcMessage,
        id: NotificationId,
    ) -> NotificationId {
        let buffered = BufferedMessage {
            id: id.clone(),
            message,
        };

        let mut buffer = self.message_buffer.write().await;
        buffer.push(buffered);

        id
    }

    pub async fn prune_buffer(&self, latest_id: &str) {
        let mut buffer = self.message_buffer.write().await;

        // Find the index of the message with latest_id and remove all messages up to and including it
        if let Some(index) = buffer.iter().position(|msg| msg.id == latest_id) {
            buffer.drain(0..=index);
        }
    }

    /// Get buffered messages after a given ID.
    ///
    /// **IMPORTANT:** Caller must acquire the lock on `message_buffer` before calling this function.
    /// This allows the caller to control lock scope for atomicity.
    ///
    /// # Example
    /// ```ignore
    /// let buffer = session.message_buffer.read().await;
    /// let messages = SessionState::get_buffered_messages_after(&buffer, "notif_5");
    /// ```
    pub fn get_buffered_messages_after(
        buffer: &[BufferedMessage],
        latest_id: &str,
    ) -> Vec<BufferedMessage> {
        // Find the index of the message with latest_id
        if let Some(index) = buffer.iter().position(|msg| msg.id == latest_id) {
            // Return all messages after this index
            buffer.iter().skip(index + 1).cloned().collect()
        } else {
            // If latest_id not found, return all messages (empty latest_id case)
            buffer.iter().cloned().collect()
        }
    }
}

// ============================================================================
// Push Helpers
// ============================================================================

fn push_jsonrpc(sender: &ChannelSender, message: &JsonRpcMessage) {
    match serde_json::to_value(message) {
        Ok(payload) => sender.push("jsonrpc", payload),
        Err(e) => error!("Failed to serialize JSON-RPC message: {}", e),
    }
}

// ============================================================================
// Channel Init
// ============================================================================

/// Initialize an `acp:{acp_id}` channel.
///
/// Starts (or reuses) an ACP process for the given spawn options, then runs a
/// message forwarding loop until the channel is left (incoming_rx closed) or
/// the process exits.
pub async fn init(
    state: &AcpChannelState,
    msg: &PhxMessage,
    outgoing_tx: mpsc::UnboundedSender<PhxMessage>,
    mut incoming_rx: mpsc::UnboundedReceiver<PhxMessage>,
) -> InitResult {
    // Extract acp_id from topic "acp:{acp_id}"
    let acp_id = match msg.topic.strip_prefix("acp:") {
        Some(id) => id.to_string(),
        None => {
            return InitResult::Error("invalid topic format, expected acp:{acp_id}".to_string());
        }
    };

    let channel_id = Uuid::new_v4();

    debug!(
        "ACP channel join: acp_id={}, channel_id={}",
        acp_id, channel_id
    );

    let sender = ChannelSender {
        tx: outgoing_tx.clone(),
        topic: msg.topic.clone(),
        join_ref: msg.join_ref.clone(),
    };

    // Store the sender for this channel
    state.channel_senders.insert(channel_id, sender);

    // Parse spawn options from join payload
    let spawn_opts: TidewaveSpawnOptions = match serde_json::from_value(msg.payload.clone()) {
        Ok(opts) => opts,
        Err(e) => {
            state.channel_senders.remove(&channel_id);
            return InitResult::Error(format!("invalid spawn options: {}", e));
        }
    };

    // Generate process key and start/reuse process
    let process_key = format!("{}:{}", &spawn_opts.command, &spawn_opts.cwd);

    // Acquire or create a lock for this process_key
    let lock = state
        .process_start_locks
        .entry(process_key.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let guard = lock.lock().await;

    // Check if we already have a process for this key
    if !state.processes.contains_key(&process_key) {
        // Need to start a new process
        let new_process = Arc::new(ProcessState::new(process_key.clone(), spawn_opts));

        match start_acp_process(new_process.clone(), state.clone()).await {
            Ok(()) => {
                state.processes.insert(process_key.clone(), new_process);
            }
            Err(e) => {
                drop(guard);
                state.process_start_locks.remove(&process_key);
                state.channel_senders.remove(&channel_id);
                return InitResult::Error(format!("failed to start process: {}", e));
            }
        }
    }

    drop(guard);
    state.process_start_locks.remove(&process_key);

    // Map channel to process
    state
        .channel_to_process
        .insert(channel_id, process_key.clone());

    // Subscribe to exit broadcast before entering the loop.
    // This must happen before the OK reply so we don't miss an exit that
    // occurs between reply and the first select! iteration.
    let mut exit_rx = {
        let process_state = state.processes.get(&process_key).map(|p| p.clone());
        match process_state {
            Some(ps) => ps.exit_broadcast.subscribe(),
            None => {
                // Process already gone
                state.channel_senders.remove(&channel_id);
                state.channel_to_process.remove(&channel_id);
                return InitResult::Error("process exited before channel init".to_string());
            }
        }
    };

    // Send success reply
    let _ = outgoing_tx.send(PhxMessage::ok_reply(msg, json!({})));

    let mut result = InitResult::Done;

    // Main loop: handle incoming messages and process exit
    loop {
        tokio::select! {
            exit_result = exit_rx.recv() => {
                match exit_result {
                    Ok(exit_event) => {
                        send_agent_exit(
                            state,
                            channel_id,
                            &exit_event.error,
                            &exit_event.message,
                            exit_event.stdout,
                            exit_event.stderr,
                        );
                        result = InitResult::Shutdown("agent_exit".to_string());
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            msg = incoming_rx.recv() => {
                match msg {
                    Some(phx_msg) => {
                        match phx_msg.event.as_str() {
                            "jsonrpc" => {
                                // Parse the JSON-RPC message
                                let message: JsonRpcMessage =
                                    match serde_json::from_value(phx_msg.payload.clone()) {
                                        Ok(msg) => msg,
                                        Err(e) => {
                                            error!("Failed to parse JSON-RPC message: {}", e);
                                            continue;
                                        }
                                    };

                                trace!(
                                    "Received jsonrpc from channel {}: {:?}",
                                    channel_id,
                                    message
                                );

                                if let Err(e) = handle_client_message(state, channel_id, message).await {
                                    error!("Error handling client message: {}", e);
                                }
                            }
                            "exit" => {
                                if let Err(e) = handle_exit_request(state, channel_id).await {
                                    error!("Error handling exit request: {}", e);
                                }
                            }
                            _ => {
                                warn!("Unknown event in ACP channel: {}", phx_msg.event);
                            }
                        }
                    }
                    None => {
                        // Channel was left/disconnected
                        break;
                    }
                }
            }
        }
    }

    // Cleanup (equivalent to old terminate)
    debug!("ACP channel terminating for channel_id: {}", channel_id);

    state.channel_senders.remove(&channel_id);
    state.channel_to_process.remove(&channel_id);

    // Capture sessions for this channel and remove mappings.
    // We first collect all session IDs that map to this channel, then look up
    // their state separately. This is important because the process exit handler
    // might have already removed sessions from state.sessions, but we still need
    // to clean up session_to_channel.
    let session_ids_for_channel: Vec<SessionId> = state
        .session_to_channel
        .iter()
        .filter(|entry| *entry.value() == channel_id)
        .map(|entry| entry.key().clone())
        .collect();

    // Now get the cancel counters for sessions that still exist
    let mut sessions_for_channel = Vec::new();
    for session_id in &session_ids_for_channel {
        state.session_to_channel.remove(session_id);
        if let Some(session) = state.sessions.get(session_id) {
            sessions_for_channel.push((
                session_id.clone(),
                session.cancel_counter.load(Ordering::Relaxed),
            ));
        }
    }

    // Spawn a task to send session/cancel after 10 seconds
    let state_clone = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        for (session_id, counter) in sessions_for_channel {
            // Check if session is still unmapped (not reconnected)
            if state_clone.session_to_channel.contains_key(&session_id) {
                debug!("Skipping session/cancel for {} (reconnected)", session_id);
            } else if let Some(session_state) = state_clone.sessions.get(&session_id) {
                // Check if cancel_counter matches (session hasn't been reloaded)
                if session_state.cancel_counter.load(Ordering::Relaxed) != counter {
                    debug!(
                        "Skipping session/cancel for {} because counter does not match!",
                        session_id
                    );
                    continue;
                }

                // Session is still unmapped, send cancel notification
                let process_key = &session_state.process_key;
                if let Some(process_state) = state_clone.processes.get(process_key) {
                    // Mark session as cancelled
                    session_state.cancelled.store(true, Ordering::SeqCst);

                    let cancel_notification = JsonRpcNotification {
                        jsonrpc: "2.0".to_string(),
                        method: "session/cancel".to_string(),
                        params: Some(json!({
                            "sessionId": session_id
                        })),
                    };

                    if let Err(e) = process_state
                        .send_to_process(JsonRpcMessage::Notification(cancel_notification))
                        .await
                    {
                        error!("Failed to send session/cancel for {}: {}", session_id, e);
                    } else {
                        debug!("Sent session/cancel for unmapped session: {}", session_id);
                    }
                }
            }
        }
    });

    result
}

// ============================================================================
// Client Message Handlers
// ============================================================================

async fn handle_client_message(
    state: &AcpChannelState,
    channel_id: ChannelId,
    message: JsonRpcMessage,
) -> Result<()> {
    match &message {
        JsonRpcMessage::Request(req) => {
            debug!("Handling request: {} with method {}", req.id, req.method);
            handle_client_request(state, channel_id, req).await
        }
        JsonRpcMessage::Notification(notif) => {
            debug!("Handling notification with method {}", notif.method);
            handle_client_notification(state, channel_id, notif).await
        }
        JsonRpcMessage::Response(resp) => {
            // Forward client responses (e.g., permission responses) back to the process.
            // Note that we don't need to perform ID mapping here, because the process is
            // the one that generated the request ID, so it will necessarily be unique.
            debug!("Forwarding response for ID {} to process", resp.id);
            forward_response_to_process(state, channel_id, resp).await
        }
    }
}

async fn handle_client_request(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
) -> Result<()> {
    match request.method.as_str() {
        // We handle init because we need to
        //   1. Start a new process in case there's no running one for the given parameters.
        //   2. In case we start, store the request ID to store the response later on.
        "initialize" => handle_initialize_request(state, channel_id, request).await,
        // Our custom session load handler
        "_tidewave.ai/session/load" => {
            handle_tidewave_session_load(state, channel_id, request).await
        }
        // ACP session load. We need to intercept it because we need to update the session mapping.
        "session/load" => handle_acp_session_load(state, channel_id, request).await,
        // Any other requests only perform proxy_id mapping and are otherwise forwarded as is.
        _ => handle_regular_request(state, channel_id, request).await,
    }
}

async fn handle_initialize_request(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
) -> Result<()> {
    // Process was already started during channel join
    let process_state = ensure_process_for_channel(state, channel_id)?;

    // Fast path: already have a cached init response
    if let Some(cached_response) = process_state.cached_init_response.read().await.as_ref() {
        let mut response = cached_response.clone();
        response.id = request.id.clone();
        send_to_channel(state, channel_id, JsonRpcMessage::Response(response));
        return Ok(());
    }

    // Try to be the one that sends the init request
    if process_state
        .init_sent
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // We won the race — send the init request to the process
        let session_id = extract_session_id_from_request(request);
        let proxy_id =
            process_state.map_client_id_to_proxy(channel_id, request.id.clone(), session_id);
        let mut proxy_request = request.clone();
        proxy_request.id = proxy_id.clone();

        *process_state.init_request_id.write().await = Some(proxy_id);

        if let Err(e) = process_state
            .send_to_process(JsonRpcMessage::Request(proxy_request))
            .await
        {
            error!("Failed to send initialize request to process: {}", e);
            send_agent_exit(
                state,
                channel_id,
                "communication_error",
                "Failed to communicate with process",
                None,
                None,
            );
        }
    } else {
        // Another client already sent the init request — wait for the response
        process_state.init_complete.notified().await;

        if let Some(cached_response) = process_state.cached_init_response.read().await.as_ref() {
            let mut response = cached_response.clone();
            response.id = request.id.clone();
            send_to_channel(state, channel_id, JsonRpcMessage::Response(response));
        } else {
            // Init completed but no cached response (error case, e.g. process died)
            send_agent_exit(
                state,
                channel_id,
                "init_error",
                "Process init failed",
                None,
                None,
            );
        }
    }

    Ok(())
}

async fn handle_tidewave_session_load(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
) -> Result<()> {
    let params: TidewaveSessionLoadRequest =
        serde_json::from_value(request.params.clone().unwrap_or(Value::Null))
            .map_err(|e| anyhow!("Invalid session/load params: {}", e))?;

    let session_state = match state.sessions.get(&params.session_id) {
        Some(session) => {
            let s = session.clone();
            s.cancel_counter.fetch_add(1, Ordering::SeqCst);
            s
        }
        None => {
            send_error_response(
                state,
                channel_id,
                &request.id,
                JsonRpcError {
                    code: -32002,
                    message: "Session not found".to_string(),
                    data: None,
                },
            );
            return Ok(());
        }
    };

    if !ensure_session_not_active(state, channel_id, request, &params.session_id) {
        return Ok(());
    }

    let was_cancelled = session_state.cancelled.swap(false, Ordering::SeqCst);

    if let Some(sender) = state.channel_senders.get(&channel_id) {
        // Map channel to process (needed for any requests during catchup)
        state
            .channel_to_process
            .insert(channel_id, session_state.process_key.clone());

        // ATOMIC CATCHUP: Hold the buffer read lock for the entire operation
        // This prevents new messages from being added while we're catching up,
        // ensuring we don't miss any messages.
        {
            let buffer = session_state.message_buffer.read().await;

            // Get buffered messages after latest_id
            let buffered_messages =
                SessionState::get_buffered_messages_after(&buffer, &params.latest_id);

            // Stream buffered messages while holding the lock
            // This is safe because sender.push() is non-blocking (unbounded channel)
            for buffered in buffered_messages {
                push_jsonrpc(&sender, &buffered.message);
            }

            // NOW register the session mapping while still holding the lock
            // This ensures no messages arrive between catchup and registration
            state
                .session_to_channel
                .insert(params.session_id.clone(), channel_id);
        } // Lock released here - new messages can now be buffered AND sent directly

        let response_data = TidewaveSessionLoadResponse {
            cancelled: was_cancelled,
        };

        let success_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: serde_json::to_value(response_data).ok(),
            error: None,
        };

        push_jsonrpc(&sender, &JsonRpcMessage::Response(success_response));
    }

    Ok(())
}

async fn handle_acp_session_load(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
) -> Result<()> {
    let session_id = extract_session_id_from_request(request);

    if let Some(session_id) = session_id {
        if !ensure_session_not_active(state, channel_id, request, &session_id) {
            return Ok(());
        }

        let process_key = match state.sessions.get(&session_id) {
            Some(session_state) => {
                info!(
                    "session/load for existing session {} on channel {}",
                    session_id, channel_id
                );
                session_state.process_key.clone()
            }
            None => {
                let process_key = match find_process_key_for_channel(state, channel_id) {
                    Some(key) => key,
                    None => {
                        warn!(
                            "session/load: no process mapping found for channel {}",
                            channel_id
                        );
                        return handle_regular_request(state, channel_id, request).await;
                    }
                };

                let session_state = Arc::new(SessionState::new(process_key.clone()));
                state.sessions.insert(session_id.clone(), session_state);

                info!(
                    "Created new session {} for session/load on channel {}",
                    session_id, channel_id
                );
                process_key
            }
        };

        // Map this channel to the session BEFORE forwarding the request
        // This ensures that when the agent sends notifications during session/load,
        // we can route them to the correct websocket
        state
            .session_to_channel
            .insert(session_id.clone(), channel_id);
        state.channel_to_process.insert(channel_id, process_key);

        info!(
            "Mapped channel {} to session {} for session/load",
            channel_id, session_id
        );
    }

    // Forward the request to the agent as a regular request
    handle_regular_request(state, channel_id, request).await
}

async fn handle_exit_request(state: &AcpChannelState, channel_id: ChannelId) -> Result<()> {
    let process_key = match state.channel_to_process.get(&channel_id) {
        Some(key) => key.clone(),
        None => {
            warn!(
                "Exit request for channel {} with no process mapping",
                channel_id
            );
            return Ok(());
        }
    };

    let process_state = match state.processes.get(&process_key) {
        Some(process) => process.clone(),
        None => {
            warn!("Exit request for non-existent process: {}", process_key);
            return Ok(());
        }
    };

    info!("Exit request received for process: {}", process_key);

    if let Some(exit_tx) = process_state.exit_tx.read().await.as_ref() {
        if let Err(e) = exit_tx.send(()) {
            error!(
                "Failed to send exit signal for process {}: {}",
                process_key, e
            );
        } else {
            info!("Sent exit signal for process: {}", process_key);
        }
    } else {
        warn!("No exit channel available for process: {}", process_key);
    }

    // The exit monitor will handle killing the process, sending notifications, and cleanup

    Ok(())
}

async fn handle_regular_request(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
) -> Result<()> {
    let process_state = ensure_process_for_channel(state, channel_id)?;

    // Map client ID to proxy ID
    let session_id = extract_session_id_from_request(request);
    let proxy_id =
        process_state.map_client_id_to_proxy(channel_id, request.id.clone(), session_id.clone());
    let mut proxy_request = request.clone();
    proxy_request.id = proxy_id.clone();

    match request.method.as_str() {
        // We intercept new sessions to map the sessionId to the channel
        "session/new" => {
            process_state.new_request_ids.insert(proxy_id.clone());
        }
        // We intercept load requests since we need to handle the unsuccessful case
        // and clear the session mapping.
        "session/load" => {
            if let Some(session_id) = session_id {
                process_state
                    .load_request_ids
                    .insert(proxy_id.clone(), session_id);
            }
        }
        // We intercept resume / fork sessions to map the sessionId to the channel
        "session/resume" => {
            if let Some(session_id) = session_id {
                process_state
                    .resume_request_ids
                    .insert(proxy_id.clone(), session_id);
            }
        }
        "session/fork" => {
            process_state.fork_request_ids.insert(proxy_id.clone());
        }
        _ => (),
    }

    // Forward to process
    if let Err(e) = process_state
        .send_to_process(JsonRpcMessage::Request(proxy_request))
        .await
    {
        error!("Failed to send request to process: {}", e);
    }

    Ok(())
}

async fn handle_client_notification(
    state: &AcpChannelState,
    channel_id: ChannelId,
    notification: &JsonRpcNotification,
) -> Result<()> {
    match notification.method.as_str() {
        "_tidewave.ai/ack" => handle_ack_notification(state, channel_id, notification).await,
        _ => forward_notification_to_process(state, channel_id, notification).await,
    }
}

async fn handle_ack_notification(
    state: &AcpChannelState,
    channel_id: ChannelId,
    notification: &JsonRpcNotification,
) -> Result<()> {
    let params: TidewaveAckNotification =
        serde_json::from_value(notification.params.clone().unwrap_or(Value::Null))
            .map_err(|e| anyhow!("Invalid ack params: {}", e))?;

    // Find the specific session to prune
    if let Some(session_state) = state.sessions.get(&params.session_id) {
        // Verify this websocket is actually connected to this session
        if let Some(mapped_channel_id) = state.session_to_channel.get(&params.session_id) {
            if *mapped_channel_id == channel_id {
                session_state.prune_buffer(&params.latest_id).await;
            } else {
                warn!(
                    "Channel {} tried to ACK session {} but is not the owner",
                    channel_id, params.session_id
                );
            }
        }
    } else {
        warn!("ACK for unknown session: {}", params.session_id);
    }

    Ok(())
}

async fn forward_notification_to_process(
    state: &AcpChannelState,
    channel_id: ChannelId,
    notification: &JsonRpcNotification,
) -> Result<()> {
    let process_state = ensure_process_for_channel(state, channel_id)?;

    process_state
        .send_to_process(JsonRpcMessage::Notification(notification.clone()))
        .await?;

    Ok(())
}

async fn forward_response_to_process(
    state: &AcpChannelState,
    channel_id: ChannelId,
    response: &JsonRpcResponse,
) -> Result<()> {
    let process_state = ensure_process_for_channel(state, channel_id)?;

    // Forward response directly (no ID mapping needed for process -> client -> process flow)
    process_state
        .send_to_process(JsonRpcMessage::Response(response.clone()))
        .await?;

    Ok(())
}

// ============================================================================
// Process Management
// ============================================================================

async fn start_acp_process(process_state: Arc<ProcessState>, state: AcpChannelState) -> Result<()> {
    let (stdin, stdout, stderr, child) =
        (state.process_starter)(process_state.spawn_opts.clone()).await?;

    *process_state.child.write().await = child;

    let (stdin_sender, mut stdin_receiver) = mpsc::unbounded_channel::<JsonRpcMessage>();
    *process_state.stdin_tx.write().await = Some(stdin_sender);

    // Start stdin handler
    let mut stdin_writer = stdin;
    tokio::spawn(async move {
        while let Some(message) = stdin_receiver.recv().await {
            if let Ok(json_str) = serde_json::to_string(&message) {
                let json_line = format!("{}\n", json_str);
                if let Err(e) = stdin_writer.write_all(json_line.as_bytes()).await {
                    error!("Failed to write to process stdin: {}", e);
                    break;
                }
                if let Err(e) = stdin_writer.flush().await {
                    error!("Failed to flush process stdin: {}", e);
                    break;
                }
            }
        }
        debug!("Process stdin handler ended");
    });

    // Start stdout handler
    let process_state_clone = process_state.clone();
    let state_clone = state.clone();
    tokio::spawn(async move {
        let mut lines = stdout.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(message) = serde_json::from_str::<JsonRpcMessage>(&line) {
                if let Err(e) =
                    handle_process_message(&process_state_clone, &state_clone, message).await
                {
                    error!("Failed to handle process message: {}", e);
                }
            } else {
                debug!("Received non-JSON line from process: {}", line);
                // Buffer if init hasn't completed yet
                if process_state_clone
                    .cached_init_response
                    .read()
                    .await
                    .is_none()
                {
                    process_state_clone.stdout_buffer.write().await.push(line);
                }
            }
        }
        debug!("Process stdout handler ended");
    });

    // Start stderr handler
    let process_state_stderr = process_state.clone();
    tokio::spawn(async move {
        let mut lines = stderr.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!("Process stderr: {}", line);
            // Buffer if init hasn't completed yet
            if process_state_stderr
                .cached_init_response
                .read()
                .await
                .is_none()
            {
                process_state_stderr.stderr_buffer.write().await.push(line);
            }
        }
        debug!("Process stderr handler ended");
    });

    // Create exit channel and store the sender in process_state
    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<()>();
    *process_state.exit_tx.write().await = Some(exit_tx);

    // Start process exit monitor
    let process_state_exit = process_state.clone();
    let state_exit = state.clone();
    tokio::spawn(async move {
        let exit_reason = {
            let mut child_guard = process_state_exit.child.write().await;
            if let Some(process) = child_guard.as_mut() {
                // Use tokio::select! to wait for either process exit or exit signal
                tokio::select! {
                    // Process exited naturally
                    status = process.child.wait() => {
                        match status {
                            Ok(s) => {
                                if s.success() {
                                    debug!("Process exited successfully");
                                    ("process_exit", format!("ACP process exited with code {}", s.code().unwrap_or(0)))
                                } else {
                                    debug!("Process exited with status: {}", s);
                                    ("process_exit", format!("ACP process exited with code {}", s.code().unwrap_or(-1)))
                                }
                            }
                            Err(e) => {
                                error!("Failed to wait for process: {}", e);
                                ("process_exit", format!("ACP process failed: {}", e))
                            }
                        }
                    }
                    // Received exit signal
                    _ = exit_rx.recv() => {
                        debug!("Exit signal received for process: {}", process_state_exit.key);
                        // Take ownership and drop to kill the process tree via ChildProcess::Drop
                        child_guard.take();
                        ("exit_requested", "ACP process was stopped by exit request".to_string())
                    }
                }
            } else {
                return;
            }
        };

        let (error_type, exit_message) = exit_reason;

        // If init never completed, include buffered output in the exit notification
        let (stdout, stderr) = if process_state_exit
            .cached_init_response
            .read()
            .await
            .is_none()
        {
            let stdout_buf = process_state_exit.stdout_buffer.read().await;
            let stderr_buf = process_state_exit.stderr_buffer.read().await;
            (
                if stdout_buf.is_empty() {
                    None
                } else {
                    Some(stdout_buf.join("\n"))
                },
                if stderr_buf.is_empty() {
                    None
                } else {
                    Some(stderr_buf.join("\n"))
                },
            )
        } else {
            (None, None)
        };

        // Broadcast exit event to all subscribed init loops
        let _ = process_state_exit.exit_broadcast.send(AgentExitEvent {
            error: error_type.to_string(),
            message: exit_message,
            stdout,
            stderr,
        });

        // Clean up sessions
        let sessions_to_remove: Vec<SessionId> = state_exit
            .sessions
            .iter()
            .filter(|entry| entry.value().process_key == process_state_exit.key)
            .map(|entry| entry.key().clone())
            .collect();

        for session_id in &sessions_to_remove {
            state_exit.sessions.remove(session_id);
        }

        // Notify any waiters for init response so they don't hang forever
        process_state_exit.init_complete.notify_waiters();

        state_exit.processes.remove(&process_state_exit.key);

        debug!(
            "Process exit handler ended, cleaned up {} sessions",
            sessions_to_remove.len()
        );
    });

    Ok(())
}

// ============================================================================
// Helper Functions
// ============================================================================

fn send_to_channel(state: &AcpChannelState, channel_id: ChannelId, message: JsonRpcMessage) {
    if let Some(sender) = state.channel_senders.get(&channel_id) {
        push_jsonrpc(&sender, &message);
    }
}

/// Helper function to send a JSON-RPC error response to the client.
/// This is used for expected error conditions that should be communicated to the client.
fn send_error_response(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request_id: &Value,
    error: JsonRpcError,
) {
    debug!("Sending JSON-RPC error to client: {}", error.message);

    let response = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: request_id.clone(),
        result: None,
        error: Some(error),
    };

    send_to_channel(state, channel_id, JsonRpcMessage::Response(response));
}

fn send_agent_exit(
    state: &AcpChannelState,
    channel_id: ChannelId,
    error_type: &str,
    message: &str,
    stdout: Option<String>,
    stderr: Option<String>,
) {
    let exit_params = TidewaveExitParams {
        error: error_type.to_string(),
        message: message.to_string(),
        stdout,
        stderr,
    };

    if let Some(sender) = state.channel_senders.get(&channel_id) {
        match serde_json::to_value(exit_params) {
            Ok(payload) => sender.push("agent_exit", payload),
            Err(e) => error!("Failed to serialize agent exit params: {}", e),
        }
    }
}

/// Helper function to ensure a session is not already active on another channel.
/// Returns false if the session is already active (and sends an error response to the client).
fn ensure_session_not_active(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    session_id: &str,
) -> bool {
    if state.session_to_channel.contains_key(session_id) {
        send_error_response(
            state,
            channel_id,
            &request.id,
            JsonRpcError {
                code: -32003,
                message: "Session already has an active connection".to_string(),
                data: None,
            },
        );
        return false;
    }
    true
}

/// Looks up the process for the given channel ID.
fn ensure_process_for_channel(
    state: &AcpChannelState,
    channel_id: ChannelId,
) -> Result<Arc<ProcessState>> {
    let process_key = match state.channel_to_process.get(&channel_id) {
        Some(key) => key.clone(),
        None => {
            return Err(anyhow!(
                "No process mapping found for channel: {}",
                channel_id
            ))
        }
    };

    let process_state = match state.processes.get(&process_key) {
        Some(process) => process.clone(),
        None => {
            return Err(anyhow!("Process not found for key: {}", process_key));
        }
    };

    Ok(process_state)
}

fn find_process_key_for_channel(
    state: &AcpChannelState,
    channel_id: ChannelId,
) -> Option<ProcessKey> {
    state
        .channel_to_process
        .get(&channel_id)
        .map(|key| key.clone())
}

// ============================================================================
// Process Message Handlers (standalone functions for use in spawned tasks)
// ============================================================================

async fn handle_process_message(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    message: JsonRpcMessage,
) -> Result<()> {
    debug!("Got process message {:?}", message);
    match &message {
        JsonRpcMessage::Response(response) => {
            handle_process_response(process_state, state, response).await
        }
        JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => {
            handle_process_notification_or_request(state, message).await
        }
    }
}

/// Handle response from process - map proxy ID back to client ID and route
async fn handle_process_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
) -> Result<()> {
    // Handle responses - map proxy ID back to client ID
    if let Some((channel_id, client_id)) = process_state.resolve_proxy_id_to_client(&response.id) {
        // Create response with original client ID
        let mut client_response = response.clone();
        client_response.id = client_id;

        maybe_handle_init_response(process_state, response, &mut client_response).await;
        maybe_handle_session_load_resume(
            process_state,
            state,
            channel_id,
            response,
            &client_response,
        )
        .await?;
        maybe_handle_session_new_response(
            process_state,
            state,
            response,
            &client_response,
            channel_id,
        )
        .await;

        if let Some(sender) = state.channel_senders.get(&channel_id) {
            push_jsonrpc(&sender, &JsonRpcMessage::Response(client_response));
        } else {
            handle_disconnected_client_response(process_state, state, response).await;
        }

        process_state.cleanup_id_mappings(&response.id);
    }

    Ok(())
}

/// Handle initialize response - inject version and cache
async fn maybe_handle_init_response(
    process_state: &Arc<ProcessState>,
    response: &JsonRpcResponse,
    client_response: &mut JsonRpcResponse,
) {
    let init_request_id = process_state.init_request_id.read().await;
    if init_request_id.as_ref() == Some(&response.id) {
        drop(init_request_id); // Release read lock
                               // Store init response for future inits
        *process_state.cached_init_response.write().await = Some(client_response.clone());
        // Notify any waiters that the init response is now cached
        process_state.init_complete.notify_waiters();
        // Clear buffers
        *process_state.stderr_buffer.write().await = Vec::new();
        *process_state.stdout_buffer.write().await = Vec::new();
    }
}

/// Handle load/resume response - check if successful and kill session
async fn maybe_handle_session_load_resume(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    channel_id: ChannelId,
    response: &JsonRpcResponse,
    client_response: &JsonRpcResponse,
) -> Result<()> {
    if let Some((_proxy_id, session_id)) = process_state
        .load_request_ids
        .remove(&response.id)
        .or_else(|| process_state.resume_request_ids.remove(&response.id))
    {
        if client_response.error.is_some() {
            info!("Failed to load session, removing mapping! {}", session_id);
            state.sessions.remove(&session_id);
            state.session_to_channel.remove(&session_id);
            if let Some(sender) = state.channel_senders.get(&channel_id) {
                push_jsonrpc(&sender, &JsonRpcMessage::Response(client_response.clone()));
            }
            return Err(anyhow!(
                "Failed to load session, removing mapping! {}",
                session_id
            ));
        } else {
            map_session_id_to_channel(state, session_id, channel_id).await;
        }
    }

    Ok(())
}

/// Handle session/new response - create new session
async fn maybe_handle_session_new_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
    client_response: &JsonRpcResponse,
    channel_id: ChannelId,
) {
    // check if new or fork - both are treated the same
    if process_state
        .new_request_ids
        .remove(&response.id)
        .or_else(|| process_state.fork_request_ids.remove(&response.id))
        .is_some()
    {
        if let Some(result) = &client_response.result {
            if let Ok(session_response) =
                serde_json::from_value::<NewSessionResponse>(result.clone())
            {
                map_session_id_to_channel(state, session_response.session_id, channel_id).await;
            }
        }
    }
}

async fn map_session_id_to_channel(
    state: &AcpChannelState,
    session_id: SessionId,
    channel_id: ChannelId,
) {
    if !state.sessions.contains_key(&session_id) {
        let process_key = state
            .channel_to_process
            .get(&channel_id)
            .map(|key| key.clone());

        if let Some(process_key) = process_key {
            let session_state = Arc::new(SessionState::new(process_key));
            state.sessions.insert(session_id.clone(), session_state);
            state.session_to_channel.insert(session_id, channel_id);
        }
    } else {
        warn!(
            "Unexpectedly got new/load/fork session response for already known session! {}",
            session_id
        );
    }
}

/// Handle response for disconnected client - forward to new channel or buffer
async fn handle_disconnected_client_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
) {
    debug!("Missing original channel for request {}", response.id);
    // Fallback: Client disconnected, try to find session and forward to new channel
    let session_info = process_state
        .proxy_to_session_ids
        .get(&response.id)
        .map(|entry| entry.value().clone());

    if let Some((session_id, client_id)) = session_info {
        let mut client_response = response.clone();
        client_response.id = client_id.clone();

        if let Some(current_channel_id) = state.session_to_channel.get(&session_id) {
            let current_channel_id = *current_channel_id;
            if let Some(sender) = state.channel_senders.get(&current_channel_id) {
                push_jsonrpc(&sender, &JsonRpcMessage::Response(client_response));
            }
        } else {
            // Client disconnected, buffer response
            if let Some(session_state) = state.sessions.get(&session_id) {
                let session_state = session_state.clone();
                let _ = session_state
                    .add_to_buffer(
                        JsonRpcMessage::Response(client_response),
                        client_id.to_string(),
                    )
                    .await;
            }
        }

        // Clean up the session mapping
        process_state.proxy_to_session_ids.remove(&response.id);
    }
}

async fn handle_process_notification_or_request(
    state: &AcpChannelState,
    message: JsonRpcMessage,
) -> Result<()> {
    // Handle requests/notifications from process - route by sessionId
    let session_id = extract_session_id_from_message(&message);

    if let Some(session_id) = session_id {
        if let Some(session_state) = state.sessions.get(&session_id) {
            let session_state = session_state.clone();

            // Add notification ID if this is a notification
            let mut routed_message = message.clone();
            let buffer_id = if let JsonRpcMessage::Notification(ref mut n) = routed_message {
                let notif_id = session_state.generate_notification_id();
                inject_notification_id(n, notif_id.clone());
                notif_id
            } else {
                // For requests/responses, use their existing ID converted to string
                match &routed_message {
                    JsonRpcMessage::Request(req) => req.id.to_string(),
                    JsonRpcMessage::Response(resp) => resp.id.to_string(),
                    _ => unreachable!(),
                }
            };

            let _buffer_id = session_state
                .add_to_buffer(routed_message.clone(), buffer_id)
                .await;

            // Route to appropriate WebSocket based on session_id
            if let Some(channel_id) = state.session_to_channel.get(&session_id) {
                if let Some(sender) = state.channel_senders.get(&channel_id) {
                    push_jsonrpc(&sender, &routed_message);
                }
            }
        } else {
            warn!("Session not found for sessionId: {}", session_id);
        }
    } else {
        warn!(
            "Message from process missing sessionId, ignoring: {:?}",
            message
        );
    }

    Ok(())
}

// ============================================================================
// Utility Functions
// ============================================================================

fn inject_notification_id(notification: &mut JsonRpcNotification, notif_id: NotificationId) {
    // Add _meta.tidewave.ai/notificationId to params
    if let Some(params) = &mut notification.params {
        if let Some(params_obj) = params.as_object_mut() {
            let mut meta_obj = params_obj
                .get("_meta")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            meta_obj.insert(
                "tidewave.ai/notificationId".to_string(),
                Value::String(notif_id.clone()),
            );
            params_obj.insert("_meta".to_string(), Value::Object(meta_obj));
        }
    } else {
        // Create params with just _meta if params is None
        let mut params_obj = Map::new();
        let mut meta_obj = Map::new();
        meta_obj.insert(
            "tidewave.ai/notificationId".to_string(),
            Value::String(notif_id.clone()),
        );
        params_obj.insert("_meta".to_string(), Value::Object(meta_obj));
        notification.params = Some(Value::Object(params_obj));
    }
}

fn extract_session_id_from_message(message: &JsonRpcMessage) -> Option<String> {
    let params = match message {
        JsonRpcMessage::Request(req) => req.params.as_ref(),
        JsonRpcMessage::Response(resp) => resp.result.as_ref(),
        JsonRpcMessage::Notification(notif) => notif.params.as_ref(),
    };

    if let Some(p) = params {
        if let Some(obj) = p.as_object() {
            if let Some(session_id) = obj.get("sessionId") {
                return session_id.as_str().map(|s| s.to_string());
            }
        }
    }
    None
}

fn extract_session_id_from_request(request: &JsonRpcRequest) -> Option<String> {
    if let Some(params) = &request.params {
        if let Some(obj) = params.as_object() {
            if let Some(session_id) = obj.get("sessionId") {
                return session_id.as_str().map(|s| s.to_string());
            }
        }
    }
    None
}

/// Real process starter that spawns actual OS processes
pub fn real_process_starter() -> ProcessStarterFn {
    Arc::new(|spawn_opts: TidewaveSpawnOptions| {
        Box::pin(async move {
            info!("Starting ACP process: {}", spawn_opts.command);

            let mut cmd = create_shell_command(
                &spawn_opts.command,
                spawn_opts.env,
                &spawn_opts.cwd,
                spawn_opts.is_wsl,
            );

            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut process =
                spawn_command(cmd).map_err(|e| anyhow!("Failed to spawn process: {}", e))?;

            let stdin = process
                .child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("Failed to get stdin"))?;
            let stdout = process
                .child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("Failed to get stdout"))?;
            let stderr = process
                .child
                .stderr
                .take()
                .ok_or_else(|| anyhow!("Failed to get stderr"))?;

            Ok::<ProcessIo, anyhow::Error>((
                Box::new(stdin),
                Box::new(BufReader::new(stdout)),
                Box::new(BufReader::new(stderr)),
                Some(process),
            ))
        })
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Helper function to create test TidewaveSpawnOptions
    fn test_spawn_opts() -> TidewaveSpawnOptions {
        TidewaveSpawnOptions {
            command: "test_cmd".to_string(),
            env: HashMap::new(),
            cwd: ".".to_string(),
            is_wsl: false,
        }
    }

    // Helper function to create a test SessionState
    fn create_test_session() -> SessionState {
        SessionState::new("test_command:params".to_string())
    }

    // Helper function to create a test notification message
    fn create_test_notification(method: &str, session_id: &str) -> JsonRpcMessage {
        JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: Some(json!({
                "sessionId": session_id,
                "update": "test_data"
            })),
        })
    }

    // Helper function to create a test response message
    fn create_test_response(id: u64) -> JsonRpcMessage {
        JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(serde_json::Number::from(id)),
            result: Some(json!({"status": "ok"})),
            error: None,
        })
    }

    // ============================================================================
    // Basic Buffer Operations Tests
    // ============================================================================

    #[tokio::test]
    async fn test_generate_notification_id() {
        let session = create_test_session();

        let id1 = session.generate_notification_id();
        let id2 = session.generate_notification_id();
        let id3 = session.generate_notification_id();

        assert_eq!(id1, "notif_1");
        assert_eq!(id2, "notif_2");
        assert_eq!(id3, "notif_3");
    }

    #[tokio::test]
    async fn test_add_to_buffer() {
        let session = create_test_session();

        let msg1 = create_test_notification("session/update", "sess_123");
        let msg2 = create_test_notification("session/update", "sess_123");

        let id1 = session
            .add_to_buffer(msg1.clone(), "notif_1".to_string())
            .await;
        let id2 = session
            .add_to_buffer(msg2.clone(), "notif_2".to_string())
            .await;

        assert_eq!(id1, "notif_1");
        assert_eq!(id2, "notif_2");

        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer[0].id, "notif_1");
        assert_eq!(buffer[1].id, "notif_2");
    }

    // ============================================================================
    // Buffer Pruning Tests
    // ============================================================================

    #[tokio::test]
    async fn test_prune_buffer_basic() {
        let session = create_test_session();

        // Add three messages
        let msg1 = create_test_notification("session/update", "sess_123");
        let msg2 = create_test_notification("session/update", "sess_123");
        let msg3 = create_test_notification("session/update", "sess_123");

        session.add_to_buffer(msg1, "notif_1".to_string()).await;
        session.add_to_buffer(msg2, "notif_2".to_string()).await;
        session.add_to_buffer(msg3, "notif_3".to_string()).await;

        // Prune up to and including notif_1
        session.prune_buffer("notif_1").await;

        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer[0].id, "notif_2");
        assert_eq!(buffer[1].id, "notif_3");
    }

    #[tokio::test]
    async fn test_prune_buffer_unknown_id() {
        let session = create_test_session();

        // Add three messages
        for i in 1..=3 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Try to prune with an unknown ID (should be a no-op)
        session.prune_buffer("notif_unknown").await;

        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer[0].id, "notif_1");
        assert_eq!(buffer[1].id, "notif_2");
        assert_eq!(buffer[2].id, "notif_3");
    }

    #[tokio::test]
    async fn test_prune_buffer_empty() {
        let session = create_test_session();

        // Pruning an empty buffer should not panic
        session.prune_buffer("notif_1").await;

        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 0);
    }

    #[tokio::test]
    async fn test_prune_buffer_all() {
        let session = create_test_session();

        // Add three messages
        for i in 1..=3 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Prune all messages
        session.prune_buffer("notif_3").await;

        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 0);
    }

    // ============================================================================
    // Buffer Retrieval Tests
    // ============================================================================

    #[tokio::test]
    async fn test_get_buffered_messages_after_basic() {
        let session = create_test_session();

        // Add five messages
        for i in 1..=5 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Get messages after notif_2
        let buffer = session.message_buffer.read().await;
        let messages = SessionState::get_buffered_messages_after(&buffer, "notif_2");

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].id, "notif_3");
        assert_eq!(messages[1].id, "notif_4");
        assert_eq!(messages[2].id, "notif_5");
    }

    #[tokio::test]
    async fn test_get_buffered_messages_after_unknown_id() {
        let session = create_test_session();

        // Add three messages
        for i in 1..=3 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Get messages with unknown ID (should return all messages)
        let buffer = session.message_buffer.read().await;
        let messages = SessionState::get_buffered_messages_after(&buffer, "notif_unknown");

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].id, "notif_1");
        assert_eq!(messages[1].id, "notif_2");
        assert_eq!(messages[2].id, "notif_3");
    }

    #[tokio::test]
    async fn test_get_buffered_messages_after_last_id() {
        let session = create_test_session();

        // Add three messages
        for i in 1..=3 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Get messages after the last ID (should return empty)
        let buffer = session.message_buffer.read().await;
        let messages = SessionState::get_buffered_messages_after(&buffer, "notif_3");

        assert_eq!(messages.len(), 0);
    }

    // ============================================================================
    // Combined Operations Test
    // ============================================================================

    #[tokio::test]
    async fn test_buffer_workflow() {
        let session = create_test_session();

        // Step 1: Add initial messages
        for i in 1..=3 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Step 2: Client connects and gets all messages (empty latest_id)
        let buffer = session.message_buffer.read().await;
        let messages = SessionState::get_buffered_messages_after(&buffer, "");
        drop(buffer);
        assert_eq!(messages.len(), 3);

        // Step 3: Client acknowledges up to notif_2
        session.prune_buffer("notif_2").await;

        // Step 4: Verify only notif_3 remains in buffer
        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer[0].id, "notif_3");
        drop(buffer); // Release lock

        // Step 5: Add more messages
        for i in 4..=6 {
            let msg = create_test_notification("session/update", "sess_123");
            session.add_to_buffer(msg, format!("notif_{}", i)).await;
        }

        // Step 6: Client reconnects and gets messages after notif_2
        let buffer = session.message_buffer.read().await;
        let messages = SessionState::get_buffered_messages_after(&buffer, "notif_2");
        drop(buffer);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].id, "notif_3");
        assert_eq!(messages[1].id, "notif_4");
        assert_eq!(messages[2].id, "notif_5");
        assert_eq!(messages[3].id, "notif_6");

        // Step 7: Client acknowledges all messages
        session.prune_buffer("notif_6").await;

        // Step 8: Verify buffer is empty
        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 0);
    }

    #[tokio::test]
    async fn test_buffer_with_different_message_types() {
        let session = create_test_session();

        // Add notification
        let notif = create_test_notification("session/update", "sess_123");
        session.add_to_buffer(notif, "notif_1".to_string()).await;

        // Add response
        let response = create_test_response(42);
        session.add_to_buffer(response, "notif_2".to_string()).await;

        // Add another notification
        let notif2 = create_test_notification("session/complete", "sess_123");
        session.add_to_buffer(notif2, "notif_3".to_string()).await;

        // Verify all messages are in buffer
        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 3);

        // Verify message types are preserved
        match &buffer[0].message {
            JsonRpcMessage::Notification(_) => {}
            _ => panic!("Expected notification"),
        }
        match &buffer[1].message {
            JsonRpcMessage::Response(_) => {}
            _ => panic!("Expected response"),
        }
        match &buffer[2].message {
            JsonRpcMessage::Notification(_) => {}
            _ => panic!("Expected notification"),
        }
    }

    // ============================================================================
    // ProcessState ID Mapping Tests
    // ============================================================================

    #[tokio::test]
    async fn test_process_generate_proxy_id() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());

        let id1 = process.generate_proxy_id();
        let id2 = process.generate_proxy_id();
        let id3 = process.generate_proxy_id();

        assert_eq!(id1, Value::Number(serde_json::Number::from(1)));
        assert_eq!(id2, Value::Number(serde_json::Number::from(2)));
        assert_eq!(id3, Value::Number(serde_json::Number::from(3)));
    }

    #[tokio::test]
    async fn test_process_id_mapping_basic() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());
        let ws_id = Uuid::new_v4();
        let client_id = Value::String("client_1".to_string());

        let proxy_id = process.map_client_id_to_proxy(ws_id, client_id.clone(), None);

        // Should be able to resolve back
        let resolved = process.resolve_proxy_id_to_client(&proxy_id);
        assert!(resolved.is_some());
        let (resolved_ws, resolved_client) = resolved.unwrap();
        assert_eq!(resolved_ws, ws_id);
        assert_eq!(resolved_client, client_id);
    }

    #[tokio::test]
    async fn test_process_id_mapping_with_session() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());
        let ws_id = Uuid::new_v4();
        let client_id = Value::String("client_1".to_string());
        let session_id = "sess_123".to_string();

        let proxy_id =
            process.map_client_id_to_proxy(ws_id, client_id.clone(), Some(session_id.clone()));

        // Should have session mapping
        let session_mapping = process.proxy_to_session_ids.get(&proxy_id);
        assert!(session_mapping.is_some());
        let (mapped_session, mapped_client) = session_mapping.unwrap().clone();
        assert_eq!(mapped_session, session_id);
        assert_eq!(mapped_client, client_id);
    }

    #[tokio::test]
    async fn test_process_id_cleanup() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());
        let ws_id = Uuid::new_v4();
        let client_id = Value::String("client_1".to_string());
        let session_id = "sess_123".to_string();

        let proxy_id = process.map_client_id_to_proxy(ws_id, client_id, Some(session_id));

        // Verify mappings exist
        assert!(process.resolve_proxy_id_to_client(&proxy_id).is_some());
        assert!(process.proxy_to_session_ids.contains_key(&proxy_id));

        // Cleanup
        process.cleanup_id_mappings(&proxy_id);

        // Verify mappings are removed
        assert!(process.resolve_proxy_id_to_client(&proxy_id).is_none());
        assert!(!process.proxy_to_session_ids.contains_key(&proxy_id));
    }

    #[tokio::test]
    async fn test_process_multiple_clients_same_process() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());

        let ws_id1 = Uuid::new_v4();
        let ws_id2 = Uuid::new_v4();
        let client_id1 = Value::String("1".to_string());
        let client_id2 = Value::String("1".to_string());

        let proxy_id1 = process.map_client_id_to_proxy(ws_id1, client_id1.clone(), None);
        let proxy_id2 = process.map_client_id_to_proxy(ws_id2, client_id2.clone(), None);

        // Should have different proxy IDs
        assert_ne!(proxy_id1, proxy_id2);

        // Both should resolve correctly
        let (resolved_ws1, resolved_client1) =
            process.resolve_proxy_id_to_client(&proxy_id1).unwrap();
        let (resolved_ws2, resolved_client2) =
            process.resolve_proxy_id_to_client(&proxy_id2).unwrap();

        assert_eq!(resolved_ws1, ws_id1);
        assert_eq!(resolved_client1, client_id1);
        assert_eq!(resolved_ws2, ws_id2);
        assert_eq!(resolved_client2, client_id2);
    }

    // ============================================================================
    // Message Extraction Tests
    // ============================================================================

    #[test]
    fn test_extract_session_id_from_request() {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(serde_json::Number::from(1)),
            method: "session/prompt".to_string(),
            params: Some(json!({
                "sessionId": "sess_123",
                "prompt": "test"
            })),
        };

        let session_id = extract_session_id_from_request(&request);
        assert_eq!(session_id, Some("sess_123".to_string()));
    }

    #[test]
    fn test_extract_session_id_from_request_missing() {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(serde_json::Number::from(1)),
            method: "initialize".to_string(),
            params: Some(json!({
                "protocolVersion": 1
            })),
        };

        let session_id = extract_session_id_from_request(&request);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_from_notification() {
        let notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "session/update".to_string(),
            params: Some(json!({
                "sessionId": "sess_456",
                "update": "data"
            })),
        };

        let message = JsonRpcMessage::Notification(notification);
        let session_id = extract_session_id_from_message(&message);
        assert_eq!(session_id, Some("sess_456".to_string()));
    }

    #[test]
    fn test_inject_notification_id() {
        let mut notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "session/update".to_string(),
            params: Some(json!({
                "sessionId": "sess_123",
                "update": "test"
            })),
        };

        inject_notification_id(&mut notification, "notif_42".to_string());

        let params = notification.params.unwrap();
        let meta = params.get("_meta").unwrap();
        let notif_id = meta.get("tidewave.ai/notificationId").unwrap();
        assert_eq!(notif_id, "notif_42");
    }

    #[test]
    fn test_inject_notification_id_with_existing_meta() {
        let mut notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "session/update".to_string(),
            params: Some(json!({
                "sessionId": "sess_123",
                "_meta": {
                    "existing": "value"
                }
            })),
        };

        inject_notification_id(&mut notification, "notif_99".to_string());

        let params = notification.params.unwrap();
        let meta = params.get("_meta").unwrap();

        // Should preserve existing meta
        assert_eq!(meta.get("existing").unwrap(), "value");

        // Should add notification ID
        assert_eq!(meta.get("tidewave.ai/notificationId").unwrap(), "notif_99");
    }
}
