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
#[cfg(all(unix, any(test, feature = "test-hooks")))]
pub fn fake_command_dispatcher() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-command.sh")
}

/// A worker stand-in carrying the same build identity as this test process.
/// Built before tests start, so executing it cannot race a test's writer.
#[cfg(all(unix, any(test, feature = "test-hooks")))]
pub fn fake_worker_dispatcher() -> std::path::PathBuf {
    std::path::Path::new(env!("OUT_DIR")).join("fake-worker.sh")
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
#[cfg(all(unix, any(test, feature = "test-hooks")))]
pub fn install_fake_command(directory: &std::path::Path, program: &str, script: &str) {
    install_fake_with_dispatcher(fake_command_dispatcher(), directory, program, script);
}

/// Install a worker stand-in, like [`install_fake_command`], that carries this
/// build's worker stamp so the controller accepts it as a matching worker.
#[cfg(all(unix, any(test, feature = "test-hooks")))]
pub fn install_fake_worker(directory: &std::path::Path, program: &str, script: &str) {
    install_fake_with_dispatcher(fake_worker_dispatcher(), directory, program, script);
}

#[cfg(all(unix, any(test, feature = "test-hooks")))]
fn install_fake_with_dispatcher(
    dispatcher: std::path::PathBuf,
    directory: &std::path::Path,
    program: &str,
    script: &str,
) {
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

/// Environment shared by the controller and daemon tests that run a stand-in
/// relay: the parent test sets it on the command that runs the relay, and
/// `mj_worker::stand_in_relay::serve_latch_relay` reads it. The relay runs as
/// [`latch_relay::CHILD_TEST`] in a second process of the same test binary.
#[cfg(feature = "test-hooks")]
pub mod latch_relay {
    /// The `--exact` test that serves the relay; every test binary that starts
    /// one defines it.
    pub const CHILD_TEST: &str = "stand_in_relay::latch_relay_child_serves_stdio";
    pub const LATCH_RELAY_ROOT: &str = "MJ_TEST_LATCH_RELAY_ROOT";
    pub const LATCH_RELAY_MAX_PROTOCOL: &str = "MJ_TEST_LATCH_RELAY_MAX_PROTOCOL";
    pub const LATCH_REJECT_PROJECT_MEMORY: &str = "MJ_TEST_LATCH_REJECT_PROJECT_MEMORY";
    pub const LATCH_RECORD_SUBAGENT_ADMISSION: &str = "MJ_TEST_LATCH_RECORD_SUBAGENT_ADMISSION";
    pub const LATCH_DROP_ADMISSION_CLOSE_REPLY: &str = "MJ_TEST_LATCH_DROP_ADMISSION_CLOSE_REPLY";
    pub const LATCH_RELAY_STARTS: &str = "MJ_TEST_LATCH_RELAY_STARTS";
    pub const LATCH_RELAY_REJECT_RELEASE: &str = "MJ_TEST_LATCH_REJECT_RELEASE";
    pub const LATCH_RELAY_RUNNING: &str = "MJ_TEST_LATCH_RELAY_RUNNING";
    pub const LATCH_CHECKPOINT_ONLY: &str = "MJ_TEST_LATCH_CHECKPOINT_ONLY";
    /// Open a new native session with this id instead of reloading one, as a
    /// restored worker does when the harness has no record of the session it was
    /// asked to reload.
    pub const LATCH_RELAY_FRESH_NATIVE: &str = "MJ_TEST_LATCH_FRESH_NATIVE";
    /// With [`LATCH_RELAY_FRESH_NATIVE`]: the native session the new one replaced
    /// because this session never used it. Unset, the relay gives no reason.
    pub const LATCH_RELAY_REPLACED_UNUSED: &str = "MJ_TEST_LATCH_REPLACED_UNUSED";
    /// Answer every dispatched prompt with one agent message, like a harness.
    pub const LATCH_RELAY_ANSWER_PROMPTS: &str = "MJ_TEST_LATCH_ANSWER_PROMPTS";
    /// The text of the answer [`LATCH_RELAY_ANSWER_PROMPTS`] gives.
    pub const LATCH_RELAY_ANSWER: &str = "answered by the stand-in harness";
    pub const LATCH_RELAY_STARTUP_DELAY_MS: &str = "MJ_TEST_LATCH_STARTUP_DELAY_MS";
    /// Have the worker write something of its own just before it reads a Close,
    /// and complete the Close once the barrier it seals has ended, the way a live
    /// worker does. The value names the write: [`LATE_HARNESS_NOTIFICATION`] or
    /// [`LATE_HARNESS_QUESTION`].
    pub const LATCH_RELAY_JOURNALS_BEFORE_CLOSE: &str = "MJ_TEST_LATCH_JOURNALS_BEFORE_CLOSE";
    /// A harness notification that changes no work, which waits behind the cut.
    pub const LATE_HARNESS_NOTIFICATION: &str = "notification";
    /// Hold the first request that submits this relay command (`begin_checkpoint`
    /// or `close`) until the parent test lets it go: the worker creates
    /// `<LATCH_RELAY_HOLD_PATH>.reached` and waits for
    /// `<LATCH_RELAY_HOLD_PATH>.release`. A test uses it to act at an exact point
    /// of a close.
    pub const LATCH_RELAY_HOLD_COMMAND: &str = "MJ_TEST_LATCH_HOLD_COMMAND";
    pub const LATCH_RELAY_HOLD_PATH: &str = "MJ_TEST_LATCH_HOLD_PATH";
    /// A question for a person, which cannot wait and spoils the cut.
    pub const LATE_HARNESS_QUESTION: &str = "question";
    pub const LATCH_RELAY_SESSION: &str = "018f9dd2-a3b4-7c8d-9000-0123456789ab";
}

/// Environment of the stand-in relay that the leased-submission tests run;
/// `mj_worker::stand_in_relay::serve_leased_relay` reads it.
#[cfg(feature = "test-hooks")]
pub mod leased_relay {
    /// The `--exact` test that serves the relay; every test binary that starts
    /// one defines it.
    pub const CHILD_TEST: &str = "stand_in_relay::leased_relay_child_serves_stdio";
    pub const LEASED_RELAY_ROOT: &str = "MJ_TEST_LEASED_RELAY_ROOT";
    pub const LEASED_RELAY_SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";
    /// File in the relay root where the stdio relay child logs each request.
    pub const RELAY_REQUEST_LOG: &str = "requests.log";
    /// Makes the stdio relay child report an open harness session.
    pub const RELAY_SESSION_CONFIGURED: &str = "MJ_TEST_RELAY_SESSION_CONFIGURED";
    pub const CLEAR_MAILBOX_CONTROL: &str = "MJ_TEST_CLEAR_MAILBOX_CONTROL";
    pub const DROP_ADMISSION_OPEN_REPLY: &str = "MJ_TEST_DROP_ADMISSION_OPEN_REPLY";
}

#[cfg(all(test, unix, feature = "test-hooks"))]
mod tests {
    use super::*;

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
