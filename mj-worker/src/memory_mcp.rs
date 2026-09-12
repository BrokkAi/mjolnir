//! MCP server for the target project memory replica.
use anyhow::{Context, Result, bail};
use mj_core::project_memory::*;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;
/// Serve the project memory tools over MCP's JSON-lines stdio transport.
pub fn run_mcp_stdio(root: &Path) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("create project memory root {}", root.display()))?;
    let store = ProjectMemoryStore::new(root);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.context("read MCP request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_json_line(
                    &mut output,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
                )?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => json_rpc_result(
                id,
                json!({
                    "protocolVersion": request.pointer("/params/protocolVersion").cloned().unwrap_or_else(|| json!("2025-03-26")),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "mj-project-memory", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": MEMORY_GUIDANCE
                }),
            ),
            "ping" => json_rpc_result(id, json!({})),
            "tools/list" => json_rpc_result(id, json!({"tools": tool_definitions()})),
            "tools/call" => match call_tool(&store, request.get("params")) {
                Ok((structured, is_error)) => json_rpc_result(
                    id,
                    json!({
                        "content": [{"type":"text", "text": serde_json::to_string_pretty(&structured)?}],
                        "structuredContent": structured,
                        "isError": is_error
                    }),
                ),
                Err(error) => json_rpc_error(id, -32602, format!("{error:#}")),
            },
            _ => json_rpc_error(id, -32601, format!("unknown MCP method {method:?}")),
        };
        write_json_line(&mut output, &response)?;
    }
    Ok(())
}

fn call_tool(store: &ProjectMemoryStore, params: Option<&Value>) -> Result<(Value, bool)> {
    let params = params.context("tools/call is missing params")?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call is missing name")?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let result = match name {
        "memory_list" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Arguments {
                path_prefix: Option<String>,
                cursor: Option<String>,
            }
            let arguments: Arguments = serde_json::from_value(arguments)?;
            serde_json::to_value(store.list(
                arguments.path_prefix.as_deref(),
                arguments.cursor.as_deref(),
            ))?
        }
        "memory_read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Arguments {
                path: String,
            }
            let arguments: Arguments = serde_json::from_value(arguments)?;
            serde_json::to_value(store.read(&arguments.path))?
        }
        "memory_write" => {
            let request: MemoryWriteRequest = serde_json::from_value(arguments)?;
            serde_json::to_value(store.write(request))?
        }
        _ => bail!("unknown memory tool {name:?}"),
    };
    let is_error = result
        .get("outcome")
        .and_then(Value::as_str)
        .is_some_and(|outcome| outcome != "ok");
    Ok((result, is_error))
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_list",
            "description": "Discover persistent memory documents when the supplied /MEMORY.md index is insufficient for the current task. Select relevant notes rather than routinely listing and reading the entire store.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path_prefix": {"type":"string", "description":"Optional virtual directory prefix, such as /roots/api/."},
                    "cursor": {"type":"string", "description":"Last path from the previous page."}
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_read",
            "description": "Read one persistent memory document and its version token. Choose notes whose index descriptions match the current task. Reuse information already in context unless freshness or an upcoming update requires another read.",
            "inputSchema": {
                "type": "object",
                "properties": {"path": {"type":"string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "memory_write",
            "description": "Save a concise, reusable lesson or decision. Replaces the whole document; use the read version or new. Index new notes in /MEMORY.md.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type":"string"},
                    "content": {"type":"string", "description":"Full UTF-8 replacement content."},
                    "if_version": {"type":"string", "maxLength":64, "description":"Version from memory_read, or new when creating."}
                },
                "required": ["path", "content", "if_version"],
                "additionalProperties": false
            }
        }),
    ]
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

fn json_rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}

fn write_json_line(output: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

use serde::Deserialize;
use std::fs;
