//! Running a command and turning a failure into the error its output describes.

use anyhow::{Result, bail};
use mj_core::targets::{CommandExecutor, CommandOutput, CommandSpec};

use crate::targets::step_script_error;

pub fn execute_checked(
    executor: &impl CommandExecutor,
    command: CommandSpec,
) -> Result<CommandOutput> {
    let output = executor.execute(&command)?;
    check_output(&command, output)
}

/// `output` of `command` when it succeeded, otherwise the error its stderr
/// describes.
pub fn check_output(command: &CommandSpec, output: CommandOutput) -> Result<CommandOutput> {
    if let Some(error) = step_script_error(&output) {
        bail!("{error}");
    }
    if output.status != 0 {
        let detail = command_error_detail(&output.stderr);
        if detail.is_empty() {
            bail!("{} failed with status {}", command.purpose, output.status);
        }
        bail!("{detail}");
    }
    Ok(output)
}

pub fn command_error_detail(stderr: &[u8]) -> String {
    let reported = String::from_utf8_lossy(stderr);
    let reported = reported.trim();
    let detail = reported
        .rsplit_once("\nCaused by:\n")
        .map_or(reported, |(_, causes)| causes);
    let detail = detail.strip_prefix("Error: ").unwrap_or(detail);
    detail
        .lines()
        .map(|line| line.strip_prefix("    ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Whether the digest `command` prints for an installed file equals
/// `expected`. A command that fails, or prints nothing, does not match.
pub fn installed_digest_matches(
    executor: &impl CommandExecutor,
    command: &CommandSpec,
    expected: &str,
) -> bool {
    executor.execute(command).as_ref().is_ok_and(|output| {
        output.status == 0
            && String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .is_some_and(|digest| digest.eq_ignore_ascii_case(expected))
    })
}

/// Whether a command failed because its program is not installed.
pub fn is_missing_command(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}
