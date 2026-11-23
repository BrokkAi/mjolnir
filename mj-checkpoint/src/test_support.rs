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

/// Record a one-file edit as a fixture tree and restore the clean fixture index.
pub(crate) fn fixture_tree_with_edit(repository: &Path, path: &str) -> String {
    git(repository, &["add", "--", path]);
    let tree = git_line(repository, &["write-tree"]);
    git(repository, &["read-tree", "HEAD"]);
    tree
}

/// Git 2.31 does not support NUL-delimited worktree lists.
pub(crate) struct Git231;

impl crate::archive::GitCommandRunner for Git231 {
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
        crate::archive::SystemGit.run(repository, command)
    }
}
