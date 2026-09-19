//! Crash-boundary hooks and subprocess fixtures for isolated tests.
//!
//! The default build contains only the no-op function. Environment lookup,
//! marker files, and waiting are compiled in solely with `test-hooks`.

use anyhow::Result;

#[cfg(not(feature = "test-hooks"))]
#[inline]
pub fn reach_test_hook(_name: &'static str) -> Result<()> {
    Ok(())
}

#[cfg(feature = "test-hooks")]
pub fn reach_test_hook(name: &'static str) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, bail, ensure};

    if std::env::var_os("MJ_TEST_HOOK").as_deref() != Some(std::ffi::OsStr::new(name)) {
        return Ok(());
    }
    ensure!(
        std::env::var_os("MJ_CHAOS_ISOLATED").as_deref() == Some(std::ffi::OsStr::new("1")),
        "test hook {name} requires MJ_CHAOS_ISOLATED=1"
    );
    ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "invalid test hook name {name:?}"
    );
    let directory = PathBuf::from(
        std::env::var_os("MJ_TEST_HOOK_DIR")
            .context("active test hook requires MJ_TEST_HOOK_DIR")?,
    );
    ensure!(
        directory.is_dir(),
        "test hook directory {} does not exist",
        directory.display()
    );
    let reached = directory.join(format!("{name}.reached"));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&reached)
    {
        Ok(mut marker) => {
            marker
                .write_all(format!("pid={}\n", std::process::id()).as_bytes())
                .with_context(|| format!("write test hook marker {}", reached.display()))?;
            marker
                .sync_all()
                .with_context(|| format!("sync test hook marker {}", reached.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create test hook marker {}", reached.display()));
        }
    }

    let continuation = directory.join(format!("{name}.continue"));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !continuation.is_file() {
        if Instant::now() >= deadline {
            bail!(
                "test hook {name} timed out waiting for {}",
                continuation.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// The checked-in executable used by fake commands and worker fixtures.
#[cfg(all(unix, feature = "test-hooks"))]
pub fn fake_command_dispatcher() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-command.sh")
}

/// Install a shell stand-in, under `program`, for a command the code under
/// test looks up on `PATH` in `directory`.
///
/// No test may exec a file it has just written. `execve` answers `ETXTBSY`
/// ("Text file busy") while any process still holds that file open for
/// writing, and a test binary is multi-threaded: if another thread forks
/// between the write and the exec, its child inherits the still-open write
/// descriptor and keeps the file busy past the point where the writer closed
/// it. That is the race behind issue #1036. Writing under a temporary name and
/// renaming does not fix it, because a rename keeps the same inode.
///
/// So the name on `PATH` is a symlink to a dispatcher checked in at
/// `mj-core/tests/fixtures/fake-command.sh`, which this process never
/// opens for writing, and the behaviour goes in `<program>.script`, which only
/// `/bin/sh` ever reads. Nothing execs a written file at any point.
#[cfg(all(unix, feature = "test-hooks"))]
pub fn install_fake_command(directory: &std::path::Path, program: &str, script: &str) {
    let dispatcher = fake_command_dispatcher();
    assert!(
        dispatcher.is_file(),
        "fake command dispatcher is missing at {}",
        dispatcher.display()
    );
    std::fs::write(directory.join(format!("{program}.script")), script)
        .unwrap_or_else(|error| panic!("write the {program} stand-in: {error}"));
    let installed = directory.join(program);
    // A directory may host several fakes, and a test may replace one.
    match std::fs::remove_file(&installed) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("replace the {program} stand-in: {error}"),
    }
    std::os::unix::fs::symlink(&dispatcher, &installed)
        .unwrap_or_else(|error| panic!("link the {program} stand-in: {error}"));
}

#[cfg(all(test, feature = "test-hooks"))]
mod tests {
    use super::*;

    #[test]
    fn inactive_hook_does_not_require_isolation_environment() {
        reach_test_hook("not_selected").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fake_command_preserves_arguments_output_and_status_with_restricted_path() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("tools with spaces");
        std::fs::create_dir(&directory).unwrap();
        install_fake_command(
            &directory,
            "fake tool",
            "printf '<%s>\\n' \"$@\"; printf 'diagnostic\\n' >&2; exit 23\n",
        );
        let mut command = std::process::Command::new("fake tool");
        command
            .env_clear()
            .env("PATH", &directory)
            .args(["two words", "", "quote'and$dollar"]);
        let output = crate::subprocess::run_with_input(&mut command, &[]).unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(output.stdout, b"<two words>\n<>\n<quote'and$dollar>\n");
        assert_eq!(output.stderr, b"diagnostic\n");

        install_fake_command(&directory, "fake tool", "printf 'replacement\\n'\n");
        let output = crate::subprocess::run_with_input(&mut command, &[]).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"replacement\n");
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fake_command_streams_input_while_its_script_is_open_for_writing() {
        let directory = tempfile::tempdir().unwrap();
        install_fake_command(directory.path(), "echo-input", "exec /bin/cat\n");
        // An inherited writer can outlive installation. Reading the script
        // through an interpreter must still work while that writer is open.
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(directory.path().join("echo-input.script"))
            .unwrap();
        let input = vec![b'x'; 200 * 1024];
        let output = crate::subprocess::run_with_input(
            &mut std::process::Command::new(directory.path().join("echo-input")),
            &input,
        )
        .unwrap();
        drop(writer);
        assert!(output.status.success());
        assert_eq!(output.stdout, input);
        assert!(output.stderr.is_empty());
    }
}
