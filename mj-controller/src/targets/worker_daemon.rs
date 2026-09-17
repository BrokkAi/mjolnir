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
    [ -f "$hel_root/{pid_file}" ] || return 1
    hel_pid=$(cat "$hel_root/{pid_file}" 2>/dev/null)
    case "$hel_pid" in
        '' | *[!0-9]*) return 1 ;;
    esac
    hel_is_worker "$hel_pid" || return 1
    printf '%s\n' "$hel_pid"
}}"#,
        root = posix_quote(worker_root),
        pid_file = mj_core::relay::WORKER_PID_FILE,
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
