//! Parent-worker queue behind the Mjolnir sub-agent MCP server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use tokio::io::AsyncBufReadExt;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, Semaphore};
use tokio::time::{Instant, sleep_until};

use mj_core::subagent::{SubagentToolRequest, SubagentToolResult};

use crate::subagent_mcp::DaemonContact;

pub const SUBAGENT_SOCKET: &str = "subagents.sock";

/// Finalize the controller's staged registration on the target, where the
/// worker root is absolute. Claude launches MCP servers from the checkout,
/// not from the home against which remote staging paths were written.
pub(super) fn resolve_claude_mcp_paths(root: &Path, home: &Path) -> Result<()> {
    anyhow::ensure!(
        root.is_absolute(),
        "Claude MCP worker root must be absolute"
    );
    let path = home.join(".claude.json");
    let body = std::fs::read(&path)
        .with_context(|| format!("read staged Claude configuration {}", path.display()))?;
    let mut config: serde_json::Value = serde_json::from_slice(&body)
        .with_context(|| format!("parse staged Claude configuration {}", path.display()))?;
    let server = config
        .get_mut("mcpServers")
        .and_then(|servers| servers.get_mut(mj_core::subagent::SUBAGENT_MCP_SERVER))
        .and_then(serde_json::Value::as_object_mut)
        .with_context(|| {
            format!(
                "missing staged Claude delegation server in {}",
                path.display()
            )
        })?;
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

/// Register the owned server in Codex's session-private profile. The ACP bridge
/// cannot carry omit_tools_from. Do this on the worker before launching the
/// harness so an upgraded worker also repairs an older staged profile.
pub(super) fn configure_codex_mcp(
    root: &Path,
    home: &Path,
    role: Option<mj_core::subagent::SubagentMcpRole>,
    policy: mj_core::config::ExecutionPolicy,
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
                     "--harness", "codex", "--role", role.id()],
            // Keep direct and code-mode access; other servers retain their policy.
            "omit_tools_from": ["deferred"]
        });
        if policy == mj_core::config::ExecutionPolicy::ConfiguredApprovals {
            server["default_tools_approval_mode"] = serde_json::json!("approve");
        }
        servers.insert(name.into(), toml::Value::try_from(server)?);
    } else if servers.remove(name).is_none() {
        return Ok(true);
    }
    mj_core::config::atomic_write(&path, toml::to_string(&config)?.as_bytes())
        .with_context(|| format!("write staged Codex configuration {}", path.display()))?;
    Ok(true)
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// The daemon's reads of this queue since this worker started. It lives
    /// under the queue's lock so a request is either in a read or after it.
    #[serde(skip)]
    collections: Collections,
    /// Waiters currently registered in this worker process. Registration and
    /// completion share the queue lock to decide the fallback race once.
    #[serde(skip)]
    live_waiters: BTreeMap<String, usize>,
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
    ) -> Option<SubagentToolResult> {
        loop {
            let notified = self.completed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.cached_result(request_id) {
                waiter.finish();
                return Some(result);
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = sleep_until(deadline) => return waiter.finish_and_take_result(),
            }
        }
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
        self.enqueue_marked(request).map(|(result, _)| result)
    }

    /// Queue `request`. Also return any result already recorded for it, and
    /// the number of daemon reads that happened before it joined the queue.
    fn enqueue_marked(
        &self,
        mut request: SubagentToolRequest,
    ) -> Result<(Option<SubagentToolResult>, u64)> {
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
            return Ok((Some(result.clone()), mark));
        }
        let mut next = state.clone();
        next.requests
            .entry(request.request_id.clone())
            .or_insert(request);
        self.persist(&next)?;
        *state = next;
        Ok((None, mark))
    }

    pub fn complete(&self, result: SubagentToolResult) -> Result<bool> {
        let request_id = result.request_id.clone();
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
        let delivered = state.live_waiters.get(&request_id).copied().unwrap_or(0) > 0;
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
        drop(state);
        self.completed.notify_waiters();
        Ok(delivered)
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

    #[tokio::test]
    async fn claude_delegation_launches_from_a_checkout_after_remote_profile_repair() {
        use mj_core::subagent::SubagentMcpRole;
        use std::os::unix::fs::PermissionsExt;

        for (role, old_root) in [
            (SubagentMcpRole::Parent, ".local/share/hel/profiles/session"),
            (
                SubagentMcpRole::FixedParent,
                ".local/share/hel/workers/session",
            ),
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
            resolve_claude_mcp_paths(&root, &home).unwrap();
            let first = std::fs::read(&path).unwrap();
            resolve_claude_mcp_paths(&root, &home).unwrap();
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

    #[test]
    fn claude_delegation_reports_invalid_registration_without_rewriting_it() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir(&home).unwrap();
        let path = home.join(".claude.json");
        assert!(resolve_claude_mcp_paths(root.path(), &home).is_err());
        assert!(!path.exists());
        for broken in [
            "{",
            r#"{"mcpServers":{}}"#,
            r#"{"mcpServers":{"mj-agents":{"args":[]}}}"#,
            r#"{"mcpServers":{"mj-agents":{"args":["--socket"]}}}"#,
            r#"{"mcpServers":{"mj-agents":{"args":["--socket",42]}}}"#,
        ] {
            std::fs::write(&path, broken).unwrap();
            assert!(resolve_claude_mcp_paths(root.path(), &home).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        }
    }

    #[test]
    fn codex_delegation_profile_preserves_other_servers_and_handles_upgrades_and_disable() {
        use mj_core::config::ExecutionPolicy;
        use mj_core::subagent::SubagentMcpRole;
        for role in [
            SubagentMcpRole::Parent,
            SubagentMcpRole::FixedParent,
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
                        role.id()
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
        for broken in ["[", "mcp_servers = 3"] {
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
    async fn saturated_wait_lane_rejects_before_queueing_and_keeps_interrupt_available() {
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
        let mut interrupt = request("interrupt-request");
        interrupt.action = SubagentToolAction::InterruptAgent {
            child_session_id: "child".into(),
        };
        let mut body = serde_json::to_vec(&interrupt).unwrap();
        body.push(b'\n');
        client.write_all(&body).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !endpoint
                .snapshot()
                .0
                .iter()
                .any(|r| r.request_id == "interrupt-request")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        endpoint.complete(done("interrupt-request")).unwrap();
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
        assert_eq!(completed.result, Some(done("interrupt-request")));
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

    #[tokio::test]
    async fn a_waiting_socket_call_gives_up_at_its_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = SubagentEndpoint::open(directory.path()).unwrap();
        assert_eq!(endpoint.enqueue(request("r1")).unwrap(), None);
        let result = endpoint
            .await_result("r1", Instant::now() + Duration::from_millis(50))
            .await;
        assert_eq!(result, None);
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
    root: &Path,
    relay: Arc<Mutex<crate::relay::DurableRelay>>,
    harness: mj_core::config::HarnessKind,
) -> Result<(SubagentEndpoint, super::unix::SocketGuard)> {
    let mut endpoint = SubagentEndpoint::open(root)?;
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
    tokio::spawn(async move {
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
    Ok((endpoint, super::unix::SocketGuard(path)))
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
    let (queued, mark) = endpoint.enqueue_marked(request)?;
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
                .await
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
