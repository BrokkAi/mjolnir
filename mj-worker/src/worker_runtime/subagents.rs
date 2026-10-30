//! Parent-worker queue behind the Mjolnir sub-agent MCP server.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use tokio::io::AsyncBufReadExt;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, Semaphore};
use tokio::time::{Instant, sleep_until};

use mj_core::subagent::{SubagentCompletion, SubagentQueueChange, SubagentQueuePage};
use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

mod feed;
pub(super) use feed::SubagentFeed;
mod replies;

use crate::subagent_mcp::DaemonContact;

pub const SUBAGENT_SOCKET: &str = "subagents.sock";

const CODEX_CONFIG_ENV: &str = "CODEX_CONFIG";
const CODEX_MAILBOX_HOOK_MARKER: &str = "MJOLNIR_MAILBOX_HOOK=1 ";
const CLAUDE_MAILBOX_HOOK_MARKER: &str = "MJOLNIR_MAILBOX_HOOK=1 ";

/// Register this launch's `mj-agents` server in the session's private Claude
/// profile, and allow every tool it lists, before Claude starts.
///
/// The worker owns this registration, as it owns Codex's, and writes it on
/// every start. A profile is staged once, but a worker upgrade or recovery
/// replaces the launch configuration and binary without restaging it, so a
/// registration written only at staging went stale: a profile staged before
/// top-level sessions could message each other never got `send_message`.
///
/// Paths are absolute on the target, so Claude can launch the server from the
/// checkout. Without a role the server is removed. Rules the person already
/// has are kept. A home that is not a private staged directory may be the
/// person's own, so it is never edited: a delegating parent or a child cannot
/// start without its tools there, while a message-only session starts without
/// `send_message`.
pub(super) fn configure_claude_subagent_mcp(
    root: &Path,
    session_id: &str,
    home: &Path,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    agent_mailboxes_enabled: bool,
    subtle_subagents: bool,
    review_gate: bool,
) -> Result<()> {
    use mj_core::subagent::{SUBAGENT_MCP_SERVER, SubagentMcpRole};
    anyhow::ensure!(
        root.is_absolute(),
        "Claude MCP worker root must be absolute"
    );
    let config_path = home.join(".claude.json");
    let settings_path = home.join("settings.json");
    if !(private_staged_home(root, session_id, home)
        && regular_file_or_absent(&config_path)
        && regular_file_or_absent(&settings_path))
    {
        return match role {
            Some(
                role @ (SubagentMcpRole::Parent
                | SubagentMcpRole::Child
                | SubagentMcpRole::FetchOnly),
            ) => Err(anyhow::anyhow!(
                "cannot register the Claude {} tools: {} is not a private staged profile",
                role.id(),
                home.display()
            )),
            Some(SubagentMcpRole::MessageOnly) => {
                tracing::warn!(path = %home.display(), "skipping the Claude send_message server: the profile is not a private staged directory");
                Ok(())
            }
            None => Ok(()),
        };
    }

    edit_profile_json(&config_path, "staged Claude configuration", |config| {
        if role.is_none() && !config.contains_key("mcpServers") {
            return Ok(());
        }
        let servers = config
            .entry("mcpServers")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "mcpServers in {} must be a JSON object",
                    config_path.display()
                )
            })?;
        let Some(role) = role else {
            servers.remove(SUBAGENT_MCP_SERVER);
            return Ok(());
        };
        let mut args = vec![
            "worker".to_owned(),
            "subagent-mcp".to_owned(),
            "--socket".to_owned(),
            root.join(SUBAGENT_SOCKET).to_string_lossy().into_owned(),
            // The harness selects the matching MCP server instructions.
            "--harness".to_owned(),
            mj_core::config::HarnessKind::Claude.id().to_owned(),
            "--role".to_owned(),
            role.id().to_owned(),
            "--agent-mailboxes-enabled".to_owned(),
            agent_mailboxes_enabled.to_string(),
        ];
        if review_gate && role == mj_core::subagent::SubagentMcpRole::Parent {
            args.push("--review-gate".into());
        }
        if !subtle_subagents {
            // Only the non-default value is written, as for Codex.
            args.extend(["--subtle-subagents".to_owned(), "false".to_owned()]);
        }
        servers.insert(
            SUBAGENT_MCP_SERVER.into(),
            serde_json::json!({
                "type": "stdio",
                // Delegation is part of the session's core toolset, like
                // Claude's native Agent tool, rather than optional discovery.
                "alwaysLoad": true,
                "command": root.join("hel"),
                "args": args,
            }),
        );
        Ok(())
    })?;

    let Some(role) = role else {
        return Ok(());
    };
    // In any mode but Auto, Claude would otherwise ask a person before a
    // child's mandatory `handback` or a parent's `spawn` (R11-1).
    edit_profile_json(&settings_path, "staged Claude settings", |settings| {
        let allow = settings
            .entry("permissions")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "permissions in {} must be a JSON object",
                    settings_path.display()
                )
            })?
            .entry("allow")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| {
                format!(
                    "permissions.allow in {} must be a JSON array",
                    settings_path.display()
                )
            })?;
        let mut tools = role.tool_names().to_vec();
        if review_gate && role == SubagentMcpRole::Parent {
            tools.push("request_review");
        }
        for tool in tools {
            let rule = serde_json::Value::from(format!("mcp__{SUBAGENT_MCP_SERVER}__{tool}"));
            if !allow.contains(&rule) {
                allow.push(rule);
            }
        }
        Ok(())
    })
}

/// Kimi binds stdio servers to a runtime through its private MCP profile.
pub(super) fn configure_kimi_subagent_mcp(
    root: &Path,
    session_id: &str,
    home: &Path,
    role: mj_core::subagent::SubagentMcpRole,
) -> Result<()> {
    let path = home.join("mcp.json");
    anyhow::ensure!(
        root.is_absolute()
            && private_staged_home(root, session_id, home)
            && regular_file_or_absent(&path),
        "cannot register the Kimi {} tools outside a private staged profile",
        role.id(),
    );
    let command = root.join("hel");
    let command = command
        .to_str()
        .context("Kimi MCP command path is not UTF-8")?;
    let socket = root.join(SUBAGENT_SOCKET);
    let socket = socket
        .to_str()
        .context("Kimi MCP socket path is not UTF-8")?;
    edit_profile_json(&path, "staged Kimi MCP configuration", |config| {
        let servers = config
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("Kimi mcpServers must be an object")?;
        servers.insert(
            mj_core::subagent::SUBAGENT_MCP_SERVER.into(),
            serde_json::json!({
                "transport": "stdio",
                "command": command,
                "args": ["worker", "subagent-mcp", "--socket", socket, "--harness", "kimi", "--role", role.id()],
                "runtime_id": "local",
            }),
        );
        Ok(())
    })
}

/// Rewrite a staged profile's JSON file, writing only when its content changes.
/// A missing file starts as an empty object.
fn edit_profile_json(
    path: &Path,
    label: &str,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
) -> Result<()> {
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
        Err(error) => {
            return Err(error).with_context(|| format!("read {label} {}", path.display()));
        }
    };
    let original: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("parse {label} {}", path.display()))?;
    let mut value = original.clone();
    edit(
        value
            .as_object_mut()
            .with_context(|| format!("{label} {} must be a JSON object", path.display()))?,
    )?;
    if value != original {
        let mut resolved = serde_json::to_vec_pretty(&value)?;
        resolved.push(b'\n');
        mj_core::config::atomic_write(path, &resolved)
            .with_context(|| format!("write {label} {}", path.display()))?;
    }
    Ok(())
}

/// Whether `home` is a profile this session staged for itself:
/// `<root>/profile` on this machine, `profiles/<session>` in a container or
/// under a remote login (the controller's `removable_profile_root`). An
/// earlier local session's `<root>/profile` may link to the person's own home,
/// so each directory is checked without following links. Only the local
/// profile's parent and root are resolved for comparison, allowing symlinked
/// ancestors without accepting a linked profile, profiles, or root directory.
pub(super) fn private_staged_home(root: &Path, session_id: &str, home: &Path) -> bool {
    let local_profile = home.file_name() == Some("profile".as_ref())
        && home.parent().is_some_and(|parent| {
            match (std::fs::canonicalize(parent), std::fs::canonicalize(root)) {
                (Ok(parent), Ok(root)) => parent == root,
                _ => false,
            }
        });
    let staged_directories = if local_profile {
        vec![root, home]
    } else if home.file_name() == Some(session_id.as_ref())
        && home.parent().and_then(Path::file_name) == Some("profiles".as_ref())
    {
        vec![home.parent().expect("profiles parent was checked"), home]
    } else {
        Vec::new()
    };
    !staged_directories.is_empty()
        && staged_directories
            .into_iter()
            .all(|path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()))
}

/// A profile file is edited in place only when it is a regular file or does
/// not exist yet; a link may lead out of the private profile.
pub(super) fn regular_file_or_absent(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_file(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Install the mailbox command hook in the session-private Claude settings and
/// resolve staged remote paths on the worker target before Claude reads them.
///
/// The Claude home is the staged profile directory itself: `<root>/profile`
/// on this machine, `profiles/<session>` in a container or under a remote
/// login (the controller's `removable_profile_root`).
pub(super) fn configure_claude_mailbox_hook(
    root: &Path,
    session_id: &str,
    home: &Path,
) -> Result<bool> {
    let private = private_staged_home(root, session_id, home)
        && regular_file_or_absent(&home.join("settings.json"));
    if !private {
        tracing::warn!(path = %home.display(), "skipping Claude mailbox hook repair: settings are not in a private staged directory");
        return Ok(false);
    }
    let path = home.join("settings.json");
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut settings: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("parse staged Claude settings {}", path.display()))?;
    let root_object = settings.as_object_mut().with_context(|| {
        format!(
            "staged Claude settings {} must be a JSON object",
            path.display()
        )
    })?;
    anyhow::ensure!(root.is_absolute(), "Claude worker root must be absolute");
    let worker = std::env::current_exe().context("locate worker for Claude mailbox hook")?;
    let socket = root.join("control.sock");
    let command = format!(
        "MJOLNIR_MAILBOX_HOOK=1 {} worker mailbox-hook --socket {} --event PostToolBatch",
        mj_core::targets::posix_quote(&worker.to_string_lossy()),
        mj_core::targets::posix_quote(&socket.to_string_lossy())
    );
    let groups = root_object
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .with_context(|| format!("hooks in {} must be a JSON object", path.display()))?
        .entry("PostToolBatch")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| {
            format!(
                "hooks.PostToolBatch in {} must be a JSON array",
                path.display()
            )
        })?;
    let existing = groups.iter_mut().find_map(|group| {
        group
            .get_mut("hooks")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|hooks| {
                hooks.iter_mut().find(|hook| {
                    hook.get("type").and_then(serde_json::Value::as_str) == Some("command")
                        && hook
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|existing| {
                                existing.starts_with(CLAUDE_MAILBOX_HOOK_MARKER)
                            })
                })
            })
    });
    if let Some(existing) = existing {
        existing["command"] = serde_json::Value::String(command);
        existing["timeout"] = serde_json::json!(mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS);
    } else {
        groups.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS
            }]
        }));
    }
    let mut resolved = serde_json::to_vec_pretty(&settings)?;
    resolved.push(b'\n');
    if resolved != body {
        mj_core::config::atomic_write(&path, &resolved)
            .with_context(|| format!("write staged Claude settings {}", path.display()))?;
    }
    Ok(true)
}

/// Register the owned server in Codex's session-private profile. The ACP bridge
/// cannot carry omit_tools_from. Do this on the worker before launching the
/// harness so an upgraded worker also repairs an older staged profile.
#[cfg(test)]
pub(super) fn configure_codex_mcp(
    root: &Path,
    home: &Path,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    policy: mj_core::config::ExecutionPolicy,
) -> Result<bool> {
    configure_codex_mcp_with_mailboxes(root, "", home, role, policy, true, true, false)
}

#[allow(clippy::too_many_arguments)] // Mirrors the worker startup policy and owned MCP options.
fn configure_codex_mcp_with_mailboxes(
    root: &Path,
    session_id: &str,
    home: &Path,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    policy: mj_core::config::ExecutionPolicy,
    mailboxes_enabled: bool,
    subtle_subagents: bool,
    review_gate: bool,
) -> Result<bool> {
    if !private_staged_home(root, session_id, home) {
        return Ok(false);
    }
    // Install on every worker start, including without delegation and in YOLO,
    // so changing the execution policy never leaves an older profile behind.
    let rules = home.join("rules");
    match std::fs::symlink_metadata(&rules) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_dir(),
            "Codex rules directory {} must be a real directory",
            rules.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&rules)
                .with_context(|| format!("create Codex rules directory {}", rules.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect Codex rules {}", rules.display()));
        }
    }
    let rule = rules.join("mj-gh.rules");
    anyhow::ensure!(
        regular_file_or_absent(&rule),
        "managed Codex rule {} must be a regular file",
        rule.display()
    );
    mj_core::config::atomic_write(
        &rule,
        b"prefix_rule(pattern=[\"gh\"], decision=\"allow\")\n",
    )
    .with_context(|| format!("write managed Codex rule {}", rule.display()))?;
    // The staged copy is session-private, so re-serializing it through a
    // structured table (which drops comments and formatting) is acceptable.
    let path = home.join("config.toml");
    let mut config = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<toml::Table>(&text)
            .with_context(|| format!("parse staged Codex configuration {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let name = mj_core::subagent::SUBAGENT_MCP_SERVER;
    if role.is_none() && !config.contains_key("mcp_servers") {
        return Ok(true);
    }
    let servers = config
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .with_context(|| format!("mcp_servers in {} must be a table", path.display()))?;
    if let Some(role) = role {
        let worker = std::env::current_exe().context("locate worker for Codex delegation")?;
        let mut args = vec![
            "worker".to_owned(),
            "subagent-mcp".to_owned(),
            "--socket".to_owned(),
            root.join(SUBAGENT_SOCKET).to_string_lossy().into_owned(),
            "--harness".to_owned(),
            "codex".to_owned(),
            "--role".to_owned(),
            role.id().to_owned(),
            "--agent-mailboxes-enabled".to_owned(),
            mailboxes_enabled.to_string(),
        ];
        if review_gate && role == mj_core::subagent::SubagentMcpRole::Parent {
            args.push("--review-gate".into());
        }
        if !subtle_subagents {
            // Written only when off, so a launch offering both types keeps
            // the arguments an older worker accepts.
            args.extend(["--subtle-subagents".to_owned(), "false".to_owned()]);
        }
        let mut server = serde_json::json!({
            "command": worker,
            "args": args,
            // Expose the tools up front, including when code mode is enabled.
            "omit_tools_from": ["deferred"]
        });
        if policy == mj_core::config::ExecutionPolicy::ConfiguredApprovals {
            server["default_tools_approval_mode"] = serde_json::json!("approve");
        }
        servers.insert(name.into(), toml::Value::try_from(server)?);

        // Keep delegation tools outside the code-mode executor so Codex
        // suspends inference until the actual MCP call returns.
        let features = config
            .entry("features")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .context("features in staged Codex configuration must be a table")?;
        let code_mode = features
            .entry("code_mode")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Boolean(enabled) = code_mode {
            *code_mode = toml::Value::Table(toml::Table::from_iter([(
                "enabled".into(),
                toml::Value::Boolean(*enabled),
            )]));
        }
        let namespaces = code_mode
            .as_table_mut()
            .context("features.code_mode in staged Codex configuration must be a table or boolean")?
            .entry("direct_only_tool_namespaces")
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .context("features.code_mode.direct_only_tool_namespaces must be an array")?;
        let namespace = toml::Value::String("mcp__mj_agents".into());
        if !namespaces.contains(&namespace) {
            namespaces.push(namespace);
        }
    } else if servers.remove(name).is_none() {
        return Ok(true);
    }
    mj_core::config::atomic_write(&path, toml::to_string(&config)?.as_bytes())
        .with_context(|| format!("write staged Codex configuration {}", path.display()))?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)] // Startup supplies the session identity alongside its profile.
pub(super) fn configure_codex_profile(
    root: &Path,
    session_id: &str,
    home: &Path,
    environment: &mut BTreeMap<String, String>,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    policy: mj_core::config::ExecutionPolicy,
    options: crate::subagent_mcp::ParentTools,
) -> Result<bool> {
    configure_codex_mailbox_hook(root, environment)?;
    configure_codex_mcp_with_mailboxes(
        root,
        session_id,
        home,
        role,
        policy,
        options.agent_mailboxes_enabled,
        options.subtle_subagents,
        options.review_gate,
    )
}

/// Put the mailbox hook in codex-acp's per-thread config override. Its state
/// trusts only this generated handler; using Codex's global trust bypass would
/// also enable untrusted handlers from the user's and project's config layers.
fn configure_codex_mailbox_hook(
    root: &Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    anyhow::ensure!(root.is_absolute(), "Codex worker root must be absolute");
    let worker = std::env::current_exe().context("locate worker for Codex mailbox hook")?;
    let socket = root.join("control.sock");
    let command = format!(
        "{CODEX_MAILBOX_HOOK_MARKER}{} worker mailbox-hook --socket {} --event PostToolUse",
        mj_core::targets::posix_quote(&worker.to_string_lossy()),
        mj_core::targets::posix_quote(&socket.to_string_lossy())
    );
    let mut config = match environment.get(CODEX_CONFIG_ENV) {
        None => serde_json::Map::new(),
        Some(existing) => serde_json::from_str::<serde_json::Value>(existing)
            .ok()
            .and_then(|value| match value {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            })
            .with_context(|| {
                format!("{CODEX_CONFIG_ENV} must be a JSON object for mailbox hooks")
            })?,
    };
    let hooks = config
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .context("hooks in CODEX_CONFIG must be a JSON object")?;
    let groups = hooks
        .entry("PostToolUse")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut()
        .context("hooks.PostToolUse in CODEX_CONFIG must be a JSON array")?;
    let mut found = None;
    for (group_index, group) in groups.iter_mut().enumerate() {
        let Some(handlers) = group
            .get_mut("hooks")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        if let Some(handler_index) = handlers.iter_mut().position(|handler| {
            handler.get("type").and_then(serde_json::Value::as_str) == Some("command")
                && handler
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|existing| existing.starts_with(CODEX_MAILBOX_HOOK_MARKER))
        }) {
            let handler = &mut handlers[handler_index];
            handler["command"] = serde_json::Value::String(command.clone());
            handler["timeout"] = serde_json::json!(mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS);
            found = Some((group_index, handler_index));
            break;
        }
    }
    let (group_index, handler_index) = if let Some(found) = found {
        found
    } else {
        groups.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": command.clone(),
                "timeout": mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS
            }]
        }));
        (groups.len() - 1, 0)
    };
    let hash = codex_mailbox_hook_hash(&command)?;
    let state = hooks
        .entry("state")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .context("hooks.state in CODEX_CONFIG must be a JSON object")?;
    state.insert(
        format!("/<session-flags>/config.toml:post_tool_use:{group_index}:{handler_index}"),
        serde_json::json!({"enabled": true, "trusted_hash": hash}),
    );
    environment.insert(
        CODEX_CONFIG_ENV.to_owned(),
        serde_json::Value::Object(config).to_string(),
    );
    Ok(())
}

/// Match Codex 0.159.1's hook identity hash for the generated command hook.
fn codex_mailbox_hook_hash(command: &str) -> Result<String> {
    let identity = serde_json::json!({
        "event_name": "post_tool_use",
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS,
            "async": false
        }]
    });
    let identity: toml::Value =
        toml::Value::try_from(identity).context("serialize Codex mailbox hook identity")?;
    let identity = canonicalize_json(serde_json::to_value(identity)?);
    let serialized = serde_json::to_vec(&identity)?;
    let hash = Sha256::digest(serialized);
    Ok(format!(
        "sha256:{}",
        hash.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize_json(value));
            }
            serde_json::Value::Object(canonical)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize_json).collect())
        }
        other => other,
    }
}

const SUBAGENT_QUEUE: &str = "subagents.json";

const MAX_SOCKET_TASKS: usize = 96;
const LONG_SOCKET_TASKS: usize = 32;
const CONTROL_SOCKET_TASKS: usize = 32;
const SOCKET_IO_DEADLINE: Duration = Duration::from_secs(2);

struct SocketAdmission {
    long: Arc<Semaphore>,
    control: Arc<Semaphore>,
}

impl Default for SocketAdmission {
    fn default() -> Self {
        Self {
            long: Arc::new(Semaphore::new(LONG_SOCKET_TASKS)),
            control: Arc::new(Semaphore::new(CONTROL_SOCKET_TASKS)),
        }
    }
}

/// How much sooner than the MCP server gives up this worker answers a request.
/// The MCP server waits
/// [`crate::subagent_mcp::REPLY_TIMEOUT`]; answering first lets it tell the
/// model what this worker knows (whether the daemon has picked the request up)
/// instead of only that nothing came back (#1197).
const WORKER_REPLY_MARGIN: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueState {
    #[serde(default)]
    requests: BTreeMap<String, SubagentToolRequest>,
    #[serde(default)]
    results: BTreeMap<String, SubagentToolResult>,
    #[serde(default)]
    completions: BTreeMap<String, SubagentCompletion>,
    #[serde(default)]
    caller_acknowledged: BTreeSet<String>,
    #[serde(default)]
    daemon_acknowledged: BTreeSet<String>,
    #[serde(default)]
    // Decode queue files from older workers without using their delivery decisions.
    #[serde(rename = "delivered_to_waiter", skip_serializing)]
    _legacy_delivered_to_waiter: BTreeMap<String, bool>,
    /// Protected by the same lock as `requests`, making gate closure atomic
    /// with queue admission. Old queue files default to the open state.
    #[serde(default = "default_mutating_admission")]
    mutating_admission_open: bool,
    /// The daemon's reads of this queue since this worker started. It lives
    /// under the queue's lock so a request is either in a read or after it.
    #[serde(skip)]
    collections: Collections,
    #[serde(skip)]
    generation: u64,
    /// Waiters currently registered in this worker process. Registration and
    /// completion share the queue lock to decide the fallback race once.
    #[serde(skip)]
    live_waiters: BTreeMap<String, usize>,
}

impl Default for QueueState {
    fn default() -> Self {
        Self {
            requests: BTreeMap::new(),
            results: BTreeMap::new(),
            completions: BTreeMap::new(),
            caller_acknowledged: BTreeSet::new(),
            daemon_acknowledged: BTreeSet::new(),
            _legacy_delivered_to_waiter: BTreeMap::new(),
            mutating_admission_open: true,
            collections: Collections::default(),
            generation: 0,
            live_waiters: BTreeMap::new(),
        }
    }
}

fn default_mutating_admission() -> bool {
    true
}

/// How often and how recently the daemon has read the queue.
#[derive(Clone, Copy, Debug, Default)]
struct Collections {
    count: u64,
    last: Option<Instant>,
}

#[derive(Clone)]
pub struct SubagentEndpoint {
    path: PathBuf,
    admission: Arc<SocketAdmission>,
    relay: Option<Arc<Mutex<crate::relay::DurableRelay>>>,
    state: Arc<Mutex<QueueState>>,
    /// Woken whenever a result lands, so a socket call awaiting its own request
    /// returns the moment the daemon completes it.
    completed: Arc<Notify>,
}

impl SubagentEndpoint {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join(SUBAGENT_QUEUE);
        let state: QueueState = match std::fs::read(&path) {
            Ok(body) => serde_json::from_slice(&body)
                .with_context(|| format!("parse sub-agent queue {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => QueueState::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read sub-agent queue {}", path.display()));
            }
        };
        let endpoint = Self {
            path,
            admission: Arc::default(),
            relay: None,
            state: Arc::new(Mutex::new(state)),
            completed: Arc::new(Notify::new()),
        };
        endpoint.import_legacy_results()?;
        Ok(endpoint)
    }

    fn cached_result(&self, request_id: &str) -> Result<Option<SubagentToolResult>> {
        let state = self.state.lock().expect("sub-agent queue lock poisoned");
        self.read_result(&state, request_id)
    }

    fn register_waiter(&self, request_id: &str) -> LiveWaiter {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        *state.live_waiters.entry(request_id.to_owned()).or_default() += 1;
        LiveWaiter {
            state: self.state.clone(),
            request_id: request_id.to_owned(),
            active: true,
        }
    }

    async fn await_result_registered(
        &self,
        request_id: &str,
        deadline: Instant,
        waiter: &mut LiveWaiter,
    ) -> Result<Option<SubagentToolResult>> {
        loop {
            let notified = self.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.cached_result(request_id)? {
                waiter.finish();
                return Ok(Some(result));
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = sleep_until(deadline) => return waiter.finish_and_take_result(self),
            }
        }
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> (Vec<SubagentToolRequest>, Vec<SubagentToolResult>) {
        let state = self.state.lock().expect("sub-agent queue lock poisoned");
        (
            state.requests.values().cloned().collect(),
            state
                .completions
                .keys()
                .filter_map(|id| self.read_result(&state, id).unwrap())
                .collect(),
        )
    }

    #[cfg(test)]
    fn record_collection(&self) {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        state.collections.count += 1;
        state.collections.last = Some(Instant::now());
    }

    /// Serialize an admission change with every enqueue and persist it before
    /// reporting success. A request that raced the close is either already in
    /// `requests` for the controller to drain, or is rejected by that enqueue.
    pub fn set_mutating_admission(&self, open: bool) -> Result<()> {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        if state.mutating_admission_open == open {
            return Ok(());
        }
        let mut next = state.clone();
        next.mutating_admission_open = open;
        self.persist(&mut next)?;
        *state = next;
        Ok(())
    }

    /// Whether the daemon has read the queue after the read numbered `mark`,
    /// and how long ago it last read it.
    fn daemon_contact(&self, mark: u64) -> DaemonContact {
        let collections = self
            .state
            .lock()
            .expect("sub-agent queue lock poisoned")
            .collections;
        DaemonContact {
            picked_up: collections.count > mark,
            last_collected_seconds_ago: collections.last.map(|last| last.elapsed().as_secs()),
        }
    }

    #[cfg(test)]
    pub fn enqueue(&self, request: SubagentToolRequest) -> Result<Option<SubagentToolResult>> {
        self.enqueue_marked(request).map(|(_, result, _)| result)
    }

    /// Queue `request`. Also return any result already recorded for it, and
    /// the number of daemon reads that happened before it joined the queue.
    fn enqueue_marked(
        &self,
        mut request: SubagentToolRequest,
    ) -> Result<(bool, Option<SubagentToolResult>, u64)> {
        // One owner selects the originating turn and keeps it selected until
        // the request is durable; a later daemon never reconstructs this fact.
        let relay = self
            .relay
            .as_ref()
            .map(|relay| relay.lock().expect("relay lock poisoned"));
        if let Some(relay) = &relay {
            request.originating_command_id = relay.originating_command_id();
        }
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        let mark = state.collections.count;
        if let Some(result) = self.read_result(&state, &request.request_id)? {
            return Ok((true, Some(result), mark));
        }
        // A retry of a request already accepted before the gate closed is not
        // a new admission. Keep it attached to the original queued operation.
        if state.requests.contains_key(&request.request_id) {
            return Ok((true, None, mark));
        }
        if state.requests.len()
            + state.completions.len().saturating_sub(
                state
                    .caller_acknowledged
                    .intersection(&state.daemon_acknowledged)
                    .count(),
            )
            >= mj_core::relay::RELAY_RETRY_ID_RETENTION
        {
            return Ok((false, Some(SubagentToolResult {
                request_id: request.request_id,
                completed_at_ms: mj_core::clock::epoch_millis(), is_error: true,
                message: "Sub-agent request was not accepted: outstanding replies must be acknowledged before more work can be queued.".into(),
            }), mark));
        }
        if request.action.mutates_child_state() && !state.mutating_admission_open {
            return Ok((false, Some(SubagentToolResult {
                request_id: request.request_id,
                completed_at_ms: mj_core::clock::epoch_millis(),
                is_error: true,
                message: "Sub-agent request was not accepted: this session is replacing its harness (Move). Retry after the Move finishes.".into(),
            }), mark));
        }
        let mut next = state.clone();
        next.requests
            .entry(request.request_id.clone())
            .or_insert(request);
        self.persist(&mut next)?;
        *state = next;
        Ok((true, None, mark))
    }

    pub fn complete(&self, result: SubagentToolResult) -> Result<bool> {
        let request_id = result.request_id.clone();
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        if state.completions.contains_key(&request_id) {
            return Ok(true);
        }
        let delivered = state.live_waiters.get(&request_id).copied().unwrap_or(0) > 0;
        self.store_result(&mut state, result)?;
        drop(state);
        self.completed.notify_waiters();
        Ok(delivered)
    }

    fn store_result(&self, state: &mut QueueState, result: SubagentToolResult) -> Result<()> {
        let request_id = result.request_id.clone();
        let mut next = state.clone();
        next.requests.remove(&request_id);
        self.write_result(&result)?;
        next.completions
            .insert(request_id.clone(), SubagentCompletion::from(&result));
        self.retire_receipts(&mut next)?;
        self.persist(&mut next)?;
        *state = next;
        Ok(())
    }

    fn persist(&self, state: &mut QueueState) -> Result<()> {
        let body = serde_json::to_vec_pretty(state)?;
        mj_core::config::atomic_write(&self.path, &body)
            .with_context(|| format!("write sub-agent queue {}", self.path.display()))?;
        state.generation = state.generation.wrapping_add(1);
        Ok(())
    }
}

struct LiveWaiter {
    state: Arc<Mutex<QueueState>>,
    request_id: String,
    active: bool,
}

impl LiveWaiter {
    /// End the waiter and inspect its result under the same lock used by
    /// `complete`, so timeout and completion have one serialized winner.
    fn finish_and_take_result(
        &mut self,
        endpoint: &SubagentEndpoint,
    ) -> Result<Option<SubagentToolResult>> {
        let state_ref = self.state.clone();
        let request_id = self.request_id.clone();
        let mut state = state_ref.lock().expect("sub-agent queue lock poisoned");
        Self::remove_from(&mut self.active, &request_id, &mut state);
        endpoint.read_result(&state, &request_id)
    }

    fn finish(&mut self) {
        let state_ref = self.state.clone();
        let request_id = self.request_id.clone();
        let mut state = state_ref.lock().expect("sub-agent queue lock poisoned");
        Self::remove_from(&mut self.active, &request_id, &mut state);
    }

    fn remove_from(active: &mut bool, request_id: &str, state: &mut QueueState) {
        if !*active {
            return;
        }
        if let Some(waiters) = state.live_waiters.get_mut(request_id) {
            *waiters -= 1;
            if *waiters == 0 {
                state.live_waiters.remove(request_id);
            }
        }
        *active = false;
    }
}

impl Drop for LiveWaiter {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kimi_reviewer_registers_handback_in_its_private_runtime_profile() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(
            home.join("mcp.json"),
            br#"{"mcpServers":{"existing":{"command":"keep"}}}"#,
        )
        .unwrap();
        configure_kimi_subagent_mcp(
            root.path(),
            "reviewer",
            &home,
            mj_core::subagent::SubagentMcpRole::Child,
        )
        .unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["existing"]["command"], "keep");
        let server = &config["mcpServers"]["mj-agents"];
        assert_eq!(server["runtime_id"], "local");
        assert_eq!(server["transport"], "stdio");
        assert_eq!(server["command"], root.path().join("hel").to_str().unwrap());
        assert_eq!(
            server["args"],
            serde_json::json!([
                "worker",
                "subagent-mcp",
                "--socket",
                root.path().join(SUBAGENT_SOCKET),
                "--harness",
                "kimi",
                "--role",
                "child"
            ])
        );
        let outside = tempfile::tempdir().unwrap();
        assert!(
            configure_kimi_subagent_mcp(
                root.path(),
                "reviewer",
                outside.path(),
                mj_core::subagent::SubagentMcpRole::Child
            )
            .is_err()
        );
    }

    use mj_core::subagent::SubagentToolAction;

    // Hard-won: a602a316a: a symlinked data ancestor disabled Claude messaging and native steering.
    #[cfg(unix)]
    #[test]
    fn claude_profile_repair_accepts_symlinked_ancestors_with_different_path_spellings() {
        use mj_core::subagent::SubagentMcpRole;

        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let alias = temp.path().join("data-link");
        let staged_root = data.join("workers/session");
        std::fs::create_dir_all(staged_root.join("profile")).unwrap();
        std::os::unix::fs::symlink(&data, &alias).unwrap();
        let alias_root = alias.join("workers/session");
        let owner = super::super::WorkerRootOwner::acquire(&alias_root).unwrap();
        let canonical_root = owner.root();

        for (root, home) in [
            (canonical_root, alias_root.join("profile")),
            (alias_root.as_path(), canonical_root.join("profile")),
        ] {
            std::fs::write(home.join(".claude.json"), b"{}").unwrap();
            std::fs::write(home.join("settings.json"), b"{}").unwrap();
            configure_claude_subagent_mcp(
                root,
                "session",
                &home,
                Some(SubagentMcpRole::MessageOnly),
                true,
                true,
                false,
            )
            .unwrap();
            assert!(configure_claude_mailbox_hook(root, "session", &home).unwrap());

            let config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
            assert_eq!(
                config["mcpServers"]["mj-agents"]["command"],
                serde_json::json!(root.join("hel"))
            );
            let settings: serde_json::Value =
                serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap())
                    .unwrap();
            assert_eq!(
                settings["permissions"]["allow"],
                serde_json::json!(["mcp__mj-agents__send_message", "mcp__mj-agents__fetch"])
            );
            let command = settings["hooks"]["PostToolBatch"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(command.ends_with(&format!(
                "--socket {} --event PostToolBatch",
                mj_core::targets::posix_quote(&root.join("control.sock").to_string_lossy())
            )));
        }
    }

    // Hard-won: 3c8973b87: the check expected <root>/profile/.claude, so it refused every real Claude home and steering was never offered.
    #[test]
    fn claude_mailbox_hook_resolves_the_worker_socket_and_preserves_profile_settings() {
        let worker_root = tempfile::tempdir().unwrap();
        let staged_home = worker_root.path().join("profile");
        std::fs::create_dir_all(&staged_home).unwrap();
        let remote_root = ".local/share/hel/workers/session";
        let path = staged_home.join("settings.json");
        let user_hook = serde_json::json!({
            "hooks": [{"type": "command", "command": "user-hook"}]
        });
        let relative_command = format!(
            "MJOLNIR_MAILBOX_HOOK=1 {} worker mailbox-hook --socket {} --event PostToolBatch",
            mj_core::targets::posix_quote(&format!("{remote_root}/hel")),
            mj_core::targets::posix_quote(&format!("{remote_root}/control.sock"))
        );
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "model": "opus",
                "hooks": {
                    "PostToolBatch": [
                        user_hook.clone(),
                        {"hooks": [{
                            "type": "command",
                            "command": relative_command,
                            "timeout": 10
                        }]}
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            configure_claude_mailbox_hook(worker_root.path(), "session", &staged_home).unwrap()
        );
        let first = std::fs::read(&path).unwrap();
        assert!(
            configure_claude_mailbox_hook(worker_root.path(), "session", &staged_home).unwrap()
        );
        assert_eq!(std::fs::read(&path).unwrap(), first);

        let settings: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(settings["model"], "opus");
        let groups = settings["hooks"]["PostToolBatch"].as_array().unwrap();
        assert_eq!(groups[0], user_hook);
        let mailbox_hooks = groups
            .iter()
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .filter(|hook| {
                hook["command"]
                    .as_str()
                    .is_some_and(|command| command.starts_with(CLAUDE_MAILBOX_HOOK_MARKER))
            })
            .collect::<Vec<_>>();
        assert_eq!(mailbox_hooks.len(), 1);
        assert_eq!(
            mailbox_hooks[0]["timeout"],
            mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS
        );
        let worker = std::env::current_exe().unwrap();
        let expected_command = format!(
            "{CLAUDE_MAILBOX_HOOK_MARKER}{} worker mailbox-hook --socket {} --event PostToolBatch",
            mj_core::targets::posix_quote(&worker.to_string_lossy()),
            mj_core::targets::posix_quote(
                &worker_root.path().join("control.sock").to_string_lossy()
            )
        );
        assert_eq!(mailbox_hooks[0]["command"], expected_command);

        std::fs::write(&path, br#"{"model":"opus"}"#).unwrap();
        assert!(
            configure_claude_mailbox_hook(worker_root.path(), "session", &staged_home).unwrap()
        );
        let repaired: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(repaired["model"], "opus");
        assert!(
            repaired["hooks"]["PostToolBatch"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .starts_with(CLAUDE_MAILBOX_HOOK_MARKER)
        );

        // An earlier local session ran from the person's own home, and its
        // staged path links there. Neither path may be edited.
        let source_home = tempfile::tempdir().unwrap();
        let source_settings = source_home.path().join("settings.json");
        std::fs::write(&source_settings, br#"{"user":"source"}"#).unwrap();
        let legacy_root = tempfile::tempdir().unwrap();
        let linked_home = legacy_root.path().join("profile");
        std::os::unix::fs::symlink(source_home.path(), &linked_home).unwrap();
        for home in [source_home.path(), linked_home.as_path()] {
            assert!(!configure_claude_mailbox_hook(legacy_root.path(), "session", home).unwrap());
        }
        assert_eq!(
            std::fs::read(source_settings).unwrap(),
            br#"{"user":"source"}"#
        );
    }

    // Hard-won: 3c8973b87: a remote profile outside the worker root kept its home-relative hook command.
    #[test]
    fn claude_mailbox_hook_repairs_a_remote_profile_outside_the_worker_root() {
        let login = tempfile::tempdir().unwrap();
        let root = login.path().join(".local/share/hel/workers/worker-7");
        let home = login.path().join(".local/share/hel/profiles/session-7");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        // The controller wrote a home-relative command, which Claude would
        // run from the checkout.
        std::fs::write(
            home.join("settings.json"),
            br#"{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"MJOLNIR_MAILBOX_HOOK=1 '.local/share/hel/workers/worker-7/hel' worker mailbox-hook"}]}]}}"#,
        )
        .unwrap();

        assert!(configure_claude_mailbox_hook(&root, "session-7", &home).unwrap());
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap();
        let command = settings["hooks"]["PostToolBatch"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        let socket = root.join("control.sock");
        assert!(command.ends_with(&format!(
            "--socket {} --event PostToolBatch",
            mj_core::targets::posix_quote(&socket.to_string_lossy())
        )));

        // Another session's profile, or one that is not under `profiles`, is
        // not this session's.
        assert!(!configure_claude_mailbox_hook(&root, "session-8", &home).unwrap());
        let elsewhere = login.path().join("session-7");
        std::fs::create_dir(&elsewhere).unwrap();
        assert!(!configure_claude_mailbox_hook(&root, "session-7", &elsewhere).unwrap());
    }

    // Hard-won: #1302: a nested staging link allowed writes to the source profile.
    #[test]
    fn claude_mailbox_repair_rejects_nested_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::os::unix::fs::symlink(outside.path(), &home).unwrap();
        let path = outside.path().join("settings.json");
        // Check both missing hooks and a hook that the startup repair would refresh.
        for settings in [br#"{}"#.as_slice(), br#"{"hooks":{"PostToolBatch":[{"hooks":[{"type":"command","command":"MJOLNIR_MAILBOX_HOOK=1 old"}]}]}}"#.as_slice()] {
            std::fs::write(&path, settings).unwrap();
            assert!(!configure_claude_mailbox_hook(root.path(), "session", &home).unwrap());
            assert_eq!(std::fs::read(&path).unwrap(), settings);
        }
        std::fs::remove_file(&home).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(&path, home.join("settings.json")).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(!configure_claude_mailbox_hook(root.path(), "session", &home).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        // A root that is itself a link stays unsafe, even when the home is
        // spelled through the real directory. Unrelated profiles stay unsafe.
        std::fs::remove_file(home.join("settings.json")).unwrap();
        let linked_root = outside.path().join("worker-link");
        std::os::unix::fs::symlink(root.path(), &linked_root).unwrap();
        let unrelated_home = outside.path().join("profile");
        std::fs::create_dir(&unrelated_home).unwrap();
        for (root, home) in [
            (linked_root.as_path(), home.clone()),
            (linked_root.as_path(), linked_root.join("profile")),
            (root.path(), unrelated_home),
        ] {
            for name in ["settings.json", ".claude.json"] {
                std::fs::write(home.join(name), b"{}").unwrap();
            }
            assert!(!configure_claude_mailbox_hook(root, "session", &home).unwrap());
            configure_claude_subagent_mcp(
                root,
                "session",
                &home,
                Some(mj_core::subagent::SubagentMcpRole::MessageOnly),
                true,
                true,
                false,
            )
            .unwrap();
            for name in ["settings.json", ".claude.json"] {
                assert_eq!(std::fs::read(home.join(name)).unwrap(), b"{}");
            }
        }
    }

    #[test]
    fn codex_thread_mailbox_hook_preserves_overrides_and_trusts_only_its_handler() {
        let worker_root = tempfile::tempdir().unwrap();
        let user_hook = serde_json::json!({
            "hooks": [{"type": "command", "command": "user-hook"}]
        });
        let mut environment = BTreeMap::from([(
            CODEX_CONFIG_ENV.to_owned(),
            serde_json::json!({
                "model": "session-model",
                "hooks": {"PostToolUse": [user_hook.clone()]}
            })
            .to_string(),
        )]);

        configure_codex_mailbox_hook(worker_root.path(), &mut environment).unwrap();
        let first = environment[CODEX_CONFIG_ENV].clone();
        configure_codex_mailbox_hook(worker_root.path(), &mut environment).unwrap();
        assert_eq!(environment[CODEX_CONFIG_ENV], first);

        let config: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(config["model"], "session-model");
        let groups = config["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups[0], user_hook);
        assert_eq!(groups.len(), 2);
        let hook = &groups[1]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["timeout"], mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS);
        let worker = std::env::current_exe().unwrap();
        let socket = worker_root.path().join("control.sock");
        let expected_command = format!(
            "{CODEX_MAILBOX_HOOK_MARKER}{} worker mailbox-hook --socket {} --event PostToolUse",
            mj_core::targets::posix_quote(&worker.to_string_lossy()),
            mj_core::targets::posix_quote(&socket.to_string_lossy())
        );
        assert_eq!(hook["command"], expected_command);
        let expected_hash = codex_mailbox_hook_hash(&expected_command).unwrap();
        let state = &config["hooks"]["state"]["/<session-flags>/config.toml:post_tool_use:1:0"];
        assert_eq!(state["enabled"], true);
        assert_eq!(state["trusted_hash"], expected_hash);
        assert_eq!(config["hooks"]["state"].as_object().unwrap().len(), 1);
        assert!(config.get("bypass_hook_trust").is_none());
    }

    #[test]
    fn mailbox_hooks_replace_old_handlers_and_preserve_user_handlers() {
        let claude_root = tempfile::tempdir().unwrap();
        let claude_home = claude_root.path().join("profile");
        std::fs::create_dir_all(&claude_home).unwrap();
        let claude_settings_path = claude_home.join("settings.json");
        std::fs::write(
            &claude_settings_path,
            serde_json::to_vec(&serde_json::json!({
                "hooks": {"PostToolBatch": [{"hooks": [
                    {"type": "command", "command": "user-hook"},
                    {"type": "command", "command": "MJOLNIR_MAILBOX_HOOK=1 old-worker worker mailbox-hook"}
                ]}]}
            }))
            .unwrap(),
        )
        .unwrap();
        configure_claude_mailbox_hook(claude_root.path(), "session", &claude_home).unwrap();
        let claude: serde_json::Value =
            serde_json::from_slice(&std::fs::read(claude_settings_path).unwrap()).unwrap();
        let claude_hooks = claude["hooks"]["PostToolBatch"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(claude_hooks.len(), 2);
        assert_eq!(claude_hooks[0]["command"], "user-hook");
        assert!(
            claude_hooks[1]["command"]
                .as_str()
                .unwrap()
                .starts_with(CLAUDE_MAILBOX_HOOK_MARKER)
        );

        let codex_root = tempfile::tempdir().unwrap();
        let user_hook = serde_json::json!({
            "hooks": [{"type": "command", "command": "user-hook"}]
        });
        let mut environment = BTreeMap::from([(
            CODEX_CONFIG_ENV.to_owned(),
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [user_hook.clone(), {"hooks": [
                        {"type": "command", "command": "MJOLNIR_MAILBOX_HOOK=1 old-worker worker mailbox-hook"}
                    ]}],
                    "state": {
                        "/<session-flags>/config.toml:post_tool_use:1:0": {"enabled": true}
                    }
                }
            })
            .to_string(),
        )]);
        configure_codex_mailbox_hook(codex_root.path(), &mut environment).unwrap();
        let codex: serde_json::Value =
            serde_json::from_str(&environment[CODEX_CONFIG_ENV]).unwrap();
        let groups = codex["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], user_hook);
        assert!(
            groups[1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .starts_with(CODEX_MAILBOX_HOOK_MARKER)
        );
        assert_eq!(codex["hooks"]["state"].as_object().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn claude_delegation_launches_from_a_checkout_after_remote_profile_repair() {
        use mj_core::subagent::SubagentMcpRole;
        use std::os::unix::fs::PermissionsExt;

        for (role, old_root) in [
            (SubagentMcpRole::Parent, ".local/share/hel/profiles/session"),
            (SubagentMcpRole::Parent, ".local/share/hel/workers/session"),
            (SubagentMcpRole::Child, "/previous-target/workers/session"),
            (
                SubagentMcpRole::MessageOnly,
                "/previous-target/workers/session",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("worker with spaces");
            let home = root.join("profile");
            let checkout = temp.path().join("checkout");
            for directory in [&root, &home, &checkout] {
                std::fs::create_dir(directory).unwrap();
            }
            let worker = root.join("hel");
            std::fs::write(
                &worker,
                r#"#!/usr/bin/env python3
import socket, sys
assert sys.argv[1:3] == ['worker', 'subagent-mcp']
assert sys.argv[sys.argv.index('--harness') + 1] == 'claude'
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
    connection.connect(sys.argv[sys.argv.index('--socket') + 1])
    assert connection.recv(64) == b'connected'
print(sys.argv[sys.argv.index('--role') + 1])
"#,
            )
            .unwrap();
            std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
            // A registration an earlier controller staged with home-relative
            // paths from another target.
            let original = serde_json::json!({
                "userSetting": "retained",
                "mcpServers": {
                    "user-server": {"command": "user-tool"},
                    "mj-agents": {
                        "type": "stdio",
                        "alwaysLoad": true,
                        "command": Path::new(old_root).join("hel"),
                        "args": ["worker", "subagent-mcp", "--socket",
                                 Path::new(old_root).join(SUBAGENT_SOCKET),
                                 "--harness", "claude", "--role", "parent"]
                    }
                }
            });
            let path = home.join(".claude.json");
            std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
            let configure = || {
                configure_claude_subagent_mcp(
                    &root,
                    "session",
                    &home,
                    Some(role),
                    true,
                    true,
                    false,
                )
            };
            configure().unwrap();
            let first = std::fs::read(&path).unwrap();
            configure().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), first);
            let config: serde_json::Value = serde_json::from_slice(&first).unwrap();
            assert_eq!(config["userSetting"], original["userSetting"]);
            assert_eq!(
                config["mcpServers"]["user-server"],
                original["mcpServers"]["user-server"]
            );
            let server = &config["mcpServers"]["mj-agents"];
            assert_eq!(server["alwaysLoad"], true);
            let socket = UnixListener::bind(root.join(SUBAGENT_SOCKET)).unwrap();
            let mut command = tokio::process::Command::new(server["command"].as_str().unwrap());
            command
                .args(
                    server["args"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|arg| arg.as_str().unwrap()),
                )
                .current_dir(&checkout);
            let (output, ()) = tokio::join!(
                mj_core::subprocess::run_bounded(&mut command, 64 * 1024, Duration::from_secs(10)),
                async {
                    let (mut connection, _) =
                        tokio::time::timeout(Duration::from_secs(10), socket.accept())
                            .await
                            .unwrap()
                            .unwrap();
                    connection.write_all(b"connected").await.unwrap();
                }
            );
            let output = output.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), role.id());
        }
    }

    // Hard-won: adc0d368: an upgraded worker failed preparation on a profile staged without the message-only server.
    // Hard-won: f9a3130c: a profile staged before message-only sessions kept no send_message through every upgrade.
    #[test]
    fn a_message_only_session_gets_send_message_on_a_profile_staged_without_it() {
        use mj_core::subagent::SubagentMcpRole;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        // What a controller from before message-only sessions staged: other
        // servers, no `mj-agents`, and no rule for it.
        let path = home.join(".claude.json");
        std::fs::write(&path, br#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        configure_claude_subagent_mcp(
            root.path(),
            "session",
            &home,
            Some(SubagentMcpRole::MessageOnly),
            true,
            true,
            false,
        )
        .unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["mcpServers"]["other"]["command"], "x");
        let args = config["mcpServers"]["mj-agents"]["args"]
            .as_array()
            .unwrap();
        let role = args.iter().position(|arg| arg == "--role").unwrap() + 1;
        assert_eq!(args[role], "message_only");
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap();
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["mcp__mj-agents__send_message", "mcp__mj-agents__fetch"])
        );

        // Losing the role removes the server; the person's servers stay.
        configure_claude_subagent_mcp(root.path(), "session", &home, None, true, true, false)
            .unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(config["mcpServers"].get("mj-agents").is_none());
        assert_eq!(config["mcpServers"]["other"]["command"], "x");

        // A session with no role writes nothing it did not already have.
        let bare = tempfile::tempdir().unwrap();
        let bare_home = bare.path().join("profile");
        std::fs::create_dir(&bare_home).unwrap();
        configure_claude_subagent_mcp(bare.path(), "session", &bare_home, None, true, true, false)
            .unwrap();
        assert!(!bare_home.join(".claude.json").exists());
        assert!(!bare_home.join("settings.json").exists());
    }

    /// Launch finding R11-1: a Claude child on a model without Auto mode ran
    /// in Accept edits, and Claude asked a person before it would run the
    /// child's own `handback`, so the child could not report without one. The
    /// settings allow every tool the role's `mj-agents` server lists, whatever
    /// the mode, and keep the person's own settings and rules.
    // Hard-won: 91244ea: a Claude child asked the person for permission before its own handback
    #[test]
    fn the_claude_profile_allows_its_own_sub_agent_tools() {
        use mj_core::subagent::SubagentMcpRole;
        let profile = || {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("profile")).unwrap();
            root
        };
        let configure = |root: &tempfile::TempDir, role, subtle| {
            let home = root.path().join("profile");
            configure_claude_subagent_mcp(
                root.path(),
                "session",
                &home,
                Some(role),
                true,
                subtle,
                false,
            )
            .unwrap();
            let settings: serde_json::Value =
                serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap())
                    .unwrap();
            let config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
            let allow: Vec<String> = settings["permissions"]["allow"]
                .as_array()
                .unwrap()
                .iter()
                .map(|rule| rule.as_str().unwrap().to_owned())
                .collect();
            (settings, config, allow)
        };

        // Children can hand back and fetch without asking the person.
        let root = profile();
        std::fs::write(
            root.path().join("profile/settings.json"),
            r#"{"model":"opus","permissions":{"allow":["Bash(ls:*)"],"deny":["WebFetch"]}}"#,
        )
        .unwrap();
        let (settings, _, allow) = configure(&root, SubagentMcpRole::Child, true);
        assert_eq!(
            allow,
            [
                "Bash(ls:*)",
                "mcp__mj-agents__handback",
                "mcp__mj-agents__fetch"
            ]
        );
        assert_eq!(
            settings["permissions"]["deny"],
            serde_json::json!(["WebFetch"])
        );
        assert_eq!(settings["model"], "opus");

        // A parent delegates without asking; a rule the person already has is
        // kept once, in its place.
        let root = profile();
        std::fs::write(
            root.path().join("profile/settings.json"),
            r#"{"permissions":{"allow":["mcp__mj-agents__wait"]}}"#,
        )
        .unwrap();
        let (_, config, allow) = configure(&root, SubagentMcpRole::Parent, true);
        assert!(
            !config["mcpServers"]["mj-agents"]["args"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("--subtle-subagents")),
            "the default stays off the command line"
        );
        assert_eq!(allow[0], "mcp__mj-agents__wait");
        assert_eq!(
            allow
                .iter()
                .filter(|rule| *rule == "mcp__mj-agents__wait")
                .count(),
            1,
            "{allow:?}"
        );
        for tool in ["spawn", "list_agents", "send_message", "close", "fetch"] {
            assert!(
                allow.contains(&format!("mcp__mj-agents__{tool}")),
                "{tool}: {allow:?}"
            );
        }
        assert!(
            !allow.iter().any(|rule| rule.ends_with("__handback")),
            "a parent has no handback: {allow:?}"
        );

        // With only the straightforward type configured, the parent's server
        // is told so and offers no task type.
        let root = profile();
        let (_, config, _) = configure(&root, SubagentMcpRole::Parent, false);
        let args = config["mcpServers"]["mj-agents"]["args"]
            .as_array()
            .unwrap();
        assert_eq!(
            args[args.len() - 2..],
            [
                serde_json::json!("--subtle-subagents"),
                serde_json::json!("false")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_claude_registration_never_edits_a_profile_that_may_be_the_persons_own() {
        use mj_core::subagent::SubagentMcpRole;
        let root = tempfile::tempdir().unwrap();
        let own = tempfile::tempdir().unwrap();
        std::fs::write(own.path().join(".claude.json"), b"{}").unwrap();
        // An earlier local session linked its profile to the person's home.
        std::os::unix::fs::symlink(own.path(), root.path().join("profile")).unwrap();
        let home = root.path().join("profile");
        for role in [SubagentMcpRole::Parent, SubagentMcpRole::Child] {
            assert!(
                configure_claude_subagent_mcp(
                    root.path(),
                    "session",
                    &home,
                    Some(role),
                    true,
                    true,
                    false
                )
                .is_err(),
                "{role:?} cannot start without its tools"
            );
        }
        configure_claude_subagent_mcp(
            root.path(),
            "session",
            &home,
            Some(SubagentMcpRole::MessageOnly),
            true,
            true,
            false,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(own.path().join(".claude.json")).unwrap(),
            b"{}"
        );
        assert!(!own.path().join("settings.json").exists());

        // Invalid JSON in a private profile is reported, not replaced.
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        for broken in ["{", "[]", r#"{"mcpServers":[]}"#] {
            std::fs::write(home.join(".claude.json"), broken).unwrap();
            assert!(
                configure_claude_subagent_mcp(
                    root.path(),
                    "session",
                    &home,
                    Some(SubagentMcpRole::MessageOnly),
                    true,
                    true,
                    false
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(home.join(".claude.json")).unwrap(),
                broken
            );
        }
    }

    // Hard-won: #1287: direct exposure still allowed blocking waits inside code-mode exec cells.
    #[test]
    fn codex_delegation_profile_preserves_other_servers_and_handles_upgrades_and_disable() {
        use mj_core::config::ExecutionPolicy;
        use mj_core::subagent::SubagentMcpRole;
        for role in [
            SubagentMcpRole::Parent,
            SubagentMcpRole::Parent,
            SubagentMcpRole::MessageOnly,
            SubagentMcpRole::Child,
        ] {
            for policy in [
                ExecutionPolicy::ConfiguredApprovals,
                ExecutionPolicy::Unconstrained,
            ] {
                let root = tempfile::tempdir().unwrap();
                let home = root.path().join("profile");
                std::fs::create_dir(&home).unwrap();
                let path = home.join("config.toml");
                let rules = home.join("rules");
                std::fs::create_dir(&rules).unwrap();
                let user_rule = "prefix_rule(pattern=[\"git\"], decision=\"prompt\")\n";
                std::fs::write(rules.join("user.rules"), user_rule).unwrap();
                let original = r#"
model = "parent-model"
[features]
hooks = true
[features.code_mode]
enabled = false
excluded_tool_namespaces = ["mcp__user_hidden"]
direct_only_tool_namespaces = ["mcp__user_direct"]
[mcp_servers.user-server]
command = "user-tool"
omit_tools_from = ["direct"]
[mcp_servers.mj-agents]
url = "https://obsolete.invalid"
enabled = false
"#;
                std::fs::write(&path, original).unwrap();
                let worker = std::env::current_exe().unwrap();
                let socket = root.path().join(SUBAGENT_SOCKET);
                assert!(configure_codex_mcp(root.path(), &home, Some(role), policy).unwrap());
                let first = std::fs::read_to_string(&path).unwrap();
                assert!(configure_codex_mcp(root.path(), &home, Some(role), policy).unwrap());
                assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
                let gh_rule = "prefix_rule(pattern=[\"gh\"], decision=\"allow\")\n";
                assert_eq!(
                    std::fs::read_to_string(rules.join("mj-gh.rules")).unwrap(),
                    gh_rule
                );
                assert_eq!(
                    std::fs::read_to_string(rules.join("user.rules")).unwrap(),
                    user_rule
                );
                let config: toml::Value = toml::from_str(&first).unwrap();
                let original: toml::Value = toml::from_str(original).unwrap();
                assert_eq!(config["model"], original["model"]);
                assert_eq!(config["features"]["hooks"], original["features"]["hooks"]);
                let code_mode = &config["features"]["code_mode"];
                assert_eq!(code_mode["enabled"].as_bool(), Some(false));
                assert_eq!(
                    code_mode["excluded_tool_namespaces"],
                    original["features"]["code_mode"]["excluded_tool_namespaces"]
                );
                assert_eq!(
                    code_mode["direct_only_tool_namespaces"].as_array().unwrap(),
                    &vec![
                        toml::Value::String("mcp__user_direct".into()),
                        toml::Value::String("mcp__mj_agents".into())
                    ]
                );
                assert_eq!(
                    config["mcp_servers"]["user-server"],
                    original["mcp_servers"]["user-server"]
                );
                let server = &config["mcp_servers"]["mj-agents"];
                assert_eq!(server["command"].as_str(), worker.to_str());
                assert_eq!(
                    server["omit_tools_from"].as_array().unwrap(),
                    &vec![toml::Value::String("deferred".into())]
                );
                assert!(server.get("url").is_none());
                assert!(server.get("enabled").is_none());
                let args: Vec<_> = server["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|arg| arg.as_str().unwrap())
                    .collect();
                assert_eq!(
                    args,
                    [
                        "worker",
                        "subagent-mcp",
                        "--socket",
                        socket.to_str().unwrap(),
                        "--harness",
                        "codex",
                        "--role",
                        role.id(),
                        "--agent-mailboxes-enabled",
                        "true"
                    ]
                );
                assert_eq!(
                    server
                        .get("default_tools_approval_mode")
                        .and_then(toml::Value::as_str),
                    (policy == ExecutionPolicy::ConfiguredApprovals).then_some("approve")
                );
                assert!(configure_codex_mcp(root.path(), &home, None, policy).unwrap());
                let disabled: toml::Value =
                    toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                assert!(disabled["mcp_servers"].get("mj-agents").is_none());
                assert_eq!(
                    std::fs::read_to_string(rules.join("mj-gh.rules")).unwrap(),
                    gh_rule
                );
                assert_eq!(
                    std::fs::read_to_string(rules.join("user.rules")).unwrap(),
                    user_rule
                );
                assert_eq!(
                    disabled["mcp_servers"]["user-server"],
                    original["mcp_servers"]["user-server"]
                );
            }
        }
    }

    #[test]
    fn codex_delegation_profile_handles_missing_and_invalid_configuration() {
        use mj_core::config::ExecutionPolicy;
        use mj_core::subagent::SubagentMcpRole;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        let path = home.join("config.toml");
        let configure =
            |role| configure_codex_mcp(root.path(), &home, role, ExecutionPolicy::Unconstrained);
        configure(None).unwrap();
        assert!(!path.exists());
        let rule = home.join("rules/mj-gh.rules");
        let expected = "prefix_rule(pattern=[\"gh\"], decision=\"allow\")\n";
        assert_eq!(std::fs::read_to_string(&rule).unwrap(), expected);
        std::fs::write(&rule, "obsolete managed rule").unwrap();
        configure(None).unwrap();
        assert_eq!(std::fs::read_to_string(&rule).unwrap(), expected);
        configure(Some(SubagentMcpRole::Child)).unwrap();
        let config: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config["mcp_servers"]["mj-agents"]["omit_tools_from"][0].as_str(),
            Some("deferred")
        );
        assert_eq!(
            config["features"]["code_mode"]["direct_only_tool_namespaces"][0].as_str(),
            Some("mcp__mj_agents")
        );
        for enabled in [false, true] {
            std::fs::write(&path, format!("[features]\ncode_mode = {enabled}\n")).unwrap();
            configure(Some(SubagentMcpRole::Parent)).unwrap();
            let config: toml::Value =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                config["features"]["code_mode"]["enabled"].as_bool(),
                Some(enabled)
            );
            assert_eq!(
                config["features"]["code_mode"]["direct_only_tool_namespaces"][0].as_str(),
                Some("mcp__mj_agents")
            );
        }
        for broken in [
            "[",
            "mcp_servers = 3",
            "features = 3",
            "[features]\ncode_mode = 3",
            "[features.code_mode]\ndirect_only_tool_namespaces = 3",
        ] {
            std::fs::write(&path, broken).unwrap();
            assert!(configure(Some(SubagentMcpRole::Parent)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        }
        let remote = root.path().join("profiles/session");
        std::fs::create_dir_all(&remote).unwrap();
        for policy in [
            ExecutionPolicy::ConfiguredApprovals,
            ExecutionPolicy::Unconstrained,
        ] {
            for role in [
                None,
                Some(SubagentMcpRole::Parent),
                Some(SubagentMcpRole::MessageOnly),
                Some(SubagentMcpRole::Child),
            ] {
                assert!(
                    configure_codex_mcp_with_mailboxes(
                        root.path(),
                        "session",
                        &remote,
                        role,
                        policy,
                        true,
                        true,
                        false
                    )
                    .unwrap()
                );
                assert_eq!(
                    std::fs::read_to_string(remote.join("rules/mj-gh.rules")).unwrap(),
                    expected
                );
            }
        }
        assert!(
            !configure_codex_mcp_with_mailboxes(
                root.path(),
                "different-session",
                &remote,
                None,
                ExecutionPolicy::ConfiguredApprovals,
                true,
                true,
                false
            )
            .unwrap()
        );
    }

    // Hard-won: a602a316a5: a linked profiles parent let private-profile writes reach a source profile.
    #[test]
    fn legacy_codex_homes_stay_on_acp_without_writing_the_users_profile() {
        use mj_core::config::ExecutionPolicy;
        use mj_core::subagent::SubagentMcpRole;
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("config.toml");
        let original = "model = \"original\"\n";
        std::fs::write(&path, original).unwrap();
        let configure = |home: &Path, role| {
            configure_codex_mcp(
                root.path(),
                home,
                role,
                ExecutionPolicy::ConfiguredApprovals,
            )
        };
        assert!(!configure(source.path(), Some(SubagentMcpRole::Parent)).unwrap());
        let staged = root.path().join("profile");
        std::os::unix::fs::symlink(source.path(), &staged).unwrap();
        for home in [source.path(), staged.as_path()] {
            for role in [
                Some(SubagentMcpRole::Parent),
                Some(SubagentMcpRole::Child),
                None,
            ] {
                assert!(!configure(home, role).unwrap());
                assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
                assert!(!source.path().join("rules").exists());
            }
        }
        std::fs::remove_file(&staged).unwrap();
        std::fs::create_dir(&staged).unwrap();
        assert!(!configure(source.path(), Some(SubagentMcpRole::Parent)).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(configure(&staged, Some(SubagentMcpRole::Parent)).unwrap());
        assert!(staged.join("config.toml").is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!source.path().join("rules").exists());
        let rules = staged.join("rules");
        std::fs::remove_dir_all(&rules).unwrap();
        std::os::unix::fs::symlink(source.path(), &rules).unwrap();
        assert!(configure(&staged, None).is_err());
        assert!(!source.path().join("mj-gh.rules").exists());
        std::fs::remove_file(&rules).unwrap();
        std::fs::create_dir(&rules).unwrap();
        std::os::unix::fs::symlink(&path, rules.join("mj-gh.rules")).unwrap();
        assert!(configure(&staged, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let source_session = source.path().join("session");
        std::fs::create_dir(&source_session).unwrap();
        let source_config = source_session.join("config.toml");
        std::fs::write(&source_config, original).unwrap();
        let profiles = root.path().join("profiles");
        std::os::unix::fs::symlink(source.path(), &profiles).unwrap();
        let remote_home = profiles.join("session");
        for role in [
            None,
            Some(SubagentMcpRole::Parent),
            Some(SubagentMcpRole::Child),
        ] {
            assert!(!private_staged_home(root.path(), "session", &remote_home));
            assert!(
                !configure_codex_mcp_with_mailboxes(
                    root.path(),
                    "session",
                    &remote_home,
                    role,
                    ExecutionPolicy::ConfiguredApprovals,
                    true,
                    true,
                    false,
                )
                .unwrap()
            );
            assert_eq!(std::fs::read_to_string(&source_config).unwrap(), original);
            assert!(!source_session.join("rules").exists());
        }

        // A link above the real profiles directory still names a private home.
        std::fs::remove_file(&profiles).unwrap();
        std::fs::create_dir_all(&remote_home).unwrap();
        let ancestor = source.path().join("worker-alias");
        std::os::unix::fs::symlink(root.path(), &ancestor).unwrap();
        assert!(private_staged_home(
            root.path(),
            "session",
            &ancestor.join("profiles/session")
        ));
    }

    fn request(id: &str) -> SubagentToolRequest {
        SubagentToolRequest {
            originating_command_id: None,
            request_id: id.into(),
            created_at_ms: 1,
            action: SubagentToolAction::ListAgents,
        }
    }

    #[test]
    fn closed_mutation_admission_rejects_changes_but_keeps_observations_open() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(root.path()).unwrap();
        assert!(endpoint.state.lock().unwrap().mutating_admission_open);
        endpoint.set_mutating_admission(false).unwrap();
        let reopened = SubagentEndpoint::open(root.path()).unwrap();
        assert!(!reopened.state.lock().unwrap().mutating_admission_open);

        let mutations = [
            SubagentToolAction::Spawn {
                task_type: mj_core::subagent::SubagentTaskType::Straightforward,
                task_name: "task".into(),
                instructions: "work".into(),

                working_directory: PathBuf::new(),
                context: None,
                files: Vec::new(),
            },
            SubagentToolAction::SendInput {
                child_session_id: "child".into(),
                message: "continue".into(),
            },
            SubagentToolAction::SendMessage {
                child_session_id: "child".into(),
                message: "note".into(),
            },
            SubagentToolAction::LegacyInterruptAgent {
                child_session_id: "child".into(),
            },
            SubagentToolAction::CloseAgent {
                child_session_id: "child".into(),
            },
        ];
        for (index, action) in mutations.into_iter().enumerate() {
            let mut request = request(&format!("mutation-{index}"));
            request.action = action;
            let (accepted, result, _) = reopened.enqueue_marked(request).unwrap();
            assert!(!accepted);
            assert!(result.unwrap().is_error);
        }
        for (id, action) in [
            ("list", SubagentToolAction::ListAgents),
            ("wait", SubagentToolAction::LegacyWaitAgents),
        ] {
            let mut request = request(id);
            request.action = action;
            let (accepted, result, _) = reopened.enqueue_marked(request).unwrap();
            assert!(accepted);
            assert_eq!(result, None);
        }
        let (queued, _) = reopened.snapshot();
        assert_eq!(queued.len(), 2);
        reopened.set_mutating_admission(true).unwrap();
        let mut request = request("reopened");
        request.action = SubagentToolAction::SendMessage {
            child_session_id: "child".into(),
            message: "note".into(),
        };
        assert!(reopened.enqueue_marked(request).unwrap().0);
    }

    #[test]
    fn accepted_mutation_retry_remains_attached_after_admission_closes() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(root.path()).unwrap();
        let mut accepted = request("accepted-before-close");
        accepted.action = SubagentToolAction::CloseAgent {
            child_session_id: "child".into(),
        };
        assert!(endpoint.enqueue_marked(accepted.clone()).unwrap().0);
        endpoint.set_mutating_admission(false).unwrap();

        let (admitted, result, _) = endpoint.enqueue_marked(accepted).unwrap();
        assert!(admitted);
        assert!(result.is_none());
        let (queued, _) = endpoint.snapshot();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].request_id, "accepted-before-close");

        let mut new_request = request("new-after-close");
        new_request.action = SubagentToolAction::CloseAgent {
            child_session_id: "child".into(),
        };
        let (admitted, result, _) = endpoint.enqueue_marked(new_request).unwrap();
        assert!(!admitted);
        assert!(result.unwrap().is_error);
    }

    #[test]
    fn admission_stamps_worker_turn_and_replay_cannot_retarget_it() {
        use crate::relay::test_support::{prompt, submit_relay};
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(Mutex::new(
            crate::relay::DurableRelay::open(root.path(), "session-123", "test").unwrap(),
        ));
        {
            let mut owner = relay.lock().unwrap();
            owner
                .record_observation(mj_core::relay::RelayObservation::SessionConfigured {
                    config_options: vec![],
                })
                .unwrap();
            submit_relay(&mut owner, "original-turn", prompt("first"));
            owner.claim_pending_commands(true).unwrap();
        }
        let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
        endpoint.relay = Some(relay.clone());
        let mut report = request("handback-request");
        report.action = SubagentToolAction::Handback {
            message: "original report".into(),
        };
        report.originating_command_id = Some("untrusted-forged-turn".into());
        endpoint.enqueue(report.clone()).unwrap();
        {
            let mut owner = relay.lock().unwrap();
            owner
                .record_command_completed(
                    "original-turn",
                    mj_core::relay::RelayCommandOutcome::Prompt {
                        stop_reason: "end_turn".into(),
                        usage: None,
                        diagnostic: None,
                    },
                )
                .unwrap();
            submit_relay(&mut owner, "replacement-turn", prompt("second"));
            owner.claim_pending_commands(true).unwrap();
        }
        endpoint.enqueue(report).unwrap();
        let pending = SubagentEndpoint::open(root.path()).unwrap().snapshot().0;
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].originating_command_id.as_deref(),
            Some("original-turn")
        );
    }

    #[test]
    fn failed_persistence_never_publishes_admission_or_completion() {
        let root = tempfile::tempdir().unwrap();
        let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
        let valid = endpoint.path.clone();
        endpoint.path = root.path().to_path_buf(); // Atomic rename cannot replace a directory.
        assert!(endpoint.enqueue(request("failed-admission")).is_err());
        assert!(endpoint.snapshot().0.is_empty());
        endpoint.path = valid;
        endpoint.enqueue(request("accepted-request")).unwrap();
        endpoint.path = root.path().to_path_buf();
        assert!(endpoint.complete(done("accepted-request")).is_err());
        let (pending, results) = endpoint.snapshot();
        assert_eq!(pending.len(), 1);
        assert!(results.is_empty());
    }

    #[test]
    fn legacy_queue_delivery_bits_decode_without_affecting_completions() {
        let queue: QueueState = serde_json::from_value(serde_json::json!({
            "delivered_to_waiter": {"old-wait": false},
            "requests": {"old-wait": {"request_id":"old-wait", "created_at_ms":1, "action":{"action":"wait_agents"}}}
        })).unwrap();
        assert_eq!(
            queue.requests["old-wait"].action,
            SubagentToolAction::LegacyWaitAgents
        );
        assert!(
            serde_json::to_value(queue)
                .unwrap()
                .get("delivered_to_waiter")
                .is_none()
        );
    }

    #[test]
    fn queue_survives_reopen_and_returns_cached_completion_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        assert_eq!(endpoint.enqueue(request("request-1")).unwrap(), None);
        assert_eq!(endpoint.snapshot().0, vec![request("request-1")]);

        let result = SubagentToolResult {
            request_id: "request-1".into(),
            completed_at_ms: 2,
            is_error: false,
            message: "done".into(),
        };
        endpoint.complete(result.clone()).unwrap();

        let reopened = SubagentEndpoint::open(directory.path()).unwrap();
        assert!(reopened.snapshot().0.is_empty());
        assert_eq!(
            reopened.enqueue(request("request-1")).unwrap(),
            Some(result)
        );

        let legacy_request = serde_json::json!({
            "originating_command_id":null,
            "request_id":"legacy-message",
            "created_at_ms":1,
            "action":{"action":"send_message","params":{
                "child_session_id":"child","message":"continue"
            }}
        });
        let cached_alias = serde_json::json!({
            "originating_command_id":null,
            "request_id":"cached-input",
            "created_at_ms":2,
            "action":{"action":"send_input","params":{
                "child_session_id":"child","message":"continue through the cached alias"
            }}
        });
        std::fs::write(
            directory.path().join(SUBAGENT_QUEUE),
            serde_json::to_vec(&serde_json::json!({
                "requests":{
                    "legacy-message":legacy_request,
                    "cached-input":cached_alias
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let legacy_queue = SubagentEndpoint::open(directory.path()).unwrap();
        let requests = legacy_queue.snapshot().0;
        assert_eq!(
            requests
                .iter()
                .find(|request| request.request_id == "legacy-message")
                .unwrap()
                .action,
            SubagentToolAction::SendMessage {
                child_session_id: "child".into(),
                message: "continue".into(),
            }
        );
        assert_eq!(
            requests
                .iter()
                .find(|request| request.request_id == "cached-input")
                .unwrap()
                .action,
            SubagentToolAction::SendInput {
                child_session_id: "child".into(),
                message: "continue through the cached alias".into(),
            }
        );
    }

    fn done(id: &str) -> SubagentToolResult {
        SubagentToolResult {
            request_id: id.into(),
            completed_at_ms: 2,
            is_error: false,
            message: "done".into(),
        }
    }

    #[tokio::test]
    async fn send_input_acknowledges_storage_without_completing_the_request() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        let mut input = request("input-1");
        input.action = SubagentToolAction::SendInput {
            child_session_id: "starting-child".into(),
            message: "x".repeat(128 * 1024),
        };
        let (client, server) = UnixStream::pair().unwrap();
        let served = tokio::spawn(serve_one(server, endpoint.clone()));
        let (read, mut write) = client.into_split();
        write
            .write_all(format!("{}\n", serde_json::to_string(&input).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(read).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        served.await.unwrap().unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["acknowledgment_required"], false);
        let payload: serde_json::Value =
            serde_json::from_str(reply["result"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(payload["status"], "queued");
        assert_eq!(payload["child_session_id"], "starting-child");
        assert!(payload.get("turn_id").is_none());
        let reopened = SubagentEndpoint::open(directory.path()).unwrap();
        assert_eq!(reopened.snapshot(), (vec![input.clone()], vec![]));
        assert_eq!(reopened.enqueue(input.clone()).unwrap(), None);
        let completed = done("input-1");
        reopened.complete(completed.clone()).unwrap();
        assert_eq!(
            SubagentEndpoint::open(directory.path())
                .unwrap()
                .enqueue(input)
                .unwrap(),
            Some(completed)
        );
    }

    // Hard-won: 131223a: sub-agent MCP calls returned accepted placeholders instead of their daemon result.
    #[tokio::test]
    async fn a_socket_waiter_is_registered_before_the_request_becomes_visible() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = tokio::spawn(serve_one(server, endpoint.clone()));
        let mut body = serde_json::to_vec(&request("r1")).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !endpoint
                .snapshot()
                .0
                .iter()
                .any(|request| request.request_id == "r1")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            endpoint.state.lock().unwrap().live_waiters.get("r1"),
            Some(&1),
            "a daemon may complete immediately once it sees the queued request"
        );
        assert!(endpoint.complete(done("r1")).unwrap());
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(&mut client).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        served.await.unwrap().unwrap();
        let reply: SocketReply = serde_json::from_str(&line).unwrap();
        assert_eq!(reply.result, Some(done("r1")));
    }

    /// #1197: the MCP server gives up after `REPLY_TIMEOUT`. This worker
    /// answers first, and says whether the daemon
    /// has read the queue since the request joined it; a read before the
    /// request does not count.
    // Hard-won: dfd2e32: a stalled daemon launched a queued child after the parent had moved on.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_request_is_answered_here_with_whether_the_daemon_picked_it_up() {
        use tokio::io::{AsyncWriteExt, BufReader};

        for daemon_reads_it in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
            endpoint.record_collection();
            let socket = directory.path().join("test.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let served = tokio::spawn({
                let endpoint = endpoint.clone();
                async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    serve_one(stream, endpoint).await.unwrap();
                }
            });

            let mut client = UnixStream::connect(&socket).await.unwrap();
            let mut body = serde_json::to_vec(&request("r-list")).unwrap();
            body.push(b'\n');
            client.write_all(&body).await.unwrap();
            client.flush().await.unwrap();
            let started = Instant::now();
            while endpoint.snapshot().0.is_empty() {
                tokio::task::yield_now().await;
            }
            if daemon_reads_it {
                endpoint.record_collection();
            }

            let mut line = String::new();
            BufReader::new(&mut client)
                .read_line(&mut line)
                .await
                .unwrap();
            served.await.unwrap();
            assert!(
                started.elapsed() < crate::subagent_mcp::REPLY_TIMEOUT,
                "the worker must answer before the MCP server gives up, took {:?}",
                started.elapsed()
            );
            let reply: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(reply["accepted"], true, "{reply}");
            assert!(reply.get("result").is_none(), "{reply}");
            let contact: DaemonContact = serde_json::from_value(reply["daemon"].clone()).unwrap();
            assert_eq!(contact.picked_up, daemon_reads_it, "{reply}");
            assert!(contact.last_collected_seconds_ago.is_some(), "{reply}");
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketReply {
    accepted: bool,
    #[serde(default)]
    acknowledgment_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<SubagentToolResult>,
    /// Set only when `result` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daemon: Option<DaemonContact>,
}

pub(super) fn serve(
    runtime: &tokio::runtime::Handle,
    root: &Path,
    relay: Arc<Mutex<crate::relay::DurableRelay>>,
    _harness: mj_core::config::HarnessKind,
) -> Result<(SubagentEndpoint, SubagentSocketGuard)> {
    let mut endpoint = SubagentEndpoint::open(root)?;
    // Preserve a closed gate if this worker restarts during Move recovery. A
    // replacement worker explicitly reopens admission after it reconnects.
    endpoint.relay = Some(relay);
    let path = root.join(SUBAGENT_SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = mj_core::local_sockets::bind_unix_listener(&path)
        .with_context(|| format!("bind sub-agent socket {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set sub-agent socket {} nonblocking", path.display()))?;
    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("register sub-agent socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let service = endpoint.clone();
    let task = runtime.spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                finished = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = finished {
                        tracing::error!(%error, "sub-agent socket task panicked");
                    }
                }
                accepted = listener.accept(), if tasks.len() < MAX_SOCKET_TASKS => {
                    let stream = match accepted {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            tracing::warn!(%error, "sub-agent socket accept failed");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };
                    let service = service.clone();
                    tasks.spawn(async move {
                        if let Err(error) = serve_one(stream, service).await {
                            tracing::warn!(error = %format!("{error:#}"), "sub-agent MCP dispatch failed");
                        }
                    });
                }
            }
        }
    });
    Ok((
        endpoint,
        SubagentSocketGuard {
            task,
            socket: Some(super::unix::SocketGuard(path)),
        },
    ))
}

pub(super) struct SubagentSocketGuard {
    task: tokio::task::JoinHandle<()>,
    socket: Option<super::unix::SocketGuard>,
}

impl Drop for SubagentSocketGuard {
    fn drop(&mut self) {
        self.task.abort();
        self.socket.take();
    }
}

async fn serve_one(stream: UnixStream, endpoint: SubagentEndpoint) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let line = tokio::time::timeout(
        SOCKET_IO_DEADLINE,
        super::unix::read_bounded_line(&mut reader, mj_core::relay::RELAY_COMMAND_BYTE_BUDGET),
    )
    .await
    .context("sub-agent request read timed out")??
    .context("sub-agent request ended before a frame")?;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SocketRequest {
        Acknowledge { acknowledge_result: String },
        Tool(SubagentToolRequest),
    }
    let request = match serde_json::from_str(&line).context("parse sub-agent request")? {
        SocketRequest::Tool(request) => request,
        SocketRequest::Acknowledge { acknowledge_result } => {
            endpoint.acknowledge_replies(&[acknowledge_result], true)?;
            return write_reply(
                &mut write,
                SocketReply {
                    accepted: true,
                    acknowledgment_required: false,
                    result: None,
                    daemon: None,
                },
            )
            .await;
        }
    };
    let lane = if matches!(
        request.action,
        mj_core::subagent::SubagentToolAction::Spawn { .. }
    ) {
        &endpoint.admission.long
    } else {
        &endpoint.admission.control
    };
    let Ok(_admission) = lane.clone().try_acquire_owned() else {
        let body = SocketReply { accepted: false, acknowledgment_required: false, result: Some(SubagentToolResult {
            request_id: request.request_id, completed_at_ms: mj_core::clock::epoch_millis(),
            is_error: true, message: "Sub-agent request was not accepted: this operation's queue is full; retry later.".into(),
        }), daemon: None };
        return write_reply(&mut write, body).await;
    };
    let request_id = request.request_id.clone();
    let deadline_budget = crate::subagent_mcp::REPLY_TIMEOUT - WORKER_REPLY_MARGIN;
    let queued_input = match &request.action {
        mj_core::subagent::SubagentToolAction::SendInput { child_session_id, .. } => {
            Some(SubagentToolResult {
                request_id: request_id.clone(),
                completed_at_ms: mj_core::clock::epoch_millis(),
                is_error: false,
                message: serde_json::json!({
                    "request_id": request_id,
                    "child_session_id": child_session_id,
                    "status": "queued",
                    "next_action": "Input is stored, not yet confirmed delivered. Use list_agents to check delivery; do not resend it."
                }).to_string(),
            })
        }
        _ => None,
    };
    let started = Instant::now();
    // Register before durable enqueue: the daemon can collect and complete a
    // request as soon as the queue write becomes visible.
    let mut waiter = queued_input
        .is_none()
        .then(|| endpoint.register_waiter(&request_id));
    let (accepted, queued, mark) = endpoint.enqueue_marked(request)?;
    if !accepted {
        return write_reply(
            &mut write,
            SocketReply {
                accepted: false,
                acknowledgment_required: false,
                result: queued,
                daemon: None,
            },
        )
        .await;
    }
    let (result, acknowledgment_required) = match queued {
        Some(cached) => (Some(cached), true),
        None if queued_input.is_some() => (queued_input, false),
        None => {
            let result = endpoint
                .await_result_registered(
                    &request_id,
                    started + deadline_budget,
                    waiter.as_mut().expect("a waiting request has a waiter"),
                )
                .await?;
            let acknowledgment_required = result.is_some();
            (result, acknowledgment_required)
        }
    };
    // Without a result, say whether the daemon has picked the request up: a
    // daemon that is restarting or stalled otherwise looks the same as a slow
    // action, and the model should act differently on the two.
    let daemon = result.is_none().then(|| endpoint.daemon_contact(mark));
    write_reply(
        &mut write,
        SocketReply {
            accepted: true,
            acknowledgment_required,
            result,
            daemon,
        },
    )
    .await
}

async fn write_reply(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    reply: SocketReply,
) -> Result<()> {
    let mut body = serde_json::to_vec(&reply)?;
    body.push(b'\n');
    tokio::time::timeout(SOCKET_IO_DEADLINE, async {
        write
            .write_all(&body)
            .await
            .context("answer sub-agent request")?;
        write.flush().await.context("flush sub-agent response")
    })
    .await
    .context("sub-agent response write timed out")?
}

#[cfg(test)]
mod review_gate_tests {
    use super::*;
    use mj_core::subagent::SubagentMcpRole;

    #[test]
    fn private_profile_registration_passes_the_gate_only_to_parents() {
        for role in [
            SubagentMcpRole::Parent,
            SubagentMcpRole::Child,
            SubagentMcpRole::MessageOnly,
            SubagentMcpRole::FetchOnly,
        ] {
            for gate in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let home = root.path().join("profile");
                std::fs::create_dir(&home).unwrap();
                configure_claude_subagent_mcp(
                    root.path(),
                    "session",
                    &home,
                    Some(role),
                    true,
                    true,
                    gate,
                )
                .unwrap();
                let config: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap())
                        .unwrap();
                let expected = gate && role == SubagentMcpRole::Parent;
                assert_eq!(
                    config["mcpServers"]["mj-agents"]["args"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|value| value == "--review-gate"),
                    expected
                );
                let settings: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap())
                        .unwrap();
                assert_eq!(
                    settings["permissions"]["allow"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|value| value == "mcp__mj-agents__request_review"),
                    expected
                );
                configure_codex_mcp_with_mailboxes(
                    root.path(),
                    "",
                    &home,
                    Some(role),
                    mj_core::config::ExecutionPolicy::ConfiguredApprovals,
                    true,
                    true,
                    gate,
                )
                .unwrap();
                let config: toml::Table =
                    toml::from_str(&std::fs::read_to_string(home.join("config.toml")).unwrap())
                        .unwrap();
                assert_eq!(
                    config["mcp_servers"]["mj-agents"]["args"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|value| value.as_str() == Some("--review-gate")),
                    expected
                );
            }
        }
    }
}
