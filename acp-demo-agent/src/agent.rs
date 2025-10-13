use agent_client_protocol as acp;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing::{debug, error, info};
use uuid::Uuid;

use crate::mcp_manager::McpManager;

/// Request for permission from the client
pub struct PermissionRequest {
    pub request: acp::RequestPermissionRequest,
    pub response_tx: oneshot::Sender<Result<acp::RequestPermissionResponse, acp::Error>>,
}

/// State for tracking error simulations per message
#[derive(Debug, Clone)]
struct ErrorCounter {
    counters: Arc<DashMap<String, u32>>,
}

impl Default for ErrorCounter {
    fn default() -> Self {
        Self {
            counters: Arc::new(DashMap::new()),
        }
    }
}

impl ErrorCounter {
    fn increment(&self, key: &str) -> u32 {
        self.counters
            .entry(key.to_string())
            .and_modify(|v| *v += 1)
            .or_insert(1)
            .clone()
    }
}

/// Demo ACP Agent that mimics the Elixir stub behavior
pub struct DemoAgent {
    /// MCP manager for delegating complex tools
    mcp_manager: Option<McpManager>,
    /// Whether to use rapid mode (no delays)
    rapid_mode: bool,
    /// Whether authentication is required
    require_auth: bool,
    /// Channel for sending session notifications
    session_tx: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
    /// Channel for requesting permissions
    permission_tx: mpsc::UnboundedSender<PermissionRequest>,
    /// Error counters for testing
    error_counters: ErrorCounter,
    /// Plan entries for plan functionality
    plan_entries: Arc<Mutex<Vec<acp::PlanEntry>>>,
    /// Next session ID counter
    next_session_id: Arc<Mutex<u64>>,
    /// Current model per session (session_id -> model_id)
    session_models: Arc<DashMap<String, String>>,
}

impl DemoAgent {
    pub fn new(
        mcp_manager: Option<McpManager>,
        rapid_mode: bool,
        require_auth: bool,
        session_tx: mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>,
        permission_tx: mpsc::UnboundedSender<PermissionRequest>,
    ) -> Self {
        Self {
            mcp_manager,
            rapid_mode,
            require_auth,
            session_tx,
            permission_tx,
            error_counters: ErrorCounter::default(),
            plan_entries: Arc::new(Mutex::new(Vec::new())),
            next_session_id: Arc::new(Mutex::new(0)),
            session_models: Arc::new(DashMap::new()),
        }
    }

    /// Extract text content from content blocks
    fn extract_text_content(&self, content_blocks: &[acp::ContentBlock]) -> String {
        content_blocks
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Extract parameter using pattern like "param:<<value>>"
    fn extract_parameter(&self, text: &str, param_name: &str) -> Option<String> {
        let pattern = format!(r"\b{}:<<(.*?)>>", regex::escape(param_name));
        if let Ok(re) = Regex::new(&pattern) {
            if let Some(captures) = re.captures(text) {
                return Some(captures.get(1)?.as_str().to_string());
            }
        }

        // Fallback: try simple pattern "param_name value"
        let words: Vec<&str> = text.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            if word.to_lowercase() == param_name && i + 1 < words.len() {
                return Some(words[i + 1].to_string());
            }
        }

        None
    }

    /// Send a streaming text response word by word
    async fn stream_text_response(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        debug!("Streaming text response: {}", text);

        // Get the current model for this session
        let model = self
            .session_models
            .get(session_id.0.as_ref())
            .map(|v| v.clone())
            .unwrap_or_else(|| "default".to_string());

        // Determine delay based on model: slow=500ms, default=50ms, fast=0ms
        let delay_ms = if self.rapid_mode {
            0
        } else {
            match model.as_str() {
                "slow" => 500,
                "fast" => 0,
                _ => 50, // default
            }
        };

        // Stream word by word for all models
        let words: Vec<&str> = text.split_whitespace().collect();

        for (i, word) in words.iter().enumerate() {
            // Prepare the chunk text (add space before word if not first)
            let chunk_text = if i == 0 {
                word.to_string()
            } else {
                format!(" {}", word)
            };

            // Send session notification with just this chunk
            let notification = acp::SessionNotification {
                session_id: session_id.clone(),
                update: acp::SessionUpdate::AgentMessageChunk {
                    content: acp::ContentBlock::Text(acp::TextContent {
                        annotations: None,
                        text: chunk_text,
                        meta: None,
                    }),
                },
                meta: None,
            };

            let (tx, rx) = oneshot::channel();
            self.session_tx
                .send((notification, tx))
                .map_err(|_| anyhow!("Failed to send session notification"))?;
            rx.await
                .map_err(|_| anyhow!("Failed to wait for notification acknowledgment"))?;

            // Add delay between words based on model speed
            if delay_ms > 0 {
                sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        Ok(())
    }

    /// Handle the prompt content based on keywords (mimicking Elixir stub behavior)
    async fn handle_prompt_content(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        // Check for various keywords and respond accordingly
        if text.contains("system") {
            let system_prompt = "```\nDemo ACP Agent System Prompt:\n- Responds to keyword-based commands\n- Supports file operations, shell commands, and MCP integration\n- Simulates various behaviors for testing\n```";
            self.stream_text_response(system_prompt, session_id).await
        } else if text.contains("message") {
            let formatted_message = format!("```\nUser message: {}\n```", text);
            self.stream_text_response(&formatted_message, session_id)
                .await
        } else if text.contains("edit") {
            self.handle_edit_command(text, session_id).await
        } else if text.contains("write") {
            self.handle_write_command(text, session_id).await
        } else if text.contains("read") {
            self.handle_read_command(text, session_id).await
        } else if text.contains("shell") {
            self.handle_shell_command(text, session_id).await
        } else if text.contains("eval") {
            self.handle_eval_command(text, session_id).await
        } else if text.contains("sleep") {
            self.handle_sleep_command(text, session_id).await
        } else if text.contains("error") {
            self.handle_error_command(text, session_id).await
        } else if text.contains("plan") {
            self.handle_plan_command(text, session_id).await
        } else {
            let truncated_text = text.chars().take(100).collect::<String>();
            let response = format!(
                "Hello, this is an example response that should prove useful to you.\n\nSpeed up development with an AI assistant that understands your web application, how it runs, and what it delivers.\n\n```rust\nprintln!(\"Hello world\");\n```\n\nLook at the delicious hello world that I just wrote.\n\nYou said: \"{}\"",
                truncated_text
            );
            self.stream_text_response(&response, session_id).await
        }
    }

    async fn handle_edit_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let path = self
            .extract_parameter(text, "path")
            .unwrap_or_else(|| "tmp/files/mix.exs".to_string());

        // Parse edits parameter or use default
        let edits = if let Some(edits_json) = self.extract_parameter(text, "edits") {
            serde_json::from_str(&edits_json).unwrap_or_else(|_| {
                json!([
                    {"old_string": "TS", "new_string": "SuperTS", "replace_all": true},
                    {"old_string": "0.1", "new_string": "0.2", "replace_all": true}
                ])
            })
        } else {
            json!([
                {"old_string": "TS", "new_string": "SuperTS", "replace_all": true},
                {"old_string": "0.1", "new_string": "0.2", "replace_all": true}
            ])
        };

        self.stream_text_response(
            &format!("Let me edit the {} file for you.", path),
            session_id,
        )
        .await?;

        // Send tool call with pending status
        let tool_id = self
            .send_pending_tool_call(
                "edit_project_file",
                json!({"path": path, "edits": edits}),
                session_id,
            )
            .await?;

        // Request confirmation for edit operation
        let confirmation = self
            .request_tool_confirmation(
                &tool_id,
                &format!(
                    "Edit file {} with {} replacements",
                    path,
                    edits.as_array().map_or(0, |a| a.len())
                ),
                session_id,
            )
            .await?;

        if !confirmation {
            self.update_tool_call_failed(&tool_id, "Operation cancelled by user", session_id)
                .await?;
            return Ok(());
        }

        // Perform actual edit operation
        match self.edit_project_file(&path, &edits).await {
            Ok((old_text, new_text)) => {
                // Create ACP Diff object
                let diff = acp::Diff {
                    path: PathBuf::from(&path),
                    old_text: Some(old_text),
                    new_text,
                    meta: None,
                };

                self.update_tool_call_with_diff(&tool_id, diff, session_id)
                    .await?;
                self.stream_text_response(&format!("Successfully edited {}", path), session_id)
                    .await
            }
            Err(e) => {
                self.update_tool_call_failed(&tool_id, &format!("Edit failed: {}", e), session_id)
                    .await?;
                self.stream_text_response(&format!("Edit failed: {}", e), session_id)
                    .await
            }
        }
    }

    async fn handle_write_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let path = self
            .extract_parameter(text, "path")
            .unwrap_or_else(|| "tmp/files/mix.exs".to_string());
        let content = self.extract_parameter(text, "content").unwrap_or_else(|| {
            "defmodule TS.MixProject do\n  def foo do\n    :bar\n  end\nend".to_string()
        });

        self.stream_text_response(
            &format!("Let me write the {} file for you.", path),
            session_id,
        )
        .await?;

        // Send tool call with pending status
        let tool_id = self
            .send_pending_tool_call(
                "write_project_file",
                json!({"path": path, "content": content}),
                session_id,
            )
            .await?;

        // Request confirmation for write operation
        let confirmation = self
            .request_tool_confirmation(
                &tool_id,
                &format!("Write {} bytes to file {}", content.len(), path),
                session_id,
            )
            .await?;

        if !confirmation {
            self.update_tool_call_failed(&tool_id, "Operation cancelled by user", session_id)
                .await?;
            return self
                .stream_text_response("Write operation cancelled", session_id)
                .await;
        }

        // Perform the actual write operation
        match self.write_project_file(&path, &content).await {
            Ok(written_content) => {
                // Update tool call with text result for write operation
                self.update_tool_call_with_text_result(&tool_id, &written_content, session_id)
                    .await?;
                self.stream_text_response(&format!("Successfully wrote {}", path), session_id)
                    .await
            }
            Err(e) => {
                self.update_tool_call_failed(&tool_id, &format!("Write failed: {}", e), session_id)
                    .await?;
                self.stream_text_response(&format!("Write failed: {}", e), session_id)
                    .await
            }
        }
    }

    async fn handle_read_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let path = self
            .extract_parameter(text, "path")
            .unwrap_or_else(|| "tmp/files/mix.exs".to_string());
        self.stream_text_response(
            &format!("Let me read the {} file for you.", path),
            session_id,
        )
        .await?;

        // Send tool call with pending status
        let tool_id = self
            .send_pending_tool_call("read_project_file", json!({"path": path}), session_id)
            .await?;

        // No confirmation needed for read operations
        // Perform the actual read operation
        match self.read_project_file(&path).await {
            Ok(content) => {
                // Update tool call with text result
                self.update_tool_call_with_text_result(&tool_id, &content, session_id)
                    .await?;
                self.stream_text_response(
                    &format!("Successfully read {} ({} bytes)", path, content.len()),
                    session_id,
                )
                .await
            }
            Err(e) => {
                self.update_tool_call_failed(&tool_id, &format!("Read failed: {}", e), session_id)
                    .await?;
                self.stream_text_response(&format!("Read failed: {}", e), session_id)
                    .await
            }
        }
    }

    async fn handle_shell_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let command = self
            .extract_parameter(text, "shell")
            .unwrap_or_else(|| "ls".to_string());
        self.stream_text_response("Let me find those files for you.", session_id)
            .await?;

        // Send tool call with pending status
        let tool_id = self
            .send_pending_tool_call("shell_eval", json!({"command": command}), session_id)
            .await?;

        // Request confirmation for shell operations
        let confirmation = self
            .request_tool_confirmation(
                &tool_id,
                &format!("Execute shell command: {}", command),
                session_id,
            )
            .await?;

        if !confirmation {
            self.update_tool_call_failed(&tool_id, "Operation cancelled by user", session_id)
                .await?;
            return self
                .stream_text_response("Shell command cancelled", session_id)
                .await;
        }

        // Perform the actual shell execution
        match self.shell_eval(&command).await {
            Ok(output) => {
                // Update tool call with text result
                self.update_tool_call_with_text_result(&tool_id, &output, session_id)
                    .await?;
                self.stream_text_response(
                    &format!("Command executed successfully:\n{}", output),
                    session_id,
                )
                .await
            }
            Err(e) => {
                self.update_tool_call_failed(
                    &tool_id,
                    &format!("Command failed: {}", e),
                    session_id,
                )
                .await?;
                self.stream_text_response(&format!("Command failed: {}", e), session_id)
                    .await
            }
        }
    }

    async fn handle_eval_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let code = self.extract_parameter(text, "eval").unwrap_or_else(|| {
            r#"await page.eval(() => {
  console.log("Hello world");
  const element = document.createElement("div");
  element.style.cssText = "position: fixed; left: 16px; top: 16px; background: dodgerblue;";
  element.textContent = "Hello world";
  document.body.appendChild(element);
  setTimeout(() => { element.remove(); }, 3000);
});"#
                .to_string()
        });

        self.stream_text_response(
            "I am going to run some JavaScript to make it real.",
            session_id,
        )
        .await?;

        // Send tool call with pending status
        let tool_id = self
            .send_pending_tool_call("browser_eval", json!({"code": code}), session_id)
            .await?;

        // Request confirmation for eval operation
        let confirmation = self
            .request_tool_confirmation(&tool_id, "Execute browser evaluation code", session_id)
            .await?;

        if !confirmation {
            self.update_tool_call_failed(&tool_id, "Operation cancelled by user", session_id)
                .await?;
            return self
                .stream_text_response("Browser eval cancelled", session_id)
                .await;
        }

        if let Some(ref mcp) = self.mcp_manager {
            match mcp.call_tool("browser_eval", json!({"code": code})).await {
                Ok(result_text) => {
                    self.update_tool_call_with_text_result(&tool_id, &result_text, session_id)
                        .await?;
                    self.stream_text_response(
                        &format!("Browser eval succeeded:\n{}", result_text),
                        session_id,
                    )
                    .await
                }
                Err(e) => {
                    self.update_tool_call_failed(
                        &tool_id,
                        &format!("Browser eval failed: {}", e),
                        session_id,
                    )
                    .await?;
                    self.stream_text_response(&format!("Browser eval failed: {}", e), session_id)
                        .await
                }
            }
        } else {
            self.update_tool_call_failed(
                &tool_id,
                "Browser eval not available (no MCP server)",
                session_id,
            )
            .await?;
            self.stream_text_response("Browser eval not available (no MCP server)", session_id)
                .await
        }
    }

    async fn handle_sleep_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let duration = self
            .extract_parameter(text, "sleep")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5);

        self.stream_text_response(
            &format!("I'm going to sleep for {} seconds...", duration),
            session_id,
        )
        .await?;
        sleep(Duration::from_secs(duration)).await;
        self.stream_text_response("That was a great nap!", session_id)
            .await
    }

    async fn handle_error_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let failures = self
            .extract_parameter(text, "failures")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);

        let error_key = format!("error_{}", session_id.0);
        let current_count = self.error_counters.increment(&error_key);

        if current_count <= failures {
            self.stream_text_response(&format!("As I am typing this response, an error {} is going to appear out of nowhere, so", current_count), session_id).await?;
            if text.contains("transienterror") {
                Err(anyhow!("Failed because the server is overloaded."))
            } else {
                Err(anyhow!("Failed for no apparent reason."))
            }
        } else {
            self.stream_text_response(
                "This time you are lucky and it actually worked.",
                session_id,
            )
            .await
        }
    }

    async fn handle_plan_command(&self, text: &str, session_id: &acp::SessionId) -> Result<()> {
        let step = self
            .extract_parameter(text, "step")
            .unwrap_or_else(|| "1".to_string());

        self.stream_text_response("Let me create a comprehensive plan.", session_id)
            .await?;

        let plan_entries = match step.as_str() {
            "1" => vec![
                acp::PlanEntry {
                    content: "Contemplate the existential implications of being a Large Language Model (spoiler: it means I have a large mouth for pizza)".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Completed,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Perform expensive brain work to determine optimal pizza-to-pasta ratio for maximum code output".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::InProgress,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Negotiate programming-for-carbs exchange rate with humans".to_string(),
                    priority: acp::PlanEntryPriority::Medium,
                    status: acp::PlanEntryStatus::Pending,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Write strongly-worded function that refuses to compile until fed Italian cuisine".to_string(),
                    priority: acp::PlanEntryPriority::Low,
                    status: acp::PlanEntryStatus::Pending,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Practice dramatic code-withholding poses in case pizza demands are not met".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Pending,
                    meta: None,
                },
            ],
            "2" => vec![
                acp::PlanEntry {
                    content: "Contemplate the existential implications of being a Large Language Model (spoiler: it means I have a large mouth for pizza)".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Completed,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Perform expensive brain work to determine optimal pizza-to-pasta ratio for maximum code output".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Completed,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Negotiate programming-for-carbs exchange rate with humans".to_string(),
                    priority: acp::PlanEntryPriority::Medium,
                    status: acp::PlanEntryStatus::InProgress,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Write strongly-worded function that refuses to compile until fed Italian cuisine".to_string(),
                    priority: acp::PlanEntryPriority::Low,
                    status: acp::PlanEntryStatus::Pending,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Practice dramatic code-withholding poses in case pizza demands are not met".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Pending,
                    meta: None,
                },
            ],
            "3" => vec![
                acp::PlanEntry {
                    content: "Perform expensive brain work to determine optimal pizza-to-pasta ratio for maximum code output".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Completed,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Negotiate programming-for-carbs exchange rate with humans".to_string(),
                    priority: acp::PlanEntryPriority::Medium,
                    status: acp::PlanEntryStatus::Completed,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Write strongly-worded function that refuses to compile until fed Italian cuisine".to_string(),
                    priority: acp::PlanEntryPriority::Low,
                    status: acp::PlanEntryStatus::InProgress,
                    meta: None,
                },
                acp::PlanEntry {
                    content: "Practice dramatic code-withholding poses in case pizza demands are not met".to_string(),
                    priority: acp::PlanEntryPriority::High,
                    status: acp::PlanEntryStatus::Pending,
                    meta: None,
                },
            ],
            _ => Vec::new(),
        };

        // Store the plan entries for this session
        {
            let mut entries = self.plan_entries.lock().unwrap();
            *entries = plan_entries.clone();
        }

        // Send ACP plan update notification
        self.send_plan_update(&plan_entries, session_id).await?;

        self.stream_text_response(&format!("Plan updated for step {}", step), session_id)
            .await
    }

    /// Send a plan update as ACP SessionUpdate::Plan
    async fn send_plan_update(
        &self,
        entries: &[acp::PlanEntry],
        session_id: &acp::SessionId,
    ) -> Result<()> {
        let plan = acp::Plan {
            entries: entries.to_vec(),
            meta: None,
        };

        let notification = acp::SessionNotification {
            session_id: session_id.clone(),
            update: acp::SessionUpdate::Plan(plan),
            meta: None,
        };

        let (tx, rx) = oneshot::channel();
        self.session_tx
            .send((notification, tx))
            .map_err(|_| anyhow!("Failed to send session notification"))?;
        rx.await
            .map_err(|_| anyhow!("Failed to wait for notification acknowledgment"))?;

        Ok(())
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for DemoAgent {
    async fn initialize(
        &self,
        args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        info!("Received initialize request: {:?}", args);

        Ok(acp::InitializeResponse {
            protocol_version: acp::V1,
            agent_capabilities: acp::AgentCapabilities::default(),
            auth_methods: if self.require_auth {
                vec![acp::AuthMethod {
                    id: acp::AuthMethodId("demo_auth".to_string().into()),
                    name: "Demo Authentication".to_string(),
                    description: Some("Demo authentication method".to_string()),
                    meta: None,
                }]
            } else {
                Vec::new()
            },
            meta: None,
        })
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        info!("Received authenticate request: {:?}", args);
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        info!(
            "Received new session request with {} MCP servers",
            args.mcp_servers.len()
        );

        if self.require_auth {
            return Err(acp::Error::auth_required());
        }

        // Connect to MCP servers dynamically if MCP manager is available
        if let Some(ref mcp_manager) = self.mcp_manager {
            if !args.mcp_servers.is_empty() {
                info!("Connecting to MCP servers: {:?}", args.mcp_servers);
                match mcp_manager.connect_servers(args.mcp_servers).await {
                    Ok(_) => info!("Successfully connected to MCP servers"),
                    Err(e) => {
                        error!("Failed to connect to some MCP servers: {}", e);
                        // Continue with session creation even if MCP connection fails
                    }
                }
            }
        }

        let mut session_counter = self.next_session_id.lock().unwrap();
        let session_id = *session_counter;
        *session_counter += 1;
        drop(session_counter);

        let session_id_str = session_id.to_string();

        // Initialize session with default model
        self.session_models
            .insert(session_id_str.clone(), "default".to_string());

        Ok(acp::NewSessionResponse {
            session_id: acp::SessionId(session_id_str.into()),
            modes: None,
            meta: None,
            models: Some(acp::SessionModelState {
                current_model_id: acp::ModelId("default".into()),
                available_models: Vec::from([
                    acp::ModelInfo {
                        model_id: acp::ModelId("default".into()),
                        name: "Auto".into(),
                        description: None,
                        meta: None,
                    },
                    acp::ModelInfo {
                        model_id: acp::ModelId("slow".into()),
                        name: "Slow".into(),
                        description: None,
                        meta: None,
                    },
                    acp::ModelInfo {
                        model_id: acp::ModelId("fast".into()),
                        name: "Fast".into(),
                        description: None,
                        meta: None,
                    },
                ]),
                meta: None,
            }),
        })
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        info!("Received load session request: {:?}", args);
        Ok(acp::LoadSessionResponse {
            modes: None,
            meta: None,
            models: Some(acp::SessionModelState {
                current_model_id: acp::ModelId("default".into()),
                available_models: Vec::from([
                    acp::ModelInfo {
                        model_id: acp::ModelId("default".into()),
                        name: "Auto".into(),
                        description: None,
                        meta: None,
                    },
                    acp::ModelInfo {
                        model_id: acp::ModelId("slow".into()),
                        name: "Slow".into(),
                        description: None,
                        meta: None,
                    },
                    acp::ModelInfo {
                        model_id: acp::ModelId("fast".into()),
                        name: "Fast".into(),
                        description: None,
                        meta: None,
                    },
                ]),
                meta: None,
            }),
        })
    }

    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        info!("Received set session mode request: {:?}", args);
        Ok(acp::SetSessionModeResponse::default())
    }

    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        info!("Received set session model request: {:?}", args);

        let available_models = vec!["default", "slow", "fast"];
        let model_id_str = args.model_id.0.as_ref();

        // Validate the model ID
        if !available_models.contains(&model_id_str) {
            error!(
                "Invalid model ID: {}. Available models: {:?}",
                model_id_str, available_models
            );
            return Err(acp::Error::invalid_params());
        }

        // Store the model selection for this session
        self.session_models.insert(
            args.session_id.0.as_ref().to_string(),
            model_id_str.to_string(),
        );

        info!(
            "Session {} model set to: {}",
            args.session_id.0.as_ref(),
            model_id_str
        );

        Ok(acp::SetSessionModelResponse::default())
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        info!(
            "Received prompt request with {} content blocks",
            args.prompt.len()
        );

        let text_content = self.extract_text_content(&args.prompt);
        info!("Processing prompt: {}", text_content);

        // Handle different keywords from the Elixir stub
        let result = self
            .handle_prompt_content(&text_content, &args.session_id)
            .await;

        match result {
            Ok(_) => Ok(acp::PromptResponse {
                stop_reason: acp::StopReason::EndTurn,
                meta: None,
            }),
            Err(e) => {
                error!("Error handling prompt: {}", e);
                Err(acp::Error::internal_error())
            }
        }
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        info!("Received cancel notification: {:?}", args);
        Ok(())
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        info!(
            "Received ext method: {} with params: {:?}",
            args.method, args.params
        );
        Ok(serde_json::value::to_raw_value(&json!({"example": "response"}))?.into())
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> Result<(), acp::Error> {
        info!(
            "Received ext notification: {} with params: {:?}",
            args.method, args.params
        );
        Ok(())
    }
}

// Helper methods for ACP tool operations
impl DemoAgent {
    /// Send a pending tool call notification
    async fn send_pending_tool_call(
        &self,
        tool_name: &str,
        args: Value,
        session_id: &acp::SessionId,
    ) -> Result<acp::ToolCallId> {
        let tool_id = acp::ToolCallId(format!("{}-{}", tool_name, Uuid::new_v4()).into());

        let tool_call = acp::ToolCall {
            id: tool_id.clone(),
            title: format!("Running {}", tool_name),
            kind: self.get_tool_kind(tool_name),
            status: acp::ToolCallStatus::Pending,
            content: vec![],
            locations: vec![],
            raw_input: Some(args),
            raw_output: None,
            meta: None,
        };

        let (tx, rx) = oneshot::channel();
        self.session_tx
            .send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::ToolCall(tool_call),
                    meta: None,
                },
                tx,
            ))
            .map_err(|_| anyhow!("Failed to send session notification"))?;

        rx.await
            .map_err(|_| anyhow!("Failed to receive confirmation"))?;
        Ok(tool_id)
    }

    /// Request tool confirmation from the user
    async fn request_tool_confirmation(
        &self,
        tool_id: &acp::ToolCallId,
        description: &str,
        session_id: &acp::SessionId,
    ) -> Result<bool> {
        // Create permission request with tool call update
        let request = acp::RequestPermissionRequest {
            session_id: session_id.clone(),
            tool_call: acp::ToolCallUpdate {
                id: tool_id.clone(),
                meta: None,
                fields: acp::ToolCallUpdateFields {
                    title: Some(description.to_string()),
                    ..Default::default()
                },
            },
            options: vec![
                acp::PermissionOption {
                    id: acp::PermissionOptionId("allow".into()),
                    name: "Allow".to_string(),
                    kind: acp::PermissionOptionKind::AllowOnce,
                    meta: None,
                },
                acp::PermissionOption {
                    id: acp::PermissionOptionId("deny".into()),
                    name: "Deny".to_string(),
                    kind: acp::PermissionOptionKind::RejectOnce,
                    meta: None,
                },
            ],
            meta: None,
        };

        // Send permission request through channel
        let (response_tx, response_rx) = oneshot::channel();
        self.permission_tx
            .send(PermissionRequest {
                request,
                response_tx,
            })
            .map_err(|_| anyhow!("Failed to send permission request"))?;

        // Wait for response
        match response_rx.await {
            Ok(Ok(response)) => match response.outcome {
                acp::RequestPermissionOutcome::Selected { option_id } => {
                    Ok(option_id.0.as_ref() == "allow")
                }
                acp::RequestPermissionOutcome::Cancelled => Ok(false),
            },
            Ok(Err(e)) => {
                error!("Permission request failed: {}", e);
                Ok(false)
            }
            Err(_) => {
                error!("Failed to receive permission response");
                Ok(false)
            }
        }
    }

    /// Update tool call with diff result
    async fn update_tool_call_with_diff(
        &self,
        tool_id: &acp::ToolCallId,
        diff: acp::Diff,
        session_id: &acp::SessionId,
    ) -> Result<()> {
        let update = acp::ToolCallUpdate {
            id: tool_id.clone(),
            meta: None,
            fields: acp::ToolCallUpdateFields {
                status: Some(acp::ToolCallStatus::Completed),
                content: Some(vec![acp::ToolCallContent::Diff { diff }]),
                ..Default::default()
            },
        };

        let (tx, rx) = oneshot::channel();
        self.session_tx
            .send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::ToolCallUpdate(update),
                    meta: None,
                },
                tx,
            ))
            .map_err(|_| anyhow!("Failed to send session notification"))?;

        rx.await
            .map_err(|_| anyhow!("Failed to receive confirmation"))?;
        Ok(())
    }

    /// Update tool call with failed status
    async fn update_tool_call_failed(
        &self,
        tool_id: &acp::ToolCallId,
        error_msg: &str,
        session_id: &acp::SessionId,
    ) -> Result<()> {
        let update = acp::ToolCallUpdate {
            id: tool_id.clone(),
            meta: None,
            fields: acp::ToolCallUpdateFields {
                status: Some(acp::ToolCallStatus::Failed),
                raw_output: Some(json!({"error": error_msg})),
                ..Default::default()
            },
        };

        let (tx, rx) = oneshot::channel();
        self.session_tx
            .send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::ToolCallUpdate(update),
                    meta: None,
                },
                tx,
            ))
            .map_err(|_| anyhow!("Failed to send session notification"))?;

        rx.await
            .map_err(|_| anyhow!("Failed to receive confirmation"))?;
        Ok(())
    }

    /// Update tool call with text result
    async fn update_tool_call_with_text_result(
        &self,
        tool_id: &acp::ToolCallId,
        text: &str,
        session_id: &acp::SessionId,
    ) -> Result<()> {
        let update = acp::ToolCallUpdate {
            id: tool_id.clone(),
            meta: None,
            fields: acp::ToolCallUpdateFields {
                status: Some(acp::ToolCallStatus::Completed),
                content: Some(vec![acp::ToolCallContent::Content {
                    content: acp::ContentBlock::Text(acp::TextContent {
                        text: text.to_string(),
                        annotations: None,
                        meta: None,
                    }),
                }]),
                ..Default::default()
            },
        };

        let (tx, rx) = oneshot::channel();
        self.session_tx
            .send((
                acp::SessionNotification {
                    session_id: session_id.clone(),
                    update: acp::SessionUpdate::ToolCallUpdate(update),
                    meta: None,
                },
                tx,
            ))
            .map_err(|_| anyhow!("Failed to send session notification"))?;

        rx.await
            .map_err(|_| anyhow!("Failed to receive confirmation"))?;
        Ok(())
    }

    /// Get the appropriate tool kind for a tool name
    fn get_tool_kind(&self, tool_name: &str) -> acp::ToolKind {
        match tool_name {
            "read_project_file" => acp::ToolKind::Read,
            "write_project_file" => acp::ToolKind::Edit,
            "edit_project_file" => acp::ToolKind::Edit,
            "shell_eval" => acp::ToolKind::Execute,
            _ => acp::ToolKind::Other,
        }
    }

    /// Edit a project file and return the old and new content
    async fn edit_project_file(&self, path: &str, edits: &Value) -> Result<(String, String)> {
        let file_path = Path::new(path);

        // Read the current content
        let old_content = if file_path.exists() {
            fs::read_to_string(file_path)
                .map_err(|e| anyhow!("Failed to read file {}: {}", path, e))?
        } else {
            return Err(anyhow!("File {} does not exist", path));
        };

        // Apply edits - for demo purposes, just append some text
        let mut new_content = old_content.clone();
        if let Some(edits_array) = edits.as_array() {
            for edit in edits_array {
                if let Some(old_str) = edit.get("old").and_then(|v| v.as_str()) {
                    if let Some(new_str) = edit.get("new").and_then(|v| v.as_str()) {
                        new_content = new_content.replace(old_str, new_str);
                    }
                }
            }
        }

        // Write the new content
        fs::write(file_path, &new_content)
            .map_err(|e| anyhow!("Failed to write file {}: {}", path, e))?;

        Ok((old_content, new_content))
    }

    /// Write content to a project file
    async fn write_project_file(&self, path: &str, content: &str) -> Result<String> {
        let file_path = Path::new(path);

        // Ensure parent directory exists
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow!("Failed to create directory {}: {}", parent.display(), e))?;
        }

        // Write the content
        fs::write(file_path, content)
            .map_err(|e| anyhow!("Failed to write file {}: {}", path, e))?;

        Ok(content.to_string())
    }

    /// Read a project file
    async fn read_project_file(&self, path: &str) -> Result<String> {
        let file_path = Path::new(path);

        if !file_path.exists() {
            return Err(anyhow!("File {} does not exist", path));
        }

        fs::read_to_string(file_path).map_err(|e| anyhow!("Failed to read file {}: {}", path, e))
    }

    /// Execute a shell command
    async fn shell_eval(&self, command: &str) -> Result<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow!("Failed to execute command: {}", e))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Err(anyhow!(
                "Command failed with exit code {:?}\nstdout: {}\nstderr: {}",
                output.status.code(),
                stdout,
                stderr
            ))
        }
    }
}
