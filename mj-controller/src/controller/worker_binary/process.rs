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
            CommandSpec::new("sh", ["-c", &detached_script])
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
    match worker_last_words(executor, locator, worker_root) {
        Some(last_words) => error.context(last_words),
        None => error,
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
/// The three facts are read from the same command, because on a container or
/// SSH target every probe costs a round trip: whether the process is there,
/// which startup step it last recorded, and the diagnostic text a failure
/// should carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::controller) struct WorkerProbe {
    /// A process for this worker root is running on the target.
    pub alive: bool,
    /// The latest step from `worker-startup.json`, when the worker wrote one.
    pub step: Option<String>,
    /// The worker recorded its own death.
    pub exited: bool,
    /// Exit record, log tail and process state, for an error to carry.
    pub diagnostics: String,
}

/// Fetch the worker's startup record, structured exit record, log tail, and
/// current process state from the target, so unreachable-worker errors carry
/// the root cause. The process section distinguishes a worker that died early
/// from one that is still running but never accepted a relay connection; it
/// must be read before the caller stops the worker.
pub(in crate::controller) fn probe_worker(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Option<WorkerProbe> {
    let text = worker_last_words(executor, locator, worker_root)?;
    Some(WorkerProbe {
        alive: process_section(&text).is_some_and(|section| section.starts_with("alive")),
        step: startup_step(&text),
        exited: text.contains(WORKER_EXIT_RECORD_MARKER),
        diagnostics: text,
    })
}

/// The text under the process marker, which is `alive (...)` or `absent`.
fn process_section(text: &str) -> Option<&str> {
    text.split_once(WORKER_PROCESS_MARKER)
        .map(|(_, rest)| rest.trim_start())
}

/// The latest step name from the startup record embedded in a probe.
///
/// The record is pretty-printed JSON, so it is bounded by the next section
/// marker rather than by counting braces.
fn startup_step(text: &str) -> Option<String> {
    let (_, rest) = text.split_once(WORKER_STARTUP_RECORD_MARKER)?;
    let body = rest.split("\n--- ").next().unwrap_or(rest);
    let record: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    record
        .get("step")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

pub(in crate::controller) fn worker_last_words(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    worker_root: &str,
) -> Option<String> {
    let script = format!(
        r#"{identity}
if [ -f {root}/{startup_file} ]; then echo '{startup_marker}'; cat {root}/{startup_file}; fi
if [ -f {root}/worker-exit.json ]; then echo '{marker}'; cat {root}/worker-exit.json; fi
if [ -f {root}/worker.log ]; then echo '--- worker.log (tail) ---'; tail -n 20 {root}/worker.log; fi
echo '{process_marker}'
if hel_pid=$(hel_recorded_worker); then
    echo "alive (recorded pid $hel_pid)"
    hel_ps -o pid=,ppid=,stat=,etime=,args= -p "$hel_pid"
    exit 0
fi
hel_found=0
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*)
            hel_found=1
            echo "alive (unrecorded pid $hel_pid)"
            hel_ps -o pid=,ppid=,stat=,etime=,args= -p "$hel_pid"
            ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
[ "$hel_found" -eq 1 ] || echo 'absent'
"#,
        identity = targets::worker_daemon_identity_script(worker_root),
        root = targets::posix_quote(worker_root),
        startup_file = mj_core::relay::WORKER_STARTUP_FILE,
        startup_marker = WORKER_STARTUP_RECORD_MARKER,
        process_marker = WORKER_PROCESS_MARKER,
        marker = WORKER_EXIT_RECORD_MARKER
    );
    let command = targets::locator_command(locator, vec!["sh".into(), "-c".into(), script])
        .purpose("collect worker last words");
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(
                worker_root,
                %error,
                "could not collect worker diagnostics"
            );
            return None;
        }
    };
    if output.status != 0 {
        tracing::debug!(
            worker_root,
            status = output.status,
            "worker diagnostic probe returned a failure"
        );
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then(|| format!("worker diagnostics:\n{text}"))
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// The probe's output is one block of text from the target. Reading the
    /// step and the process state out of it is what lets the readiness wait
    /// tell a worker that is still working from one that has died, so both
    /// have to survive the pretty-printed JSON and the sections around it.
    #[test]
    fn a_probe_reads_the_latest_step_and_whether_the_worker_is_alive() {
        let text = format!(
            "worker diagnostics:\n{WORKER_STARTUP_RECORD_MARKER}\n\
             {{\n  \"step\": \"review-baseline\",\n  \"pid\": 41,\n  \
             \"steps\": [\n    {{ \"step\": \"start\" }},\n    \
             {{ \"step\": \"review-baseline\" }}\n  ]\n}}\n\
             --- worker.log (tail) ---\n\n{WORKER_PROCESS_MARKER}\n\
             alive (recorded pid 41)\n41 1 Sl 00:12 hel worker run"
        );

        assert_eq!(startup_step(&text).as_deref(), Some("review-baseline"));
        assert!(process_section(&text).is_some_and(|section| section.starts_with("alive")));
    }

    #[test]
    fn a_worker_that_left_no_startup_record_reports_no_step() {
        let text = format!("worker diagnostics:\n{WORKER_PROCESS_MARKER}\nabsent");

        assert_eq!(startup_step(&text), None);
        assert!(process_section(&text).is_some_and(|section| section.starts_with("absent")));
    }
}
