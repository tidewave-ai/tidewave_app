use agent_client_protocol::{self as acp, Agent};
use anyhow::{anyhow, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
};
use dashmap::DashMap;
use futures::{stream::Stream, StreamExt};
use serde::{Deserialize, Serialize};
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
    process::Command,
    sync::{mpsc, oneshot, Mutex, RwLock},
    task::{JoinHandle, LocalSet},
    time::timeout,
};
use tracing::{debug, error, info};

// ============================================================================
// Types and State
// ============================================================================

#[derive(Clone)]
pub struct AcpServerState {
    /// Active ACP connections (id -> connection)
    pub connections: Arc<DashMap<String, Arc<AcpConnection>>>,
    /// Active sessions (session_id -> session)
    pub sessions: Arc<DashMap<acp::SessionId, Arc<AcpSession>>>,
    /// WebSocket connections for sessions (session_id -> WebSocket sender)
    pub ws_connections: Arc<DashMap<acp::SessionId, mpsc::UnboundedSender<ServerMessage>>>,
    /// Connection status updates SSE
    pub connection_updates: Arc<RwLock<Vec<mpsc::UnboundedSender<ConnectionUpdate>>>>,
}

impl AcpServerState {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            ws_connections: Arc::new(DashMap::new()),
            connection_updates: Arc::new(RwLock::new(Vec::new())),
        }
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
    /// Handle to the local set running the agent
    pub local_set_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

pub struct AcpSession {
    pub id: acp::SessionId,
    pub acp_id: String,
    pub pending_requests: Arc<DashMap<u64, oneshot::Sender<acp::ClientResponse>>>,
    pub message_buffer: Arc<RwLock<Vec<ServerMessage>>>,
    pub ws_id_counter: Arc<AtomicU64>,
}

/// Requests we can send to the agent task
#[derive(Debug)]
pub enum AgentRequest {
    NewSession {
        response: oneshot::Sender<Result<acp::SessionId, String>>,
    },
    Prompt {
        session_id: acp::SessionId,
        messages: Vec<acp::ContentBlock>,
        response: oneshot::Sender<Result<acp::PromptResponse, String>>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "prompt")]
    Prompt {
        id: u64,
        messages: Vec<acp::ContentBlock>,
    },
    #[serde(rename = "permission-response")]
    PermissionResponse {
        id: u64,
        response: acp::RequestPermissionResponse,
    },
    #[serde(rename = "ack")]
    Ack {
        #[serde(rename = "latestId")]
        latest_id: u64,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "session-notification")]
    SessionNotification {
        id: u64,
        params: acp::SessionNotification,
    },
    #[serde(rename = "permission-request")]
    PermissionRequest {
        id: u64,
        params: acp::RequestPermissionRequest,
    },
    #[serde(rename = "prompt-response")]
    PromptResponse {
        id: u64,
        #[serde(rename = "response-type")]
        response_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<acp::PromptResponse>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<acp::SessionId>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct SessionInfo {
    pub id: acp::SessionId,
    #[serde(rename = "acpId")]
    pub acp_id: String,
}

// ============================================================================
// ACP Client Implementation
// ============================================================================

#[async_trait::async_trait(?Send)]
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
            .find(|entry| entry.value().id == args.session_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| acp::Error::internal_error())?;

        let ws_id = session.ws_id_counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        session.pending_requests.insert(ws_id, tx);

        let message = ServerMessage::PermissionRequest {
            id: ws_id,
            params: args,
        };

        // Send to WebSocket or buffer
        if let Some(ws_tx) = self.state.ws_connections.get(&session.id) {
            let _ = ws_tx.send(message.clone());
        } else {
            session.message_buffer.write().await.push(message.clone());
        }

        // Wait for response with timeout
        match timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(acp::ClientResponse::RequestPermissionResponse(response))) => Ok(response),
            Ok(Ok(_)) => Err(acp::Error::internal_error()), // Wrong response type
            Ok(Err(_)) => Err(acp::Error::internal_error()),
            Err(_) => Err(acp::Error::internal_error()),
        }
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        // Find the session by ACP session ID
        let session = self
            .state
            .sessions
            .iter()
            .find(|entry| entry.value().id == args.session_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| acp::Error::internal_error())?;

        let ws_id = session.ws_id_counter.fetch_add(1, Ordering::Relaxed);

        let message = ServerMessage::SessionNotification {
            id: ws_id,
            params: args,
        };

        // Send to WebSocket or buffer
        if let Some(ws_tx) = self.state.ws_connections.get(&session.id) {
            debug!("Got WebSocket connection, id {}", ws_id);
            let _ = ws_tx.send(message.clone());
        } else {
            debug!("Buffering message with id {}", ws_id);
            session.message_buffer.write().await.push(message.clone());
        }

        Ok(())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> Result<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
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
        let req_id = req.id.clone();
        let req_name = req.name.clone();
        let req_command = req.command.clone();
        match spawn_acp_process(&state, req).await {
            Ok(info) => results.push(info),
            Err(e) => {
                error!("Failed to create connection {}: {}", req_id, e);
                results.push(ConnectionInfo {
                    id: req_id,
                    name: req_name,
                    command: req_command,
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
            yield Ok::<Event, Infallible>(Event::default().data(data));
        }
    };

    Sse::new(Box::pin(stream) as Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>)
        .keep_alive(KeepAlive::default())
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
            let (tx, rx) = oneshot::channel();

            // Send request to agent task
            if let Err(e) = connection
                .agent_tx
                .send(AgentRequest::NewSession { response: tx })
            {
                return Ok(Json(CreateSessionResponse {
                    session_id: None,
                    status: "error".to_string(),
                    error: Some(format!("Failed to send request: {}", e)),
                }));
            }

            // Wait for response
            match timeout(Duration::from_secs(5), rx).await {
                Ok(Ok(Ok(acp_session_id))) => {
                    // Create our session tracking
                    let session = Arc::new(AcpSession {
                        id: acp_session_id.clone(),
                        acp_id: req.acp_id.clone(),
                        pending_requests: Arc::new(DashMap::new()),
                        message_buffer: Arc::new(RwLock::new(Vec::new())),
                        ws_id_counter: Arc::new(AtomicU64::new(1)),
                    });

                    state.sessions.insert(acp_session_id.clone(), session);

                    Ok(Json(CreateSessionResponse {
                        session_id: Some(acp_session_id),
                        status: "created".to_string(),
                        error: None,
                    }))
                }
                Ok(Ok(Err(e))) => {
                    // Check if it's an auth error
                    if e.contains("auth_required") {
                        Ok(Json(CreateSessionResponse {
                            session_id: None,
                            status: "auth_required".to_string(),
                            error: Some(e),
                        }))
                    } else {
                        Ok(Json(CreateSessionResponse {
                            session_id: None,
                            status: "error".to_string(),
                            error: Some(e),
                        }))
                    }
                }
                _ => Ok(Json(CreateSessionResponse {
                    session_id: None,
                    status: "error".to_string(),
                    error: Some("Request timed out".to_string()),
                })),
            }
        }
        ConnectionStatus::Error { message } => Ok(Json(CreateSessionResponse {
            session_id: None,
            status: "error".to_string(),
            error: Some(message),
        })),
        _ => Ok(Json(CreateSessionResponse {
            session_id: None,
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

pub async fn session_ws(
    ws: WebSocketUpgrade,
    State(state): State<AcpServerState>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, StatusCode> {
    let session = state
        .sessions
        .get(&acp::SessionId(std::sync::Arc::from(session_id)))
        .ok_or(StatusCode::NOT_FOUND)?
        .clone();

    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state, session, params)))
}

async fn handle_websocket(
    socket: WebSocket,
    state: AcpServerState,
    session: Arc<AcpSession>,
    params: HashMap<String, String>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Create channel for sending messages to WebSocket
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Check if client wants messages from a specific ID
    let from_id = params
        .get("from_id")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // Send buffered messages starting from the requested ID
    let buffered = session.message_buffer.read().await.clone();
    for msg in buffered {
        let msg_id = match &msg {
            ServerMessage::SessionNotification { id, .. } => *id,
            ServerMessage::PermissionRequest { id, .. } => *id,
            ServerMessage::PromptResponse { id, .. } => *id,
            ServerMessage::Exit { id } => *id,
        };
        if msg_id > from_id {
            let _ = tx.send(msg);
        }
    }

    // Register WebSocket connection
    state.ws_connections.insert(session.id.clone(), tx.clone());

    // Handle outgoing messages (server to client)
    let session_id_out = session.id.clone();
    let state_out = state.clone();
    let tx_task = tokio::spawn(async move {
        use futures::SinkExt;
        while let Some(msg) = rx.recv().await {
            let json_str = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(e) => {
                    debug!("Failed to serialize message: {}", e);
                    continue;
                }
            };

            if sender.send(Message::Text(json_str.into())).await.is_err() {
                debug!("WebSocket send failed for session: {}", session_id_out);
                break;
            }
        }
        // Clean up when sending task ends
        state_out.ws_connections.remove(&session_id_out);
    });

    // Handle incoming messages (client to server)
    let session_rx = session.clone();
    let state_rx = state.clone();
    let rx_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Err(e) = handle_client_message(&state_rx, &session_rx, &text).await {
                        debug!("Error handling client message: {}", e);
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("WebSocket closed for session: {}", session_rx.id);
                    break;
                }
                Err(e) => {
                    debug!("WebSocket error for session {}: {}", session_rx.id, e);
                    break;
                }
                _ => {} // Ignore other message types
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = tx_task => {},
        _ = rx_task => {},
    }

    // Clean up WebSocket connection
    state.ws_connections.remove(&session.id);
}

async fn handle_client_message(
    state: &AcpServerState,
    session: &AcpSession,
    text: &str,
) -> Result<()> {
    let client_msg: ClientMessage =
        serde_json::from_str(text).map_err(|e| anyhow!("Failed to parse client message: {}", e))?;

    match client_msg {
        ClientMessage::Prompt { id, messages } => {
            // Get the connection for this session
            let connection = state
                .connections
                .get(&session.acp_id)
                .ok_or_else(|| anyhow!("Connection not found"))?;

            // Check connection status
            let status = connection.status.read().await.clone();
            if !matches!(status, ConnectionStatus::Connected) {
                // Send error response
                let response = ServerMessage::PromptResponse {
                    id,
                    response_type: "error".to_string(),
                    response: None,
                    error: Some("Connection not ready".to_string()),
                };
                if let Some(tx) = state.ws_connections.get(&session.id) {
                    let _ = tx.send(response);
                }
                return Ok(());
            }

            // Send prompt request to agent task
            let (tx, rx) = oneshot::channel();
            if let Err(e) = connection.agent_tx.send(AgentRequest::Prompt {
                session_id: session.id.clone(),
                messages,
                response: tx,
            }) {
                let response = ServerMessage::PromptResponse {
                    id,
                    response_type: "error".to_string(),
                    response: None,
                    error: Some(format!("Failed to send request: {}", e)),
                };
                if let Some(ws_tx) = state.ws_connections.get(&session.id) {
                    let _ = ws_tx.send(response);
                }
                return Ok(());
            }

            // Wait for response and send back via WebSocket
            let ws_tx = state.ws_connections.get(&session.id).map(|tx| tx.clone());
            tokio::spawn(async move {
                let response = match timeout(Duration::from_secs(30), rx).await {
                    Ok(Ok(Ok(prompt_response))) => ServerMessage::PromptResponse {
                        id,
                        response_type: "success".to_string(),
                        response: Some(prompt_response),
                        error: None,
                    },
                    Ok(Ok(Err(e))) => ServerMessage::PromptResponse {
                        id,
                        response_type: "error".to_string(),
                        response: None,
                        error: Some(e),
                    },
                    Ok(Err(_)) => ServerMessage::PromptResponse {
                        id,
                        response_type: "error".to_string(),
                        response: None,
                        error: Some("Response channel closed".to_string()),
                    },
                    Err(_) => ServerMessage::PromptResponse {
                        id,
                        response_type: "error".to_string(),
                        response: None,
                        error: Some("Request timed out".to_string()),
                    },
                };

                if let Some(tx) = ws_tx {
                    let _ = tx.send(response);
                }
            });
        }
        ClientMessage::PermissionResponse { id, response } => {
            // Find the pending request and respond
            if let Some((_, tx)) = session.pending_requests.remove(&id) {
                let _ = tx.send(acp::ClientResponse::RequestPermissionResponse(response));
            }
        }
        ClientMessage::Ack { latest_id } => {
            // Remove acknowledged messages from buffer
            let mut buffer = session.message_buffer.write().await;
            buffer.retain(|msg| {
                let msg_id = match msg {
                    ServerMessage::SessionNotification { id, .. } => *id,
                    ServerMessage::PermissionRequest { id, .. } => *id,
                    ServerMessage::PromptResponse { id, .. } => *id,
                    ServerMessage::Exit { id } => *id,
                };
                msg_id > latest_id
            });
        }
    }

    Ok(())
}

// ============================================================================
// Process Management
// ============================================================================

async fn spawn_acp_process(
    state: &AcpServerState,
    req: CreateConnectionRequest,
) -> Result<ConnectionInfo> {
    debug!("spawn_acp_process called for connection: {}", req.id);

    let parts: Vec<&str> = req.command.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow!("Empty command"));
    }

    debug!(
        "Spawning process: {} with args: {:?}",
        parts[0],
        &parts[1..]
    );

    let mut child = Command::new(parts[0])
        .args(&parts[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn process: {}", e))?;

    debug!("Process spawned successfully for connection: {}", req.id);

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
        local_set_handle: Arc::new(Mutex::new(None)),
    });

    state.connections.insert(req.id.clone(), connection.clone());
    debug!("Connection stored in state for: {}", req.id);

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
    debug!("Client implementation created for: {}", req.id);

    // Set up ACP connection in a LocalSet
    let connection_clone = connection.clone();
    let state_clone = state.clone();
    let acp_id = req.id.clone();
    let acp_id_for_log = acp_id.clone();

    debug!("Starting blocking thread for ACP connection: {}", acp_id);
    let local_set_handle = tokio::task::spawn_blocking(move || {
        debug!(
            "Inside blocking thread for ACP connection: {}",
            acp_id_for_log
        );

        // Create a new runtime for this blocking thread
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        let local_set = LocalSet::new();

        debug!("Created runtime and LocalSet for: {}", acp_id_for_log);

        rt.block_on(local_set.run_until(async move {
            debug!("Inside LocalSet run_until for: {}", acp_id);

            // Create ACP connection
            let (conn, io_task) =
                acp::ClientSideConnection::new(client_impl, stdin, stdout, |fut| {
                    tokio::task::spawn_local(fut);
                });

            debug!("ACP ClientSideConnection created for: {}", acp_id);

            // Spawn the I/O task first - it needs to run concurrently with initialization
            let acp_id_io = acp_id.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = io_task.await {
                    debug!("IO task error for {}: {}", acp_id_io, e);
                }
            });

            // Initialize the agent
            debug!("Starting ACP initialization for: {}", acp_id);
            match conn
                .initialize(acp::InitializeRequest {
                    protocol_version: acp::V1,
                    client_capabilities: acp::ClientCapabilities {
                        fs: acp::FileSystemCapability {
                            read_text_file: false,
                            write_text_file: false,
                            meta: None,
                        },
                        terminal: false,
                        meta: None,
                    },
                    meta: None,
                })
                .await
            {
                Ok(response) => {
                    info!("ACP[{}] initialized successfully", acp_id);
                    *connection_clone.status.write().await = ConnectionStatus::Connected;
                    let capabilities = response.agent_capabilities.clone();
                    *connection_clone.capabilities.write().await = Some(capabilities.clone());

                    // Notify listeners
                    let update = ConnectionUpdate {
                        id: acp_id.clone(),
                        status: ConnectionStatus::Connected,
                        capabilities: Some(capabilities),
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
            let request_handler = tokio::task::spawn_local(async move {
                while let Some(request) = agent_rx.recv().await {
                    match request {
                        AgentRequest::NewSession { response } => {
                            let result = conn
                                .new_session(acp::NewSessionRequest {
                                    cwd: std::env::current_dir().unwrap_or_else(|_| "/tmp".into()),
                                    mcp_servers: Vec::new(),
                                    meta: None,
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
                                    prompt: messages,
                                    meta: None,
                                })
                                .await;

                            let _ = response.send(match result {
                                Ok(prompt_response) => Ok(prompt_response),
                                Err(e) => Err(e.to_string()),
                            });
                        }
                    }
                }
            });

            // Wait for the agent request handler to complete (keeps LocalSet alive)
            let _ = request_handler.await;
        }));

        info!("ACP[{}] LocalSet finished", acp_id_for_log);
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
        // Send exit event to WebSocket
        if let Some(session) = state.sessions.get(&session_id) {
            let ws_id = session.ws_id_counter.fetch_add(1, Ordering::Relaxed);
            if let Some(tx) = state.ws_connections.get(&session_id) {
                let msg = ServerMessage::Exit { id: ws_id };
                let _ = tx.send(msg);
            }
        }

        // Remove session
        state.sessions.remove(&session_id);
        state.ws_connections.remove(&session_id);
    }

    // Update connection status
    if let Some(conn) = state.connections.get_mut(&acp_id.to_string()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acp_server_state_creation() {
        let state = AcpServerState::new();

        assert_eq!(state.connections.len(), 0);
        assert_eq!(state.sessions.len(), 0);
        assert_eq!(state.ws_connections.len(), 0);
    }

    #[tokio::test]
    async fn test_connection_status_updates() {
        let state = AcpServerState::new();
        let (agent_tx, _agent_rx) = mpsc::unbounded_channel();
        let connection = Arc::new(AcpConnection {
            id: "test-conn".to_string(),
            name: "Test Connection".to_string(),
            command: "test-command".to_string(),
            status: Arc::new(RwLock::new(ConnectionStatus::Loading)),
            capabilities: Arc::new(RwLock::new(None)),
            agent_tx,
            local_set_handle: Arc::new(Mutex::new(None)),
        });

        state
            .connections
            .insert("test-conn".to_string(), connection.clone());

        // Test initial status
        assert!(matches!(
            *connection.status.read().await,
            ConnectionStatus::Loading
        ));

        // Test status change
        *connection.status.write().await = ConnectionStatus::Connected;
        assert!(matches!(
            *connection.status.read().await,
            ConnectionStatus::Connected
        ));

        // Test error status
        *connection.status.write().await = ConnectionStatus::Error {
            message: "Test error".to_string(),
        };
        let status = connection.status.read().await;
        if let ConnectionStatus::Error { message } = &*status {
            assert_eq!(message, "Test error");
        } else {
            panic!("Expected error status");
        }
    }

    #[tokio::test]
    async fn test_websocket_message_serialization() {
        // Test ServerMessage serialization
        let session_notification = ServerMessage::SessionNotification {
            id: 42,
            params: acp::SessionNotification {
                session_id: acp::SessionId(std::sync::Arc::from("test-session")),
                update: acp::SessionUpdate::UserMessageChunk {
                    content: acp::ContentBlock::Text(acp::TextContent {
                        text: "Hello world".to_string(),
                        annotations: None,
                        meta: None,
                    }),
                },
                meta: None,
            },
        };

        let serialized = serde_json::to_string(&session_notification).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(parsed["type"], "session-notification");
        assert_eq!(parsed["id"], 42);
        assert!(parsed["params"].is_object());

        // Test ClientMessage deserialization
        let client_ack = r#"{"type": "ack", "latestId": 123}"#;
        let parsed: ClientMessage = serde_json::from_str(client_ack).unwrap();

        match parsed {
            ClientMessage::Ack { latest_id } => assert_eq!(latest_id, 123),
            _ => panic!("Expected Ack message"),
        }

        let exit_message = ServerMessage::Exit { id: 99 };
        let serialized = serde_json::to_string(&exit_message).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(parsed["type"], "exit");
        assert_eq!(parsed["id"], 99);
    }

    #[tokio::test]
    async fn test_session_management() {
        let id = acp::SessionId(Arc::from("test-acp-session"));
        let state = AcpServerState::new();
        let session = Arc::new(AcpSession {
            id: id.clone(),
            acp_id: "test-conn".to_string(),
            pending_requests: Arc::new(DashMap::new()),
            message_buffer: Arc::new(RwLock::new(Vec::new())),
            ws_id_counter: Arc::new(AtomicU64::new(1)),
        });

        state.sessions.insert(id.clone(), session.clone());

        // Verify session was added
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions.contains_key(&id));

        // Verify session properties
        assert_eq!(session.id, id);
        assert_eq!(session.acp_id, "test-conn");
        assert_eq!(session.pending_requests.len(), 0);
        assert_eq!(session.message_buffer.read().await.len(), 0);
    }
}
