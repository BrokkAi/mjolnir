//! Fixtures shared by more than one test module in this crate.

#[cfg(unix)]
use std::path::Path;

/// Run Git in `repository` and return its trimmed standard output.
///
/// Every test that builds a repository needs this, so it lives here rather
/// than once per test module. Callers that only need the side effect discard
/// the returned text.
#[cfg(unix)]
pub(crate) fn git(repository: &Path, arguments: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
