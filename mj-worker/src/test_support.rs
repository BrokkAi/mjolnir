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

/// tracing-core caches each callsite's interest process-wide. While at most
/// one dispatcher is registered it asks only the current thread's default, so
/// a parallel test reaching a callsite first without a subscriber caches
/// it as `never`, and a scoped log capture silently loses the line.
/// A global subscriber that answers `sometimes` keeps every callsite asking
/// the scoped subscriber per event.
#[cfg(unix)]
struct DeferToScoped;

#[cfg(unix)]
impl tracing::Subscriber for DeferToScoped {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        false
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[cfg(unix)]
pub(crate) fn scoped_log_capture_sees_every_callsite() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        tracing::subscriber::set_global_default(DeferToScoped)
            .expect("no other global subscriber in worker unit tests");
    });
}
