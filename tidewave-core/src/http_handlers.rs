//! HTTP request handlers for proxy and download functionality.
//!
//! # Testing via Command Line
//!
//! ## Start the CLI server
//! ```bash
//! cargo run -p tidewave-cli
//! # Server starts on http://localhost:9832 by default
//! ```
//!
//! ## Test Proxy Endpoint
//!
//! Proxy a simple GET request:
//! ```bash
//! curl "http://localhost:9832/proxy?url=https://httpbin.org/get"
//! ```
//!
//! Proxy with custom headers:
//! ```bash
//! curl -H "X-Custom-Header: value" \
//!   "http://localhost:9832/proxy?url=https://httpbin.org/headers"
//! ```
//!
//! ## Test Download Endpoint
//!
//! ### 1. Create a test file to serve
//! ```bash
//! # Create a 1MB test file
//! dd if=/dev/zero of=/tmp/testfile.bin bs=1024 count=1024
//!
//! # Serve it with Python's HTTP server (port 8000)
//! cd /tmp && python3 -m http.server 8000
//! ```
//!
//! ### 2. Download without throttling
//! ```bash
//! curl "http://localhost:9832/download?key=myfile&url=http://localhost:8000/testfile.bin"
//! # Returns newline-delimited JSON (NDJSON) using chunked transfer encoding:
//! # With Content-Length header (total size known):
//! # {"status":"progress","size":10240,"total":1048576}   (progress every 1%)
//! # {"status":"progress","size":20480,"total":1048576}
//! # ...
//! # {"status":"done","path":"/path/to/cached/file"}
//! #
//! # Without Content-Length header (total size unknown):
//! # {"status":"progress","size":1048576}                 (progress every 1MB)
//! # {"status":"progress","size":2097152}
//! # ...
//! # {"status":"done","path":"/path/to/cached/file"}
//! ```
//!
//! ### 3. Test caching
//! ```bash
//! # First download
//! curl "http://localhost:9832/download?key=cached&url=http://localhost:8000/testfile.bin"
//!
//! # Second download (returns immediately with just done message)
//! curl "http://localhost:9832/download?key=cached&url=http://localhost:8000/testfile.bin"
//! # Returns: {"status":"done","path":"/path/to/cached/file"}
//! ```
//!
//! ### 4. Test concurrent downloads
//! ```bash
//! # Terminal 1
//! curl "http://localhost:9832/download?key=shared&url=http://localhost:8000/testfile.bin&throttle=10240"
//!
//! # Terminal 2 (start shortly after)
//! curl "http://localhost:9832/download?key=shared&url=http://localhost:8000/testfile.bin&throttle=10240"
//! # Both requests share the same download stream
//! ```

use axum::body::{Body, Bytes};
use axum::extract::{Query, Request};
use axum::http::StatusCode;
use axum::response::Response;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info};

#[derive(Deserialize)]
pub struct ProxyParams {
    pub url: String,
}

#[derive(Deserialize)]
pub struct DownloadParams {
    pub key: String,
    pub url: String,
    #[serde(default)]
    pub throttle: Option<u64>, // Optional bytes per second throttle for testing
}

#[derive(Serialize, Clone)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum DownloadProgress {
    Progress {
        size: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    Done {
        path: String,
    },
}

pub struct ActiveDownload {
    /// Broadcast channel to send progress updates to all listeners
    pub tx: tokio::sync::broadcast::Sender<Result<DownloadProgress, String>>,
    /// Flag to ensure only one task starts the download
    pub started: AtomicBool,
}

#[derive(Clone)]
pub struct DownloadState {
    /// Active downloads (key -> active_download)
    pub downloads: Arc<dashmap::DashMap<String, Arc<ActiveDownload>>>,
    /// Cache directory for downloaded files
    pub cache_dir: Arc<std::path::PathBuf>,
}

impl DownloadState {
    pub fn new() -> Self {
        // Use platform-specific cache directory
        // Linux: ~/.cache/tidewave/downloads
        // macOS: ~/Library/Caches/tidewave/downloads
        // Windows: {FOLDERID_LocalAppData}/tidewave/downloads
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| std::env::temp_dir())
            .join("tidewave")
            .join("downloads");

        // Clean up old .part files on startup to remove orphaned files from crashes
        // We don't delete the entire .tmp directory in case other instances are running
        let tmp_dir = cache_dir.join(".tmp");
        if tmp_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
                for entry in entries.flatten() {
                    if let Ok(metadata) = entry.metadata() {
                        // Delete .part files older than 1 hour
                        if let Ok(modified) = metadata.modified() {
                            if let Ok(elapsed) = modified.elapsed() {
                                if elapsed.as_secs() > 3600 && entry.path().extension().map(|e| e == "part").unwrap_or(false) {
                                    let _ = std::fs::remove_file(entry.path());
                                    debug!("Cleaned up old temp file: {:?}", entry.path());
                                }
                            }
                        }
                    }
                }
            }
        }

        Self {
            downloads: Arc::new(dashmap::DashMap::new()),
            cache_dir: Arc::new(cache_dir),
        }
    }

    pub fn get_file_path(&self, key: &str) -> std::path::PathBuf {
        self.cache_dir.join(key)
    }
}

/// Build a request for proxying or downloading, with optional custom host header
pub fn build_http_request(
    client: &Client,
    method: axum::http::Method,
    url: &str,
    headers: &axum::http::HeaderMap,
    body_bytes: Bytes,
    custom_host: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut req_builder = client.request(method, url);

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

    req_builder.body(body_bytes)
}

pub async fn download_handler(
    Query(params): Query<DownloadParams>,
    axum::extract::State(download_state): axum::extract::State<DownloadState>,
    client: Client,
) -> Result<Response<Body>, StatusCode> {
    let key = params.key;
    let url = params.url;
    let throttle = params.throttle;

    // Validate key to prevent path traversal attacks
    if key.contains('/') || key.contains('\\') || key.contains(':') || key.contains("..") || key.contains('.') {
        debug!("Invalid key (contains forbidden characters): {}", key);
        return Err(StatusCode::BAD_REQUEST);
    }

    if !url.starts_with("http://") && !url.starts_with("https://") {
        debug!("Invalid URL: {}", url);
        return Err(StatusCode::BAD_REQUEST);
    }

    let file_path = download_state.get_file_path(&key);

    // Get or create active download entry
    let active_download = download_state
        .downloads
        .entry(key.clone())
        .or_insert_with(|| {
            let (tx, _rx) = tokio::sync::broadcast::channel(100);
            Arc::new(ActiveDownload {
                tx,
                started: AtomicBool::new(false),
            })
        })
        .clone();

    // Always subscribe to the download progress
    let mut rx = active_download.tx.subscribe();

    // Atomically check if we should start the download
    // Only the first caller will transition from false -> true
    let should_start_download = active_download
        .started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();

    if should_start_download {
        let tx = active_download.tx.clone();
        let download_state_clone = download_state.clone();
        let key_clone = key.clone();
        let file_path_clone = file_path.clone();
        let cache_dir_clone = download_state.cache_dir.clone();

        tokio::spawn(async move {
            // Check if file already exists (completed previously)
            if file_path_clone.exists() {
                // File already downloaded - just close the channel
                // Subscribers will check file and emit done themselves
                drop(tx);
                download_state_clone.downloads.remove(&key_clone);
                return;
            }

            // File doesn't exist - perform the download
            let result = perform_download(client, url, file_path_clone, cache_dir_clone, tx.clone(), throttle).await;

            match result {
                Ok(_final_path) => {
                    // Download succeeded
                }
                Err(e) => {
                    // Download failed
                    let _ = tx.send(Err(e));
                }
            }

            // Drop tx to close the channel, signaling subscribers we're done
            drop(tx);

            // Remove from active downloads
            download_state_clone.downloads.remove(&key_clone);
        });
    }

    // Stream the progress updates to the client as newline-delimited JSON
    let file_path_for_stream = file_path.clone();
    let stream = async_stream::stream! {
        // Receive updates from broadcast channel
        loop {
            match rx.recv().await {
                Ok(progress_result) => {
                    match progress_result {
                        Ok(progress) => {
                            let json = match serde_json::to_string(&progress) {
                                Ok(j) => j,
                                Err(_) => break,
                            };
                            let line = format!("{}\n", json);
                            yield Ok::<_, std::io::Error>(Bytes::from(line));
                        }
                        Err(error_msg) => {
                            error!("Download error: {}", error_msg);
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Channel closed - download finished or never started
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // We missed some messages but download is still ongoing
                    continue;
                }
            }
        }

        // Channel closed - file must exist or something went wrong
        // Each subscriber emits their own done chunk, so no worries about duplicates
        if !file_path_for_stream.exists() {
            error!(
                "Download completed but file does not exist: {:?}. This indicates a bug in the download logic.",
                file_path_for_stream
            );
            // End the stream - client will see no done message
            return;
        }

        let done_progress = DownloadProgress::Done {
            path: file_path_for_stream.display().to_string(),
        };
        if let Ok(json) = serde_json::to_string(&done_progress) {
            let line = format!("{}\n", json);
            yield Ok(Bytes::from(line));
        }
    };

    let body = Body::from_stream(stream);

    // Axum automatically uses chunked transfer encoding when Content-Length is not set
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn perform_download(
    client: Client,
    url: String,
    file_path: std::path::PathBuf,
    cache_dir: Arc<std::path::PathBuf>,
    tx: tokio::sync::broadcast::Sender<Result<DownloadProgress, String>>,
    throttle: Option<u64>,
) -> Result<String, String> {
    debug!("Starting download from {} to {:?}", url, file_path);

    // Ensure cache directory exists
    tokio::fs::create_dir_all(&*cache_dir)
        .await
        .map_err(|e| format!("Failed to create cache directory: {}", e))?;

    // Create .tmp directory for partial downloads
    let tmp_dir = cache_dir.join(".tmp");
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    // Generate unique temp file name using UUID
    let temp_file_name = format!("{}.part", uuid::Uuid::new_v4());
    let temp_path = tmp_dir.join(temp_file_name);

    debug!("Downloading to temp file: {:?}", temp_path);

    // Make the request with Accept-Encoding header
    let response = client
        .get(&url)
        .header("Accept-Encoding", "gzip, br")
        .send()
        .await
        .map_err(|e| format!("Failed to send request: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP error: {}", response.status()));
    }

    // Check if response is compressed (clone the value to avoid borrowing)
    let content_encoding = response.headers()
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Get total size (compressed size if compressed, uncompressed otherwise)
    let total_size = response.content_length().unwrap_or(0);

    // Open temp file for writing
    let mut file = tokio::fs::File::create(&temp_path)
        .await
        .map_err(|e| format!("Failed to create temp file: {}", e))?;

    let mut next_progress_threshold: u64 = 0;

    // Determine progress reporting strategy:
    // - If total_size known: report every 1% (total_size / 100)
    // - If total_size unknown: report every 1MB (1048576 bytes)
    let progress_step = if total_size > 0 {
        total_size / 100
    } else {
        1048576 // 1MB
    };

    let stream = response.bytes_stream();

    // Download and write chunks - wrap in a closure for error handling
    let download_result: Result<(), String> = async {
        use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder};
        use futures::StreamExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        use tokio_util::io::StreamReader;

        // Create a shared counter for bytes downloaded over the wire
        let bytes_counter = Arc::new(AtomicU64::new(0));
        let counter_clone = bytes_counter.clone();

        // Create a stream that tracks bytes
        let counting_stream = stream.map(move |result| {
            result.map(|bytes| {
                counter_clone.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                bytes
            })
        });

        let stream_reader = StreamReader::new(counting_stream.map(|result| {
            result.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        }));

        // Create reader - either with decompression or passthrough
        let mut reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = match content_encoding.as_deref() {
            Some("br") => {
                debug!("Using brotli decompression");
                Box::new(BrotliDecoder::new(tokio::io::BufReader::new(stream_reader)))
            }
            Some("gzip") => {
                debug!("Using gzip decompression");
                Box::new(GzipDecoder::new(tokio::io::BufReader::new(stream_reader)))
            }
            _ => {
                debug!("No compression");
                Box::new(tokio::io::BufReader::new(stream_reader))
            }
        };

        // Unified read loop for both compressed and uncompressed
        let mut buffer = vec![0u8; 8192];
        loop {
            // Timeout per chunk: 30 seconds
            let bytes_read = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::io::AsyncReadExt::read(&mut reader, &mut buffer)
            )
            .await
            .map_err(|_| "Chunk read timed out after 30 seconds".to_string())?
            .map_err(|e| format!("Failed to read/decompress: {}", e))?;

            if bytes_read == 0 {
                break;
            }

            // Apply throttling if specified - hardcoded 1 second delay
            if throttle.is_some() {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }

            file.write_all(&buffer[..bytes_read])
                .await
                .map_err(|e| format!("Failed to write to file: {}", e))?;

            // Get bytes downloaded over the wire (compressed or uncompressed)
            let network_bytes = bytes_counter.load(Ordering::Relaxed);

            // Send progress update based on network bytes
            if network_bytes >= next_progress_threshold {
                next_progress_threshold = network_bytes + progress_step;
                let progress = DownloadProgress::Progress {
                    size: network_bytes,
                    total: if total_size > 0 { Some(total_size) } else { None },
                };
                let _ = tx.send(Ok(progress));
            }
        }

        // Flush and close file
        file.flush()
            .await
            .map_err(|e| format!("Failed to flush file: {}", e))?;

        Ok(())
    }
    .await;

    // If download failed, clean up temp file and return error
    if let Err(e) = download_result {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(e);
    }

    // Download succeeded - move temp file to final location
    tokio::fs::rename(&temp_path, &file_path)
        .await
        .map_err(|e| {
            // Try to clean up temp file on rename failure
            let _ = std::fs::remove_file(&temp_path);
            format!("Failed to move downloaded file to final location: {}", e)
        })?;

    let final_path = file_path.display().to_string();
    info!("Download completed: {}", final_path);

    Ok(final_path)
}

pub async fn do_proxy(
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
        build_http_request(&client, method.clone(), url, &headers, body_bytes.clone(), custom_host)
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

    // Unwrap the response or return error with appropriate header
    let response = match response {
        Ok(resp) => resp,
        Err(e) => {
            // Check if this is a certificate error and log detailed information
            let error_debug = format!("{:?}", e);
            let error_display = format!("{}", e);

            // Detect specific error types
            let error_type = if error_debug.contains("NotValidForName") {
                "not-valid-for-name"
            } else if error_debug.contains("InvalidCertificate") {
                "certificate-error"
            } else if error_debug.contains("ConnectionRefused") {
                "bad-connection"
            } else {
                "general"
            };

            error!("Proxy request failed ({}): {}", error_type, error_display);
            debug!("Detailed error: {}", error_debug);

            // Log source chain to see the underlying TLS error
            if let Some(source) = e.source() {
                debug!("Error source: {:?}", source);
                let mut current = source;
                while let Some(next_source) = current.source() {
                    debug!("  Caused by: {:?}", next_source);
                    current = next_source;
                }
            }

            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header("X-Tidewave-Error", error_type)
                .body(Body::empty())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

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

pub async fn proxy_handler(
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

pub async fn client_proxy_handler(
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
