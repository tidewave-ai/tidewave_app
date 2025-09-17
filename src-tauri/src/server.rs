use axum::{response::Html, routing::get, Router};
use std::env;
use tokio::net::TcpListener;

pub async fn start_http_server(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = Router::new().route("/", get(root));

    let listener = TcpListener::bind(&format!("0.0.0.0:{}", port)).await?;
    println!("HTTP server running on {}", port);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> Html<String> {
    let client_url = env::var("TIDEWAVE_CLIENT_URL")
        .unwrap_or_else(|_| "https://tidewave.ai".to_string());

    let html = format!(
        r#"<html>
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <script type="module" src="{}/tc/tc.js"></script>
  </head>
  <body></body>
</html>"#,
        client_url
    );

    Html(html)
}