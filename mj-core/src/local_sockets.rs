//! Unix domain socket binding and connecting that tolerates long paths.
//!
//! The kernel copies the socket path into `sockaddr_un.sun_path`, which holds
//! 104 bytes on macOS and 108 on Linux. Worker sockets live under
//! `<data_dir>/workers/<32-hex session id>/`, which routinely exceeds the macOS
//! limit even though every individual component is short.
//!
//! The limit applies to the string handed to the kernel, not to the resolved
//! location, so a bare file name always fits. When a path is too long we
//! switch the process working directory to the socket's parent, bind or
//! connect the bare file name, and switch back. A process-wide mutex
//! serializes those switches and a drop guard restores the previous directory
//! on every exit path, so the switch is invisible to the rest of the process.
//! Callers must hold no other relative-path assumptions for the duration of
//! the call.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;

#[cfg(unix)]
static CWD_SWITCH: Mutex<()> = Mutex::new(());

/// Number of path bytes the platform accepts in a Unix socket address.
///
/// This is `sun_path`'s length minus the terminating NUL.
#[cfg(unix)]
pub fn unix_socket_path_limit() -> usize {
    // SAFETY: `sockaddr_un` is a plain C struct with no invalid bit patterns.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_path.len() - 1
}

/// Bind a Unix listener at `path`, working around `sun_path` length limits.
#[cfg(unix)]
pub fn bind_unix_listener(path: &Path) -> Result<UnixListener> {
    with_short_socket_name(path, |short| UnixListener::bind(short))
        .with_context(|| format!("bind unix socket {}", path.display()))
}

/// Connect to a Unix socket at `path`, working around `sun_path` length limits.
#[cfg(unix)]
pub fn connect_unix_stream(path: &Path) -> Result<UnixStream> {
    with_short_socket_name(path, |short| UnixStream::connect(short))
        .with_context(|| format!("connect unix socket {}", path.display()))
}

/// Run `act` on a socket address short enough for `sun_path`.
///
/// Uses `path` directly when it already fits; otherwise runs `act` on the bare
/// file name with the process working directory moved to `path`'s parent.
#[cfg(unix)]
fn with_short_socket_name<T>(
    path: &Path,
    act: impl FnOnce(&Path) -> std::io::Result<T>,
) -> Result<T> {
    if path.as_os_str().as_bytes().len() <= unix_socket_path_limit() {
        return Ok(act(path)?);
    }

    let parent = path
        .parent()
        .with_context(|| format!("socket path has no parent directory: {}", path.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("socket path has no file name: {}", path.display()))?;

    let _serialized = CWD_SWITCH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _restore = WorkingDirectoryGuard::switch_to(parent)?;
    Ok(act(Path::new(name))?)
}

/// Restores the previous working directory when dropped.
#[cfg(unix)]
struct WorkingDirectoryGuard {
    previous: PathBuf,
}

#[cfg(unix)]
impl WorkingDirectoryGuard {
    fn switch_to(directory: &Path) -> Result<Self> {
        let previous = std::env::current_dir().context("read current working directory")?;
        std::env::set_current_dir(directory)
            .with_context(|| format!("enter socket directory {}", directory.display()))?;
        Ok(Self { previous })
    }
}

#[cfg(unix)]
impl Drop for WorkingDirectoryGuard {
    fn drop(&mut self) {
        if let Err(error) = std::env::set_current_dir(&self.previous) {
            tracing::error!(
                directory = %self.previous.display(),
                %error,
                "failed to restore working directory after binding a unix socket"
            );
        }
    }
}

#[cfg(not(unix))]
pub fn unix_socket_path_limit() -> usize {
    0
}

#[cfg(not(unix))]
pub fn bind_unix_listener(_path: &Path) -> Result<std::convert::Infallible> {
    anyhow::bail!("Unix domain sockets require Unix")
}

#[cfg(not(unix))]
pub fn connect_unix_stream(_path: &Path) -> Result<std::convert::Infallible> {
    anyhow::bail!("Unix domain sockets require Unix")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Nested directories whose joined length is guaranteed to exceed
    /// `sun_path` on every supported platform.
    fn deep_directory(base: &Path) -> PathBuf {
        let mut directory = base.to_path_buf();
        while directory.as_os_str().len() < 200 {
            directory.push("nested-directory-component");
        }
        std::fs::create_dir_all(&directory).expect("create nested directories");
        directory
    }

    #[test]
    fn bind_and_connect_work_through_a_path_longer_than_sun_path() {
        // Changing cwd affects every thread, including concurrent subprocess
        // launches. Exercise the long-path fallback in its own process.
        const CHILD: &str = "MJ_LONG_SOCKET_PATH_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "local_sockets::tests::bind_and_connect_work_through_a_path_longer_than_sun_path",
                    "--nocapture",
                ])
                .env(CHILD, "1");
            let output = crate::subprocess::run_with_input(&mut command, &[]).unwrap();
            assert!(
                output.status.success(),
                "long socket path test failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }
        let temporary = tempfile::tempdir().expect("tempdir");
        let socket = deep_directory(temporary.path()).join("control.sock");
        assert!(socket.as_os_str().len() > unix_socket_path_limit());

        let before = std::env::current_dir().expect("cwd");
        let listener = bind_unix_listener(&socket).expect("bind long path");
        assert_eq!(std::env::current_dir().expect("cwd"), before);

        let accepting = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).expect("read");
            byte[0]
        });

        let mut client = connect_unix_stream(&socket).expect("connect long path");
        assert_eq!(std::env::current_dir().expect("cwd"), before);
        client.write_all(&[7]).expect("write");
        drop(client);

        assert_eq!(accepting.join().expect("join"), 7);
    }

    #[test]
    fn a_short_path_binds_without_switching_directory() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let socket = temporary.path().join("short.sock");

        let before = std::env::current_dir().expect("cwd");
        let listener = bind_unix_listener(&socket).expect("bind short path");
        assert_eq!(std::env::current_dir().expect("cwd"), before);
        drop(listener);
        assert!(socket.exists());
    }

    #[test]
    fn binding_into_a_missing_directory_names_the_full_path() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let socket = temporary.path().join("absent").join("control.sock");

        let error = bind_unix_listener(&socket).expect_err("missing directory");
        assert!(
            format!("{error:#}").contains(&socket.display().to_string()),
            "error should name the full path: {error:#}"
        );
    }
}
