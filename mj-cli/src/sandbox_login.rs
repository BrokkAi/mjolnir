//! `mj login` runs the harness in the harness sandbox, never on this machine.
//!
//! The worker inside the container signs the mounted profile home in
//! (`mj-worker worker sign-in`); this side gives it the person's terminal and
//! opens any browser address it asks for. The sandbox has no display, so its
//! `BROWSER` writes the address to a file in the shared work directory and
//! this process opens it here.

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mj_controller::harness_sandbox::{
    self, HarnessSandbox, SANDBOX_WORK, SANDBOX_WORKER, SandboxMount, SandboxNetwork, SandboxRun,
};
use mj_core::config::{Config, HarnessKind, HarnessProfile};
use mj_core::worker_launch::{SignInAction, SignInSpec};

/// Whether the child's standard output reaches the terminal or is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    Terminal,
    Captured,
}

pub(crate) struct SignInRun {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
}

/// One sign-in action for `profile`, run in the harness sandbox.
pub(crate) async fn run(
    config: &Config,
    profile: &HarnessProfile,
    action: SignInAction,
    method: Option<String>,
    cli: bool,
    extra_environment: &[(&str, &str)],
    output: Output,
) -> Result<SignInRun> {
    refuse_foreign_login_store(profile)?;
    create_profile_home(&profile.home)?;
    let selection_config = config.clone();
    let sandbox = tokio::task::spawn_blocking(move || {
        HarnessSandbox::select(&selection_config, &mj_controller::targets::ProcessExecutor)
    })
    .await
    .context("choose the harness sandbox")??;
    let architecture = sandbox.architecture()?;
    let worker = tokio::task::spawn_blocking(move || {
        mj_controller::controller::portable_linux_worker(architecture)
    })
    .await
    .context("find the Linux worker")??;

    let work_root = mj_core::config::data_dir()
        .join("harness-sandbox")
        .join("work");
    std::fs::create_dir_all(&work_root)
        .with_context(|| format!("create {}", work_root.display()))?;
    let work = tempfile::Builder::new()
        .prefix("sign-in-")
        .tempdir_in(&work_root)
        .context("create the sign-in work directory")?;
    restrict(work.path(), 0o700)?;
    let opener = work.path().join("open-url");
    std::fs::write(
        &opener,
        format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> {SANDBOX_WORK}/urls\n"),
    )?;
    restrict(&opener, 0o755)?;

    let mut environment = profile.environment.resolved().clone();
    // A provider key exported to `mj` chooses the key method, as it did when
    // sign-in ran here.
    for name in ["CODEX_API_KEY", "OPENAI_API_KEY"]
        .into_iter()
        .chain(profile.kind.api_key_variable())
    {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            environment.entry(name.to_owned()).or_insert(value);
        }
    }
    environment.insert("BROWSER".to_owned(), format!("{SANDBOX_WORK}/open-url"));
    environment.extend(harness_sandbox::home_environment());
    for (name, value) in extra_environment {
        environment.insert((*name).to_owned(), (*value).to_owned());
    }
    let desktop = host_has_desktop();
    let spec = SignInSpec {
        harness: profile.kind,
        profile_home: harness_sandbox::profile_mount_point(profile.kind),
        environment,
        action,
        method,
        cli,
        desktop,
    };
    mj_core::config::atomic_write(
        &work.path().join("sign-in.json"),
        &serde_json::to_vec(&spec)?,
    )?;
    let terminal = output == Output::Terminal && std::io::stdin().is_terminal();
    let command = sandbox.command(&SandboxRun {
        purpose: "sign in".to_owned(),
        mounts: vec![
            SandboxMount::writable(
                &profile.home,
                harness_sandbox::profile_mount_point(profile.kind)
                    .to_string_lossy()
                    .into_owned(),
            ),
            SandboxMount::writable(work.path(), SANDBOX_WORK),
            SandboxMount::read_only(worker, SANDBOX_WORKER),
        ],
        command: vec![
            SANDBOX_WORKER.to_owned(),
            "worker".to_owned(),
            "sign-in".to_owned(),
            "--spec".to_owned(),
            format!("{SANDBOX_WORK}/sign-in.json"),
        ],
        stdin: true,
        terminal,
        network: SandboxNetwork::BrowserCallbacks(callback_ports(profile.kind).to_vec()),
        working_directory: None,
        environment: Default::default(),
    })?;
    if output == Output::Terminal {
        eprintln!(
            "Signing in in a {} container from {}.",
            sandbox.engine().command(),
            sandbox.image()
        );
    }

    let relay = BrowserRelay::start(work.path().join("urls"), desktop);
    let mut child = command.process();
    child
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(match output {
            Output::Terminal => Stdio::inherit(),
            Output::Captured => Stdio::piped(),
        })
        .kill_on_drop(true);
    // Not `output()`: tokio's pipes both streams for it, which would swallow
    // the sign-in's own terminal output.
    let result = async {
        let mut child = child
            .spawn()
            .with_context(|| format!("run {}", command.program))?;
        let stdout = child.stdout.take();
        let reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut bytes).await?;
            }
            Ok::<_, std::io::Error>(bytes)
        });
        let status = child
            .wait()
            .await
            .context("wait for the sign-in container")?;
        let stdout = reader
            .await
            .context("sign-in output reader stopped")?
            .context("read the sign-in output")?;
        anyhow::Ok(SignInRun { status, stdout })
    }
    .await;
    relay.stop();
    let remove = command.clone();
    let _ = tokio::task::spawn_blocking(move || {
        remove.remove(
            &mj_controller::targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(
                30,
            )),
        )
    })
    .await;
    result
}

/// A profile pointed at the person's own OpenCode, Muse, or MiMo installation
/// keeps its logins in a database that installation owns. The sandbox's
/// pinned harness could migrate it out from under them, so sign-in stays with
/// that installation.
fn refuse_foreign_login_store(profile: &HarnessProfile) -> Result<()> {
    let own_nested = profile.kind.nested_home() && profile.kind.is_own_nested_home(&profile.home);
    let own_data = !profile
        .kind
        .data_source(&profile.home)
        .starts_with(&profile.home);
    if own_nested || own_data {
        let (program, args) = profile.kind.native_login_command();
        bail!(
            "this profile uses your own {} installation, whose login database Mjolnir does not own. Sign in with that installation (`{program} {}`), or add a Mjolnir-managed {} profile in Settings → Agent Profiles, which signs in inside a container.",
            profile.kind.display_name(),
            args.join(" "),
            profile.kind.display_name()
        );
    }
    Ok(())
}

fn create_profile_home(home: &Path) -> Result<()> {
    let created = !home.exists();
    std::fs::create_dir_all(home).with_context(|| {
        format!(
            "create the harness profile home {} for sign-in",
            home.display()
        )
    })?;
    if created {
        restrict(home, 0o700)?;
    }
    Ok(())
}

fn restrict(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("set permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// The `localhost` ports a harness's browser sign-in returns to. Codex's
/// ChatGPT login listens on 1455. On Linux the sandbox shares the host's
/// network and every port reaches it; elsewhere these are published.
fn callback_ports(kind: HarnessKind) -> &'static [u16] {
    match kind {
        HarnessKind::Codex => &[1455],
        _ => &[],
    }
}

/// Whether this machine can show a browser for a callback flow.
fn host_has_desktop() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
        || ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// Opens the addresses the sandbox's `BROWSER` records, here.
struct BrowserRelay {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BrowserRelay {
    fn start(path: PathBuf, desktop: bool) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let running = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut opened = 0usize;
            loop {
                let finished = running.load(std::sync::atomic::Ordering::Acquire);
                if let Ok(file) = std::fs::File::open(&path) {
                    let lines = std::io::BufReader::new(file)
                        .lines()
                        .map_while(std::result::Result::ok)
                        .collect::<Vec<_>>();
                    for url in lines.iter().skip(opened) {
                        open(url, desktop);
                    }
                    opened = lines.len();
                }
                if finished {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("The browser relay for sign-in stopped unexpectedly.");
        }
    }
}

fn open(url: &str, desktop: bool) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }
    if desktop && webbrowser::open(url).is_ok() {
        eprintln!("Opened {url} in your browser.");
    } else {
        eprintln!("Open this address in a browser to sign in: {url}");
    }
}
