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

To start an agent, we hijack init requests and dynamically start a new
ACP process in case there's no existing one running.

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

5. Exit notifications

    To tell a client when an ACP process dies, we send a custom `_tidewave.ai/exit` notification:

    {
      "jsonrpc": "2.0",
      "method": "_tidewave.ai/exit",
      "params": {
        "error": "process_exit",
        "message": "ACP process exited with code 137"
      }
    }

6. Exit request

    To allow a client to stop an ACP process (for example in order to restart
    after logging in in Claude Code), we add a `_tidewave.ai/exit` request.

    This will stop the ACP process for that WebSocket and send exit notifications to all
    connected clients. When they reconnect, a new process will be started.

    {
      "jsonrpc": "2.0",
      "id": ...,
      "method": "_tidewave.ai/exit",
      "params": {}
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
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use phoenix_rs::{Channel, HandleResult, InfoSender, JoinResult, SocketRef};
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
    sync::{mpsc, Mutex, RwLock},
};
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

// ============================================================================
// Version
// ============================================================================

/// Returns the Tidewave CLI version from Cargo.toml
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

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

// ============================================================================
// Regular ACP Message Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionResponse {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

// ============================================================================
// Process Starter Abstraction
// ============================================================================

pub type ProcessIo = (
    Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    Option<ChildProcess>,
);

pub type ProcessStarterFn = Arc<
    dyn Fn(
            TidewaveSpawnOptions,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ProcessIo>> + Send>>
        + Send
        + Sync,
>;

// ============================================================================
// Channel Info Message Type
// ============================================================================

/// Message sent from background tasks to the channel's handle_info
#[derive(Debug)]
pub enum AcpChannelInfo {
    /// A JSON-RPC message from the ACP process
    JsonRpc(JsonRpcMessage),
}

// ============================================================================
// Server State Types
// ============================================================================

pub type ChannelId = Uuid;
pub type ProcessKey = String;
pub type SessionId = String;
pub type NotificationId = String;

#[derive(Clone)]
pub struct AcpChannelState {
    /// Active ACP processes (process_key -> process_state)
    pub processes: Arc<DashMap<ProcessKey, Arc<ProcessState>>>,
    /// Channel senders (channel_id -> info_sender)
    pub channel_senders: Arc<DashMap<ChannelId, InfoSender<AcpChannelInfo>>>,
    /// Sessions (session_id -> session_state)
    pub sessions: Arc<DashMap<SessionId, Arc<SessionState>>>,
    /// Session to Channel mapping (session_id -> channel_id)
    pub session_to_channel: Arc<DashMap<SessionId, ChannelId>>,
    /// Channel to Process mapping (channel_id -> process_key)
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
    pub stdin_tx: Arc<RwLock<Option<mpsc::UnboundedSender<JsonRpcMessage>>>>,
    pub exit_tx: Arc<RwLock<Option<mpsc::UnboundedSender<()>>>>,
    pub next_proxy_id: Arc<AtomicU64>,
    pub client_to_proxy_ids: Arc<DashMap<(ChannelId, Value), Value>>,
    pub proxy_to_client_ids: Arc<DashMap<Value, (ChannelId, Value)>>,
    pub proxy_to_session_ids: Arc<DashMap<Value, (SessionId, Value)>>,
    pub cached_init_response: Arc<RwLock<Option<JsonRpcResponse>>>,
    pub stdout_buffer: Arc<RwLock<Vec<String>>>,
    pub stderr_buffer: Arc<RwLock<Vec<String>>>,
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
        Self {
            key,
            spawn_opts,
            child: Arc::new(RwLock::new(None)),
            stdin_tx: Arc::new(RwLock::new(None)),
            exit_tx: Arc::new(RwLock::new(None)),
            next_proxy_id: Arc::new(AtomicU64::new(1)),
            client_to_proxy_ids: Arc::new(DashMap::new()),
            proxy_to_client_ids: Arc::new(DashMap::new()),
            proxy_to_session_ids: Arc::new(DashMap::new()),
            cached_init_response: Arc::new(RwLock::new(None)),
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
        if let Some(index) = buffer.iter().position(|msg| msg.id == latest_id) {
            buffer.drain(0..=index);
        }
    }

    pub fn get_buffered_messages_after(
        buffer: &[BufferedMessage],
        latest_id: &str,
    ) -> Vec<BufferedMessage> {
        if let Some(index) = buffer.iter().position(|msg| msg.id == latest_id) {
            buffer.iter().skip(index + 1).cloned().collect()
        } else {
            buffer.iter().cloned().collect()
        }
    }
}

// ============================================================================
// Phoenix Channel Implementation
// ============================================================================

/// Phoenix channel handler for ACP connections.
pub struct AcpChannel {
    state: AcpChannelState,
}

impl AcpChannel {
    pub fn new() -> Self {
        Self {
            state: AcpChannelState::new(),
        }
    }

    pub fn with_state(state: AcpChannelState) -> Self {
        Self { state }
    }
}

impl Default for AcpChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for AcpChannel {
    async fn join(&self, topic: &str, _payload: Value, socket: &mut SocketRef) -> JoinResult {
        // Extract acp_id from topic "acp:{acp_id}"
        let _acp_id = match topic.strip_prefix("acp:") {
            Some(id) => id.to_string(),
            None => {
                return JoinResult::error(json!({
                    "reason": "invalid topic format, expected acp:{acp_id}"
                }));
            }
        };

        let channel_id = Uuid::new_v4();
        socket.assign("channel_id", channel_id);

        debug!("ACP channel join: channel_id={}", channel_id);

        // Get info sender for forwarding messages from the ACP process
        let info_sender = match socket.info_sender::<AcpChannelInfo>() {
            Some(sender) => sender,
            None => {
                return JoinResult::error(json!({
                    "reason": "failed to get info sender"
                }));
            }
        };

        // Store the info sender for this channel
        self.state
            .channel_senders
            .insert(channel_id, info_sender.clone());

        // The actual process initialization happens when the client sends
        // an "initialize" request via the jsonrpc event
        JoinResult::ok(json!({
            "channel_id": channel_id.to_string(),
            "version": version()
        }))
    }

    async fn handle_in(&self, event: &str, payload: Value, socket: &mut SocketRef) -> HandleResult {
        let channel_id = match socket.get_assign::<ChannelId>("channel_id") {
            Some(id) => *id,
            None => {
                return HandleResult::error(json!({
                    "reason": "no channel_id in assigns"
                }));
            }
        };

        match event {
            "jsonrpc" => {
                // Parse the JSON-RPC message
                let message: JsonRpcMessage = match serde_json::from_value(payload.clone()) {
                    Ok(msg) => msg,
                    Err(e) => {
                        return HandleResult::error(json!({
                            "reason": format!("failed to parse JSON-RPC message: {}", e)
                        }));
                    }
                };

                trace!(
                    "Received jsonrpc from channel {}: {:?}",
                    channel_id,
                    message
                );

                // Handle the message
                if let Err(e) = self
                    .handle_client_message(&self.state, channel_id, message, socket)
                    .await
                {
                    error!("Error handling client message: {}", e);
                }

                HandleResult::no_reply()
            }
            _ => {
                warn!("Unknown event in ACP channel: {}", event);
                HandleResult::no_reply()
            }
        }
    }

    async fn handle_info(&self, message: Box<dyn std::any::Any + Send>, socket: &mut SocketRef) {
        if let Ok(info) = message.downcast::<AcpChannelInfo>() {
            match *info {
                AcpChannelInfo::JsonRpc(json_rpc) => {
                    let payload = match serde_json::to_value(&json_rpc) {
                        Ok(v) => v,
                        Err(e) => {
                            error!("Failed to serialize JSON-RPC message: {}", e);
                            return;
                        }
                    };
                    socket.push("jsonrpc", payload);
                }
            }
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut SocketRef) {
        debug!("ACP channel terminating: {}", reason);

        let channel_id = match socket.get_assign::<ChannelId>("channel_id") {
            Some(id) => *id,
            None => return,
        };

        // Cleanup
        self.state.channel_senders.remove(&channel_id);
        self.state.channel_to_process.remove(&channel_id);

        // Capture sessions for this channel and remove mappings
        let mut sessions_for_channel = Vec::new();
        for entry in self.state.session_to_channel.iter() {
            if *entry.value() == channel_id {
                if let Some(session) = self.state.sessions.get(entry.key()) {
                    sessions_for_channel.push((
                        entry.key().clone(),
                        session.cancel_counter.load(Ordering::Relaxed),
                    ));
                }
            }
        }

        for (session_id, _counter) in &sessions_for_channel {
            self.state.session_to_channel.remove(session_id);
        }

        // Spawn a task to send session/cancel after 10 seconds
        let state_clone = self.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

            for (session_id, counter) in sessions_for_channel {
                if state_clone.session_to_channel.contains_key(&session_id) {
                    debug!("Skipping session/cancel for {} (reconnected)", session_id);
                } else if let Some(session_state) = state_clone.sessions.get(&session_id) {
                    if session_state.cancel_counter.load(Ordering::Relaxed) != counter {
                        debug!(
                            "Skipping session/cancel for {} because counter does not match!",
                            session_id
                        );
                        continue;
                    }

                    let process_key = &session_state.process_key;
                    if let Some(process_state) = state_clone.processes.get(process_key) {
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
    }
}

impl AcpChannel {
    async fn handle_client_message(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        message: JsonRpcMessage,
        socket: &mut SocketRef,
    ) -> Result<()> {
        match &message {
            JsonRpcMessage::Request(req) => {
                debug!("Handling request: {} with method {}", req.id, req.method);
                self.handle_client_request(state, channel_id, req, socket)
                    .await
            }
            JsonRpcMessage::Notification(notif) => {
                debug!("Handling notification with method {}", notif.method);
                self.handle_client_notification(state, channel_id, notif)
                    .await
            }
            JsonRpcMessage::Response(resp) => {
                debug!("Forwarding response for ID {} to process", resp.id);
                self.forward_response_to_process(state, channel_id, resp)
                    .await
            }
        }
    }

    async fn handle_client_request(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        request: &JsonRpcRequest,
        socket: &mut SocketRef,
    ) -> Result<()> {
        match request.method.as_str() {
            "initialize" => {
                self.handle_initialize_request(state, channel_id, request, socket)
                    .await
            }
            "_tidewave.ai/session/load" => {
                self.handle_tidewave_session_load(state, channel_id, request)
                    .await
            }
            "session/load" => {
                self.handle_acp_session_load(state, channel_id, request)
                    .await
            }
            "_tidewave.ai/exit" => self.handle_exit_request(state, channel_id).await,
            _ => {
                self.handle_regular_request(state, channel_id, request)
                    .await
            }
        }
    }

    async fn handle_initialize_request(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        request: &JsonRpcRequest,
        socket: &mut SocketRef,
    ) -> Result<()> {
        let init_params = request.params.as_ref().unwrap_or(&Value::Null);

        let spawn_opts = init_params
            .get("_meta")
            .and_then(|meta| meta.get("tidewave.ai/spawn"))
            .ok_or_else(|| anyhow!("Missing _meta.tidewave.ai/spawn in initialize request"))?;

        let spawn_opts: TidewaveSpawnOptions = serde_json::from_value(spawn_opts.clone())
            .map_err(|e| anyhow!("Invalid spawn options format: {}", e))?;

        debug!(
            "Handling initialize request for command: {}",
            spawn_opts.command
        );
        let process_key = generate_process_key(&spawn_opts.command, &spawn_opts.cwd, init_params);
        trace!("Generated process key: {}", process_key);

        socket.assign("process_key", process_key.clone());

        // Acquire or create a lock for this process_key
        let lock = state
            .process_start_locks
            .entry(process_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let guard = lock.lock().await;

        let result = self
            .handle_initialize_request_locked(state, channel_id, spawn_opts, request, &process_key)
            .await;

        drop(guard);
        state.process_start_locks.remove(&process_key);

        result
    }

    async fn handle_initialize_request_locked(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        spawn_opts: TidewaveSpawnOptions,
        request: &JsonRpcRequest,
        process_key: &ProcessKey,
    ) -> Result<()> {
        // Check if we already have a process for this command + params
        if let Some(process_state) = state.processes.get(process_key) {
            debug!("Found existing process for key: {}", process_key);
            if let Some(cached_response) = process_state.cached_init_response.read().await.as_ref()
            {
                let mut response = cached_response.clone();
                response.id = request.id.clone();

                self.send_to_channel(state, channel_id, JsonRpcMessage::Response(response));

                state
                    .channel_to_process
                    .insert(channel_id, process_key.clone());

                return Ok(());
            }
        }

        // Need to start a new process
        let process_state = match state.processes.get(process_key) {
            Some(_existing) => {
                let response = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id.clone(),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32004,
                        message: "Process alive, but no init response.".to_string(),
                        data: None,
                    }),
                };

                self.send_to_channel(state, channel_id, JsonRpcMessage::Response(response));

                return Ok(());
            }
            None => {
                let new_process =
                    Arc::new(ProcessState::new(process_key.clone(), spawn_opts.clone()));

                match self
                    .start_acp_process(new_process.clone(), state.clone())
                    .await
                {
                    Ok(()) => {
                        state
                            .processes
                            .insert(process_key.clone(), new_process.clone());
                        new_process
                    }
                    Err(e) => {
                        self.send_exit_notification(
                            state,
                            channel_id,
                            "process_start_failed",
                            &e.to_string(),
                            None,
                            None,
                        );
                        return Ok(());
                    }
                }
            }
        };

        state
            .channel_to_process
            .insert(channel_id, process_key.clone());

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
            self.send_exit_notification(
                state,
                channel_id,
                "communication_error",
                "Failed to communicate with process",
                None,
                None,
            );
        }

        Ok(())
    }

    async fn handle_tidewave_session_load(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        request: &JsonRpcRequest,
    ) -> Result<()> {
        let params: TidewaveSessionLoadRequest =
            serde_json::from_value(request.params.clone().unwrap_or(Value::Null))
                .map_err(|e| anyhow!("Invalid session/load params: {}", e))?;

        let session_state = match state.sessions.get(&params.session_id) {
            Some(session) => {
                let state = session.clone();
                state.cancel_counter.fetch_add(1, Ordering::SeqCst);
                state
            }
            None => {
                self.send_error_response(
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

        if !self.ensure_session_not_active(state, channel_id, request, &params.session_id) {
            return Ok(());
        }

        let was_cancelled = session_state.cancelled.swap(false, Ordering::SeqCst);

        if let Some(sender) = state.channel_senders.get(&channel_id) {
            state
                .channel_to_process
                .insert(channel_id, session_state.process_key.clone());

            {
                let buffer = session_state.message_buffer.read().await;
                let buffered_messages =
                    SessionState::get_buffered_messages_after(&buffer, &params.latest_id);

                for buffered in buffered_messages {
                    let _ = sender.send(AcpChannelInfo::JsonRpc(buffered.message));
                }

                state
                    .session_to_channel
                    .insert(params.session_id.clone(), channel_id);
            }

            let response_data = TidewaveSessionLoadResponse {
                cancelled: was_cancelled,
            };

            let success_response = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                result: serde_json::to_value(response_data).ok(),
                error: None,
            };

            let _ = sender.send(AcpChannelInfo::JsonRpc(JsonRpcMessage::Response(
                success_response,
            )));
        }

        Ok(())
    }

    async fn handle_acp_session_load(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        request: &JsonRpcRequest,
    ) -> Result<()> {
        let session_id = extract_session_id_from_request(request);

        if let Some(session_id) = session_id {
            if !self.ensure_session_not_active(state, channel_id, request, &session_id) {
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
                    let process_key = match self.find_process_key_for_channel(state, channel_id) {
                        Some(key) => key,
                        None => {
                            warn!(
                                "session/load: no process mapping found for channel {}",
                                channel_id
                            );
                            return self
                                .handle_regular_request(state, channel_id, request)
                                .await;
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

            state
                .session_to_channel
                .insert(session_id.clone(), channel_id);
            state.channel_to_process.insert(channel_id, process_key);

            info!(
                "Mapped channel {} to session {} for session/load",
                channel_id, session_id
            );
        }

        self.handle_regular_request(state, channel_id, request)
            .await
    }

    async fn handle_exit_request(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
    ) -> Result<()> {
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

        Ok(())
    }

    async fn handle_regular_request(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        request: &JsonRpcRequest,
    ) -> Result<()> {
        let process_state = self.ensure_process_for_channel(state, channel_id)?;

        let session_id = extract_session_id_from_request(request);
        let proxy_id = process_state.map_client_id_to_proxy(
            channel_id,
            request.id.clone(),
            session_id.clone(),
        );
        let mut proxy_request = request.clone();
        proxy_request.id = proxy_id.clone();

        match request.method.as_str() {
            "session/new" => {
                process_state.new_request_ids.insert(proxy_id.clone());
            }
            "session/load" => {
                if let Some(session_id) = session_id {
                    process_state
                        .load_request_ids
                        .insert(proxy_id.clone(), session_id);
                }
            }
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

        if let Err(e) = process_state
            .send_to_process(JsonRpcMessage::Request(proxy_request))
            .await
        {
            error!("Failed to send request to process: {}", e);
        }

        Ok(())
    }

    async fn handle_client_notification(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        notification: &JsonRpcNotification,
    ) -> Result<()> {
        match notification.method.as_str() {
            "_tidewave.ai/ack" => {
                self.handle_ack_notification(state, channel_id, notification)
                    .await
            }
            _ => {
                self.forward_notification_to_process(state, channel_id, notification)
                    .await
            }
        }
    }

    async fn handle_ack_notification(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        notification: &JsonRpcNotification,
    ) -> Result<()> {
        let params: TidewaveAckNotification =
            serde_json::from_value(notification.params.clone().unwrap_or(Value::Null))
                .map_err(|e| anyhow!("Invalid ack params: {}", e))?;

        if let Some(session_state) = state.sessions.get(&params.session_id) {
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
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        notification: &JsonRpcNotification,
    ) -> Result<()> {
        let process_state = self.ensure_process_for_channel(state, channel_id)?;

        process_state
            .send_to_process(JsonRpcMessage::Notification(notification.clone()))
            .await?;

        Ok(())
    }

    async fn forward_response_to_process(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        response: &JsonRpcResponse,
    ) -> Result<()> {
        let process_state = self.ensure_process_for_channel(state, channel_id)?;

        process_state
            .send_to_process(JsonRpcMessage::Response(response.clone()))
            .await?;

        Ok(())
    }

    // ============================================================================
    // Process Management
    // ============================================================================

    async fn start_acp_process(
        &self,
        process_state: Arc<ProcessState>,
        state: AcpChannelState,
    ) -> Result<()> {
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

        // Create exit channel
        let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<()>();
        *process_state.exit_tx.write().await = Some(exit_tx);

        // Start process exit monitor
        let process_state_exit = process_state.clone();
        let state_exit = state.clone();
        tokio::spawn(async move {
            let exit_reason = {
                let mut child_guard = process_state_exit.child.write().await;
                if let Some(process) = child_guard.as_mut() {
                    tokio::select! {
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
                        _ = exit_rx.recv() => {
                            debug!("Exit signal received for process: {}", process_state_exit.key);
                            child_guard.take();
                            ("exit_requested", "ACP process was stopped by exit request".to_string())
                        }
                    }
                } else {
                    return;
                }
            };

            let (error_type, exit_message) = exit_reason;

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

            // Notify all channels connected to this process
            let channels_to_notify: Vec<ChannelId> = state_exit
                .channel_to_process
                .iter()
                .filter(|entry| entry.value() == &process_state_exit.key)
                .map(|entry| *entry.key())
                .collect();

            for channel_id in channels_to_notify {
                let exit_notification = JsonRpcNotification {
                    jsonrpc: "2.0".to_string(),
                    method: "_tidewave.ai/exit".to_string(),
                    params: Some(
                        serde_json::to_value(TidewaveExitParams {
                            error: error_type.to_string(),
                            message: exit_message.clone(),
                            stdout: stdout.clone(),
                            stderr: stderr.clone(),
                        })
                        .unwrap(),
                    ),
                };

                if let Some(sender) = state_exit.channel_senders.get(&channel_id) {
                    let _ = sender.send(AcpChannelInfo::JsonRpc(JsonRpcMessage::Notification(
                        exit_notification,
                    )));
                }
            }

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

            state_exit.processes.remove(&process_state_exit.key);

            debug!(
                "Process exit handler ended, cleaned up {} sessions",
                sessions_to_remove.len()
            );
        });

        Ok(())
    }

    // ============================================================================
    // Helper Methods
    // ============================================================================

    fn send_to_channel(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        message: JsonRpcMessage,
    ) {
        if let Some(sender) = state.channel_senders.get(&channel_id) {
            let _ = sender.send(AcpChannelInfo::JsonRpc(message));
        }
    }

    fn send_error_response(
        &self,
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

        self.send_to_channel(state, channel_id, JsonRpcMessage::Response(response));
    }

    fn send_exit_notification(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        error_type: &str,
        message: &str,
        stdout: Option<String>,
        stderr: Option<String>,
    ) {
        let exit_notification = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "_tidewave.ai/exit".to_string(),
            params: Some(
                serde_json::to_value(TidewaveExitParams {
                    error: error_type.to_string(),
                    message: message.to_string(),
                    stdout,
                    stderr,
                })
                .unwrap(),
            ),
        };

        self.send_to_channel(
            state,
            channel_id,
            JsonRpcMessage::Notification(exit_notification),
        );
    }

    fn ensure_session_not_active(
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
        request: &JsonRpcRequest,
        session_id: &str,
    ) -> bool {
        if state.session_to_channel.contains_key(session_id) {
            self.send_error_response(
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

    fn ensure_process_for_channel(
        &self,
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
        &self,
        state: &AcpChannelState,
        channel_id: ChannelId,
    ) -> Option<ProcessKey> {
        state
            .channel_to_process
            .get(&channel_id)
            .map(|key| key.clone())
    }
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

async fn handle_process_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
) -> Result<()> {
    if let Some((channel_id, client_id)) = process_state.resolve_proxy_id_to_client(&response.id) {
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
            let _ = sender.send(AcpChannelInfo::JsonRpc(JsonRpcMessage::Response(
                client_response,
            )));
        } else {
            handle_disconnected_client_response(process_state, state, response).await;
        }

        process_state.cleanup_id_mappings(&response.id);
    }

    Ok(())
}

async fn maybe_handle_init_response(
    process_state: &Arc<ProcessState>,
    response: &JsonRpcResponse,
    client_response: &mut JsonRpcResponse,
) {
    let init_request_id = process_state.init_request_id.read().await;
    if init_request_id.as_ref() == Some(&response.id) {
        drop(init_request_id);
        inject_tidewave_version(client_response, version());
        inject_proxy_capabilities(client_response);
        *process_state.cached_init_response.write().await = Some(client_response.clone());
        *process_state.stderr_buffer.write().await = Vec::new();
        *process_state.stdout_buffer.write().await = Vec::new();
    }
}

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
                let _ = sender.send(AcpChannelInfo::JsonRpc(JsonRpcMessage::Response(
                    client_response.clone(),
                )));
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

async fn maybe_handle_session_new_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
    client_response: &JsonRpcResponse,
    channel_id: ChannelId,
) {
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

async fn handle_disconnected_client_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
) {
    debug!("Missing original channel for request {}", response.id);
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
                let _ = sender.send(AcpChannelInfo::JsonRpc(JsonRpcMessage::Response(
                    client_response,
                )));
            }
        } else {
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

        process_state.proxy_to_session_ids.remove(&response.id);
    }
}

async fn handle_process_notification_or_request(
    state: &AcpChannelState,
    message: JsonRpcMessage,
) -> Result<()> {
    let session_id = extract_session_id_from_message(&message);

    if let Some(session_id) = session_id {
        if let Some(session_state) = state.sessions.get(&session_id) {
            let session_state = session_state.clone();

            let mut routed_message = message.clone();
            let buffer_id = if let JsonRpcMessage::Notification(ref mut n) = routed_message {
                let notif_id = session_state.generate_notification_id();
                inject_notification_id(n, notif_id.clone());
                notif_id
            } else {
                match &routed_message {
                    JsonRpcMessage::Request(req) => req.id.to_string(),
                    JsonRpcMessage::Response(resp) => resp.id.to_string(),
                    _ => unreachable!(),
                }
            };

            let _buffer_id = session_state
                .add_to_buffer(routed_message.clone(), buffer_id)
                .await;

            if let Some(channel_id) = state.session_to_channel.get(&session_id) {
                if let Some(sender) = state.channel_senders.get(&channel_id) {
                    let _ = sender.send(AcpChannelInfo::JsonRpc(routed_message));
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

pub fn generate_process_key(command: &str, cwd: &str, init_params: &Value) -> ProcessKey {
    let mut params_without_spawn_meta = init_params.clone();
    if let Some(obj) = params_without_spawn_meta.as_object_mut() {
        if let Some(meta) = obj.get_mut("_meta") {
            if let Some(meta_obj) = meta.as_object_mut() {
                meta_obj.remove("tidewave.ai/spawn");
                if meta_obj.is_empty() {
                    obj.remove("_meta");
                }
            }
        }
    }

    format!(
        "{}:{}:{}",
        command,
        cwd,
        serde_json::to_string(&params_without_spawn_meta).unwrap_or_default()
    )
}

fn inject_tidewave_version(response: &mut JsonRpcResponse, version: &str) {
    if let Some(result) = &mut response.result {
        if let Some(result_obj) = result.as_object_mut() {
            let mut meta_obj = result_obj
                .get("_meta")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            let mut tidewave_obj = Map::new();
            tidewave_obj.insert("version".to_string(), Value::String(version.to_string()));
            meta_obj.insert("tidewave.ai".to_string(), Value::Object(tidewave_obj));

            result_obj.insert("_meta".to_string(), Value::Object(meta_obj));
        }
    }
}

fn inject_proxy_capabilities(response: &mut JsonRpcResponse) {
    if let Some(result) = &mut response.result {
        if let Some(result_obj) = result.as_object_mut() {
            if let Some(agent_caps) = result_obj.get_mut("agentCapabilities") {
                if let Some(caps_obj) = agent_caps.as_object_mut() {
                    let mut meta_obj = caps_obj
                        .get("_meta")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();

                    let mut tidewave_obj = Map::new();
                    tidewave_obj.insert("exit".to_string(), Value::Bool(true));
                    meta_obj.insert("tidewave.ai".to_string(), Value::Object(tidewave_obj));

                    caps_obj.insert("_meta".to_string(), Value::Object(meta_obj));
                }
            }
        }
    }
}

fn inject_notification_id(notification: &mut JsonRpcNotification, notif_id: NotificationId) {
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
