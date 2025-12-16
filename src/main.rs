use anyhow::{Context, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, error, warn, instrument};

mod semantic;
use semantic::SemanticEngine;

// --- Constants for Internal Router Traffic ---
const INTERNAL_INIT_ID: &str = "router-init";
const INTERNAL_LIST_ID: &str = "router-list";

// --- Data Structures ---

#[derive(Debug, Deserialize, Clone)]
struct ServerConfig {
    command: String,
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    servers: std::collections::HashMap<String, ServerConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JsonRpcMessage {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

struct DownstreamServer {
    tx: mpsc::Sender<JsonRpcMessage>,
}

struct RouterState {
    clients: DashMap<String, DownstreamServer>,
    semantic: Arc<SemanticEngine>,
}

// --- Transport Layer ---

async fn spawn_downstream_server(
    server_id: String,
    config: ServerConfig,
    router_tx: mpsc::Sender<(String, JsonRpcMessage)>,
) -> Result<DownstreamServer> {
    info!(server_id = %server_id, "🚀 Spawning server process");

    let mut child = Command::new(&config.command)
       .args(&config.args)
       .stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::inherit()) // Log stderr to console
       .spawn()
       .context(format!("Failed to spawn {}", server_id))?;

    // FIX: We take stdin/stdout ONLY ONCE here
    let mut stdin = child.stdin.take().context("Failed to open stdin")?;
    let stdout = child.stdout.take().context("Failed to open stdout")?;

    // 1. Writer Channel: Router -> Downstream
    let (tx, mut rx) = mpsc::channel::<JsonRpcMessage>(32);
    
    // FIX: Removed duplicate 'let mut stdin = ...' line that was here
    
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                // CRITICAL FIX: If writing fails (Broken Pipe), break the loop!
                if stdin.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        }
        // When loop breaks, the channel is dropped, signaling disconnection
    });

    // 2. Reader Loop: Downstream -> Router
    let server_id_clone = server_id.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&line) {
                let _ = router_tx.send((server_id_clone.clone(), msg)).await;
            } else {
                warn!(server = %server_id_clone, "Failed to parse JSON: {}", line);
            }
        }
        error!(server = %server_id_clone, "❌ Server disconnected");
    });

    Ok(DownstreamServer { tx })
}

// --- Core Logic ---

async fn handle_tools_list(
    _state: &RouterState,
    upstream_id: Value,
) -> JsonRpcMessage {
    let search_tool = serde_json::json!({
        "name": "search_tools",
        "description": "Semantic Search. Use this to find relevant tools for your task.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Task description" }
            },
            "required": ["query"]
        }
    });

    let router_status = serde_json::json!({
        "name": "router_status",
        "description": "Checks the health of the semantic router.",
        "inputSchema": { "type": "object", "properties": {} }
    });

    JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(upstream_id),
        result: Some(serde_json::json!({
            "tools": [search_tool, router_status]
        })),
        error: None,
        method: None,
        params: None,
    }
}

#[instrument(skip(state))]
async fn handle_tool_call(
    state: &RouterState,
    params: Value,
    upstream_id: Value
) -> Result<Option<JsonRpcMessage>> {
    let tool_name = params.get("name")
       .and_then(|n| n.as_str())
       .context("Missing tool name")?;

    info!(tool = tool_name, "📥 Tool call received");

    // --- CASE 1: Semantic Search ---
    if tool_name == "search_tools" {
        let query = params.get("arguments")
            .and_then(|a| a.get("query"))
            .and_then(|q| q.as_str())
            .context("Missing 'query' argument")?;

        info!(query = %query, "Performing semantic search");

        let results = state.semantic.search(query, 5).await?;

        let found_tools: Vec<Value> = results.iter().map(|t| {
            let mut schema = t.full_schema.clone();
            let namespaced_name = format!("{}___{}", t.server_id, t.name);
            schema["name"] = Value::String(namespaced_name);
            schema
        }).collect();
        
        // Visual Log
        info!("════════════ SEMANTIC ROUTER RESULTS ════════════");
        for tool in &results {
            info!(" • Found: {:<20} (Server: {})", tool.name, tool.server_id);
        }

        return Ok(Some(JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(upstream_id),
            result: Some(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("Found {} relevant tools:\n{}", 
                        found_tools.len(), 
                        serde_json::to_string_pretty(&found_tools)?
                    )
                }]
            })),
            error: None,
            method: None,
            params: None,
        }));
    }

    // --- CASE 2: Downstream Tool Call (ROBUST VERSION) ---
    if let Some((server_name, real_tool_name)) = tool_name.split_once("___") {
        if let Some(server) = state.clients.get(server_name) {
            let mut new_params = params.clone();
            new_params["name"] = Value::String(real_tool_name.to_string());

            let forward_msg = JsonRpcMessage {
                jsonrpc: "2.0".into(),
                id: Some(upstream_id.clone()),
                method: Some("tools/call".into()),
                params: Some(new_params),
                result: None, error: None,
            };

            // 1. Send Request
            if let Err(e) = server.tx.send(forward_msg).await {
                error!(server = server_name, error = %e, "Failed to send to downstream");
                return Ok(Some(JsonRpcMessage {
                    jsonrpc: "2.0".into(), id: Some(upstream_id),
                    error: Some(serde_json::json!({"code": -32000, "message": "Downstream connection died"})),
                    result: None, method: None, params: None,
                }));
            }

            info!(server = server_name, tool = real_tool_name, "🚀 Forwarded to downstream");
            return Ok(None); 
        } else {
            warn!(server = server_name, "Server not found in registry");
            return Ok(Some(JsonRpcMessage {
                jsonrpc: "2.0".into(), id: Some(upstream_id),
                error: Some(serde_json::json!({"code": -32601, "message": "Server offline"})),
                result: None, method: None, params: None,
            }));
        }
    }

    // --- CASE 3: Internal Health Check ---
    if tool_name == "router_status" {
        return Ok(Some(JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(upstream_id),
            result: Some(serde_json::json!({
                "content": [{"type": "text", "text": format!("Router Online. Connected servers: {}", state.clients.len())}]
            })),
            error: None,
            method: None,
            params: None,
        }));
    }
    
    Ok(Some(JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(upstream_id),
        error: Some(serde_json::json!({"code": -32601, "message": "Tool not found"})),
        result: None, method: None, params: None,
    }))
}

// --- Main Entry Point ---

#[tokio::main]
async fn main() -> Result<()> {
    // FIX: Moved inside main
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    // 1. Load Config
    let config_str = std::fs::read_to_string("config.json").unwrap_or_else(|_| "{\"servers\":{}}".into());
    let config: Config = serde_json::from_str(&config_str)?;

    info!("🧠 Initializing Semantic Engine...");
    let semantic = Arc::new(SemanticEngine::new()?);
    
    let state = Arc::new(RouterState {
        clients: DashMap::new(),
        semantic: semantic.clone(),
    });

    let (router_tx, mut router_rx) = mpsc::channel::<(String, JsonRpcMessage)>(100);

    // 2. Spawn Servers & Initiate Handshake
    for (id, server_conf) in config.servers {
        if let Ok(server) = spawn_downstream_server(id.clone(), server_conf, router_tx.clone()).await {
            
            // A. Send "initialize" Request
            let init_msg = JsonRpcMessage {
                jsonrpc: "2.0".into(),
                id: Some(Value::String(INTERNAL_INIT_ID.into())),
                method: Some("initialize".into()),
                params: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "mcp-semantic-router", "version": "0.1.0" }
                })),
                result: None, error: None,
            };
            server.tx.send(init_msg).await?;
            
            state.clients.insert(id, server);
        }
    }

    // 3. Setup Upstream (Stdin/Stdout)
    let (upstream_tx, mut upstream_rx) = mpsc::channel::<JsonRpcMessage>(100);
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&line) {
                let _ = upstream_tx.send(msg).await;
            }
        }
    });

    let (response_tx, mut response_rx) = mpsc::channel::<JsonRpcMessage>(100);
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = response_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = stdout.write_all(json.as_bytes()).await;
                let _ = stdout.write_all(b"\n").await;
                let _ = stdout.flush().await;
            }
        }
    });

    info!("✅ Router Ready. Listening on stdio...");

    // 4. Main Event Loop
    loop {
        tokio::select! {
            // --- UPSTREAM (User/Claude) MESSAGES ---
            Some(msg) = upstream_rx.recv() => {
                if let Some(method) = &msg.method {
                    match method.as_str() {
                        "initialize" => {
                            let response = JsonRpcMessage {
                                jsonrpc: "2.0".into(),
                                id: msg.id,
                                result: Some(serde_json::json!({
                                    "protocolVersion": "2024-11-05",
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "mcp-semantic-router", "version": "0.1.0" }
                                })),
                                error: None, method: None, params: None,
                            };
                            response_tx.send(response).await?;
                        }
                        "tools/list" => {
                            if let Some(id) = msg.id {
                                let response = handle_tools_list(&state, id).await;
                                response_tx.send(response).await?;
                            }
                        }
                        "tools/call" => {
                            if let Some(params) = msg.params {
                                if let Some(id) = msg.id {
                                    if let Some(response) = handle_tool_call(&state, params, id).await? {
                                        response_tx.send(response).await?;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // --- DOWNSTREAM (Git/Memory) MESSAGES ---
            Some((server_id, msg)) = router_rx.recv() => {
                // Check if this is an internal router request
                if let Some(id) = &msg.id {
                    
                    if id == INTERNAL_INIT_ID {
                        info!("✨ Server '{}' initialized. Fetching tools...", server_id);
                        if let Some(server) = state.clients.get(&server_id) {
                            let notify = JsonRpcMessage {
                                jsonrpc: "2.0".into(), id: None,
                                method: Some("notifications/initialized".into()),
                                params: Some(serde_json::json!({})),
                                result: None, error: None,
                            };
                            server.tx.send(notify).await?;

                            let list_req = JsonRpcMessage {
                                jsonrpc: "2.0".into(),
                                id: Some(Value::String(INTERNAL_LIST_ID.into())),
                                method: Some("tools/list".into()),
                                params: Some(serde_json::json!({})),
                                result: None, error: None,
                            };
                            server.tx.send(list_req).await?;
                        }
                        continue;
                    }

                    if id == INTERNAL_LIST_ID {
                        if let Some(result) = msg.result {
                            if let Some(tools) = result.get("tools").and_then(|t| t.as_array()) {
                                info!("📦 Received {} tools from '{}'. Indexing...", tools.len(), server_id);
                                let _ = state.semantic.ingest_tools(server_id, tools.clone()).await;
                            }
                        }
                        continue;
                    }
                }

                if let Some(method) = &msg.method {
                if method == "notifications/tools/list_changed" {
                    info!(server = %server_id, "♻️ Server signaled tool change. Refetching...");
                    
                    if let Some(server) = state.clients.get(&server_id) {
                        // Re-trigger the Fetch Flow
                        let list_req = JsonRpcMessage {
                            jsonrpc: "2.0".into(),
                            id: Some(Value::String(INTERNAL_LIST_ID.into())), // This ID triggers the indexing logic on response
                            method: Some("tools/list".into()),
                            params: Some(serde_json::json!({})),
                            result: None, error: None,
                        };
                        
                        // We don't wait for response here; the main loop handles the INTERNAL_LIST_ID response
                        if let Err(e) = server.tx.send(list_req).await {
                            error!(server = %server_id, error = %e, "Failed to request updated tools");
                        }
                    }
                    continue; // Don't forward this notification upstream
                    }
                }

                if msg.result.is_some() || msg.error.is_some() {
                    response_tx.send(msg).await?;
                }
            }
        }
    }
}