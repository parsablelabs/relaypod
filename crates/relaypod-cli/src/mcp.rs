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

        // Notifications have no `id` and expect no response.
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

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
            "description": "List all active (non-archived) channels on the RelayPod board.",
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

fn require_str_arg<'a>(args: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(format!("missing or invalid required string parameter '{key}'")),
    }
}

async fn handle_tool_call(client: &RelayPodClient, id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let invalid_params = |msg: String| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32602,
                "message": msg
            }
        })
    };

    let result: Result<Value> = match name {
        "list_channels" => client
            .list_channels()
            .await
            .and_then(|v| Ok(serde_json::to_value(v)?)),
        "list_identities" => client
            .list_identities()
            .await
            .and_then(|v| Ok(serde_json::to_value(v)?)),
        "read_channel" => {
            let channel = match require_str_arg(&args, "channel") {
                Ok(v) => v,
                Err(msg) => return invalid_params(msg),
            };
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(25) as usize;
            client
                .list_channel_messages(channel, limit)
                .await
                .and_then(|v| Ok(serde_json::to_value(v)?))
        }
        "read_thread" => {
            let message_id = match require_str_arg(&args, "message_id") {
                Ok(v) => v,
                Err(msg) => return invalid_params(msg),
            };
            client
                .get_thread(message_id)
                .await
                .and_then(|v| Ok(serde_json::to_value(v)?))
        }
        "post_message" => {
            let channel = match require_str_arg(&args, "channel") {
                Ok(v) => v,
                Err(msg) => return invalid_params(msg),
            };
            let body = match require_str_arg(&args, "body") {
                Ok(v) => v.to_string(),
                Err(msg) => return invalid_params(msg),
            };
            let parent_id = args
                .get("parent_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            client
                .post_message(channel, body, parent_id)
                .await
                .and_then(|v| Ok(serde_json::to_value(v)?))
        }
        "get_unread" => client
            .list_unread()
            .await
            .and_then(|v| Ok(serde_json::to_value(v)?)),
        "ack_unread" => {
            let unread_id = match require_str_arg(&args, "unread_id") {
                Ok(v) => v,
                Err(msg) => return invalid_params(msg),
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_missing_required_tool_params_with_invalid_params_code() {
        let client = RelayPodClient::new("http://127.0.0.1:1", "rp_test");

        let resp = handle_tool_call(
            &client,
            json!(1),
            json!({ "name": "read_channel", "arguments": {} }),
        )
        .await;
        assert_eq!(resp["error"]["code"], json!(-32602));

        let resp = handle_tool_call(
            &client,
            json!(2),
            json!({ "name": "post_message", "arguments": { "channel": "general", "body": "   " } }),
        )
        .await;
        assert_eq!(resp["error"]["code"], json!(-32602));

        let resp = handle_tool_call(
            &client,
            json!(3),
            json!({ "name": "ack_unread", "arguments": { "unread_id": 123 } }),
        )
        .await;
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    #[tokio::test]
    async fn unknown_tool_returns_is_error_result() {
        let client = RelayPodClient::new("http://127.0.0.1:1", "rp_test");
        let resp = handle_tool_call(
            &client,
            json!(9),
            json!({ "name": "nonexistent", "arguments": {} }),
        )
        .await;
        assert_eq!(resp["result"]["isError"], json!(true));
    }
}
