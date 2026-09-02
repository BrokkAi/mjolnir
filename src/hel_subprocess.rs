//! Shared helper for running a child process that needs both piped stdin and
//! captured stdout/stderr.
//!
//! `Command::spawn()` followed by `write_all(stdin)` and then
//! `wait_with_output()` deadlocks once the child writes enough to stdout or
//! stderr to fill the OS pipe buffer (commonly 64KB) before it has consumed
//! all of stdin: the child blocks writing its output, so it stops reading
//! stdin, so the parent's `write_all` blocks on the full stdin pipe, and
//! neither side can make progress. `run_with_input` avoids this by writing
//! stdin from a dedicated thread while the caller's thread drains stdout and
//! stderr concurrently via `wait_with_output`.
//!
//! This module is the one sanctioned caller of `wait_with_output`; every
//! other call site should go through [`run_with_input`] instead (enforced by
//! the workspace's `disallowed-methods` clippy lint).
#![allow(
    clippy::disallowed_methods,
    reason = "this module exists to wrap wait_with_output safely"
)]

use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};

use anyhow::{Context, Result, anyhow};

/// Launch a long-lived background process with no inherited terminal streams.
///
/// The process is genuinely detached: on Unix it is a grandchild reparented to
/// init, not a child of the caller. A double fork is the right shape here
/// rather than a background reaper thread, because the one caller
/// (`connect_or_start` in `mj-cli`) starts a daemon that outlives the spawner,
/// discards the returned PID, and confirms readiness over IPC instead. A reaper
/// thread would make the spawner hold a `Child` for the whole life of a process
/// it explicitly wanted to detach from, and would pin that process's
/// process-table entry under a long-lived spawner until it exits; init reaps a
/// grandchild straight away.
///
/// The returned PID is the real process rather than the intermediate, so
/// callers can still signal or probe it, and that process still leads its own
/// process group, so group termination works exactly as before.
pub fn spawn_detached(command: &mut Command, log_path: &Path) -> Result<u32> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create detached process log directory {}", parent.display())
        })?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options
        .open(log_path)
        .with_context(|| format!("open detached process log {}", log_path.display()))?;
    let stderr = log.try_clone().context("clone detached process log")?;
    command.stdin(Stdio::null()).stdout(log).stderr(stderr);

    #[cfg(unix)]
    {
        spawn_detached_unix(command)
    }
    #[cfg(not(unix))]
    {
        // Windows has no zombie state: dropping the handle releases it while
        // the process keeps running.
        let child = command.spawn().context("spawn detached child process")?;
        Ok(child.id())
    }
}

/// Double-fork `command` and report the grandchild's PID.
///
/// The intermediate is an ordinary `Command` child that forks once more inside
/// `pre_exec`: the fork's parent (this process's direct child) reports the
/// grandchild's PID down a pipe and `_exit`s at once, while the fork's child
/// takes a session of its own and goes on to exec the real program. The
/// intermediate is waited for here, which returns immediately, so this process
/// has nothing left to reap and the grandchild is reparented to init.
///
/// Forking inside `pre_exec` rather than forking this process directly keeps
/// the fork out of a multi-threaded address space: only `fork`, `setsid`,
/// `write` and `_exit` run between the fork and the exec, and all of those are
/// async-signal-safe.
#[cfg(unix)]
fn spawn_detached_unix(command: &mut Command) -> Result<u32> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let (mut reader, writer) = std::io::pipe().context("create detached spawn pid pipe")?;
    let report_fd = writer.as_raw_fd();

    // The intermediate leads a group of its own, so it never signals the
    // caller's group; the grandchild then takes a session, and with it a group,
    // of its own, which is the group callers terminate by the returned PID.
    command.process_group(0);

    // SAFETY: the closure runs between fork and exec in the child. It calls
    // only async-signal-safe functions and touches no state shared with
    // another thread.
    unsafe {
        // `report_fd` is still open in the child: fork does not honour
        // FD_CLOEXEC, only exec does.
        command.pre_exec(move || match libc::fork() {
            -1 => Err(std::io::Error::last_os_error()),
            0 => {
                // Grandchild: lead a new session, and so a new process group,
                // before carrying on to exec.
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
            grandchild => {
                // Intermediate: report the real PID and leave at once. A short
                // or failed write surfaces as a read failure in the parent, so
                // no error is swallowed here.
                let pid = (grandchild as u32).to_ne_bytes();
                let mut written = 0;
                while written < pid.len() {
                    let count = libc::write(
                        report_fd,
                        pid.as_ptr().add(written).cast(),
                        pid.len() - written,
                    );
                    if count <= 0 {
                        break;
                    }
                    written += count as usize;
                }
                libc::_exit(0)
            }
        });
    }

    let spawned = command.spawn().context("spawn detached child process");
    // Close this process's write end, so the read below sees EOF instead of
    // blocking if the intermediate died without reporting a PID.
    drop(writer);
    let mut intermediate = spawned?;

    let mut pid_bytes = [0_u8; 4];
    let reported = reader.read_exact(&mut pid_bytes);
    let status = intermediate
        .wait()
        .context("wait for detached spawn intermediate")?;
    reported.context("read detached child pid from the spawn intermediate")?;
    if !status.success() {
        return Err(anyhow!(
            "detached spawn intermediate exited with {status}, so the child may not have started"
        ));
    }

    Ok(u32::from_ne_bytes(pid_bytes))
}

/// Run `command` with `input` written to its stdin, returning the captured
/// output.
///
/// Sets `command`'s stdin/stdout/stderr to piped, spawns it, writes `input`
/// to stdin from a separate thread (closing stdin when the write finishes or
/// fails), and drains stdout/stderr on the caller's thread via
/// `wait_with_output`. The writer thread is joined before returning, so a
/// write failure is never silently discarded -- except a broken pipe, which
/// just means the child exited (or closed stdin) before consuming all of
/// `input`; in that case the child's real exit status and stderr are more
/// useful to the caller than a generic I/O error, so the output is still
/// returned.
pub fn run_with_input(command: &mut Command, input: &[u8]) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn child process")?;

    let mut stdin = child.stdin.take().context("child stdin is missing")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let result = stdin.write_all(&input);
        // Close stdin whether or not the write succeeded, so a child that is
        // blocked reading stdin (e.g. waiting for EOF) can proceed.
        drop(stdin);
        result
    });

    let output = child.wait_with_output().context("wait for child process")?;

    match writer.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {
            // The child exited, or otherwise stopped reading, before we
            // finished writing. Report the child's actual status/stderr
            // instead of this expected write failure.
        }
        Ok(Err(error)) => return Err(error).context("write child process stdin"),
        Err(panic) => {
            return Err(anyhow!(
                "child process stdin writer thread panicked: {panic:?}"
            ));
        }
    }

    Ok(output)
}

/// Run a foreground child that talks to the terminal but whose stdout the
/// caller needs to read.
///
/// An interactive login prints its prompts and progress on stderr and its one
/// machine-readable answer on stdout. Inheriting stdin and stderr keeps the
/// prompts and any typed reply on the real terminal, while stdout is captured.
/// Only one pipe exists and this thread drains it, so there is no second
/// stream to deadlock against. Long-running callers must invoke this helper
/// from their supervised blocking-work facility.
pub fn run_capturing_stdout(command: &mut Command) -> Result<Output> {
    let child = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn child process")?;
    child.wait_with_output().context("wait for child process")
}

/// Run a foreground child with no stdin and with its output inherited.
///
/// With no pipe to feed or drain, waiting synchronously cannot hit the pipe
/// deadlock that [`run_with_input`] prevents. Long-running callers must invoke
/// this helper from their supervised blocking-work facility.
pub fn run_inherited(command: &mut Command) -> Result<ExitStatus> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("run child process")
}

/// Send `signal` to the process group led by `pid`.
///
/// Every caller signals a group it created for its own child, and wants that
/// group gone. `ESRCH` means it already is, so it reports success rather than
/// a failure. Darwin excludes zombies while counting signalable members of a
/// group and returns `EPERM` once that count reaches zero, which for a group
/// we own likewise means only exiting descendants remain. Any other error is
/// a real teardown failure and is returned so the caller can report it.
#[cfg(unix)]
pub fn signal_process_group(pid: i32, signal: i32) -> std::io::Result<()> {
    // SAFETY: the negated pid targets only the process group this process
    // created for its own child.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if group_signal_error_is_ignorable(&error) {
        return Ok(());
    }
    Err(error)
}

#[cfg(unix)]
fn group_signal_error_is_ignorable(error: &std::io::Error) -> bool {
    if error.raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(libc::EPERM) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn run_with_input_completes_when_child_echoes_input_larger_than_pipe_buffer() {
        // `cat` echoes stdin to stdout; feeding it well past the typical 64KB
        // pipe buffer reproduces the old deadlock (parent blocked in
        // write_all while the child blocks writing stdout that nobody is
        // draining yet) unless stdin is fed concurrently with output drain.
        let input = vec![b'x'; 512 * 1024];
        let mut command = Command::new("sh");
        command.arg("-c").arg("cat");

        let output = run_with_input(&mut command, &input)
            .expect("run_with_input should not deadlock or fail");

        assert!(output.status.success());
        assert_eq!(output.stdout, input);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_input_reports_child_status_when_child_exits_before_reading_all_input() {
        // The child exits immediately without reading stdin, so the writer
        // thread hits a broken pipe partway through writing. That must not
        // surface as a generic write error; the caller should still see the
        // child's real exit status.
        let input = vec![b'x'; 512 * 1024];
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 3");

        let output = run_with_input(&mut command, &input)
            .expect("a broken pipe from an early exit must not be a hard error");

        assert_eq!(output.status.code(), Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn run_capturing_stdout_collects_more_than_one_pipe_buffer() {
        // The child writes well past the 64KB pipe buffer, so a helper that
        // waited before draining would deadlock here.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("dd if=/dev/zero bs=1024 count=512 2>/dev/null | tr '\\0' 'x'");

        let output = run_capturing_stdout(&mut command).expect("capture a large stdout");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 512 * 1024);
    }

    #[test]
    fn run_with_input_returns_output_for_empty_input() {
        let mut command = Command::new("true");
        let output = run_with_input(&mut command, &[]).expect("run_with_input should succeed");
        assert!(output.status.success());
    }

    #[cfg(unix)]
    #[test]
    fn spawn_detached_leaves_no_zombie_under_a_spawner_that_keeps_running() {
        // This test process outlives the child, which is exactly the case that
        // used to leave a zombie: nothing reaped it, so its process-table entry
        // survived and every existence probe still called it alive.
        use std::time::{Duration, Instant};

        let log_dir = tempfile::tempdir().expect("create log directory");
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        let pid = spawn_detached(&mut command, &log_dir.path().join("child.log"))
            .expect("spawn_detached should start the child");

        // The reported PID is the real process, not the intermediate, and it is
        // not a child of this process, so init reaps it.
        let raw_pid = libc::pid_t::try_from(pid).expect("pid fits pid_t");
        let mut status = 0;
        // SAFETY: `status` is writable and WNOHANG never blocks; waiting on a
        // non-child fails with ECHILD without changing any process state.
        let waited = unsafe { libc::waitpid(raw_pid, &mut status, libc::WNOHANG) };
        assert_eq!(waited, -1, "the detached child must not be our own child");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match detached_test_process_state(pid) {
                None => break,
                Some(state) => assert_ne!(
                    state, 'Z',
                    "process {pid} is still a zombie of this process"
                ),
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} never left the process table"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The state letter from `/proc/<pid>/stat`, or `None` once the entry is
    /// gone. Other Unixes have no `/proc` in this shape, so there the poll ends
    /// as soon as `kill(pid, 0)` stops finding the process.
    #[cfg(unix)]
    fn detached_test_process_state(pid: u32) -> Option<char> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // The comm field can contain spaces and parentheses, so the state
            // letter is the first non-space character after the last ')'.
            let after_comm = stat.rsplit_once(')')?.1;
            after_comm.split_whitespace().next()?.chars().next()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let raw_pid = libc::pid_t::try_from(pid).ok()?;
            // SAFETY: signal 0 is an existence probe that sends no signal.
            if unsafe { libc::kill(raw_pid, 0) } == 0 {
                Some('?')
            } else {
                None
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_detached_reports_the_real_child_and_leaves_it_leading_its_own_group() {
        // Callers signal the returned PID's process group to tear the child
        // down, so the PID has to be the exec'd program rather than the
        // short-lived intermediate, and it has to lead that group.
        let log_dir = tempfile::tempdir().expect("create log directory");
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        let pid = spawn_detached(&mut command, &log_dir.path().join("child.log"))
            .expect("spawn_detached should start the child");

        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .expect("the reported pid must name a live process");
        assert_eq!(comm.trim(), "sh");

        let raw_pid = libc::pid_t::try_from(pid).expect("pid fits pid_t");
        // SAFETY: getpgid only reads the group of an existing process.
        let group = unsafe { libc::getpgid(raw_pid) };
        assert_eq!(group, raw_pid, "the child must lead its own process group");

        signal_process_group(raw_pid, libc::SIGKILL).expect("terminate the detached child group");
    }

    #[cfg(unix)]
    #[test]
    fn signalling_a_group_that_is_already_gone_succeeds() {
        // Cancelling a command whose child already exited is the common case;
        // it must not look like a teardown failure.
        use std::os::unix::process::CommandExt as _;

        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        command.process_group(0);
        let mut child = command.spawn().expect("spawn short-lived child");
        let pid = child.id() as i32;
        child.wait().expect("reap short-lived child");

        signal_process_group(pid, libc::SIGKILL)
            .expect("signalling an already-exited process group must succeed");
    }

    #[cfg(unix)]
    #[test]
    fn signalling_a_live_group_reports_a_real_failure() {
        // An invalid signal number is a caller bug, not a group that already
        // exited, so it must surface instead of being swallowed.
        use std::os::unix::process::CommandExt as _;

        let mut command = Command::new("sleep");
        command.arg("30");
        command.process_group(0);
        let mut child = command.spawn().expect("spawn long-lived child");
        let pid = child.id() as i32;

        let error =
            signal_process_group(pid, 1234).expect_err("an invalid signal number must be reported");
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));

        signal_process_group(pid, libc::SIGKILL).expect("terminate the test child");
        child.wait().expect("reap long-lived child");
    }

    #[cfg(unix)]
    #[test]
    fn group_signal_error_only_ignores_a_gone_owned_group() {
        let missing = std::io::Error::from_raw_os_error(libc::ESRCH);
        assert!(group_signal_error_is_ignorable(&missing));

        let invalid = std::io::Error::from_raw_os_error(libc::EINVAL);
        assert!(!group_signal_error_is_ignorable(&invalid));

        let denied = std::io::Error::from_raw_os_error(libc::EPERM);
        #[cfg(target_os = "macos")]
        assert!(group_signal_error_is_ignorable(&denied));
        #[cfg(not(target_os = "macos"))]
        assert!(!group_signal_error_is_ignorable(&denied));
    }
}
