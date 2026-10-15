//! Parent-worker queue behind the Mjolnir sub-agent MCP server.

use std::collections::BTreeMap;
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

use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

use crate::subagent_mcp::DaemonContact;

pub const SUBAGENT_SOCKET: &str = "subagents.sock";

const CODEX_CONFIG_ENV: &str = "CODEX_CONFIG";
const CODEX_MAILBOX_HOOK_MARKER: &str = "MJOLNIR_MAILBOX_HOOK=1 ";
const CLAUDE_MAILBOX_HOOK_MARKER: &str = "MJOLNIR_MAILBOX_HOOK=1 ";

/// Finalize the controller's staged registration on the target, where the
/// worker root is absolute. Claude launches MCP servers from the checkout,
/// not from the home against which remote staging paths were written.
///
/// A delegating parent's registration is `required`. The message-only server
/// is not: a profile staged by an older controller lacks it, and a worker
/// upgrade replaces the launch configuration without restaging the profile.
/// Such a session runs without `send_message` until it is restaged, rather
/// than failing its harness preparation.
pub(super) fn resolve_claude_mcp_paths(root: &Path, home: &Path, required: bool) -> Result<()> {
    anyhow::ensure!(
        root.is_absolute(),
        "Claude MCP worker root must be absolute"
    );
    let path = home.join(".claude.json");
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read staged Claude configuration {}", path.display()));
        }
    };
    let mut config: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("parse staged Claude configuration {}", path.display()))?;
    let Some(server) = config
        .get_mut("mcpServers")
        .and_then(|servers| servers.get_mut(mj_core::subagent::SUBAGENT_MCP_SERVER))
        .and_then(serde_json::Value::as_object_mut)
    else {
        anyhow::ensure!(
            !required,
            "missing staged Claude delegation server in {}",
            path.display()
        );
        tracing::info!(
            path = %path.display(),
            "the staged Claude profile predates the message-only server; send_message is unavailable until the session is restaged"
        );
        return Ok(());
    };
    let args = server
        .get_mut("args")
        .and_then(serde_json::Value::as_array_mut)
        .context("staged Claude delegation server must have arguments")?;
    let socket = args
        .iter()
        .position(|arg| arg.as_str() == Some("--socket"))
        .and_then(|index| args.get_mut(index + 1))
        .filter(|arg| arg.is_string())
        .context("staged Claude delegation server must have a socket argument")?;
    *socket = serde_json::to_value(root.join(SUBAGENT_SOCKET))?;
    server.insert("command".into(), serde_json::to_value(root.join("hel"))?);
    let mut resolved = serde_json::to_vec_pretty(&config)?;
    resolved.push(b'\n');
    if resolved != body {
        mj_core::config::atomic_write(&path, &resolved)
            .with_context(|| format!("write staged Claude configuration {}", path.display()))?;
    }
    Ok(())
}

/// Install the mailbox command hook in the session-private Claude settings and
/// resolve staged remote paths on the worker target before Claude reads them.
pub(super) fn configure_claude_mailbox_hook(
    root: &Path,
    home: &Path,
    enabled: bool,
) -> Result<bool> {
    let path = home.join("settings.json");
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
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
    if !enabled {
        let Some(groups) = root_object
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|hooks| hooks.get_mut("PostToolBatch"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            return Ok(false);
        };
        let mut changed = false;
        groups.retain_mut(|group| {
            let Some(hooks) = group
                .get_mut("hooks")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return true;
            };
            let previous_len = hooks.len();
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|command| command.starts_with(CLAUDE_MAILBOX_HOOK_MARKER))
            });
            let removed = previous_len != hooks.len();
            changed |= removed;
            !(removed && hooks.is_empty())
        });
        if changed {
            let mut resolved = serde_json::to_vec_pretty(&settings)?;
            resolved.push(b'\n');
            mj_core::config::atomic_write(&path, &resolved)
                .with_context(|| format!("write staged Claude settings {}", path.display()))?;
        }
        return Ok(false);
    }
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
    let Some(existing) = groups.iter_mut().find_map(|group| {
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
    }) else {
        // A missing marker means this home was not staged with the worker hook.
        // Do not create or edit a profile file that might belong to the user.
        return Ok(false);
    };
    existing["command"] = serde_json::Value::String(command);
    existing["timeout"] = serde_json::json!(mj_core::mailbox::MAILBOX_HOOK_TIMEOUT_SECS);
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
    configure_codex_mcp_with_mailboxes(root, home, role, policy, true)
}

fn configure_codex_mcp_with_mailboxes(
    root: &Path,
    home: &Path,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    policy: mj_core::config::ExecutionPolicy,
    mailboxes_enabled: bool,
) -> Result<bool> {
    // Earlier local workers used the person's original profile, directly or
    // through <root>/profile as a symlink. Keep those on ACP until restaging;
    // never write configuration through that compatibility link.
    let staged = root.join("profile");
    match std::fs::symlink_metadata(&staged) {
        Ok(metadata) if !metadata.is_dir() => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspect private Codex profile"),
        Ok(_) => {}
    }
    let canonical = |path: &Path| {
        std::fs::canonicalize(path)
            .with_context(|| format!("resolve Codex profile {}", path.display()))
    };
    if canonical(home)? != canonical(&staged)? {
        return Ok(false);
    }
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
        let mut server = serde_json::json!({
            "command": worker,
            "args": ["worker", "subagent-mcp", "--socket", root.join(SUBAGENT_SOCKET),
                     "--harness", "codex", "--role", role.id(),
                     "--agent-mailboxes-enabled", mailboxes_enabled.to_string()],
            // Expose the tools up front, including when code mode is enabled.
            "omit_tools_from": ["deferred"]
        });
        if policy == mj_core::config::ExecutionPolicy::ConfiguredApprovals {
            server["default_tools_approval_mode"] = serde_json::json!("approve");
        }
        servers.insert(name.into(), toml::Value::try_from(server)?);

        // A nested blocking wait yields its exec cell and makes the model poll
        // that cell. Keep this namespace outside the code-mode executor so
        // Codex suspends inference until the actual MCP call returns.
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

pub(super) fn configure_codex_profile(
    root: &Path,
    home: &Path,
    environment: &mut BTreeMap<String, String>,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    policy: mj_core::config::ExecutionPolicy,
    mailboxes_enabled: bool,
) -> Result<bool> {
    configure_codex_mailbox_hook(root, environment, mailboxes_enabled)?;
    configure_codex_mcp_with_mailboxes(root, home, role, policy, mailboxes_enabled)
}

/// Put the mailbox hook in codex-acp's per-thread config override. Its state
/// trusts only this generated handler; using Codex's global trust bypass would
/// also enable untrusted handlers from the user's and project's config layers.
fn configure_codex_mailbox_hook(
    root: &Path,
    environment: &mut BTreeMap<String, String>,
    enabled: bool,
) -> Result<()> {
    if !enabled {
        let Some(existing) = environment.get(CODEX_CONFIG_ENV) else {
            return Ok(());
        };
        let mut config = serde_json::from_str::<serde_json::Value>(existing)
            .ok()
            .and_then(|value| match value {
                serde_json::Value::Object(map) => Some(map),
                _ => None,
            })
            .with_context(|| {
                format!("{CODEX_CONFIG_ENV} must be a JSON object for mailbox hooks")
            })?;
        let Some(hooks) = config
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return Ok(());
        };
        let mut removed_paths = Vec::new();
        if let Some(groups) = hooks
            .get_mut("PostToolUse")
            .and_then(serde_json::Value::as_array_mut)
        {
            for (group_index, group) in groups.iter().enumerate() {
                if let Some(handlers) = group.get("hooks").and_then(serde_json::Value::as_array) {
                    for (handler_index, handler) in handlers.iter().enumerate() {
                        if handler
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|command| command.starts_with(CODEX_MAILBOX_HOOK_MARKER))
                        {
                            removed_paths.push(format!(
                                "/<session-flags>/config.toml:post_tool_use:{group_index}:{handler_index}"
                            ));
                        }
                    }
                }
            }
            if !removed_paths.is_empty() {
                groups.retain_mut(|group| {
                    let Some(handlers) = group
                        .get_mut("hooks")
                        .and_then(serde_json::Value::as_array_mut)
                    else {
                        return true;
                    };
                    let previous_len = handlers.len();
                    handlers.retain(|handler| {
                        !handler
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|command| command.starts_with(CODEX_MAILBOX_HOOK_MARKER))
                    });
                    !(handlers.len() != previous_len && handlers.is_empty())
                });
            }
        }
        if removed_paths.is_empty() {
            return Ok(());
        }
        if let Some(state) = hooks
            .get_mut("state")
            .and_then(serde_json::Value::as_object_mut)
        {
            for path in removed_paths {
                state.remove(&path);
            }
        }
        environment.insert(
            CODEX_CONFIG_ENV.to_owned(),
            serde_json::Value::Object(config).to_string(),
        );
        return Ok(());
    }
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

/// How much sooner than the MCP server gives up this worker answers an action
/// other than `wait` by itself. The MCP server waits
/// [`crate::subagent_mcp::REPLY_TIMEOUT`]; answering first lets it tell the
/// model what this worker knows (whether the daemon has picked the request up)
/// instead of only that nothing came back (#1197).
const WORKER_REPLY_MARGIN: Duration = Duration::from_secs(10);

/// Slack a `wait` gets on top of the caller's own timeout before this worker
/// stops waiting for the daemon and answers by itself. It covers the hop that
/// carries the daemon's result back here; past it, answering late helps nobody.
const WORKER_WAIT_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueState {
    #[serde(default)]
    requests: BTreeMap<String, SubagentToolRequest>,
    #[serde(default)]
    results: BTreeMap<String, SubagentToolResult>,
    /// Whether each persisted result reached a live waiter when it arrived.
    /// Older queue files lack this fact and are treated as already delivered.
    #[serde(default)]
    delivered_to_waiter: BTreeMap<String, bool>,
    /// Protected by the same lock as `requests`, making gate closure atomic
    /// with queue admission. Old queue files default to the open state.
    #[serde(default = "default_mutating_admission")]
    mutating_admission_open: bool,
    /// The daemon's reads of this queue since this worker started. It lives
    /// under the queue's lock so a request is either in a read or after it.
    #[serde(skip)]
    collections: Collections,
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
            delivered_to_waiter: BTreeMap::new(),
            mutating_admission_open: true,
            collections: Collections::default(),
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
    harness: Option<mj_core::config::HarnessKind>,
    state: Arc<Mutex<QueueState>>,
    /// Woken whenever a result lands, so a socket call awaiting its own request
    /// returns the moment the daemon completes it.
    completed: Arc<Notify>,
}

impl SubagentEndpoint {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join(SUBAGENT_QUEUE);
        let state = match std::fs::read(&path) {
            Ok(body) => serde_json::from_slice(&body)
                .with_context(|| format!("parse sub-agent queue {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => QueueState::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read sub-agent queue {}", path.display()));
            }
        };
        Ok(Self {
            path,
            admission: Arc::default(),
            relay: None,
            harness: None,
            state: Arc::new(Mutex::new(state)),
            completed: Arc::new(Notify::new()),
        })
    }

    fn cached_result(&self, request_id: &str) -> Option<SubagentToolResult> {
        self.state
            .lock()
            .expect("sub-agent queue lock poisoned")
            .results
            .get(request_id)
            .cloned()
    }

    /// Wait for the daemon to complete `request_id`, up to `deadline`. Returns
    /// the result, or `None` at the deadline. Interest is registered before
    /// each read of the queue, so a completion racing the check is never lost.
    #[cfg(test)]
    pub async fn await_result(
        &self,
        request_id: &str,
        deadline: Instant,
    ) -> Option<SubagentToolResult> {
        let mut waiter = self.register_waiter(request_id);
        self.await_result_registered(request_id, deadline, &mut waiter)
            .await
            .unwrap()
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
        // Subscribe before checking: a steer admitted between the check and
        // sleep remains visible even if the daemon is being replaced.
        let mut relay_changes = self.relay.as_ref().map(|relay| {
            relay
                .lock()
                .expect("relay lock poisoned")
                .subscribe_changes()
        });
        loop {
            let notified = self.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.cached_result(request_id) {
                waiter.finish();
                return Ok(Some(result));
            }
            if let Some(result) = self.interrupt_wait_at_turn_boundary(request_id, waiter)? {
                return Ok(Some(result));
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = sleep_until(deadline) => return Ok(waiter.finish_and_take_result()),
                _ = async {
                    match relay_changes.as_mut() {
                        Some(changes) => { let _ = changes.changed().await; }
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    }

    fn interrupt_wait_at_turn_boundary(
        &self,
        request_id: &str,
        waiter: &mut LiveWaiter,
    ) -> Result<Option<SubagentToolResult>> {
        let Some(relay) = &self.relay else {
            return Ok(None);
        };
        let relay = relay.lock().expect("relay lock poisoned");
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        // Completion takes these locks in the same order. A turn boundary
        // cannot race a report's delivery receipt, even before this task wakes.
        if let Some(result) = state.results.get(request_id).cloned() {
            LiveWaiter::remove_from(&mut waiter.active, request_id, &mut state);
            return Ok(Some(result));
        }
        let Some(result) = Self::interrupted_wait_result(&relay, &state, request_id) else {
            return Ok(None);
        };
        // Publish the worker's answer durably so the daemon retires its wait
        // too. A later daemon report was NOT delivered by this answer: its
        // receipt must restore any child report markers it tentatively set.
        self.store_result(&mut state, result.clone(), false)?;
        LiveWaiter::remove_from(&mut waiter.active, request_id, &mut state);
        drop(state);
        self.completed.notify_waiters();
        Ok(Some(result))
    }

    fn interrupted_wait_result(
        relay: &crate::relay::DurableRelay,
        state: &QueueState,
        request_id: &str,
    ) -> Option<SubagentToolResult> {
        let request = state.requests.get(request_id)?;
        if !matches!(
            request.action,
            mj_core::subagent::SubagentToolAction::WaitAgents
        ) {
            return None;
        }
        // Legacy/untracked calls have no turn identity; keep their existing
        // socket-based delivery semantics rather than guessing an origin.
        let turn = request.originating_command_id.as_deref()?;
        let next_action = if !relay.subagent_wait_turn_is_live(turn) {
            "The originating parent turn ended or was cancelled. Your children continue working; their reports have not been collected by this reply. Call wait from the next turn to collect them."
        } else if relay.boundary_steer_needs_turn(turn) {
            "A user correction is waiting at this tool boundary. Read it before calling wait again. Your children continue working; their reports have not been collected by this reply."
        } else {
            return None;
        };
        let now = mj_core::clock::epoch_millis();
        let waited_seconds = now.saturating_sub(request.created_at_ms).max(0) as u64 / 1_000;
        let payload = serde_json::json!({
            "status": "interrupted",
            "waited_seconds": waited_seconds,
            "agents": [],
            "next_action": next_action,
        });
        Some(SubagentToolResult {
            request_id: request_id.to_owned(),
            completed_at_ms: now,
            is_error: false,
            message: payload.to_string(),
        })
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> (Vec<SubagentToolRequest>, Vec<SubagentToolResult>) {
        let state = self.state.lock().expect("sub-agent queue lock poisoned");
        (
            state.requests.values().cloned().collect(),
            state.results.values().cloned().collect(),
        )
    }

    /// The daemon's read of the queue: every queued request and recorded result. It
    /// also records the read, so a caller still waiting can learn whether the
    /// daemon has seen its request.
    pub fn collect_for_daemon(&self) -> (Vec<SubagentToolRequest>, Vec<SubagentToolResult>) {
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        state.collections.count += 1;
        state.collections.last = Some(Instant::now());
        (
            state.requests.values().cloned().collect(),
            state.results.values().cloned().collect(),
        )
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
        self.persist(&next)?;
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
        if let Some(result) = state.results.get(&request.request_id) {
            return Ok((true, Some(result.clone()), mark));
        }
        // A retry of a request already accepted before the gate closed is not
        // a new admission. Keep it attached to the original queued operation.
        if state.requests.contains_key(&request.request_id) {
            return Ok((true, None, mark));
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
        self.persist(&next)?;
        *state = next;
        Ok((true, None, mark))
    }

    pub fn complete(&self, result: SubagentToolResult) -> Result<bool> {
        let request_id = result.request_id.clone();
        let relay = self
            .relay
            .as_ref()
            .map(|relay| relay.lock().expect("relay lock poisoned"));
        let mut state = self.state.lock().expect("sub-agent queue lock poisoned");
        if let Some(delivered) = state.delivered_to_waiter.get(&request_id).copied() {
            return Ok(delivered);
        }
        // A result retained by an older worker has no delivery bit. Preserve
        // legacy behavior: a duplicate completion is assumed to have reached
        // its caller rather than reopening a report that may already be read.
        if state.results.contains_key(&request_id) {
            return Ok(true);
        }
        if let Some(relay) = relay.as_deref()
            && let Some(interrupted) = Self::interrupted_wait_result(relay, &state, &request_id)
        {
            self.store_result(&mut state, interrupted, false)?;
            drop(state);
            self.completed.notify_waiters();
            return Ok(false);
        }
        let delivered = state.live_waiters.get(&request_id).copied().unwrap_or(0) > 0;
        self.store_result(&mut state, result, delivered)?;
        drop(state);
        self.completed.notify_waiters();
        Ok(delivered)
    }

    fn store_result(
        &self,
        state: &mut QueueState,
        result: SubagentToolResult,
        delivered: bool,
    ) -> Result<()> {
        let request_id = result.request_id.clone();
        let mut next = state.clone();
        next.requests.remove(&request_id);
        next.results.insert(request_id.clone(), result);
        next.delivered_to_waiter
            .insert(request_id.clone(), delivered);
        // Results are small but bound retained history so a long-lived parent does not
        // grow this control file forever.
        while next.results.len() > 256 {
            let Some(oldest) = next
                .results
                .values()
                .min_by_key(|result| result.completed_at_ms)
                .map(|result| result.request_id.clone())
            else {
                break;
            };
            next.results.remove(&oldest);
            next.delivered_to_waiter.remove(&oldest);
        }
        self.persist(&next)?;
        *state = next;
        Ok(())
    }

    fn persist(&self, state: &QueueState) -> Result<()> {
        let body = serde_json::to_vec_pretty(state)?;
        mj_core::config::atomic_write(&self.path, &body)
            .with_context(|| format!("write sub-agent queue {}", self.path.display()))
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
    fn finish_and_take_result(&mut self) -> Option<SubagentToolResult> {
        let state_ref = self.state.clone();
        let request_id = self.request_id.clone();
        let mut state = state_ref.lock().expect("sub-agent queue lock poisoned");
        Self::remove_from(&mut self.active, &request_id, &mut state);
        state.results.get(&request_id).cloned()
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
    use mj_core::subagent::SubagentToolAction;

    #[test]
    fn claude_mailbox_hook_resolves_the_worker_socket_and_preserves_profile_settings() {
        let worker_root = tempfile::tempdir().unwrap();
        let staged_home = worker_root.path().join("profile/.claude");
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

        assert!(configure_claude_mailbox_hook(worker_root.path(), &staged_home, true).unwrap());
        let first = std::fs::read(&path).unwrap();
        assert!(configure_claude_mailbox_hook(worker_root.path(), &staged_home, true).unwrap());
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

        let source_profile = tempfile::tempdir().unwrap();
        let source_home = source_profile.path().join(".claude");
        std::fs::create_dir(&source_home).unwrap();
        let source_settings = source_home.join("settings.json");
        std::fs::write(&source_settings, br#"{"user":"source"}"#).unwrap();
        let legacy_root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(source_profile.path(), legacy_root.path().join("profile"))
            .unwrap();
        assert!(!configure_claude_mailbox_hook(legacy_root.path(), &source_home, true).unwrap());
        assert_eq!(
            std::fs::read(source_settings).unwrap(),
            br#"{"user":"source"}"#
        );
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

        configure_codex_mailbox_hook(worker_root.path(), &mut environment, true).unwrap();
        let first = environment[CODEX_CONFIG_ENV].clone();
        configure_codex_mailbox_hook(worker_root.path(), &mut environment, true).unwrap();
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
    fn disabled_mailbox_hooks_are_removed_from_both_staged_harness_profiles() {
        let claude_root = tempfile::tempdir().unwrap();
        let claude_home = claude_root.path().join(".claude");
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
        configure_claude_mailbox_hook(claude_root.path(), &claude_home, false).unwrap();
        let claude: serde_json::Value =
            serde_json::from_slice(&std::fs::read(claude_settings_path).unwrap()).unwrap();
        let claude_hooks = claude["hooks"]["PostToolBatch"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(claude_hooks.len(), 1);
        assert_eq!(claude_hooks[0]["command"], "user-hook");

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
        configure_codex_mailbox_hook(codex_root.path(), &mut environment, false).unwrap();
        let codex: serde_json::Value =
            serde_json::from_str(&environment[CODEX_CONFIG_ENV]).unwrap();
        assert_eq!(
            codex["hooks"]["PostToolUse"].as_array().unwrap(),
            &[user_hook]
        );
        assert!(codex["hooks"]["state"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn claude_delegation_launches_from_a_checkout_after_remote_profile_repair() {
        use mj_core::subagent::SubagentMcpRole;
        use std::os::unix::fs::PermissionsExt;

        for (role, old_root) in [
            (SubagentMcpRole::Parent, ".local/share/hel/profiles/session"),
            (SubagentMcpRole::Parent, ".local/share/hel/workers/session"),
            (SubagentMcpRole::Child, "/previous-target/workers/session"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("worker with spaces");
            let home = temp.path().join("profile");
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
                                 "--harness", "claude", "--role", role.id()]
                    }
                }
            });
            let path = home.join(".claude.json");
            std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
            resolve_claude_mcp_paths(&root, &home, true).unwrap();
            let first = std::fs::read(&path).unwrap();
            resolve_claude_mcp_paths(&root, &home, true).unwrap();
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
    #[test]
    fn a_message_only_session_starts_without_a_staged_registration() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        // No profile file at all, then one an older controller staged.
        resolve_claude_mcp_paths(root.path(), &home, false).unwrap();
        let path = home.join(".claude.json");
        let staged = br#"{"mcpServers":{"other":{"command":"x"}}}"#;
        std::fs::write(&path, staged).unwrap();
        resolve_claude_mcp_paths(root.path(), &home, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), staged, "left untouched");
        // A delegating parent still requires its registration.
        assert!(resolve_claude_mcp_paths(root.path(), &home, true).is_err());
    }

    #[test]
    fn claude_delegation_reports_invalid_registration_without_rewriting_it() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        let path = home.join(".claude.json");
        assert!(resolve_claude_mcp_paths(root.path(), &home, true).is_err());
        assert!(!path.exists());
        for broken in [
            "{",
            r#"{"mcpServers":{}}"#,
            r#"{"mcpServers":{"mj-agents":{"args":[]}}}"#,
            r#"{"mcpServers":{"mj-agents":{"args":["--socket"]}}}"#,
            r#"{"mcpServers":{"mj-agents":{"args":["--socket",42]}}}"#,
        ] {
            std::fs::write(&path, broken).unwrap();
            assert!(resolve_claude_mcp_paths(root.path(), &home, true).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
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
    }

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
            }
        }
        std::fs::remove_file(&staged).unwrap();
        std::fs::create_dir(&staged).unwrap();
        assert!(!configure(source.path(), Some(SubagentMcpRole::Parent)).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(configure(&staged, Some(SubagentMcpRole::Parent)).unwrap());
        assert!(staged.join("config.toml").is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
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
    fn wait_budget_uses_the_harness_default_from_request_creation() {
        let action = SubagentToolAction::WaitAgents;
        assert_eq!(
            wait_budget(&action, None, 1_000_000, 1_010_000),
            Duration::from_secs(mj_core::subagent::DEFAULT_WAIT_SECONDS - 10) + WORKER_WAIT_GRACE
        );
        assert_eq!(
            wait_budget(
                &action,
                Some(mj_core::config::HarnessKind::Codex),
                1_000_000,
                1_010_000,
            ),
            Duration::from_secs(mj_core::subagent::MAX_CODEX_WAIT_SECONDS - 10) + WORKER_WAIT_GRACE
        );
        assert_eq!(
            wait_budget(&action, None, 1_000_000, 5_000_000),
            WORKER_WAIT_GRACE,
            "an expired durable wait only gets the fallback delivery grace"
        );
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
            ("wait", SubagentToolAction::WaitAgents),
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

    #[tokio::test]
    async fn saturated_wait_lane_keeps_send_message_available() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(root.path()).unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        let mut clients = Vec::new();
        for n in 0..LONG_SOCKET_TASKS {
            let (server, mut client) = UnixStream::pair().unwrap();
            let service = endpoint.clone();
            tasks.spawn(async move { serve_one(server, service).await });
            let mut wait = request(&format!("wait-{n}"));
            wait.action = SubagentToolAction::WaitAgents;
            let mut body = serde_json::to_vec(&wait).unwrap();
            body.push(b'\n');
            client.write_all(&body).await.unwrap();
            clients.push(client);
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while endpoint.snapshot().0.len() < LONG_SOCKET_TASKS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let (server, mut client) = UnixStream::pair().unwrap();
        let service = endpoint.clone();
        tasks.spawn(async move { serve_one(server, service).await });
        let mut excess = request("excess-wait");
        excess.action = SubagentToolAction::WaitAgents;
        let mut body = serde_json::to_vec(&excess).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(client).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let refused: SocketReply = serde_json::from_str(&line).unwrap();
        assert!(!refused.accepted);
        assert!(refused.result.unwrap().is_error);
        assert!(
            !endpoint
                .snapshot()
                .0
                .iter()
                .any(|r| r.request_id == "excess-wait")
        );

        let (server, mut client) = UnixStream::pair().unwrap();
        let service = endpoint.clone();
        tasks.spawn(async move { serve_one(server, service).await });
        let mut message = request("message-request");
        message.action = SubagentToolAction::SendMessage {
            child_session_id: "child".into(),
            message: "note".into(),
        };
        let mut body = serde_json::to_vec(&message).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !endpoint
                .snapshot()
                .0
                .iter()
                .any(|r| r.request_id == "message-request")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        endpoint.complete(done("message-request")).unwrap();
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            BufReader::new(client).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let completed: SocketReply = serde_json::from_str(&line).unwrap();
        assert!(completed.accepted);
        assert_eq!(completed.result, Some(done("message-request")));
        assert_eq!(endpoint.snapshot().0.len(), LONG_SOCKET_TASKS);
        tasks.abort_all();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(result) => result.unwrap(),
                Err(error) => assert!(error.is_cancelled()),
            }
        }
        drop(clients);
    }

    // Hard-won: 8c73d3a8f: Claude's hour-long wait prevented its steering hook from running.
    #[tokio::test]
    async fn claude_steering_wakes_the_socket_wait_without_a_daemon_or_lost_reports() {
        use crate::relay::test_support::{prompt, submit_relay};
        use mj_core::config::HarnessKind;
        use mj_core::relay::{RelayResponsePayload, SteeringStatus};

        for steer_before_wait in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut owner = crate::relay::DurableRelay::open_for_harness(
                root.path(),
                "session-123",
                "test",
                HarnessKind::Claude,
            )
            .unwrap();
            owner.set_steering_delivery_mode(HarnessKind::Claude, true);
            owner.set_automatic_steering(true);
            submit_relay(&mut owner, "prompt-first", prompt("work"));
            owner.claim_pending_commands(true).unwrap();
            let relay = Arc::new(Mutex::new(owner));
            let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
            endpoint.relay = Some(relay.clone());
            endpoint.harness = Some(HarnessKind::Claude);
            let steer = || {
                let mut owner = relay.lock().unwrap();
                owner.set_steering_supported(Some(true));
                submit_relay(&mut owner, "correction", prompt("change direction"));
                assert!(owner.claim_pending_commands(true).unwrap().is_empty());
            };
            if steer_before_wait {
                steer();
            }
            let (server, mut client) = UnixStream::pair().unwrap();
            let service = endpoint.clone();
            let task = tokio::spawn(async move { serve_one(server, service).await });
            let mut wait = request("waiting");
            wait.created_at_ms = mj_core::clock::epoch_millis();
            wait.action = SubagentToolAction::WaitAgents;
            let mut body = serde_json::to_vec(&wait).unwrap();
            body.push(b'\n');
            client.write_all(&body).await.unwrap();
            if !steer_before_wait {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while endpoint.snapshot().0.is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(!task.is_finished());
                steer();
            }
            let mut line = String::new();
            tokio::time::timeout(
                Duration::from_secs(2),
                BufReader::new(client).read_line(&mut line),
            )
            .await
            .expect("steering must yield the wait, not wait an hour")
            .unwrap();
            task.await.unwrap().unwrap();
            let reply: SocketReply = serde_json::from_str(&line).unwrap();
            let payload: serde_json::Value =
                serde_json::from_str(&reply.result.unwrap().message).unwrap();
            assert_eq!(payload["status"], "interrupted");
            assert!(endpoint.state.lock().unwrap().live_waiters.is_empty());
            assert!(
                endpoint.snapshot().0.is_empty(),
                "the yielded request must retire"
            );

            // A late daemon report was not part of this answer. Its delivery
            // receipt must leave that report available for the parent's next wait.
            assert!(!endpoint.complete(done("waiting")).unwrap());
            let recovered = SubagentEndpoint::open(root.path()).unwrap();
            assert!(!recovered.complete(done("waiting")).unwrap());
            assert!(recovered.snapshot().0.is_empty());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&recovered.snapshot().1[0].message)
                    .unwrap()["status"],
                "interrupted"
            );
            let mut owner = relay.lock().unwrap();
            assert_eq!(
                owner.operational_state().steering.unwrap().status,
                SteeringStatus::Pending
            );
            let RelayResponsePayload::MailboxDrained {
                lease_id: Some(lease),
                text: Some(text),
                count: 1,
            } = owner.drain_mailbox("PostToolBatch").unwrap()
            else {
                panic!("steer must remain for the hook")
            };
            assert!(text.contains("change direction"));
            let acknowledged = owner.handle(crate::relay::test_support::relay_request(
                "hook-ack",
                mj_core::relay::RelayRequest::AckMailbox { lease_id: lease },
            ));
            assert!(matches!(
                acknowledged.body,
                mj_core::relay::RelayResponseBody::Ok {
                    payload: RelayResponsePayload::MailboxAcknowledged { acknowledged: true }
                }
            ));
            assert_eq!(
                owner.operational_state().steering.unwrap().status,
                SteeringStatus::Applied
            );
            assert!(matches!(
                owner.drain_mailbox("PostToolBatch").unwrap(),
                RelayResponsePayload::MailboxDrained { count: 0, .. }
            ));
        }
    }

    #[tokio::test]
    async fn turn_boundary_preserves_completed_reports_and_child_mutations() {
        use crate::relay::test_support::{prompt, submit_relay};
        use mj_core::config::HarnessKind;

        let root = tempfile::tempdir().unwrap();
        let mut owner = crate::relay::DurableRelay::open_for_harness(
            root.path(),
            "session-123",
            "test",
            HarnessKind::Claude,
        )
        .unwrap();
        owner.set_steering_delivery_mode(HarnessKind::Claude, true);
        owner.set_steering_supported(Some(true));
        owner.set_automatic_steering(true);
        submit_relay(&mut owner, "prompt-first", prompt("work"));
        owner.claim_pending_commands(true).unwrap();
        let relay = Arc::new(Mutex::new(owner));
        let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
        endpoint.relay = Some(relay.clone());
        let mut wait = request("report-first");
        wait.action = SubagentToolAction::WaitAgents;
        endpoint.enqueue(wait).unwrap();
        let mut waiter = endpoint.register_waiter("report-first");
        assert!(endpoint.complete(done("report-first")).unwrap());
        {
            let mut owner = relay.lock().unwrap();
            submit_relay(&mut owner, "correction", prompt("change direction"));
            owner.claim_pending_commands(true).unwrap();
        }
        assert_eq!(
            endpoint
                .await_result_registered(
                    "report-first",
                    Instant::now() + Duration::from_secs(2),
                    &mut waiter
                )
                .await
                .unwrap(),
            Some(done("report-first"))
        );
        for (id, action, origin) in [
            (
                "mutation",
                SubagentToolAction::CloseAgent {
                    child_session_id: "child".into(),
                },
                "prompt-first",
            ),
            ("stale", SubagentToolAction::WaitAgents, "old-turn"),
        ] {
            let mut other = request(id);
            other.action = action;
            endpoint.enqueue(other).unwrap();
            endpoint
                .state
                .lock()
                .unwrap()
                .requests
                .get_mut(id)
                .unwrap()
                .originating_command_id = Some(origin.into());
            let mut waiter = endpoint.register_waiter(id);
            let interrupted = endpoint
                .interrupt_wait_at_turn_boundary(id, &mut waiter)
                .unwrap();
            assert_eq!(interrupted.is_some(), id == "stale");
            assert_eq!(endpoint.complete(done(id)).unwrap(), id != "stale");
        }
    }

    // Hard-won: #1287: a cancelled direct MCP wait consumed a later child report through its live socket.
    #[test]
    fn cancelled_wait_completion_is_serialized_with_its_turn_owner() {
        use crate::relay::test_support::{prompt, submit_relay};
        use mj_core::config::HarnessKind;
        use mj_core::relay::{RelayCommand, RelayCommandOutcome};

        for cancel in [
            RelayCommand::CancelTurnFor {
                active_prompt_id: "original-turn".into(),
            },
            RelayCommand::CancelTurn,
            RelayCommand::Cancel,
        ] {
            for completion_first in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let mut owner = crate::relay::DurableRelay::open_for_harness(
                    root.path(),
                    "session-123",
                    "test",
                    HarnessKind::Codex,
                )
                .unwrap();
                submit_relay(&mut owner, "original-turn", prompt("work"));
                owner.claim_pending_commands(true).unwrap();
                let relay = Arc::new(Mutex::new(owner));
                let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
                endpoint.relay = Some(relay.clone());
                let mut request = request("waiting");
                request.action = SubagentToolAction::WaitAgents;
                endpoint.enqueue(request).unwrap();
                let _waiter = endpoint.register_waiter("waiting");
                if completion_first {
                    assert!(endpoint.complete(done("waiting")).unwrap());
                }
                submit_relay(
                    &mut relay.lock().unwrap(),
                    "cancel-original-turn",
                    cancel.clone(),
                );
                {
                    let mut owner = relay.lock().unwrap();
                    owner.claim_pending_commands(true).unwrap();
                    owner
                        .record_command_completed(
                            "cancel-original-turn",
                            RelayCommandOutcome::Cancelled,
                        )
                        .unwrap();
                }
                // The waiter has not polled the relay change. Completion itself
                // must use the turn owner even after the cancel RPC was acknowledged.
                assert_eq!(
                    endpoint.complete(done("waiting")).unwrap(),
                    completion_first
                );
                let payload = endpoint.cached_result("waiting").unwrap();
                if completion_first {
                    assert_eq!(payload, done("waiting"));
                } else {
                    let payload: serde_json::Value =
                        serde_json::from_str(&payload.message).unwrap();
                    assert_eq!(payload["status"], "interrupted");
                }
                let restored = SubagentEndpoint::open(root.path()).unwrap();
                assert_eq!(
                    restored.complete(done("waiting")).unwrap(),
                    completion_first
                );
            }
        }
    }

    // Hard-won: #1287: cancelled or superseded turns left their MCP waiter registered until its deadline.
    #[tokio::test]
    async fn turn_end_retires_pending_wait_without_collecting_child_reports() {
        use crate::relay::test_support::{prompt, submit_relay};
        use mj_core::config::HarnessKind;
        use mj_core::relay::{RelayCommand, RelayCommandOutcome};

        for replacement_turn in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut owner = crate::relay::DurableRelay::open_for_harness(
                root.path(),
                "session-123",
                "test",
                HarnessKind::Codex,
            )
            .unwrap();
            submit_relay(&mut owner, "original-turn", prompt("work"));
            owner.claim_pending_commands(true).unwrap();
            let relay = Arc::new(Mutex::new(owner));
            let mut endpoint = SubagentEndpoint::open(root.path()).unwrap();
            endpoint.relay = Some(relay.clone());
            let mut request = request("waiting");
            request.action = SubagentToolAction::WaitAgents;
            endpoint.enqueue(request).unwrap();
            let mut waiter = endpoint.register_waiter("waiting");
            let mut pending = Box::pin(endpoint.await_result_registered(
                "waiting",
                Instant::now() + Duration::from_secs(240),
                &mut waiter,
            ));
            assert!(
                std::future::poll_fn(|context| {
                    std::task::Poll::Ready(std::future::Future::poll(pending.as_mut(), context))
                })
                .await
                .is_pending()
            );
            {
                let mut owner = relay.lock().unwrap();
                if replacement_turn {
                    submit_relay(&mut owner, "cancel-original-turn", RelayCommand::CancelTurn);
                    owner.claim_pending_commands(true).unwrap();
                    owner
                        .record_command_completed(
                            "original-turn",
                            RelayCommandOutcome::Prompt {
                                stop_reason: "cancelled".into(),
                                usage: None,
                                diagnostic: None,
                            },
                        )
                        .unwrap();
                    submit_relay(&mut owner, "next-turn", prompt("collect reports"));
                    owner.claim_pending_commands(true).unwrap();
                    assert!(owner.originating_command_id().is_none());
                    owner
                        .record_command_completed(
                            "cancel-original-turn",
                            RelayCommandOutcome::Cancelled,
                        )
                        .unwrap();
                    assert!(owner.subagent_wait_turn_is_live("next-turn"));
                } else {
                    submit_relay(&mut owner, "cancel-original-turn", RelayCommand::CancelTurn);
                }
            }
            let result = tokio::time::timeout(Duration::from_secs(2), pending)
                .await
                .expect("turn changes must wake the wait before its deadline")
                .unwrap()
                .unwrap();
            let payload: serde_json::Value = serde_json::from_str(&result.message).unwrap();
            assert_eq!(payload["status"], "interrupted");
            assert!(!endpoint.complete(done("waiting")).unwrap());
            assert!(endpoint.state.lock().unwrap().live_waiters.is_empty());
            assert!(endpoint.snapshot().0.is_empty());
        }
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

    #[tokio::test]
    async fn completion_records_when_a_waiter_timed_out_and_replays_that_status() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        endpoint.enqueue(request("late-result")).unwrap();

        assert_eq!(
            endpoint.await_result("late-result", Instant::now()).await,
            None,
            "the local fallback wins before the daemon completion"
        );
        assert!(!endpoint.complete(done("late-result")).unwrap());
        assert!(
            !endpoint.complete(done("late-result")).unwrap(),
            "a retried completion must keep the original delivery decision"
        );

        let reopened = SubagentEndpoint::open(directory.path()).unwrap();
        assert!(!reopened.complete(done("late-result")).unwrap());
    }

    // Hard-won: 96b9154: the worker waited past the deadline requested by the model.
    #[tokio::test(start_paused = true)]
    async fn a_wait_the_daemon_never_answers_without_guessing_child_state() {
        #[cfg(test)]
        use tokio::io::AsyncBufReadExt;
        use tokio::io::{AsyncWriteExt, BufReader};

        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
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
        let request = SubagentToolRequest {
            originating_command_id: None,
            request_id: "r-late".into(),
            created_at_ms: mj_core::clock::epoch_millis()
                .saturating_sub(((mj_core::subagent::DEFAULT_WAIT_SECONDS - 45) * 1_000) as i64),
            action: SubagentToolAction::WaitAgents,
        };
        let mut body = serde_json::to_vec(&request).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();
        client.flush().await.unwrap();

        // Nothing ever completes this request. The answer must still arrive,
        // at the caller's deadline plus this worker's small grace.
        let started = Instant::now();
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        served.await.unwrap();

        assert!(
            elapsed >= Duration::from_secs(45) && elapsed <= Duration::from_secs(55),
            "the answer must land at the caller's deadline, took {elapsed:?}"
        );
        let reply: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(reply["accepted"], true);
        assert_eq!(reply["result"]["is_error"], false, "{reply}");
        let payload: serde_json::Value =
            serde_json::from_str(reply["result"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(
            payload["status"],
            mj_core::subagent::WAIT_STATUS_STILL_RUNNING,
            "{payload}"
        );
        assert_eq!(payload["agents"], serde_json::json!([]));
        assert!(
            payload["next_action"]
                .as_str()
                .unwrap()
                .contains("Call wait again"),
            "{payload}"
        );
    }

    /// #1197: the MCP server gives up on anything but `wait` after
    /// `REPLY_TIMEOUT`. This worker answers first, and says whether the daemon
    /// has read the queue since the request joined it; a read before the
    /// request does not count.
    // Hard-won: dfd2e32: a stalled daemon launched a queued child after the parent had moved on.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_request_is_answered_here_with_whether_the_daemon_picked_it_up() {
        use tokio::io::{AsyncWriteExt, BufReader};

        for daemon_reads_it in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
            endpoint.collect_for_daemon();
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
                endpoint.collect_for_daemon();
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
    harness: mj_core::config::HarnessKind,
) -> Result<(SubagentEndpoint, SubagentSocketGuard)> {
    let mut endpoint = SubagentEndpoint::open(root)?;
    // Preserve a closed gate if this worker restarts during Move recovery. A
    // replacement worker explicitly reopens admission after it reconnects.
    endpoint.relay = Some(relay);
    endpoint.harness = Some(harness);
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

/// How long this worker waits for the daemon before answering by itself. A
/// `wait` uses the harness's default window measured from request creation;
/// every other action is answered just before the MCP server stops listening.
fn wait_budget(
    action: &mj_core::subagent::SubagentToolAction,
    harness: Option<mj_core::config::HarnessKind>,
    created_at_ms: i64,
    now_ms: i64,
) -> Duration {
    match action {
        mj_core::subagent::SubagentToolAction::WaitAgents => {
            mj_core::subagent::remaining_subagent_wait(created_at_ms, harness, now_ms)
                + WORKER_WAIT_GRACE
        }
        _ => crate::subagent_mcp::REPLY_TIMEOUT - WORKER_REPLY_MARGIN,
    }
}

/// The children a `wait` is about, or `None` for any other action. Only a
/// `wait` can be answered by this worker alone; the rest have no answer that
/// does not come from the daemon.
/// This worker's own answer to a `wait` the daemon did not finish in time. It
/// has no child list or current status, so it reports that the timed-out check
/// is unknown and tells the parent to call `wait` later.
fn late_daemon_reply(request_id: &str, waited_seconds: u64) -> SubagentToolResult {
    let payload = mj_core::subagent::still_running_payload(
        waited_seconds,
        Some(
            "Mjolnir did not finish checking child reports within this call's timeout. \
             Their state here is unknown rather than observed; call wait again later.",
        ),
    );
    SubagentToolResult {
        request_id: request_id.to_owned(),
        completed_at_ms: mj_core::clock::epoch_millis(),
        is_error: false,
        message: serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
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
    let request: SubagentToolRequest =
        serde_json::from_str(&line).context("parse sub-agent request")?;
    let lane = if matches!(
        request.action,
        mj_core::subagent::SubagentToolAction::WaitAgents
            | mj_core::subagent::SubagentToolAction::Spawn { .. }
    ) {
        &endpoint.admission.long
    } else {
        &endpoint.admission.control
    };
    let Ok(_admission) = lane.clone().try_acquire_owned() else {
        let body = SocketReply { accepted: false, result: Some(SubagentToolResult {
            request_id: request.request_id, completed_at_ms: mj_core::clock::epoch_millis(),
            is_error: true, message: "Sub-agent request was not accepted: this operation's queue is full; retry later.".into(),
        }), daemon: None };
        return write_reply(&mut write, body).await;
    };
    // Input acknowledges durable queue admission, not delivery. Keep it pending
    // for the daemon; wait/list_agents expose its eventual delivery result.
    // Other tools still return their completed result.
    //
    // A `wait` gets the caller harness's deadline here, on this host's monotonic
    // clock. This is the timer the model depends on: it is unaffected by a
    // daemon restart, by a result that could not be handed back, and by clock
    // skew between the two hosts. When it expires this worker answers the call
    // itself rather than leaving the harness in silence.
    let request_id = request.request_id.clone();
    let deadline_budget = wait_budget(
        &request.action,
        endpoint.harness,
        request.created_at_ms,
        mj_core::clock::epoch_millis(),
    );
    let is_wait = matches!(
        request.action,
        mj_core::subagent::SubagentToolAction::WaitAgents
    );
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
                    "next_action": "Input is stored, not yet confirmed delivered. Use wait or list_agents to check delivery; do not resend it."
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
                result: queued,
                daemon: None,
            },
        )
        .await;
    }
    let result = match queued {
        Some(cached) => Some(cached),
        None if queued_input.is_some() => queued_input,
        None => {
            endpoint
                .await_result_registered(
                    &request_id,
                    started + deadline_budget,
                    waiter.as_mut().expect("a waiting request has a waiter"),
                )
                .await?
        }
    };
    let result = result.or_else(|| {
        is_wait.then(|| {
            tracing::warn!(
                request_id = %request_id,
                waited_seconds = started.elapsed().as_secs(),
                "the daemon did not answer a sub-agent wait by its deadline; \
                 answering that the children are still running"
            );
            late_daemon_reply(&request_id, started.elapsed().as_secs())
        })
    });
    // Without a result, say whether the daemon has picked the request up: a
    // daemon that is restarting or stalled otherwise looks the same as a slow
    // action, and the model should act differently on the two.
    let daemon = result.is_none().then(|| endpoint.daemon_contact(mark));
    write_reply(
        &mut write,
        SocketReply {
            accepted: true,
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
