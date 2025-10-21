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
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::process::Stdio;
use tokio::{io::AsyncReadExt, net::TcpListener, process::Command};
use tracing::{debug, error, info};

#[derive(Deserialize)]
struct ProxyParams {
    url: String,
}

#[derive(Deserialize)]
struct ShellParams {
    command: String,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
}

#[derive(Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
    parents: Option<bool>,
}

#[derive(Serialize)]
struct ReadFileResponse {
    content: String,
    total_lines: usize,
}

#[derive(Serialize)]
struct WriteFileResponse {
    success: bool,
    bytes_written: usize,
}

#[derive(Clone)]
struct ServerConfig {
    allowed_origins: Vec<String>,
}

pub async fn start_http_server(
    config: Config,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = bind_http_server(config.clone()).await?;
    serve_http_server(config, listener).await
}

pub async fn bind_http_server(
    config: Config,
) -> Result<TcpListener, Box<dyn std::error::Error + Send + Sync>> {
    let port = config.port;
    let listener = TcpListener::bind(&format!("127.0.0.1:{}", port)).await?;
    info!("HTTP server bound to port {}", port);
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
    let client = Client::new();
    let port = config.port;
    let mcp_state = crate::mcp_remote::McpRemoteState::new();
    let acp_state = crate::acp_proxy::AcpProxyState::new();

    let config = ServerConfig {
        allowed_origins: vec![
            format!("http://localhost:{}", port),
            format!("http://127.0.0.1:{}", port),
        ],
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
    let app = Router::new()
        .layer(middleware::from_fn(move |mut req: Request, next| {
            req.extensions_mut().insert(config.clone());
            verify_origin(req, next)
        }))
        .route("/", get(root))
        .route("/shell", post(shell_handler))
        .route("/read", post(read_file_handler))
        .route("/write", post(write_file_handler))
        .route(
            "/proxy",
            axum::routing::any(move |params, req| {
                let client = client.clone();
                proxy_handler(params, req, client)
            }),
        )
        .merge(mcp_routes)
        .merge(acp_routes);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await?;
    Ok(())
}

async fn verify_origin(
    req: Request,
    next: axum::middleware::Next,
) -> Result<Response<Body>, StatusCode> {
    let headers = req.headers();

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;

        let config = req.extensions().get::<ServerConfig>().ok_or_else(|| {
            error!("ServerConfig not found in request extensions");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        if !config.allowed_origins.contains(&origin_str.to_string()) {
            debug!("Rejected request with origin: {}", origin_str);
            return Err(StatusCode::FORBIDDEN);
        }

        return Ok(next.run(req).await);
    }

    Ok(next.run(req).await)
}

async fn shell_handler(Json(payload): Json<ShellParams>) -> Result<Response<Body>, StatusCode> {
    let (cmd, args) = get_shell_command(&payload.command);
    let cwd = payload.cwd.unwrap_or(".".to_string());
    let env = payload.env.unwrap_or_else(|| std::env::vars().collect());

    let mut child = Command::new(cmd)
        .args(args)
        .envs(env)
        .current_dir(Path::new(&cwd))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut stdout = Some(
        child
            .stdout
            .take()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    let mut stderr = Some(
        child
            .stderr
            .take()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?,
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
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?)
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

fn get_shell_command(cmd: &str) -> (&'static str, Vec<&str>) {
    #[cfg(target_os = "windows")]
    {
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        ("cmd.exe", vec!["/s", "/c", cmd])
    }

    #[cfg(not(target_os = "windows"))]
    {
        ("sh", vec!["-c", cmd])
    }
}

async fn read_file_handler(
    Json(payload): Json<ReadFileParams>,
) -> Result<Json<ReadFileResponse>, StatusCode> {
    let content = tokio::fs::read_to_string(&payload.path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    Ok(Json(ReadFileResponse {
        content: lines.join("\n"),
        total_lines,
    }))
}

async fn write_file_handler(
    Json(payload): Json<WriteFileParams>,
) -> Result<Json<WriteFileResponse>, StatusCode> {
    let path = Path::new(&payload.path);
    let parent_path = path.parent().unwrap_or_else(|| &path);

    if payload.parents.unwrap_or(false) && !parent_path.exists() {
        tokio::fs::create_dir_all(parent_path)
            .await
            .map_err(|_e| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    tokio::fs::write(&path, &payload.content)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    let bytes_written = payload.content.len();

    Ok(Json(WriteFileResponse {
        success: true,
        bytes_written,
    }))
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

    debug!("Proxying {} request to: {}", req.method(), target_url);

    let method = req.method().clone();
    let headers = req.headers().clone();

    // Convert body to bytes
    let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Build the request
    let mut req_builder = client.request(method, &target_url);

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

    // Forward the body (bytes can be converted to reqwest::Body)
    req_builder = req_builder.body(body_bytes);

    // Execute the request
    let response = req_builder.send().await.map_err(|e| {
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

async fn root() -> Html<String> {
    let client_url =
        env::var("TIDEWAVE_CLIENT_URL").unwrap_or_else(|_| "https://tidewave.ai".to_string());

    let html = format!(
        r#"<html>
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <meta name="tidewave:source" content="cli" />
    <script type="module" src="{}/tc/tc.js"></script>
  </head>
  <body></body>
</html>"#,
        client_url
    );

    Html(html)
}
