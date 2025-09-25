use agent_client_protocol::McpServer;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use rmcp::{
    model::{CallToolRequestParam, ClientCapabilities, ClientInfo, Implementation, Tool},
    service::RunningService,
    transport::StreamableHttpClientTransport,
    RoleClient, ServiceExt,
};
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, error, info};

/// Manages connections to MCP servers using HTTP transport
#[derive(Clone)]
pub struct McpManager {
    /// Active MCP client connections by server name
    clients: Arc<DashMap<String, RunningService<RoleClient, ClientInfo>>>,
    /// Cached tools by server name
    tool_cache: Arc<DashMap<String, Vec<Tool>>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
            tool_cache: Arc::new(DashMap::new()),
        }
    }

    /// Connect to MCP servers from ACP session configuration
    pub async fn connect_servers(&self, servers: Vec<McpServer>) -> Result<()> {
        info!("Connecting to {} MCP servers", servers.len());

        for server in servers {
            let server_name = match &server {
                McpServer::Stdio { name, .. } => name,
                McpServer::Http { name, .. } => name,
                McpServer::Sse { name, .. } => name,
            };

            if let Err(e) = self.connect_server(&server).await {
                error!("Failed to connect to MCP server '{}': {}", server_name, e);
                // Continue with other servers instead of failing completely
            }
        }

        Ok(())
    }

    /// Connect to a specific MCP server
    async fn connect_server(&self, server: &McpServer) -> Result<()> {
        let server_name = match server {
            McpServer::Stdio { name, .. } => name,
            McpServer::Http { name, .. } => name,
            McpServer::Sse { name, .. } => name,
        };
        info!("Connecting to MCP server: {}", server_name);

        let client = match server {
            McpServer::Http { name, url, headers } => {
                self.connect_http_server(url, headers, name).await?
            }
            McpServer::Stdio { name, .. } => {
                return Err(anyhow!(
                    "Stdio transport not implemented for server '{}'",
                    name
                ));
            }
            McpServer::Sse { name, .. } => {
                return Err(anyhow!(
                    "SSE transport not yet implemented for server '{}'",
                    name
                ));
            }
        };

        // Test connection by listing tools and cache them
        match client.list_tools(Default::default()).await {
            Ok(tools) => {
                info!(
                    "Successfully connected to MCP server '{}' with {} tools",
                    server_name,
                    tools.tools.len()
                );
                debug!(
                    "Available tools: {:?}",
                    tools.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
                );

                // Cache the tools for this server
                self.tool_cache.insert(server_name.clone(), tools.tools);
            }
            Err(e) => {
                error!("Failed to list tools from server '{}': {}", server_name, e);
                return Err(anyhow!(
                    "Connection test failed for server '{}': {}",
                    server_name,
                    e
                ));
            }
        }

        // Store the client
        self.clients.insert(server_name.clone(), client);
        Ok(())
    }

    /// Connect to an HTTP MCP server
    async fn connect_http_server(
        &self,
        url: &str,
        _headers: &[agent_client_protocol::HttpHeader],
        server_name: &str,
    ) -> Result<RunningService<RoleClient, ClientInfo>> {
        info!("Connecting to HTTP MCP server '{}' at {}", server_name, url);

        // Create HTTP transport using rmcp StreamableHttpClientTransport
        let transport = StreamableHttpClientTransport::from_uri(url);

        // Create client info using the pattern from the working example
        let client_info = ClientInfo {
            protocol_version: Default::default(),
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "acp-demo-agent".to_string(),
                title: None,
                version: "0.1.0".to_string(),
                website_url: None,
                icons: None,
            },
        };

        // Create client using the working pattern
        let client = client_info.serve(transport).await?;

        info!("MCP server '{}' connected successfully", server_name);

        Ok(client)
    }

    /// Call a tool on any connected MCP server that has it
    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<String> {
        debug!(
            "Attempting to call tool '{}' with args: {:?}",
            tool_name, args
        );

        // Try to find the tool in cached tools
        for cache_entry in self.tool_cache.iter() {
            let (server_name, tools) = (cache_entry.key(), cache_entry.value());

            // Check if this server has the tool in cache
            if tools.iter().any(|tool| tool.name == tool_name) {
                info!("Found tool '{}' on server '{}'", tool_name, server_name);

                // Get the client for this server
                if let Some(client_entry) = self.clients.get(server_name) {
                    let client = client_entry.value();

                    // Call the tool - convert Value to Map if it's an object
                    let arguments = if let Value::Object(map) = args {
                        Some(map)
                    } else {
                        None
                    };

                    let result = client
                        .call_tool(CallToolRequestParam {
                            name: tool_name.to_string().into(),
                            arguments,
                        })
                        .await?;

                    // Check if the result content contains text and return only the text
                    for content in &result.content {
                        if let Some(text_content) = content.as_text() {
                            return Ok(text_content.text.clone());
                        }
                    }

                    // If no text content found, return the full content as a pretty-printed JSON string
                    let content_json = serde_json::to_string_pretty(&result.content)?;
                    return Ok(content_json);
                } else {
                    error!("Server '{}' found in cache but not in clients", server_name);
                    continue;
                }
            }
        }

        Err(anyhow!(
            "Tool '{}' not found on any connected MCP server",
            tool_name
        ))
    }
}
