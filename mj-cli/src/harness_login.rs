//! `mj login` as one procedure over the selected harness runner.
//!
//! The worker signs the profile home in (`mj-worker worker sign-in`), on this
//! machine with Mjolnir's pinned harness or in a container from the agent
//! image, as `[harnesses] run_on` selects. This side writes the spec, gives
//! the worker the person's terminal, and cleans up; the runner supplies every
//! step that depends on where the harness runs.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use mj_controller::harness_runner::{self, WorkerIo, WorkerRun};
use mj_core::config::{Config, HarnessKind, HarnessProfile};
use mj_core::worker_launch::{HarnessRuntimePolicy, SignInAction, SignInSpec};

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

/// One sign-in action for `profile`.
pub(crate) async fn run(
    config: &Config,
    profile: &HarnessProfile,
    action: SignInAction,
    method: Option<String>,
    cli: bool,
    extra_environment: &[(&str, &str)],
    output: Output,
) -> Result<SignInRun> {
    let selection_config = config.clone();
    let runner = tokio::task::spawn_blocking(move || {
        harness_runner::select(&selection_config, &mj_controller::targets::ProcessExecutor)
    })
    .await
    .context("choose where the harness runs")??;
    // A profile pointed at the person's own OpenCode, Muse, or MiMo login
    // database signs in through their own installation's CLI, where the
    // runner can reach it; Mjolnir's pinned version could migrate a database
    // it does not own. `--cli` likewise asks for the installation the runner
    // finds rather than Mjolnir's pin.
    let foreign_store = harness_runner::uses_foreign_login_store(profile);
    if foreign_store {
        runner.may_sign_in_foreign_store(profile)?;
    }
    let runtime = if foreign_store || cli {
        HarnessRuntimePolicy::Ambient
    } else {
        runner.runtime()
    };
    create_profile_home(&profile.home)?;
    let work = harness_runner::work_directory(runner.as_ref(), "sign-in-")?;
    let view = runner.work_view(work.path());

    let mut environment = runner.base_environment();
    environment.extend(profile.environment.resolved().clone());
    // A provider key exported to `mj` chooses the key method, as it always
    // has, wherever the harness runs.
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
    environment.extend(runner.profile_environment(profile.kind, &profile.home));
    for (name, value) in extra_environment {
        environment.insert((*name).to_owned(), (*value).to_owned());
    }
    let spec = SignInSpec {
        harness: profile.kind,
        profile_home: runner.profile_view(profile.kind, &profile.home),
        environment,
        action,
        runtime,
        method,
        cli: cli || foreign_store,
        desktop: host_has_desktop(),
    };
    mj_core::config::atomic_write(
        &work.path().join("sign-in.json"),
        &serde_json::to_vec(&spec)?,
    )?;
    let run = WorkerRun {
        purpose: "sign in".to_owned(),
        profile: Some((profile.kind, profile.home.clone())),
        work: Some(work.path().to_path_buf()),
        arguments: vec![
            "worker".to_owned(),
            "sign-in".to_owned(),
            "--spec".to_owned(),
            view.join("sign-in.json").to_string_lossy().into_owned(),
        ],
        environment: Default::default(),
        io: match output {
            Output::Terminal => WorkerIo::Terminal {
                callback_ports: callback_ports(profile.kind).to_vec(),
            },
            Output::Captured => WorkerIo::Stream,
        },
    };
    // Finding or downloading the worker, and writing the container's files,
    // is blocking work.
    let launch_runner = runner.clone();
    let launch = tokio::task::spawn_blocking(move || launch_runner.launch_worker(run))
        .await
        .context("prepare the sign-in")??;
    if output == Output::Terminal {
        eprintln!(
            "Signing in to {} {}.",
            profile.kind.display_name(),
            runner.describe()
        );
    }

    let mut child = launch.process();
    child
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(match output {
            Output::Terminal => Stdio::inherit(),
            Output::Captured => Stdio::piped(),
        })
        .kill_on_drop(true);
    // Not `output()`: tokio pipes both streams for it, which would swallow
    // the sign-in's own terminal output.
    let result = async {
        let mut child = child
            .spawn()
            .with_context(|| format!("run {}", launch.program))?;
        let stdout = child.stdout.take();
        let reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut bytes).await?;
            }
            Ok::<_, std::io::Error>(bytes)
        });
        let status = child.wait().await.context("wait for the sign-in")?;
        let stdout = reader
            .await
            .context("sign-in output reader stopped")?
            .context("read the sign-in output")?;
        anyhow::Ok(SignInRun { status, stdout })
    }
    .await;
    // Undone on every exit: a container is removed, a browser relay stopped.
    if let Err(error) = tokio::task::spawn_blocking(move || launch.finish()).await {
        eprintln!("Could not clean up after the sign-in: {error}");
    }
    result
}

fn create_profile_home(home: &Path) -> Result<()> {
    let created = !home.exists();
    std::fs::create_dir_all(home).with_context(|| {
        format!(
            "create the harness profile home {} for sign-in",
            home.display()
        )
    })?;
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "restrict permissions on the harness profile home {}",
                    home.display()
                )
            },
        )?;
    }
    #[cfg(not(unix))]
    let _ = created;
    Ok(())
}

/// The `localhost` ports a harness's browser sign-in returns to. Codex's
/// ChatGPT login listens on 1455. A runner that shares this machine's network
/// needs none of them.
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
