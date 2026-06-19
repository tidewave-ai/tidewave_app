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
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
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
    pub wsl_distro: Option<String>,
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
    /// Process starter function for creating new ACP processes
    pub process_starter: ProcessStarterFn,
    /// Locks to prevent multiple concurrent process starts for the same process_key
    pub process_lifecycle_locks: Arc<DashMap<ProcessKey, Arc<Mutex<()>>>>,
}

impl AcpChannelState {
    pub fn new() -> Self {
        Self::with_process_starter(real_process_starter())
    }

    pub fn with_process_starter(process_starter: ProcessStarterFn) -> Self {
        Self {
            processes: Arc::new(DashMap::new()),
            channel_senders: Arc::new(DashMap::new()),
            process_starter,
            process_lifecycle_locks: Arc::new(DashMap::new()),
        }
    }
}

pub struct ProcessState {
    pub key: ProcessKey,
    pub spawn_opts: TidewaveSpawnOptions,
    pub child: RwLock<Option<ChildProcess>>,
    /// Channel used to forward a message to the ACP process.
    pub stdin_tx: RwLock<Option<mpsc::UnboundedSender<JsonRpcMessage>>>,
    /// Channel used to signal the exit monitor to kill the process.
    pub exit_tx: RwLock<Option<mpsc::UnboundedSender<()>>>,
    /// Broadcast channel used to notify all subscribed init loops about process exit.
    pub exit_broadcast: broadcast::Sender<AgentExitEvent>,

    // In-flight requests keyed by proxy ID for multiplexed connections.
    pub next_proxy_id: AtomicU64,
    pub inflight_requests: DashMap<Value, InflightRequest>,

    /// Sessions owned by this ACP process.
    pub sessions: RwLock<HashMap<SessionId, SessionEntry>>,
    /// Locks to prevent races between session/close and session reconnect.
    pub session_lifecycle_locks: RwLock<HashMap<SessionId, Arc<Mutex<()>>>>,

    /// Initialization state shared by all clients connected to this ACP process.
    pub init_state: InitState,

    /// Buffers for stdout/stderr output before init completes.
    /// Used to provide detailed error messages when process exits before init.
    pub stdout_buffer: RwLock<Vec<String>>,
    pub stderr_buffer: RwLock<Vec<String>>,

    /// Epoch counter bumped each time a client connects. Used to invalidate
    /// pending inactivity timers when a new client arrives.
    pub connect_epoch: AtomicU64,

    /// Channels currently connected to this process. Used to broadcast
    /// process messages that are not scoped to a particular session.
    pub channels: RwLock<HashSet<ChannelId>>,
}

pub struct SessionState {
    pub process_key: ProcessKey,
    pub message_buffer: RwLock<Vec<BufferedMessage>>,
    pub notification_id_counter: AtomicU64,
    pub cancelled: AtomicBool,
    pub cancel_counter: AtomicU64,
}

pub struct SessionEntry {
    pub state: Arc<SessionState>,
    pub channel_id: Option<ChannelId>,
}

#[derive(Debug, Clone)]
pub struct InflightRequest {
    pub channel_id: ChannelId,
    pub client_id: Value,
    pub session_id: Option<SessionId>,
    pub pending: Option<PendingRequest>,
}

pub struct InitState {
    inner: Mutex<InitStateInner>,
    notify: Notify,
}

#[derive(Debug, Clone)]
pub struct InitComplete {
    pub response: JsonRpcResponse,
    pub capabilities: AgentCapabilities,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentCapabilities {
    /// Whether the agent supports resuming sessions (loadSession, session.fork,
    /// or session.resume in agentCapabilities).
    /// When true, the process can be stopped on inactivity since sessions
    /// can be restored after a restart.
    pub supports_resuming: bool,
    /// Whether the agent supports session/close (session.close in agentCapabilities).
    /// When true, on disconnect we send session/close instead of session/cancel,
    /// and remove the session state entirely.
    pub supports_session_close: bool,
}

impl AgentCapabilities {
    pub fn from_response(response: &JsonRpcResponse) -> Self {
        Self {
            supports_resuming: check_supports_resuming(response),
            supports_session_close: check_supports_session_close(response),
        }
    }
}

enum InitStateInner {
    NotStarted,
    InFlight { request_id: Value },
    Complete { init: InitComplete },
    Failed,
}

pub enum BeginInit {
    Start,
    Wait,
    Complete(InitComplete),
    Failed,
}

#[derive(Debug, Clone)]
pub enum PendingRequest {
    SessionNew,
    SessionLoad {
        session_id: SessionId,
        created_session: bool,
    },
    SessionResume {
        session_id: SessionId,
        created_session: bool,
    },
    SessionFork,
    SessionClose {
        session_id: SessionId,
    },
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
            child: RwLock::new(None),
            stdin_tx: RwLock::new(None),
            exit_tx: RwLock::new(None),
            exit_broadcast,
            next_proxy_id: AtomicU64::new(1),
            inflight_requests: DashMap::new(),
            sessions: RwLock::new(HashMap::new()),
            session_lifecycle_locks: RwLock::new(HashMap::new()),
            init_state: InitState::new(),
            stdout_buffer: RwLock::new(Vec::new()),
            stderr_buffer: RwLock::new(Vec::new()),
            connect_epoch: AtomicU64::new(0),
            channels: RwLock::new(HashSet::new()),
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
        pending: Option<PendingRequest>,
    ) -> Value {
        let proxy_id = self.generate_proxy_id();
        self.register_inflight_request(
            proxy_id.clone(),
            channel_id,
            client_id,
            session_id,
            pending,
        );
        proxy_id
    }

    pub fn register_inflight_request(
        &self,
        proxy_id: Value,
        channel_id: ChannelId,
        client_id: Value,
        session_id: Option<SessionId>,
        pending: Option<PendingRequest>,
    ) {
        self.inflight_requests.insert(
            proxy_id,
            InflightRequest {
                channel_id,
                client_id,
                session_id,
                pending,
            },
        );
    }

    pub fn inflight_request(&self, proxy_id: &Value) -> Option<InflightRequest> {
        self.inflight_requests
            .get(proxy_id)
            .map(|entry| entry.value().clone())
    }

    pub fn remove_inflight_request(&self, proxy_id: &Value) -> Option<InflightRequest> {
        self.inflight_requests
            .remove(proxy_id)
            .map(|(_, request)| request)
    }

    pub async fn add_channel(&self, channel_id: ChannelId) {
        self.channels.write().await.insert(channel_id);
    }

    pub async fn remove_channel(&self, channel_id: ChannelId) {
        self.channels.write().await.remove(&channel_id);
    }

    pub async fn connected_channels(&self) -> Vec<ChannelId> {
        self.channels.read().await.iter().copied().collect()
    }

    pub async fn session_state(&self, session_id: &str) -> Option<Arc<SessionState>> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .map(|entry| entry.state.clone())
    }

    pub async fn session_channel(&self, session_id: &str) -> Option<ChannelId> {
        self.sessions
            .read()
            .await
            .get(session_id)
            .and_then(|entry| entry.channel_id)
    }

    pub async fn has_session_channel(&self, session_id: &str) -> bool {
        self.session_channel(session_id).await.is_some()
    }

    pub async fn map_session_channel(&self, session_id: &str, channel_id: ChannelId) {
        if let Some(entry) = self.sessions.write().await.get_mut(session_id) {
            entry.channel_id = Some(channel_id);
        }
    }

    pub async fn unmap_session_if_owned_by(&self, session_id: &str, channel_id: ChannelId) -> bool {
        let mut sessions = self.sessions.write().await;

        let Some(entry) = sessions.get_mut(session_id) else {
            return false;
        };

        if entry.channel_id == Some(channel_id) {
            entry.channel_id = None;
            return true;
        }

        false
    }

    pub async fn claim_or_insert_session_channel(
        &self,
        session_id: SessionId,
        channel_id: ChannelId,
        process_key: &ProcessKey,
    ) -> Result<bool, ()> {
        let mut sessions = self.sessions.write().await;

        if let Some(entry) = sessions.get_mut(&session_id) {
            if entry.channel_id.is_some() {
                return Err(());
            }

            entry.channel_id = Some(channel_id);
            return Ok(false);
        }

        sessions.insert(
            session_id,
            SessionEntry {
                state: Arc::new(SessionState::new(process_key.clone())),
                channel_id: Some(channel_id),
            },
        );
        Ok(true)
    }

    pub async fn unmap_sessions_for_channel(&self, channel_id: ChannelId) -> Vec<(SessionId, u64)> {
        let mut sessions = self.sessions.write().await;
        let mut unmapped = Vec::new();

        for (session_id, entry) in sessions.iter_mut() {
            if entry.channel_id == Some(channel_id) {
                entry.channel_id = None;
                unmapped.push((
                    session_id.clone(),
                    entry.state.cancel_counter.load(Ordering::Relaxed),
                ));
            }
        }

        unmapped
    }

    pub async fn insert_session(
        &self,
        session_id: SessionId,
        session_state: Arc<SessionState>,
        channel_id: Option<ChannelId>,
    ) -> bool {
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&session_id) {
            return false;
        }

        sessions.insert(
            session_id,
            SessionEntry {
                state: session_state,
                channel_id,
            },
        );
        true
    }

    pub async fn remove_session(&self, session_id: &str) {
        self.sessions.write().await.remove(session_id);
        self.session_lifecycle_locks
            .write()
            .await
            .remove(session_id);
    }

    pub async fn remove_session_if_unowned_or_owned_by(
        &self,
        session_id: &str,
        channel_id: ChannelId,
    ) -> bool {
        let should_remove = {
            let mut sessions = self.sessions.write().await;

            let should_remove = sessions.get(session_id).is_some_and(|entry| {
                entry.channel_id.is_none() || entry.channel_id == Some(channel_id)
            });

            if should_remove {
                sessions.remove(session_id);
            }

            should_remove
        };

        if should_remove {
            self.session_lifecycle_locks
                .write()
                .await
                .remove(session_id);
        }

        should_remove
    }

    pub async fn session_lifecycle_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_lifecycle_locks.write().await;
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

impl InitState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(InitStateInner::NotStarted),
            notify: Notify::new(),
        }
    }

    pub async fn begin(&self, request_id: Value) -> BeginInit {
        let mut inner = self.inner.lock().await;

        match &*inner {
            InitStateInner::NotStarted => {
                *inner = InitStateInner::InFlight { request_id };
                BeginInit::Start
            }
            InitStateInner::InFlight { .. } => BeginInit::Wait,
            InitStateInner::Complete { init } => BeginInit::Complete(init.clone()),
            InitStateInner::Failed => BeginInit::Failed,
        }
    }

    pub async fn wait_for_completion(&self) -> Option<InitComplete> {
        loop {
            let notified = {
                let inner = self.inner.lock().await;

                match &*inner {
                    InitStateInner::Complete { init } => return Some(init.clone()),
                    InitStateInner::Failed => return None,
                    InitStateInner::NotStarted | InitStateInner::InFlight { .. } => {
                        self.notify.notified()
                    }
                }
            };

            notified.await;
        }
    }

    pub async fn complete_if_current(
        &self,
        request_id: &Value,
        response: JsonRpcResponse,
        capabilities: AgentCapabilities,
    ) -> bool {
        let mut inner = self.inner.lock().await;

        let InitStateInner::InFlight {
            request_id: current_request_id,
        } = &*inner
        else {
            return false;
        };

        if current_request_id != request_id {
            return false;
        }

        *inner = InitStateInner::Complete {
            init: InitComplete {
                response,
                capabilities,
            },
        };
        drop(inner);
        self.notify.notify_waiters();
        true
    }

    pub async fn is_current_request(&self, request_id: &Value) -> bool {
        let inner = self.inner.lock().await;

        matches!(
            &*inner,
            InitStateInner::InFlight {
                request_id: current_request_id,
            } if current_request_id == request_id
        )
    }

    pub async fn is_in_flight(&self) -> bool {
        matches!(*self.inner.lock().await, InitStateInner::InFlight { .. })
    }

    pub async fn fail(&self) {
        let mut inner = self.inner.lock().await;
        *inner = InitStateInner::Failed;
        drop(inner);
        self.notify.notify_waiters();
    }

    pub async fn is_complete(&self) -> bool {
        matches!(*self.inner.lock().await, InitStateInner::Complete { .. })
    }

    pub async fn capabilities(&self) -> Option<AgentCapabilities> {
        let inner = self.inner.lock().await;

        match &*inner {
            InitStateInner::Complete { init } => Some(init.capabilities),
            InitStateInner::NotStarted
            | InitStateInner::InFlight { .. }
            | InitStateInner::Failed => None,
        }
    }
}

impl SessionState {
    pub fn new(process_key: ProcessKey) -> Self {
        Self {
            process_key,
            message_buffer: RwLock::new(Vec::new()),
            notification_id_counter: AtomicU64::new(1),
            cancelled: AtomicBool::new(false),
            cancel_counter: AtomicU64::new(0),
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

    // Parse spawn options from join payload
    let spawn_opts: TidewaveSpawnOptions =
        match serde_json::from_value(msg.payload.clone().into_json()) {
            Ok(opts) => opts,
            Err(e) => {
                return InitResult::Error(format!("invalid spawn options: {}", e));
            }
        };

    // Store the sender for this channel
    state.channel_senders.insert(channel_id, sender);
    // Generate process key and start/reuse process
    let process_key = format!("{}:{}", &spawn_opts.command, &spawn_opts.cwd);

    // Acquire or create a lock for this process_key
    let lock = state
        .process_lifecycle_locks
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
                state.channel_senders.remove(&channel_id);
                return InitResult::Error(format!("failed to start process: {}", e));
            }
        }
    }

    // Subscribe to exit broadcast and bump epoch while still holding the
    // lifecycle lock.  This prevents the inactivity timer (which also acquires
    // this lock) from removing the process between lock release and subscribe.
    let mut exit_rx = {
        let process_state = state.processes.get(&process_key).map(|p| p.clone());
        match process_state {
            Some(ps) => {
                // Bump connect epoch to invalidate any pending inactivity timer
                ps.connect_epoch.fetch_add(1, Ordering::SeqCst);
                ps.exit_broadcast.subscribe()
            }
            None => {
                // Process already gone
                drop(guard);
                state.channel_senders.remove(&channel_id);
                return InitResult::Error("process exited before channel init".to_string());
            }
        }
    };

    drop(guard);

    let process_state = match state.processes.get(&process_key) {
        Some(process) => process.clone(),
        None => {
            state.channel_senders.remove(&channel_id);
            return InitResult::Error("process exited before channel init".to_string());
        }
    };

    // Track this channel so process messages without a sessionId can be
    // broadcast to all connected clients.
    process_state.add_channel(channel_id).await;

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
                                    match serde_json::from_value(phx_msg.payload.clone().into_json()) {
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

                                if let Err(e) = handle_client_message(state, channel_id, message, &process_key).await {
                                    error!("Error handling client message: {}", e);
                                }
                            }
                            "exit" => {
                                if let Err(e) = handle_exit_request(state, &process_key).await {
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

    // Cleanup
    debug!("ACP channel terminating for channel_id: {}", channel_id);

    // Drop our exit broadcast subscription before checking receiver count,
    // so our own subscription is not counted.
    drop(exit_rx);

    state.channel_senders.remove(&channel_id);
    process_state.remove_channel(channel_id).await;

    // Capture sessions for this channel and remove channel ownership.
    let sessions_for_channel = process_state.unmap_sessions_for_channel(channel_id).await;

    // Spawn a task to send session/cancel or session/close after 10 seconds
    let process_state_clone = process_state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        for (session_id, counter) in sessions_for_channel {
            stop_unmapped_session_if_still_disconnected(
                process_state_clone.clone(),
                session_id,
                counter,
            )
            .await;
        }
    });

    // If this was the last connected client, spawn a 1-minute inactivity timer
    // that stops the process if the agent supports resuming sessions.
    if let Some(process_state) = state.processes.get(&process_key) {
        if process_state.exit_broadcast.receiver_count() == 0 {
            let epoch = process_state.connect_epoch.load(Ordering::SeqCst);

            if process_state.init_state.is_in_flight().await {
                let state_clone = state.clone();
                let process_state = process_state.clone();
                let process_key = process_key.clone();

                tokio::spawn(async move {
                    if process_state
                        .init_state
                        .wait_for_completion()
                        .await
                        .is_some_and(|init| init.capabilities.supports_resuming)
                    {
                        spawn_process_stop_after_inactivity(
                            state_clone,
                            process_state,
                            process_key,
                            epoch,
                        );
                    }
                });
            } else if process_state
                .init_state
                .capabilities()
                .await
                .is_some_and(|capabilities| capabilities.supports_resuming)
            {
                spawn_process_stop_after_inactivity(
                    state.clone(),
                    process_state.clone(),
                    process_key.clone(),
                    epoch,
                );
            }
        }
    }

    result
}

async fn stop_unmapped_session_if_still_disconnected(
    process_state: Arc<ProcessState>,
    session_id: SessionId,
    counter: u64,
) {
    // Check if session is still unmapped (not reconnected)
    if process_state.has_session_channel(&session_id).await {
        debug!("Skipping session/cancel for {} (reconnected)", session_id);
        return;
    }

    let Some(session_state) = process_state.session_state(&session_id).await else {
        return;
    };

    // Check if cancel_counter matches (session hasn't been reloaded)
    if session_state.cancel_counter.load(Ordering::Relaxed) != counter {
        debug!(
            "Skipping session/cancel for {} because counter does not match!",
            session_id
        );
        return;
    }

    // Acquire session lock to prevent races between stop and reconnect
    let lock = process_state.session_lifecycle_lock(&session_id).await;
    let _guard = lock.lock().await;

    if process_state.has_session_channel(&session_id).await {
        debug!(
            "Skipping session/cancel for {} (reconnected after lifecycle lock)",
            session_id
        );
        return;
    }

    // Check if agent supports session/close
    let supports_stop = process_state
        .init_state
        .capabilities()
        .await
        .is_some_and(|capabilities| capabilities.supports_session_close);

    if supports_stop {
        // Send session/close as a request (no proxy ID mapping — the
        // response will be silently ignored since there is no client).
        let proxy_id = process_state.generate_proxy_id();
        let stop_request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: proxy_id,
            method: "session/close".to_string(),
            params: Some(json!({
                "sessionId": session_id
            })),
        };

        if let Err(e) = process_state
            .send_to_process(JsonRpcMessage::Request(stop_request))
            .await
        {
            error!("Failed to send session/close for {}: {}", session_id, e);
        } else {
            debug!("Sent session/close for unmapped session: {}", session_id);
        }

        // Remove session state entirely — session is permanently gone
        process_state.remove_session(&session_id).await;
    } else {
        // Mark session as cancelled (keep state for potential reconnect)
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

fn spawn_process_stop_after_inactivity(
    state: AcpChannelState,
    process_state: Arc<ProcessState>,
    process_key: ProcessKey,
    epoch: u64,
) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

        // Acquire the lifecycle lock so we don't race with a new
        // client that is between process lookup and exit_broadcast
        // subscribe (where it bumps connect_epoch).
        let lock = state
            .process_lifecycle_locks
            .entry(process_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        // Re-check epoch now that we hold the lock – a client that
        // connected while we were waiting will have bumped it.
        if process_state.connect_epoch.load(Ordering::SeqCst) != epoch {
            debug!(
                "Skipping process stop for {} (client reconnected)",
                process_key
            );
            return;
        }

        if !process_state
            .init_state
            .capabilities()
            .await
            .is_some_and(|capabilities| capabilities.supports_resuming)
        {
            debug!(
                "Skipping process stop for {} (agent does not support resuming)",
                process_key
            );
            return;
        }

        info!(
            "Stopping process {} after 1 minute of inactivity",
            process_key
        );

        // Remove from state first so new clients start a fresh process
        // instead of connecting to one that's about to be killed.
        state.processes.remove(&process_key);

        if let Some(exit_tx) = process_state.exit_tx.read().await.as_ref() {
            let _ = exit_tx.send(());
        }
    });
}

// ============================================================================
// Client Message Handlers
// ============================================================================

async fn handle_client_message(
    state: &AcpChannelState,
    channel_id: ChannelId,
    message: JsonRpcMessage,
    process_key: &ProcessKey,
) -> Result<()> {
    match &message {
        JsonRpcMessage::Request(req) => {
            debug!("Handling request: {} with method {}", req.id, req.method);
            handle_client_request(state, channel_id, req, process_key).await
        }
        JsonRpcMessage::Notification(notif) => {
            debug!("Handling notification with method {}", notif.method);
            handle_client_notification(state, channel_id, notif, process_key).await
        }
        JsonRpcMessage::Response(resp) => {
            // Forward client responses (e.g., permission responses) back to the process.
            // Note that we don't need to perform ID mapping here, because the process is
            // the one that generated the request ID, so it will necessarily be unique.
            debug!("Forwarding response for ID {} to process", resp.id);
            forward_response_to_process(state, resp, process_key).await
        }
    }
}

async fn handle_client_request(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    process_key: &ProcessKey,
) -> Result<()> {
    match request.method.as_str() {
        // We handle init because we need to
        //   1. Start a new process in case there's no running one for the given parameters.
        //   2. In case we start, store the request ID to store the response later on.
        "initialize" => handle_initialize_request(state, channel_id, request, process_key).await,
        // Our custom session load handler
        "_tidewave.ai/session/load" => {
            handle_tidewave_session_load(state, channel_id, request, process_key).await
        }
        // ACP session load. We need to intercept it because we need to update the session mapping.
        "session/load" => {
            handle_acp_session_load_or_resume(state, channel_id, request, process_key).await
        }
        // ACP session resume also needs to atomically claim the session before forwarding.
        "session/resume" => {
            handle_acp_session_load_or_resume(state, channel_id, request, process_key).await
        }
        // Any other requests only perform proxy_id mapping and are otherwise forwarded as is.
        _ => handle_regular_request(state, channel_id, request, process_key).await,
    }
}

async fn handle_initialize_request(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    process_key: &ProcessKey,
) -> Result<()> {
    // Process was already started during channel join
    let process_state = ensure_process(state, process_key)?;

    let proxy_id = process_state.generate_proxy_id();

    match process_state.init_state.begin(proxy_id.clone()).await {
        BeginInit::Complete(cached_init) => {
            let mut response = cached_init.response;
            response.id = request.id.clone();
            send_to_channel(state, channel_id, JsonRpcMessage::Response(response));
        }
        BeginInit::Failed => {
            send_agent_exit(
                state,
                channel_id,
                "init_error",
                "Process init failed",
                None,
                None,
            );
        }
        BeginInit::Start => {
            // We won the race — send the init request to the process
            let session_id = extract_session_id_from_request(request);
            process_state.register_inflight_request(
                proxy_id.clone(),
                channel_id,
                request.id.clone(),
                session_id,
                None,
            );
            let mut proxy_request = request.clone();
            proxy_request.id = proxy_id;

            if let Err(e) = process_state
                .send_to_process(JsonRpcMessage::Request(proxy_request))
                .await
            {
                process_state.init_state.fail().await;
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
        }
        BeginInit::Wait => {
            // Another client already sent the init request — wait for the response
            if let Some(cached_init) = process_state.init_state.wait_for_completion().await {
                let mut response = cached_init.response;
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
    }

    Ok(())
}

async fn handle_tidewave_session_load(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    process_key: &ProcessKey,
) -> Result<()> {
    let process_state = ensure_process(state, process_key)?;
    let params: TidewaveSessionLoadRequest =
        serde_json::from_value(request.params.clone().unwrap_or(Value::Null))
            .map_err(|e| anyhow!("Invalid session/load params: {}", e))?;

    // Acquire session lock to prevent races with session/close on disconnect
    let lock = process_state
        .session_lifecycle_lock(&params.session_id)
        .await;
    let _guard = lock.lock().await;

    let session_state = match process_state.session_state(&params.session_id).await {
        Some(s) => {
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

    if !ensure_session_not_active(
        &process_state,
        state,
        channel_id,
        request,
        &params.session_id,
    )
    .await
    {
        return Ok(());
    }

    let was_cancelled = session_state.cancelled.swap(false, Ordering::SeqCst);

    if let Some(sender) = state.channel_senders.get(&channel_id) {
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
            process_state
                .map_session_channel(&params.session_id, channel_id)
                .await;
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

async fn handle_acp_session_load_or_resume(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    process_key: &ProcessKey,
) -> Result<()> {
    let session_id = extract_session_id_from_request(request);

    if let Some(session_id) = session_id {
        let process_state = ensure_process(state, process_key)?;
        let lock = process_state.session_lifecycle_lock(&session_id).await;
        let _guard = lock.lock().await;

        let created_session = match process_state
            .claim_or_insert_session_channel(session_id.clone(), channel_id, process_key)
            .await
        {
            Ok(false) => {
                info!(
                    "session/load for existing session {} on channel {}",
                    session_id, channel_id
                );
                false
            }
            Ok(true) => {
                info!(
                    "Created new session {} for session/load on channel {}",
                    session_id, channel_id
                );
                true
            }
            Err(()) => {
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
                return Ok(());
            }
        };

        // Claim this channel for the session BEFORE forwarding the request
        // This ensures that when the agent sends notifications during load/resume,
        // we can route them to the correct websocket
        info!(
            "Mapped channel {} to session {} for {}",
            channel_id, session_id, request.method
        );

        let pending = match request.method.as_str() {
            "session/load" => Some(PendingRequest::SessionLoad {
                session_id: session_id.clone(),
                created_session,
            }),
            "session/resume" => Some(PendingRequest::SessionResume {
                session_id: session_id.clone(),
                created_session,
            }),
            _ => None,
        };
        forward_request_to_process(state, channel_id, request, process_key, pending).await?;
        return Ok(());
    }

    // Forward the request to the agent as a regular request
    handle_regular_request(state, channel_id, request, process_key).await
}

async fn handle_exit_request(state: &AcpChannelState, process_key: &ProcessKey) -> Result<()> {
    let process_state = match state.processes.get(process_key) {
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
    process_key: &ProcessKey,
) -> Result<()> {
    // Map client ID to proxy ID
    let session_id = extract_session_id_from_request(request);

    let pending = match request.method.as_str() {
        // We intercept new sessions to map the sessionId to the channel
        "session/new" => Some(PendingRequest::SessionNew),
        // We intercept load requests since we need to handle the unsuccessful case
        // and clear the session mapping.
        "session/load" => session_id
            .clone()
            .map(|session_id| PendingRequest::SessionLoad {
                session_id,
                created_session: false,
            }),
        // We intercept resume / fork sessions to map the sessionId to the channel
        "session/resume" => session_id
            .clone()
            .map(|session_id| PendingRequest::SessionResume {
                session_id,
                created_session: false,
            }),
        "session/fork" => Some(PendingRequest::SessionFork),
        // We intercept session/close to remove session state on response.
        "session/close" => session_id
            .clone()
            .map(|session_id| PendingRequest::SessionClose { session_id }),
        _ => None,
    };

    forward_request_to_process(state, channel_id, request, process_key, pending).await
}

async fn forward_request_to_process(
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    process_key: &ProcessKey,
    pending: Option<PendingRequest>,
) -> Result<()> {
    let process_state = ensure_process(state, process_key)?;
    let session_id = extract_session_id_from_request(request);

    let proxy_id =
        process_state.map_client_id_to_proxy(channel_id, request.id.clone(), session_id, pending);
    let mut proxy_request = request.clone();
    proxy_request.id = proxy_id.clone();

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
    process_key: &ProcessKey,
) -> Result<()> {
    match notification.method.as_str() {
        "_tidewave.ai/ack" => {
            handle_ack_notification(state, channel_id, notification, process_key).await
        }
        _ => forward_notification_to_process(state, notification, process_key).await,
    }
}

async fn handle_ack_notification(
    state: &AcpChannelState,
    channel_id: ChannelId,
    notification: &JsonRpcNotification,
    process_key: &ProcessKey,
) -> Result<()> {
    let process_state = ensure_process(state, process_key)?;
    let params: TidewaveAckNotification =
        serde_json::from_value(notification.params.clone().unwrap_or(Value::Null))
            .map_err(|e| anyhow!("Invalid ack params: {}", e))?;

    // Find the specific session to prune
    if let Some(session_state) = process_state.session_state(&params.session_id).await {
        // Verify this websocket is actually connected to this session
        if let Some(mapped_channel_id) = process_state.session_channel(&params.session_id).await {
            if mapped_channel_id == channel_id {
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
    notification: &JsonRpcNotification,
    process_key: &ProcessKey,
) -> Result<()> {
    let process_state = ensure_process(state, process_key)?;

    process_state
        .send_to_process(JsonRpcMessage::Notification(notification.clone()))
        .await?;

    Ok(())
}

async fn forward_response_to_process(
    state: &AcpChannelState,
    response: &JsonRpcResponse,
    process_key: &ProcessKey,
) -> Result<()> {
    let process_state = ensure_process(state, process_key)?;

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
                if !process_state_clone.init_state.is_complete().await {
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
            if !process_state_stderr.init_state.is_complete().await {
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
        let (stdout, stderr) = if !process_state_exit.init_state.is_complete().await {
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

        // Acquire the lifecycle lock so that broadcast + remove is atomic
        // with respect to new clients subscribing to exit_broadcast.
        let lock = state_exit
            .process_lifecycle_locks
            .entry(process_state_exit.key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        // Broadcast exit event to all subscribed init loops
        let _ = process_state_exit.exit_broadcast.send(AgentExitEvent {
            error: error_type.to_string(),
            message: exit_message,
            stdout,
            stderr,
        });

        let sessions_to_remove = process_state_exit.sessions.read().await.len();
        process_state_exit.sessions.write().await.clear();
        process_state_exit
            .session_lifecycle_locks
            .write()
            .await
            .clear();

        // Notify any waiters for init response so they don't hang forever.
        process_state_exit.init_state.fail().await;

        state_exit.processes.remove(&process_state_exit.key);

        debug!(
            "Process exit handler ended, cleaned up {} sessions",
            sessions_to_remove
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
async fn ensure_session_not_active(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    channel_id: ChannelId,
    request: &JsonRpcRequest,
    session_id: &str,
) -> bool {
    if process_state.has_session_channel(session_id).await {
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

/// Looks up the process for the given process key.
fn ensure_process(state: &AcpChannelState, process_key: &ProcessKey) -> Result<Arc<ProcessState>> {
    match state.processes.get(process_key) {
        Some(process) => Ok(process.clone()),
        None => Err(anyhow!("Process not found for key: {}", process_key)),
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
            handle_process_notification_or_request(process_state, state, message).await
        }
    }
}

/// Handle response from process - map proxy ID back to client ID and route
async fn handle_process_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    response: &JsonRpcResponse,
) -> Result<()> {
    if let Some(inflight_request) = process_state.remove_inflight_request(&response.id) {
        // Create response with original client ID
        let mut client_response = response.clone();
        client_response.id = inflight_request.client_id.clone();

        maybe_handle_init_response(process_state, response, &mut client_response).await;
        let response_session_id = maybe_handle_pending_request(
            process_state,
            state,
            inflight_request.channel_id,
            inflight_request.pending.clone(),
            &client_response,
        )
        .await?;
        let disconnected_session_id = response_session_id.or(inflight_request.session_id.clone());

        if let Some(sender) = state.channel_senders.get(&inflight_request.channel_id) {
            push_jsonrpc(&sender, &JsonRpcMessage::Response(client_response));
        } else {
            handle_disconnected_client_response(
                process_state,
                state,
                client_response,
                disconnected_session_id,
            )
            .await;
        }

        // Remove session state after forwarding a session/close response.
        if let Some(PendingRequest::SessionClose { session_id }) = inflight_request.pending {
            process_state.remove_session(&session_id).await;
            info!(
                "Removed session {} after session/close response",
                session_id
            );
        }
    }

    Ok(())
}

/// Handle initialize response - inject version and cache
async fn maybe_handle_init_response(
    process_state: &Arc<ProcessState>,
    response: &JsonRpcResponse,
    client_response: &mut JsonRpcResponse,
) {
    if process_state
        .init_state
        .is_current_request(&response.id)
        .await
    {
        if process_state
            .init_state
            .complete_if_current(
                &response.id,
                client_response.clone(),
                AgentCapabilities::from_response(response),
            )
            .await
        {
            // Clear buffers
            *process_state.stderr_buffer.write().await = Vec::new();
            *process_state.stdout_buffer.write().await = Vec::new();
        }
    }
}

async fn maybe_handle_pending_request(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    channel_id: ChannelId,
    pending_request: Option<PendingRequest>,
    client_response: &JsonRpcResponse,
) -> Result<Option<SessionId>> {
    let Some(pending_request) = pending_request else {
        return Ok(None);
    };

    let mut response_session_id = None;

    match pending_request {
        PendingRequest::SessionNew | PendingRequest::SessionFork => {
            if let Some(result) = &client_response.result {
                if let Ok(session_response) =
                    serde_json::from_value::<NewSessionResponse>(result.clone())
                {
                    response_session_id = Some(session_response.session_id.clone());
                    create_session_from_agent_response(
                        state,
                        session_response.session_id,
                        channel_id,
                        &process_state.key,
                    )
                    .await;
                }
            }
        }
        PendingRequest::SessionLoad {
            session_id,
            created_session,
        }
        | PendingRequest::SessionResume {
            session_id,
            created_session,
        } => {
            if client_response.error.is_some() {
                if created_session {
                    if process_state
                        .remove_session_if_unowned_or_owned_by(&session_id, channel_id)
                        .await
                    {
                        info!("Failed to load session, removing mapping! {}", session_id);
                    } else {
                        info!(
                            "Failed to load session {}, but it is now owned by another channel",
                            session_id
                        );
                    }
                } else if process_state
                    .unmap_session_if_owned_by(&session_id, channel_id)
                    .await
                {
                    info!("Failed to load session, releasing claim! {}", session_id);
                }
            } else {
                confirm_claimed_session_response(process_state, state, session_id, channel_id)
                    .await;
            }
        }
        PendingRequest::SessionClose { .. } => {}
    }

    Ok(response_session_id)
}

async fn create_session_from_agent_response(
    state: &AcpChannelState,
    session_id: SessionId,
    channel_id: ChannelId,
    process_key: &ProcessKey,
) {
    let Ok(process_state) = ensure_process(state, process_key) else {
        warn!(
            "Cannot map session {} without process {}",
            session_id, process_key
        );
        return;
    };

    let inserted = process_state
        .insert_session(
            session_id.clone(),
            Arc::new(SessionState::new(process_key.clone())),
            Some(channel_id),
        )
        .await;

    if !state.channel_senders.contains_key(&channel_id)
        && process_state
            .unmap_session_if_owned_by(&session_id, channel_id)
            .await
    {
        debug!(
            "Created session {} from late agent response without active channel",
            session_id
        );
    }

    if !inserted && process_state.session_channel(&session_id).await != Some(channel_id) {
        warn!(
            "Unexpectedly got new/fork session response for already known session! {}",
            session_id
        );
    }
}

async fn confirm_claimed_session_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    session_id: SessionId,
    channel_id: ChannelId,
) {
    if !state.channel_senders.contains_key(&channel_id)
        && process_state
            .unmap_session_if_owned_by(&session_id, channel_id)
            .await
    {
        debug!(
            "Confirmed session {} from late load/resume response without active channel",
            session_id
        );
        return;
    }

    let current_channel = process_state.session_channel(&session_id).await;

    if current_channel.is_none() {
        warn!(
            "Successful load/resume response for session {} but no channel owns it",
            session_id
        );
        return;
    }

    if current_channel != Some(channel_id) {
        warn!(
            "Successful load/resume response for session {} is now owned by another channel",
            session_id
        );
    }
}

/// Handle response for disconnected client - forward to new channel or buffer
async fn handle_disconnected_client_response(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    client_response: JsonRpcResponse,
    session_id: Option<SessionId>,
) {
    debug!(
        "Missing original channel for request {}",
        client_response.id
    );
    // Fallback: Client disconnected, try to find session and forward to new channel
    if let Some(session_id) = session_id {
        if push_response_to_session_channel(process_state, state, &client_response, &session_id)
            .await
        {
            return;
        }

        if let Some(session_state) = process_state.session_state(&session_id).await {
            let _ = session_state
                .add_to_buffer(
                    JsonRpcMessage::Response(client_response.clone()),
                    client_response.id.to_string(),
                )
                .await;

            if push_response_to_session_channel(process_state, state, &client_response, &session_id)
                .await
            {
                return;
            }
        }
    }
}

async fn push_response_to_session_channel(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    client_response: &JsonRpcResponse,
    session_id: &str,
) -> bool {
    let Some(current_channel_id) = process_state.session_channel(session_id).await else {
        return false;
    };

    let Some(sender) = state.channel_senders.get(&current_channel_id) else {
        return false;
    };

    push_jsonrpc(&sender, &JsonRpcMessage::Response(client_response.clone()));
    true
}

async fn handle_process_notification_or_request(
    process_state: &Arc<ProcessState>,
    state: &AcpChannelState,
    message: JsonRpcMessage,
) -> Result<()> {
    // Handle requests/notifications from process - route by sessionId
    let session_id = extract_session_id_from_message(&message);

    if let Some(session_id) = session_id {
        if let Some(session_state) = process_state.session_state(&session_id).await {
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
            if let Some(channel_id) = process_state.session_channel(&session_id).await {
                if let Some(sender) = state.channel_senders.get(&channel_id) {
                    push_jsonrpc(&sender, &routed_message);
                }
            }
        } else {
            warn!("Session not found for sessionId: {}", session_id);
        }
    } else {
        // No sessionId: forward to every client connected to this process.
        debug!(
            "Message from process missing sessionId, forwarding to all connected clients: {:?}",
            message
        );

        for channel_id in process_state.connected_channels().await {
            if let Some(sender) = state.channel_senders.get(&channel_id) {
                push_jsonrpc(&sender, &message);
            }
        }
    }

    Ok(())
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Check if the agent supports resuming sessions by examining agentCapabilities
/// in the init response. Checks for loadSession, session.fork, or session.resume.
fn check_supports_resuming(response: &JsonRpcResponse) -> bool {
    let caps = response
        .result
        .as_ref()
        .and_then(|r| r.get("agentCapabilities"));

    let Some(caps) = caps else {
        return false;
    };

    // loadSession: true
    if caps.get("loadSession").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }

    // session: { fork: {}, resume: {} }
    if let Some(session) = caps.get("session").and_then(|v| v.as_object()) {
        if session.contains_key("fork") || session.contains_key("resume") {
            return true;
        }
    }

    false
}

/// Check if the agent supports session/close by examining agentCapabilities
/// in the init response. Checks for session.close.
fn check_supports_session_close(response: &JsonRpcResponse) -> bool {
    let caps = response
        .result
        .as_ref()
        .and_then(|r| r.get("agentCapabilities"));

    let Some(caps) = caps else {
        return false;
    };

    if let Some(session) = caps.get("session").and_then(|v| v.as_object()) {
        if session.contains_key("close") {
            return true;
        }
    }

    false
}

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
                spawn_opts.wsl_distro.as_deref(),
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
            wsl_distro: None,
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
    // InitState Tests
    // ============================================================================

    #[tokio::test]
    async fn test_init_state_wait_after_completion_returns_cached_response() {
        let init_state = InitState::new();
        let request_id = Value::String("proxy_init".to_string());
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request_id.clone(),
            result: Some(json!({
                "protocolVersion": 1,
                "agentCapabilities": {}
            })),
            error: None,
        };

        assert!(matches!(
            init_state.begin(request_id.clone()).await,
            BeginInit::Start
        ));
        assert!(
            init_state
                .complete_if_current(
                    &request_id,
                    response.clone(),
                    AgentCapabilities::from_response(&response)
                )
                .await
        );

        let cached = tokio::time::timeout(
            tokio::time::Duration::from_millis(100),
            init_state.wait_for_completion(),
        )
        .await
        .expect("wait should not miss an already completed init")
        .expect("init should be complete");

        assert_eq!(cached.response.id, response.id);
        assert_eq!(cached.response.result, response.result);
        assert!(!cached.capabilities.supports_resuming);
        assert!(!cached.capabilities.supports_session_close);
    }

    #[tokio::test]
    async fn test_init_state_wait_after_failure_returns_none() {
        let init_state = InitState::new();

        assert!(matches!(
            init_state
                .begin(Value::String("proxy_init".to_string()))
                .await,
            BeginInit::Start
        ));
        init_state.fail().await;

        let failed = tokio::time::timeout(tokio::time::Duration::from_millis(100), async {
            init_state.wait_for_completion().await
        })
        .await
        .expect("wait should not hang after init failure");

        assert!(failed.is_none());
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

        let proxy_id = process.map_client_id_to_proxy(ws_id, client_id.clone(), None, None);

        // Should be able to resolve back
        let inflight_request = process.inflight_request(&proxy_id);
        assert!(inflight_request.is_some());
        let inflight_request = inflight_request.unwrap();
        assert_eq!(inflight_request.channel_id, ws_id);
        assert_eq!(inflight_request.client_id, client_id);
        assert_eq!(inflight_request.session_id, None);
        assert!(inflight_request.pending.is_none());
    }

    #[tokio::test]
    async fn test_process_id_mapping_with_session() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());
        let ws_id = Uuid::new_v4();
        let client_id = Value::String("client_1".to_string());
        let session_id = "sess_123".to_string();

        let proxy_id = process.map_client_id_to_proxy(
            ws_id,
            client_id.clone(),
            Some(session_id.clone()),
            None,
        );

        // Should have session mapping
        let inflight_request = process.inflight_request(&proxy_id).unwrap();
        assert_eq!(inflight_request.channel_id, ws_id);
        assert_eq!(inflight_request.client_id, client_id);
        assert_eq!(inflight_request.session_id, Some(session_id));
    }

    #[tokio::test]
    async fn test_process_id_cleanup() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());
        let ws_id = Uuid::new_v4();
        let client_id = Value::String("client_1".to_string());
        let session_id = "sess_123".to_string();

        let proxy_id = process.map_client_id_to_proxy(ws_id, client_id, Some(session_id), None);

        // Verify mapping exists
        assert!(process.inflight_request(&proxy_id).is_some());

        // Cleanup
        process.remove_inflight_request(&proxy_id);

        // Verify mapping is removed
        assert!(process.inflight_request(&proxy_id).is_none());
    }

    #[tokio::test]
    async fn test_disconnected_response_sent_if_session_reconnects_while_buffering() {
        let state = AcpChannelState::new();
        let process = Arc::new(ProcessState::new("test_key".to_string(), test_spawn_opts()));
        let session_id = "sess_reconnect".to_string();
        let session = Arc::new(SessionState::new(process.key.clone()));
        process
            .insert_session(session_id.clone(), session.clone(), None)
            .await;

        let original_channel_id = Uuid::new_v4();
        let reconnected_channel_id = Uuid::new_v4();
        let client_id = Value::String("session/prompt!sess_reconnect!!prompt_1".to_string());
        let proxy_id = process.map_client_id_to_proxy(
            original_channel_id,
            client_id.clone(),
            Some(session_id.clone()),
            None,
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        state.channel_senders.insert(
            reconnected_channel_id,
            ChannelSender {
                tx,
                topic: "acp:test".to_string(),
                join_ref: Some("j2".to_string()),
            },
        );

        let buffer_guard = session.message_buffer.write().await;
        let process_clone = process.clone();
        let state_clone = state.clone();
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: proxy_id.clone(),
            result: Some(json!({ "stopReason": "end_turn" })),
            error: None,
        };
        let response_task = tokio::spawn(async move {
            handle_process_response(&process_clone, &state_clone, &response)
                .await
                .expect("response should be handled");
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        process
            .map_session_channel(&session_id, reconnected_channel_id)
            .await;
        drop(buffer_guard);
        response_task.await.expect("response task panicked");

        let sent = tokio::time::timeout(tokio::time::Duration::from_millis(250), rx.recv())
            .await
            .expect("Expected response to be sent to reconnected channel")
            .expect("Channel sender closed");
        assert_eq!(sent.event, "jsonrpc");
        let payload = sent.payload.as_json();
        assert_eq!(payload["id"], client_id);
        assert_eq!(payload["result"]["stopReason"], "end_turn");
    }

    #[tokio::test]
    async fn test_disconnected_response_buffered_if_mapped_channel_has_no_sender() {
        let state = AcpChannelState::new();
        let process = Arc::new(ProcessState::new("test_key".to_string(), test_spawn_opts()));
        let session_id = "sess_disconnecting".to_string();
        let session = Arc::new(SessionState::new(process.key.clone()));
        let disconnected_channel_id = Uuid::new_v4();
        process
            .insert_session(
                session_id.clone(),
                session.clone(),
                Some(disconnected_channel_id),
            )
            .await;

        let client_id = Value::String("session/prompt!sess_disconnecting!!prompt_1".to_string());
        let proxy_id = process.map_client_id_to_proxy(
            disconnected_channel_id,
            client_id.clone(),
            Some(session_id.clone()),
            None,
        );

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: proxy_id,
            result: Some(json!({ "stopReason": "end_turn" })),
            error: None,
        };

        handle_process_response(&process, &state, &response)
            .await
            .expect("response should be handled");

        let buffer = session.message_buffer.read().await;
        assert_eq!(buffer.len(), 1);
        let JsonRpcMessage::Response(buffered_response) = &buffer[0].message else {
            panic!("expected buffered response");
        };
        assert_eq!(buffered_response.id, client_id);
        assert_eq!(buffered_response.result, response.result);
    }

    #[tokio::test]
    async fn test_message_without_session_id_broadcast_to_connected_channels() {
        let state = AcpChannelState::new();
        let process = Arc::new(ProcessState::new("test_key".to_string(), test_spawn_opts()));

        // Two clients connected to this process.
        let channel_a = Uuid::new_v4();
        let channel_b = Uuid::new_v4();
        process.add_channel(channel_a).await;
        process.add_channel(channel_b).await;

        // A third channel that exists globally but belongs to another process
        // (never registered on this process_state).
        let channel_other = Uuid::new_v4();

        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let (tx_other, mut rx_other) = mpsc::unbounded_channel();
        state.channel_senders.insert(
            channel_a,
            ChannelSender {
                tx: tx_a,
                topic: "acp:test".to_string(),
                join_ref: Some("ja".to_string()),
            },
        );
        state.channel_senders.insert(
            channel_b,
            ChannelSender {
                tx: tx_b,
                topic: "acp:test".to_string(),
                join_ref: Some("jb".to_string()),
            },
        );
        state.channel_senders.insert(
            channel_other,
            ChannelSender {
                tx: tx_other,
                topic: "acp:other".to_string(),
                join_ref: Some("jo".to_string()),
            },
        );

        // A notification from the process that is not scoped to a session.
        let message = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "some/global_notification".to_string(),
            params: Some(json!({ "data": "no_session" })),
        });

        handle_process_notification_or_request(&process, &state, message)
            .await
            .expect("message should be handled");

        for rx in [&mut rx_a, &mut rx_b] {
            let sent = rx.recv().await.expect("Expected message to be forwarded");
            assert_eq!(sent.event, "jsonrpc");
            let payload = sent.payload.as_json();
            assert_eq!(payload["method"], "some/global_notification");
            assert_eq!(payload["params"]["data"], "no_session");
        }

        // A channel not connected to this process must not receive the message.
        assert!(
            rx_other.try_recv().is_err(),
            "channel from another process should not receive the broadcast"
        );
    }

    #[tokio::test]
    async fn test_failed_session_load_cleans_proxy_id_mappings() {
        let state = AcpChannelState::new();
        let process = Arc::new(ProcessState::new("test_key".to_string(), test_spawn_opts()));
        let session_id = "sess_failed_load".to_string();
        let channel_id = Uuid::new_v4();
        let client_id = Value::String("session/load!sess_failed_load!!load_1".to_string());
        let proxy_id = process.map_client_id_to_proxy(
            channel_id,
            client_id.clone(),
            Some(session_id.clone()),
            Some(PendingRequest::SessionLoad {
                session_id: session_id.clone(),
                created_session: true,
            }),
        );

        let session = Arc::new(SessionState::new(process.key.clone()));
        process
            .insert_session(session_id.clone(), session, Some(channel_id))
            .await;

        let (tx, mut rx) = mpsc::unbounded_channel();
        state.channel_senders.insert(
            channel_id,
            ChannelSender {
                tx,
                topic: "acp:test".to_string(),
                join_ref: Some("j1".to_string()),
            },
        );

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: proxy_id.clone(),
            result: None,
            error: Some(JsonRpcError {
                code: -32002,
                message: "Session not found".to_string(),
                data: None,
            }),
        };

        handle_process_response(&process, &state, &response)
            .await
            .expect("Failed session/load response should be handled");

        assert!(process.inflight_request(&proxy_id).is_none());
        assert!(!process.sessions.read().await.contains_key(&session_id));

        let sent = rx.recv().await.expect("Expected response to be forwarded");
        assert_eq!(sent.event, "jsonrpc");
        let payload = sent.payload.as_json();
        assert_eq!(payload["id"], client_id);
        assert_eq!(payload["error"]["message"], "Session not found");
    }

    #[tokio::test]
    async fn test_delayed_cancel_skips_session_that_reconnects_while_waiting_for_lock() {
        let process = Arc::new(ProcessState::new("test_key".to_string(), test_spawn_opts()));
        let session_id = "sess_reconnected".to_string();
        let session = Arc::new(SessionState::new(process.key.clone()));
        process
            .insert_session(session_id.clone(), session.clone(), None)
            .await;

        let lock = process.session_lifecycle_lock(&session_id).await;
        let guard = lock.lock().await;
        let process_clone = process.clone();
        let session_id_clone = session_id.clone();
        let stop_task = tokio::spawn(async move {
            stop_unmapped_session_if_still_disconnected(process_clone, session_id_clone, 0).await;
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        process
            .map_session_channel(&session_id, Uuid::new_v4())
            .await;
        drop(guard);

        stop_task.await.expect("stop task panicked");
        assert!(!session.cancelled.load(Ordering::SeqCst));
        assert!(process.session_state(&session_id).await.is_some());
    }

    #[tokio::test]
    async fn test_process_multiple_clients_same_process() {
        let process = ProcessState::new("test_key".to_string(), test_spawn_opts());

        let ws_id1 = Uuid::new_v4();
        let ws_id2 = Uuid::new_v4();
        let client_id1 = Value::String("1".to_string());
        let client_id2 = Value::String("1".to_string());

        let proxy_id1 = process.map_client_id_to_proxy(ws_id1, client_id1.clone(), None, None);
        let proxy_id2 = process.map_client_id_to_proxy(ws_id2, client_id2.clone(), None, None);

        // Should have different proxy IDs
        assert_ne!(proxy_id1, proxy_id2);

        // Both should resolve correctly
        let resolved1 = process.inflight_request(&proxy_id1).unwrap();
        let resolved2 = process.inflight_request(&proxy_id2).unwrap();

        assert_eq!(resolved1.channel_id, ws_id1);
        assert_eq!(resolved1.client_id, client_id1);
        assert_eq!(resolved2.channel_id, ws_id2);
        assert_eq!(resolved2.client_id, client_id2);
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

    // ============================================================================
    // check_supports_resuming Tests
    // ============================================================================

    fn make_init_response(agent_capabilities: Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(serde_json::Number::from(0)),
            result: Some(json!({
                "protocolVersion": 1,
                "agentCapabilities": agent_capabilities,
            })),
            error: None,
        }
    }

    #[test]
    fn test_supports_resuming_with_load_session() {
        let response = make_init_response(json!({ "loadSession": true }));
        assert!(check_supports_resuming(&response));
    }

    #[test]
    fn test_supports_resuming_with_session_fork() {
        let response = make_init_response(json!({ "session": { "fork": {} } }));
        assert!(check_supports_resuming(&response));
    }

    #[test]
    fn test_supports_resuming_with_session_resume() {
        let response = make_init_response(json!({ "session": { "resume": {} } }));
        assert!(check_supports_resuming(&response));
    }

    #[test]
    fn test_supports_resuming_with_fork_and_resume() {
        let response = make_init_response(json!({ "session": { "fork": {}, "resume": {} } }));
        assert!(check_supports_resuming(&response));
    }

    #[test]
    fn test_supports_resuming_empty_capabilities() {
        let response = make_init_response(json!({}));
        assert!(!check_supports_resuming(&response));
    }

    #[test]
    fn test_supports_resuming_no_result() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(serde_json::Number::from(0)),
            result: None,
            error: None,
        };
        assert!(!check_supports_resuming(&response));
    }

    #[test]
    fn test_supports_resuming_load_session_false() {
        let response = make_init_response(json!({ "loadSession": false }));
        assert!(!check_supports_resuming(&response));
    }

    // ============================================================================
    // check_supports_session_close Tests
    // ============================================================================

    #[test]
    fn test_supports_session_close_with_close() {
        let response = make_init_response(json!({ "session": { "close": {} } }));
        assert!(check_supports_session_close(&response));
    }

    #[test]
    fn test_supports_session_close_with_multiple_capabilities() {
        let response =
            make_init_response(json!({ "session": { "fork": {}, "resume": {}, "close": {} } }));
        assert!(check_supports_session_close(&response));
    }

    #[test]
    fn test_supports_session_close_empty_capabilities() {
        let response = make_init_response(json!({}));
        assert!(!check_supports_session_close(&response));
    }

    #[test]
    fn test_supports_session_close_session_without_close() {
        let response = make_init_response(json!({ "session": { "fork": {} } }));
        assert!(!check_supports_session_close(&response));
    }

    #[test]
    fn test_supports_session_close_no_result() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Value::Number(serde_json::Number::from(0)),
            result: None,
            error: None,
        };
        assert!(!check_supports_session_close(&response));
    }
}
