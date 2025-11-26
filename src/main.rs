// src/main.rs
use anyhow::{Context, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

mod semantic;
use semantic::SemanticEngine;

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
    eprintln!(" Spawning server: {}", server_id);

    let mut child = Command::new(&config.command)
       .args(&config.args)
       .stdin(Stdio::piped())
       .stdout(Stdio::piped())
       .stderr(Stdio::inherit())
       .spawn()
       .context(format!("Failed to spawn {}", server_id))?;

    let mut stdin = child.stdin.take().context("Failed to open stdin")?;
    let stdout = child.stdout.take().context("Failed to open stdout")?;

    // 1. Writer Channel: Router -> Downstream
    let (tx, mut rx) = mpsc::channel::<JsonRpcMessage>(32);
    
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = stdin.write_all(json.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            }
        }
    });

    // 2. Reader Loop: Downstream -> Router
    let server_id_clone = server_id.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&line) {
                let _ = router_tx.send((server_id_clone.clone(), msg)).await;
            }
        }
        eprintln!(" Server {} disconnected", server_id_clone);
    });

    Ok(DownstreamServer { tx })
}

// --- Core Logic ---

// --- Core Logic ---

async fn handle_tools_list(
    _state: &RouterState,
    upstream_id: Value,
) -> JsonRpcMessage {
    // 1. Define the "Meta-Tool"
    // Instead of showing all 100 tools, we only show this ONE tool.
    // This forces the Agent to ask us what it needs.
    let search_tool = serde_json::json!({
        "name": "search_tools",
        "description": "Semantic Search. Use this to find relevant tools for your task. Input a natural language query describing what you want to do (e.g., 'save code to git', 'read file').",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The natural language description of the task"
                }
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

async fn handle_tool_call(
    state: &RouterState,
    params: Value,
    upstream_id: Value
) -> Result<Option<JsonRpcMessage>> {
    let tool_name = params.get("name")
       .and_then(|n| n.as_str())
       .context("Missing tool name")?;

    eprintln!(" Tool call received: {}", tool_name);

    // --- CASE 1: The Agent is searching for tools ---
    if tool_name == "search_tools" {
        let query = params.get("arguments")
            .and_then(|a| a.get("query"))
            .and_then(|q| q.as_str())
            .context("Missing 'query' argument")?;

        eprintln!(" Performing Semantic Search for: '{}'", query);

        // 1. Search the index (limit 3 results)
        let results = state.semantic.search(query, 3).await?;

        // 2. Format the results as a string to give back to the Agent
        // We return the actual JSON schemas of the found tools so the Agent knows how to use them.
        let found_tools: Vec<Value> = results.iter().map(|t| {
            // We namespace the tool name so the Router knows where to send it later
            let mut schema = t.full_schema.clone();
            let namespaced_name = format!("{}___{}", t.server_id, t.name);
            schema["name"] = Value::String(namespaced_name);
            schema
        }).collect();

        return Ok(Some(JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(upstream_id),
            result: Some(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("Found {} relevant tools. Use these schemas to perform your task:\n{}", 
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

    // --- CASE 2: The Agent is calling a specific downstream tool ---
    // Note: We use "___" (3 underscores) as a separator to avoid conflict with tool names using single underscores
    if let Some((server_name, real_tool_name)) = tool_name.split_once("___") {
        if let Some(server) = state.clients.get(server_name) {
            let mut new_params = params.clone();
            
            // Fix the params to remove the namespaced name
            new_params["name"] = Value::String(real_tool_name.to_string());

            let forward_msg = JsonRpcMessage {
                jsonrpc: "2.0".into(),
                id: Some(upstream_id),
                method: Some("tools/call".into()),
                params: Some(new_params),
                result: None,
                error: None,
            };

            let tx = server.tx.clone();
            tx.send(forward_msg).await?;
            
            return Ok(None); 
        }
    }

    // --- CASE 3: Internal Tools ---
    if tool_name == "router_status" {
        return Ok(Some(JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(upstream_id),
            result: Some(serde_json::json!({
                "content": [{"type": "text", "text": "Router is Online"}]
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
        result: None,
        method: None,
        params: None,
    }))
}

// --- Main Entry Point ---

async fn mock_ingest_tools(state: Arc<RouterState>) {
    let git_tools = serde_json::json!([
        {
            "name": "git_commit",
            "description": "Records changes to the repository",
            "inputSchema": {}
        },
        {
            "name": "git_push",
            "description": "Updates remote refs along with associated objects",
            "inputSchema": {}
        }
    ]);
    
    let fs_tools = serde_json::json!([
        {
            "name": "read_file",
            "description": "Reads the contents of a file from the disk",
            "inputSchema": {}
        },
        {
            "name": "list_directory",
            "description": "Lists files in a specific directory",
            "inputSchema": {}
        }
    ]);

    if let Some(arr) = git_tools.as_array() {
        let _ = state.semantic.ingest_tools("git_server".into(), arr.clone()).await;
    }
    if let Some(arr) = fs_tools.as_array() {
        let _ = state.semantic.ingest_tools("fs_server".into(), arr.clone()).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Load Config
    let config_str = std::fs::read_to_string("config.json").unwrap_or_else(|_| "{\"servers\":{}}".into());
    let config: Config = serde_json::from_str(&config_str)?;

    eprintln!("Initializing Semantic Engine...");
    let semantic = Arc::new(SemanticEngine::new()?);
    
    // 2. Setup State
    let state = Arc::new(RouterState {
        clients: DashMap::new(),
        semantic: semantic.clone(),
    });

    let (router_tx, mut router_rx) = mpsc::channel::<(String, JsonRpcMessage)>(100);

    // 3. Spawn Downstream Servers
    for (id, server_conf) in config.servers {
        if let Ok(server) = spawn_downstream_server(id.clone(), server_conf, router_tx.clone()).await {
            state.clients.insert(id, server);
        }
    }

    // 4. Setup Upstream (Stdin/Stdout)
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

    eprintln!(" Ready. Listening on stdio...");

    mock_ingest_tools(state.clone()).await;

    // 5. Main Event Loop
    loop {
        tokio::select! {
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
                                    "serverInfo": { "name": "mcp-router", "version": "0.1.0" }
                                })),
                                error: None,
                                method: None,
                                params: None,
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

            Some((_server_id, msg)) = router_rx.recv() => {
                if msg.result.is_some() || msg.error.is_some() {
                    response_tx.send(msg).await?;
                }
            }
        }
    }
}