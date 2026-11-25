//! Stdio bridge for the worker's selective utility tools.
use anyhow::{Context, Result};
use mj_core::claude_mcp::{
    CLAUDE_MCP_SERVER, ClaudeMcpCapabilities, ClaudeMcpRpcRequest, ClaudeMcpRpcResponse,
};
use mj_core::config::HarnessKind;
use serde_json::{Value, json};
use std::{io, path::Path};

pub fn serve_stdio(socket: &Path, harness: HarnessKind) -> Result<()> {
    serve(socket, harness, io::stdin().lock(), io::stdout())
}

fn serve<R: io::BufRead, W: io::Write + Send + Sync + 'static>(
    socket: &Path,
    harness: HarnessKind,
    reader: R,
    writer: W,
) -> Result<()> {
    let capabilities = ClaudeMcpCapabilities::for_harness(harness);
    let names = capabilities.tool_names();
    let instructions = format!(
        "Worker utility tools: {}. Background notifications are delivered to this session.",
        names.join(", ")
    );
    let socket = socket.to_path_buf();
    crate::mcp_stdio::serve(
        reader,
        writer,
        crate::mcp_stdio::McpServer {
            name: CLAUDE_MCP_SERVER,
            instructions: Some(instructions),
            tools: names.into_iter().map(definition).collect(),
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                let result = (|| {
                    let params = params.context("tools/call is missing params")?;
                    let name = params["name"].as_str().context("tool name is missing")?;
                    anyhow::ensure!(
                        capabilities.supports_tool(name),
                        "tool {name} is not enabled for {}",
                        harness.id()
                    );
                    request(
                        &socket,
                        &ClaudeMcpRpcRequest {
                            tool: name.into(),
                            arguments: params
                                .get("arguments")
                                .cloned()
                                .unwrap_or_else(|| json!({})),
                        },
                    )
                })();
                Ok(match result {
                    Ok(response) if response.ok => (response.result.unwrap_or(Value::Null), false),
                    Ok(response) => (
                        json!({"error": response.error.unwrap_or_else(|| "worker tool failed".into())}),
                        true,
                    ),
                    Err(error) => (json!({"error": format!("{error:#}")}), true),
                })
            },
        },
    )
}

#[cfg(unix)]
fn request(socket: &Path, request: &ClaudeMcpRpcRequest) -> Result<ClaudeMcpRpcResponse> {
    // Web fetch has a 30s HTTP deadline; allow its response to arrive. Control
    // calls should answer immediately and get a shorter stalled-peer deadline.
    let seconds = if request.tool == "web_fetch" { 60 } else { 15 };
    crate::mcp_stdio::socket_request(
        socket,
        request,
        "mj-claude",
        std::time::Duration::from_secs(seconds),
    )?
    .context("mj-claude worker reply timed out")
}

#[cfg(not(unix))]
fn request(_: &Path, _: &ClaudeMcpRpcRequest) -> Result<ClaudeMcpRpcResponse> {
    anyhow::bail!("mj-claude worker sockets require Unix")
}

fn definition(name: &str) -> Value {
    let (description, properties, required) = match name {
        "web_fetch" => (
            "Fetch an HTTP(S) URL as Markdown, text, or raw content with character paging.",
            json!({
                "url":{"type":"string","description":"HTTP or HTTPS URL to read."},
                "format":{"type":"string","enum":["markdown","text","raw"],"default":"markdown","description":"Output representation."},
                "offset":{"type":"integer","minimum":0,"default":0,"description":"Starting character offset."},
                "max_chars":{"type":"integer","minimum":1,"default":32768,"description":"Maximum characters returned."}
            }),
            vec!["url"],
        ),
        "monitor" => (
            "Run a background shell command and deliver stdout as session wake notifications.",
            json!({
                "description":{"type":"string","description":"Short purpose of this watch."},
                "command":{"type":"string","description":"Shell command to run in the session directory."},
                "timeout_ms":{"type":"integer","minimum":0,"default":300000,"description":"Lifetime in milliseconds; zero runs until stopped."},
                "persistent":{"type":"boolean","default":false,"description":"Run until stopped or session end, ignoring timeout_ms."}
            }),
            vec!["description", "command"],
        ),
        "monitor_stop" => (
            "Stop a monitor and its process group.",
            json!({"task_id":{"type":"string","description":"Monitor task identifier."}}),
            vec!["task_id"],
        ),
        "monitor_list" => ("List active monitors.", json!({}), vec![]),
        "cron_create" => (
            "Schedule a local-time prompt; recurring jobs expire after seven days.",
            json!({
                "cron":{"type":"string","description":"Five local-time fields: minute hour day-of-month month day-of-week."},
                "prompt":{"type":"string","description":"Prompt to deliver when the job fires."},
                "recurring":{"type":"boolean","default":true,"description":"Repeat after firing; false creates a one-shot job."}
            }),
            vec!["cron", "prompt"],
        ),
        "cron_list" => (
            "List cron jobs and their next local fire times.",
            json!({}),
            vec![],
        ),
        "cron_delete" => (
            "Delete a cron job.",
            json!({"id":{"type":"string","description":"Cron job identifier."}}),
            vec!["id"],
        ),
        "schedule_wakeup" => (
            "Replace the pending dynamic wakeup, or cancel it with stop; leaves cron jobs intact.",
            json!({
                "delay_seconds":{"type":"integer","description":"Delay clamped to 60..=3600 seconds; required unless stop is true."},
                "prompt":{"type":"string","description":"Wake prompt; required unless stop is true."},
                "reason":{"type":"string","description":"Reason for waking; required unless stop is true."},
                "stop":{"type":"boolean","default":false,"description":"Cancel the pending dynamic wakeup."},
                "noop":{"type":"boolean","default":false,"description":"Record whether the current loop check found no change to report (metadata only; the wakeup still fires)."}
            }),
            vec![],
        ),
        _ => unreachable!("capability registry returned unknown tool"),
    };
    let schema = json!({"type":"object","properties":properties,"required":required});
    json!({"name":name,"description":description,"inputSchema":schema})
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;

    fn exchange(socket: &Path, harness: HarnessKind, method: &str, params: Value) -> Value {
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let socket = socket.to_path_buf();
        let thread = std::thread::spawn(move || {
            serve(
                &socket,
                harness,
                BufReader::new(server.try_clone().unwrap()),
                server,
            )
            .unwrap()
        });
        serde_json::to_writer(
            &mut client,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap();
        client.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        thread.join().unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn claude_mcp_stdio_selectively_advertises_tools_and_instructions() {
        use HarnessKind::*;
        for (harness, expected) in [
            (
                Codex,
                vec![
                    "web_fetch",
                    "monitor",
                    "monitor_stop",
                    "monitor_list",
                    "cron_create",
                    "cron_list",
                    "cron_delete",
                    "schedule_wakeup",
                ],
            ),
            (
                Antigravity,
                vec!["web_fetch", "monitor", "monitor_stop", "monitor_list"],
            ),
            (
                Kimi,
                vec!["monitor", "monitor_stop", "monitor_list", "schedule_wakeup"],
            ),
            (Muse, vec!["schedule_wakeup"]),
            (Claude, vec![]),
            (
                OpenCode,
                vec![
                    "monitor",
                    "monitor_stop",
                    "monitor_list",
                    "cron_create",
                    "cron_list",
                    "cron_delete",
                    "schedule_wakeup",
                ],
            ),
            (
                Dsh,
                vec![
                    "monitor",
                    "monitor_stop",
                    "monitor_list",
                    "cron_create",
                    "cron_list",
                    "cron_delete",
                    "schedule_wakeup",
                ],
            ),
            (
                Grok,
                vec![
                    "monitor",
                    "monitor_stop",
                    "monitor_list",
                    "cron_create",
                    "cron_list",
                    "cron_delete",
                    "schedule_wakeup",
                ],
            ),
        ] {
            let reply = exchange(Path::new("/missing.sock"), harness, "tools/list", json!({}));
            let actual: Vec<_> = reply["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect();
            assert_eq!(actual, expected);
            let init = exchange(
                Path::new("/missing.sock"),
                harness,
                "initialize",
                json!({"protocolVersion":"2025-03-26"}),
            );
            assert_eq!(init["result"]["serverInfo"]["name"], "mj-claude");
            assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
            let instructions = init["result"]["instructions"].as_str().unwrap();
            for name in ["web_fetch", "monitor", "cron_create", "schedule_wakeup"] {
                assert_eq!(instructions.contains(name), expected.contains(&name));
            }
        }
    }

    #[test]
    fn claude_mcp_stdio_round_trips_worker_results_and_refuses_disabled_tools() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("claude.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        for (harness, name) in [
            (HarnessKind::Kimi, "web_fetch"),
            (HarnessKind::Antigravity, "cron_create"),
        ] {
            let reply = exchange(
                &socket,
                harness,
                "tools/call",
                json!({"name":name,"arguments":{}}),
            );
            assert_eq!(reply["result"]["isError"], true);
            assert!(
                reply["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("not enabled")
            );
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            listener.set_nonblocking(false).unwrap();
        }
        let worker = std::thread::spawn(move || {
            for ok in [true, false] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: ClaudeMcpRpcRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(request.tool, "web_fetch");
                assert_eq!(request.arguments, json!({"url":"https://example.com"}));
                let response = ClaudeMcpRpcResponse {
                    ok,
                    result: ok.then(|| json!({"content":"x".repeat(128*1024)})),
                    error: (!ok).then(|| "fetch failed".into()),
                };
                serde_json::to_writer(&mut stream, &response).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        for ok in [true, false] {
            let reply = exchange(
                &socket,
                HarnessKind::Codex,
                "tools/call",
                json!({"name":"web_fetch","arguments":{"url":"https://example.com"}}),
            );
            assert_eq!(reply["result"]["isError"], !ok);
            let text = reply["result"]["content"][0]["text"].as_str().unwrap();
            let content: Value = serde_json::from_str(text).unwrap();
            if ok {
                assert_eq!(content["content"].as_str().unwrap().len(), 128 * 1024);
            } else {
                assert_eq!(content["error"], "fetch failed");
            }
        }
        worker.join().unwrap();
    }
}
