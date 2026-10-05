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

/// Own a child-created process group through all of its I/O and cleanup.
/// The group can outlive its leader, so reaping the child does not disarm it.
pub struct ProcessGroupGuard {
    pid: Option<i32>,
}

impl ProcessGroupGuard {
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid: pid
                .and_then(|pid| i32::try_from(pid).ok())
                .filter(|pid| *pid > 1),
        }
    }

    pub fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            terminate_process_group(pid, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = self.pid;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A process birth identity, shared with remote teardown's POSIX shell.
/// Linux uses boot identity and kernel start ticks; other Unix hosts use ps's
/// process start timestamp. A PID by itself is never a durable identity.
pub const PROCESS_BIRTH_SCRIPT: &str = r#"mj_process_birth() {
    if [ -r "/proc/$1/stat" ] && [ -r /proc/sys/kernel/random/boot_id ]; then
        mj_stat=$(cat "/proc/$1/stat") || return 1
        mj_stat=${mj_stat##*) }
        mj_ticks=$(printf '%s\n' "$mj_stat" | awk '{print $20}') || return 1
        [ -n "$mj_ticks" ] || return 1
        mj_boot=$(cat /proc/sys/kernel/random/boot_id) || return 1
        printf '%s:%s\n' "$mj_boot" "$mj_ticks"
    else
        LC_ALL=C ps -o lstart= -p "$1" | awk '{$1=$1; print}'
    fi
}
"#;

pub fn process_birth_identity(pid: u32) -> Result<String> {
    let output = run_with_input(
        Command::new("sh").args([
            "-c",
            &format!("{PROCESS_BIRTH_SCRIPT}\nmj_process_birth \"$1\""),
            "mj-process-birth",
            &pid.to_string(),
        ]),
        &[],
    )?;
    let identity = String::from_utf8(output.stdout)?.trim().to_owned();
    anyhow::ensure!(
        output.status.success() && !identity.is_empty(),
        "process birth identity is unavailable for {pid}"
    );
    Ok(identity)
}

/// How long a bounded command's output may keep arriving after its leader
/// exits. A backgrounded descendant that inherited the pipes would otherwise
/// hold a finished command until the whole-command timeout. The same bound as
/// Codex's `IO_DRAIN_TIMEOUT_MS`. The owned process group is terminated after.
const IO_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Capture a process with byte/time bounds, draining both pipes concurrently.
/// Kill its process group before returning on overflow, timeout, or cancellation.
pub async fn run_bounded(
    command: &mut tokio::process::Command,
    max_bytes: usize,
    timeout: std::time::Duration,
) -> Result<Output> {
    use tokio::io::AsyncReadExt;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start bounded subprocess")?;
    let group = ProcessGroupGuard::new(child.id());
    // Bytes go to buffers owned here, so output read while draining survives
    // the reader being dropped at the drain deadline.
    async fn read(
        mut pipe: impl tokio::io::AsyncRead + Unpin,
        bytes: &mut Vec<u8>,
        max: usize,
    ) -> Result<()> {
        let mut chunk = [0_u8; 8192];
        loop {
            let count = pipe.read(&mut chunk).await?;
            if count == 0 {
                return Ok(());
            }
            bytes.extend_from_slice(&chunk[..count]);
            anyhow::ensure!(bytes.len() <= max, "subprocess output exceeds {max} bytes");
        }
    }
    let stdout = child.stdout.take().context("missing subprocess stdout")?;
    let stderr = child.stderr.take().context("missing subprocess stderr")?;
    let (mut stdout_bytes, mut stderr_bytes) = (Vec::new(), Vec::new());
    let result = tokio::time::timeout(timeout, async {
        // Owning the pipes here closes our ends when the readers are dropped.
        let mut readers = std::pin::pin!(async {
            tokio::try_join!(
                read(stdout, &mut stdout_bytes, max_bytes),
                read(stderr, &mut stderr_bytes, max_bytes)
            )
            .map(|_| ())
        });
        let status = tokio::select! {
            done = &mut readers => {
                done?;
                child.wait().await?
            }
            status = child.wait() => {
                // The command completes when its leader exits. A descendant
                // that keeps the pipes open only gets the drain bound.
                let status = status?;
                if let Ok(done) = tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut readers).await {
                    done?;
                }
                status
            }
        };
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    let result = result.map(|status| {
        status.map(|status| Output {
            status,
            stdout: std::mem::take(&mut stdout_bytes),
            stderr: std::mem::take(&mut stderr_bytes),
        })
    });
    match result {
        Ok(Ok(output)) => {
            // Completed pipes do not authorize a background descendant to
            // keep running after this bounded command releases ownership.
            drop(group);
            Ok(output)
        }
        outcome => {
            drop(group);
            if let Err(error) = child.start_kill() {
                tracing::debug!(%error, "bounded subprocess already exited during termination");
            }
            // The operation deadline stops execution, not cleanup ownership.
            // Callers may remove working files as soon as this returns.
            child
                .wait()
                .await
                .context("reap terminated bounded subprocess")?;
            match outcome {
                Ok(Err(error)) => Err(error),
                _ => anyhow::bail!("subprocess timed out"),
            }
        }
    }
}

/// Capture a process with byte and time bounds while writing its complete
/// stdin concurrently. This preserves path arguments on the platform's native
/// `Command` API and avoids filling either output pipe while a child is still
/// consuming a large input.
pub async fn run_bounded_with_input(
    command: &mut tokio::process::Command,
    input: &[u8],
    max_bytes: usize,
    timeout: std::time::Duration,
) -> Result<Output> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start bounded subprocess")?;
    let group = ProcessGroupGuard::new(child.id());

    async fn read(
        mut pipe: impl tokio::io::AsyncRead + Unpin,
        bytes: &mut Vec<u8>,
        max: usize,
    ) -> Result<()> {
        let mut chunk = [0_u8; 8192];
        loop {
            let count = pipe.read(&mut chunk).await?;
            if count == 0 {
                return Ok(());
            }
            bytes.extend_from_slice(&chunk[..count]);
            anyhow::ensure!(bytes.len() <= max, "subprocess output exceeds {max} bytes");
        }
    }
    async fn write_input(mut pipe: impl tokio::io::AsyncWrite + Unpin, input: &[u8]) -> Result<()> {
        match pipe.write_all(input).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    let stdin = child.stdin.take().context("missing subprocess stdin")?;
    let stdout = child.stdout.take().context("missing subprocess stdout")?;
    let stderr = child.stderr.take().context("missing subprocess stderr")?;
    let (mut stdout_bytes, mut stderr_bytes) = (Vec::new(), Vec::new());
    let result = tokio::time::timeout(timeout, async {
        let (status, (), ()) = tokio::try_join!(
            async { child.wait().await.map_err(anyhow::Error::from) },
            write_input(stdin, input),
            async {
                tokio::try_join!(
                    read(stdout, &mut stdout_bytes, max_bytes),
                    read(stderr, &mut stderr_bytes, max_bytes)
                )
                .map(|_| ())
            }
        )?;
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    match result {
        Ok(Ok(status)) => {
            drop(group);
            Ok(Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            })
        }
        outcome => {
            drop(group);
            if let Err(error) = child.start_kill() {
                tracing::debug!(%error, "bounded subprocess already exited during termination");
            }
            child
                .wait()
                .await
                .context("reap terminated bounded subprocess")?;
            match outcome {
                Ok(Err(error)) => Err(error),
                _ => anyhow::bail!("subprocess timed out"),
            }
        }
    }
}

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
/// `fcntl` (or `close_range`), `write` and `_exit` run between the fork and
/// the exec, and all of those are async-signal-safe.
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

    // Computed before the fork: the loop fallback in the child must not
    // allocate or call anything that is not async-signal-safe.
    let descriptor_limit = inherited_descriptor_limit();

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
                // The long-lived process keeps nothing of its launcher beyond
                // the stdio set up above. A descriptor the launcher holds
                // without close-on-exec -- on macOS, a sibling thread's pipe
                // caught between `pipe()` and its `FD_CLOEXEC` call -- would
                // otherwise stay open for the daemon's whole life, and whoever
                // reads that pipe would never see EOF. Marking rather than
                // closing keeps the standard library's exec-error pipe working.
                mark_inherited_descriptors_cloexec(descriptor_limit);
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

/// One past the highest descriptor this process can hold, for the per-fd
/// loop in [`mark_inherited_descriptors_cloexec`].
#[cfg(unix)]
fn inherited_descriptor_limit() -> libc::c_int {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable rlimit.
    let current = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0
        && limit.rlim_cur != libc::RLIM_INFINITY
    {
        limit.rlim_cur as i64
    } else {
        // SAFETY: sysconf only reads a system constant.
        (unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }) as i64
    };
    current.clamp(3, i64::from(libc::c_int::MAX)) as libc::c_int
}

/// Set `FD_CLOEXEC` on every descriptor from 3 upward. Runs between fork and
/// exec, so it uses only async-signal-safe calls.
#[cfg(unix)]
fn mark_inherited_descriptors_cloexec(limit: libc::c_int) {
    // Linux 5.11+ marks the whole range in one call. It is invoked as a raw
    // syscall so the glibc 2.28 build does not need the newer libc wrapper;
    // older kernels reject it and take the per-descriptor loop.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: close_range with CLOSE_RANGE_CLOEXEC only changes
        // descriptor flags.
        let marked = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if marked == 0 {
            return;
        }
    }
    for fd in 3..limit {
        // SAFETY: F_SETFD on a closed descriptor fails with EBADF and changes
        // nothing; on an open one it only sets close-on-exec.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
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

/// Run a noninteractive child with stdout sent directly to a file. No payload
/// pipe or in-memory output buffer is created. Call from supervised blocking
/// work, whose process group owns cancellation of the child.
pub fn run_to_file(command: &mut Command, output: std::fs::File) -> Result<ExitStatus> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::inherit())
        .status()
        .context("run child process with file output")
}

/// Transfer the terminal to a foreground child, including its input. Used
/// for process replacement on platforms without exec; the caller must have
/// released its own terminal event reader before waiting here.
pub fn run_interactive(command: &mut Command) -> Result<ExitStatus> {
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("run interactive child process")
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

/// Signal a whole process group. Terminals reuse this so process-group
/// termination lives in one place. A group that is already gone counts as
/// success; anything else is reported rather than dropped.
#[cfg(unix)]
pub fn terminate_process_group(pid: i32, signal: i32) {
    if let Err(error) = signal_process_group(pid, signal) {
        tracing::warn!(pid, signal, %error, "could not signal process group");
    }
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
    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_timeout_reaps_the_child_before_returning() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("child.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "echo $$ > \"$1\"; sleep 60 & wait", "reap-fixture"])
            .arg(&pid_file);
        let error =
            super::run_bounded(&mut command, 100_000, std::time::Duration::from_millis(200))
                .await
                .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "bounded subprocess must already be reaped"
        );
    }

    /// The leader's exit completes a bounded command; a descendant that keeps
    /// the pipes open is stopped after the drain instead of failing the
    /// command at its timeout.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bounded_capture_completes_at_leader_exit_when_a_descendant_holds_the_pipes() {
        use std::time::{Duration, Instant};
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("descendant");
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "sleep 300 & echo $! > \"$1\"; echo hi", "held-pipes"])
            .arg(&pid_file);
        let started = Instant::now();
        let output = super::run_bounded(&mut command, 100_000, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(output.stdout, b"hi\n");
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_none_or(|state| {
                state
                    .rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z'))
            })
        {
            if Instant::now() >= deadline {
                // SAFETY: cleans up this test's own fixture.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("descendant {pid} survived the completed command");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_capture_drains_large_pipes_and_stops_on_overflow_or_timeout() {
        use std::time::Duration;
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2",
        ]);
        let output = super::run_bounded(&mut command, 200_000, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(output.stdout.len(), 131072);
        assert_eq!(output.stderr.len(), 131072);
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "head -c 200000 /dev/zero; sleep 30"]);
        let error = super::run_bounded(&mut command, 100_000, Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"));
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 30 & wait"]);
        let error = super::run_bounded(&mut command, 100_000, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_input_streams_more_than_one_pipe_buffer_concurrently() {
        use std::time::Duration;
        let input = vec![b'x'; 512 * 1024];
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "cat"]);
        let output = super::run_bounded_with_input(
            &mut command,
            &input,
            600 * 1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, input);
    }
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

        // The grandchild can briefly be a zombie between exit and init's
        // wait. What matters is that its adoptive parent eventually reaps it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = detached_test_process_state(pid);
            if state.is_none() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process {pid} never left the process table (last state: {state:?})"
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
        let mut command = Command::new("sleep");
        command.arg("30");
        let pid = spawn_detached(&mut command, &log_dir.path().join("child.log"))
            .expect("spawn_detached should start the child");

        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .expect("the reported pid must name a live process");
        assert_eq!(comm.trim(), "sleep");

        let raw_pid = libc::pid_t::try_from(pid).expect("pid fits pid_t");
        // SAFETY: getpgid only reads the group of an existing process.
        let group = unsafe { libc::getpgid(raw_pid) };
        assert_eq!(group, raw_pid, "the child must lead its own process group");

        signal_process_group(raw_pid, libc::SIGKILL).expect("terminate the detached child group");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_detached_child_keeps_no_descriptor_its_launcher_left_inheritable() {
        // A launcher can hold a descriptor without close-on-exec: on macOS a
        // sibling thread's pipe is briefly in that state. If the detached
        // child kept it, whoever reads that pipe would wait for EOF for the
        // child's whole life.
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::time::Duration;

        let (mut reader, writer) = std::io::pipe().expect("create pipe");
        // SAFETY: clears FD_CLOEXEC on a descriptor this test owns.
        let cleared = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0) };
        assert_eq!(cleared, 0, "clear FD_CLOEXEC on the write end");

        let log_dir = tempfile::tempdir().expect("create log directory");
        let mut command = Command::new("sleep");
        command.arg("30");
        let spawned = spawn_detached(&mut command, &log_dir.path().join("child.log"));
        drop(writer);
        let pid = spawned.expect("spawn_detached should start the child");

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = sender.send(reader.read_to_end(&mut rest).map(|_| ()));
        });
        let outcome = receiver.recv_timeout(Duration::from_secs(10));

        let raw_pid = libc::pid_t::try_from(pid).expect("pid fits pid_t");
        signal_process_group(raw_pid, libc::SIGKILL).expect("terminate the detached child group");
        outcome
            .expect("the pipe must reach EOF while the detached child is still running")
            .expect("read the pipe to EOF");
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
