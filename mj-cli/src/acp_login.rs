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
}

/// Sign in `profile` through the pinned harness's ACP methods.
pub(crate) async fn login(
    profile: &HarnessProfile,
    requested_method: Option<&str>,
) -> Result<Outcome> {
    if !profile.auth_scheme().syncs_login() {
        return Ok(Outcome::Unavailable(describe_auth(&profile.auth_scheme())));
    }
    std::fs::create_dir_all(&profile.home).with_context(|| {
        format!(
            "create the harness profile home {} for sign-in",
            profile.home.display()
        )
    })?;
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        HarnessHost::current(),
        &mut environment,
    );
    let mut launch_environment = match mj_core::login_environment::with_overrides(&environment)
        .await
    {
        Ok(environment) => environment,
        Err(error) => {
            // A login must not depend on an interactive shell being startable
            // in this process. The account identity and the profile overrides
            // are enough to run the installer and the harness.
            eprintln!(
                "Could not read the account login shell ({error:#}); using the account environment."
            );
            let mut base = mj_core::login_environment::bootstrap()?;
            base.extend(environment.clone());
            base
        }
    };
    let managed =
        match mj_worker::worker_runtime::managed_harness(profile.kind, &launch_environment).await {
            Ok(managed) => managed,
            Err(error) => {
                return Ok(Outcome::Unavailable(format!(
                    "the pinned {} could not be prepared: {error:#}",
                    profile.kind.display_name()
                )));
            }
        };
    launch_environment.extend(managed.environment.clone());
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
    } else {
        match select(&session.methods, requested_method) {
            Ok(method) => method.clone(),
            Err(error) => {
                session.shutdown().await;
                return Err(error);
            }
        }
    };
    println!("Signing in with {} ({}).", method.name, method.id);
    match method.terminal_command(kind, entrypoint)? {
        Some(terminal) => {
            session.shutdown().await;
            let rendered = render_command(&terminal);
            println!("Running `{rendered}`.");
            let status = Command::new(&terminal.command)
                .args(&terminal.args)
                .env_clear()
                .envs(environment)
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
            let result =
                tokio::time::timeout(AUTHENTICATE_TIMEOUT, session.authenticate(&method.id))
                    .await
                    .context("the ACP sign-in flow timed out")?;
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
}

/// What a bridge says about a `terminal` auth method.
#[derive(Debug, Clone)]
struct TerminalHint {
    command: Option<String>,
    args: Vec<String>,
}

#[derive(Debug, Clone)]
struct TerminalCommand {
    command: PathBuf,
    args: Vec<String>,
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
        _ => return None,
    };
    Some(TerminalCommand {
        command: entrypoint.to_path_buf(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
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
        let mut child = Command::new(command)
            .args(args)
            .env_clear()
            .envs(environment)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "start the {} ACP bridge {}",
                    kind.display_name(),
                    command.display()
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
        let initialized = tokio::time::timeout(INITIALIZE_TIMEOUT, session.initialize()).await;
        match initialized {
            Ok(Ok(())) => Ok(session),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let _ = session.child.start_kill();
                bail!(
                    "the {} ACP bridge did not answer initialize within {} seconds",
                    kind.display_name(),
                    INITIALIZE_TIMEOUT.as_secs()
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
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    bail!("the ACP bridge rejected {method}: {error}");
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if let (Some(server_id), Some(server_method)) = (
                message.get("id"),
                message.get("method").and_then(Value::as_str),
            ) {
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
            // Notifications (no id) are progress or logging; keep reading.
        }
    }

    async fn shutdown(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
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
    let terminal = (value.get("type").and_then(Value::as_str) == Some("terminal")).then(|| {
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
        TerminalHint { command, args }
    });
    Some(AuthMethod {
        id,
        name,
        description,
        terminal,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
    async fn a_terminal_method_runs_the_advertised_command_with_the_profile_environment() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("terminal-ran");
        let login = write_script(
            directory.path(),
            "login.sh",
            &format!("#!/bin/sh\nprintf '%s' \"$HOME\" > {}\n", marker.display()),
        );
        let methods = json!([{
            "id": "muse-login",
            "name": "Log in with Muse",
            "type": "terminal",
            "args": ["login"],
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
            directory.path().display().to_string()
        );
    }

    #[tokio::test]
    async fn an_agent_method_sends_authenticate() {
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
        };
        let command = method
            .terminal_command(HarnessKind::OpenCode, Path::new("/pinned/opencode"))
            .unwrap()
            .unwrap();
        assert_eq!(command.command, Path::new("/pinned/opencode"));
        assert_eq!(command.args, ["auth", "login", "--standalone"]);
    }
}
