//! MCP server for project history search and code provenance.
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
            name: "mj-memory",
            instructions: history::GUIDANCE,
            tools,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                client.call(params.context("tools/call is missing params")?)
            },
        },
    )
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
        send(&mut client, 1, "tools/list", json!({}));
        let mut line = String::new();
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
        for name in [
            "search_sessions",
            "get_session_brief",
            "search_session",
            "read_session",
            "trace_file",
            "session_files",
            "blame_file",
        ] {
            assert!(names.contains(&name), "history tool {name} is missing");
        }

        client.shutdown(std::net::Shutdown::Write).unwrap();
        serving.join().unwrap();
    }
}
