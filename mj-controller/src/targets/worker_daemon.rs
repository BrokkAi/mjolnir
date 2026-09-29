use super::*;

/// POSIX shell helpers that identify the daemon for one exact worker root.
/// The match is assembled at run time so the script's own command line cannot
/// select itself, and `worker proxy` command lines cannot match either.
pub fn worker_daemon_identity_script(worker_root: &str) -> String {
    format!(
        r#"{process_birth_script}
hel_root={root}
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
    # Startup records survive process death. Match the process birth as well
    # as its PID before treating a breadcrumb as authority to signal it.
    [ -f "$hel_root/{startup_file}" ] || return 1
    hel_pid=$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$hel_root/{startup_file}" 2>/dev/null | head -n 1)
    case "$hel_pid" in
        '' | *[!0-9]*) return 1 ;;
    esac
    hel_birth=$(sed -n 's/.*"process_birth"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$hel_root/{startup_file}" 2>/dev/null | head -n 1)
    if [ -n "$hel_birth" ]; then
        hel_current_birth=$(mj_process_birth "$hel_pid") || return 1
        [ "$hel_birth" = "$hel_current_birth" ] || return 1
    else
        # Older workers have no birth record; only their exact command line
        # can identify them. PID existence alone cannot identify a worker.
        hel_is_worker "$hel_pid" || return 1
    fi
    printf '%s\n' "$hel_pid"
}}"#,
        root = posix_quote(worker_root),
        process_birth_script = mj_core::subprocess::PROCESS_BIRTH_SCRIPT,
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
    if [ "$hel_group" = "$2" ]; then
        kill -"$1" -- "-$2" 2>/dev/null && return 0
        kill -"$1" "-$2" 2>/dev/null
        return $?
    fi
    kill -"$1" "$2" 2>/dev/null
}
hel_group_running() {
    if [ "$hel_group" = "$1" ]; then
        hel_processes=$(hel_ps -eo pgid=,stat=) || return 2
        printf '%s\n' "$hel_processes" | awk -v group="$1" '$1 == group && $2 !~ /^Z/ {found=1} END {exit !found}'
    else
        hel_process_state=$(hel_ps -o stat= -p "$1") || return 1
        printf '%s\n' "$hel_process_state" | awk '$1 !~ /^Z/ && NF {found=1} END {exit !found}'
    fi
}
# The harness and its helpers lead process groups of their own, so the leader's
# group can empty while they still write into the worker root. `hel_tree` is the
# snapshot, taken before any signal, of every process descended from the leader
# or in its group, as "pid pgid" lines; the stop is finished only when none is
# left.
hel_snapshot_tree() {
    hel_tree=$(hel_ps -eo pid=,ppid=,pgid= | awk -v lead="$1" -v self="$$" '
        {pid[NR]=$1; ppid[NR]=$2; pg[NR]=$3; n=NR}
        END {
            for (i = 1; i <= n; i++) if (pid[i] == lead || pg[i] == lead) member[pid[i]] = 1
            do {
                grew = 0
                for (i = 1; i <= n; i++) if (!(pid[i] in member) && (ppid[i] in member)) { member[pid[i]] = 1; grew = 1 }
            } while (grew)
            for (i = 1; i <= n; i++) if ((pid[i] in member) && pid[i] != self) print pid[i], pg[i]
        }') || hel_tree=""
}
hel_tree_running() {
    [ -n "$hel_tree" ] || return 1
    hel_processes=$(hel_ps -eo pid=,stat=) || return 2
    hel_tree_pids=" $(printf '%s\n' "$hel_tree" | while read -r hel_tpid hel_tgroup; do printf '%s ' "$hel_tpid"; done)"
    while read -r hel_ppid hel_pstat; do
        [ -n "$hel_ppid" ] || continue
        case "$hel_pstat" in Z*) continue ;; esac
        case "$hel_tree_pids" in *" $hel_ppid "*) return 0 ;; esac
    done <<MJ_TREE
$hel_processes
MJ_TREE
    return 1
}
hel_running() {
    hel_group_running "$1"
    hel_run_status=$?
    [ "$hel_run_status" -eq 1 ] || return "$hel_run_status"
    hel_tree_running
}
hel_kill_tree() {
    printf '%s\n' "$hel_tree" | while read -r hel_tpid hel_tgroup; do
        [ -n "$hel_tpid" ] || continue
        kill -KILL "$hel_tpid" 2>/dev/null || true
        case "$hel_tgroup" in
            '' | 0 | 1 | *[!0-9]*) ;;
            *) kill -KILL -- "-$hel_tgroup" 2>/dev/null || kill -KILL "-$hel_tgroup" 2>/dev/null || true ;;
        esac
    done
}
hel_stop() {
    hel_group=$1
    hel_tree=""
    hel_running "$1" || {
        hel_status=$?
        [ "$hel_status" -eq 1 ] || return "$hel_status"
        hel_group=legacy
    }
    hel_snapshot_tree "$1"
    hel_signal TERM "$1" || true
    hel_waited=0
    while [ "$hel_waited" -lt 2 ]; do
        hel_running "$1" || { hel_status=$?; [ "$hel_status" -eq 1 ] && return 0; return "$hel_status"; }
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
    hel_signal KILL "$1" || true
    hel_kill_tree
    hel_waited=0
    while [ "$hel_waited" -lt 3 ]; do
        hel_running "$1" || { hel_status=$?; [ "$hel_status" -eq 1 ] && return 0; return "$hel_status"; }
        sleep 1
        hel_waited=$((hel_waited + 1))
    done
    echo "worker process tree still running after stop: $1" >&2
    return 1
}
if hel_pid=$(hel_recorded_worker); then
    hel_stop "$hel_pid" || exit $?
fi
hel_ps -eo pid=,args= | while read -r hel_pid hel_args; do
    case "$hel_pid" in
        '' | *[!0-9]*) continue ;;
    esac
    [ "$hel_pid" -eq $$ ] && continue
    case "$hel_args" in
        *"$hel_match"*|*"$hel_match_home"*) hel_stop "$hel_pid" || exit $? ;;
    esac
done || exit $?
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
///
/// A staged home that is a link to a profile home, left for a session an
/// earlier release started there (`controller::local_profile_homes`), is
/// unlinked too. The restore that follows writes the native session into the
/// staged home while the worker files are installed beside it, and it must
/// land in a directory of the session's own, not in the profile home.
pub fn clear_relay_state_plan(
    locator: &TargetLocator,
    session_id: &str,
) -> Result<Option<CommandSpec>> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    let staged_home = posix_quote(&format!("{session_worker_root}/profile"));
    let script = format!(
        "{}\nrm -rf -- {} {}\nif [ -L {staged_home} ]; then rm -f -- {staged_home}; fi\n",
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
    previous_profile_root: &str,
) -> Result<CommandSpec> {
    verify_locator(locator, session_id)?;
    let session_worker_root = worker_root(locator, session_id)?;
    // A root that is a link left for a session an earlier release started
    // (`controller::local_profile_homes`) is unlinked, never followed: the path
    // carries no trailing slash, so `rm` removes the link itself.
    let script = format!(
        "{}\nrm -rf -- {} {}\nrm -f -- {} {} {}\nrm -rf -- {}\nmkdir -p -- {}\n",
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
        posix_quote(previous_profile_root.trim_end_matches('/')),
        posix_quote(&session_worker_root),
    );
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
            serde_json::to_vec(&serde_json::json!({
                "step": "review-baseline",
                "pid": std::process::id(),
                "process_birth": mj_core::subprocess::process_birth_identity(std::process::id()).unwrap(),
                "steps": [],
            })).unwrap(),
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

    #[test]
    fn a_recycled_startup_pid_never_identifies_or_stops_an_unrelated_process() {
        let root = tempfile::tempdir().unwrap();
        for birth in [
            serde_json::Value::Null,
            serde_json::json!("previous-process-birth"),
        ] {
            std::fs::write(
                root.path().join(mj_core::relay::WORKER_STARTUP_FILE),
                serde_json::to_vec(
                    &serde_json::json!({"pid": std::process::id(), "process_birth": birth}),
                )
                .unwrap(),
            )
            .unwrap();
            let script = format!(
                "{}\nhel_recorded_worker",
                worker_daemon_identity_script(&root.path().to_string_lossy())
            );
            let (status, stdout) = run_script(&script);
            assert_ne!(status, 0);
            assert!(stdout.is_empty());
            let (status, _) =
                run_script(&stop_worker_daemon_script(&root.path().to_string_lossy()));
            assert_eq!(status, 0);
        }
    }

    #[test]
    #[cfg(unix)]
    fn stopping_worker_waits_for_term_resistant_descendants_after_leader_exit() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sh -c 'trap \"\" TERM; echo ready > ready; while :; do sleep 1; done' & echo $! > descendant; wait"])
            .current_dir(root.path()).process_group(0)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .spawn().unwrap();
        let _group = mj_core::subprocess::ProcessGroupGuard::new(Some(child.id()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.path().join("ready").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(root.path().join(mj_core::relay::WORKER_STARTUP_FILE),
            serde_json::to_vec(&serde_json::json!({"pid": child.id(), "process_birth": mj_core::subprocess::process_birth_identity(child.id()).unwrap()})).unwrap()).unwrap();
        let (status, _) = run_script(&stop_worker_daemon_script(&root.path().to_string_lossy()));
        assert_eq!(status, 0);
        assert!(child.wait().is_ok());
        let descendant = std::fs::read_to_string(root.path().join("descendant")).unwrap();
        let (_, state) = run_script(&format!("ps -o stat= -p {}", descendant.trim()));
        assert!(
            state.is_empty() || state.starts_with('Z'),
            "descendant survived: {state}"
        );
    }

    /// The harness runs in a process group of its own, so the worker leader's
    /// group empties as soon as the leader exits. The stop must still wait for
    /// the harness, or the `rm` that follows races its last writes (I2-9).
    #[test]
    #[cfg(target_os = "linux")]
    fn stopping_worker_waits_for_a_harness_in_its_own_process_group() {
        use std::os::unix::process::CommandExt;
        let outside = tempfile::tempdir().unwrap();
        let root = outside.path().join("worker");
        std::fs::create_dir_all(root.join("profile")).unwrap();
        // The harness ignores TERM and keeps writing into the staged home.
        let harness = format!(
            "trap \"\" TERM; echo $$ > {out}/harness; while :; do echo x >> profile/state.sqlite-wal; sleep 0.05; done",
            out = outside.path().display()
        );
        let mut leader = std::process::Command::new("sh")
            .args(["-c", &format!("setsid sh -c '{harness}' & wait")])
            .current_dir(&root)
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !outside.path().join("harness").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::fs::write(
            root.join(mj_core::relay::WORKER_STARTUP_FILE),
            serde_json::to_vec(&serde_json::json!({"pid": leader.id(), "process_birth": mj_core::subprocess::process_birth_identity(leader.id()).unwrap()})).unwrap(),
        )
        .unwrap();
        let root_path = root.to_string_lossy().into_owned();
        let script = format!(
            "{}\nrm -rf -- \"$hel_root\"\n",
            stop_worker_daemon_script(&root_path)
        );
        let (status, _) = run_script(&script);
        let _ = leader.wait();
        assert_eq!(status, 0, "the stop and the removal succeed");
        assert!(!root.exists(), "the removal is complete");
        let harness = std::fs::read_to_string(outside.path().join("harness")).unwrap();
        let (_, state) = run_script(&format!("ps -o stat= -p {}", harness.trim()));
        assert!(
            state.is_empty() || state.starts_with('Z'),
            "harness survived: {state}"
        );
    }

    /// The script runs through the target's own shell and awk. macOS awk
    /// rejects a newline inside a `-v` value, and `/bin/sh` may be dash,
    /// busybox or bash, so the script must be plain POSIX sh with no
    /// multi-line `-v` assignment.
    #[test]
    fn stop_script_is_posix_sh_and_passes_no_multiline_value_to_awk() {
        let script = stop_worker_daemon_script("/tmp/mj-worker-root");
        let mut assignments = 0;
        for (i, _) in script.match_indices(" -v ") {
            assignments += 1;
            let assignment = script[i + 4..].split_whitespace().next().unwrap();
            let (_, value) = assignment.split_once('=').expect("-v name=value");
            assert!(
                matches!(value, "\"$1\"" | "\"$$\""),
                "awk -v value may span lines: {assignment}"
            );
            assert!(!assignment.contains('\n'));
        }
        assert!(assignments > 0, "the check found no awk -v assignments");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("stop.sh");
        std::fs::write(&file, &script).unwrap();
        for shell in ["sh", "dash", "busybox"] {
            let mut cmd = std::process::Command::new(shell);
            if shell == "busybox" {
                cmd.arg("sh");
            }
            match cmd.arg("-n").arg(&file).output() {
                Ok(out) => assert!(
                    out.status.success(),
                    "{shell} -n: {}",
                    String::from_utf8_lossy(&out.stderr)
                ),
                Err(_) if shell != "sh" => {}
                Err(e) => panic!("{shell}: {e}"),
            }
        }
    }
}
