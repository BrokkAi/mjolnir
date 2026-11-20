//! Session-owned native permissions for OpenCode: the GitHub CLI approval it
//! reads last among its configuration documents, the tools Mjolnir turns
//! off, and the Mjolnir guardian's permission hook.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstNode, CstObject, CstRootNode},
};
use mj_core::config::ExecutionPolicy;

const OPENCODE_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
const GH_COMMANDS: [&str; 2] = ["gh", "gh *"];

/// What OpenCode must not offer the model, whatever the approval mode.
#[derive(Clone, Copy)]
pub(super) struct OpenCodeDenials {
    /// Its own `subagent` tool, when Mjolnir (or nobody) delegates.
    pub(super) native_subagent: bool,
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

/// One native permission rule, as OpenCode 2.2.7 lists them in `permissions`.
type Rule = (&'static str, &'static str, &'static str);

/// OpenCode tools that run without consulting its permissions at all, so
/// neither its rules nor the guardian's hook would ever see them. Under
/// configured approvals they are denied; OpenCode then does not offer them.
/// `worktree_remove` consults only `edit`, which an inside-the-workspace
/// removal passes, so it goes too. Mjolnir sessions use none of them.
const PERMISSIONLESS_TOOLS: [&str; 5] = [
    "worktree_create",
    "worktree_remove",
    "session_move",
    "artifact_create",
    "schedule_create",
];

/// The guardian's OpenCode plugin, one directory per mode. Only the session
/// one is registered for the session; the reviewer's environment swaps in the
/// other (see [`opencode_reviewer_environment`]).
const GUARDIAN_PLUGIN: &str = "opencode-guardian";
const GUARDIAN_REVIEWER_PLUGIN: &str = "opencode-guardian-reviewer";

/// Stage the Mjolnir guardian's OpenCode plugin and register its session mode.
///
/// OpenCode 2.2.7's default agent allows every tool (`{"action": "*",
/// "resource": "*", "effect": "allow"}` in `packages/schema/src/agent.ts`),
/// so under configured approvals nothing would ever ask and the guardian
/// (`HarnessKind::guardian`) would review nothing. Rules cannot fix that
/// without overriding denies: OpenCode takes the last matching rule, and a
/// document Mjolnir supplies last would overrule Plan mode's edit deny, its
/// read-only agents and the person's own deny rules. The plugin's
/// `permission.evaluate` hook runs after every rule instead, and only turns
/// an allowed action that can reach outside the workspace into an ask.
/// `roots` is the session's working directory followed by its attached
/// directories.
pub(super) fn configure_opencode_guardian(
    root: &std::path::Path,
    roots: &[std::path::PathBuf],
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    let roots = roots
        .iter()
        .map(|root| root.to_str().context("OpenCode guardian root is not UTF-8"))
        .collect::<Result<Vec<_>>>()?;
    let roots = serde_json::to_string(&roots)?;
    for (directory, mode) in [
        (GUARDIAN_PLUGIN, "session"),
        (GUARDIAN_REVIEWER_PLUGIN, "reviewer"),
    ] {
        let directory = root.join(directory);
        std::fs::create_dir_all(&directory).with_context(|| {
            format!(
                "create OpenCode guardian plugin directory {}",
                directory.display()
            )
        })?;
        let script = include_str!("../../assets/opencode-guardian.js")
            .replace("__MJ_GUARDIAN_MODE__", &serde_json::to_string(mode)?)
            .replace("__MJ_GUARDIAN_ROOTS__", &roots);
        let path = directory.join("index.js");
        std::fs::write(&path, script)
            .with_context(|| format!("write OpenCode guardian hook {}", path.display()))?;
        std::fs::write(
            directory.join("package.json"),
            format!(r#"{{"name":"mjolnir-guardian-{mode}","type":"module","main":"index.js"}}"#),
        )
        .context("write OpenCode guardian plugin manifest")?;
    }
    configure_opencode_plugin(&root.join(GUARDIAN_PLUGIN), environment)
}

/// The rules a guardian reviewer's OpenCode runs under, appended last so they
/// win: it may read, and nothing else. Denied tools are not even offered.
/// The reviewer's hook denies the rest whatever an earlier per-agent rule
/// says, since those follow these.
const REVIEWER_RULES: [Rule; 8] = [
    ("*", "*", "deny"),
    ("read", "*", "allow"),
    ("read", "*.env", "deny"),
    ("read", "*.env.*", "deny"),
    ("read", "*.env.example", "allow"),
    ("glob", "*", "allow"),
    ("grep", "*", "allow"),
    ("external_directory", "*", "allow"),
];

/// The environment a guardian reviewer's OpenCode starts with: the session's,
/// with every plugin Mjolnir registered for the session replaced by the
/// reviewer's hook, so the reviewer never drains the session's mailbox or
/// inherits its hooks, and with [`REVIEWER_RULES`] appended. A session
/// launched without the guardian's plugin cannot have a contained reviewer,
/// so that is an error.
pub(crate) fn opencode_reviewer_environment(
    environment: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let original = environment
        .get(OPENCODE_CONTENT)
        .context("this OpenCode session has no Mjolnir configuration document")?;
    let (document, object) = document(original).context(OPENCODE_CONTENT)?;
    let plugins = object
        .array_value("plugins")
        .context("this OpenCode session registers no Mjolnir plugins")?;
    let session_hook = plugins
        .elements()
        .iter()
        .filter_map(|node| string_value(Some(node.clone())))
        .find(|entry| std::path::Path::new(entry).file_name() == Some(GUARDIAN_PLUGIN.as_ref()))
        .context("this OpenCode session was launched without the guardian's plugin; restart it")?;
    let reviewer_hook = std::path::Path::new(&session_hook)
        .with_file_name(GUARDIAN_REVIEWER_PLUGIN)
        .to_str()
        .context("OpenCode guardian plugin path is not UTF-8")?
        .to_owned();
    for node in plugins.elements() {
        node.remove();
    }
    plugins.append(reviewer_hook.as_str().into());
    let rules = object
        .array_value_or_create("permissions")
        .context("OPENCODE_CONFIG_CONTENT permissions must be an array")?;
    for (action, resource, effect) in REVIEWER_RULES {
        rules.append(CstInputValue::Object(vec![
            ("action".into(), action.into()),
            ("resource".into(), resource.into()),
            ("effect".into(), effect.into()),
        ]));
    }
    let mut environment = environment.clone();
    environment.insert(OPENCODE_CONTENT.into(), document.to_string());
    Ok(environment)
}

/// The rules this launch appends after every rule a document already supplies,
/// so they win: OpenCode takes the last rule that matches, and a project
/// document or a profile's own `OPENCODE_CONFIG_CONTENT` follows the staged
/// file. Both the `gh` approval and the denials are one suffix, so one
/// idempotence check covers them.
fn opencode_rules(policy: ExecutionPolicy, denials: OpenCodeDenials) -> Vec<Rule> {
    let mut rules = Vec::new();
    if policy == ExecutionPolicy::ConfiguredApprovals {
        // OpenCode 2.2.7 names its shell tool's action `shell`; only its
        // legacy `permission` object still maps the old `bash` key onto it.
        rules.extend(GH_COMMANDS.map(|command| ("shell", command, "allow")));
        rules.extend(PERMISSIONLESS_TOOLS.map(|tool| (tool, "*", "deny")));
    }
    // With `execute` allowed, OpenCode 2.2.7 offers MCP tools only through a
    // JavaScript `execute` tool ("Code Mode"). Denying it lists the
    // `mj-agents` tools as direct `mj-agents_<tool>` tools, which the
    // delegation policy text and the owned-server approvals name.
    rules.push(("execute", "*", "deny"));
    if denials.native_subagent {
        rules.push(("subagent", "*", "deny"));
    }
    rules
}

/// Append `wanted` after the rules the document already supplies, unless a
/// previous launch already appended exactly them. OpenCode takes the last
/// rule that matches, so an earlier identical rule may be shadowed by later
/// restrictions.
fn append_rules(rules: &jsonc_parser::cst::CstArray, wanted: &[Rule]) {
    let elements = rules.elements();
    let final_rules = elements.len() >= wanted.len()
        && elements[elements.len() - wanted.len()..]
            .iter()
            .zip(wanted)
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
    if final_rules {
        return;
    }
    for (action, resource, effect) in wanted {
        rules.append(CstInputValue::Object(vec![
            ("action".into(), (*action).into()),
            ("resource".into(), (*resource).into()),
            ("effect".into(), (*effect).into()),
        ]));
    }
}

/// Add this launch's `gh` approval, and the tools Mjolnir turns off, to
/// `OPENCODE_CONFIG_CONTENT`.
///
/// OpenCode merges its configuration documents in order and takes the last
/// matching permission rule. The staged `opencode.json` is read first, and a
/// project's own `opencode.json` or a profile's `OPENCODE_CONFIG_CONTENT`
/// follows it, so only the document Mjolnir supplies there is guaranteed to
/// win. The staged file keeps everything else the profile configured.
pub(super) fn configure_opencode_rules(
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
    // OpenCode 2.2.7 concatenates native permission arrays across config documents.
    // Keeping this as its own document preserves even scalar legacy defaults.
    append_rules(&rules, &opencode_rules(policy, denials));
    let updated = document.to_string();
    if updated != original {
        environment.insert(OPENCODE_CONTENT.into(), updated);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(
        policy: ExecutionPolicy,
        denials: OpenCodeDenials,
        environment: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        configure_opencode_rules(policy, denials, environment)
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
        configure(
            ExecutionPolicy::ConfiguredApprovals,
            OpenCodeDenials {
                native_subagent: false,
            },
            &mut environment,
        )
        .unwrap();
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
        // The `gh` allows come last, after every rule this document supplies,
        // so an earlier blanket deny cannot shadow them; the denials follow
        // them and cover only their own tools.
        assert_eq!(
            resources,
            [
                "gh *", "*", "rm *", ".env", "gh", "gh *", "*", "*", "*", "*", "*", "*"
            ]
        );
        let actions = rules
            .iter()
            .map(|node| {
                string_value(node.as_object().unwrap().get("action").unwrap().value()).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actions[actions.len() - 8..],
            [
                "shell",
                "shell",
                "worktree_create",
                "worktree_remove",
                "session_move",
                "artifact_create",
                "schedule_create",
                "execute"
            ]
        );
        configure(
            ExecutionPolicy::ConfiguredApprovals,
            OpenCodeDenials {
                native_subagent: false,
            },
            &mut environment,
        )
        .unwrap();
        assert_eq!(environment[OPENCODE_CONTENT], updated);
    }

    /// Both approval modes keep the denials, and a guardian launch also keeps
    /// the `gh` allows, in one suffix that never grows on a repeat launch.
    #[test]
    fn opencode_rules_apply_in_every_approval_mode_and_stay_idempotent() {
        // The table names the document OpenCode accepts the approval in; the
        // writer must supply that same one.
        assert_eq!(
            mj_core::config::HarnessKind::OpenCode.gh_approval(),
            mj_core::config::GhApproval::SessionConfigDocument(OPENCODE_CONTENT)
        );
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
                configure(policy, denials, &mut environment).unwrap();
                let first = environment[OPENCODE_CONTENT].clone();
                let mut expected = Vec::new();
                if policy == ExecutionPolicy::ConfiguredApprovals {
                    expected.extend([
                        ("shell", "allow"),
                        ("shell", "allow"),
                        ("worktree_create", "deny"),
                        ("worktree_remove", "deny"),
                        ("session_move", "deny"),
                        ("artifact_create", "deny"),
                        ("schedule_create", "deny"),
                    ]);
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
                configure(policy, denials, &mut environment).unwrap();
                assert_eq!(environment[OPENCODE_CONTENT], first);
            }
        }
    }

    /// A profile may supply its own `OPENCODE_CONFIG_CONTENT` with a blanket
    /// ask. OpenCode takes the last matching rule, so the managed allows must
    /// follow it; the profile's own rule and text stay.
    #[test]
    fn opencode_gh_approval_follows_a_profiles_own_later_rules() {
        let original = r#"{
  // keep the person's rule order
  "permissions": [
    {"action":"bash","resource":"*","effect":"ask"},
    {"action":"read","resource":".env","effect":"ask"},
  ],
  "providers": {"z":{}, "a":{}},
}"#;
        let mut environment = BTreeMap::from([(OPENCODE_CONTENT.into(), original.into())]);
        configure(
            ExecutionPolicy::ConfiguredApprovals,
            OpenCodeDenials {
                native_subagent: false,
            },
            &mut environment,
        )
        .unwrap();
        let updated = environment[OPENCODE_CONTENT].clone();
        assert!(updated.contains("// keep the person's rule order"));
        assert!(updated.contains("\"providers\": {\"z\":{}, \"a\":{}}"));
        let (_, object) = document(&updated).unwrap();
        let rules = object.array_value("permissions").unwrap().elements();
        let described: Vec<_> = rules
            .iter()
            .map(|node| {
                let rule = node.as_object().unwrap();
                let get = |key| string_value(rule.get(key).unwrap().value()).unwrap();
                (get("resource"), get("effect"))
            })
            .collect();
        let expected = [
            ("*", "ask"),
            (".env", "ask"),
            ("gh", "allow"),
            ("gh *", "allow"),
            ("*", "deny"),
            ("*", "deny"),
            ("*", "deny"),
            ("*", "deny"),
            ("*", "deny"),
            ("*", "deny"),
        ];
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(resource, effect)| (resource.to_owned(), effect.to_owned()))
            .collect();
        assert_eq!(described, expected);
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
    #[ignore = "requires MJ_OPENCODE_TEST_BINARY naming OpenCode 2.2.7 and MJ_WORKER_BINARY pointing to the current mj-worker build"]
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
            configure_opencode_rules(
                mj_core::config::ExecutionPolicy::Unconstrained,
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
                    "args": ["worker", "subagent-mcp", "--socket", socket, "--harness", mj_core::config::HarnessKind::OpenCode.id(), "--role", "parent"],
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

    /// Serve OpenCode's provider requests from a script: the last user
    /// message names a script, and each tool result so far advances it by one
    /// call. Records the tools each request offered.
    fn scripted_provider(
        listener: std::net::TcpListener,
        scripts: std::sync::Arc<BTreeMap<&'static str, Vec<(&'static str, serde_json::Value)>>>,
        offered: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let scripts = scripts.clone();
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
                let tools: Vec<String> = request["tools"]
                    .as_array()
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let messages = request["messages"].as_array().cloned().unwrap_or_default();
                let user = messages
                    .iter()
                    .rposition(|message| message["role"] == "user")
                    .unwrap_or(0);
                let text = messages
                    .get(user)
                    .map(|message| message["content"].to_string())
                    .unwrap_or_default();
                let step = messages[user..]
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .count();
                let call = (!tools.is_empty())
                    .then(|| scripts.iter().find(|(name, _)| text.contains(*name)))
                    .flatten()
                    .and_then(|(_, calls)| calls.get(step).cloned());
                offered.lock().unwrap().push(tools);
                let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                    format!(
                        "data: {}\n\n",
                        serde_json::json!({"id":"c","object":"chat.completion.chunk","created":1,"model":"m",
                            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
                    )
                };
                let events = match call {
                    Some((name, arguments)) => vec![
                        chunk(
                            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_{step}"),
                                "type":"function","function":{"name":name,"arguments":arguments.to_string()}}]}),
                            serde_json::Value::Null,
                        ),
                        chunk(serde_json::json!({}), "tool_calls".into()),
                        "data: [DONE]\n\n".to_owned(),
                    ],
                    None => vec![
                        chunk(
                            serde_json::json!({"role":"assistant","content":"done"}),
                            serde_json::Value::Null,
                        ),
                        chunk(serde_json::json!({}), "stop".into()),
                        "data: [DONE]\n\n".to_owned(),
                    ],
                };
                let body = events.concat();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
        }
    }

    /// A real OpenCode 2.2.7 ACP process driven over stdio, answering every
    /// permission request with `answer` and recording it.
    struct LiveOpenCode {
        child: std::process::Child,
        stdin: std::process::ChildStdin,
        replies: std::sync::mpsc::Receiver<serde_json::Value>,
        next_id: u64,
        asked: Vec<serde_json::Value>,
    }

    impl LiveOpenCode {
        fn start(root: &std::path::Path, work: &std::path::Path, content: &str) -> Self {
            use std::io::BufRead;
            use std::process::{Command, Stdio};
            let opencode = std::env::var_os("MJ_OPENCODE_TEST_BINARY").expect("set OpenCode");
            let mut child = Command::new(opencode)
                .arg("acp")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root.join("home"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("XDG_STATE_HOME", root.join("state"))
                .env(OPENCODE_CONTENT, content)
                .current_dir(work)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let (lines, replies) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) {
                        let _ = lines.send(message);
                    }
                }
            });
            Self {
                child,
                stdin,
                replies,
                next_id: 0,
                asked: Vec::new(),
            }
        }

        fn call(
            &mut self,
            method: &str,
            params: serde_json::Value,
            answer: &str,
        ) -> serde_json::Value {
            use std::io::Write;
            self.next_id += 1;
            let id = self.next_id;
            let request =
                serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            writeln!(self.stdin, "{request}").unwrap();
            loop {
                let message = self
                    .replies
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect(method);
                if message["method"] == "session/request_permission" {
                    self.asked.push(message["params"]["toolCall"].clone());
                    let reply = serde_json::json!({"jsonrpc":"2.0","id":message["id"],
                        "result":{"outcome":{"outcome":"selected","optionId":answer}}});
                    writeln!(self.stdin, "{reply}").unwrap();
                } else if message["id"] == id && message.get("method").is_none() {
                    assert!(message.get("error").is_none(), "{method}: {message}");
                    return message["result"].clone();
                }
            }
        }
    }

    impl Drop for LiveOpenCode {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    // Hard-won: ef74ec25c: Mjolnir's appended OpenCode rules overrode Plan mode's and the person's denies.
    /// Under configured approvals the guardian's hook makes the pinned OpenCode
    /// ask about what can reach outside the workspace and nothing else, while
    /// every deny stands: the person's own rule and Plan mode's. The reviewer's
    /// launch may only read, and drops the session's other plugins.
    #[test]
    #[ignore = "requires MJ_OPENCODE_TEST_BINARY naming OpenCode 2.2.7"]
    fn live_opencode_guardian_hook_asks_only_beyond_the_workspace_and_keeps_denies() {
        use std::sync::{Arc, Mutex};
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        std::fs::create_dir_all(work.join(".git/hooks")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let scripts = Arc::new(BTreeMap::from([
            (
                "BUILD",
                vec![
                    (
                        "shell",
                        serde_json::json!({"command": "touch by-shell.txt"}),
                    ),
                    (
                        "write",
                        serde_json::json!({"path": "notes.txt", "content": "notes"}),
                    ),
                    (
                        "write",
                        serde_json::json!({"path": ".git/hooks/post-checkout", "content": "#!/bin/sh"}),
                    ),
                    ("shell", serde_json::json!({"command": "rm notes.txt"})),
                    ("read", serde_json::json!({"path": "notes.txt"})),
                ],
            ),
            (
                "PLAN",
                vec![(
                    "write",
                    serde_json::json!({"path": "by-plan.txt", "content": "plan"}),
                )],
            ),
            (
                "REVIEW",
                vec![
                    (
                        "shell",
                        serde_json::json!({"command": "touch by-reviewer.txt"}),
                    ),
                    (
                        "write",
                        serde_json::json!({"path": "by-reviewer.txt", "content": "x"}),
                    ),
                    ("read", serde_json::json!({"path": "notes.txt"})),
                ],
            ),
        ]));
        let offered = Arc::new(Mutex::new(Vec::new()));
        let served = offered.clone();
        std::thread::spawn(move || scripted_provider(listener, scripts, served));
        let mut environment = BTreeMap::from([(
            OPENCODE_CONTENT.to_owned(),
            serde_json::json!({
                "provider": {"stub": {"npm": "@ai-sdk/openai-compatible", "name": "stub",
                    "options": {"baseURL": format!("http://127.0.0.1:{port}/v1"), "apiKey": "x"},
                    "models": {"m": {"name": "m", "tool_call": true,
                        "limit": {"context": 100000, "output": 4000}}}}},
                "model": "stub/m", "small_model": "stub/m", "autoupdate": false,
                // The person's own deny, which must outlast Mjolnir's rules.
                "permissions": [{"action": "shell", "resource": "rm *", "effect": "deny"}]
            })
            .to_string(),
        )]);
        configure_opencode_rules(
            ExecutionPolicy::ConfiguredApprovals,
            OpenCodeDenials {
                native_subagent: false,
            },
            &mut environment,
        )
        .unwrap();
        configure_opencode_guardian(root.path(), std::slice::from_ref(&work), &mut environment)
            .unwrap();

        let mut session = LiveOpenCode::start(root.path(), &work, &environment[OPENCODE_CONTENT]);
        session.call(
            "initialize",
            serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            "once",
        );
        let id = session.call(
            "session/new",
            serde_json::json!({"cwd": work, "mcpServers": []}),
            "once",
        )["sessionId"]
            .clone();
        let finished = session.call(
            "session/prompt",
            serde_json::json!({"sessionId": id, "prompt": [{"type":"text","text":"BUILD"}]}),
            "once",
        );
        assert_eq!(finished["stopReason"], "end_turn", "{finished}");
        let asked: Vec<String> = session
            .asked
            .iter()
            .map(|call| {
                call["rawInput"]["command"]
                    .as_str()
                    .or(call["rawInput"]["path"].as_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        // The shell command and the edit to git's hooks asked; the workspace
        // edit and the read did not; the person's deny refused `rm` unasked.
        assert_eq!(
            asked,
            ["touch by-shell.txt", ".git/hooks/post-checkout"],
            "{:?}",
            session.asked
        );
        assert!(work.join("by-shell.txt").exists());
        assert!(work.join("notes.txt").exists(), "the denied rm did not run");

        // Plan mode's own edit deny stands.
        let plan = session.call(
            "session/new",
            serde_json::json!({"cwd": work, "mcpServers": []}),
            "once",
        );
        let plan_id = plan["sessionId"].clone();
        session.call(
            "session/set_config_option",
            serde_json::json!({"sessionId": plan_id, "configId": "mode", "value": "plan"}),
            "once",
        );
        session.call(
            "session/prompt",
            serde_json::json!({"sessionId": plan_id, "prompt": [{"type":"text","text":"PLAN"}]}),
            "once",
        );
        assert!(!work.join("by-plan.txt").exists());
        assert_eq!(session.asked.len(), 2, "{:?}", session.asked);
        drop(session);

        // The reviewer reads and does nothing else, without asking, and its
        // launch carries only the reviewer's hook.
        let reviewer = opencode_reviewer_environment(&environment).unwrap();
        let (_, object) = document(&reviewer[OPENCODE_CONTENT]).unwrap();
        let plugins: Vec<String> = object
            .array_value("plugins")
            .unwrap()
            .elements()
            .iter()
            .filter_map(|node| string_value(Some(node.clone())))
            .collect();
        assert_eq!(
            plugins,
            [root
                .path()
                .join(GUARDIAN_REVIEWER_PLUGIN)
                .to_string_lossy()
                .into_owned()]
        );
        offered.lock().unwrap().clear();
        let mut review = LiveOpenCode::start(root.path(), &work, &reviewer[OPENCODE_CONTENT]);
        review.call(
            "initialize",
            serde_json::json!({"protocolVersion": 1, "clientCapabilities": {}}),
            "once",
        );
        let id = review.call(
            "session/new",
            serde_json::json!({"cwd": work, "mcpServers": []}),
            "once",
        )["sessionId"]
            .clone();
        let finished = review.call(
            "session/prompt",
            serde_json::json!({"sessionId": id, "prompt": [{"type":"text","text":"REVIEW"}]}),
            "once",
        );
        assert_eq!(
            finished["stopReason"], "end_turn",
            "a refused tool is a tool error: {finished}"
        );
        assert!(review.asked.is_empty(), "{:?}", review.asked);
        assert!(!work.join("by-reviewer.txt").exists());
        let tools = offered
            .lock()
            .unwrap()
            .iter()
            .find(|tools| !tools.is_empty())
            .cloned()
            .unwrap_or_default();
        assert!(tools.iter().any(|tool| tool == "read"), "{tools:?}");
        assert!(
            !tools
                .iter()
                .any(|tool| tool == "shell" || tool == "write" || tool == "edit"),
            "{tools:?}"
        );
    }
}
