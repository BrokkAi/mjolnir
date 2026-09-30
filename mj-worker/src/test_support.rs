//! Fixtures shared by more than one test module in this crate.

#[cfg(unix)]
pub(crate) const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A hang detector for fixtures that perform real filesystem, process, or
/// socket I/O. Tests of application deadlines use their own controlled clock.
#[cfg(unix)]
pub(crate) async fn wait_for<T>(
    description: &str,
    future: impl std::future::Future<Output = T>,
) -> T {
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
}

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
