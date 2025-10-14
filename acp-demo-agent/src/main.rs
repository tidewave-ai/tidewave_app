use agent_client_protocol::{self as acp, Client};
use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{error, info};

mod agent;
mod mcp_manager;

use agent::DemoAgent;
use mcp_manager::McpManager;

#[derive(Parser, Debug)]
#[command(name = "acp-demo-agent")]
#[command(about = "A demo ACP agent with real MCP server integration")]
struct Args {
    /// Agent mode: rapid (no delays) or normal (realistic timing)
    #[arg(long, default_value = "normal")]
    mode: String,

    /// Require authentication for session creation
    #[arg(long, default_value_t = false)]
    require_auth: bool,

    /// Disable session loading capability (enabled by default)
    #[arg(long, default_value_t = false)]
    disable_load_session: bool,

    /// Enable debug logging
    #[arg(long, default_value_t = false)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging - MUST use stderr since stdout is used for ACP protocol
    let log_level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(format!("{},rmcp=warn", log_level))
        .init();

    info!("Starting ACP Demo Agent");
    info!("Mode: {}", args.mode);
    info!("Require Auth: {}", args.require_auth);
    info!("Load Session Enabled: {}", !args.disable_load_session);

    // Initialize MCP manager (servers will be connected from session requests)
    let mcp_manager = Some(McpManager::new());

    // Set up stdio streams
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let stdin_compat = stdin.compat();
    let stdout_compat = stdout.compat_write();

    info!("Creating ACP agent-side connection...");

    // Create ACP connection
    let local_set = tokio::task::LocalSet::new();
    let result = local_set
        .run_until(async move {
            // Create channel for session notifications
            let (session_tx, mut session_rx) = mpsc::unbounded_channel();

            // Create channel for permission requests
            let (permission_tx, mut permission_rx) =
                mpsc::unbounded_channel::<agent::PermissionRequest>();

            // Create the demo agent
            let agent = DemoAgent::new(
                mcp_manager,
                args.mode == "rapid",
                args.require_auth,
                !args.disable_load_session,
                session_tx,
                permission_tx,
            );

            let (connection, io_task) =
                acp::AgentSideConnection::new(agent, stdout_compat, stdin_compat, |fut| {
                    tokio::task::spawn_local(fut);
                });

            info!("ACP connection established, running I/O task...");

            // Create an Arc wrapper for the connection to share between tasks
            let connection = Arc::new(connection);
            let session_conn = connection.clone();
            let perm_conn = connection.clone();

            // Background task to handle permission requests
            tokio::task::spawn_local(async move {
                while let Some(req) = permission_rx.recv().await {
                    let result = perm_conn.request_permission(req.request).await;
                    req.response_tx.send(result).ok();
                }
            });

            // Background task to handle session notifications
            tokio::task::spawn_local(async move {
                while let Some((notification, tx)) = session_rx.recv().await {
                    let result = session_conn.session_notification(notification).await;
                    if let Err(e) = result {
                        tracing::error!("Failed to send session notification: {}", e);
                        break;
                    }
                    tx.send(()).ok();
                }
            });

            // Run the I/O task
            io_task.await
        })
        .await;

    // Handle any errors from the I/O task
    if let Err(e) = result {
        error!("ACP Demo Agent encountered an error: {}", e);
        return Err(e.into());
    }

    info!("ACP Demo Agent shut down");
    Ok(())
}
