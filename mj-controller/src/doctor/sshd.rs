//! Effective host SSH limits; inspection never changes the host.
use super::*;

pub(super) fn checks(
    config: ConfigStatus<'_>,
    executor: &impl CommandExecutor,
) -> Vec<DoctorCheck> {
    let Ok(config) = config else {
        return Vec::new();
    };
    config
        .targets
        .iter()
        .filter_map(|(id, target)| {
            let ssh = match target {
                TargetTemplate::SshBare { ssh, .. }
                | TargetTemplate::SshPodman { ssh, .. }
                | TargetTemplate::SshDocker { ssh, .. }
                | TargetTemplate::SshAppleContainer { ssh, .. }
                | TargetTemplate::SshNitro { ssh, .. } => RuntimeSshTarget::from(ssh),
                _ => return None,
            };
            Some(check(id, &ssh, executor))
        })
        .collect()
}

pub(super) fn check(
    id: &str,
    ssh: &RuntimeSshTarget,
    executor: &impl CommandExecutor,
) -> DoctorCheck {
    let command = strict_ssh_validation_command(
        ssh,
        vec![
            "sh".into(),
            "-c".into(),
            // SSH supplies the addresses of this connection, including multiplexed channels.
            r#"set -eu; set -- $SSH_CONNECTION; sudo -n sshd -T -C "user=$(id -un),addr=$1,host=$1,laddr=$3,lport=$4""#.into(),
        ],
        "check effective SSH server limits",
    );
    let result = executor.execute(&command);
    let values = result
        .as_ref()
        .ok()
        .filter(|o| o.status == 0)
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            let value = |key: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(key))
                    .and_then(|s| s.trim().parse::<u32>().ok())
            };
            let sessions = value("maxsessions ")?;
            let startups = text
                .lines()
                .find_map(|line| line.strip_prefix("maxstartups "))?;
            let parts = startups
                .trim()
                .split(':')
                .map(str::parse::<u32>)
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            let limits = match parts.as_slice() {
                [limit] => (*limit, 0, *limit),
                [start, rate, full] => (*start, *rate, *full),
                _ => return None,
            };
            Some((
                sessions,
                startups.trim().to_owned(),
                limits,
                text.lines().any(|line| line.trim() == "usedns yes"),
            ))
        });
    let check_id = format!("sshd.{id}");
    let title = format!("SSH server limits for target {id}");
    let Some((sessions, startups, (start, rate, full), use_dns)) = values else {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "Effective SSH server limits on {} are unknown; passwordless `sudo -n sshd -T` could not read them.",
                ssh.destination
            ),
            r#"In an SSH connection to the host, run `set -- $SSH_CONNECTION; sudo sshd -T -C "user=$(id -un),addr=$1,host=$1,laddr=$3,lport=$4"` and check MaxSessions and MaxStartups; mj recommends 200 and 100:30:300 respectively."#,
        );
    };
    let mut detail = format!(
        "MaxSessions {sessions}, MaxStartups {startups}. Mj multiplexes worker relay sessions over shared SSH connections."
    );
    if use_dns {
        detail.push_str("\nHost-based Match rules were evaluated against the client address and may differ because UseDNS is enabled.");
    }
    let low_sessions = sessions < 200;
    let low_startups = start < 100 || rate > 30 || full < 300;
    if !low_sessions && !low_startups {
        DoctorCheck::ready(check_id, title, detail)
    } else {
        let mut content = String::from(
            "# mj multiplexes worker relay sessions over shared SSH connections.\n# Measured relay-session refusals at ~40 containers on morannon (Series 20/21).\n",
        );
        if low_sessions {
            content.push_str("MaxSessions 200\n");
        }
        if low_startups {
            content.push_str("MaxStartups 100:30:300\n");
        }
        DoctorCheck::warning(
            check_id,
            title,
            detail,
            format!(
                r#"On the host, put these lines in /etc/ssh/sshd_config.d/60-mj-mux.conf:
{content}This works only if sshd_config includes sshd_config.d/*.conf (most Linux distributions do). Otherwise add the settings to sshd_config itself. Run `sudo sshd -t` to validate before reloading. On Linux with systemd, reload with `sudo systemctl reload "$(systemctl show -p Id --value sshd.service)"`. On macOS, launchd starts sshd for each new connection, so reconnect to apply the settings. Reconnect existing SSH masters, then rerun `mj doctor` to verify the effective values."#,
            ),
        )
    }
}
