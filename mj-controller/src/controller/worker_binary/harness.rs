use super::*;
use mj_core::harness_runtime::{GROK_VERSION, KIMI_VERSION};

/// Reuse the worker's installer for controller-owned vendor services. The
/// response carries private environment values and must never enter logs.
pub(crate) async fn prepare_local_managed_harness(
    harness: HarnessKind,
    home: PathBuf,
    environment: std::collections::BTreeMap<String, String>,
) -> Result<(mj_core::worker_launch::PreparedHarnessInfo, File)> {
    use futures::FutureExt;
    let (reply, response) = tokio::sync::oneshot::channel();
    // The owner retains staging through child completion even if this caller
    // disappears. Panic and ordinary failure are both reported independently.
    drop(tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(prepare_local_managed_harness_owned(
            harness,
            home,
            environment,
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "managed harness preparation owner panicked"
            ))
        });
        if let Err(error) = &result {
            tracing::warn!(%error, "managed harness preparation failed");
        }
        let _ = reply.send(result);
    }));
    response
        .await
        .context("managed harness preparation owner stopped")?
}

async fn prepare_local_managed_harness_owned(
    harness: HarnessKind,
    home: PathBuf,
    environment: std::collections::BTreeMap<String, String>,
) -> Result<(mj_core::worker_launch::PreparedHarnessInfo, File)> {
    let (binary, staging, config_path) = tokio::task::spawn_blocking(move || -> Result<_> {
        let binary =
            binary_select::materialize_worker_source(native_worker_binary_prerequisite()?)?;
        let staging =
            tempfile::tempdir().context("create private harness preparation directory")?;
        let config_path = staging.path().join("launch.json");
        let launch = WorkerLaunchConfig {
            expected_runtime_identity: None,
            goal_resume_request: None,
            run_mode: Default::default(),
            session_id: "vendor-service".into(),
            subagents: Default::default(),
            handback_tool: false,
            review_capture: false,
            target_environment: Default::default(),
            seed_image_environment: false,
            harness,
            harness_home: home.clone(),
            authentication_marker: None,
            bridge_command: harness.cli_binary_name().into(),
            bridge_args: Vec::new(),
            harness_runtime: HarnessRuntimePolicy::Managed,
            environment,
            excluded_environment: Vec::new(),
            cwd: home,
            additional_directories: Vec::new(),
            native_session_id: None,
            project_memory: None,
            execution_policy: mj_core::config::ExecutionPolicy::ConfiguredApprovals,
        };
        launch.write(&config_path)?;
        Ok((binary, staging, config_path))
    })
    .await
    .context("prepare local harness configuration task failed")??;
    // Keep rather than Drop: runtime teardown must not remove files under a
    // process whose termination has not yet been confirmed.
    let staging = staging.keep();
    let mut child_reaped = false;
    let prepared =
        prepare_local_harness_with_worker(&binary, &config_path, &mut child_reaped).await;
    if child_reaped {
        tokio::fs::remove_dir_all(&staging)
            .await
            .context("remove completed harness preparation files")?;
    } else {
        tracing::warn!(path = %staging.display(), "retaining private harness preparation files because child exit was not confirmed");
    }
    prepared
}

async fn prepare_local_harness_with_worker(
    binary: &Path,
    config_path: &Path,
    child_reaped: &mut bool,
) -> Result<(mj_core::worker_launch::PreparedHarnessInfo, File)> {
    let directory = config_path
        .parent()
        .context("harness preparation config has no directory")?;
    let info_path = directory.join("runtime.json");
    let ack_path = directory.join("runtime.ack");
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["worker", "prepare-harness", "--config"])
        .arg(config_path)
        .arg("--runtime-info")
        .arg(&info_path)
        .arg("--runtime-ack")
        .arg(&ack_path);
    let child = mj_core::subprocess::run_bounded(
        &mut command,
        1024 * 1024,
        std::time::Duration::from_secs(300),
    );
    tokio::pin!(child);
    let transfer = receive_prepared_harness(&info_path);
    tokio::pin!(transfer);
    let result = tokio::select! {
        biased;
        output = &mut child => {
            let output = output.context("prepare local managed harness")?;
            *child_reaped = true;
            ensure!(output.status.success(), "managed harness preparation failed ({})", output.status);
            anyhow::bail!("managed harness preparation exited before its runtime lease was transferred");
        }
        result = &mut transfer => result,
    };
    let ack = if result.is_ok() {
        b"retained".as_slice()
    } else {
        b"aborted".as_slice()
    };
    let ack_result =
        match tokio::task::spawn_blocking(move || mj_core::config::atomic_write(&ack_path, ack))
            .await
        {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!(
                "acknowledge harness lease task failed: {error}"
            )),
        };
    // Never abandon the process owner on a protocol/ACK error. Its bounded
    // supervisor finishes termination before staging can be removed.
    let output = child.await.context("finish managed harness preparation")?;
    *child_reaped = true;
    ack_result.context("acknowledge prepared harness lease")?;
    let result = result?;
    ensure!(
        output.status.success(),
        "managed harness preparation failed ({})",
        output.status
    );
    Ok(result)
}

async fn receive_prepared_harness(
    info_path: &Path,
) -> Result<(mj_core::worker_launch::PreparedHarnessInfo, File)> {
    use tokio::io::AsyncReadExt;
    let file = loop {
        match tokio::fs::File::open(info_path).await {
            Ok(file) => break file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("read private harness preparation response"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    let mut body = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut body)
        .await
        .context("read private harness preparation response")?;
    ensure!(
        body.len() <= 1024 * 1024,
        "managed harness preparation response exceeds size limit"
    );
    let prepared: mj_core::worker_launch::PreparedHarnessInfo = serde_json::from_slice(&body)
        .map_err(|error| {
            anyhow::anyhow!(
                "invalid managed harness preparation response ({:?}, line {}, column {})",
                error.classify(),
                error.line(),
                error.column()
            )
        })?;
    ensure!(
        prepared.version == mj_core::worker_launch::PreparedHarnessInfo::VERSION,
        "unsupported managed harness preparation response version {}",
        prepared.version
    );
    let lease_path = prepared.lease_path.clone();
    let lease = tokio::task::spawn_blocking(move || -> Result<File> {
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lease_path)
            .context("open prepared harness lease")?;
        lease
            .lock_shared()
            .context("retain prepared harness lease")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let held = lease.metadata()?;
            let named = std::fs::metadata(&lease_path)?;
            ensure!(
                held.dev() == named.dev() && held.ino() == named.ino(),
                "prepared harness lease was replaced during transfer"
            );
        }
        Ok(lease)
    })
    .await
    .context("retain prepared harness lease task failed")??;
    Ok((prepared, lease))
}

#[cfg(all(test, unix))]
mod preparation_tests {
    use super::*;

    fn fake_worker(root: &Path, script: &str) -> PathBuf {
        mj_core::test_hooks::install_fake_command(root, "worker", script);
        root.join("worker")
    }

    #[tokio::test]
    async fn kimi_preparation_drains_large_private_output_and_retains_the_runtime_lease() {
        let directory = tempfile::tempdir().unwrap();
        let lease_path = directory.path().join(".lease");
        std::fs::write(&lease_path, []).unwrap();
        let config = directory.path().join("launch.json");
        let response = mj_core::worker_launch::PreparedHarnessInfo {
            version: mj_core::worker_launch::PreparedHarnessInfo::VERSION,
            command: directory.path().join("kimi"),
            environment: std::collections::BTreeMap::from([("SECRET".into(), "x".repeat(100_000))]),
            lease_path: lease_path.clone(),
        };
        std::fs::write(&config, serde_json::to_vec(&response).unwrap()).unwrap();
        let binary = fake_worker(
            directory.path(),
            "#!/bin/sh\n[ \"$1 $2 $3\" = 'worker prepare-harness --config' ] || exit 2\ncat \"$4\" >&2\ncat \"$4\"\ncp \"$4\" \"$6.tmp\"\nmv \"$6.tmp\" \"$6\"\nwhile [ ! -e \"$8\" ]; do sleep 0.01; done\n[ \"$(cat \"$8\")\" = retained ] || exit 3\n",
        );
        let (prepared, lease) = prepare_local_harness_with_worker(&binary, &config, &mut false)
            .await
            .unwrap();
        assert_eq!(prepared.environment["SECRET"].len(), 100_000);
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lease_path)
            .unwrap();
        assert!(contender.try_lock().is_err());
        drop(lease);
        // Concurrent process tests can fork while this descriptor is open.
        // CLOEXEC closes their copies at exec, rather than at the parent's drop.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match contender.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("probe released runtime lease: {error}"),
                }
            }
        })
        .await
        .expect("runtime lease remained locked after intentional owners dropped and fork/exec copies should have closed");
    }

    #[tokio::test]
    async fn kimi_preparation_never_includes_private_child_output_in_errors() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fake_worker(
            directory.path(),
            "#!/bin/sh\nprintf secret-token\nprintf secret-token >&2\nexit 3\n",
        );
        let error = prepare_local_harness_with_worker(
            &binary,
            &directory.path().join("config"),
            &mut false,
        )
        .await
        .unwrap_err();
        assert!(!format!("{error:#}").contains("secret-token"));
        assert!(
            error
                .to_string()
                .contains("managed harness preparation failed")
        );
    }
    #[tokio::test]
    async fn kimi_preparation_redacts_invalid_responses_and_rejects_old_empty_success() {
        for (script, expected) in [
            (
                "#!/bin/sh\nprintf '\"secret-token\"' > \"$6\"\nwhile [ ! -e \"$8\" ]; do sleep 0.01; done\nexit 3\n",
                "invalid managed harness preparation response",
            ),
            (
                "#!/bin/sh\nexit 0\n",
                "exited before its runtime lease was transferred",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let binary = fake_worker(directory.path(), script);
            let mut reaped = false;
            let error = prepare_local_harness_with_worker(
                &binary,
                &directory.path().join("config"),
                &mut reaped,
            )
            .await
            .unwrap_err();
            assert!(
                reaped,
                "preparation failed before confirmed child exit: {error:#}"
            );
            assert!(!format!("{error:#}").contains("secret-token"));
            assert!(error.to_string().contains(expected), "{error:#}");
        }
    }
}

/// The repository paths a worker opens. `session_id` and `container_workspace`
/// identify the session whose workspace is used, which for a sub-agent child is
/// its parent.
pub(super) fn workspace_paths(
    locator: &targets::TargetLocator,
    bundle: &ProjectBundle,
    session_id: &str,
    container_workspace: Option<&Path>,
) -> Result<(String, Vec<String>)> {
    let root = match locator {
        targets::TargetLocator::LocalBare { .. } => {
            bail!("local bare projects use their selected directory")
        }
        targets::TargetLocator::LocalPodman { .. }
        | targets::TargetLocator::LocalDocker { .. }
        | targets::TargetLocator::AppleContainer { .. }
        | targets::TargetLocator::SshPodman { .. }
        | targets::TargetLocator::SshDocker { .. } => {
            targets::container_workspace_root(container_workspace)
        }
        targets::TargetLocator::AwsEc2 { workspace, .. }
        | targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
    };
    if matches!(locator, targets::TargetLocator::AwsEc2 { .. }) {
        let expected = format!(".local/share/hel/workspaces/{session_id}");
        if root != expected {
            bail!("AWS workspace does not match session")
        }
    }
    let primary = bundle.primary().context("bundle primary is missing")?;
    let primary_path = format!("{root}/{}", primary.destination.to_string_lossy());
    let additional = bundle
        .repositories
        .iter()
        .filter(|repository| repository.id != bundle.primary_repo)
        .map(|repository| format!("{root}/{}", repository.destination.to_string_lossy()))
        .collect();
    Ok((primary_path, additional))
}

// Package versions for ACP bridges and their harnesses. Keep these in lockstep with the global
// npm installs in containers/Containerfile.agent-dev; bridge_pins_match_containerfile() below
// fails the build when they drift.
// Codex 0.148 reuses pending MCP startups during runtime reconciliation. Older
// releases could cancel the first project-memory startup while immediately
// replacing it with an equivalent connection, leaving a false failed-tool
// event at the beginning of every session.
/// Stage shown after the worker is reachable and while its ACP bridge becomes
/// ready. Every remaining harness launches through a default launcher that can
/// fetch it, so the stage names the harness being installed.
pub(in crate::controller) fn bridge_readiness_stage(profile: &HarnessProfile) -> ProvisionStage {
    ProvisionStage::Installing(profile.kind)
}

/// The Kimi and Grok fallbacks install the pinned release before starting
/// the ACP server. The installer writes progress on stdout, which is the ACP
/// stream, so its stdout goes to stderr, where the worker keeps the bridge's
/// diagnostics (#1136).
pub(in crate::controller) fn bridge_launch(
    harness: mj_core::config::HarnessKind,
    policy: mj_core::config::ExecutionPolicy,
) -> (String, Vec<String>) {
    match harness {
        mj_core::config::HarnessKind::Muse => ("muse-acp".into(), Vec::new()),
        mj_core::config::HarnessKind::Codex | mj_core::config::HarnessKind::Claude => {
            let bridge =
                mj_core::harness_runtime::npm_bridge(harness).expect("npm harness has a bridge");
            ("sh".into(), vec!["-c".into(), bridge.bootstrap_script()])
        }
        mj_core::config::HarnessKind::Kimi => (
            "sh".into(),
            vec![
                "-c".into(),
                format!(
                    "if command -v kimi >/dev/null 2>&1; then exec kimi acp; elif [ -x \"$HOME/.kimi-code/bin/kimi\" ]; then exec \"$HOME/.kimi-code/bin/kimi\" acp; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://code.kimi.com/kimi-code/install.sh | KIMI_VERSION={KIMI_VERSION} bash >&2 && exec \"$HOME/.kimi-code/bin/kimi\" acp; else echo 'Mjolnir needs compatible Kimi Code or curl for its official installer; add the tool to PATH' >&2; exit 127; fi"
                ),
            ],
        ),
        mj_core::config::HarnessKind::Grok => {
            let acp = mj_core::config::HarnessKind::Grok
                .bridge_args(policy)
                .join(" ");
            (
                "sh".into(),
                vec![
                    "-c".into(),
                    format!(
                        "if command -v grok >/dev/null 2>&1; then exec grok {acp}; elif [ -x \"$GROK_HOME/bin/grok\" ]; then exec \"$GROK_HOME/bin/grok\" {acp}; elif [ -x \"$HOME/.grok/bin/grok\" ]; then exec \"$HOME/.grok/bin/grok\" {acp}; elif command -v curl >/dev/null 2>&1; then curl -fsSL https://x.ai/cli/install.sh | bash -s {GROK_VERSION} >&2 && exec \"$HOME/.grok/bin/grok\" {acp}; else echo 'Mjolnir needs compatible Grok Build or curl for its official installer; add the tool to PATH' >&2; exit 127; fi"
                    ),
                ],
            )
        }
    }
}

pub(in crate::controller) fn preflight_harness(
    template: &mj_core::config::TargetTemplate,
    profile: &HarnessProfile,
    executor: &impl CommandExecutor,
) -> Result<()> {
    use mj_core::config::TargetTemplate;
    if let TargetTemplate::SshBare { ssh, .. } = template {
        let ssh = SshTarget::from(ssh);
        execute_checked(executor, crate::targets::ssh_login_script(&ssh,
            "git --version >/dev/null || { echo 'Git is missing or unusable; install Git and, on macOS, the Xcode Command Line Tools on the target' >&2; exit 1; }",
        ).purpose("preflight remote Git"))?;
    }
    if !matches!(profile.kind, HarnessKind::Codex | HarnessKind::Claude) {
        return Ok(());
    }
    if !matches!(
        template,
        TargetTemplate::LocalBare | TargetTemplate::SshBare { .. }
    ) {
        return Ok(());
    }
    // Mjolnir installs its own pinned copy of the agent with Node.js and npm,
    // so the agent's own command is not a prerequisite. When Node.js or npm
    // is missing, though, the failure first says whether the agent itself is
    // missing too, because that is what a person on a fresh machine needs to
    // hear (launch finding R13-1).
    let cli = profile.kind.cli_binary_name();
    let script = format!(
        "status=0; if ! command -v node >/dev/null 2>&1; then echo 'Node.js is missing from PATH; install Node.js 22 or newer in the target environment' >&2; status=127; elif ! node -e 'process.exit(Number(process.versions.node.split(\".\")[0]) >= 22 ? 0 : 1)'; then echo 'Node.js 22 or newer is required in the target environment' >&2; status=1; elif ! command -v npm >/dev/null 2>&1 || ! npm --version >/dev/null; then echo 'npm is missing or unusable; install npm in the target environment' >&2; status=127; fi; if [ \"$status\" -ne 0 ] && ! command -v {cli} >/dev/null 2>&1; then exit {HARNESS_CLI_MISSING_STATUS}; fi; exit \"$status\""
    );
    let mut args = if profile.environment.contains_key("PATH") {
        vec![
            "-c".to_owned(),
            format!("export PATH=\"$1\"; {script}"),
            "mj-node-preflight".into(),
            profile.environment["PATH"].clone(),
        ]
    } else {
        vec!["-lc".to_owned(), script.clone()]
    };
    let (command, destination) = match template {
        TargetTemplate::LocalBare => (CommandSpec::new("sh", args), "local host".to_owned()),
        TargetTemplate::SshBare { ssh, .. } => {
            let ssh = SshTarget::from(ssh);
            if profile.environment.contains_key("PATH") {
                args.insert(0, "sh".into());
                (crate::targets::ssh_command(&ssh, args), ssh.destination)
            } else {
                (
                    crate::targets::ssh_login_script(&ssh, &script),
                    ssh.destination,
                )
            }
        }
        _ => unreachable!(),
    };
    let command = command.purpose("preflight managed harness Node.js and npm");
    let output = executor.execute(&command)?;
    if output.status == 0 {
        return Ok(());
    }
    let detail = crate::controller::command_error_detail(&output.stderr);
    let failure = if detail.is_empty() {
        anyhow::anyhow!("{} failed with status {}", command.purpose, output.status)
    } else {
        anyhow::anyhow!(detail)
    };
    let name = profile.kind.display_name();
    Err(if output.status == HARNESS_CLI_MISSING_STATUS {
        failure.context(format!(
            "{name} is not installed on {destination}: `{cli}` is not on PATH. {}, sign in to it, then retry the launch",
            profile.kind.install_advice()
        ))
    } else {
        failure.context(format!(
            "{name} launch preflight failed on {destination}; Node.js 22+ and npm must be available on the target PATH"
        ))
    })
}

/// The exit status the harness preflight script uses when Node.js or npm is
/// unusable and the agent's own command is missing as well.
const HARNESS_CLI_MISSING_STATUS: i32 = 3;

pub(super) const MJ_CONTAINER_ENVIRONMENT: &str = "## Mjolnir disposable environment\n\nThis session runs in a disposable Mjolnir container. When the session closes, Mjolnir checkpoints everything in project workspace directories under `/workspace`, including committed work, staged and unstaged changes, and untracked files. Mjolnir then removes the container.\n\nEverything outside `/workspace`, including installed packages, `$HOME`, and `/tmp`, is ephemeral and will be lost. Keep durable results in the workspace or push them to a remote.\n\nNew workspaces start on their own session branch from the default network fetch remote’s default branch. Local unpublished commits and uncommitted files are not copied. Use normal git push to publish the current branch to the configured network push destination. Closing saves a checkpoint; it does not publish commits or update the original local checkout. Resumed sessions restore their saved work.\n";

pub(in crate::controller) fn stage_profile(
    profile: &mj_core::config::HarnessProfile,
    destination: &Path,
) -> Result<()> {
    let harness = profile.kind;
    let source = profile.home.as_path();
    std::fs::create_dir_all(destination)?;
    // Only the files peculiar to a harness are listed per harness. The
    // instruction file and the synced skill directories are the same facts the
    // rest of Hel reads off `HarnessKind`, so they are appended from there
    // rather than repeated in all five arms.
    let harness_files: &[&str] = match harness {
        mj_core::config::HarnessKind::Muse => {
            &["auth.json", "settings.json", "trust.json", "rules"]
        }
        mj_core::config::HarnessKind::Codex => {
            &["auth.json", "config.toml", "instructions.md", "rules"]
        }
        mj_core::config::HarnessKind::Claude => &[
            ".claude.json",
            ".credentials.json",
            "settings.json",
            "plugins",
        ],
        mj_core::config::HarnessKind::Kimi => &[
            "credentials",
            "config.toml",
            "device_id",
            "SYSTEM.md",
            "mcp.json",
            "agents",
            "plugins",
        ],
        mj_core::config::HarnessKind::Grok => &["auth.json", "config.toml", "agent_id", "plugins"],
    };
    let allowlist: Vec<&str> = harness_files
        .iter()
        .copied()
        .chain(std::iter::once(harness.agent_instructions_file()))
        .chain(harness.synced_skill_dirs().iter().copied())
        .collect();
    // Allowlist entries (and, within each, a copied directory's children) are
    // independent of one another, so copying them concurrently shortens the
    // stage step for profiles with large skills/plugins trees.
    // What the harness maintains itself inside an allowlisted entry, such as
    // the skills Claude Code syncs from the user's claude.ai account, stays
    // behind: the harness provisions its own copy in the session home.
    let harness_owned = harness
        .harness_owned_skill_paths()
        .iter()
        .map(|owned| source.join(owned))
        .collect::<Vec<_>>();
    allowlist.par_iter().try_for_each(|name| -> Result<()> {
        let from = source.join(name);
        if from.exists() {
            copy_profile_entry_except(&from, &destination.join(name), &harness_owned)?;
        }
        Ok(())
    })?;
    Ok(())
}
