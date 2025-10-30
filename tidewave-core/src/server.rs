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
use std::time::UNIX_EPOCH;
use tokio::{io::AsyncReadExt, net::TcpListener, process::Command};
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
}

#[derive(Deserialize)]
struct StatFileParams {
    path: String,
}

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
}

#[derive(Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct WhichParams {
    command: String,
    // Note that cwd is only used in case PATH in env is also set
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
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
    let bind_addr = if config.allow_remote_access {
        format!("0.0.0.0:{}", port)
    } else {
        format!("127.0.0.1:{}", port)
    };
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
        .route("/about", get(about))
        .route("/shell", post(shell_handler))
        .route("/read", post(read_file_handler))
        .route("/write", post(write_file_handler))
        .route("/stat", get(stat_file_handler))
        .route("/which", post(which_handler))
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
    let path = Path::new(&payload.path);

    if !path.is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let result = async {
        let content = tokio::fs::read_to_string(&payload.path)
            .await
            .map_err(|e| e.kind().to_string())?;
        let mtime = fetch_mtime(payload.path)?;
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
    let path = Path::new(&payload.path);

    if !path.is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let path_str = payload.path.clone();
    let content = payload.content.clone();
    let bytes_written = content.len();

    let result = async {
        let path = Path::new(&path_str);

        let parent_path = path.parent().unwrap_or_else(|| &path);
        if !parent_path.exists() {
            tokio::fs::create_dir_all(parent_path)
                .await
                .map_err(|e| e.kind().to_string())?;
        }

        tokio::fs::write(&path, content)
            .await
            .map_err(|e| e.kind().to_string())?;

        let mtime = fetch_mtime(path_str)?;
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
    let path = Path::new(&query.path);

    if !path.is_absolute() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mtime_op = fetch_mtime(query.path);

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

async fn about() -> Json<AboutResponse> {
    Json(AboutResponse {
        name: "tidewave-app".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn root() -> Html<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;

    #[tokio::test]
    async fn test_which_handler_finds_common_command() {
        let params = WhichParams {
            command: "sh".to_string(),
            cwd: None,
            env: None,
        };

        let result = which_handler(Json(params)).await;

        assert!(result.is_ok());
        let response = result.unwrap().0;
        assert!(response.path.is_some());
        let path = response.path.unwrap();
        assert!(!path.is_empty());
        assert!(path.contains("sh"));
    }

    #[tokio::test]
    async fn test_which_handler_nonexistent_command() {
        let params = WhichParams {
            command: "this_command_definitely_does_not_exist_12345".to_string(),
            cwd: None,
            env: None,
        };

        let result = which_handler(Json(params)).await;

        assert!(result.is_ok());
        let response = result.unwrap().0;
        assert!(response.path.is_none());
    }

    #[tokio::test]
    async fn test_which_handler_respects_custom_path() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = std::env::temp_dir().join(format!("which_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

        let executable_name = "test_executable_unique_12345";
        let executable_path = temp_dir.join(executable_name);
        fs::write(&executable_path, "#!/bin/sh\necho test").expect("Failed to write executable");

        let mut perms = fs::metadata(&executable_path)
            .expect("Failed to get metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&executable_path, perms).expect("Failed to set permissions");

        // First, verify the binary is NOT found without custom PATH
        let params_without_env = WhichParams {
            command: executable_name.to_string(),
            cwd: None,
            env: None,
        };

        let result_without_env = which_handler(Json(params_without_env)).await;
        assert!(result_without_env.is_ok());
        let response_without_env = result_without_env.unwrap().0;
        assert!(
            response_without_env.path.is_none(),
            "Binary should not be found in system PATH"
        );

        // Now verify the binary IS found with custom PATH
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), temp_dir.to_string_lossy().to_string());

        let params = WhichParams {
            command: executable_name.to_string(),
            cwd: Some(temp_dir.to_string_lossy().to_string()),
            env: Some(env),
        };

        let result = which_handler(Json(params)).await;

        fs::remove_dir_all(&temp_dir).ok();

        assert!(result.is_ok());
        let response = result.unwrap().0;
        assert!(response.path.is_some());
        let path = response.path.unwrap();
        assert!(path.contains(executable_name));
        assert!(path.contains(temp_dir.to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn test_verify_origin() {
        use tower::ServiceExt;

        let config = ServerConfig {
            allowed_origins: vec!["http://localhost:3000".to_string()],
        };

        let app = Router::new()
            .route("/stat", get(stat_file_handler))
            .route("/about", get(about))
            .layer(middleware::from_fn(move |mut req: Request, next| {
                req.extensions_mut().insert(config.clone());
                verify_origin(req, next)
            }));

        // Test 1: Allowed origin should pass on /stat
        let req = Request::builder()
            .uri("/stat?path=/tmp")
            .header("Origin", "http://localhost:3000")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::FORBIDDEN);

        // Test 2: Evil origin should be blocked on /stat
        let req = Request::builder()
            .uri("/stat?path=/tmp")
            .header("Origin", "http://evil.com")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // Test 3: Evil origin should work on /about
        let req = Request::builder()
            .uri("/about")
            .header("Origin", "http://evil.com")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
