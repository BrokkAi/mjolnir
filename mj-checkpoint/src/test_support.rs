//! Fixtures shared by more than one test module in this crate.

use std::path::Path;

/// Run Git in `repository` and return its raw standard output.
pub(crate) fn git(repository: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Run Git in `repository` and return its trimmed standard output.
pub(crate) fn git_line(repository: &Path, arguments: &[&str]) -> String {
    String::from_utf8(git(repository, arguments))
        .unwrap()
        .trim()
        .to_owned()
}

/// Runs Git for real, but answers as Git 2.25.1 does to the newer options
/// checkpoints have used. SSH machines on Ubuntu 20.04 still run that Git.
pub(crate) struct Git225;

impl crate::archive::GitCommandRunner for Git225 {
    fn run(
        &self,
        repository: &Path,
        command: &crate::archive::GitCommand,
    ) -> anyhow::Result<crate::archive::GitOutput> {
        let has = |option: &str| command.arguments.iter().any(|argument| argument == option);
        if has("worktree") && has("-z") {
            return Ok(crate::archive::GitOutput {
                status: 129,
                stdout: Vec::new(),
                stderr: b"error: unknown switch `z'".to_vec(),
            });
        }
        if has("rev-parse") && has("--path-format=absolute") {
            // It prints the option back as a result, answers with paths
            // relative to where it ran, and exits successfully.
            let mut older = command.clone();
            older
                .arguments
                .retain(|argument| argument != "--path-format=absolute");
            let mut output = crate::archive::SystemGit.run(repository, &older)?;
            output.stdout = [b"--path-format=absolute\n".as_slice(), &output.stdout].concat();
            return Ok(output);
        }
        crate::archive::SystemGit.run(repository, command)
    }
}
