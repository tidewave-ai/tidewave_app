use crate::command::create_shell_command;
use crate::config::Config;
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
use std::time::UNIX_EPOCH;
use tokio::{io::AsyncReadExt, net::TcpListener};
use tracing::{debug, error, info};
use which;

#[derive(Deserialize)]
struct ProxyParams {
    url: String,
}

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
struct StatFileParams {
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

#[derive(Serialize)]
#[serde(untagged)]
enum StatFileResponse {
    StatFileResponseOk { success: bool, mtime: u64 },
    StatFileResponseErr { success: bool, error: String },
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
struct WhichResponse {
    path: Option<String>,
}

#[derive(Serialize)]
struct AboutResponse {
    name: String,
    version: String,
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
    let listener = bind_http_server(config.clone()).await?;
    serve_http_server(config, listener).await
}

fn get_bind_addr(port: u16, allow_remote_access: bool) -> String {
    if allow_remote_access {
        format!("0.0.0.0:{}", port)
    } else {
        format!("127.0.0.1:{}", port)
    }
}

pub async fn bind_http_server(
    config: Config,
) -> Result<TcpListener, Box<dyn std::error::Error + Send + Sync>> {
    let bind_addr = get_bind_addr(config.port, config.allow_remote_access);
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("HTTP server bound to {}", bind_addr);
    Ok(listener)
}

pub async fn serve_http_server(
    config: Config,
    listener: TcpListener,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_http_server_with_shutdown(config, listener, std::future::pending()).await
}

pub async fn serve_http_server_with_shutdown(
    config: Config,
    listener: TcpListener,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let client = Client::builder()
        .use_preconfigured_tls(ClientConfig::with_platform_verifier())
        .build()?;

    let port = if config.port == 0 {
        listener.local_addr()?.port()
    } else {
        config.port
    };

    let https_port = config.https_port;
    let mcp_state = crate::mcp_remote::McpRemoteState::new();
    let acp_state = crate::acp_proxy::AcpProxyState::new();

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
        .with_state(acp_state);

    // Create the main app without state
    let client_for_proxy = client.clone();
    let mut app = Router::new()
        .route("/", get(root))
        .route("/about", get(about))
        .route("/shell", post(shell_handler))
        .route("/read", post(read_file_handler))
        .route("/write", post(write_file_handler))
        .route("/stat", get(stat_file_handler))
        .route("/which", post(which_handler))
        .route(
            "/proxy",
            axum::routing::any(move |params, req| {
                let client = client_for_proxy.clone();
                proxy_handler(params, req, client)
            }),
        )
        .merge(mcp_routes)
        .merge(acp_routes);

    // Add dev mode proxy routes if TIDEWAVE_DEV_MODE=1 and
    // TIDEWAVE_CLIENT_URL is set
    if env::var("TIDEWAVE_DEV_MODE").as_deref() == Ok("1") {
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

    // Start HTTP server
    let http_task = {
        let app = app.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal)
                .await
        })
    };

    // Optionally start HTTPS server
    let https_task = if let Some(https_port) = https_port {
        info!("Starting HTTPS server on port {}", https_port);

        let cert_path = config
            .https_cert_path
            .as_ref()
            .expect("https_cert_path validated");
        let key_path = config
            .https_key_path
            .as_ref()
            .expect("https_key_path validated");
        let rustls_config = crate::tls::load_tls_config_from_paths(cert_path, key_path)?;

        // Create HTTPS listener using the same binding logic as HTTP
        let bind_addr = get_bind_addr(https_port, config.allow_remote_access);
        let https_addr: std::net::SocketAddr = bind_addr.parse()?;

        let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(rustls_config);
        let https_handle = axum_server::Handle::new();

        Some(tokio::spawn(async move {
            axum_server::bind_rustls(https_addr, tls_config)
                .handle(https_handle)
                .serve(app.into_make_service())
                .await
        }))
    } else {
        None
    };

    // Wait for both servers
    let http_result = http_task.await;
    if let Some(https_task) = https_task {
        let https_result = https_task.await;
        https_result??;
    }
    http_result??;

    Ok(())
}

async fn verify_origin(
    req: Request,
    next: axum::middleware::Next,
) -> Result<Response<Body>, StatusCode> {
    // Skip origin verification for /about route
    if req.uri().path() == "/about" {
        return Ok(next.run(req).await);
    }

    let headers = req.headers();

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;

        let config = req.extensions().get::<ServerConfig>().ok_or_else(|| {
            error!("ServerConfig not found in request extensions");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        if config.allowed_origins.contains(&origin_str.to_string()) {
            return Ok(next.run(req).await);
        }

        if let Ok(origin_url) = Url::parse(origin_str) {
            if let Some(host) = origin_url.host_str() {
                let is_localhost_variant =
                    host == "localhost" || host == "127.0.0.1" || host.ends_with(".localhost");

                let scheme = origin_url.scheme();
                let origin_port = origin_url.port();

                // Check if scheme + port matches our HTTP server
                if is_localhost_variant
                    && scheme == "http"
                    && origin_port.unwrap_or(80) == config.port
                {
                    return Ok(next.run(req).await);
                }

                // Check if scheme + port matches our HTTPS server
                if is_localhost_variant
                    && scheme == "https"
                    && config.https_port.is_some()
                    && origin_port.unwrap_or(443) == config.https_port.unwrap()
                {
                    return Ok(next.run(req).await);
                }
            }
        }

        debug!("Rejected request with origin: {}", origin_str);
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(next.run(req).await)
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

    let mut child = command
        .spawn()
        .map_err(|e| shell_error(&format!("Failed to spawn command: {}", e)))?;

    let mut stdout = Some(
        child
            .stdout
            .take()
            .ok_or_else(|| shell_error("Failed to get process stdout"))?,
    );
    let mut stderr = Some(
        child
            .stderr
            .take()
            .ok_or_else(|| shell_error("Failed to get process stderr"))?,
    );

    let stream = async_stream::stream! {
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

        match child.wait().await {
            Ok(status) => yield Ok(create_status_chunk(status.code().unwrap_or(-1))),
            Err(e) => yield Err(e),
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

#[cfg(target_os = "windows")]
async fn wslpath_to_windows(wsl_path: &str) -> Result<String, String> {
    use tokio::process::Command;
    let mut command = Command::new("wsl.exe");
    command
        .arg("wslpath")
        .arg("-w")
        .arg(wsl_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = command
        .output()
        .await
        .map_err(|e| format!("Failed to run wslpath: {}", e))?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(path)
    } else {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("wslpath failed: {}", error))
    }
}

async fn read_file_handler(
    Json(payload): Json<ReadFileParams>,
) -> Result<Json<ReadFileResponse>, StatusCode> {
    #[cfg(target_os = "windows")]
    let file_path = if payload.is_wsl {
        match wslpath_to_windows(&payload.path).await {
            Ok(windows_path) => windows_path,
            Err(error) => {
                return Ok(Json(ReadFileResponse::ReadFileResponseErr {
                    success: false,
                    error,
                }));
            }
        }
    } else {
        payload.path.clone()
    };

    #[cfg(not(target_os = "windows"))]
    let file_path = payload.path.clone();

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
    #[cfg(target_os = "windows")]
    let file_path = if payload.is_wsl {
        match wslpath_to_windows(&payload.path).await {
            Ok(windows_path) => windows_path,
            Err(error) => {
                return Ok(Json(WriteFileResponse::WriteFileResponseErr {
                    success: false,
                    error,
                }));
            }
        }
    } else {
        payload.path.clone()
    };

    #[cfg(not(target_os = "windows"))]
    let file_path = payload.path.clone();

    if !Path::new(&file_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let content = payload.content.clone();
    let bytes_written = content.len();

    let result = async {
        let path = Path::new(&file_path);

        let parent_path = path.parent().unwrap_or_else(|| &path);
        if !parent_path.exists() {
            tokio::fs::create_dir_all(parent_path)
                .await
                .map_err(|e| e.kind().to_string())?;
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(|e| e.kind().to_string())?;

        let mtime = fetch_mtime(file_path)?;
        Ok::<_, String>(mtime)
    }
    .await;

    match result {
        Ok(mtime) => Ok(Json(WriteFileResponse::WriteFileResponseOk {
            success: true,
            bytes_written,
            mtime,
        })),
        Err(error) => Ok(Json(WriteFileResponse::WriteFileResponseErr {
            success: false,
            error,
        })),
    }
}

async fn stat_file_handler(
    Query(query): Query<StatFileParams>,
) -> Result<Json<StatFileResponse>, StatusCode> {
    #[cfg(target_os = "windows")]
    let file_path = if query.is_wsl {
        match wslpath_to_windows(&query.path).await {
            Ok(windows_path) => windows_path,
            Err(error) => {
                return Ok(Json(StatFileResponse::StatFileResponseErr {
                    success: false,
                    error,
                }));
            }
        }
    } else {
        query.path.clone()
    };

    #[cfg(not(target_os = "windows"))]
    let file_path = query.path.clone();

    if !Path::new(&file_path).is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mtime_op = fetch_mtime(file_path);

    match mtime_op {
        Ok(mtime) => Ok(Json(StatFileResponse::StatFileResponseOk {
            success: true,
            mtime,
        })),
        Err(error) => Ok(Json(StatFileResponse::StatFileResponseErr {
            success: false,
            error,
        })),
    }
}

fn fetch_mtime(path: String) -> Result<u64, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.kind().to_string())?;
    let mtime = metadata.modified().map_err(|e| e.kind().to_string())?;

    return mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| "system time error".to_string());
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

async fn do_proxy(
    target_url: String,
    req: Request,
    client: Client,
) -> Result<Response<Body>, StatusCode> {
    debug!("Proxying {} request to: {}", req.method(), target_url);

    let method = req.method().clone();
    let headers = req.headers().clone();

    // Convert body to bytes
    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Helper closure to build a request with the given URL and optional Host header
    let build_request = |url: &str, custom_host: Option<&str>| {
        let mut req_builder = client.request(method.clone(), url);

        // Forward headers (excluding Host, connection-specific headers, and compression headers)
        for (key, value) in headers.iter() {
            let key_str = key.as_str();
            if ![
                "host",
                "connection",
                "transfer-encoding",
                "upgrade",
                "accept-encoding",
                "content-encoding",
                "origin",
            ]
            .contains(&key_str)
            {
                req_builder = req_builder.header(key.clone(), value.clone());
            }
        }

        // Set custom Host header if provided
        if let Some(host) = custom_host {
            req_builder = req_builder.header("Host", host);
        }

        req_builder.body(body_bytes.clone())
    };

    // Execute the request
    let mut response = build_request(&target_url, None).send().await;

    // If connection failed for *.localhost, retry with 127.0.0.1, as in RFC 6761
    if let Err(e) = &response {
        if e.is_connect() {
            if let Ok(mut url) = Url::parse(&target_url) {
                if let Some(host) = url.host_str() {
                    if host.ends_with(".localhost") {
                        let host_string = host.to_string();
                        info!(
                            "Connection to {} failed, retrying with 127.0.0.1",
                            host_string
                        );
                        url.set_host(Some("127.0.0.1")).ok();

                        response = build_request(url.as_str(), Some(&host_string)).send().await;
                    }
                }
            }
        }
    }

    // Unwrap the response or return error
    let response = response.map_err(|e| {
        error!("Proxy request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    // Get status and headers from the response
    let status = response.status();
    let headers = response.headers().clone();

    // Stream the response body
    let body_stream = response.bytes_stream();
    let body = Body::from_stream(body_stream);

    // Build the response
    let mut resp_builder = Response::builder().status(status.as_u16());

    // Forward response headers (excluding connection and encoding headers since we're not handling compression)
    for (key, value) in headers.iter() {
        let key_str = key.as_str();
        if ![
            "connection",
            "transfer-encoding",
            "content-encoding",
            "content-length",
        ]
        .contains(&key_str)
        {
            resp_builder = resp_builder.header(key.clone(), value.clone());
        }
    }

    resp_builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn proxy_handler(
    Query(params): Query<ProxyParams>,
    req: Request,
    client: Client,
) -> Result<Response<Body>, StatusCode> {
    let target_url = params.url;

    // Ensure the URL is valid
    if !target_url.starts_with("http://") && !target_url.starts_with("https://") {
        debug!("Invalid URL: {}", target_url);
        return Err(StatusCode::BAD_REQUEST);
    }

    do_proxy(target_url, req, client).await
}

async fn client_proxy_handler(
    req: Request,
    client: Client,
    client_url: String,
) -> Result<Response<Body>, StatusCode> {
    let path = req.uri().path();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let target_url = format!("{}{}{}", client_url, path, query);

    do_proxy(target_url, req, client).await
}

async fn about() -> Result<Response<Body>, StatusCode> {
    let response_body = AboutResponse {
        name: "tidewave-cli".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let json_body =
        serde_json::to_string(&response_body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Response::builder()
        .header("Access-Control-Allow-Origin", "*")
        .header("Content-Type", "application/json")
        .body(Body::from(json_body))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
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
