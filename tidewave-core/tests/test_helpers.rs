use agent_client_protocol::{self as acp, Agent};
use futures::{AsyncRead, AsyncWrite, Sink, Stream, StreamExt};
use reqwest::Client;
use std::{
    io,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use futures::stream::SplitSink;
use futures::stream::SplitStream;

/// A guard that ensures processes are killed on drop, especially on test failures (panics)
pub struct TestGuard {
    server_process: Option<tokio::process::Child>,
    stderr_buffer: Arc<Mutex<Vec<String>>>,
}

impl TestGuard {
    pub fn new(
        server_process: tokio::process::Child,
        stderr_buffer: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            server_process: Some(server_process),
            stderr_buffer,
        }
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        // If we're dropping because of a panic, print the stderr content
        if std::thread::panicking() {
            eprintln!("Test failed! Server stderr output:");
            for line in self.stderr_buffer.lock().unwrap().iter() {
                eprintln!("{}", line);
            }
        }

        // Force kill the server process
        if let Some(mut server_process) = self.server_process.take() {
            let _ = server_process.start_kill();
        }
    }
}

/// Collects stderr lines in the background
fn collect_stderr(
    mut stderr_reader: BufReader<tokio::process::ChildStderr>,
) -> Arc<Mutex<Vec<String>>> {
    let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = stderr_buffer.clone();

    tokio::spawn(async move {
        let mut line = String::new();
        while let Ok(bytes_read) = stderr_reader.read_line(&mut line).await {
            if bytes_read == 0 {
                break;
            }
            buffer_clone.lock().unwrap().push(line.clone());
            line.clear();
        }
    });

    stderr_buffer
}

/// Helper to spawn tidewave server as external process
pub async fn spawn_tidewave_server(port: u16) -> TestGuard {
    let mut server_cmd = tokio::process::Command::new(get_binary_path("tidewave"));
    server_cmd
        .arg("--port")
        .arg(port.to_string())
        .arg("--debug")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut server_process = server_cmd.spawn().expect("Failed to start tidewave server");

    // Collect stderr for debugging
    let stderr = server_process.stderr.take().unwrap();
    let stderr_reader = BufReader::new(stderr);
    let stderr_buffer = collect_stderr(stderr_reader);

    // Wait for server to be ready
    wait_for_server(port).await.unwrap();

    TestGuard::new(server_process, stderr_buffer)
}

// Helper to find an available port
pub async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// Helper to get a binary path using CARGO_MANIFEST_DIR
fn get_binary_path(bin_name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // Go up to workspace root
    path.push("target");
    path.push("debug");
    path.push(bin_name);

    if !path.exists() {
        panic!(
            "{} binary not found at {:?}. Run 'cargo build --bin {}' first.",
            bin_name, path, bin_name
        );
    }

    path
}

// Helper to get the demo agent binary path
pub fn get_demo_agent_path() -> PathBuf {
    get_binary_path("acp-demo-agent")
}

// Helper to wait for server to be ready
async fn wait_for_server(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();
    let url = format!("http://127.0.0.1:{}/", port);

    for _ in 0..30 {
        // Try for 3 seconds
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err("Server did not start in time".into())
}

/// WebSocket read adapter
pub struct WebSocketReader {
    ws_read: SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    read_buffer: Vec<u8>,
    read_pos: usize,
}

impl WebSocketReader {
    pub fn new(
        ws_read: SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    ) -> Self {
        Self {
            ws_read,
            read_buffer: Vec::new(),
            read_pos: 0,
        }
    }
}

impl AsyncRead for WebSocketReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // If we have data in buffer, serve from buffer first
        if self.read_pos < self.read_buffer.len() {
            let available = self.read_buffer.len() - self.read_pos;
            let to_read = buf.len().min(available);

            buf[..to_read]
                .copy_from_slice(&self.read_buffer[self.read_pos..self.read_pos + to_read]);
            self.read_pos += to_read;

            return Poll::Ready(Ok(to_read));
        }

        // Buffer is empty, read new message from WebSocket
        match Pin::new(&mut self.ws_read).poll_next(cx) {
            Poll::Ready(Some(Ok(Message::Text(text)))) => {
                println!("WebSocket received: {}", text);
                let mut text_bytes = text.into_bytes();
                text_bytes.push(b'\n'); // Add newline for JSON-RPC line protocol

                let to_read = buf.len().min(text_bytes.len());
                buf[..to_read].copy_from_slice(&text_bytes[..to_read]);

                // Store remainder in buffer if needed
                if to_read < text_bytes.len() {
                    self.read_buffer = text_bytes[to_read..].to_vec();
                    self.read_pos = 0;
                } else {
                    self.read_buffer.clear();
                    self.read_pos = 0;
                }

                Poll::Ready(Ok(to_read))
            }
            Poll::Ready(Some(Ok(Message::Close(_)))) => {
                Poll::Ready(Ok(0)) // EOF
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
            Poll::Ready(None) => {
                Poll::Ready(Ok(0)) // EOF
            }
            Poll::Pending => Poll::Pending,
            _ => Poll::Pending, // Ignore other message types
        }
    }
}

/// WebSocket write adapter
pub struct WebSocketWriter {
    ws_write: SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>,
}

impl WebSocketWriter {
    pub fn new(
        ws_write: SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    ) -> Self {
        Self { ws_write }
    }
}

impl AsyncWrite for WebSocketWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        // Convert bytes to string and remove trailing newline
        let mut text = String::from_utf8_lossy(buf).into_owned();
        if text.ends_with('\n') {
            text.pop();
        }

        println!("{}", text);

        // Check for special close message
        // This is kind of hacky, but I did not find a better way to actually close the ACP websocket
        if text.contains("__TIDEWAVE_CLOSE__") {
            println!("WebSocket received close signal, sending close frame");
            let close_message = Message::Close(None);
            match Pin::new(&mut self.ws_write).poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    let _ = Pin::new(&mut self.ws_write).start_send(close_message);
                    let _ = Pin::new(&mut self.ws_write).poll_close(cx);
                }
                _ => {}
            }
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Connection closed by special message",
            )));
        }

        println!("WebSocket sending: {}", text);
        let message = Message::Text(text);
        match Pin::new(&mut self.ws_write).poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                match Pin::new(&mut self.ws_write).start_send(message) {
                    Ok(()) => {
                        // Immediately flush to ensure message is sent
                        match Pin::new(&mut self.ws_write).poll_flush(cx) {
                            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
                            Poll::Ready(Err(e)) => {
                                Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)))
                            }
                            Poll::Pending => Poll::Pending,
                        }
                    }
                    Err(e) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match Pin::new(&mut self.ws_write).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match Pin::new(&mut self.ws_write).poll_close(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Test ACP client implementation with notification channel
pub struct TestClient {
    pub notification_tx: mpsc::UnboundedSender<acp::SessionNotification>,
}

impl TestClient {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<acp::SessionNotification>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            TestClient {
                notification_tx: tx,
            },
            rx,
        )
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for TestClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        // For tests, always deny permissions
        Ok(acp::RequestPermissionResponse {
            outcome: acp::RequestPermissionOutcome::Selected {
                option_id: acp::PermissionOptionId("deny".into()),
            },
            meta: None,
        })
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

    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        println!("Session notification: {:?}", args.update);
        let _ = self.notification_tx.send(args); // Ignore send errors if receiver is dropped
        Ok(())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

/// Wrapper for ACP connection that ensures proper WebSocket closure
pub struct AcpConnectionWrapper {
    pub conn: acp::ClientSideConnection,
    pub notification_rx: mpsc::UnboundedReceiver<acp::SessionNotification>,
}

/// Helper to create an ACP client connection over WebSocket
pub async fn create_acp_connection(
    ws_url: &str,
) -> Result<AcpConnectionWrapper, Box<dyn std::error::Error>> {
    // Connect to WebSocket
    let (ws_stream, _) = connect_async(ws_url).await?;

    // Split WebSocket stream
    let (ws_write, ws_read) = ws_stream.split();
    let ws_reader = WebSocketReader::new(ws_read);

    // Create close channel
    let ws_writer = WebSocketWriter::new(ws_write);

    // Create test client with notification channel
    let (test_client, notification_rx) = TestClient::new();

    // Create ACP client connection
    let (conn, handle_io) =
        acp::ClientSideConnection::new(test_client, ws_writer, ws_reader, |fut| {
            tokio::task::spawn_local(fut);
        });

    // Start I/O handler
    let _ = tokio::task::spawn_local(async move {
        if let Err(e) = handle_io.await {
            println!("I/O handler error: {:?}", e);
        } else {
            println!("I/O handler completed normally");
        }
    });

    // Initialize the connection
    conn.initialize(acp::InitializeRequest {
        protocol_version: acp::V1,
        client_capabilities: acp::ClientCapabilities::default(),
        meta: None,
    })
    .await?;

    Ok(AcpConnectionWrapper {
        conn,
        notification_rx,
    })
}
