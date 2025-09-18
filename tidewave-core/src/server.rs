use axum::{
    body::Body,
    extract::{Query, Request},
    http::{header, StatusCode},
    middleware,
    response::{Html, Response},
    routing::get,
    Router,
};
use reqwest::Client;
use serde::Deserialize;
use std::env;
use tokio::net::TcpListener;
use tracing::{debug, info, error};

#[derive(Deserialize)]
struct ProxyParams {
    url: String,
}

#[derive(Clone)]
struct ServerConfig {
    allowed_origins: Vec<String>,
}

async fn verify_origin(req: Request, next: axum::middleware::Next) -> Result<Response<Body>, StatusCode> {
    let headers = req.headers();

    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin_str = origin.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;

        let config = req
            .extensions()
            .get::<ServerConfig>()
            .ok_or_else(|| {
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

pub async fn start_http_server(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Client::new();

    let config = ServerConfig {
        allowed_origins: vec![
            format!("http://localhost:{}", port),
            format!("http://127.0.0.1:{}", port),
        ],
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/proxy",
            axum::routing::any(move |params, req| {
                let client = client.clone();
                proxy_handler(params, req, client)
            })
        )
        .route("/acp", get(crate::acp::acp_handler))
        .layer(middleware::from_fn(move |mut req: Request, next| {
            req.extensions_mut().insert(config.clone());
            verify_origin(req, next)
        }));

    let listener = TcpListener::bind(&format!("127.0.0.1:{}", port)).await?;
    info!("HTTP server running on port {}", port);
    axum::serve(listener, app).await?;
    Ok(())
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
        if !["host", "connection", "transfer-encoding", "upgrade", "accept-encoding", "content-encoding"].contains(&key_str) {
            req_builder = req_builder.header(key.clone(), value.clone());
        }
    }

    // Forward the body (bytes can be converted to reqwest::Body)
    req_builder = req_builder.body(body_bytes);

    // Execute the request
    let response = req_builder
        .send()
        .await
        .map_err(|e| {
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
        if !["connection", "transfer-encoding", "content-encoding", "content-length"].contains(&key_str) {
            resp_builder = resp_builder.header(key.clone(), value.clone());
        }
    }

    resp_builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn root() -> Html<String> {
    let client_url = env::var("TIDEWAVE_CLIENT_URL")
        .unwrap_or_else(|_| "https://tidewave.ai".to_string());

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