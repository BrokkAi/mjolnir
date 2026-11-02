//! Sign in to a profile through the harness's own ACP server, inside the
//! target that holds the harness.
//!
//! The controller never runs a harness itself. `mj login` starts this worker
//! in a short-lived container (the harness sandbox) with the profile home
//! mounted, and the worker selects the harness exactly as a container session
//! does: the image's bridge when it is compatible, otherwise the pinned
//! installation inside the container. It then starts that harness as an ACP
//! agent, reads the authentication methods it advertises, and performs the
//! one the person picks. A `terminal` method names a command the client is
//! expected to run (Muse does this); a method without a command is either
//! handled by ACP `authenticate` (Codex) or by a known terminal fallback for
//! bridges that only describe the command in text (OpenCode and MiMo).
//!
//! The command runs with inherited standard input and output because it is an
//! interactive browser or device-code flow; the container has the person's
//! terminal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mj_core::config::{ExecutionPolicy, HarnessKind};
use mj_core::worker_launch::{HarnessRuntimePolicy, SignInAction, SignInSpec};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::AcpSupervisorSpec;

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
}

/// The harness this target provides, ready to run: the image's when it is
/// compatible, otherwise the pinned installation inside the target, exactly
/// as a container session selects it.
struct TargetHarness {
    prepared: super::PreparedHarnessLaunch,
    /// The ACP adapter or native harness a terminal sign-in method names.
    entrypoint: PathBuf,
    /// The harness's own command-line program.
    cli: PathBuf,
}

async fn prepare_target_harness(
    harness: HarnessKind,
    runtime: HarnessRuntimePolicy,
    cwd: &Path,
    environment: BTreeMap<String, String>,
) -> Result<TargetHarness> {
    let policy = ExecutionPolicy::ConfiguredApprovals;
    let (command, args) = mj_core::harness_runtime::fallback_launch(harness, policy);
    let prepared = super::prepare_harness_launch(
        harness,
        runtime,
        policy,
        AcpSupervisorSpec {
            command: command.into(),
            args,
            environment,
            excluded_environment: Vec::new(),
            cwd: cwd.to_path_buf(),
            harness_lease: None,
            node_lease: None,
        },
    )
    .await
    .with_context(|| format!("prepare {}", harness.display_name()))?;
    let (entrypoint, cli) = match &prepared.managed {
        Some(managed) => (managed.entrypoint.clone(), managed.cli_entrypoint.clone()),
        None => (
            PathBuf::from(target_entrypoint(harness)),
            PathBuf::from(harness.native_login_command().0),
        ),
    };
    Ok(TargetHarness {
        prepared,
        entrypoint,
        cli,
    })
}

/// Run the target's harness CLI with `arguments` and this process's standard
/// streams: a quota probe or a login renewal the controller asked for. The
/// installation stays leased until the CLI exits.
pub async fn run_harness_cli(
    harness: HarnessKind,
    runtime: HarnessRuntimePolicy,
    environment: BTreeMap<String, String>,
    drop_environment: Vec<String>,
    arguments: Vec<String>,
) -> Result<i32> {
    let cwd = std::env::current_dir().context("read the working directory")?;
    let target = prepare_target_harness(harness, runtime, &cwd, environment).await?;
    let mut environment = target.prepared.environment.clone();
    for name in &drop_environment {
        environment.remove(name);
    }
    let status = Command::new(&target.cli)
        .args(&arguments)
        .env_clear()
        .envs(&environment)
        .current_dir(&cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("run `{}`", target.cli.display()))?;
    Ok(status.code().unwrap_or(1))
}

/// Run `spec` in this target and return the exit code the person's terminal
/// should see.
pub async fn run_sign_in(spec: SignInSpec) -> Result<i32> {
    // The caller names the profile home in the environment the way its host
    // reads it (on macOS a Claude login stays in the Keychain).
    let target = prepare_target_harness(
        spec.harness,
        spec.runtime,
        &spec.profile_home,
        spec.environment.clone(),
    )
    .await?;
    let environment = target.prepared.environment.clone();
    let (entrypoint, cli, prepared) = (&target.entrypoint, &target.cli, &target.prepared);
    match spec.action {
        SignInAction::ClaudeSetupToken => {
            println_status("Running `claude setup-token`.");
            run_terminal(
                cli,
                &["setup-token".to_owned()],
                &environment,
                &spec.profile_home,
            )
            .await
        }
        SignInAction::ClaudeAuthStatus => {
            run_terminal(
                cli,
                &["auth".to_owned(), "status".to_owned()],
                &environment,
                &spec.profile_home,
            )
            .await
        }
        SignInAction::Login => {
            if !spec.cli {
                match sign_in(
                    spec.harness,
                    &spec.profile_home,
                    &prepared.spec.command,
                    &prepared.spec.args,
                    entrypoint,
                    &environment,
                    spec.desktop,
                    spec.method.as_deref(),
                )
                .await?
                {
                    Outcome::SignedIn { method } => {
                        println_status(&format!("Signed in through the ACP method {method}."));
                        return Ok(0);
                    }
                    Outcome::Unavailable(reason) => {
                        if spec.method.is_some() {
                            bail!("ACP sign-in was requested but is unavailable: {reason}");
                        }
                        println_status(&format!(
                            "ACP sign-in is unavailable for {} ({reason}); using its command-line login.",
                            spec.harness.display_name()
                        ));
                    }
                }
            }
            let (_, arguments) = spec.harness.native_login_command();
            println_status(&format!(
                "Running `{} {}`.",
                cli.display(),
                arguments.join(" ")
            ));
            run_terminal(cli, &arguments, &environment, &spec.profile_home).await
        }
    }
}

/// Status lines go to standard error: `claude setup-token` prints the token
/// on standard output, and the controller reads it from there.
fn println_status(message: &str) {
    eprintln!("{message}");
}

/// The program a target-provided harness's terminal sign-in runs: the native
/// harness, or the ACP adapter whose own subcommand signs in.
fn target_entrypoint(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Codex => "codex-acp",
        HarnessKind::Claude => "claude-agent-acp",
        HarnessKind::Kimi => "kimi",
        HarnessKind::Grok => "grok",
        HarnessKind::Muse => "muse-acp",
        HarnessKind::OpenCode => "opencode",
        HarnessKind::Mimo => "mimo",
        HarnessKind::Dsh => "dsh-acp",
    }
}

async fn run_terminal(
    program: &Path,
    arguments: &[String],
    environment: &BTreeMap<String, String>,
    home: &Path,
) -> Result<i32> {
    restore_default_sigint();
    let status = Command::new(program)
        .args(arguments)
        .env_clear()
        .envs(environment)
        .current_dir(home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("run `{}`", program.display()))?;
    Ok(status.code().unwrap_or(1))
}

/// The core of ACP sign-in. Tests drive this directly with a scripted bridge.
// Every argument is an independent input: the harness, where it runs, how to
// start its bridge, and what the person chose.
#[allow(clippy::too_many_arguments)]
async fn sign_in(
    kind: HarnessKind,
    home: &Path,
    command: &Path,
    args: &[String],
    entrypoint: &Path,
    environment: &BTreeMap<String, String>,
    desktop: bool,
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
        && !desktop
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
    if !desktop && method.id.eq_ignore_ascii_case("chat-gpt") && !api_key_present(environment) {
        session.shutdown().await;
        bail!(
            "the Codex ChatGPT sign-in needs a desktop browser; set CODEX_API_KEY or OPENAI_API_KEY, or run `mj login` on a desktop"
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
        ])
    }

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
            true,
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
            true,
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
            true,
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
            true,
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
            true,
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
