use agent_client_protocol as acp;
use anyhow::{anyhow, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Json,
    },
};
use dashmap::DashMap;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex, RwLock},
    task::{JoinHandle, LocalSet},
    time::timeout,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ============================================================================
// Types and State
// ============================================================================

#[derive(Clone)]
pub struct AcpServerState {
    /// Active ACP connections (id -> connection)
    pub connections: Arc<DashMap<String, Arc<AcpConnection>>>,
    /// Active sessions (session_id -> session)
    pub sessions: Arc<DashMap<String, Arc<AcpSession>>>,
    /// SSE connections for sessions (session_id -> SSE sender)
    pub sse_connections: Arc<DashMap<String, mpsc::UnboundedSender<SseMessage>>>,
    /// Connection status updates SSE
    pub connection_updates: Arc<RwLock<Vec<mpsc::UnboundedSender<ConnectionUpdate>>>>,
    /// JSON-RPC ID counter
    pub rpc_id_counter: Arc<AtomicU64>,
}

impl AcpServerState {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            sse_connections: Arc::new(DashMap::new()),
            connection_updates: Arc::new(RwLock::new(Vec::new())),
            rpc_id_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn next_rpc_id(&self) -> u64 {
        self.rpc_id_counter.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Loading,
    Connected,
    Error { message: String },
}

pub struct AcpConnection {
    pub id: String,
    pub name: String,
    pub command: String,
    pub status: Arc<RwLock<ConnectionStatus>>,
    pub capabilities: Arc<RwLock<Option<acp::AgentCapabilities>>>,
    /// Channel to send requests to the agent task
    pub agent_tx: mpsc::UnboundedSender<AgentRequest>,
    /// Handle to the process
    pub process_handle: Arc<Mutex<Option<Child>>>,
    /// Handle to the local set running the agent
    pub local_set_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

pub struct AcpSession {
    pub id: String,
    pub acp_id: String,
    pub acp_session_id: acp::SessionId,
    pub pending_requests: Arc<DashMap<u64, oneshot::Sender<Value>>>,
    pub message_buffer: Arc<RwLock<Vec<SseMessage>>>,
    pub sse_id_counter: Arc<AtomicU64>,
}

/// Requests we can send to the agent task
#[derive(Debug)]
pub enum AgentRequest {
    NewSession {
        session_id: String,
        response: oneshot::Sender<Result<acp::SessionId, String>>,
    },
    Prompt {
        session_id: acp::SessionId,
        messages: Vec<acp::ContentBlock>,
        response: oneshot::Sender<Result<(), String>>,
    },
}

/// Client implementation that handles requests from the agent
struct ClientImpl {
    state: AcpServerState,
}

impl ClientImpl {
    fn new(state: AcpServerState) -> Self {
        Self { state }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum SseMessage {
    #[serde(rename = "acp-jsonrpc")]
    AcpJsonRpc {
        id: u64,
        #[serde(rename = "jsonRpcMessage")]
        json_rpc_message: Value,
    },
    #[serde(rename = "exit")]
    Exit { id: u64 },
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectionUpdate {
    pub id: String,
    pub status: ConnectionStatus,
    pub capabilities: Option<acp::AgentCapabilities>,
}

// ============================================================================
// Request/Response Types
// ============================================================================

#[derive(Deserialize)]
pub struct CreateConnectionRequest {
    pub id: String,
    pub name: String,
    pub command: String,
}

#[derive(Serialize)]
pub struct ConnectionInfo {
    pub id: String,
    pub name: String,
    pub command: String,
    pub status: ConnectionStatus,
    pub capabilities: Option<acp::AgentCapabilities>,
}

#[derive(Deserialize)]
pub struct CreateSessionRequest {
    #[serde(rename = "acpId")]
    pub acp_id: String,
}

#[derive(Serialize)]
pub struct CreateSessionResponse {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct SessionUpdateRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Serialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(rename = "acpId")]
    pub acp_id: String,
}

// ============================================================================
// ACP Client Implementation
// ============================================================================

impl acp::Client for ClientImpl {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        // Find the session by ACP session ID
        let session = self
            .state
            .sessions
            .iter()
            .find(|entry| entry.value().acp_session_id == args.session_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| acp::Error::Internal(anyhow!("Session not found")))?;

        let rpc_id = self.state.next_rpc_id();
        let (tx, rx) = oneshot::channel();

        session.pending_requests.insert(rpc_id, tx);

        let sse_id = session.sse_id_counter.fetch_add(1, Ordering::Relaxed);
        let json_rpc = json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session.id,
                "toolCall": args.tool_call,
                "options": args.options,
            }
        });

        let message = SseMessage::AcpJsonRpc {
            id: sse_id,
            json_rpc_message: json_rpc,
        };

        // Send to SSE or buffer
        if let Some(sse_tx) = self.state.sse_connections.get(&session.id) {
            let _ = sse_tx.send(message.clone());
        } else {
            session.message_buffer.write().await.push(message.clone());
        }

        // Wait for response with timeout
        match timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(response)) => {
                if let Some(result) = response.get("result") {
                    serde_json::from_value(result.clone())
                        .map_err(|e| acp::Error::Internal(anyhow!("Failed to parse response: {}", e)))
                } else if let Some(error) = response.get("error") {
                    Err(acp::Error::Internal(anyhow!("RPC error: {:?}", error)))
                } else {
                    Err(acp::Error::Internal(anyhow!("Invalid response format")))
                }
            }
            Ok(Err(_)) => Err(acp::Error::Internal(anyhow!("Response channel closed"))),
            Err(_) => Err(acp::Error::Internal(anyhow!("Request timed out"))),
        }
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "fs.writeTextFile".to_string(),
        })
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "fs.readTextFile".to_string(),
        })
    }

    async fn session_notification(
        &self,
        args: acp::SessionNotification,
    ) -> Result<(), acp::Error> {
        // Find the session by ACP session ID
        let session = self
            .state
            .sessions
            .iter()
            .find(|entry| entry.value().acp_session_id == args.session_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| acp::Error::Internal(anyhow!("Session not found")))?;

        let sse_id = session.sse_id_counter.fetch_add(1, Ordering::Relaxed);
        let json_rpc = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": serde_json::to_value(&args)
                .map_err(|e| acp::Error::Internal(anyhow!("Serialization error: {}", e)))?
        });

        let message = SseMessage::AcpJsonRpc {
            id: sse_id,
            json_rpc_message: json_rpc,
        };

        // Send to SSE or buffer
        if let Some(sse_tx) = self.state.sse_connections.get(&session.id) {
            let _ = sse_tx.send(message.clone());
        } else {
            session.message_buffer.write().await.push(message.clone());
        }

        Ok(())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "terminal".to_string(),
        })
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "terminal".to_string(),
        })
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "terminal".to_string(),
        })
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "terminal".to_string(),
        })
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> Result<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::CapabilityNotSupported {
            capability: "terminal".to_string(),
        })
    }

    async fn ext_method(
        &self,
        _method: Arc<str>,
        _params: Arc<serde_json::value::RawValue>,
    ) -> Result<Arc<serde_json::value::RawValue>, acp::Error> {
        Err(acp::Error::MethodNotFound)
    }

    async fn ext_notification(
        &self,
        _method: Arc<str>,
        _params: Arc<serde_json::value::RawValue>,
    ) -> Result<(), acp::Error> {
        Ok(())
    }
}

// ============================================================================
// HTTP Handlers
// ============================================================================

pub async fn create_connections(
    State(state): State<AcpServerState>,
    Json(requests): Json<Vec<CreateConnectionRequest>>,
) -> Result<Json<Vec<ConnectionInfo>>, StatusCode> {
    let mut results = Vec::new();

    for req in requests {
        match spawn_acp_process(&state, req).await {
            Ok(info) => results.push(info),
            Err(e) => {
                error!("Failed to create connection {}: {}", req.id, e);
                results.push(ConnectionInfo {
                    id: req.id.clone(),
                    name: req.name,
                    command: req.command,
                    status: ConnectionStatus::Error {
                        message: e.to_string(),
                    },
                    capabilities: None,
                });
            }
        }
    }

    Ok(Json(results))
}

pub async fn list_connections(
    State(state): State<AcpServerState>,
) -> Result<Json<Vec<ConnectionInfo>>, StatusCode> {
    let mut connections = Vec::new();

    for entry in state.connections.iter() {
        let conn = entry.value();
        let status = conn.status.read().await.clone();
        let capabilities = conn.capabilities.read().await.clone();

        connections.push(ConnectionInfo {
            id: conn.id.clone(),
            name: conn.name.clone(),
            command: conn.command.clone(),
            status,
            capabilities,
        });
    }

    Ok(Json(connections))
}

pub async fn connection_updates_sse(
    State(state): State<AcpServerState>,
) -> Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ConnectionUpdate>();

    // Add to listeners
    state.connection_updates.write().await.push(tx.clone());

    // Send current state
    for entry in state.connections.iter() {
        let conn = entry.value();
        let _ = tx.send(ConnectionUpdate {
            id: conn.id.clone(),
            status: conn.status.read().await.clone(),
            capabilities: conn.capabilities.read().await.clone(),
        });
    }

    let stream = async_stream::stream! {
        while let Some(update) = rx.recv().await {
            let data = serde_json::to_string(&update).unwrap_or_default();
            yield Ok(Event::default().data(data));
        }
    };

    Sse::new(Box::pin(stream)).keep_alive(KeepAlive::default())
}

pub async fn create_session(
    State(state): State<AcpServerState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, StatusCode> {
    let connection = state
        .connections
        .get(&req.acp_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Check connection status
    let status = connection.status.read().await.clone();
    match status {
        ConnectionStatus::Connected => {
            // Create session through ACP
            let session_id = Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();

            // Send request to agent task
            if let Err(e) = connection.agent_tx.send(AgentRequest::NewSession {
                session_id: session_id.clone(),
                response: tx,
            }) {
                return Ok(Json(CreateSessionResponse {
                    session_id: String::new(),
                    status: "error".to_string(),
                    error: Some(format!("Failed to send request: {}", e)),
                }));
            }

            // Wait for response
            match timeout(Duration::from_secs(5), rx).await {
                Ok(Ok(Ok(acp_session_id))) => {
                    // Create our session tracking
                    let session = Arc::new(AcpSession {
                        id: session_id.clone(),
                        acp_id: req.acp_id.clone(),
                        acp_session_id,
                        pending_requests: Arc::new(DashMap::new()),
                        message_buffer: Arc::new(RwLock::new(Vec::new())),
                        sse_id_counter: Arc::new(AtomicU64::new(1)),
                    });

                    state.sessions.insert(session_id.clone(), session);

                    Ok(Json(CreateSessionResponse {
                        session_id,
                        status: "created".to_string(),
                        error: None,
                    }))
                }
                Ok(Ok(Err(e))) => {
                    // Check if it's an auth error
                    if e.contains("auth_required") {
                        Ok(Json(CreateSessionResponse {
                            session_id: String::new(),
                            status: "auth_required".to_string(),
                            error: Some(e),
                        }))
                    } else {
                        Ok(Json(CreateSessionResponse {
                            session_id: String::new(),
                            status: "error".to_string(),
                            error: Some(e),
                        }))
                    }
                }
                _ => Ok(Json(CreateSessionResponse {
                    session_id: String::new(),
                    status: "error".to_string(),
                    error: Some("Request timed out".to_string()),
                })),
            }
        }
        ConnectionStatus::Error { message } => Ok(Json(CreateSessionResponse {
            session_id: String::new(),
            status: "error".to_string(),
            error: Some(message),
        })),
        _ => Ok(Json(CreateSessionResponse {
            session_id: String::new(),
            status: "loading".to_string(),
            error: Some("Connection not ready".to_string()),
        })),
    }
}

pub async fn list_sessions(
    State(state): State<AcpServerState>,
) -> Result<Json<Vec<SessionInfo>>, StatusCode> {
    let mut sessions = Vec::new();

    for entry in state.sessions.iter() {
        let session = entry.value();
        sessions.push(SessionInfo {
            id: session.id.clone(),
            acp_id: session.acp_id.clone(),
        });
    }

    Ok(Json(sessions))
}

pub async fn session_update(
    State(state): State<AcpServerState>,
    Path(session_id): Path<String>,
    Json(req): Json<SessionUpdateRequest>,
) -> Result<StatusCode, StatusCode> {
    let session = state
        .sessions
        .get(&session_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Route response to waiting handler
    if let Some((_, tx)) = session.pending_requests.remove(&req.id) {
        let response = if let Some(result) = req.result {
            json!({ "result": result })
        } else if let Some(error) = req.error {
            json!({ "error": error })
        } else {
            json!({})
        };

        let _ = tx.send(response);
    }

    Ok(StatusCode::OK)
}

pub async fn session_sse(
    State(state): State<AcpServerState>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>, StatusCode> {
    let session = state
        .sessions
        .get(&session_id)
        .ok_or(StatusCode::NOT_FOUND)?
        .clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<SseMessage>();

    // Check if client wants messages from a specific ID
    let from_id = params
        .get("from_id")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // Send buffered messages starting from the requested ID
    let buffered = session.message_buffer.read().await.clone();
    for msg in buffered {
        let msg_id = match &msg {
            SseMessage::AcpJsonRpc { id, .. } => *id,
            SseMessage::Exit { id } => *id,
        };
        if msg_id > from_id {
            let _ = tx.send(msg);
        }
    }

    // Register SSE connection
    state.sse_connections.insert(session_id.clone(), tx);

    let stream = async_stream::stream! {
        while let Some(msg) = rx.recv().await {
            let data = serde_json::to_string(&msg).unwrap_or_default();
            yield Ok(Event::default().data(data));
        }
    };

    Sse::new(Box::pin(stream)).keep_alive(KeepAlive::default())
}

// ============================================================================
// Process Management
// ============================================================================

async fn spawn_acp_process(
    state: &AcpServerState,
    req: CreateConnectionRequest,
) -> Result<ConnectionInfo> {
    let parts: Vec<&str> = req.command.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow!("Empty command"));
    }

    let mut child = Command::new(parts[0])
        .args(&parts[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn process: {}", e))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Failed to get stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to get stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to get stderr"))?;

    // Convert tokio streams to futures streams
    let stdin = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(stdin);
    let stdout = tokio_util::compat::TokioAsyncReadCompatExt::compat(stdout);

    // Create channel for agent requests
    let (agent_tx, mut agent_rx) = mpsc::unbounded_channel::<AgentRequest>();

    // Create connection
    let connection = Arc::new(AcpConnection {
        id: req.id.clone(),
        name: req.name.clone(),
        command: req.command.clone(),
        status: Arc::new(RwLock::new(ConnectionStatus::Loading)),
        capabilities: Arc::new(RwLock::new(None)),
        agent_tx: agent_tx.clone(),
        process_handle: Arc::new(Mutex::new(Some(child))),
        local_set_handle: Arc::new(Mutex::new(None)),
    });

    state.connections.insert(req.id.clone(), connection.clone());

    // Spawn stderr reader
    let acp_id = req.id.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                debug!("ACP[{}] stderr: {}", acp_id, trimmed);
            }
            line.clear();
        }
    });

    // Create client implementation
    let client_impl = ClientImpl::new(state.clone());

    // Set up ACP connection in a LocalSet
    let connection_clone = connection.clone();
    let state_clone = state.clone();
    let acp_id = req.id.clone();

    let local_set_handle = tokio::spawn(async move {
        let local_set = LocalSet::new();

        local_set
            .run_until(async move {
                // Create ACP connection
                let (conn, io_task) = acp::ClientSideConnection::new(
                    client_impl,
                    stdin,
                    stdout,
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );

                // Initialize the agent
                match conn
                    .initialize(acp::InitializeRequest {
                        protocol_version: acp::PROTOCOL_VERSION.to_string(),
                        client_capabilities: acp::ClientCapabilities {
                            fs: Some(acp::FileSystemCapabilities {
                                read_text_file: Some(false),
                                write_text_file: Some(false),
                            }),
                            terminal: Some(false),
                            _meta: None,
                        },
                        _meta: None,
                    })
                    .await
                {
                    Ok(response) => {
                        info!("ACP[{}] initialized successfully", acp_id);
                        *connection_clone.status.write().await = ConnectionStatus::Connected;
                        *connection_clone.capabilities.write().await = response.agent_capabilities;

                        // Notify listeners
                        let update = ConnectionUpdate {
                            id: acp_id.clone(),
                            status: ConnectionStatus::Connected,
                            capabilities: response.agent_capabilities.clone(),
                        };
                        for tx in state_clone.connection_updates.read().await.iter() {
                            let _ = tx.send(update.clone());
                        }
                    }
                    Err(e) => {
                        error!("ACP[{}] initialization failed: {}", acp_id, e);
                        *connection_clone.status.write().await = ConnectionStatus::Error {
                            message: format!("Initialization failed: {}", e),
                        };

                        // Notify listeners
                        let update = ConnectionUpdate {
                            id: acp_id.clone(),
                            status: ConnectionStatus::Error {
                                message: format!("Initialization failed: {}", e),
                            },
                            capabilities: None,
                        };
                        for tx in state_clone.connection_updates.read().await.iter() {
                            let _ = tx.send(update.clone());
                        }
                        return;
                    }
                }

                // Handle agent requests
                tokio::task::spawn_local(async move {
                    while let Some(request) = agent_rx.recv().await {
                        match request {
                            AgentRequest::NewSession {
                                session_id,
                                response,
                            } => {
                                let result = conn
                                    .new_session(acp::NewSessionRequest {
                                        _meta: None,
                                    })
                                    .await;

                                let _ = response.send(match result {
                                    Ok(resp) => Ok(resp.session_id),
                                    Err(e) => Err(e.to_string()),
                                });
                            }
                            AgentRequest::Prompt {
                                session_id,
                                messages,
                                response,
                            } => {
                                let result = conn
                                    .prompt(acp::PromptRequest {
                                        session_id,
                                        messages,
                                        _meta: None,
                                    })
                                    .await;

                                let _ = response.send(match result {
                                    Ok(_) => Ok(()),
                                    Err(e) => Err(e.to_string()),
                                });
                            }
                        }
                    }
                });

                // Run IO task
                io_task.await
            })
            .await;

        info!("ACP[{}] LocalSet finished", acp_id);
    });

    // Store handle
    *connection.local_set_handle.lock().await = Some(local_set_handle);

    // Monitor process for exit
    let connection_clone = connection.clone();
    let state_clone = state.clone();
    let acp_id = req.id.clone();
    tokio::spawn(async move {
        // Wait for local set to finish
        if let Some(handle) = connection_clone.local_set_handle.lock().await.take() {
            let _ = handle.await;
        }

        // Handle process exit
        handle_process_exit(&state_clone, &acp_id).await;

        // Attempt restart after delay
        tokio::time::sleep(Duration::from_secs(2)).await;
        // TODO: Implement restart logic
    });

    Ok(ConnectionInfo {
        id: req.id,
        name: req.name,
        command: req.command,
        status: ConnectionStatus::Loading,
        capabilities: None,
    })
}

async fn handle_process_exit(state: &AcpServerState, acp_id: &str) {
    info!("ACP[{}] process exited", acp_id);

    // Collect sessions to remove
    let mut sessions_to_remove = Vec::new();
    for entry in state.sessions.iter() {
        if entry.value().acp_id == acp_id {
            sessions_to_remove.push(entry.key().clone());
        }
    }

    // Send exit events and remove sessions
    for session_id in sessions_to_remove {
        // Send exit event to SSE
        if let Some(session) = state.sessions.get(&session_id) {
            let sse_id = session.sse_id_counter.fetch_add(1, Ordering::Relaxed);
            if let Some(tx) = state.sse_connections.get(&session_id) {
                let msg = SseMessage::Exit { id: sse_id };
                let _ = tx.send(msg);
            }
        }

        // Remove session
        state.sessions.remove(&session_id);
        state.sse_connections.remove(&session_id);
    }

    // Update connection status
    if let Some(mut conn) = state.connections.get_mut(&acp_id.to_string()) {
        *conn.status.write().await = ConnectionStatus::Error {
            message: "Process exited".to_string(),
        };

        // Notify listeners
        let update = ConnectionUpdate {
            id: acp_id.to_string(),
            status: ConnectionStatus::Error {
                message: "Process exited".to_string(),
            },
            capabilities: None,
        };

        for tx in state.connection_updates.read().await.iter() {
            let _ = tx.send(update.clone());
        }
    }
}