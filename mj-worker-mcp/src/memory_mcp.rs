//! MCP server for project history search and transcript reading.
use anyhow::{Context, Result};
use serde_json::Value;
use std::io;
use std::path::PathBuf;

mod history;

/// Serve the history tools over MCP's JSON-lines stdio transport.
pub fn run_mcp_stdio(history_socket: Option<PathBuf>) -> Result<()> {
    serve(history_socket, io::stdin().lock(), io::stdout())
}

fn serve<R: io::BufRead, W: io::Write + Send + Sync + 'static>(
    history_socket: Option<PathBuf>,
    reader: R,
    writer: W,
) -> Result<()> {
    let client = history::Client::new(history_socket);
    let tools = definitions(client.enabled());
    let reader = history::ShutdownReader::new(reader, client.clone());
    crate::mcp_stdio::serve(
        reader,
        writer,
        crate::mcp_stdio::McpServer {
            instructions: None,
            name: "mj-memory",
            tools,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                client.call(params.context("tools/call is missing params")?)
            },
        },
    )
}

/// The tools the server lists when it has a history socket, which is what a
/// harness that asks before an MCP tool is allowed.
pub fn tool_names() -> Vec<String> {
    history::tool_definitions()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

fn definitions(history: bool) -> Vec<Value> {
    if history {
        history::tool_definitions()
    } else {
        Vec::new()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
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

    #[test]
    fn mcp_registers_history_tools_without_project_document_tools() {
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let serving = std::thread::spawn(move || {
            serve(
                Some("/missing.sock".into()),
                BufReader::new(server.try_clone().unwrap()),
                server,
            )
            .unwrap()
        });

        let mut output = BufReader::new(client.try_clone().unwrap());
        send(&mut client, 0, "initialize", json!({}));
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let initialized: Value = serde_json::from_str(&line).unwrap();
        assert!(initialized["result"].get("instructions").is_none());
        assert_eq!(initialized["result"]["serverInfo"]["name"], "mj-memory");
        line.clear();
        send(&mut client, 1, "tools/list", json!({}));
        output.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();

        for name in ["list", "read", "write"] {
            assert!(!names.contains(&name), "unexpected document tool {name}");
        }
        assert_eq!(names, ["search_history", "read_session"]);
        let read = &tools[1]["inputSchema"]["properties"];
        for key in ["tail", "before", "role", "max_message_chars"] {
            assert!(read.get(key).is_some(), "read_session must expose {key}");
        }
        assert_eq!(read["max_message_chars"]["minimum"], 0);
        assert_eq!(read["max_message_chars"]["maximum"], 64000);
        assert!(
            read["role"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("dialogue"))
        );
        let search = &tools[0]["inputSchema"]["properties"];
        assert!(
            search.get("session_id").is_none(),
            "alias must stay unadvertised"
        );
        assert_eq!(search["session_ids"]["maxItems"], 50);
        assert_eq!(search["limit"]["maximum"], 50);
        assert_eq!(search["context_messages"]["maximum"], 5);

        client.shutdown(std::net::Shutdown::Write).unwrap();
        serving.join().unwrap();
    }

    /// Serve one history request from a stand-in worker and return the query it
    /// received.
    fn forwarded_query(arguments: Value, name: &str) -> mj_core::history::HistoryQuery {
        use mj_core::relay::{
            RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponseEnvelope,
            RelayResponsePayload,
        };
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("history.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let envelope: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            let RelayRequest::HistoryQuery { query } = envelope.request else {
                panic!("expected a history query");
            };
            serde_json::to_writer(
                &mut stream,
                &RelayResponseEnvelope {
                    request_id: envelope.request_id,
                    protocol_version: envelope.protocol_version,
                    body: RelayResponseBody::Ok {
                        payload: RelayResponsePayload::HistoryResult {
                            result: mj_core::history::HistoryResult {
                                request_id: "r".into(),
                                value: json!({"ok": true}),
                                is_error: false,
                            },
                        },
                    },
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            query
        });
        let client = history::Client::new(Some(socket));
        let (value, is_error) = client
            .call(&json!({"name": name, "arguments": arguments}))
            .unwrap();
        assert!(!is_error, "{value}");
        worker.join().unwrap()
    }

    #[test]
    fn search_history_accepts_both_session_spellings_and_legacy_tools_still_dispatch() {
        use mj_core::history::{HistoryQuery, merged_search_history_session_ids};
        let HistoryQuery::SearchHistory {
            session_ids,
            session_id,
            limit,
            ..
        } = forwarded_query(
            json!({"query": "quokka", "session_ids": ["a", "b"], "session_id": "b"}),
            "search_history",
        )
        else {
            panic!("expected SearchHistory");
        };
        assert_eq!(limit, 20);
        assert_eq!(
            merged_search_history_session_ids(session_ids.as_deref(), session_id.as_deref()),
            ["a", "b"]
        );
        assert!(matches!(
            forwarded_query(json!({"query": "x"}), "search_sessions"),
            HistoryQuery::SearchSessions { .. }
        ));
        assert!(matches!(
            forwarded_query(json!({"session_id": "s", "query": "x"}), "search_session"),
            HistoryQuery::SearchSession { .. }
        ));
        // Retired from tools/list, but resumed transcripts can still call it.
        assert!(matches!(
            forwarded_query(json!({"session_id": "s"}), "get_session_brief"),
            HistoryQuery::GetSessionBrief { .. }
        ));
        let client = history::Client::new(None);
        for (name, arguments) in [
            ("trace_file", json!({"path": "src/lib.rs"})),
            ("session_files", json!({"session_id": "s"})),
            (
                "blame_file",
                json!({"path": "src/lib.rs", "start_line": 1, "end_line": 1}),
            ),
        ] {
            let error = client
                .call(&json!({"name": name, "arguments": arguments}))
                .unwrap_err();
            assert!(error.to_string().contains("unknown variant"), "{error}");
        }
    }
}
