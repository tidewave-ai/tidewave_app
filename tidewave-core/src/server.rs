use crate::command::{create_shell_command, spawn_command};
use crate::config::Config;
use crate::http_handlers::{client_proxy_handler, download_handler, proxy_handler, DownloadState};
use crate::utils::{load_tls_config_from_paths, normalize_path};
use axum::{
    body::{Body, Bytes},
    extract::{Json, Query, Request},
    http::{header, StatusCode},
    middleware,
    response::{Html, Response},
    routing::{get, post},
    Router,
};
use bytes::BytesMut;
use reqwest::{Client, Url};
use rustls::ClientConfig;
use rustls_platform_verifier::ConfigVerifierExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info};
use which;

#[derive(Deserialize)]
struct ShellParams {
    command: String,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct StatParams {
    path: String,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct ListDirParams {
    path: String,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct MkdirParams {
    path: String,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
    #[serde(default)]
    exclusive: bool,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct DeleteFileParams {
    path: String,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct WhichParams {
    command: String,
    // Note that cwd is only used in case PATH in env is also set
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    #[allow(dead_code)]
    is_wsl: bool,
}

#[derive(Deserialize)]
struct AboutParams {
    #[serde(default)]
    is_wsl: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
enum StatResponse {
    StatResponseOk {
        success: bool,
        mtime: u64,
        #[serde(rename = "type")]
        path_type: String,
    },
    StatResponseErr {
        success: bool,
        error: String,
    },
}

#[derive(Serialize)]
struct DirEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ListDirResponse {
    ListDirResponseOk {
        success: bool,
        entries: Vec<DirEntry>,
    },
    ListDirResponseErr {
        success: bool,
        error: String,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum MkdirResponse {
    MkdirResponseOk { success: bool },
    MkdirResponseErr { success: bool, error: String },
}

#[derive(Serialize)]
#[serde(untagged)]
enum ReadFileResponse {
    ReadFileResponseOk {
        success: bool,
        content: String,
        mtime: u64,
    },
    ReadFileResponseErr {
        success: bool,
        error: String,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum WriteFileResponse {
    WriteFileResponseOk {
        success: bool,
        bytes_written: usize,
        mtime: u64,
    },
    WriteFileResponseErr {
        success: bool,
        error: String,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum DeleteFileResponse {
    DeleteFileResponseOk { success: bool },
    DeleteFileResponseErr { success: bool, error: String },
}

#[derive(Serialize)]
struct WhichResponse {
    path: Option<String>,
}

#[derive(Serialize)]
struct SystemInfo {
    os: &'static str,
    arch: String,
    family: &'static str,
    target: &'static str,
    wsl: bool,
}

#[derive(Serialize)]
struct AboutResponse {
    name: String,
    version: String,
    system: SystemInfo,
}

#[derive(Serialize)]
struct CheckOriginResponse {
    valid: bool,
}

#[derive(Clone)]
struct ServerConfig {
    allowed_origins: Vec<String>,
    port: u16,
    https_port: Option<u16>,
}

pub async fn start_http_server(
    config: Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_http_server_with_shutdown(config, std::future::pending()).await
}

fn get_bind_addr(port: u16, allow_remote_access: bool) -> std::net::SocketAddr {
    let ip = if allow_remote_access {
        std::net::Ipv4Addr::UNSPECIFIED
    } else {
        std::net::Ipv4Addr::LOCALHOST
    };
    std::net::SocketAddr::from((ip, port))
}

pub async fn serve_http_server_with_shutdown(
    config: Config,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_http_server_inner(config, None, shutdown_signal).await
}

pub async fn serve_http_server_with_listener(
    config: Config,
    listener: tokio::net::TcpListener,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_http_server_inner(config, Some(listener), shutdown_signal).await
}

async fn serve_http_server_inner(
    config: Config,
    listener: Option<tokio::net::TcpListener>,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Single client without automatic compression (we handle it manually in download handler)
    let client = Client::builder()
        .use_preconfigured_tls(ClientConfig::with_platform_verifier())
        .no_gzip()
        .no_brotli()
        .build()?;

    let http_addr = get_bind_addr(config.port, config.allow_remote_access);
    let http_handle = axum_server::Handle::new();

    // Determine the port - if listener provided, use its port; otherwise use config
    let port = if let Some(ref listener) = listener {
        listener.local_addr()?.port()
    } else {
        config.port
    };

    let https_port = config.https_port;
    let mcp_state = crate::mcp_remote::McpRemoteState::new();
    let acp_state = crate::acp_proxy::AcpProxyState::new();
    let download_state = DownloadState::new();

    // Build allowed origins for both HTTP and HTTPS
    let mut allowed_origins = config.allowed_origins.clone();

    allowed_origins.push(format!("http://localhost:{}", port));
    allowed_origins.push(format!("http://127.0.0.1:{}", port));

    if let Some(https_port) = https_port {
        allowed_origins.push(format!("https://localhost:{}", https_port));
        allowed_origins.push(format!("https://127.0.0.1:{}", https_port));
    }

    let server_config = ServerConfig {
        allowed_origins,
        port,
        https_port,
    };

    // Create the MCP routes that need state
    let mcp_routes = Router::new()
        .route(
            "/acp/mcp-remote",
            get(crate::mcp_remote::mcp_remote_ws_handler),
        )
        .route(
            "/acp/mcp-remote-client",
            post(crate::mcp_remote::mcp_remote_client_handler),
        )
        .with_state(mcp_state);

    // Create ACP routes
    let acp_routes = Router::new()
        .route("/acp/ws", get(crate::acp_proxy::acp_ws_handler))
        .with_state(acp_state.clone());

    // Create download routes
    let client_for_download = client.clone();
    let download_routes = Router::new()
        .route(
            "/download",
            get(move |params, state| {
                let client = client_for_download.clone();
                download_handler(params, state, client)
            }),
        )
        .with_state(download_state);

    // Create WebSocket routes
    let ws_state = crate::ws::WsState::new();
    let ws_routes = Router::new()
        .route("/socket/websocket", get(crate::ws::ws_handler))
        .with_state(ws_state.clone());

    // Create the main app without state
    let client_for_proxy = client.clone();
    let mut app = Router::new()
        .route("/", get(root))
        .route("/about", get(about))
        .route("/check-origin", post(check_origin_handler))
        .route("/read", post(read_file_handler))
        .route("/write", post(write_file_handler))
        .route("/delete", post(delete_file_handler))
        .route("/stat", get(stat_handler))
        .route("/listdir", get(listdir_handler))
        .route("/mkdir", post(mkdir_handler))
        .route("/shell", post(shell_handler))
        .route("/which", post(which_handler))
        .route(
            "/proxy",
            axum::routing::any(move |params, req| {
                let client = client_for_proxy.clone();
                proxy_handler(params, req, client)
            }),
        )
        .merge(mcp_routes)
        .merge(acp_routes)
        .merge(download_routes)
        .merge(ws_routes);

    // Add dev mode proxy routes if TIDEWAVE_CLIENT_PROXY=1 and
    // TIDEWAVE_CLIENT_URL is set
    if env::var("TIDEWAVE_CLIENT_PROXY").as_deref() == Ok("1") {
        if let Ok(client_url) = env::var("TIDEWAVE_CLIENT_URL") {
            for route_path in ["/tidewave", "/tidewave/{*path}"] {
                let client_url_clone = client_url.clone();
                let client_clone = client.clone();
                app = app.route(
                    route_path,
                    axum::routing::any(move |req| {
                        let client = client_clone.clone();
                        let dev_url = client_url_clone.clone();
                        client_proxy_handler(req, client, dev_url)
                    }),
                );
            }
        }
    }

    let app = app.layer(middleware::from_fn(move |mut req: Request, next| {
        req.extensions_mut().insert(server_config.clone());
        verify_origin(req, next)
    }));

    // Optionally set up HTTPS server
    let (https_handle, https_task) = if let Some(https_port) = https_port {
        info!("Starting HTTPS server on port {}", https_port);

        let cert_path = config
            .https_cert_path
            .as_ref()
            .expect("https_cert_path validated");
        let key_path = config
            .https_key_path
            .as_ref()
            .expect("https_key_path validated");
        let rustls_config = load_tls_config_from_paths(cert_path, key_path)?;

        let https_addr = get_bind_addr(https_port, config.allow_remote_access);
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(rustls_config);
        let https_handle = axum_server::Handle::new();
        let https_handle_clone = https_handle.clone();

        let task = tokio::spawn({
            let app = app.clone();
            async move {
                axum_server::bind_rustls(https_addr, tls_config)
                    .handle(https_handle_clone)
                    .serve(app.into_make_service())
                    .await
            }
        });

        (Some(https_handle), Some(task))
    } else {
        (None, None)
    };

    // Start HTTP server
    let http_handle_clone = http_handle.clone();
    let http_task = if let Some(listener) = listener {
        let std_listener = listener.into_std()?;
        tokio::spawn(async move {
            axum_server::from_tcp(std_listener)
                .handle(http_handle_clone)
                .serve(app.into_make_service())
                .await
        })
    } else {
        tokio::spawn(async move {
            axum_server::bind(http_addr)
                .handle(http_handle_clone)
                .serve(app.into_make_service())
                .await
        })
    };

    // Spawn shutdown handler that triggers graceful shutdown with timeout
    tokio::spawn({
        let http_handle = http_handle.clone();
        let https_handle = https_handle.clone();
        async move {
            shutdown_signal.await;
            info!("Shutdown signal received, initiating graceful shutdown with 10s timeout");
            http_handle.graceful_shutdown(Some(Duration::from_secs(10)));
            if let Some(handle) = https_handle {
                handle.graceful_shutdown(Some(Duration::from_secs(10)));
            }
        }
    });

    // Wait for both servers
    let http_result = http_task.await;
    if let Some(https_task) = https_task {
        let https_result = https_task.await;
        https_result??;
    }
    http_result??;

    // Kill all ACP processes via their exit channels.
    // Note: We use the exit_tx channel instead of directly killing, because
    // the exit monitor task holds the child write lock while waiting.
    let acp_process_count = acp_state.processes.len();
    debug!("Found {} ACP processes to clean up", acp_process_count);

    for entry in acp_state.processes.iter() {
        let process_state = entry.value();
        if let Some(exit_tx) = process_state.exit_tx.read().await.as_ref() {
            let _ = exit_tx.send(());
        }
    }

    // Clear all WebSocket state
    ws_state.clear();

    Ok(())
}

fn is_valid_origin(req: &Request, config: &ServerConfig) -> bool {
    let headers = req.headers();
    let origin_str = headers
        .get(header::ORIGIN)
        .and_then(|origin| origin.to_str().ok());

    // If there is no origin header, it is valid
    let Some(origin_str) = origin_str else {
        return true;
    };

    // Check if origin is in allowed list
    if config.allowed_origins.contains(&origin_str.to_string()) {
        return true;
    }

    // Parse and validate localhost variants
    if let Ok(origin_url) = Url::parse(origin_str) {
        if let Some(host) = origin_url.host_str() {
            let is_localhost_variant =
                host == "localhost" || host == "127.0.0.1" || host.ends_with(".localhost");

            let scheme = origin_url.scheme();
            let origin_port = origin_url.port();

            // Check if scheme + port matches our HTTP server
            if is_localhost_variant && scheme == "http" && origin_port.unwrap_or(80) == config.port
            {
                return true;
            }

            // Check if scheme + port matches our HTTPS server
            if is_localhost_variant
                && scheme == "https"
                && config.https_port.is_some()
                && origin_port.unwrap_or(443) == config.https_port.unwrap()
            {
                return true;
            }
        }
    }

    false
}

async fn verify_origin(
    req: Request,
    next: axum::middleware::Next,
) -> Result<Response<Body>, StatusCode> {
    // Skip origin verification for /about and /check-origin routes
    let path = req.uri().path();
    if path == "/about" || path == "/check-origin" {
        return Ok(next.run(req).await);
    }

    let config = req.extensions().get::<ServerConfig>().ok_or_else(|| {
        error!("ServerConfig not found in request extensions");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if is_valid_origin(&req, config) {
        return Ok(next.run(req).await);
    }

    debug!(
        "Rejected request with origin: {:?}",
        req.headers().get(header::ORIGIN)
    );
    Err(StatusCode::FORBIDDEN)
}

#[derive(Serialize)]
struct ShellError {
    error: String,
}

fn shell_error(message: &str) -> (StatusCode, Json<ShellError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ShellError {
            error: message.to_string(),
        }),
    )
}

async fn shell_handler(
    Json(payload): Json<ShellParams>,
) -> Result<Response<Body>, (StatusCode, Json<ShellError>)> {
    let cwd = payload.cwd.unwrap_or(".".to_string());
    let env = payload.env.unwrap_or_else(|| std::env::vars().collect());

    let mut command = create_shell_command(&payload.command, env, &cwd, payload.is_wsl);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut process = spawn_command(command)
        .map_err(|e| shell_error(&format!("Failed to spawn command: {}", e)))?;

    let mut stdout = Some(
        process
            .child
            .stdout
            .take()
            .ok_or_else(|| shell_error("Failed to get process stdout"))?,
    );
    let mut stderr = Some(
        process
            .child
            .stderr
            .take()
            .ok_or_else(|| shell_error("Failed to get process stderr"))?,
    );

    // Wrap process in Arc<Mutex<Option>> so we can take it out at the end.
    // If stream is dropped early, the ChildProcess::Drop will kill the process tree.
    let process_holder = Arc::new(std::sync::Mutex::new(Some(process)));
    let process_holder_clone = process_holder.clone();

    let stream = async_stream::stream! {
        // Hold reference to process_holder so ChildProcess lives as long as stream
        let _process_holder = process_holder_clone;

        let (mut stdout_buf, mut stderr_buf) = (vec![0u8; 4096], vec![0u8; 4096]);

        loop {
            tokio::select! {
                result = async { stdout.as_mut().unwrap().read(&mut stdout_buf).await }, if stdout.is_some() => {
                    match result {
                        Ok(0) => stdout = None,
                        Ok(n) => yield Ok(create_data_chunk(&stdout_buf[..n])),
                        Err(e) => { yield Err(e); break; }
                    }
                }
                result = async { stderr.as_mut().unwrap().read(&mut stderr_buf).await }, if stderr.is_some() => {
                    match result {
                        Ok(0) => stderr = None,
                        Ok(n) => yield Ok(create_data_chunk(&stderr_buf[..n])),
                        Err(e) => { yield Err(e); break; }
                    }
                }
                else => break,
            }
        }

        // Take the process to wait on it (prevents Drop from killing it since process completed normally)
        let process_opt = _process_holder.lock().ok().and_then(|mut g| g.take());
        let status = if let Some(mut process) = process_opt {
            match process.child.wait().await {
                Ok(status) => Some(status.code().unwrap_or(-1)),
                Err(e) => {
                    yield Err(e);
                    None
                }
            }
        } else {
            None
        };

        if let Some(code) = status {
            yield Ok(create_status_chunk(code));
        }
    };

    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(body)
        .map_err(|e| shell_error(&format!("Failed to build response: {}", e)))?)
}

fn create_data_chunk(data: &[u8]) -> Bytes {
    let mut chunk = BytesMut::new();
    chunk.extend_from_slice(&[0u8]); // type = 0 (data)
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes()); // length
    chunk.extend_from_slice(data); // data
    chunk.freeze()
}

fn create_status_chunk(status: i32) -> Bytes {
    let json = format!(r#"{{"status":{}}}"#, status);
    let mut chunk = BytesMut::new();
    chunk.extend_from_slice(&[1u8]); // type = 1 (status)
    chunk.extend_from_slice(&(json.len() as u32).to_be_bytes());
    chunk.extend_from_slice(json.as_bytes());
    chunk.freeze()
}

async fn read_file_handler(
    Json(payload): Json<ReadFileParams>,
) -> Result<Json<ReadFileResponse>, StatusCode> {
    let file_path = match normalize_path(&payload.path, payload.is_wsl).await {
        Ok(path) => path,
        Err(error) => {
            return Ok(Json(ReadFileResponse::ReadFileResponseErr {
                success: false,
                error,
            }));
        }
    };

    if !Path::new(&file_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let result = async {
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| e.kind().to_string())?;
        let mtime = fetch_mtime(file_path)?;
        Ok::<_, String>((content, mtime))
    }
    .await;

    match result {
        Ok((content, mtime)) => Ok(Json(ReadFileResponse::ReadFileResponseOk {
            success: true,
            content,
            mtime,
        })),
        Err(error) => Ok(Json(ReadFileResponse::ReadFileResponseErr {
            success: false,
            error,
        })),
    }
}

async fn write_file_handler(
    Json(payload): Json<WriteFileParams>,
) -> Result<Json<WriteFileResponse>, StatusCode> {
    let file_path = match normalize_path(&payload.path, payload.is_wsl).await {
        Ok(path) => path,
        Err(error) => {
            return Ok(Json(WriteFileResponse::WriteFileResponseErr {
                success: false,
                error,
            }));
        }
    };

    if !Path::new(&file_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let content = payload.content.clone();
    let exclusive = payload.exclusive;
    let bytes_written = content.len();

    let result = async {
        let path = Path::new(&file_path);

        let parent_path = path.parent().unwrap_or(path);
        if !parent_path.exists() {
            tokio::fs::create_dir_all(parent_path)
                .await
                .map_err(|e| (false, e.kind().to_string()))?;
        }

        if exclusive {
            // Atomic exclusive write using O_CREAT | O_EXCL semantics
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
                .map_err(|e| {
                    (
                        e.kind() == std::io::ErrorKind::AlreadyExists,
                        e.kind().to_string(),
                    )
                })?;
            file.write_all(content.as_bytes())
                .await
                .map_err(|e| (false, e.kind().to_string()))?;
        } else {
            tokio::fs::write(&path, content)
                .await
                .map_err(|e| (false, e.kind().to_string()))?;
        }

        let mtime = fetch_mtime(file_path).map_err(|e| (false, e))?;
        Ok::<_, (bool, String)>(mtime)
    }
    .await;

    match result {
        Ok(mtime) => Ok(Json(WriteFileResponse::WriteFileResponseOk {
            success: true,
            bytes_written,
            mtime,
        })),
        Err((true, _)) => Err(StatusCode::CONFLICT),
        Err((false, error)) => Ok(Json(WriteFileResponse::WriteFileResponseErr {
            success: false,
            error,
        })),
    }
}

async fn delete_file_handler(
    Json(payload): Json<DeleteFileParams>,
) -> Result<Json<DeleteFileResponse>, StatusCode> {
    let file_path = match normalize_path(&payload.path, payload.is_wsl).await {
        Ok(path) => path,
        Err(error) => {
            return Ok(Json(DeleteFileResponse::DeleteFileResponseErr {
                success: false,
                error,
            }));
        }
    };

    if !Path::new(&file_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let result = tokio::fs::remove_file(&file_path).await;

    match result {
        Ok(()) => Ok(Json(DeleteFileResponse::DeleteFileResponseOk {
            success: true,
        })),
        Err(error) => Ok(Json(DeleteFileResponse::DeleteFileResponseErr {
            success: false,
            error: error.kind().to_string(),
        })),
    }
}

async fn stat_handler(Query(query): Query<StatParams>) -> Result<Json<StatResponse>, StatusCode> {
    let file_path = match normalize_path(&query.path, query.is_wsl).await {
        Ok(path) => path,
        Err(error) => {
            return Ok(Json(StatResponse::StatResponseErr {
                success: false,
                error,
            }));
        }
    };

    if !Path::new(&file_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let result = fetch_stat(&file_path);

    match result {
        Ok((mtime, path_type)) => Ok(Json(StatResponse::StatResponseOk {
            success: true,
            mtime,
            path_type,
        })),
        Err(error) => Ok(Json(StatResponse::StatResponseErr {
            success: false,
            error,
        })),
    }
}

async fn listdir_handler(
    Query(query): Query<ListDirParams>,
) -> Result<Json<ListDirResponse>, StatusCode> {
    let dir_path = match normalize_path(&query.path, query.is_wsl).await {
        Ok(path) => path,
        Err(error) => {
            return Ok(Json(ListDirResponse::ListDirResponseErr {
                success: false,
                error,
            }));
        }
    };

    if !Path::new(&dir_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let result = fetch_dir_entries(&dir_path);

    match result {
        Ok(entries) => Ok(Json(ListDirResponse::ListDirResponseOk {
            success: true,
            entries,
        })),
        Err(error) => Ok(Json(ListDirResponse::ListDirResponseErr {
            success: false,
            error,
        })),
    }
}

async fn mkdir_handler(
    Json(payload): Json<MkdirParams>,
) -> Result<Json<MkdirResponse>, StatusCode> {
    let dir_path = match normalize_path(&payload.path, payload.is_wsl).await {
        Ok(path) => path,
        Err(error) => {
            return Ok(Json(MkdirResponse::MkdirResponseErr {
                success: false,
                error,
            }));
        }
    };

    if !Path::new(&dir_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let result = tokio::fs::create_dir_all(&dir_path).await;

    match result {
        Ok(()) => Ok(Json(MkdirResponse::MkdirResponseOk { success: true })),
        Err(error) => Ok(Json(MkdirResponse::MkdirResponseErr {
            success: false,
            error: error.kind().to_string(),
        })),
    }
}

fn fetch_dir_entries(path: &str) -> Result<Vec<DirEntry>, String> {
    let entries = std::fs::read_dir(path).map_err(|e| e.kind().to_string())?;

    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.kind().to_string())?;

        // Skip entries with invalid UTF-8 names
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };

        let file_type = entry.file_type().map_err(|e| e.kind().to_string())?;
        let entry_type = if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else if file_type.is_symlink() {
            "symlink"
        } else {
            "other"
        };

        result.push(DirEntry {
            name,
            entry_type: entry_type.to_string(),
        });
    }

    Ok(result)
}

fn fetch_mtime(path: String) -> Result<u64, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.kind().to_string())?;
    let mtime = metadata.modified().map_err(|e| e.kind().to_string())?;

    return mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| "system time error".to_string());
}

fn fetch_stat(path: &str) -> Result<(u64, String), String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.kind().to_string())?;
    let mtime = metadata
        .modified()
        .map_err(|e| e.kind().to_string())?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| "system time error".to_string())?;

    let path_type = if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else if metadata.is_symlink() {
        "symlink"
    } else {
        "other"
    };

    Ok((mtime, path_type.to_string()))
}

async fn which_handler(Json(params): Json<WhichParams>) -> Result<Json<WhichResponse>, StatusCode> {
    #[cfg(target_os = "windows")]
    {
        // Check if we're in WSL context
        if let Some(env) = &params.env {
            if env.get("WSL_DISTRO_NAME").is_some() {
                // Run which command inside WSL
                let cwd = params.cwd.as_deref().unwrap_or(".");
                let env_clone = env.clone();
                let command_str = format!("which {}", params.command);

                let mut command = create_shell_command(&command_str, env_clone, cwd, params.is_wsl);
                command.stdout(Stdio::piped()).stderr(Stdio::piped());

                let output = command
                    .output()
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                if output.status.success() {
                    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !path.is_empty() {
                        return Ok(Json(WhichResponse { path: Some(path) }));
                    }
                }
                return Ok(Json(WhichResponse { path: None }));
            }
        }
    }

    // Non-WSL case: use the which crate
    let result = if let Some(env) = params.env {
        if let Some(paths) = env.get("PATH") {
            let paths = paths.clone();
            let cwd = params.cwd.unwrap_or(".".to_string());
            let command = params.command.clone();
            tokio::task::spawn_blocking(move || which::which_in(command, Some(paths), cwd)).await
        } else {
            tokio::task::spawn_blocking(|| which::which(params.command)).await
        }
    } else {
        tokio::task::spawn_blocking(|| which::which(params.command)).await
    };

    match result {
        Ok(Ok(path)) => Ok(Json(WhichResponse {
            // this is a lossy conversion in case the path contains non UTF-8
            // characters, but we don't try to use the path as is and it's also unlikely,
            // so it's fine
            path: Some(path.display().to_string()),
        })),
        _ => Ok(Json(WhichResponse { path: None })),
    }
}

async fn about(Query(params): Query<AboutParams>) -> Result<Response<Body>, StatusCode> {
    #[cfg(target_os = "windows")]
    {
        if params.is_wsl {
            let mut command = create_shell_command("uname -m", HashMap::new(), "~", true);
            command.stdout(Stdio::piped()).stderr(Stdio::piped());

            let output = command
                .output()
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            if output.status.success() {
                let arch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let response_body = AboutResponse {
                    name: "tidewave-cli".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    system: SystemInfo {
                        os: "linux",
                        arch,
                        family: "unix",
                        target: env!("TARGET"),
                        wsl: true,
                    },
                };

                let json_body = serde_json::to_string(&response_body)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                return Response::builder()
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Content-Type", "application/json")
                    .body(Body::from(json_body))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            };

            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    _ = params;

    let response_body = AboutResponse {
        name: "tidewave-cli".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        system: SystemInfo {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH.to_string(),
            family: std::env::consts::FAMILY,
            target: env!("TARGET"),
            wsl: false,
        },
    };

    let json_body =
        serde_json::to_string(&response_body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .header("Content-Type", "application/json")
        .body(Body::from(json_body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn check_origin_handler(req: Request) -> Result<Json<CheckOriginResponse>, StatusCode> {
    let config = req.extensions().get::<ServerConfig>().ok_or_else(|| {
        error!("ServerConfig not found in request extensions");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let valid = is_valid_origin(&req, config);

    Ok(Json(CheckOriginResponse { valid }))
}

async fn root(_req: Request) -> Html<String> {
    let client_url =
        env::var("TIDEWAVE_CLIENT_URL").unwrap_or_else(|_| "https://tidewave.ai".to_string());

    let html = format!(
        r#"<!DOCTYPE html>
<html>
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <meta name="tidewave:source" content="cli" />
    <meta name="tidewave:version" content="{}" />
    <script type="module" src="{}/tc/tc.js"></script>
  </head>
  <body></body>
</html>"#,
        env!("CARGO_PKG_VERSION").to_string(),
        client_url,
    );

    Html(html)
}
