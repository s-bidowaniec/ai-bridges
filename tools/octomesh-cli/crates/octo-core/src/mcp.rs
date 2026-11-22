use crate::router::{AskRequest, MeshRouter};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error};

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Runs the MCP server strictly over stdio (JSON-RPC 2.0 newline-delimited).
/// All diagnostic logging must go to stderr so stdout remains pure protocol frames.
pub async fn serve_stdio(router: Arc<MeshRouter>) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Malformed JSON-RPC frame on stdin");
                continue;
            }
        };

        // Notifications have no `id` and expect no response
        let Some(id) = req.id.clone() else {
            debug!(method = %req.method, "Received MCP notification");
            continue;
        };

        let response = handle_request(&router, id, &req.method, req.params).await;
        let encoded = serde_json::to_string(&response)?;
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn handle_request(
    router: &MeshRouter,
    id: Value,
    method: &str,
    params: Value,
) -> JsonRpcResponse {
    match method {
        "initialize" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "octomesh-cli",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
            error: None,
        },
        "ping" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({})),
            error: None,
        },
        "tools/list" => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "tools": [
                    {
                        "name": "mesh_status",
                        "description": "List available mesh peers, role failover chains, and current SQLite cooldown states.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }
                    },
                    {
                        "name": "mesh_ask",
                        "description": "Delegate a coding subtask, question, or code review to another peer or role chain with automatic rate-limit failover.",
                        "inputSchema": {
                            "type": "object",
                            "required": ["target", "prompt", "caller"],
                            "properties": {
                                "target": {
                                    "type": "string",
                                    "description": "Peer name (e.g. 'claude', 'vibe') or role name (e.g. 'coder', 'reviewer')."
                                },
                                "prompt": {
                                    "type": "string",
                                    "description": "Task instructions or review prompt."
                                },
                                "caller": {
                                    "type": "string",
                                    "description": "Identifier of the calling agent (anti-loop guard)."
                                },
                                "depth": {
                                    "type": "integer",
                                    "default": 1,
                                    "description": "Current delegation hop depth (incremented on nested calls)."
                                },
                                "include_diff": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "Attach truncated git status and git diff context."
                                },
                                "readonly": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "Instruct target peer to operate in read-only review/advisory mode."
                                }
                            }
                        }
                    },
                    {
                        "name": "mesh_handoff",
                        "description": "Record a structured task summary and checkpoint to the SQLite blackboard so another terminal session can pick up seamlessly.",
                        "inputSchema": {
                            "type": "object",
                            "required": ["author", "task_summary", "next_steps"],
                            "properties": {
                                "author": {
                                    "type": "string",
                                    "description": "Agent recording the handoff note."
                                },
                                "task_summary": {
                                    "type": "string",
                                    "description": "Summary of completed work and decisions."
                                },
                                "next_steps": {
                                    "type": "string",
                                    "description": "Concrete remaining steps for the next agent."
                                },
                                "files_touched": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "default": [],
                                    "description": "List of modified or relevant file paths."
                                }
                            }
                        }
                    }
                ]
            })),
            error: None,
        },
        "tools/call" => handle_tool_call(router, id, params).await,
        other => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("Method not found: {}", other),
            }),
        },
    }
}

async fn handle_tool_call(router: &MeshRouter, id: Value, params: Value) -> JsonRpcResponse {
    let tool_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    match tool_name {
        "mesh_status" => {
            let mut peer_names: Vec<String> = router.config().peers.keys().cloned().collect();
            peer_names.sort();
            match router.blackboard().list_peers(&peer_names) {
                Ok(peers) => {
                    let handoffs = router.blackboard().recent_handoffs(3).unwrap_or_default();
                    let payload = json!({
                        "max_depth": router.config().mesh.max_depth,
                        "roles": router.config().roles,
                        "peers": peers,
                        "recent_handoffs": handoffs,
                    });
                    tool_ok(id, serde_json::to_string_pretty(&payload).unwrap_or_default())
                }
                Err(e) => tool_err(id, format!("Failed to read blackboard status: {}", e)),
            }
        }
        "mesh_ask" => {
            let target = match args.get("target").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return tool_err(id, "Missing required argument 'target'".to_string()),
            };
            let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return tool_err(id, "Missing required argument 'prompt'".to_string()),
            };
            let caller = args
                .get("caller")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            let include_diff = args
                .get("include_diff")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let readonly = args
                .get("readonly")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let req = AskRequest {
                target,
                prompt,
                caller,
                depth,
                include_diff,
                readonly,
            };

            match router.ask(req).await {
                Ok(resp) => {
                    let formatted = format!(
                        "[resolved_peer: {}]\n\n{}",
                        resp.resolved_peer, resp.output
                    );
                    tool_ok(id, formatted)
                }
                Err(e) => tool_err(id, e.to_string()),
            }
        }
        "mesh_handoff" => {
            let author = args
                .get("author")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let task_summary = match args.get("task_summary").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return tool_err(id, "Missing required argument 'task_summary'".to_string()),
            };
            let next_steps = match args.get("next_steps").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return tool_err(id, "Missing required argument 'next_steps'".to_string()),
            };
            let files_touched: Vec<String> = args
                .get("files_touched")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            match router
                .blackboard()
                .record_handoff(author, task_summary, next_steps, &files_touched)
            {
                Ok(note) => tool_ok(
                    id,
                    format!(
                        "Recorded handoff #{} by '{}' at {}",
                        note.id,
                        note.author,
                        note.created_at.to_rfc3339()
                    ),
                ),
                Err(e) => tool_err(id, format!("Failed to save handoff note: {}", e)),
            }
        }
        unknown => JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32602,
                message: format!("Unknown tool: '{}'", unknown),
            }),
        },
    }
}

fn tool_ok(id: Value, text: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "isError": false
        })),
        error: None,
    }
}

fn tool_err(id: Value, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(json!({
            "content": [
                {
                    "type": "text",
                    "text": message
                }
            ],
            "isError": true
        })),
        error: None,
    }
}
