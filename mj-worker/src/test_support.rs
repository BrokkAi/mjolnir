//! Fixtures shared by more than one test module in this crate.

mod timeouts;
pub(crate) use timeouts::IO_TIMEOUT;

/// A hang detector for fixtures that perform real filesystem, process, or
/// socket I/O. Tests of application deadlines use their own controlled clock.
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

/// A real file read that cannot finish until this writer is dropped. Keeping
/// the gate in the test also releases the read if an assertion panics.
#[cfg(unix)]
pub(crate) fn stalled_read_file(path: &Path) -> std::fs::File {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a terminated path, and mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

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
