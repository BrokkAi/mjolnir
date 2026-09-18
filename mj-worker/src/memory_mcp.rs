//! MCP server for the target project memory replica.
use anyhow::{Context, Result, bail};
use mj_core::project_memory::*;
use serde_json::{Value, json};
use std::path::Path;
/// Serve the project memory tools over MCP's JSON-lines stdio transport.
pub fn run_mcp_stdio(root: &Path) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("create project memory root {}", root.display()))?;
    let store = ProjectMemoryStore::new(root);
    crate::mcp_stdio::serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        crate::mcp_stdio::McpServer {
            name: "mj-memory",
            instructions: MEMORY_GUIDANCE,
            tools: tool_definitions(),
            // Writes compare versions; answering one call at a time keeps two
            // edits from racing inside one harness.
            dispatch: crate::mcp_stdio::Dispatch::Sequential,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            // Memory calls are local file work and answer at once.
            call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                call_tool(&store, params)
            },
        },
    )
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
        "list" => {
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
        "read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Arguments {
                path: String,
            }
            let arguments: Arguments = serde_json::from_value(arguments)?;
            serde_json::to_value(store.read(&arguments.path))?
        }
        "write" => {
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
            "name": "list",
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
            "name": "read",
            "description": "Read one persistent memory document and its version token. Choose notes whose index descriptions match the current task. Reuse information already in context unless freshness or an upcoming update requires another read.",
            "inputSchema": {
                "type": "object",
                "properties": {"path": {"type":"string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "write",
            "description": "Save a concise, reusable lesson or decision. Replaces the whole document; use the read version or new. Index new notes in /MEMORY.md.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type":"string"},
                    "content": {"type":"string", "description":"Full UTF-8 replacement content."},
                    "if_version": {"type":"string", "maxLength":64, "description":"Version from read, or new when creating."}
                },
                "required": ["path", "content", "if_version"],
                "additionalProperties": false
            }
        }),
    ]
}

use serde::Deserialize;
use std::fs;
