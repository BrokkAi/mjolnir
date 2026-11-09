//! Session-owned native permissions for the JSON-configured harnesses: GitHub CLI
//! approvals and, for OpenCode, the tools Mjolnir turns off.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstNode, CstObject, CstRootNode},
};
use mj_core::config::{ExecutionPolicy, HarnessKind};

const OPENCODE_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
const GH_COMMANDS: [&str; 2] = ["gh", "gh *"];

/// Whether this harness gets session-owned rules in every execution policy,
/// not only under configured approvals.
pub(super) fn configures_native_rules_always(harness: HarnessKind) -> bool {
    harness == HarnessKind::OpenCode
}

/// What OpenCode must not offer the model, whatever the approval mode.
#[derive(Clone, Copy)]
pub(super) struct OpenCodeDenials {
    /// Its own `subagent` tool, when Mjolnir (or nobody) delegates.
    pub(super) native_subagent: bool,
}

pub(super) fn configure_json_gh_rules(
    harness: HarnessKind,
    policy: ExecutionPolicy,
    denials: OpenCodeDenials,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    if harness == HarnessKind::OpenCode {
        return configure_opencode(policy, denials, environment);
    }
    if policy != ExecutionPolicy::ConfiguredApprovals {
        return Ok(());
    }
    match harness {
        HarnessKind::Muse => {
            let mut prefixes: Vec<Vec<String>> = environment
                .get("MUSE_ACP_ALLOW_COMMAND_PREFIXES")
                .map(|value| serde_json::from_str(value))
                .transpose()
                .context("parse Muse native command prefixes")?
                .unwrap_or_default();
            ensure!(
                prefixes
                    .iter()
                    .all(|prefix| !prefix.is_empty() && prefix.iter().all(|word| !word.is_empty())),
                "Muse native command prefixes must contain nonempty argv words"
            );
            if !prefixes.iter().any(|prefix| prefix.as_slice() == ["gh"]) {
                prefixes.push(vec!["gh".into()]);
            }
            environment.insert(
                "MUSE_ACP_ALLOW_COMMAND_PREFIXES".into(),
                serde_json::to_string(&prefixes)?,
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

fn document(text: &str) -> Result<(CstRootNode, CstObject)> {
    let document = CstRootNode::parse(text, &ParseOptions::default())
        .context("parse native gh permission configuration")?;
    let object = document
        .object_value()
        .context("native gh permission configuration must be an object")?;
    Ok((document, object))
}

fn string_value(node: Option<CstNode>) -> Option<String> {
    node?.as_string_lit()?.decoded_value().ok()
}

pub(super) fn configure_opencode_plugin(
    directory: &std::path::Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let original = environment
        .get(OPENCODE_CONTENT)
        .map(String::as_str)
        .unwrap_or("{}");
    let (document, object) = document(original).context(OPENCODE_CONTENT)?;
    let plugins = object
        .array_value_or_create("plugins")
        .context("OPENCODE_CONFIG_CONTENT plugins must be an array")?;
    let directory = directory
        .to_str()
        .context("OpenCode plugin path is not UTF-8")?;
    if !plugins
        .elements()
        .iter()
        .any(|node| string_value(Some(node.clone())).as_deref() == Some(directory))
    {
        plugins.append(directory.into());
    }
    environment.insert(OPENCODE_CONTENT.into(), document.to_string());
    Ok(())
}

/// One native permission rule, as OpenCode 2.2.6 lists them in `permissions`.
type Rule = (&'static str, &'static str, &'static str);

/// The rules this launch appends after every rule the profile supplies, so
/// they win: OpenCode takes the last rule that matches.
fn opencode_rules(policy: ExecutionPolicy, denials: OpenCodeDenials) -> Vec<Rule> {
    let mut rules = Vec::new();
    if policy == ExecutionPolicy::ConfiguredApprovals {
        rules.extend(GH_COMMANDS.map(|command| ("bash", command, "allow")));
    }
    // With `execute` allowed, OpenCode 2.2.6 offers MCP tools only through a
    // JavaScript `execute` tool ("Code Mode"). Denying it lists the
    // `mj-agents` tools as direct `mj-agents_<tool>` tools, which the
    // delegation policy text and the owned-server approvals name.
    rules.push(("execute", "*", "deny"));
    if denials.native_subagent {
        rules.push(("subagent", "*", "deny"));
    }
    rules
}

fn configure_opencode(
    policy: ExecutionPolicy,
    denials: OpenCodeDenials,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let original = environment
        .get(OPENCODE_CONTENT)
        .map(String::as_str)
        .unwrap_or("{}");
    let (document, object) = document(original).context(OPENCODE_CONTENT)?;
    let rules = object
        .array_value_or_create("permissions")
        .context("OPENCODE_CONFIG_CONTENT permissions must be an array")?;
    // OpenCode 2.2.6 concatenates native permission arrays across config documents.
    // Keeping this as its own document preserves even scalar legacy defaults.
    let wanted = opencode_rules(policy, denials);
    let elements = rules.elements();
    let final_rules = elements.len() >= wanted.len()
        && elements[elements.len() - wanted.len()..]
            .iter()
            .zip(&wanted)
            .all(|(node, (action, resource, effect))| {
                node.as_object().is_some_and(|rule| {
                    [
                        ("action", action),
                        ("resource", resource),
                        ("effect", effect),
                    ]
                    .into_iter()
                    .all(|(key, expected)| {
                        string_value(rule.get(key).and_then(|p| p.value())).as_deref()
                            == Some(*expected)
                    })
                })
            });
    // An earlier identical rule may be shadowed by later restrictions.
    if !final_rules {
        for (action, resource, effect) in wanted {
            rules.append(CstInputValue::Object(vec![
                ("action".into(), action.into()),
                ("resource".into(), resource.into()),
                ("effect".into(), effect.into()),
            ]));
        }
    }
    let updated = document.to_string();
    if updated != original {
        environment.insert(OPENCODE_CONTENT.into(), updated);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(harness: HarnessKind, environment: &mut BTreeMap<String, String>) -> Result<()> {
        configure_json_gh_rules(
            harness,
            ExecutionPolicy::ConfiguredApprovals,
            OpenCodeDenials {
                native_subagent: false,
            },
            environment,
        )
    }

    #[test]
    fn opencode_native_overlay_preserves_jsonc_and_legacy_defaults() {
        let original = r#"{
  // retain the user's scalar default and native rule order
  "permission": "ask",
  "permissions": [
    {"action":"bash","resource":"gh *","effect":"allow"},
    {"action":"bash","resource":"*","effect":"deny"},
    {"action":"bash","resource":"rm *","effect":"deny"},
    {"action":"read","resource":".env","effect":"ask"},
  ],
  "providers": {"z":{}, "a":{}},
}"#;
        let mut environment = BTreeMap::from([(OPENCODE_CONTENT.into(), original.into())]);
        configure(HarnessKind::OpenCode, &mut environment).unwrap();
        let updated = environment[OPENCODE_CONTENT].clone();
        assert!(updated.contains("// retain the user's scalar default and native rule order"));
        assert!(updated.contains("\"permission\": \"ask\""));
        assert!(updated.contains("\"providers\": {\"z\":{}, \"a\":{}}"));
        let (_, object) = document(&updated).unwrap();
        let rules = object.array_value("permissions").unwrap().elements();
        let resources: Vec<_> = rules
            .iter()
            .map(|node| {
                string_value(node.as_object().unwrap().get("resource").unwrap().value()).unwrap()
            })
            .collect();
        // The gh allows come first, then the `execute` denial, which no
        // earlier user rule can override.
        assert_eq!(resources, ["gh *", "*", "rm *", ".env", "gh", "gh *", "*"]);
        let actions = rules
            .iter()
            .map(|node| {
                string_value(node.as_object().unwrap().get("action").unwrap().value()).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(actions[actions.len() - 3..], ["bash", "bash", "execute"]);
        configure(HarnessKind::OpenCode, &mut environment).unwrap();
        assert_eq!(environment[OPENCODE_CONTENT], updated);
    }

    #[test]
    fn opencode_denials_apply_in_every_approval_mode_and_stay_idempotent() {
        let rules = |environment: &BTreeMap<String, String>| -> Vec<(String, String)> {
            let (_, object) = document(&environment[OPENCODE_CONTENT]).unwrap();
            object
                .array_value("permissions")
                .unwrap()
                .elements()
                .iter()
                .map(|node| {
                    let rule = node.as_object().unwrap();
                    let get = |key| string_value(rule.get(key).unwrap().value()).unwrap();
                    (get("action"), get("effect"))
                })
                .collect()
        };
        for policy in [
            ExecutionPolicy::Unconstrained,
            ExecutionPolicy::ConfiguredApprovals,
        ] {
            for native_subagent in [false, true] {
                let denials = OpenCodeDenials { native_subagent };
                let mut environment = BTreeMap::new();
                configure_json_gh_rules(HarnessKind::OpenCode, policy, denials, &mut environment)
                    .unwrap();
                let first = environment[OPENCODE_CONTENT].clone();
                let mut expected = Vec::new();
                if policy == ExecutionPolicy::ConfiguredApprovals {
                    expected.extend([("bash", "allow"); 2]);
                }
                expected.push(("execute", "deny"));
                if native_subagent {
                    expected.push(("subagent", "deny"));
                }
                let expected: Vec<(String, String)> = expected
                    .into_iter()
                    .map(|(action, effect)| (action.into(), effect.into()))
                    .collect();
                assert_eq!(
                    rules(&environment),
                    expected,
                    "{policy:?} {native_subagent}"
                );
                configure_json_gh_rules(HarnessKind::OpenCode, policy, denials, &mut environment)
                    .unwrap();
                assert_eq!(environment[OPENCODE_CONTENT], first);
            }
        }
        // Other harnesses get nothing outside configured approvals.
        let mut environment = BTreeMap::new();
        configure_json_gh_rules(
            HarnessKind::Muse,
            ExecutionPolicy::Unconstrained,
            OpenCodeDenials {
                native_subagent: true,
            },
            &mut environment,
        )
        .unwrap();
        assert!(environment.is_empty());
    }

    /// Serve OpenCode's provider requests from this thread, recording the
    /// tool names each chat request offered the model.
    fn stub_provider(
        listener: std::net::TcpListener,
        offered: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let offered = offered.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let is_post = line.starts_with("POST");
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                if !is_post {
                    let models = r#"{"data":[{"id":"m","object":"model"}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{models}",
                        models.len()
                    )
                    .unwrap();
                    return;
                }
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let tools = request["tools"]
                    .as_array()
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                offered.lock().unwrap().push(tools);
                let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                    format!(
                        "data: {}\n\n",
                        serde_json::json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"m",
                            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
                    )
                };
                let events = [
                    chunk(
                        serde_json::json!({"role":"assistant","content":"stub ok"}),
                        serde_json::Value::Null,
                    ),
                    chunk(serde_json::json!({}), "stop".into()),
                    "data: [DONE]\n\n".to_owned(),
                ]
                .concat();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{events}",
                    events.len()
                );
            });
        }
    }

    /// The configuration this module builds, run through the pinned OpenCode:
    /// `execute` is gone so `mj-agents` tools are direct tools, and
    /// `subagent` is gone exactly when native delegation is suppressed. A
    /// stub provider records what the model is offered; nothing leaves the
    /// machine.
    #[test]
    #[ignore = "requires MJ_OPENCODE_TEST_BINARY naming OpenCode 2.2.6 and MJ_WORKER_BINARY pointing to the current mj-worker build"]
    fn live_opencode_offers_mj_agents_directly_and_drops_denied_tools() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;
        let opencode = std::env::var_os("MJ_OPENCODE_TEST_BINARY").expect("set OpenCode");
        let worker = std::env::var_os("MJ_WORKER_BINARY").expect("set mj-worker");
        for native_subagent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let work = root.path().join("work");
            std::fs::create_dir_all(&work).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let offered = Arc::new(Mutex::new(Vec::new()));
            let served = offered.clone();
            std::thread::spawn(move || stub_provider(listener, served));
            let mut environment = BTreeMap::from([(
                OPENCODE_CONTENT.to_owned(),
                serde_json::json!({
                    "provider": {"stub": {"npm": "@ai-sdk/openai-compatible", "name": "stub",
                        "options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "x"},
                        "models": {"m": {"name": "m", "tool_call": true,
                            "limit": {"context": 100000, "output": 4000}}}}},
                    "model": "stub/m", "small_model": "stub/m", "autoupdate": false
                })
                .to_string(),
            )]);
            configure_json_gh_rules(
                HarnessKind::OpenCode,
                ExecutionPolicy::Unconstrained,
                OpenCodeDenials { native_subagent },
                &mut environment,
            )
            .unwrap();
            let mut child = Command::new(&opencode)
                .arg("acp")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root.path().join("home"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env(OPENCODE_CONTENT, &environment[OPENCODE_CONTENT])
                .current_dir(&work)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let (lines, replies) = mpsc::channel();
            let stdout = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) {
                        let _ = lines.send(message);
                    }
                }
            });
            let mut next_id = 0;
            let mut call = |method: &str, params: serde_json::Value| -> serde_json::Value {
                next_id += 1;
                let request = serde_json::json!({"jsonrpc":"2.0","id":next_id,"method":method,"params":params});
                writeln!(stdin, "{request}").unwrap();
                loop {
                    let message = replies.recv_timeout(Duration::from_secs(60)).expect(method);
                    if message["id"] == next_id && message.get("method").is_none() {
                        assert!(message.get("error").is_none(), "{method}: {message}");
                        return message["result"].clone();
                    }
                }
            };
            call(
                "initialize",
                serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            );
            let socket = root.path().join("subagents.sock");
            let session = call(
                "session/new",
                serde_json::json!({"cwd": work, "mcpServers": [{
                    "name": "mj-agents", "command": worker.to_string_lossy(),
                    "args": ["worker", "subagent-mcp", "--socket", socket, "--harness", HarnessKind::OpenCode.id(), "--role", "parent"],
                    "env": []}]}),
            );
            let session_id = session["sessionId"].clone();
            // The only prompt, sent right after `session/new`, must already
            // list the connected server's tools. A title request may precede
            // it and offer no tools.
            call(
                "session/prompt",
                serde_json::json!({"sessionId": session_id, "prompt": [{"type":"text","text":"hello"}]}),
            );
            let tools = offered
                .lock()
                .unwrap()
                .iter()
                .find(|tools| tools.iter().any(|tool| tool == "mj-agents_spawn"))
                .cloned()
                .unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            assert!(
                tools.iter().any(|tool| tool == "mj-agents_list_agents"),
                "{tools:?} of {:?}",
                offered.lock().unwrap()
            );
            assert!(!tools.iter().any(|tool| tool == "execute"), "{tools:?}");
            assert_eq!(
                tools.iter().any(|tool| tool == "subagent"),
                !native_subagent,
                "{tools:?}"
            );
        }
    }
}
