//! vim-mcp: an MCP server bridging Claude Code and Vim/Neovim.
//!
//! Two sides:
//! - MCP over stdio (this file), exposing tools and resources to the client.
//! - A Unix-socket server (see `bridge`) that Vim instances connect to.

mod bridge;
mod protocol;
mod registry;

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    AnnotateAble, CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, RawResource,
    ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
    Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServiceExt};
use serde_json::{json, Map, Value};

use registry::Registry;

#[derive(Clone)]
struct VimMcp {
    registry: Arc<Registry>,
}

/// Build a JSON-Schema object from a `serde_json` value.
fn schema(v: Value) -> Arc<Map<String, Value>> {
    match v {
        Value::Object(m) => Arc::new(m),
        _ => Arc::new(Map::new()),
    }
}

impl VimMcp {
    async fn selected(&self) -> Result<String, McpError> {
        self.registry.selected.lock().await.clone().ok_or_else(|| {
            McpError::invalid_request(
                "No Vim instance selected. Use select_vim_instance tool first.",
                None,
            )
        })
    }
}

impl ServerHandler for VimMcp {
    fn get_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new(
                "vim-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Connect Claude Code to Vim/Neovim: query state, execute commands, \
                 search help, record macros.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let empty = || schema(json!({ "type": "object", "properties": {} }));
        let tools = vec![
            Tool::new("list_vim_instances", "List all available Vim instances", empty()),
            Tool::new(
                "select_vim_instance",
                "Select a Vim instance to connect to",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "instance_id": { "type": "string", "description": "The ID of the Vim instance to select" }
                    },
                    "required": ["instance_id"]
                })),
            ),
            Tool::new("get_vim_state", "Get the current state of the selected Vim instance", empty()),
            Tool::new(
                "vim_execute",
                "Execute an Ex command in the selected Vim instance",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The Ex command to execute (e.g., \"w\", \"q\", \"set number\")" }
                    },
                    "required": ["command"]
                })),
            ),
            Tool::new(
                "exit_vim",
                "Exit the selected Vim instance, with handling for unsaved changes",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "Action to take: \"check\" (default), \"save_and_exit\", or \"force_exit\"",
                            "enum": ["check", "save_and_exit", "force_exit"]
                        }
                    }
                })),
            ),
            Tool::new(
                "vim_search_help",
                "Search Vim help documentation for a topic and open the most relevant help section",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The help topic to search for (e.g., \"channel\", \"buffers\")" }
                    },
                    "required": ["query"]
                })),
            ),
            Tool::new(
                "vim_record_macro",
                "Record a Vim macro from a sequence of keystrokes.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "macro_sequence": { "type": "string", "description": "The Vim keystroke sequence to record (e.g., \"0gUwj2j\")" },
                        "register": { "type": "string", "description": "Register to save the macro in (default: \"q\"). Single letter a-z or digit 0-9" },
                        "execute": { "type": "boolean", "description": "Whether to execute the macro immediately after recording (default: true)" }
                    },
                    "required": ["macro_sequence"]
                })),
            ),
        ];
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request.arguments.unwrap_or_default();
        let text = self.dispatch_tool(&request.name, args).await?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut resources = vec![RawResource::new("vim://instances", "Vim Instances").no_annotation()];
        if self.registry.selected.lock().await.is_some() {
            resources.push(RawResource::new("vim://state", "Vim State").no_annotation());
            resources.push(RawResource::new("vim://buffers", "Vim Buffers").no_annotation());
            resources.push(RawResource::new("vim://tabs", "Vim Tabs").no_annotation());
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.as_str();
        let text = self.read_resource_text(uri).await?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri)]))
    }
}

impl VimMcp {
    async fn dispatch_tool(&self, name: &str, args: Map<String, Value>) -> Result<String, McpError> {
        match name {
            "list_vim_instances" => {
                let conns = self.registry.connections.lock().await;
                if conns.is_empty() {
                    return Ok("No Vim instances connected. Open Vim with the vim-mcp plugin loaded, or run :VimMCPReconnect.".to_string());
                }
                let selected = self.registry.selected.lock().await.clone();
                let mut lines = Vec::new();
                for (id, conn) in conns.iter() {
                    lines.push(format!(
                        "- {id} (PID: {})\n  File: {}\n  CWD: {}",
                        conn.info.pid,
                        if conn.info.main_file.is_empty() { "unnamed" } else { &conn.info.main_file },
                        conn.info.cwd
                    ));
                }
                let list = lines.join("\n");
                let n = conns.len();
                if n > 1 && selected.is_none() {
                    Ok(format!("Found {n} Vim instance(s):\n{list}\n\nPlease select an instance using select_vim_instance with instance_id."))
                } else {
                    Ok(format!("Found {n} Vim instance(s):\n{list}"))
                }
            }
            "select_vim_instance" => {
                let id = args
                    .get("instance_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| McpError::invalid_params("instance_id is required", None))?;
                if self.registry.get_connection(id).await.is_none() {
                    let available: Vec<String> =
                        self.registry.connections.lock().await.keys().cloned().collect();
                    let avail = if available.is_empty() { "none".to_string() } else { available.join(", ") };
                    return Err(McpError::invalid_params(
                        format!("Instance '{id}' not found. Available instances: {avail}"),
                        None,
                    ));
                }
                *self.registry.selected.lock().await = Some(id.to_string());
                self.registry.save_preference(id).await;
                let conn = self.registry.get_connection(id).await.unwrap();
                let file = if conn.info.main_file.is_empty() { "unnamed" } else { &conn.info.main_file };
                Ok(format!("Connected to Vim instance: {id}\nFile: {file}\nCWD: {}", conn.info.cwd))
            }
            "get_vim_state" => {
                let id = self.selected().await?;
                let state = self.registry.request_state(&id).await.map_err(internal)?;
                Ok(serde_json::to_string_pretty(&state).unwrap_or_default())
            }
            "vim_execute" => {
                let id = self.selected().await?;
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| McpError::invalid_params("Command parameter is required", None))?;
                bridge::execute_vim_command(&self.registry, &id, command)
                    .await
                    .map_err(|e| internal(format!("Failed to execute command: {e}")))
            }
            "exit_vim" => {
                let id = self.selected().await?;
                let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("check");
                match action {
                    "check" => {
                        let state = self.registry.request_state(&id).await.map_err(internal)?;
                        let modified: Vec<String> = state
                            .get("buffers")
                            .and_then(|b| b.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter(|b| b.get("modified").and_then(|m| m.as_bool()).unwrap_or(false))
                                    .map(|b| {
                                        b.get("name")
                                            .and_then(|n| n.as_str())
                                            .filter(|s| !s.is_empty())
                                            .unwrap_or("[No Name]")
                                            .to_string()
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if modified.is_empty() {
                            bridge::execute_vim_command(&self.registry, &id, "qall").await.map_err(internal)?;
                            Ok("Vim exited successfully (no unsaved changes)".to_string())
                        } else {
                            Ok(format!(
                                "Cannot exit Vim: {} buffer(s) have unsaved changes: {}\n\nOptions:\n- Save manually and run exit_vim again\n- exit_vim action='save_and_exit'\n- exit_vim action='force_exit'",
                                modified.len(),
                                modified.join(", ")
                            ))
                        }
                    }
                    "save_and_exit" => {
                        bridge::execute_vim_command(&self.registry, &id, "wqall").await.map_err(internal)?;
                        Ok("All changes saved and Vim exited successfully".to_string())
                    }
                    "force_exit" => {
                        bridge::execute_vim_command(&self.registry, &id, "qall!").await.map_err(internal)?;
                        Ok("Vim force exited (unsaved changes discarded)".to_string())
                    }
                    other => Err(McpError::invalid_params(
                        format!("Invalid action '{other}'. Use: check, save_and_exit, or force_exit"),
                        None,
                    )),
                }
            }
            "vim_search_help" => {
                let id = self.selected().await?;
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| McpError::invalid_params("Query parameter is required", None))?;
                let result = self
                    .registry
                    .request(&id, "search_help", json!({ "query": query }), 3000)
                    .await
                    .map_err(internal)?;
                if result.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                    let tag = result.get("tag").and_then(|v| v.as_str()).unwrap_or("");
                    let file = result.get("file").and_then(|v| v.as_str()).unwrap_or("");
                    let line = result.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
                    Ok(format!("Found help for '{query}':\n\nTag: {tag}\nFile: {file}\nLine: {line}\n\nHelp window opened in Vim."))
                } else {
                    Ok(format!("No help found for '{query}'. Try a more specific term, :helpgrep, or :help."))
                }
            }
            "vim_record_macro" => {
                let id = self.selected().await?;
                let macro_sequence = args
                    .get("macro_sequence")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| McpError::invalid_params("macro_sequence parameter is required", None))?;
                let register = args.get("register").and_then(|v| v.as_str()).unwrap_or("q");
                let execute = args.get("execute").and_then(|v| v.as_bool()).unwrap_or(true);

                if register.len() != 1 || !register.chars().next().unwrap().is_ascii_alphanumeric() {
                    return Err(McpError::invalid_params(
                        "Register must be a single letter (a-z) or digit (0-9)",
                        None,
                    ));
                }

                let result = self
                    .registry
                    .request(
                        &id,
                        "record_macro",
                        json!({ "macro_sequence": macro_sequence, "register": register, "execute": execute }),
                        2000,
                    )
                    .await
                    .map_err(internal)?;

                if result.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                    let mut msg = format!("Macro recorded in register \"{register}\"");
                    if execute {
                        msg.push_str(" and executed");
                    }
                    msg.push_str(&format!("\nSequence: {macro_sequence}"));
                    if let Some(out) = result.get("output").and_then(|v| v.as_str()) {
                        msg.push('\n');
                        msg.push_str(out);
                    }
                    Ok(msg)
                } else {
                    let err = result.get("error").and_then(|v| v.as_str()).unwrap_or("Unknown error");
                    Ok(format!("Failed to record macro: {err}"))
                }
            }
            other => Err(McpError::invalid_request(format!("Unknown tool: {other}"), None)),
        }
    }

    async fn read_resource_text(&self, uri: &str) -> Result<String, McpError> {
        if uri == "vim://instances" {
            let conns = self.registry.connections.lock().await;
            let mut map = Map::new();
            for (id, conn) in conns.iter() {
                map.insert(
                    id.clone(),
                    json!({
                        "pid": conn.info.pid,
                        "cwd": conn.info.cwd,
                        "main_file": conn.info.main_file,
                        "buffers": conn.info.buffers,
                        "connected": true
                    }),
                );
            }
            return Ok(serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_default());
        }

        let id = self.selected().await?;
        let state = self.registry.request_state(&id).await.map_err(internal)?;
        let value = match uri {
            "vim://buffers" => state.get("buffers").cloned().unwrap_or(json!([])),
            "vim://tabs" => state.get("tabs").cloned().unwrap_or(json!([])),
            "vim://state" => state,
            other => return Err(McpError::invalid_request(format!("Unknown resource: {other}"), None)),
        };
        Ok(serde_json::to_string_pretty(&value).unwrap_or_default())
    }
}

fn internal(msg: impl std::fmt::Display) -> McpError {
    McpError::internal_error(msg.to_string(), None)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Registry::new();

    // Run the Unix-socket server in the background.
    let socket_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        if let Err(e) = bridge::serve(socket_registry).await {
            eprintln!("Unix socket server error: {e}");
        }
    });

    install_cleanup();

    // Serve MCP over stdio.
    let handler = VimMcp { registry };
    let service = handler.serve(rmcp::transport::stdio()).await?;
    eprintln!("Vim MCP Server started");
    service.waiting().await?;
    Ok(())
}

/// On termination signals, remove the socket and scratch files.
fn install_cleanup() {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = hup.recv() => {}
        }
        for path in [registry::SOCKET_PATH, registry::REGISTRY_PATH, registry::PREFERENCE_PATH] {
            let _ = std::fs::remove_file(path);
        }
        std::process::exit(0);
    });
}
