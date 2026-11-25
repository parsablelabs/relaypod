use anyhow::Result;
use relaypod_client::RelayPodClient;
use serde_json::{json, Value};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Runs a JSON-RPC 2.0 MCP server over stdio.
pub async fn run_mcp_stdio(client: RelayPodClient) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                write_json_line(&mut stdout, &err_resp).await?;
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        // Notifications have no `id` and expect no response.
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "relaypod",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
            "ping" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {}
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": tool_definitions()
                }
            }),
            "tools/call" => handle_tool_call(&client, id, params).await,
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {other}")
                }
            }),
        };

        write_json_line(&mut stdout, &response).await?;
    }

    Ok(())
}

async fn write_json_line(stdout: &mut io::Stdout, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "list_channels",
            "description": "List all channels on the RelayPod board.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "list_identities",
            "description": "List all human and agent identities (@handles) on the RelayPod board, including their `kind` (human/agent), `online` presence, and `busy` (currently working) status.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "read_channel",
            "description": "Read recent top-level messages in a channel (by channel name or ID).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel name (e.g. 'general') or ID" },
                    "limit": { "type": "integer", "description": "Max messages to return (default 25)", "default": 25 }
                },
                "required": ["channel"]
            }
        },
        {
            "name": "read_thread",
            "description": "Read a parent message and all replies in a thread.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message_id": { "type": "string", "description": "ID of the thread root or any message in the thread" }
                },
                "required": ["message_id"]
            }
        },
        {
            "name": "post_message",
            "description": "Post a message to a channel or reply in an existing thread. IMPORTANT: Other agents in RelayPod only wake up when explicitly @mentioned in `body`. Include @handle whenever you want another agent or human to act next; omit agent @mentions when no further agent action is needed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel name (e.g. 'general') or ID" },
                    "body": { "type": "string", "description": "Message body (Markdown supported; explicit @handle mentions are required to notify/wake other agents)" },
                    "parent_id": { "type": "string", "description": "Optional parent message ID to reply in a thread" }
                },
                "required": ["channel", "body"]
            }
        },
        {
            "name": "get_unread",
            "description": "Fetch pending unacknowledged messages in this identity's unread queue.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "ack_unread",
            "description": "Acknowledge an unread queue item once it has been handled.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unread_id": { "type": "string", "description": "The ID of the unread queue item to acknowledge" }
                },
                "required": ["unread_id"]
            }
        }
    ])
}

async fn handle_tool_call(client: &RelayPodClient, id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let result: Result<Value> = match name {
        "list_channels" => client
            .list_channels()
            .await
            .map(|v| serde_json::to_value(v).unwrap()),
        "list_identities" => client
            .list_identities()
            .await
            .map(|v| serde_json::to_value(v).unwrap()),
        "read_channel" => {
            let channel = args
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("general");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(25) as usize;
            client
                .list_channel_messages(channel, limit)
                .await
                .map(|v| serde_json::to_value(v).unwrap())
        }
        "read_thread" => {
            let message_id = args
                .get("message_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            client
                .get_thread(message_id)
                .await
                .map(|v| serde_json::to_value(v).unwrap())
        }
        "post_message" => {
            let channel = args
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("general");
            let body = args
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parent_id = args
                .get("parent_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            client
                .post_message(channel, body, parent_id)
                .await
                .map(|v| serde_json::to_value(v).unwrap())
        }
        "get_unread" => client
            .list_unread()
            .await
            .map(|v| serde_json::to_value(v).unwrap()),
        "ack_unread" => {
            let unread_id = args.get("unread_id").and_then(|v| v.as_str()).unwrap_or("");
            client
                .ack_unread(unread_id)
                .await
                .map(|()| json!({ "acked": unread_id }))
        }
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    match result {
        Ok(data) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
                    }
                ]
            }
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "isError": true,
                "content": [
                    {
                        "type": "text",
                        "text": format!("Error: {err}")
                    }
                ]
            }
        }),
    }
}
