//! Harness login driver shared by the local CLI and daemon terminal sessions.
use anyhow::{Context, Result, bail};
use mj_core::config::HarnessProfile;
use std::collections::BTreeMap;
use std::io;
#[cfg(unix)]
mod acp_login;

/// Options for the harness login driver.
#[derive(Default)]
pub struct LoginOptions {
    pub setup_token: bool,
    pub cli: bool,
    pub method: Option<String>,
}

/// Run login with the caller's terminal standard streams.
/// Call in a terminal-owning process, never directly on the daemon event loop.
pub async fn login(
    profile_id: &str,
    profile: &HarnessProfile,
    options: LoginOptions,
) -> Result<i32> {
    if options.setup_token {
        return store_claude_setup_token(profile_id, profile)
            .await
            .map(|()| 0);
    }
    // OpenCode keeps its logins in its database, so its fingerprint is the
    // logins read from there, and a live session reloads to take them up.
    let stored = profile.auth_scheme() == mj_core::config::AuthScheme::NativeLoginStore;
    let before = profile_credential_fingerprint(profile)?;

    #[cfg(unix)]
    let mut pinned_fallback = None;
    #[cfg(unix)]
    let mut device_auth = false;
    #[cfg(unix)]
    if !options.cli {
        match acp_login::login(profile, options.method.as_deref()).await {
            Ok(acp_login::Outcome::SignedIn { method }) => {
                println!("Signed in to profile {profile_id} through the ACP method {method}.");
                report_login_change(profile_id, profile, before, stored)?;
                return Ok(0);
            }
            Ok(acp_login::Outcome::DeviceAuth) => {
                println!("Using device-code sign-in because this machine has no desktop browser.");
                pinned_fallback = Some(acp_login::pinned_cli(profile).await?);
                device_auth = true;
            }
            Ok(acp_login::Outcome::NoLogin(reason)) => bail!("{reason}"),
            Ok(acp_login::Outcome::Unavailable(reason)) => {
                anyhow::ensure!(
                    !mj_core::subprocess::preparation_cancellation().is_cancelled(),
                    "login cancelled"
                );
                if options.method.is_some() {
                    bail!("ACP sign-in was requested but is unavailable: {reason}");
                }
                eprintln!(
                    "ACP sign-in is unavailable for {} ({reason}); using its command-line login.",
                    profile.kind.display_name()
                );
                match acp_login::pinned_cli(profile).await {
                    Ok(pinned) => pinned_fallback = Some(pinned),
                    Err(error) => {
                        eprintln!(
                            "The pinned {} CLI is also unavailable ({error:#}); using the command on PATH.",
                            profile.kind.display_name()
                        );
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    #[cfg(not(unix))]
    let _ = options.cli;
    #[cfg(not(unix))]
    if options.method.is_some() {
        bail!("--method requires a Unix host");
    }
    #[cfg(unix)]
    acp_login::restore_default_sigint();

    let (configured_program, arguments) = mj_core::credentials::login_command(profile)
        .with_context(|| format!("profile {profile_id:?}"))?;
    #[cfg(unix)]
    let arguments = if device_auth {
        vec!["login".to_owned(), "--device-auth".to_owned()]
    } else {
        arguments
    };
    #[cfg(unix)]
    let environment = pinned_fallback
        .as_ref()
        .map(|pinned| pinned.environment.clone())
        .unwrap_or_else(|| configured_login_environment(profile));
    #[cfg(not(unix))]
    let environment = configured_login_environment(profile);
    #[cfg(unix)]
    let program = pinned_fallback
        .as_ref()
        .map(|pinned| pinned.command.clone())
        .unwrap_or_else(|| std::path::PathBuf::from(configured_program));
    #[cfg(not(unix))]
    let program = resolve_harness_program(&configured_program, &environment);
    println!(
        "Running `{} {}` against {}.",
        program.display(),
        arguments.join(" "),
        profile.home.display()
    );
    anyhow::ensure!(
        !mj_core::subprocess::preparation_cancellation().is_cancelled(),
        "login cancelled"
    );
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&arguments)
        .envs(&environment)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| login_spawn_error(error, &program, profile.kind, profile_id))
        .with_context(|| {
            format!(
                "run `{} {}` for profile {profile_id}",
                program.display(),
                arguments.join(" ")
            )
        })?;
    let cancellation = mj_core::subprocess::preparation_cancellation();
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = cancellation.cancelled() => {
            child.start_kill().context("stop login command")?;
            child.wait().await.context("reap login command")?;
            bail!("login cancelled");
        }
    };

    report_login_change(profile_id, profile, before, stored)?;
    Ok(status.code().unwrap_or(1))
}

fn configured_login_environment(
    profile: &mj_core::config::HarnessProfile,
) -> BTreeMap<String, String> {
    let mut environment = profile.environment.resolved().clone();
    profile.kind.configure_profile_home_environment(
        &profile.home,
        mj_core::config::HarnessHost::current(),
        &mut environment,
    );
    environment
}

/// Resolve a configured harness command as the child would on Windows.
///
/// Rust doubles a missing `.exe` suffix itself, but npm installs command
/// shims with `PATHEXT` suffixes such as `.cmd`. Resolve those explicitly so
/// the same configured program works for `mj login` and `claude setup-token`.
#[cfg(windows)]
fn resolve_harness_program(
    program: &str,
    environment: &BTreeMap<String, String>,
) -> std::path::PathBuf {
    let configured_path = environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| std::ffi::OsStr::new(value));
    let inherited_path = std::env::var_os("PATH");
    let path = configured_path.or(inherited_path.as_deref());
    if let Some(program) = mj_core::program_path::find_program_on_path(program, path) {
        return program;
    }
    std::path::PathBuf::from(program)
}

/// The credential fingerprint a login is expected to change.
fn profile_credential_fingerprint(
    profile: &mj_core::config::HarnessProfile,
) -> Result<Option<String>> {
    if profile.auth_scheme() == mj_core::config::AuthScheme::NativeLoginStore {
        return Ok(crate::opencode_logins::profile_logins(profile)?
            .map(|logins| mj_core::credentials::credential_fingerprint(&logins)));
    }
    let (snapshot, _) =
        mj_core::credentials::read_credential_file(profile.kind, &profile.authentication_marker())?;
    Ok(snapshot.present.then_some(snapshot.fingerprint))
}

fn report_login_change(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
    before: Option<String>,
    stored: bool,
) -> Result<()> {
    let after = profile_credential_fingerprint(profile)?;
    if after.is_some() && after != before {
        if stored {
            println!(
                "Credentials updated for profile {profile_id}. Live sessions reload with them within about a minute, once idle, while the Mjolnir daemon is running."
            );
        } else {
            println!(
                "Credentials updated for profile {profile_id}. Live sessions pick them up within about a minute while the Mjolnir daemon is running."
            );
        }
    } else {
        println!("Credentials for profile {profile_id} are unchanged.");
    }
    Ok(())
}

/// Say which program is missing and how to get it, rather than the bare
/// "No such file or directory" the operating system reports.
fn login_spawn_error(
    error: io::Error,
    program: &std::path::Path,
    kind: mj_core::config::HarnessKind,
    profile_id: &str,
) -> anyhow::Error {
    if error.kind() != io::ErrorKind::NotFound {
        return error.into();
    }
    anyhow::anyhow!(
        "`{}` is not installed or is not on PATH, so the {} login cannot run. {}, then run `mj login --profile {profile_id}` again.",
        program.display(),
        kind.display_name(),
        kind.install_advice()
    )
}

/// Mint a long-lived Claude subscription token and store it for the profile.
///
/// Claude Code has no early-refresh command, and its `/login` grant rotates: a
/// container copy that reaches expiry at the same instant as the host spends a
/// single-use refresh token the host has already spent, and the container's
/// turn dies. `claude setup-token` mints a one-year token that never rotates,
/// and Claude Code reads it from `CLAUDE_CODE_OAUTH_TOKEN` ahead of the
/// credentials file, so there is nothing left to race.
async fn store_claude_setup_token(
    profile_id: &str,
    profile: &mj_core::config::HarnessProfile,
) -> Result<()> {
    use mj_core::config::HarnessKind;
    use mj_core::credentials::CLAUDE_OAUTH_TOKEN_ENV;

    if profile.kind != HarnessKind::Claude {
        bail!(
            "--setup-token applies to Claude profiles only; profile {profile_id} is a {} profile",
            profile.kind.display_name()
        );
    }
    let token_path = mj_core::credentials::claude_oauth_token_path(profile_id);

    println!(
        "Running `claude setup-token` against {}.",
        profile.home.display()
    );
    #[cfg(unix)]
    let (program, environment) = match acp_login::pinned_cli(profile).await {
        Ok(pinned) => (pinned.command.clone(), pinned.environment.clone()),
        Err(error) => {
            eprintln!(
                "Could not prepare the pinned Claude CLI ({error:#}); using `claude` from PATH."
            );
            (
                std::path::PathBuf::from("claude"),
                configured_login_environment(profile),
            )
        }
    };
    #[cfg(not(unix))]
    let (program, environment) = {
        let environment = configured_login_environment(profile);
        let program = resolve_harness_program("claude", &environment);
        (program, environment)
    };
    let mut command = tokio::process::Command::new(&program);
    command.arg("setup-token").envs(&environment);
    let output = mj_core::subprocess::run_capturing_stdout_cancellable(&mut command)
        .await
        .map_err(|error| {
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|cause| cause.kind() == io::ErrorKind::NotFound)
            }) {
                login_spawn_error(
                    io::Error::from(io::ErrorKind::NotFound),
                    &program,
                    mj_core::config::HarnessKind::Claude,
                    profile_id,
                )
            } else {
                error
            }
        })?;
    if !output.status.success() {
        bail!(
            "`claude setup-token` exited with {} for profile {profile_id}",
            output.status
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .context("`claude setup-token` printed non-UTF-8 output")?;
    let token = extract_setup_token(&stdout).with_context(|| {
        format!("`claude setup-token` printed no token for profile {profile_id}")
    })?;
    mj_core::credentials::write_claude_oauth_token(&token_path, token.as_bytes())?;

    let verify_token = token.clone();
    let verify_program = program.clone();
    let mut command = tokio::process::Command::new(verify_program);
    command
        .args(["auth", "status"])
        .envs(&environment)
        .env(CLAUDE_OAUTH_TOKEN_ENV, &verify_token);
    let verified = mj_core::subprocess::run_capturing_stdout_cancellable(&mut command)
        .await
        .context("run `claude auth status`")?;
    if !verified.status.success() {
        bail!(
            "the stored token did not authenticate: `claude auth status` exited with {}. \
             The token is at {}; remove it to go back to the synced credentials file.",
            verified.status,
            token_path.display()
        );
    }
    if let Some(method) = reported_auth_method(&verified.stdout)
        && method != "oauth_token"
    {
        bail!(
            "Claude Code authenticated with {method:?} rather than the stored token. \
             The token is at {}; remove it to go back to the synced credentials file.",
            token_path.display()
        );
    }

    println!(
        "Stored a long-lived Claude token for profile {profile_id} at {}.",
        token_path.display()
    );
    println!(
        "New and resumed sessions of this profile run with {CLAUDE_OAUTH_TOKEN_ENV} set, so they no longer race the host over a rotating login."
    );
    Ok(())
}

/// How `claude auth status` says it authenticated, when it says so.
///
/// Exit code alone proves little: `claude auth status` reports success for any
/// value of `CLAUDE_CODE_OAUTH_TOKEN`. What is worth confirming is that Claude
/// Code read the variable and gave it precedence, which the reported method
/// says. A build whose output carries no method is not a failure to report.
fn reported_auth_method(stdout: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()?
        .get("authMethod")?
        .as_str()
        .map(str::to_owned)
}

/// The token in `claude setup-token` output.
///
/// The command prints instructions as well as the token, so the last
/// whitespace-free `sk-ant-oat01-` line wins. A future format that stops using
/// that prefix still leaves the token as the last thing printed, so the last
/// non-empty line is the fallback.
fn extract_setup_token(stdout: &str) -> Option<String> {
    let candidates = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    candidates
        .iter()
        .rev()
        .find(|line| line.starts_with("sk-ant-oat01-") && !line.contains(char::is_whitespace))
        .or_else(|| candidates.last())
        .map(|line| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// `mj login` for a harness whose CLI is not installed names the missing
    /// program and how to install it, instead of a bare ENOENT.
    // Hard-won: 613642d3f7: Launch finding J-12 found login exposed a bare ENOENT for missing Codex; this checks the program, installation command, and profile login remedy.
    #[test]
    fn a_missing_login_program_is_named_with_how_to_install_it() {
        let error = login_spawn_error(
            io::Error::from(io::ErrorKind::NotFound),
            std::path::Path::new("codex"),
            mj_core::config::HarnessKind::Codex,
            "codex",
        );
        let message = format!("{error:#}");
        assert!(message.contains("`codex` is not installed"), "{message}");
        assert!(
            message.contains("npm install -g @openai/codex"),
            "{message}"
        );
        assert!(message.contains("mj login --profile codex"), "{message}");

        let other = login_spawn_error(
            io::Error::from(io::ErrorKind::PermissionDenied),
            std::path::Path::new("codex"),
            mj_core::config::HarnessKind::Codex,
            "codex",
        );
        assert!(!format!("{other:#}").contains("not installed"));
    }

    // Hard-won: 50a0277d2: Windows login passed bare npm commands to Rust, which only resolves .exe.
    #[cfg(windows)]
    #[test]
    fn login_resolves_and_runs_an_npm_style_cmd_shim_from_the_child_path() {
        let directory = tempfile::tempdir().unwrap();
        let shim = directory.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\necho harness-ok\r\n").unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();
        let environment =
            BTreeMap::from([("PATH".to_owned(), path.to_string_lossy().into_owned())]);

        let program = resolve_harness_program("claude", &environment);
        assert!(
            program
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("claude.cmd")),
            "{program:?}"
        );
        let output = std::process::Command::new(program).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("harness-ok"),
            "{output:?}"
        );
    }

    // Hard-won: 594e6f27: Claude auth status accepted a made-up setup token by exit code alone.
    #[test]
    fn the_verification_reads_which_credential_claude_code_actually_used() {
        assert_eq!(
            reported_auth_method(br#"{"loggedIn":true,"authMethod":"oauth_token"}"#).as_deref(),
            Some("oauth_token")
        );
        assert_eq!(
            reported_auth_method(br#"{"loggedIn":true,"authMethod":"claudeai"}"#).as_deref(),
            Some("claudeai")
        );
        // Output that names no method leaves the exit code as the only check.
        assert_eq!(reported_auth_method(b"Logged in as someone\n"), None);
        assert_eq!(reported_auth_method(br#"{"loggedIn":true}"#), None);
    }
}
