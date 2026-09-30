use super::*;

/// Stop the detached worker daemon at `worker_root` without deleting its files.
///
/// The script signals the worker's process group so a wedged ACP child dies
/// with it. Checkpoint then restarts the daemon against the same relay root.
pub(in crate::controller) fn stop_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    execute_checked(executor, stop_worker_command(locator, worker_root))?;
    Ok(())
}

/// Restore a stopped Podman target before signaling its worker. Checkpoint
/// recovery uses this instead of assuming every persisted target is running.
pub(in crate::controller) fn stop_worker_after_target_recovery(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    worker_root: &str,
) -> Result<()> {
    let target = targets::target_recovery_plan(locator, session_id)?;
    targets::ensure_recovery_target_running(executor, target.as_ref())
        .context("restore Mjolnir worker target")?;
    stop_worker(executor, locator, worker_root)
}

pub(super) fn stop_worker_command(
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> CommandSpec {
    let script = targets::stop_worker_daemon_script(worker_root);
    targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("stop Mjolnir worker daemon")
}

pub(super) fn worker_liveness_command(
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> CommandSpec {
    let script = targets::worker_daemon_liveness_script(worker_root);
    targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("probe Mjolnir worker daemon liveness")
}

pub(in crate::controller) fn start_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<()> {
    execute_checked(executor, start_worker_command(locator, worker_root))?;
    Ok(())
}

pub(super) fn start_worker_command(
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> CommandSpec {
    let binary = format!("{worker_root}/hel");
    let config = format!("{worker_root}/launch.json");
    // These files describe the worker's previous life. Clear them as part of
    // the launch, before the new daemon can be probed: a stale exit record
    // aborts startup, a stale socket makes a recovering daemon look ready and
    // invites the reconnect actor to kill it as unresponsive, and a stale
    // startup record would be read as this launch's progress even if the new
    // process never ran at all.
    let clear_stale_runtime = format!(
        "rm -f {} {} {}; ",
        targets::join_remote_command(&[format!(
            "{worker_root}/{}",
            mj_core::relay::WORKER_EXIT_FILE
        )]),
        targets::join_remote_command(&[format!("{worker_root}/control.sock")]),
        targets::join_remote_command(&[format!(
            "{worker_root}/{}",
            mj_core::relay::WORKER_STARTUP_FILE
        )]),
    );
    let detached_script = format!(
        "{clear_stale_runtime}nohup {} >{} 2>&1 </dev/null &",
        targets::join_remote_command(&[
            binary.clone(),
            "worker".into(),
            "run".into(),
            "--root".into(),
            worker_root.into(),
            "--config".into(),
            config.clone(),
        ]),
        targets::join_remote_command(&[format!("{worker_root}/worker.log")]),
    );
    // Redirect daemon output to worker.log in every launch mode; an
    // unexplained dead worker is undebuggable without it.
    let exec_script = format!(
        "{clear_stale_runtime}exec {} >{} 2>&1",
        targets::join_remote_command(&[
            binary.clone(),
            "worker".into(),
            "run".into(),
            "--root".into(),
            worker_root.into(),
            "--config".into(),
            config.clone(),
        ]),
        targets::join_remote_command(&[format!("{worker_root}/worker.log")]),
    );
    match locator {
        targets::TargetLocator::LocalBare { .. } => {
            // The worker this launches outlives the launch, so completion must
            // not signal the process group it inherited from this shell.
            let mut spec = CommandSpec::new("sh", ["-c", &detached_script]);
            spec.detaches = true;
            spec
        }
        targets::TargetLocator::LocalPodman { container_id, .. } => CommandSpec::new(
            "podman",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        targets::TargetLocator::LocalDocker { container_id, .. } => CommandSpec::new(
            "docker",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        targets::TargetLocator::AppleContainer { container_id, .. } => CommandSpec::new(
            "container",
            ["exec", "--detach", container_id, "sh", "-c", &exec_script],
        ),
        targets::TargetLocator::AwsEc2 { ssh, .. }
        | targets::TargetLocator::SshBare { ssh, .. } => {
            crate::targets::ssh_command(ssh, ["sh", "-c", &detached_script])
        }
        targets::TargetLocator::SshPodman {
            ssh, container_id, ..
        } => crate::targets::ssh_command(
            ssh,
            [
                "podman",
                "exec",
                "--detach",
                container_id,
                "sh",
                "-c",
                &exec_script,
            ],
        ),
        targets::TargetLocator::SshDocker {
            ssh, container_id, ..
        } => crate::targets::ssh_command(
            ssh,
            [
                "docker",
                "exec",
                "--detach",
                container_id,
                "sh",
                "-c",
                &exec_script,
            ],
        ),
    }
    .purpose("start detached Mjolnir worker")
    // Everything before this moves data into the target and reports as Sync.
    // Start begins here, with the daemon launch.
    .stage(ProvisionStage::Starting)
}

/// Enrich an opaque handshake failure by running the installed worker binary
/// directly in the target. This surfaces loader errors (for example a
/// glibc-linked worker inside an older-glibc container) that a detached start
/// swallows.
pub(in crate::controller) fn worker_probe_diagnosis(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let error = match worker_binary_probe_failure(executor, locator, worker_root) {
        Some(failure) => error.context(failure),
        None => error,
    };
    match probe_worker(executor, locator, worker_root) {
        Ok(probe) => error.context(probe.to_string()),
        Err(probe_error) => {
            error.context(format!("the worker could not be probed: {probe_error:#}"))
        }
    }
}

pub(super) fn worker_binary_probe_failure(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Option<String> {
    let binary = format!("{worker_root}/hel");
    let command = targets::locator_command(locator, vec![binary.clone(), "--version".into()])
        .purpose("probe installed worker binary");
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => None,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = if !stderr.trim().is_empty() {
                stderr.trim()
            } else if !stdout.trim().is_empty() {
                stdout.trim()
            } else {
                "the process exited unsuccessfully without output"
            };
            Some(format!(
                "worker binary {binary} fails to run in the target: {detail}; \
                 if this is a loader/glibc error, provide a musl worker \
                 (cargo build --release --target <arch>-unknown-linux-musl \
                  -p brokk-mj-worker --bin mj-worker, \
                 or set MJ_WORKER_BINARY/MJ_WORKER_DIR)"
            ))
        }
        Err(probe_error) => Some(format!("worker probe failed: {probe_error:#}")),
    }
}

/// What one probe of a starting or dead worker found.
///
/// The probe script prints exactly this document: the worker's own startup and
/// exit records, inserted unchanged, and the live processes for its root. All
/// three facts come from one command, because on a container or SSH target
/// every probe costs a round trip.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(in crate::controller) struct WorkerProbe {
    /// `worker-startup.json`, when the worker wrote one.
    pub startup: Option<WorkerStartupRecord>,
    /// `worker-exit.json`: the worker recorded its own death.
    pub exit: Option<WorkerExitRecord>,
    /// Live worker processes for this root, the recorded one first.
    pub pids: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(in crate::controller) struct WorkerStartupRecord {
    /// The latest step the worker reached.
    pub step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(in crate::controller) struct WorkerExitRecord {
    pub reason: String,
    /// A sentence the worker wrote for whoever asked, when it stopped on a
    /// precondition the caller can fix rather than on an internal failure.
    #[serde(default)]
    pub refusal: Option<String>,
}

impl WorkerProbe {
    pub fn alive(&self) -> bool {
        !self.pids.is_empty()
    }

    pub fn step(&self) -> Option<&str> {
        self.startup.as_ref().map(|record| record.step.as_str())
    }
}

/// One sentence naming what the worker did: its recorded exit reason, or
/// whether it is running and the step it last reached.
impl std::fmt::Display for WorkerProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(exit) = &self.exit {
            return write!(formatter, "the worker exited: {}", exit.reason);
        }
        match (self.alive(), self.step()) {
            (true, Some(step)) => write!(
                formatter,
                "the worker is running; its last startup step was {step:?}"
            ),
            (true, None) => write!(
                formatter,
                "the worker is running and recorded no startup step"
            ),
            (false, Some(step)) => write!(
                formatter,
                "the worker process is gone; it reached the startup step {step:?} \
                 and left no exit record"
            ),
            (false, None) => write!(
                formatter,
                "the worker process is gone and recorded no startup step"
            ),
        }
    }
}

/// Read the worker's startup record, exit record and live processes from the
/// target. The process state distinguishes a worker that died early from one
/// that is still running but never accepted a relay connection, so it must be
/// read before the caller stops the worker.
///
/// A target that cannot be asked, or a record that does not parse, is an
/// error: the caller must not mistake it for a worker with nothing to report.
pub(in crate::controller) fn probe_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Result<WorkerProbe> {
    let command = targets::locator_command(
        locator,
        vec!["sh".into(), "-c".into(), worker_probe_script(worker_root)],
    )
    .purpose("probe worker state");
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "the worker probe exited with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "read the worker probe: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        )
    })
}

/// The shell half of [`probe_worker`]. The records are JSON the worker wrote,
/// so they are inserted as they are; everything else it prints is a number.
fn worker_probe_script(worker_root: &str) -> String {
    format!(
        r#"{identity}
hel_record() {{
    if [ -s "$1" ]; then cat "$1"; else printf null; fi
}}
printf '{{"startup":'
hel_record "$hel_root/{startup_file}"
printf ',"exit":'
hel_record "$hel_root/{exit_file}"
printf ',"pids":['
if hel_pid=$(hel_recorded_worker); then
    printf '%s' "$hel_pid"
else
    hel_separator=
    while read -r hel_pid hel_args; do
        case "$hel_pid" in
            '' | *[!0-9]*) continue ;;
        esac
        [ "$hel_pid" -eq $$ ] && continue
        case "$hel_args" in
            *"$hel_match"*|*"$hel_match_home"*)
                printf '%s%s' "$hel_separator" "$hel_pid"
                hel_separator=,
                ;;
        esac
    done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
fi
printf ']}}\n'
"#,
        identity = targets::worker_daemon_identity_script(worker_root),
        startup_file = mj_core::relay::WORKER_STARTUP_FILE,
        exit_file = mj_core::relay::WORKER_EXIT_FILE,
    )
}

/// A failure on one line, for a session's error field: the first line of each
/// cause. A worker's exit reason can carry its bridge's stderr after the first
/// line; the caller logs the full chain.
pub(in crate::controller) fn failure_line(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| {
            cause
                .to_string()
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// R8-2 (cli/048): a resume whose worker exited stored the whole probe
    /// as the session's error, 96 lines long. The error keeps one line: the
    /// reason the worker recorded, then the rest of the chain.
    #[test]
    fn a_failure_carrying_a_worker_exit_is_one_line_naming_the_workers_reason() {
        let probe = WorkerProbe {
            startup: Some(WorkerStartupRecord {
                step: "acp-initialized".into(),
            }),
            exit: Some(WorkerExitRecord {
                reason: "select required ACP execution mode auto: Cannot set permission mode \
                         to auto: auto mode unavailable for this model\n\
                         ACP bridge stderr:\n[session/create] phase=register durationMs=1"
                    .into(),
                refusal: None,
            }),
            pids: vec![],
        };
        let error = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            .context("write relay history_requests request")
            .context(probe.to_string());

        assert_eq!(
            failure_line(&error),
            "the worker exited: select required ACP execution mode auto: Cannot set \
             permission mode to auto: auto mode unavailable for this model: write relay \
             history_requests request: broken pipe"
        );
    }
}
