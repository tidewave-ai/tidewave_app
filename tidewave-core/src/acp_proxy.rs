use anyhow::{anyhow, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, RwLock},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

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

// Internal message type for WebSocket communication
#[derive(Debug, Clone)]
pub enum WebSocketMessage {
    JsonRpc(JsonRpcMessage),
    Pong,
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
pub struct SessionLoadRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "latestId")]
    pub latest_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveAckRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "latestId")]
    pub latest_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidewaveExitParams {
    pub error: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionModelParams {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "modelId")]
    pub model_id: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionResponse {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _meta: Option<Value>,
}

// ============================================================================
// Server State Types
// ============================================================================

pub type WebSocketId = Uuid;
pub type ProcessKey = String; // Hash of command + init params
pub type SessionId = String;
pub type NotificationId = String;

#[derive(Clone)]
pub struct AcpProxyState {
    /// Active ACP processes (process_key -> process_state)
    pub processes: Arc<DashMap<ProcessKey, Arc<ProcessState>>>,
    /// WebSocket connections (websocket_id -> websocket_sender)
    pub websocket_senders: Arc<DashMap<WebSocketId, mpsc::UnboundedSender<WebSocketMessage>>>,
    /// Sessions (session_id -> session_state)
    pub sessions: Arc<DashMap<SessionId, Arc<SessionState>>>,
    /// Session to WebSocket mapping (session_id -> websocket_id) - only for active connections
    pub session_to_websocket: Arc<DashMap<SessionId, WebSocketId>>,
    /// WebSocket to Process mapping (websocket_id -> process_key) - for routing regular requests
    pub websocket_to_process: Arc<DashMap<WebSocketId, ProcessKey>>,
}

impl AcpProxyState {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(DashMap::new()),
            websocket_senders: Arc::new(DashMap::new()),
            sessions: Arc::new(DashMap::new()),
            session_to_websocket: Arc::new(DashMap::new()),
            websocket_to_process: Arc::new(DashMap::new()),
        }
    }
}

pub struct ProcessState {
    pub command: String,
    pub child: Arc<RwLock<Option<Child>>>,
    pub stdin_tx: Arc<RwLock<Option<mpsc::UnboundedSender<JsonRpcMessage>>>>,
    pub next_proxy_id: Arc<AtomicU64>,

    // ID mapping for multiplexed connections
    pub client_to_proxy_ids: Arc<DashMap<(WebSocketId, Value), Value>>,
    pub proxy_to_client_ids: Arc<DashMap<Value, (WebSocketId, Value)>>,
    pub proxy_to_session_ids: Arc<DashMap<Value, (SessionId, Value)>>,

    // Cached init response
    pub cached_init_response: Arc<RwLock<Option<JsonRpcResponse>>>,
    pub init_request_id: Arc<RwLock<Option<Value>>>,
}

pub struct SessionState {
    pub process_key: ProcessKey,
    pub message_buffer: Arc<RwLock<Vec<BufferedMessage>>>,
    pub notification_id_counter: Arc<AtomicU64>,
    pub models: Arc<RwLock<Option<Value>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferedMessage {
    pub id: NotificationId,
    pub message: JsonRpcMessage,
}

impl ProcessState {
    pub fn new(command: String) -> Self {
        Self {
            command,
            child: Arc::new(RwLock::new(None)),
            stdin_tx: Arc::new(RwLock::new(None)),
            next_proxy_id: Arc::new(AtomicU64::new(1)),
            client_to_proxy_ids: Arc::new(DashMap::new()),
            proxy_to_client_ids: Arc::new(DashMap::new()),
            proxy_to_session_ids: Arc::new(DashMap::new()),
            cached_init_response: Arc::new(RwLock::new(None)),
            init_request_id: Arc::new(RwLock::new(None)),
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
        websocket_id: WebSocketId,
        client_id: Value,
        session_id: Option<SessionId>,
    ) -> Value {
        let proxy_id = self.generate_proxy_id();

        self.client_to_proxy_ids
            .insert((websocket_id, client_id.clone()), proxy_id.clone());
        self.proxy_to_client_ids
            .insert(proxy_id.clone(), (websocket_id, client_id.clone()));

        // If this request has a session_id, also store the session mapping
        if let Some(session_id) = session_id {
            self.proxy_to_session_ids
                .insert(proxy_id.clone(), (session_id, client_id));
        }

        proxy_id
    }

    pub fn resolve_proxy_id_to_client(&self, proxy_id: &Value) -> Option<(WebSocketId, Value)> {
        self.proxy_to_client_ids
            .get(proxy_id)
            .map(|entry| entry.value().clone())
    }

    pub fn cleanup_id_mappings(&self, proxy_id: &Value) {
        if let Some((_, (websocket_id, client_id))) = self.proxy_to_client_ids.remove(proxy_id) {
            self.client_to_proxy_ids.remove(&(websocket_id, client_id));
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
            models: Arc::new(RwLock::new(None)),
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

    pub async fn get_buffered_messages_after(&self, latest_id: &str) -> Vec<BufferedMessage> {
        let buffer = self.message_buffer.read().await;

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
// WebSocket Handler
// ============================================================================

#[derive(Deserialize)]
pub struct AcpWebSocketQuery {
    pub command: String,
}

pub async fn acp_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AcpProxyState>,
    Query(query): Query<AcpWebSocketQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let websocket_id = Uuid::new_v4();
    debug!("New WebSocket connection: {}", websocket_id);

    Ok(ws.on_upgrade(move |socket| handle_websocket(socket, state, websocket_id, query.command)))
}

async fn handle_websocket(
    socket: WebSocket,
    state: AcpProxyState,
    websocket_id: WebSocketId,
    command: String,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Create channel for sending messages to this WebSocket
    let (tx, mut rx) = mpsc::unbounded_channel::<WebSocketMessage>();

    // Register WebSocket sender
    state.websocket_senders.insert(websocket_id, tx.clone());

    // Task to handle outgoing messages (server -> client)
    let websocket_id_tx = websocket_id;
    let tx_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let ws_message = match message {
                WebSocketMessage::JsonRpc(json_msg) => match serde_json::to_string(&json_msg) {
                    Ok(json_str) => Message::Text(format!("{}\n", json_str).into()),
                    Err(_) => continue,
                },
                WebSocketMessage::Pong => Message::Text("pong".into()),
            };

            if ws_sender.send(ws_message).await.is_err() {
                debug!("WebSocket send failed for: {}", websocket_id_tx);
                break;
            }
        }
    });

    // Task to handle incoming messages (client -> server)
    let state_rx = state.clone();
    let websocket_id_rx = websocket_id;
    let rx_task = tokio::spawn(async move {
        debug!("Starting WebSocket receive loop for: {}", websocket_id_rx);
        while let Some(msg) = ws_receiver.next().await {
            debug!(
                "Received WebSocket message for {}: {:?}",
                websocket_id_rx, msg
            );
            match msg {
                Ok(Message::Text(text)) => {
                    debug!("Processing text message: {}", text);
                    if text.trim() == "ping" {
                        // Send pong response via channel
                        if let Some(tx) = state_rx.websocket_senders.get(&websocket_id_rx) {
                            let _ = tx.send(WebSocketMessage::Pong);
                        }
                    } else if let Err(e) =
                        handle_client_message(&state_rx, websocket_id_rx, &command, &text).await
                    {
                        error!("Error handling client message: {}", e);
                    }
                }
                Ok(Message::Close(_)) => {
                    debug!("WebSocket closed for: {}", websocket_id_rx);
                    break;
                }
                Ok(msg_type) => {
                    debug!(
                        "Received non-text message for {}: {:?}",
                        websocket_id_rx, msg_type
                    );
                }
                Err(e) => {
                    debug!("WebSocket error for {}: {}", websocket_id_rx, e);
                    break;
                }
            }
        }
        debug!("WebSocket receive loop ended for: {}", websocket_id_rx);
    });

    // Wait for either task to complete
    tokio::select! {
        _ = tx_task => {},
        _ = rx_task => {},
    }

    // Cleanup on disconnect
    state.websocket_senders.remove(&websocket_id);
    state.websocket_to_process.remove(&websocket_id);

    // Remove session mappings for this WebSocket
    let mut sessions_to_remove = Vec::new();
    for entry in state.session_to_websocket.iter() {
        if *entry.value() == websocket_id {
            sessions_to_remove.push(entry.key().clone());
        }
    }

    for session_id in sessions_to_remove {
        state.session_to_websocket.remove(&session_id);
    }

    debug!("WebSocket connection closed: {}", websocket_id);
}

async fn handle_client_message(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    command: &str,
    text: &str,
) -> Result<()> {
    debug!("Received message from WebSocket {}: {}", websocket_id, text);
    let message: JsonRpcMessage = serde_json::from_str(text)
        .map_err(|e| anyhow!("Failed to parse JSON-RPC message: {}", e))?;

    match &message {
        JsonRpcMessage::Request(req) => {
            debug!("Handling request: {} with method {}", req.id, req.method);
            handle_client_request(state, websocket_id, command, req).await
        }
        JsonRpcMessage::Notification(notif) => {
            debug!("Handling notification with method {}", notif.method);
            handle_client_notification(state, websocket_id, notif).await
        }
        JsonRpcMessage::Response(resp) => {
            // Forward client responses (e.g., permission responses) back to the process
            forward_response_to_process(state, websocket_id, resp).await
        }
    }
}

async fn handle_client_request(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    command: &str,
    request: &JsonRpcRequest,
) -> Result<()> {
    match request.method.as_str() {
        "initialize" => handle_initialize_request(state, websocket_id, command, request).await,
        "_tidewave.ai/session/load" => {
            handle_tidewave_session_load(state, websocket_id, request).await
        }
        "session/load" => handle_acp_session_load(state, websocket_id, command, request).await,
        _ => handle_regular_request(state, websocket_id, command, request).await,
    }
}

async fn handle_client_notification(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    notification: &JsonRpcNotification,
) -> Result<()> {
    match notification.method.as_str() {
        "_tidewave.ai/ack" => handle_ack_notification(state, websocket_id, notification).await,
        _ => {
            // Forward regular notifications to the process
            // Notifications don't need ID mapping since they don't expect responses
            forward_notification_to_process(state, websocket_id, notification).await
        }
    }
}

// ============================================================================
// Request Handlers
// ============================================================================

async fn handle_initialize_request(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    command: &str,
    request: &JsonRpcRequest,
) -> Result<()> {
    debug!("Handling initialize request for command: {}", command);
    let init_params = request.params.as_ref().unwrap_or(&Value::Null);
    let process_key = generate_process_key(command, init_params);
    debug!("Generated process key: {}", process_key);

    // Check if we already have a process for this command + params
    if let Some(process_state) = state.processes.get(&process_key) {
        debug!("Found existing process for key: {}", process_key);
        // Check if we have a cached init response
        if let Some(cached_response) = process_state.cached_init_response.read().await.as_ref() {
            let mut response = cached_response.clone();
            response.id = request.id.clone();

            // Send cached response
            if let Some(tx) = state.websocket_senders.get(&websocket_id) {
                let _ = tx.send(WebSocketMessage::JsonRpc(JsonRpcMessage::Response(
                    response,
                )));
            }

            // Store WebSocket -> Process mapping for future requests
            state
                .websocket_to_process
                .insert(websocket_id, process_key.clone());

            return Ok(());
        }
    }

    // Need to start a new process or get the init response
    let process_state = match state.processes.get(&process_key) {
        Some(existing) => existing.clone(),
        None => {
            // Create new process
            let new_process = Arc::new(ProcessState::new(command.to_string()));

            // Start the ACP process
            match start_acp_process(new_process.clone(), state.clone()).await {
                Ok(()) => {
                    state
                        .processes
                        .insert(process_key.clone(), new_process.clone());
                    new_process
                }
                Err(e) => {
                    // Send exit notification on process start failure
                    send_exit_notification(
                        state,
                        websocket_id,
                        "process_start_failed",
                        &e.to_string(),
                    )
                    .await;
                    return Ok(());
                }
            }
        }
    };

    // Store WebSocket -> Process mapping for future requests
    state
        .websocket_to_process
        .insert(websocket_id, process_key.clone());

    // Forward initialize request with mapped ID
    let session_id = extract_session_id_from_request(request);
    let proxy_id =
        process_state.map_client_id_to_proxy(websocket_id, request.id.clone(), session_id);
    let mut proxy_request = request.clone();
    proxy_request.id = proxy_id.clone();

    // Store the init request ID so we can identify the init response
    *process_state.init_request_id.write().await = Some(proxy_id);

    // Send to process
    if let Err(e) = process_state
        .send_to_process(JsonRpcMessage::Request(proxy_request))
        .await
    {
        error!("Failed to send initialize request to process: {}", e);
        send_exit_notification(
            state,
            websocket_id,
            "communication_error",
            "Failed to communicate with process",
        )
        .await;
    }

    Ok(())
}

async fn handle_tidewave_session_load(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    request: &JsonRpcRequest,
) -> Result<()> {
    let params: SessionLoadRequest =
        serde_json::from_value(request.params.clone().unwrap_or(Value::Null))
            .map_err(|e| anyhow!("Invalid session/load params: {}", e))?;

    // Check if this session exists
    let session_state = match state.sessions.get(&params.session_id) {
        Some(session) => session.clone(),
        None => {
            // Session not found
            let error_response = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32002,
                    message: "Session not found".to_string(),
                    data: None,
                }),
            };

            if let Some(tx) = state.websocket_senders.get(&websocket_id) {
                let _ = tx.send(WebSocketMessage::JsonRpc(JsonRpcMessage::Response(
                    error_response,
                )));
            }
            return Ok(());
        }
    };

    // Check if there's already an active WebSocket for this session
    if state.session_to_websocket.contains_key(&params.session_id) {
        // Session already has an active connection
        let error_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: None,
            error: Some(JsonRpcError {
                code: -32003, // Custom error code for "session already active"
                message: "Session already has an active connection".to_string(),
                data: None,
            }),
        };

        if let Some(tx) = state.websocket_senders.get(&websocket_id) {
            let _ = tx.send(WebSocketMessage::JsonRpc(JsonRpcMessage::Response(
                error_response,
            )));
        }
        return Ok(());
    }

    // Register this WebSocket for the session
    state
        .session_to_websocket
        .insert(params.session_id.clone(), websocket_id);

    // Also map this websocket to the process
    state
        .websocket_to_process
        .insert(websocket_id, session_state.process_key.clone());

    // Send buffered messages after the latest_id
    let buffered_messages = session_state
        .get_buffered_messages_after(&params.latest_id)
        .await;

    if let Some(tx) = state.websocket_senders.get(&websocket_id) {
        // Stream buffered messages
        for buffered in buffered_messages {
            let _ = tx.send(WebSocketMessage::JsonRpc(buffered.message));
        }

        // Send success response with model state
        let response_data = NewSessionResponse {
            session_id: params.session_id.clone(),
            models: session_state.models.read().await.clone(),
            modes: None,
            _meta: None,
        };

        let success_response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id.clone(),
            result: serde_json::to_value(response_data).ok(),
            error: None,
        };

        let _ = tx.send(WebSocketMessage::JsonRpc(JsonRpcMessage::Response(
            success_response,
        )));
    }

    Ok(())
}

async fn handle_acp_session_load(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    command: &str,
    request: &JsonRpcRequest,
) -> Result<()> {
    // Extract session_id from the request params
    let session_id = extract_session_id_from_request(request);

    if let Some(session_id) = session_id {
        // Get or create the session state
        let process_key = match state.sessions.get(&session_id) {
            Some(session_state) => {
                info!(
                    "session/load for existing session {} on websocket {}",
                    session_id, websocket_id
                );
                session_state.process_key.clone()
            }
            None => {
                // Session doesn't exist yet - create it
                let process_key = match find_process_key_for_websocket(state, websocket_id).await {
                    Some(key) => key,
                    None => {
                        warn!(
                            "session/load: no process mapping found for websocket {}",
                            websocket_id
                        );
                        return handle_regular_request(state, websocket_id, command, request).await;
                    }
                };

                let session_state = Arc::new(SessionState::new(process_key.clone()));
                state.sessions.insert(session_id.clone(), session_state);

                info!(
                    "Created new session {} for session/load on websocket {}",
                    session_id, websocket_id
                );
                process_key
            }
        };

        // Map this websocket to the session BEFORE forwarding the request
        // This ensures that when the agent sends notifications during session/load,
        // we can route them to the correct websocket
        state
            .session_to_websocket
            .insert(session_id.clone(), websocket_id);

        // Also map websocket to process
        state.websocket_to_process.insert(websocket_id, process_key);

        info!(
            "Mapped websocket {} to session {} for session/load",
            websocket_id, session_id
        );
    }

    // Forward the request to the agent as a regular request
    handle_regular_request(state, websocket_id, command, request).await
}

async fn handle_regular_request(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    _command: &str,
    request: &JsonRpcRequest,
) -> Result<()> {
    // Look up the process for this WebSocket
    let process_key = match state.websocket_to_process.get(&websocket_id) {
        Some(key) => key.clone(),
        None => {
            warn!("No process mapping found for WebSocket: {}", websocket_id);
            return Ok(());
        }
    };

    let process_state = match state.processes.get(&process_key) {
        Some(process) => process.clone(),
        None => {
            warn!("Process not found for key: {}", process_key);
            return Ok(());
        }
    };

    // Intercept session/set_model to update stored model state
    if request.method == "session/set_model" {
        if let Ok(params) = serde_json::from_value::<SetSessionModelParams>(
            request.params.clone().unwrap_or(Value::Null),
        ) {
            if let Some(session_state) = state.sessions.get(&params.session_id) {
                let mut models_guard = session_state.models.write().await;
                if let Some(models) = models_guard.as_mut() {
                    if let Some(models_obj) = models.as_object_mut() {
                        models_obj.insert("currentModelId".to_string(), params.model_id);
                    }
                }
            }
        }
    }

    // Map client ID to proxy ID
    let session_id = extract_session_id_from_request(request);
    let proxy_id =
        process_state.map_client_id_to_proxy(websocket_id, request.id.clone(), session_id);
    let mut proxy_request = request.clone();
    proxy_request.id = proxy_id;

    // Forward to process
    if let Err(e) = process_state
        .send_to_process(JsonRpcMessage::Request(proxy_request))
        .await
    {
        error!("Failed to send request to process: {}", e);
    }

    Ok(())
}

async fn handle_ack_notification(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    notification: &JsonRpcNotification,
) -> Result<()> {
    let params: TidewaveAckRequest =
        serde_json::from_value(notification.params.clone().unwrap_or(Value::Null))
            .map_err(|e| anyhow!("Invalid ack params: {}", e))?;

    // Find the specific session to prune
    if let Some(session_state) = state.sessions.get(&params.session_id) {
        // Verify this websocket is actually connected to this session (security check)
        if let Some(mapped_websocket_id) = state.session_to_websocket.get(&params.session_id) {
            if *mapped_websocket_id == websocket_id {
                session_state.prune_buffer(&params.latest_id).await;
            } else {
                warn!(
                    "WebSocket {} tried to ACK session {} but is not the owner",
                    websocket_id, params.session_id
                );
            }
        }
    } else {
        warn!("ACK for unknown session: {}", params.session_id);
    }

    Ok(())
}

async fn forward_notification_to_process(
    state: &AcpProxyState,
    _websocket_id: WebSocketId,
    notification: &JsonRpcNotification,
) -> Result<()> {
    // For now, forward to all processes - in the future we might route by session
    for entry in state.processes.iter() {
        let _ = entry
            .value()
            .send_to_process(JsonRpcMessage::Notification(notification.clone()))
            .await;
    }

    Ok(())
}

async fn forward_response_to_process(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    response: &JsonRpcResponse,
) -> Result<()> {
    // Look up the process for this WebSocket
    let process_key = match state.websocket_to_process.get(&websocket_id) {
        Some(key) => key.clone(),
        None => {
            warn!("No process mapping found for WebSocket: {}", websocket_id);
            return Ok(());
        }
    };

    let process_state = match state.processes.get(&process_key) {
        Some(process) => process.clone(),
        None => {
            warn!("Process not found for key: {}", process_key);
            return Ok(());
        }
    };

    // Forward response directly (no ID mapping needed for process -> client -> process flow)
    if let Err(e) = process_state
        .send_to_process(JsonRpcMessage::Response(response.clone()))
        .await
    {
        error!("Failed to send response to process: {}", e);
    }

    Ok(())
}

// ============================================================================
// Process Management
// ============================================================================

async fn start_acp_process(process_state: Arc<ProcessState>, state: AcpProxyState) -> Result<()> {
    let parts: Vec<&str> = process_state.command.split_whitespace().collect();
    if parts.is_empty() {
        return Err(anyhow!("Empty command"));
    }

    info!(
        "Starting ACP process: {} with args: {:?}",
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

    // Store child process
    *process_state.child.write().await = Some(child);

    // Create stdin channel and wire it up
    let (stdin_sender, mut stdin_receiver) = mpsc::unbounded_channel::<JsonRpcMessage>();
    *process_state.stdin_tx.write().await = Some(stdin_sender);

    // Start stdin handler (WebSocket -> Process)
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

    // Start stdout handler (Process -> WebSocket)
    let process_state_clone = process_state.clone();
    let state_clone = state.clone();
    let stdout_reader = BufReader::new(stdout);
    tokio::spawn(async move {
        let mut lines = stdout_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(message) = serde_json::from_str::<JsonRpcMessage>(&line) {
                if let Err(e) =
                    handle_process_message(&process_state_clone, &state_clone, message).await
                {
                    error!("Failed to handle process message: {}", e);
                }
            } else {
                debug!("Received non-JSON line from process: {}", line);
            }
        }
        debug!("Process stdout handler ended");
    });

    // Start stderr handler (for debugging)
    tokio::spawn(async move {
        let stderr_reader = BufReader::new(stderr);
        let mut lines = stderr_reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!("Process stderr: {}", line);
        }
        debug!("Process stderr handler ended");
    });

    // Start process exit monitor
    let process_state_exit = process_state.clone();
    let state_exit = state.clone();
    tokio::spawn(async move {
        // Wait for the process to exit
        let exit_status = {
            let mut child_guard = process_state_exit.child.write().await;
            if let Some(child) = child_guard.as_mut() {
                child.wait().await
            } else {
                return;
            }
        };

        match exit_status {
            Ok(status) => {
                if status.success() {
                    info!("Process exited successfully");
                } else {
                    error!("Process exited with status: {}", status);
                }
            }
            Err(e) => {
                error!("Failed to wait for process: {}", e);
            }
        }

        // Send exit notification to all connected websockets
        // We need to find all websockets connected to this process
        let mut websockets_to_notify = Vec::new();

        // Find the process key for this process state
        let mut process_key_opt = None;
        for entry in state_exit.processes.iter() {
            if Arc::ptr_eq(entry.value(), &process_state_exit) {
                process_key_opt = Some(entry.key().clone());
                break;
            }
        }

        if let Some(process_key) = process_key_opt {
            // Find all websockets mapped to this process
            for entry in state_exit.websocket_to_process.iter() {
                if entry.value() == &process_key {
                    websockets_to_notify.push(*entry.key());
                }
            }

            // Send exit notification to all connected websockets
            for websocket_id in websockets_to_notify {
                send_exit_notification(
                    &state_exit,
                    websocket_id,
                    "process_exited",
                    "The ACP process has exited",
                )
                .await;
            }

            // Clean up the process from the state
            state_exit.processes.remove(&process_key);
        }

        debug!("Process exit handler ended");
    });

    Ok(())
}

async fn handle_process_message(
    process_state: &Arc<ProcessState>,
    state: &AcpProxyState,
    message: JsonRpcMessage,
) -> Result<()> {
    debug!("Got process message {:?}", message);
    match &message {
        JsonRpcMessage::Response(response) => {
            // Handle responses - map proxy ID back to client ID
            if let Some((websocket_id, client_id)) =
                process_state.resolve_proxy_id_to_client(&response.id)
            {
                // Create response with original client ID
                let mut client_response = response.clone();
                client_response.id = client_id;

                // Check if this is an initialize response that we should cache and inject capabilities
                let init_request_id = process_state.init_request_id.read().await;
                if init_request_id.as_ref() == Some(&response.id) {
                    drop(init_request_id); // Release read lock
                    inject_tidewave_capabilities(&mut client_response);
                    *process_state.cached_init_response.write().await =
                        Some(client_response.clone());
                }

                // Check if this response contains a new session (likely from session/new or session/load)
                if let Some(result) = &client_response.result {
                    if let Ok(session_response) =
                        serde_json::from_value::<NewSessionResponse>(result.clone())
                    {
                        // Check if this sessionId is new (not already in our session mappings)
                        if !state.sessions.contains_key(&session_response.session_id) {
                            let process_key =
                                find_process_key_for_websocket(state, websocket_id).await;

                            if let Some(process_key) = process_key {
                                // Create session state and store model state
                                let session_state = Arc::new(SessionState::new(process_key));
                                *session_state.models.write().await = session_response.models;

                                state
                                    .sessions
                                    .insert(session_response.session_id.clone(), session_state);

                                // Map session to websocket
                                state
                                    .session_to_websocket
                                    .insert(session_response.session_id, websocket_id);
                            }
                        } else if let Some(session_state) =
                            state.sessions.get(&session_response.session_id)
                        {
                            // Session already exists (e.g., from session/load), update model state
                            *session_state.models.write().await = session_response.models;
                        }
                    }
                }

                // Send to the correct WebSocket
                if let Some(tx) = state.websocket_senders.get(&websocket_id) {
                    let _ = tx.send(WebSocketMessage::JsonRpc(JsonRpcMessage::Response(
                        client_response,
                    )));
                } else {
                    debug!("Missing original websocket for request {}", response.id);
                    // Fallback: Client disconnected, try to find session and forward to new websocket
                    let session_info = process_state
                        .proxy_to_session_ids
                        .get(&response.id)
                        .map(|entry| entry.value().clone());

                    if let Some((session_id, client_id)) = session_info {
                        // Find the current websocket for this session
                        if let Some(current_websocket_id) =
                            state.session_to_websocket.get(&session_id)
                        {
                            let current_websocket_id = *current_websocket_id;

                            // Create response with original client ID
                            let mut client_response = response.clone();
                            client_response.id = client_id;

                            // Send to the current websocket for this session
                            if let Some(tx) = state.websocket_senders.get(&current_websocket_id) {
                                let _ = tx.send(WebSocketMessage::JsonRpc(
                                    JsonRpcMessage::Response(client_response),
                                ));
                            }
                        }

                        // Clean up the session mapping
                        process_state.proxy_to_session_ids.remove(&response.id);
                    }
                }

                // Clean up ID mappings
                process_state.cleanup_id_mappings(&response.id);
            }
        }
        JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => {
            // Handle requests/notifications from process - route by sessionId
            let session_id = extract_session_id_from_message(&message);

            if let Some(session_id) = session_id {
                // Find the session state
                if let Some(session_state) = state.sessions.get(&session_id) {
                    let session_state = session_state.clone();

                    // Add notification ID if this is a notification
                    let mut routed_message = message.clone();
                    let buffer_id = if let JsonRpcMessage::Notification(ref mut n) = routed_message
                    {
                        let notif_id = session_state.generate_notification_id();

                        // Add _meta.tidewave.ai/notificationId to params
                        if let Some(params) = &mut n.params {
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
                            n.params = Some(Value::Object(params_obj));
                        }
                        notif_id
                    } else {
                        // For requests/responses, use their existing ID converted to string
                        match &routed_message {
                            JsonRpcMessage::Request(req) => req.id.to_string(),
                            JsonRpcMessage::Response(resp) => resp.id.to_string(),
                            _ => unreachable!(),
                        }
                    };

                    // Buffer the message in the session
                    let _buffer_id = session_state
                        .add_to_buffer(routed_message.clone(), buffer_id)
                        .await;

                    // Route to appropriate WebSocket based on session_id
                    if let Some(websocket_id) = state.session_to_websocket.get(&session_id) {
                        if let Some(tx) = state.websocket_senders.get(&websocket_id) {
                            let _ = tx.send(WebSocketMessage::JsonRpc(routed_message));
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
        }
    }

    Ok(())
}

// ============================================================================
// Utility Functions
// ============================================================================

fn generate_process_key(command: &str, init_params: &Value) -> ProcessKey {
    // String key of command + init params (from initialize request) for process deduplication
    format!(
        "{}:{}",
        command,
        serde_json::to_string(init_params).unwrap_or_default()
    )
}

fn inject_tidewave_capabilities(response: &mut JsonRpcResponse) {
    if let Some(result) = &mut response.result {
        if let Some(result_obj) = result.as_object_mut() {
            if let Some(agent_caps) = result_obj.get_mut("agentCapabilities") {
                if let Some(caps_obj) = agent_caps.as_object_mut() {
                    // Add our custom _meta capabilities within agentCapabilities
                    let mut meta_obj = caps_obj
                        .get("_meta")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();

                    let mut tidewave_obj = Map::new();
                    tidewave_obj.insert("session/load".to_string(), Value::Bool(true));
                    meta_obj.insert("tidewave.ai".to_string(), Value::Object(tidewave_obj));

                    caps_obj.insert("_meta".to_string(), Value::Object(meta_obj));
                }
            }
        }
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

async fn find_process_key_for_websocket(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
) -> Option<ProcessKey> {
    state
        .websocket_to_process
        .get(&websocket_id)
        .map(|key| key.clone())
}

async fn send_exit_notification(
    state: &AcpProxyState,
    websocket_id: WebSocketId,
    error_type: &str,
    message: &str,
) {
    let exit_notification = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "_tidewave.ai/exit".to_string(),
        params: Some(
            serde_json::to_value(TidewaveExitParams {
                error: error_type.to_string(),
                message: message.to_string(),
            })
            .unwrap(),
        ),
    };

    if let Some(tx) = state.websocket_senders.get(&websocket_id) {
        let _ = tx.send(WebSocketMessage::JsonRpc(JsonRpcMessage::Notification(
            exit_notification,
        )));
    }
}
