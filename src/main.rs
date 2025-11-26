// src/main.rs

use anyhow::{Context, Result};
use dashmap::DashMap;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

// --- Data Structures ---

#
struct ServerConfig {
    command: String,
    args: Vec<String>,
}

#
struct Config {
    servers: std::collections::HashMap<String, ServerConfig>,
}

#
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

type RouterState = Arc<DashMap<String, DownstreamServer>>;

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
       .stderr(Stdio::inherit()) // Let stderr go to console for debugging
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
                // Tag message with origin server_id before sending to main loop
                let _ = router_tx.send((server_id_clone.clone(), msg)).await;
            }
        }
        eprintln!(" Server {} disconnected", server_id_clone);
    });

    Ok(DownstreamServer { tx })
}

// --- Core Logic ---

async fn handle_tools_list(
    state: &RouterState,
    upstream_id: Value,
) -> JsonRpcMessage {
    let mut all_tools = Vec::new();
    
    // We need to query all servers. This is a "Scatter-Gather" pattern.
    // We assume every downstream server implements "tools/list".
    
    // In a real implementation, we would need to track Request IDs here to map
    // the responses back. For this skeleton, we are simplifying by assuming
    // we can just wait for a short timeout or until we get results.
    // *Implementing full ID correlation is the next step.*
    
    // NOTE: For this Skeleton Phase, we will just acknowledge the request
    // and return a placeholder to prove the router works. 
    // To implement full aggregation, we need to send requests to all children,
    // create a temporary "pending aggregation" state, and wait for them.
    
    // Let's simulate an aggregated tool list to demonstrate the structure:
    
    let tool = serde_json::json!({
        "name": "router_status",
        "description": "Returns the status of the router and connected servers",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    });
    all_tools.push(tool);

    // Add tools from downstream (conceptually - requires complex async correlation)
    // In Phase 2, we will implement the RequestId mapping to actually fetch these.

    JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(upstream_id),
        result: Some(serde_json::json!({
            "tools": all_tools
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
    // Extract tool name
    let tool_name = params.get("name")
       .and_then(|n| n.as_str())
       .context("Missing tool name")?;

    eprintln!(" Tool call received: {}", tool_name);

    // Routing Logic: Check if tool name is namespaced (e.g. "git__commit")
    if let Some((server_name, real_tool_name)) = tool_name.split_once("__") {
        if let Some(server) = state.get(server_name) {
            // Construct payload for downstream
            let mut new_params = params.clone();
            new_params["name"] = Value::String(real_tool_name.to_string());

            let forward_msg = JsonRpcMessage {
                jsonrpc: "2.0".into(),
                id: Some(upstream_id), // In production, we map this ID!
                method: Some("tools/call".into()),
                params: Some(new_params),
                result: None,
                error: None,
            };

            server.tx.send(forward_msg).await?;
            // We don't return a response here immediately; the downstream 
            // server's response will be handled by the main loop.
            return Ok(None); 
        }
    }

    // If internal tool
    if tool_name == "router_status" {
        return Ok(Some(JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(upstream_id),
            result: Some(serde_json::json!({
                "content":
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

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Load Config
    let config_str = std::fs::read_to_string("config.json").unwrap_or_else(|_| "{\"servers\":{}}".into());
    let config: Config = serde_json::from_str(&config_str)?;

    // 2. Setup State
    let state = Arc::new(DashMap::new());
    let (router_tx, mut router_rx) = mpsc::channel::<(String, JsonRpcMessage)>(100);

    // 3. Spawn Downstream Servers
    for (id, server_conf) in config.servers {
        if let Ok(server) = spawn_downstream_server(id.clone(), server_conf, router_tx.clone()).await {
            state.insert(id, server);
        }
    }

    // 4. Setup Upstream (Stdin/Stdout)
    let (upstream_tx, mut upstream_rx) = mpsc::channel::<JsonRpcMessage>(100);
    
    // Task: Read from Stdin (Upstream Client)
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&line) {
                let _ = upstream_tx.send(msg).await;
            }
        }
    });

    // Task: Write to Stdout (Upstream Client)
    // We use a separate channel for responses so we can send from anywhere
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

    // 5. Main Event Loop
    loop {
        tokio::select! {
            // Handle Upstream Request (Claude -> Router)
            Some(msg) = upstream_rx.recv() => {
                if let Some(method) = &msg.method {
                    match method.as_str() {
                        "initialize" => {
                            // Basic handshake
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
                            // Phase 1: Return aggregated list (stubbed for now)
                            if let Some(id) = msg.id {
                                let response = handle_tools_list(&state, id).await;
                                response_tx.send(response).await?;
                            }
                        }
                        "tools/call" => {
                            // Phase 1: Basic Routing
                            if let Some(params) = msg.params {
                                if let Some(id) = msg.id {
                                    if let Some(response) = handle_tool_call(&state, params, id).await? {
                                        response_tx.send(response).await?;
                                    }
                                    // If None, it means we forwarded it, and we await the response in the other branch
                                }
                            }
                        }
                        _ => {
                            // Ignore other notifications for now
                        }
                    }
                }
            }

            // Handle Downstream Response (Server -> Router -> Claude)
            Some((server_id, msg)) = router_rx.recv() => {
                // Here is where we would map the ID back to the original upstream ID.
                // For Phase 1, we just forward "Result" messages blindly up.
                if msg.result.is_some() |

| msg.error.is_some() {
                    // Forward response to upstream
                    response_tx.send(msg).await?;
                }
            }
        }
    }
}