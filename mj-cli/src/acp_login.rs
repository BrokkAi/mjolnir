//! Sign in to a profile through the harness's own ACP server.
//!
//! Mjolnir already installs an exact pinned harness for sessions. This module
//! starts that same harness as an ACP agent, reads the authentication methods
//! it advertises, and performs the one the person picks. A `terminal` method
//! names a command the client is expected to run (Muse does this); a method
//! without a command is either handled by ACP `authenticate` (Codex) or by a
//! known terminal fallback for bridges that only describe the command in text
//! (OpenCode and MiMo).
//!
//! The command runs with inherited standard input and output because it is an
//! interactive browser or device-code flow. That is why this belongs to the
//! terminal CLI rather than to the detached worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mj_core::config::{AuthScheme, HarnessHost, HarnessKind, HarnessProfile};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// How long a freshly installed bridge gets to answer `initialize`.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(180);
/// How long an ACP-driven browser/device-code flow may wait for the person.
const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
pub(crate) enum Outcome {
    SignedIn {
        method: String,
    },
    /// ACP login cannot run for this profile or target. The caller may use the
    /// harness CLI's own login command instead.
    Unavailable(String),
    /// This profile authenticates without an interactive harness login.
    NoLogin(String),
}

/// A pinned harness CLI and the environment prepared for it. The managed
/// harness keeps the installation lease alive while the child command runs.
pub(crate) struct PinnedCli {
    pub command: PathBuf,
    pub environment: BTreeMap<String, String>,
    _managed: mj_core::harness_install::ManagedHarness,
}

fn uses_ambient_cli(profile: &HarnessProfile) -> bool {
    let own_nested = profile.kind.nested_home() && profile.kind.is_own_nested_home(&profile.home);
    let own_data = !profile
        .kind
        .data_source(&profile.home)
        .starts_with(&profile.home);
    own_nested || own_data
}

/// Sign in `profile` through the pinned harness's ACP methods.
pub(crate) async fn login(
    profile: &HarnessProfile,
    requested_method: Option<&str>,
) -> Result<Outcome> {
    let scheme = profile.auth_scheme();
    if !scheme.syncs_login() {
        return Ok(Outcome::NoLogin(describe_auth(&scheme)));
    }
    // A person's own installation keeps its config in their own nested home
    // (OpenCode/Muse), or its data outside a conventional profile home
    // (MiMo's stock XDG data). Pointing the pinned harness at either could
    // migrate a database Mjolnir does not own. Use their CLI for that case.
    if uses_ambient_cli(profile) {
        return Ok(Outcome::Unavailable(format!(
            "this profile uses your own {} installation; Mjolnir signs in through that harness's installed CLI",
            profile.kind.display_name()
        )));
    }
    let (managed, launch_environment) = match managed_login(profile).await {
        Ok(prepared) => prepared,
        Err(error) => {
            return Ok(Outcome::Unavailable(format!(
                "the pinned {} could not be prepared: {error:#}",
                profile.kind.display_name()
            )));
        }
    };
    println!(
        "Using the pinned {} at {}.",
        profile.kind.display_name(),
        managed.entrypoint.display()
    );
    sign_in(
        profile.kind,
        &profile.home,
        &managed.command,
        &managed.args,
        &managed.entrypoint,
        &launch_environment,
        requested_method,
    )
    .await
}

/// Resolve the same managed installation used for sessions and return its CLI
/// with the environment that makes its shebangs and child tools use the
/// managed Node version.
pub(crate) async fn pinned_cli(profile: &HarnessProfile) -> Result<PinnedCli> {
    anyhow::ensure!(
        !uses_ambient_cli(profile),
        "this profile uses your own {} installation; Mjolnir signs in through that harness's installed CLI",
        profile.kind.display_name()
    );
    let (managed, environment) = managed_login(profile).await?;
    Ok(PinnedCli {
        command: managed.cli_entrypoint.clone(),
        environment,
        _managed: managed,
    })
}

async fn managed_login(
    profile: &HarnessProfile,
) -> Result<(
    mj_core::harness_install::ManagedHarness,
    BTreeMap<String, String>,
)> {
    let created_home = !profile.home.exists();
    std::fs::create_dir_all(&profile.home).with_context(|| {
        format!(
            "create the harness profile home {} for sign-in",
            profile.home.display()
        )
    })?;
    #[cfg(unix)]
    if created_home {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&profile.home, std::fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!(
                    "restrict permissions on the harness profile home {}",
                    profile.home.display()
                )
            })?;
    }
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        HarnessHost::current(),
        &mut environment,
    );
    // A login runs in the person's terminal, not in an isolated worker. Keep
    // the ambient display, browser and provider-key environment, then layer
    // the account login shell and finally the profile's own overrides. The
    // old CLI login inherited this environment; env_clear here would drop
    // DISPLAY/BROWSER and exported provider keys.
    let mut launch_environment: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    match mj_core::login_environment::with_overrides(&BTreeMap::new()).await {
        Ok(login) => launch_environment.extend(login),
        Err(error) => {
            eprintln!(
                "Could not read the account login shell ({error:#}); using the environment Mjolnir started with."
            );
        }
    }
    launch_environment.extend(environment.clone());
    let managed = mj_core::harness_install::resolve_with_environment(
        profile.kind,
        mj_core::config::ExecutionPolicy::ConfiguredApprovals,
        &launch_environment,
    )
    .await?;
    launch_environment.extend(managed.environment.clone());
    Ok((managed, launch_environment))
}

/// The core of ACP sign-in. Tests drive this directly with a scripted bridge.
async fn sign_in(
    kind: HarnessKind,
    home: &Path,
    command: &Path,
    args: &[String],
    entrypoint: &Path,
    environment: &BTreeMap<String, String>,
    requested_method: Option<&str>,
) -> Result<Outcome> {
    let mut session = match AcpSession::start(kind, home, command, args, environment).await {
        Ok(session) => session,
        Err(error) => {
            return Ok(Outcome::Unavailable(format!(
                "the {} ACP bridge could not start: {error:#}",
                kind.display_name()
            )));
        }
    };
    let method = if session.methods.is_empty() {
        session.shutdown().await;
        return Ok(Outcome::Unavailable(
            "the ACP bridge advertised no login methods".to_owned(),
        ));
    } else if kind == HarnessKind::Codex
        && requested_method.is_none()
        && !has_desktop_session(environment)
        && api_key_present(environment)
    {
        // Headless with an exported key: use the key method rather than
        // waiting for a browser that cannot appear.
        session
            .methods
            .iter()
            .find(|method| method.id == "api-key")
            .cloned()
            .or_else(|| select(&session.methods, requested_method).ok().cloned())
            .context("the ACP bridge advertised no usable login method")?
    } else {
        match select(&session.methods, requested_method) {
            Ok(method) => method.clone(),
            Err(error) => {
                session.shutdown().await;
                return Err(error);
            }
        }
    };
    if !has_desktop_session(environment)
        && method.id.eq_ignore_ascii_case("chat-gpt")
        && !api_key_present(environment)
    {
        session.shutdown().await;
        bail!(
            "the Codex ChatGPT sign-in needs a desktop browser; set CODEX_API_KEY or OPENAI_API_KEY, or run `mj login --cli` on a desktop"
        );
    }
    println!("Signing in with {} ({}).", method.name, method.id);
    let terminal = match method.terminal_command(kind, entrypoint) {
        Ok(terminal) => terminal,
        Err(error) => {
            session.shutdown().await;
            return Err(error);
        }
    };
    match terminal {
        Some(terminal) => {
            session.shutdown().await;
            restore_default_sigint();
            let rendered = render_command(&terminal);
            println!("Running `{rendered}`.");
            let status = Command::new(&terminal.command)
                .args(&terminal.args)
                .env_clear()
                .envs(environment)
                .envs(&terminal.env)
                .current_dir(home)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .await
                .with_context(|| {
                    format!("run `{rendered}` for the {} login", kind.display_name())
                })?;
            if !status.success() {
                bail!(
                    "`{rendered}` exited with {status}; the {} login did not complete",
                    kind.display_name()
                );
            }
        }
        None => {
            if !method.env_vars.is_empty() {
                let missing = method
                    .env_vars
                    .iter()
                    .filter(|var| {
                        !var.optional
                            && !environment
                                .get(&var.name)
                                .is_some_and(|value| !value.trim().is_empty())
                    })
                    .map(|var| var.name.clone())
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    session.shutdown().await;
                    bail!(
                        "the {} sign-in method {} needs {} in the environment before it can run",
                        kind.display_name(),
                        method.id,
                        missing.join(", ")
                    );
                }
            }
            let authenticated = tokio::select! {
                result = tokio::time::timeout(
                    AUTHENTICATE_TIMEOUT,
                    session.authenticate(&method.id),
                ) => Some(result),
                _ = tokio::signal::ctrl_c() => None,
            };
            let Some(result) = authenticated else {
                session.shutdown().await;
                bail!("the ACP sign-in flow was cancelled");
            };
            let result = match result {
                Ok(result) => result,
                Err(_) => {
                    session.shutdown().await;
                    bail!("the ACP sign-in flow timed out");
                }
            };
            if let Err(error) = result {
                session.shutdown().await;
                return Err(error.context(format!(
                    "complete the {} ACP sign-in method {}",
                    kind.display_name(),
                    method.id
                )));
            }
            session.shutdown().await;
        }
    }
    Ok(Outcome::SignedIn { method: method.id })
}

fn describe_auth(scheme: &AuthScheme) -> String {
    match scheme {
        AuthScheme::ApiKey { env_key } => {
            format!("this profile authenticates with {env_key}, so it has no interactive login")
        }
        AuthScheme::InlineApiKey => {
            "this profile inlines its provider API key, so it has no interactive login".to_owned()
        }
        AuthScheme::AwsCredentialChain => {
            "this profile authenticates with the AWS credential chain, so it has no interactive login"
                .to_owned()
        }
        AuthScheme::NoAuthentication => {
            "this profile's built-in provider does not require an interactive login".to_owned()
        }
        AuthScheme::NativeLogin | AuthScheme::NativeLoginStore => {
            "this profile has no ACP login method".to_owned()
        }
    }
}

#[derive(Debug, Clone)]
struct AuthMethod {
    id: String,
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
    terminal: Option<TerminalHint>,
    /// Variables an `env_var` method needs the agent to receive.
    env_vars: Vec<AuthEnvVar>,
}

#[derive(Debug, Clone)]
struct AuthEnvVar {
    name: String,
    optional: bool,
}

/// What a bridge says about a `terminal` auth method.
#[derive(Debug, Clone)]
struct TerminalHint {
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct TerminalCommand {
    command: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

impl AuthMethod {
    /// The command that performs this method interactively, or `None` when the
    /// method is completed by ACP `authenticate`.
    fn terminal_command(
        &self,
        kind: HarnessKind,
        entrypoint: &Path,
    ) -> Result<Option<TerminalCommand>> {
        if let Some(hint) = &self.terminal {
            if let Some(command) = &hint.command {
                return Ok(Some(TerminalCommand {
                    command: PathBuf::from(command),
                    args: hint.args.clone(),
                    env: hint.env.clone(),
                }));
            }
            if let Some(fallback) = known_terminal_fallback(kind, &self.id, entrypoint) {
                return Ok(Some(fallback));
            }
            if hint.args.is_empty() {
                bail!(
                    "the {} ACP bridge advertised a terminal login method {} without a command",
                    kind.display_name(),
                    self.id
                );
            }
            return Ok(Some(TerminalCommand {
                command: entrypoint.to_path_buf(),
                args: hint.args.clone(),
                env: hint.env.clone(),
            }));
        }
        Ok(known_terminal_fallback(kind, &self.id, entrypoint))
    }
}

/// Bridges whose terminal action is named only in the method description.
///
/// OpenCode 2.1.2 advertises `opencode-login` with the description "Run
/// `opencode auth login` in the terminal" and no `terminal-auth` metadata.
/// MiMo is OpenCode-derived and advertises the same method.
fn known_terminal_fallback(
    kind: HarnessKind,
    method: &str,
    entrypoint: &Path,
) -> Option<TerminalCommand> {
    let args: &[&str] = match (kind, method) {
        (HarnessKind::OpenCode, "opencode-login") => &["auth", "login", "--standalone"],
        (HarnessKind::Mimo, "opencode-login") => &["auth", "login"],
        (HarnessKind::Muse, "muse-login") => &["login"],
        // Grok's ACP bridge names an agent method, but its usable sign-in is
        // the CLI's own login. Running it is safer than leaving `authenticate`
        // to open a browser the target cannot show.
        (HarnessKind::Grok, "grok.com") => &["login"],
        _ => return None,
    };
    Some(TerminalCommand {
        command: entrypoint.to_path_buf(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: BTreeMap::new(),
    })
}

fn select<'a>(methods: &'a [AuthMethod], requested: Option<&str>) -> Result<&'a AuthMethod> {
    if let Some(id) = requested {
        return methods
            .iter()
            .find(|method| method.id == id)
            .with_context(|| {
                format!(
                    "the ACP bridge has no login method {id:?}; available: {}",
                    available(methods)
                )
            });
    }
    if let Some(method) = methods.iter().find(|method| method.terminal.is_some()) {
        return Ok(method);
    }
    if let Some(method) = methods.iter().find(|method| looks_interactive(&method.id)) {
        return Ok(method);
    }
    methods.first().with_context(|| {
        "the ACP bridge advertised no login methods; this harness has no ACP sign-in".to_owned()
    })
}

fn available(methods: &[AuthMethod]) -> String {
    if methods.is_empty() {
        return "none".to_owned();
    }
    methods
        .iter()
        .map(|method| method.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn looks_interactive(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    ["login", "chat-gpt", "chatgpt", "oauth", "browser"]
        .iter()
        .any(|needle| id.contains(needle))
}

/// Whether this login has a desktop session for a browser callback or opener.
fn has_desktop_session(environment: &BTreeMap<String, String>) -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
        || ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|name| {
            environment
                .get(*name)
                .is_some_and(|value| !value.trim().is_empty())
        })
}

fn api_key_present(environment: &BTreeMap<String, String>) -> bool {
    ["CODEX_API_KEY", "OPENAI_API_KEY"].iter().any(|name| {
        environment
            .get(*name)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

/// `tokio::signal::ctrl_c` installs a process-wide SIGINT handler that
/// outlives the future. A terminal login command and the CLI fallback expect
/// Ctrl+C to terminate this process, so restore the default before handing
/// over to them.
pub(crate) fn restore_default_sigint() {
    #[cfg(unix)]
    // SAFETY: setting SIGINT back to its default disposition is process-wide
    // and does not touch any memory owned by another thread.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

fn render_command(command: &TerminalCommand) -> String {
    std::iter::once(command.command.to_string_lossy().into_owned())
        .chain(command.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

struct AcpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    methods: Vec<AuthMethod>,
}

impl AcpSession {
    async fn start(
        kind: HarnessKind,
        cwd: &Path,
        command: &Path,
        args: &[String],
        environment: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let program = command;
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(environment)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        // A shell bootstrap or npx can leave a native server in the bridge's
        // process group. Own that group so shutdown reaches it.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().with_context(|| {
            format!(
                "start the {} ACP bridge {}",
                kind.display_name(),
                program.display()
            )
        })?;
        let stdin = child.stdin.take().context("ACP bridge has no stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("ACP bridge has no stdout")?);
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            methods: Vec::new(),
        };
        let initialized = tokio::select! {
            result = tokio::time::timeout(INITIALIZE_TIMEOUT, session.initialize()) => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        };
        match initialized {
            Some(Ok(Ok(()))) => Ok(session),
            Some(Ok(Err(error))) => {
                session.shutdown().await;
                Err(error)
            }
            Some(Err(_)) => {
                session.shutdown().await;
                bail!(
                    "the {} ACP bridge did not answer initialize within {} seconds",
                    kind.display_name(),
                    INITIALIZE_TIMEOUT.as_secs()
                );
            }
            None => {
                session.shutdown().await;
                bail!(
                    "the {} ACP bridge initialization was cancelled",
                    kind.display_name()
                );
            }
        }
    }

    async fn initialize(&mut self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {
                        "name": "mj",
                        "title": "Mjolnir",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
            .context("initialize the ACP bridge")?;
        self.methods = parse_methods(&result);
        Ok(())
    }

    async fn authenticate(&mut self, method_id: &str) -> Result<()> {
        self.request("authenticate", json!({ "methodId": method_id }))
            .await
            .map(|_| ())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_vec(&request)?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .context("send an ACP request")?;
        self.stdin.flush().await.context("flush an ACP request")?;
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .await
                .context("read an ACP response")?;
            if read == 0 {
                bail!("the ACP bridge exited before answering {method}");
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                // ACP reserves standard output for protocol frames. A malformed
                // line is therefore the bridge's bug; report it instead of
                // guessing which frame it meant.
                bail!(
                    "the ACP bridge wrote a non-protocol line to standard output: {}",
                    line.trim()
                );
            };
            if let Some(server_method) = message.get("method").and_then(Value::as_str) {
                // A request from the agent has its own id space. Check this
                // before matching our response id: both sides commonly start
                // at 1, and treating the request as the response would report
                // a successful login that never happened.
                if let Some(server_id) = message.get("id") {
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": server_id,
                        "error": {
                            "code": -32601,
                            "message": format!(
                                "Mjolnir sign-in cannot provide the client method {server_method}"
                            ),
                        },
                    });
                    let mut bytes = serde_json::to_vec(&response)?;
                    bytes.push(b'\n');
                    self.stdin.write_all(&bytes).await?;
                    self.stdin.flush().await?;
                    bail!("the ACP bridge asked for client method {server_method} during sign-in");
                }
                // A notification is progress or logging; keep reading.
                continue;
            }
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    bail!("the ACP bridge rejected {method}: {error}");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    async fn shutdown(&mut self) {
        // Closing the protocol stream gives the bridge a chance to finish a
        // credential write before the group is signaled.
        let _ = self.stdin.shutdown().await;
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            mj_core::subprocess::terminate_process_group(pid as i32, libc::SIGTERM);
        }
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = self.child.id() {
                    mj_core::subprocess::terminate_process_group(pid as i32, libc::SIGKILL);
                }
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
            }
        }
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        // Every early return above must still take the bridge's process group
        // with it. `try_wait` distinguishes a live child from one already
        // reaped, so a recycled pid is never signaled.
        #[cfg(unix)]
        if self.child.try_wait().ok().flatten().is_none()
            && let Some(pid) = self.child.id()
        {
            mj_core::subprocess::terminate_process_group(pid as i32, libc::SIGKILL);
        }
    }
}

fn parse_methods(result: &Value) -> Vec<AuthMethod> {
    result
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(parse_method).collect())
        .unwrap_or_default()
}

fn parse_method(value: &Value) -> Option<AuthMethod> {
    let id = value.get("id")?.as_str()?.to_owned();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let kind = value.get("type").and_then(Value::as_str);
    let terminal = (kind == Some("terminal")).then(|| {
        let meta = value.pointer("/_meta/terminal-auth");
        let command = meta
            .and_then(|meta| meta.get("command"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let args = meta
            .and_then(|meta| meta.get("args"))
            .or_else(|| value.get("args"))
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let env = value
            .get("env")
            .and_then(Value::as_object)
            .map(|env| {
                env.iter()
                    .filter_map(|(name, value)| {
                        value.as_str().map(|value| (name.clone(), value.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        TerminalHint { command, args, env }
    });
    let env_vars = if kind == Some("env_var") {
        value
            .get("vars")
            .and_then(Value::as_array)
            .map(|vars| {
                vars.iter()
                    .filter_map(|var| {
                        Some(AuthEnvVar {
                            name: var.get("name").and_then(Value::as_str)?.to_owned(),
                            optional: var
                                .get("optional")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some(AuthMethod {
        id,
        name,
        description,
        terminal,
        env_vars,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Writing and executing a fixture script can race another test's fork
    /// with `ETXTBSY`. These tests spawn real processes, so keep them serial.
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn write_script(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn home(directory: &Path) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("HOME".into(), directory.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                directory.join(".config").display().to_string(),
            ),
            (
                "XDG_DATA_HOME".into(),
                directory.join(".local/share").display().to_string(),
            ),
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ("DISPLAY".into(), ":0".into()),
        ])
    }

    #[tokio::test]
    async fn pinned_cli_comes_from_the_same_managed_install_as_the_bridge() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let tools = root.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        for name in ["node", "npm", "npx"] {
            write_script(
                &tools,
                name,
                if name == "node" {
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo v24.22.1; fi\n"
                } else {
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 11.6.2; fi\n"
                },
            );
        }
        let xdg_cache = root.join("cache");
        let cache_root = mj_core::harness_runtime::managed_harness_cache_root(
            Some(xdg_cache.as_os_str()),
            Some(root.as_os_str()),
        )
        .unwrap();
        let install =
            mj_core::harness_runtime::managed_harness_install_dir(&cache_root, HarnessKind::Claude);
        std::fs::create_dir_all(install.join("node_modules/.bin")).unwrap();
        let bridge = write_script(
            &install.join("node_modules/.bin"),
            "claude-agent-acp",
            "#!/bin/sh\nexit 0\n",
        );
        let cli = write_script(
            &install.join("node_modules/.bin"),
            "claude",
            "#!/bin/sh\necho pinned-claude\n",
        );
        let manifest =
            mj_core::harness_runtime::ManagedHarnessManifest::for_harness(HarnessKind::Claude);
        mj_core::config::atomic_write(
            &install.join(mj_core::harness_runtime::MANIFEST_FILE),
            &serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let mut environment = home(root);
        environment.insert("PATH".into(), format!("{}:/usr/bin:/bin", tools.display()));
        environment.insert("XDG_CACHE_HOME".into(), xdg_cache.display().to_string());
        let profile = HarnessProfile {
            enabled: true,
            kind: HarnessKind::Claude,
            home: root.join("claude-profile"),
            environment: environment.into(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            legacy_subagents: Default::default(),
        };
        std::fs::create_dir_all(&profile.home).unwrap();

        let pinned = pinned_cli(&profile).await.unwrap();

        assert_eq!(pinned.command, cli);
        assert!(
            pinned.environment["PATH"]
                .split(':')
                .any(|entry| Path::new(entry) == tools)
        );
        assert!(bridge.exists());
    }

    /// A bridge that answers initialize with `methods` and records every later
    /// request in `requests`.
    fn bridge(directory: &Path, methods: &str) -> PathBuf {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": 1,
                "authMethods": serde_json::from_str::<Value>(methods).unwrap(),
            },
        });
        write_script(
            directory,
            "bridge.sh",
            &format!(
                "#!/bin/sh\n\
                 read -r line\n\
                 printf '%s\\n' '{response}'\n\
                 while read -r line; do\n\
                   printf '%s\\n' \"$line\" >> {requests}\n\
                   case \"$line\" in\n\
                     *'\"id\":2'*) printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{}}}}' ;;\n\
                   esac\n\
                 done\n",
                response = response,
                requests = directory.join("requests").display(),
            ),
        )
    }

    #[tokio::test]
    async fn an_api_key_profile_reports_no_interactive_login() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let mut profile = HarnessProfile {
            enabled: true,
            kind: HarnessKind::Dsh,
            home: directory.path().to_path_buf(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            legacy_subagents: Default::default(),
        };
        profile
            .environment
            .insert("DEEPSEEK_API_KEY".to_owned(), "secret".to_owned());
        let outcome = login(&profile, None).await.unwrap();
        assert!(matches!(outcome, Outcome::NoLogin(reason) if reason.contains("DEEPSEEK_API_KEY")));
    }

    #[tokio::test]
    async fn a_terminal_method_runs_the_advertised_command_with_the_profile_environment() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("terminal-ran");
        let login = write_script(
            directory.path(),
            "login.sh",
            &format!(
                "#!/bin/sh\nprintf '%s' \"$HOME:$MJ_LOGIN_ENV\" > {}\n",
                marker.display()
            ),
        );
        let methods = json!([{
            "id": "muse-login",
            "name": "Log in with Muse",
            "type": "terminal",
            "args": ["login"],
            "env": { "MJ_LOGIN_ENV": "from-bridge" },
            "_meta": { "terminal-auth": {
                "command": login.display().to_string(),
                "args": []
            }}
        }]);
        let bridge = bridge(directory.path(), &methods.to_string());
        let outcome = sign_in(
            HarnessKind::Muse,
            directory.path(),
            &bridge,
            &[],
            &bridge,
            &home(directory.path()),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::SignedIn { method } if method == "muse-login"));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            format!("{}:from-bridge", directory.path().display())
        );
    }

    #[tokio::test]
    async fn an_agent_method_sends_authenticate() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let methods = json!([{
            "id": "chat-gpt",
            "name": "ChatGPT",
        }]);
        let bridge = bridge(directory.path(), &methods.to_string());
        let outcome = sign_in(
            HarnessKind::Codex,
            directory.path(),
            &bridge,
            &[],
            &bridge,
            &home(directory.path()),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::SignedIn { method } if method == "chat-gpt"));
        let requests = std::fs::read_to_string(directory.path().join("requests")).unwrap();
        assert!(
            requests.contains("\"method\":\"authenticate\""),
            "{requests}"
        );
        assert!(requests.contains("chat-gpt"), "{requests}");
    }

    #[tokio::test]
    async fn an_unknown_requested_method_lists_the_available_ids() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let methods = json!([
            { "id": "first", "name": "First" },
            { "id": "second", "name": "Second" }
        ]);
        let bridge = bridge(directory.path(), &methods.to_string());
        let error = sign_in(
            HarnessKind::Codex,
            directory.path(),
            &bridge,
            &[],
            &bridge,
            &home(directory.path()),
            Some("missing"),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("missing"), "{message}");
        assert!(message.contains("first, second"), "{message}");
    }

    #[tokio::test]
    async fn a_bridge_without_methods_is_unavailable() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let bridge = bridge(directory.path(), "[]");
        let outcome = sign_in(
            HarnessKind::Codex,
            directory.path(),
            &bridge,
            &[],
            &bridge,
            &home(directory.path()),
            None,
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, Outcome::Unavailable(reason) if reason.contains("no login methods"))
        );
    }

    #[tokio::test]
    async fn a_server_request_sharing_the_response_id_is_not_mistaken_for_a_login() {
        let _guard = TEST_LOCK.lock().await;
        let directory = tempfile::tempdir().unwrap();
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": 1,
                "authMethods": [{ "id": "chat-gpt", "name": "ChatGPT" }],
            },
        });
        let bridge = write_script(
            directory.path(),
            "bridge.sh",
            &format!(
                "#!/bin/sh\n\
                 read -r line\n\
                 printf '%s\\n' '{response}'\n\
                 read -r line\n\
                 printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"elicitation/create\",\"params\":{{}}}}'\n\
                 sleep 5\n",
                response = response
            ),
        );
        let error = sign_in(
            HarnessKind::Codex,
            directory.path(),
            &bridge,
            &[],
            &bridge,
            &home(directory.path()),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("elicitation/create"),
            "{error:#}"
        );
    }

    /// An empty list is what makes `login` fall back to the harness CLI; this
    /// pins the wording callers rely on.
    #[test]
    fn empty_method_selection_names_the_reason() {
        let error = select(&[], None).unwrap_err();
        assert!(
            format!("{error:#}").contains("no login methods"),
            "{error:#}"
        );
    }

    #[test]
    fn opencode_terminal_fallback_uses_the_pinned_entrypoint() {
        let method = AuthMethod {
            id: "opencode-login".into(),
            name: "Login with opencode".into(),
            description: Some("Run `opencode auth login` in the terminal".into()),
            terminal: None,
            env_vars: Vec::new(),
        };
        let command = method
            .terminal_command(HarnessKind::OpenCode, Path::new("/pinned/opencode"))
            .unwrap()
            .unwrap();
        assert_eq!(command.command, Path::new("/pinned/opencode"));
        assert_eq!(command.args, ["auth", "login", "--standalone"]);
    }
}
