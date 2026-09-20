//! MCP server for the target project memory replica.
use anyhow::{Context, Result, bail};
use mj_core::project_memory::*;
use serde_json::{Value, json};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
mod history;
/// Serve the project memory tools over MCP's JSON-lines stdio transport.
pub fn run_mcp_stdio(root: &Path) -> Result<()> {
    run_mcp_stdio_with_history(root, None, true)
}

pub fn run_mcp_stdio_with_history(
    root: &Path,
    socket: Option<PathBuf>,
    documents: bool,
) -> Result<()> {
    serve(
        root,
        socket,
        documents,
        std::io::stdin().lock(),
        std::io::stdout(),
    )
}

fn serve<R: std::io::BufRead, W: std::io::Write + Send + Sync + 'static>(
    root: &Path,
    socket: Option<PathBuf>,
    documents: bool,
    reader: R,
    writer: W,
) -> Result<()> {
    if documents {
        fs::create_dir_all(root)
            .with_context(|| format!("create project memory root {}", root.display()))?;
    }
    let store = Arc::new(Mutex::new(ProjectMemoryStore::new(root)));
    let client = history::Client::new(socket);
    let tools = definitions(documents, client.enabled());
    let reader = history::ShutdownReader::new(reader, client.clone());
    crate::mcp_stdio::serve(
        reader,
        writer,
        crate::mcp_stdio::McpServer {
            name: "mj-memory",
            instructions: if documents && client.enabled() {
                history::COMBINED_GUIDANCE
            } else if documents {
                MEMORY_GUIDANCE
            } else {
                history::GUIDANCE
            },
            tools,
            dispatch: crate::mcp_stdio::Dispatch::Concurrent,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            // Serialize document writes independently of remote history calls.
            call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                let name = params
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(name, "list" | "read" | "write") {
                    if !documents {
                        bail!("project documents use this harness's native memory");
                    }
                    call_tool(
                        &store.lock().expect("memory document lock poisoned"),
                        params,
                    )
                } else {
                    client.call(params.context("tools/call is missing params")?)
                }
            },
        },
    )
}

fn definitions(documents: bool, history: bool) -> Vec<Value> {
    let mut tools = if documents {
        tool_definitions()
    } else {
        Vec::new()
    };
    if history {
        tools.extend(history::tool_definitions());
    }
    tools
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn send(stream: &mut UnixStream, id: u64, method: &str, params: Value) {
        serde_json::to_writer(
            &mut *stream,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    }
    fn receive(stream: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn mcp_keeps_notes_responsive_and_serializes_writes_while_history_waits() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("history.sock");
        let listener = mj_core::local_sockets::bind_unix_listener(&socket).unwrap();
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let root = temp.path().join("notes");
        let serving = std::thread::spawn(move || {
            serve(
                &root,
                Some(socket),
                true,
                BufReader::new(server.try_clone().unwrap()),
                server,
            )
            .unwrap()
        });
        let mut output = BufReader::new(client.try_clone().unwrap());
        send(
            &mut client,
            1,
            "tools/call",
            json!({"name":"search_sessions","arguments":{"query":"needle"}}),
        );
        let (worker, _) = listener.accept().unwrap();
        worker
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut worker = BufReader::new(worker);
        let mut line = String::new();
        worker.read_line(&mut line).unwrap();
        let request: mj_core::relay::RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            request.request,
            mj_core::relay::RelayRequest::HistoryQuery { .. }
        ));
        // The remote call is deliberately unanswered while local notes proceed.
        for id in [2, 3] {
            send(
                &mut client,
                id,
                "tools/call",
                json!({"name":"write","arguments":{"path":"/decision.md","content":id.to_string(),"if_version":"new"}}),
            );
        }
        let mut outcomes = Vec::new();
        for _ in 0..2 {
            let reply = receive(&mut output);
            assert_ne!(reply["id"], 1);
            outcomes.push(
                reply["result"]["structuredContent"]["outcome"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        outcomes.sort();
        assert_eq!(outcomes, ["conflict", "ok"]);
        let value = json!({"text":"é🙂".repeat(30_000)});
        mj_core::relay::write_relay_frame(
            worker.get_mut(),
            &mj_core::relay::RelayResponseEnvelope {
                request_id: request.request_id,
                protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
                body: mj_core::relay::RelayResponseBody::Ok {
                    payload: mj_core::relay::RelayResponsePayload::HistoryResult {
                        result: mj_core::history::HistoryResult {
                            request_id: "history".into(),
                            value: value.clone(),
                            is_error: false,
                        },
                    },
                },
            },
        )
        .unwrap();
        let reply = receive(&mut output);
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["structuredContent"], value);
        // EOF cancels outstanding remote calls before joining call threads.
        send(
            &mut client,
            4,
            "tools/call",
            json!({"name":"trace_file","arguments":{"path":"a.rs"}}),
        );
        let (worker, _) = listener.accept().unwrap();
        worker
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut worker = BufReader::new(worker);
        line.clear();
        worker.read_line(&mut line).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        line.clear();
        assert_eq!(worker.read_line(&mut line).unwrap(), 0);
        assert_eq!(receive(&mut output)["result"]["isError"], true);
        serving.join().unwrap();
    }

    #[test]
    fn claude_history_mode_exposes_no_document_tools_or_note_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("must-not-be-created");
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let server_root = root.clone();
        let serving = std::thread::spawn(move || {
            serve(
                &server_root,
                Some("/missing.sock".into()),
                false,
                BufReader::new(server.try_clone().unwrap()),
                server,
            )
            .unwrap()
        });
        let mut output = BufReader::new(client.try_clone().unwrap());
        send(&mut client, 1, "tools/list", json!({}));
        let reply = receive(&mut output);
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "read_session"));
        assert!(!tools.iter().any(|tool| tool["name"] == "write"));
        send(
            &mut client,
            2,
            "tools/call",
            json!({"name":"write","arguments":{"path":"/MEMORY.md","content":"x","if_version":"new"}}),
        );
        assert!(receive(&mut output).get("error").is_some());
        client.shutdown(std::net::Shutdown::Write).unwrap();
        serving.join().unwrap();
        assert!(!root.exists());
    }
}
