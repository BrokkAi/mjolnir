//! Recognising a target that has run out of process slots, and saying so.
//!
//! A session container is created with a pids limit (see
//! `CONTAINER_PIDS_LIMIT`), and every thread counts against it. Past the limit
//! every `fork` and every thread spawn fails with `EAGAIN`, which reaches
//! Mjolnir as whatever the failing program printed: "os error 11",
//! "Resource temporarily unavailable", a shell's "Cannot fork", or Tokio's
//! "can't spawn worker thread". A sub-agent start that fails that way has not
//! failed for anything in its task, and its parent can fix it by closing
//! children it no longer needs (#1161), so the parent is told exactly that.

use std::time::Duration;

use anyhow::{Context, Result};

use super::{CommandExecutor, TargetLocator};

/// How long reading the container's pid counts may take. The read runs in a
/// container that may be full, where it can hang or fail; it only adds detail.
pub const PIDS_USAGE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// What the failing programs print when `fork` or a thread spawn returns
/// `EAGAIN`. Matched without regard to case. Rust prints the errno in
/// parentheses, and the closing one keeps `(os error 111)`, a refused
/// connection, from matching.
const PROCESS_EXHAUSTION_MARKERS: [&str; 5] = [
    "(os error 11)",
    "resource temporarily unavailable",
    "cannot fork",
    "can't fork",
    "can't spawn worker thread",
];

/// Whether an error's chain shows that its target could not create another
/// process or thread.
#[must_use]
pub fn shows_process_exhaustion(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let text = cause.to_string().to_ascii_lowercase();
        PROCESS_EXHAUSTION_MARKERS
            .iter()
            .any(|marker| text.contains(marker))
    })
}

/// The pid counts of a container's cgroup: `pids.current`, and `pids.max`,
/// which is `None` when the cgroup has no limit (the file says `max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidsUsage {
    pub current: u64,
    pub max: Option<u64>,
}

/// Prints `pids.current` and `pids.max` of the cgroup the command runs in,
/// under cgroup v2 or, failing that, v1.
const PIDS_USAGE_SCRIPT: &str = "cat /sys/fs/cgroup/pids.current /sys/fs/cgroup/pids.max 2>/dev/null \
     || cat /sys/fs/cgroup/pids/pids.current /sys/fs/cgroup/pids/pids.max";

/// Whether a target runs its sessions in a container, which is what has a
/// pids limit of its own.
#[must_use]
pub fn is_container(locator: &TargetLocator) -> bool {
    matches!(
        locator,
        TargetLocator::LocalPodman { .. }
            | TargetLocator::LocalDocker { .. }
            | TargetLocator::AppleContainer { .. }
            | TargetLocator::SshPodman { .. }
            | TargetLocator::SshDocker { .. }
    )
}

/// Read the pid counts of the container `locator` names, from inside it.
pub fn read_pids_usage(
    executor: &impl CommandExecutor,
    locator: &TargetLocator,
    session_id: &str,
) -> Result<PidsUsage> {
    let command = super::command_on_locator(
        locator,
        session_id,
        vec!["sh".into(), "-c".into(), PIDS_USAGE_SCRIPT.into()],
        "read the container's pid counts",
    )?;
    let output = executor.execute(&command)?;
    anyhow::ensure!(
        output.status == 0,
        "reading the pid counts failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_pids_usage(&String::from_utf8_lossy(&output.stdout))
}

fn parse_pids_usage(text: &str) -> Result<PidsUsage> {
    let mut lines = text.split_whitespace();
    let current = lines
        .next()
        .context("no pids.current value")?
        .parse()
        .context("pids.current is not a number")?;
    let max = match lines.next().context("no pids.max value")? {
        "max" => None,
        value => Some(value.parse().context("pids.max is not a number")?),
    };
    Ok(PidsUsage { current, max })
}

/// The message a parent model reads when a sub-agent could not be started
/// because its target ran out of process slots. `usage` is the result of
/// [`read_pids_usage`] for a container, or `None` for a target that is not
/// one. The original error follows, so nothing it said is lost.
#[must_use]
pub fn process_exhaustion_message(
    usage: Option<&Result<PidsUsage>>,
    error: &anyhow::Error,
) -> String {
    let (place, counts) = match usage {
        None => ("the target machine", String::new()),
        Some(Ok(PidsUsage { current, max })) => (
            "the parent's container",
            match max {
                Some(max) => format!(" (pids.current {current} of pids.max {max})"),
                None => format!(" (pids.current {current}, with no pids.max limit)"),
            },
        ),
        Some(Err(_)) => (
            "the parent's container",
            " (its pids.current and pids.max could not be read, which is what happens when \
             it is completely full)"
                .to_owned(),
        ),
    };
    format!(
        "{place} ran out of process slots{counts}, so the sub-agent's harness could not \
         start. Every live sub-agent holds hundreds of threads there. Close sub-agents you \
         no longer need, then try again. The original error was: {error:#}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_a_full_target_reports_itself_is_recognised() {
        for text in [
            "failed to spawn thread: Resource temporarily unavailable (os error 11)",
            "sh: 1: Cannot fork",
            "OS can't spawn worker thread: Resource temporarily unavailable (os error 11)",
        ] {
            let error = anyhow::anyhow!("{text}").context("start the sub-agent");
            assert!(shows_process_exhaustion(&error), "{text}");
        }
        for text in [
            "Connection refused (os error 111)",
            "Connection timed out (os error 110)",
            "permission denied (os error 13)",
        ] {
            let error = anyhow::anyhow!("{text}");
            assert!(!shows_process_exhaustion(&error), "{text}");
        }
    }

    #[test]
    fn the_message_names_the_counts_or_says_they_could_not_be_read() {
        let error = anyhow::anyhow!("sh: 1: Cannot fork");
        let read = process_exhaustion_message(
            Some(&Ok(PidsUsage {
                current: 8190,
                max: Some(8192),
            })),
            &error,
        );
        assert!(read.starts_with("the parent's container ran out of process slots"));
        assert!(
            read.contains("pids.current 8190 of pids.max 8192"),
            "{read}"
        );
        assert!(
            read.contains("Close sub-agents you no longer need"),
            "{read}"
        );
        assert!(
            read.ends_with("The original error was: sh: 1: Cannot fork"),
            "{read}"
        );

        let unread = process_exhaustion_message(Some(&Err(anyhow::anyhow!("Cannot fork"))), &error);
        assert!(
            unread.contains("its pids.current and pids.max could not be read"),
            "{unread}"
        );
        assert!(
            unread.contains("Close sub-agents you no longer need"),
            "{unread}"
        );
    }

    #[test]
    fn pid_counts_parse_with_and_without_a_limit() {
        assert_eq!(
            parse_pids_usage("1684\n8192\n").unwrap(),
            PidsUsage {
                current: 1684,
                max: Some(8192)
            }
        );
        assert_eq!(
            parse_pids_usage("12\nmax\n").unwrap(),
            PidsUsage {
                current: 12,
                max: None
            }
        );
        assert!(parse_pids_usage("").is_err());
    }
}
