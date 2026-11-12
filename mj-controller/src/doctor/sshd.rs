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
        vec!["sudo".into(), "-n".into(), "sshd".into(), "-T".into()],
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
            Some((sessions, startups.trim().to_owned(), limits))
        });
    let check_id = format!("sshd.{id}");
    let title = format!("SSH server limits for target {id}");
    let Some((sessions, startups, (start, rate, full))) = values else {
        return DoctorCheck::warning(
            check_id,
            title,
            format!(
                "Effective SSH server limits on {} are unknown; passwordless `sudo -n sshd -T` could not read them.",
                ssh.destination
            ),
            "On the host, run `sudo sshd -T` and check MaxSessions and MaxStartups; mj recommends 200 and 100:30:300 respectively.",
        );
    };
    let detail = format!(
        "MaxSessions {sessions}, MaxStartups {startups}. Mj multiplexes worker relay sessions over shared SSH connections."
    );
    if sessions >= 200 && start >= 100 && rate <= 30 && full >= 300 {
        DoctorCheck::ready(check_id, title, detail)
    } else {
        DoctorCheck::warning(
            check_id,
            title,
            detail,
            "On the host, run: sudo -n mkdir -p /etc/ssh/sshd_config.d && printf '%s\\n' '# Measured relay-session refusals at ~40 containers on morannon (Series 20/21).' 'MaxSessions 200' 'MaxStartups 100:30:300' | sudo -n tee /etc/ssh/sshd_config.d/60-mj-mux.conf && sudo -n sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config && sudo -n sshd -t && sudo -n systemctl reload \"$(systemctl show -p Id --value sshd.service)\". Then run `sudo -n sshd -T` to verify the effective values; earlier drop-ins may take precedence. Reconnect existing SSH masters to apply the new limits.",
        )
    }
}
