use super::*;

/// POSIX shell helpers that identify the daemon for one exact worker root.
/// The match is assembled at run time so the script's own command line cannot
/// select itself, and `worker proxy` command lines cannot match either.
pub fn worker_daemon_identity_script(worker_root: &str) -> String {
    format!(
        r#"hel_root={root}
hel_match="hel worker run --root $hel_root"
hel_match_home="hel worker run --root $HOME/$hel_root"
hel_ps() {{
    ps -ww "$@" 2>/dev/null || ps "$@" 2>/dev/null
}}
hel_is_worker() {{
    hel_args=$(hel_ps -o args= -p "$1") || return 1
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) return 0 ;;
    esac
    return 1
}}
hel_recorded_worker() {{
    if [ -f "$hel_root/{pid_file}" ]; then
        hel_pid=$(cat "$hel_root/{pid_file}" 2>/dev/null)
        case "$hel_pid" in
            '' | *[!0-9]*) hel_pid="" ;;
        esac
        # The pidfile outlives a launch, so a recycled pid has to be ruled out
        # by what the process actually is.
        if [ -n "$hel_pid" ] && hel_is_worker "$hel_pid"; then
            printf '%s\n' "$hel_pid"
            return 0
        fi
    fi
    # A worker records its pid in its startup file from its first moment, long
    # before it writes a pidfile, and the launch clears that file, so a pid
    # found here can only be this launch's. Existence is therefore the whole
    # check: a worker does not have to be recognisable by its command line to
    # be this session's worker.
    [ -f "$hel_root/{startup_file}" ] || return 1
    hel_pid=$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$hel_root/{startup_file}" 2>/dev/null | head -n 1)
    case "$hel_pid" in
        '' | *[!0-9]*) return 1 ;;
    esac
    kill -0 "$hel_pid" 2>/dev/null || return 1
    printf '%s\n' "$hel_pid"
}}"#,
        root = posix_quote(worker_root),
        pid_file = mj_core::relay::WORKER_PID_FILE,
        startup_file = mj_core::relay::WORKER_STARTUP_FILE,
    )
}

/// Report whether the exact session worker is alive without signaling it.
/// A successful probe prints one stable token; transport or shell failures
/// stay distinguishable from a confirmed absent worker.
pub fn worker_daemon_liveness_script(worker_root: &str) -> String {
    let mut script = worker_daemon_identity_script(worker_root);
    script.push_str(
        r#"
hel_report_worker_state() {
    if [ -S "$hel_root/control.sock" ]; then
        printf 'alive\n'
    else
        printf 'starting\n'
    fi
}
if hel_recorded_worker >/dev/null; then
    hel_report_worker_state
    exit 0
fi
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_report_worker_state; exit 0 ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
printf 'dead\n'
"#,
    );
    script
}

/// Stop the detached worker daemon rooted at `worker_root`.
///
/// The daemon leads its own process group, so the signal goes to the group
/// first to take the agent down with it. Shells disagree about how to write a
/// negative PID (`dash` rejects `--`), hence the two forms before the
/// single-process fallback for daemons predating the group leadership.
pub fn stop_worker_daemon_script(worker_root: &str) -> String {
    let mut script = worker_daemon_identity_script(worker_root);
    script.push_str(
        r#"
hel_signal() {
    kill -"$1" -- "-$2" 2>/dev/null && return 0
    kill -"$1" "-$2" 2>/dev/null && return 0
    kill -"$1" "$2" 2>/dev/null
}
hel_stop() {
    hel_signal TERM "$1" || return 0
    hel_waited=0
    while [ "$hel_waited" -lt 2 ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
    kill -0 "$1" 2>/dev/null || return 0
    hel_signal KILL "$1" || true
    hel_waited=0
    while [ "$hel_waited" -lt 3 ]; do
        kill -0 "$1" 2>/dev/null || return 0
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
}
if hel_pid=$(hel_recorded_worker); then
    hel_stop "$hel_pid"
fi
hel_ps -eo pid=,args= | while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_stop "$hel_pid" ;;
    esac
done
hel_left=0
while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_left=1 ;;
    esac
done <<MJ_PS
$(hel_ps -eo pid=,args=)
MJ_PS
if [ "$hel_left" -ne 0 ]; then
    echo "worker still running after stop: $hel_root" >&2
    exit 1
fi
"#,
    );
    script
}

/// Stop a leaked worker and delete the durable relay state under its root.
///
/// A resume seeds fresh relay state into the same root a closed session used.
/// Leftover state wins over that seed at startup, so it has to go, and
/// whatever might still be writing it has to go first. Container and instance
/// targets are rebuilt from scratch on resume, so they need nothing here.
pub fn clear_relay_state_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<CommandSpec>> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let script = format!(
        "{}\nrm -rf -- {} {}\n",
        stop_worker_daemon_script(&session_worker_root),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_STATE_FILE
        )),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_JOURNAL_DIR
        )),
    );
    Ok(match locator {
        TargetLocator::LocalBare { .. } => Some(
            CommandSpec::new("sh", ["-c", script.as_str()])
                .purpose("stop a leaked local Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::SshBare { ssh, .. } => Some(
            ssh_command(ssh, ["sh", "-c", script.as_str()])
                .purpose("stop a leaked remote Mjolnir worker and clear its relay state"),
        ),
        TargetLocator::LocalPodman { .. }
        | TargetLocator::LocalDocker { .. }
        | TargetLocator::AppleContainer { .. }
        | TargetLocator::SshPodman { .. }
        | TargetLocator::SshDocker { .. }
        | TargetLocator::AwsEc2 { .. } => None,
    })
}

/// Everything an in-place harness replacement must remove before the new
/// profile is staged: the running daemon, relay state, the installed worker
/// files, and the previous per-session profile root. Runs inside the target on
/// every locator, unlike [`clear_relay_state_plan`], because the environment
/// survives the swap whether or not it is a container.
///
/// The worker files are unlinked rather than overwritten: a mapped `hel` cannot
/// be overwritten while the daemon holds it, and a stale `ownership.json` must
/// not survive a crash and let a later reader believe the old profile is still
/// installed. The worker root itself is recreated, so the caller can install
/// straight afterwards.
pub fn in_place_worker_reset_plan(
    locator: &TargetLocator,
    session_id: &str,
    previous_profile_root: Option<&str>,
) -> Result<CommandSpec> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let mut script = format!(
        "{}\nrm -rf -- {} {}\nrm -f -- {} {} {}\n",
        stop_worker_daemon_script(&session_worker_root),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_STATE_FILE
        )),
        posix_quote(&format!(
            "{session_worker_root}/{}",
            mj_core::relay::RELAY_JOURNAL_DIR
        )),
        posix_quote(&format!("{session_worker_root}/hel")),
        posix_quote(&format!("{session_worker_root}/launch.json")),
        posix_quote(&format!("{session_worker_root}/ownership.json")),
    );
    if let Some(root) = previous_profile_root {
        script.push_str(&format!("rm -rf -- {}\n", posix_quote(root)));
    }
    script.push_str(&format!(
        "mkdir -p -- {}\n",
        posix_quote(&session_worker_root)
    ));
    Ok(
        locator_command(locator, vec!["sh".into(), "-c".into(), script])
            .purpose("reset the worker root for an in-place harness replacement")
            .stage(ProvisionStage::Syncing),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_script(script: &str) -> (i32, String) {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run the probe script");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
    }

    /// A worker records its own pid in its startup file before it writes a
    /// pidfile and before its socket exists. The probe has to believe that
    /// record: a worker is not always recognisable by its command line, and
    /// one that is not was reported as gone while it was still running.
    #[test]
    fn a_worker_is_found_by_the_pid_it_recorded_even_when_its_command_line_differs() {
        let root = tempfile::tempdir().unwrap();
        // This test process is certainly alive, and its command line is a test
        // binary, which matches nothing the probe looks for.
        std::fs::write(
            root.path().join(mj_core::relay::WORKER_STARTUP_FILE),
            format!(
                "{{\n  \"step\": \"review-baseline\",\n  \"pid\": {},\n  \"steps\": []\n}}",
                std::process::id()
            ),
        )
        .unwrap();

        let script = format!(
            "{}\nhel_recorded_worker",
            worker_daemon_identity_script(&root.path().to_string_lossy())
        );
        let (status, stdout) = run_script(&script);

        assert_eq!(status, 0, "the probe must find the recorded worker");
        assert_eq!(stdout, std::process::id().to_string());
    }

    /// A startup record left by a worker that has since gone must not be read
    /// as a live worker.
    #[test]
    fn a_startup_record_for_a_dead_pid_reports_no_worker() {
        let root = tempfile::tempdir().unwrap();
        // A pid that cannot exist: the kernel rejects it outright.
        std::fs::write(
            root.path().join(mj_core::relay::WORKER_STARTUP_FILE),
            "{\n  \"step\": \"start\",\n  \"pid\": 2147483647,\n  \"steps\": []\n}",
        )
        .unwrap();

        let script = format!(
            "{}\nhel_recorded_worker",
            worker_daemon_identity_script(&root.path().to_string_lossy())
        );
        let (status, stdout) = run_script(&script);

        assert_ne!(status, 0, "a dead pid is not a worker: {stdout}");
        assert!(stdout.is_empty(), "{stdout}");
    }

    #[test]
    fn a_worker_root_with_no_records_reports_no_worker() {
        let root = tempfile::tempdir().unwrap();

        let script = format!(
            "{}\nhel_recorded_worker",
            worker_daemon_identity_script(&root.path().to_string_lossy())
        );
        let (status, stdout) = run_script(&script);

        assert_ne!(status, 0);
        assert!(stdout.is_empty(), "{stdout}");
    }
}
