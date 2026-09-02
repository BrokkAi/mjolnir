//! Terminals Hel runs on behalf of an ACP agent.
//!
//! An agent that asks for `terminal/create` gets a shell command executed in
//! the worker process — inside the container for containerized targets — with
//! its output streamed into a capped buffer and its exit status reported back.
//! Every terminal owns a process group, so one signal reaches the descendants
//! that inherited its pipes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::hel_acp::RuntimeEvent;

/// Output retained per terminal when the agent names no limit. Kimi asks for
/// exactly this much; the constant covers the agents that ask for nothing.
pub const DEFAULT_TERMINAL_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// How far a buffer may grow past its limit before it drops from the front.
/// Trimming on every append would move the whole retained tail per read.
const TERMINAL_BUFFER_SLACK_BYTES: usize = 64 * 1024;

/// Read size for a terminal's pipes.
const TERMINAL_READ_CHUNK_BYTES: usize = 16 * 1024;

/// How long connection teardown waits for one supervisor to reap its child
/// after the process group was killed.
const TERMINAL_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What to run for one `terminal/create`, already resolved against the
/// session: no ACP types survive this boundary.
#[derive(Debug, Clone)]
pub struct TerminalSpawn {
    /// The command string exactly as the agent sent it.
    pub command: String,
    /// Arguments the agent sent, if any.
    pub args: Vec<String>,
    /// Variables to overlay on the daemon environment.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub output_byte_limit: usize,
}

/// How a terminal's child ended, in ACP's terms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalExit {
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

/// What `terminal/output` serves: the retained output plus the exit status
/// once the child has been reaped.
#[derive(Debug, Clone)]
pub struct TerminalSnapshot {
    pub output: String,
    pub truncated: bool,
    pub exit: Option<TerminalExit>,
}

/// A terminal's captured output, capped at the limit the agent asked for.
#[derive(Debug)]
pub struct TerminalBuffer {
    bytes: Vec<u8>,
    limit: usize,
    dropped: bool,
}

impl TerminalBuffer {
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit: limit.max(1),
            dropped: false,
        }
    }

    pub fn append(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > self.limit.saturating_add(TERMINAL_BUFFER_SLACK_BYTES) {
            let excess = self.bytes.len() - self.limit;
            self.bytes.drain(..excess);
            self.dropped = true;
        }
    }

    /// The retained tail as valid UTF-8, and whether anything was dropped.
    #[must_use]
    pub fn read(&self) -> (String, bool) {
        let mut start = self.bytes.len().saturating_sub(self.limit);
        let truncated = self.dropped || start > 0;
        if truncated {
            // A front cut can land inside a multi-byte character, and ACP
            // requires the served output to start on a character boundary.
            while start < self.bytes.len() && self.bytes[start] & 0b1100_0000 == 0b1000_0000 {
                start += 1;
            }
        }
        (
            String::from_utf8_lossy(&self.bytes[start..]).into_owned(),
            truncated,
        )
    }

    /// Bytes held right now, including slack a compaction has not reclaimed.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.bytes.len()
    }
}

/// A terminal's process group, killed when the registry entry holding it goes
/// away. Explicit teardown is the normal path; this guard covers the one that
/// is not, a connection future dropped mid-flight when the ACP bridge dies.
/// Nothing else in the container would stop these processes.
struct ProcessGroup {
    /// The group leader, and the target of a group signal.
    pid: i32,
    exit: watch::Receiver<Option<TerminalExit>>,
}

impl ProcessGroup {
    fn kill_if_live(&self) {
        // Only signal a group whose child has not been reaped: the kernel may
        // already have handed its ID to somebody else.
        if self.exit.borrow().is_none() {
            kill_process_group(self.pid);
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill_if_live();
    }
}

struct TerminalEntry {
    group: ProcessGroup,
    buffer: Arc<Mutex<TerminalBuffer>>,
    exit: watch::Receiver<Option<TerminalExit>>,
    supervisor: JoinHandle<()>,
}

/// The terminals one ACP connection owns.
///
/// Cloning shares the registry; the ACP handlers and connection teardown hold
/// the same one.
#[derive(Clone, Default)]
pub struct TerminalRegistry {
    terminals: Arc<Mutex<BTreeMap<String, TerminalEntry>>>,
    next_id: Arc<AtomicU64>,
}

impl TerminalRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `spawn` in its own process group and register it under a fresh
    /// terminal id.
    pub fn create(
        &self,
        spawn: TerminalSpawn,
        events: mpsc::Sender<RuntimeEvent>,
    ) -> Result<String> {
        let line = shell_line(&spawn.command, &spawn.args);
        let mut child = spawn_shell(&line, &spawn)?;
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .context("client terminal has no usable process ID")?;
        let stdout = child
            .stdout
            .take()
            .context("client terminal stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("client terminal stderr unavailable")?;
        let buffer = Arc::new(Mutex::new(TerminalBuffer::new(spawn.output_byte_limit)));
        let (exit_tx, exit_rx) = watch::channel(None);
        let terminal_id = format!("term-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let supervisor = tokio::spawn(supervise(
            terminal_id.clone(),
            child,
            stdout,
            stderr,
            buffer.clone(),
            exit_tx,
            events,
        ));
        self.terminals
            .lock()
            .expect("terminal registry lock poisoned")
            .insert(
                terminal_id.clone(),
                TerminalEntry {
                    group: ProcessGroup {
                        pid,
                        exit: exit_rx.clone(),
                    },
                    buffer,
                    exit: exit_rx,
                    supervisor,
                },
            );
        Ok(terminal_id)
    }

    /// The output served for `terminal/output`. `None` for an unknown id.
    #[must_use]
    pub fn output(&self, terminal_id: &str) -> Option<TerminalSnapshot> {
        let terminals = self
            .terminals
            .lock()
            .expect("terminal registry lock poisoned");
        let entry = terminals.get(terminal_id)?;
        let (output, truncated) = entry
            .buffer
            .lock()
            .expect("terminal buffer lock poisoned")
            .read();
        Some(TerminalSnapshot {
            output,
            truncated,
            exit: entry.exit.borrow().clone(),
        })
    }

    /// A receiver a `terminal/wait_for_exit` task can await. `None` for an
    /// unknown id.
    #[must_use]
    pub fn exit_receiver(
        &self,
        terminal_id: &str,
    ) -> Option<watch::Receiver<Option<TerminalExit>>> {
        Some(
            self.terminals
                .lock()
                .expect("terminal registry lock poisoned")
                .get(terminal_id)?
                .exit
                .clone(),
        )
    }

    /// Kill a terminal's process group. The terminal stays registered, so
    /// `terminal/output` and `terminal/wait_for_exit` keep working after it.
    /// `false` for an unknown id.
    pub fn kill(&self, terminal_id: &str) -> bool {
        let terminals = self
            .terminals
            .lock()
            .expect("terminal registry lock poisoned");
        let Some(entry) = terminals.get(terminal_id) else {
            return false;
        };
        entry.group.kill_if_live();
        true
    }

    /// Signal every live terminal's process group without forgetting it.
    /// Cancel uses this so an agent blocked in `terminal/wait_for_exit` can
    /// finish a cooperative `session/cancel`.
    pub fn kill_live(&self) {
        let terminals = self
            .terminals
            .lock()
            .expect("terminal registry lock poisoned");
        for entry in terminals.values() {
            entry.group.kill_if_live();
        }
    }

    /// Kill a terminal if it still runs, then forget it. The supervisor keeps
    /// the child until it reaps it, so the close report still arrives exactly
    /// once; the returned handle is that supervisor.
    pub fn release(&self, terminal_id: &str) -> Option<JoinHandle<()>> {
        let entry = self
            .terminals
            .lock()
            .expect("terminal registry lock poisoned")
            .remove(terminal_id)?;
        // Dropping what is left of the entry kills the group, which happens
        // before the caller can await the supervisor.
        Some(entry.supervisor)
    }

    /// Kill every live terminal and reap its supervisor. A connection must not
    /// leave process groups behind: nothing else in the container would stop
    /// them.
    ///
    /// Every group is killed before any supervisor is awaited, and the
    /// supervisors are then reaped concurrently under one deadline, so teardown
    /// costs one reap timeout rather than one per terminal.
    pub async fn shutdown(&self, events: &mpsc::Sender<RuntimeEvent>) {
        let entries = std::mem::take(
            &mut *self
                .terminals
                .lock()
                .expect("terminal registry lock poisoned"),
        );
        let mut unreaped = BTreeSet::new();
        let mut supervisors = JoinSet::new();
        for (terminal_id, entry) in entries {
            entry.group.kill_if_live();
            let TerminalEntry { supervisor, .. } = entry;
            unreaped.insert(terminal_id.clone());
            supervisors.spawn(async move { (terminal_id, supervisor.await) });
        }

        let deadline = tokio::time::Instant::now() + TERMINAL_REAP_TIMEOUT;
        let mut failures = Vec::new();
        // Collect first and report after: a report waits on the event channel,
        // and that wait must not spend the reap deadline.
        loop {
            match tokio::time::timeout_at(deadline, supervisors.join_next()).await {
                Ok(Some(Ok((terminal_id, reaped)))) => {
                    if let Err(error) = reaped {
                        failures.push(format!(
                            "client terminal {terminal_id} supervisor failed: {error}"
                        ));
                    }
                    unreaped.remove(&terminal_id);
                }
                // The task that awaited one supervisor was itself cancelled or
                // panicked, so its terminal stays unreaped and is reported as
                // such below.
                Ok(Some(Err(_))) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        for terminal_id in unreaped {
            failures.push(format!(
                "client terminal {terminal_id} did not finish within \
                 {TERMINAL_REAP_TIMEOUT:?} of being killed"
            ));
        }
        for message in failures {
            report(events, message).await;
        }
    }
}

impl TerminalSpawn {
    /// The concise command a client can show while this terminal is live.
    /// Kimi normally sends an interpreter plus `-c <script>`; showing the
    /// script itself matches the tool title it would have published.
    #[must_use]
    pub fn display_command(&self) -> String {
        let interpreter = PathBuf::from(&self.command)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "sh" | "bash" | "dash" | "zsh"));
        if interpreter
            && let Some(index) = self.args.iter().position(|arg| arg == "-c")
            && let Some(script) = self.args.get(index + 1)
        {
            return script.clone();
        }
        shell_line(&self.command, &self.args)
    }
}

/// Await a terminal's exit on a receiver cloned out of the registry.
pub async fn wait_for_exit(mut exit: watch::Receiver<Option<TerminalExit>>) -> TerminalExit {
    loop {
        if let Some(exit) = exit.borrow_and_update().clone() {
            return exit;
        }
        if exit.changed().await.is_err() {
            // The supervisor went away without recording an exit, which only
            // happens if its task was dropped. Report the terminal as ended
            // with neither code nor signal rather than waiting forever.
            return TerminalExit::default();
        }
    }
}

/// One shell line for a create request: the command verbatim, plus every
/// argument single-quoted.
///
/// Kimi sends the interpreter in `command` with `["-c", script]` in `args`;
/// Grok Build sends the whole line in `command` with no args at all. Running
/// the joined line under `sh -c` executes both shapes.
#[must_use]
pub fn shell_line(command: &str, args: &[String]) -> String {
    let mut line = command.to_owned();
    for arg in args {
        line.push(' ');
        line.push_str(&crate::hel_targets::posix_quote(arg));
    }
    line
}

#[cfg(unix)]
fn spawn_shell(line: &str, spawn: &TerminalSpawn) -> Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(line)
        .current_dir(&spawn.cwd)
        // ACP has no terminal-input method, so nothing ever writes to the
        // child; a null stdin also rules out the write-while-draining deadlock.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, value) in &spawn.env {
        command.env(name, value);
    }
    // Own the group so a kill reaches descendants holding the pipes open.
    command.process_group(0);
    command
        .spawn()
        .with_context(|| format!("spawn client terminal: {line}"))
}

#[cfg(not(unix))]
fn spawn_shell(_line: &str, _spawn: &TerminalSpawn) -> Result<tokio::process::Child> {
    anyhow::bail!("client terminals need Unix process groups")
}

#[cfg(unix)]
fn kill_process_group(pid: i32) {
    crate::hel_subprocess::terminate_process_group(pid, libc::SIGKILL);
}

#[cfg(not(unix))]
fn kill_process_group(_pid: i32) {}

#[cfg(unix)]
fn exit_from_status(status: std::process::ExitStatus) -> TerminalExit {
    use std::os::unix::process::ExitStatusExt;

    TerminalExit {
        exit_code: status.code().and_then(|code| u32::try_from(code).ok()),
        signal: status.signal().map(signal_name),
    }
}

#[cfg(not(unix))]
fn exit_from_status(status: std::process::ExitStatus) -> TerminalExit {
    TerminalExit {
        exit_code: status.code().and_then(|code| u32::try_from(code).ok()),
        signal: None,
    }
}

/// ACP reports a signal by name. Cover what a terminal actually sees; anything
/// else keeps its number so the report stays truthful.
#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGCONT => "SIGCONT",
        _ => return format!("SIG{signal}"),
    };
    name.to_owned()
}

/// Drain one terminal's pipes into its buffer, reap the child, publish the
/// exit, and report the terminal's close exactly once.
async fn supervise(
    terminal_id: String,
    mut child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    buffer: Arc<Mutex<TerminalBuffer>>,
    exit: watch::Sender<Option<TerminalExit>>,
    events: mpsc::Sender<RuntimeEvent>,
) {
    // Both pipes drain concurrently: a child that fills either one stops
    // running until somebody reads it, and a blocked child never exits.
    let (from_stdout, from_stderr, waited) = tokio::join!(
        drain_pipe(stdout, buffer.clone()),
        drain_pipe(stderr, buffer.clone()),
        child.wait(),
    );

    let (status, failed_reap) = match waited {
        Ok(status) => (exit_from_status(status), None),
        Err(error) => (
            TerminalExit::default(),
            Some(format!("reap client terminal {terminal_id}: {error}")),
        ),
    };
    // Release the waiters before any report: a report waits on a full event
    // channel, and the agent blocked in `terminal/wait_for_exit` is what keeps
    // that channel draining.
    if exit.send(Some(status.clone())).is_err() {
        tracing::debug!(
            %terminal_id,
            operation = "terminal_exit",
            "terminal exit watcher was closed before publication"
        );
    }

    if let Some(message) = failed_reap {
        report(&events, message).await;
    }
    for (stream, result) in [("stdout", from_stdout), ("stderr", from_stderr)] {
        if let Err(error) = result {
            report(
                &events,
                format!("read client terminal {terminal_id} {stream}: {error:#}"),
            )
            .await;
        }
    }

    let (output, truncated) = buffer.lock().expect("terminal buffer lock poisoned").read();
    // The single close report for this terminal: a kill or a release makes the
    // child exit, so both arrive here rather than reporting separately.
    report_event(
        &events,
        RuntimeEvent::TerminalClosed {
            terminal_id,
            output,
            truncated,
            exit_code: status.exit_code,
            signal: status.signal,
        },
    )
    .await;
}

async fn drain_pipe<R>(mut pipe: R, buffer: Arc<Mutex<TerminalBuffer>>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = vec![0_u8; TERMINAL_READ_CHUNK_BYTES];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => return Ok(()),
            Ok(read) => buffer
                .lock()
                .expect("terminal buffer lock poisoned")
                .append(&chunk[..read]),
            Err(error) => return Err(error).context("read client terminal pipe"),
        }
    }
}

async fn report(events: &mpsc::Sender<RuntimeEvent>, message: String) {
    report_event(events, RuntimeEvent::Warning { message }).await;
}

async fn report_event(events: &mpsc::Sender<RuntimeEvent>, event: RuntimeEvent) {
    // A closed channel means the relay already stopped; retain this as a
    // diagnostic because terminal cleanup can otherwise hide the first
    // failed report while still remaining cancellation-safe.
    if let Err(error) = events.send(event).await {
        tracing::debug!(
            operation = "terminal_event",
            %error,
            "terminal event could not reach the relay coordinator"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_keeps_the_tail_and_latches_truncated() {
        let mut buffer = TerminalBuffer::new(8);
        buffer.append(b"abcd");
        assert_eq!(buffer.read(), ("abcd".to_owned(), false));

        buffer.append(b"efghijkl");
        let (output, truncated) = buffer.read();
        assert_eq!(output, "efghijkl", "the buffer must serve the last bytes");
        assert!(truncated);

        // Truncation latches: a later read cannot claim the output is whole.
        let mut latched = TerminalBuffer::new(4);
        latched.append(&vec![b'x'; 4 + TERMINAL_BUFFER_SLACK_BYTES + 1]);
        assert_eq!(latched.buffered_len(), 4);
        latched.append(b"");
        assert!(latched.read().1);
    }

    #[test]
    fn the_buffer_serves_output_starting_on_a_character_boundary() {
        // "é" is two bytes; a cut through it must not reach the reader.
        let mut buffer = TerminalBuffer::new(2);
        buffer.append("aéb".as_bytes());

        let (output, truncated) = buffer.read();
        assert!(truncated);
        assert_eq!(output, "b", "a partial character must not be served");
        assert!(output.is_char_boundary(0));
    }

    #[test]
    fn the_buffer_stays_bounded_while_a_child_floods_it() {
        let limit = 4 * 1024;
        let mut buffer = TerminalBuffer::new(limit);
        for index in 0..1024 {
            buffer.append(&vec![b'0' + u8::try_from(index % 10).unwrap(); 1024]);
            assert!(
                buffer.buffered_len() <= limit + TERMINAL_BUFFER_SLACK_BYTES,
                "a flooded buffer must stay bounded, held {}",
                buffer.buffered_len()
            );
        }

        let (output, truncated) = buffer.read();
        assert!(truncated);
        assert_eq!(output.len(), limit, "a read must never exceed the limit");
        assert!(
            output.ends_with(&"3".repeat(1024)),
            "the retained bytes must be the tail"
        );
    }

    #[test]
    fn a_shell_line_carries_the_command_verbatim_and_quotes_the_arguments() {
        // Kimi's shape: interpreter plus `-c <script>`.
        assert_eq!(
            shell_line(
                "/bin/bash",
                &["-c".to_owned(), "echo 'hi'; rm -rf /".to_owned()]
            ),
            "/bin/bash '-c' 'echo '\\''hi'\\''; rm -rf /'"
        );
        // Grok Build's shape: the whole line already in `command`.
        assert_eq!(
            shell_line("/bin/bash -lc 'echo hi'", &[]),
            "/bin/bash -lc 'echo hi'"
        );
    }

    #[test]
    fn display_command_unwraps_an_interpreter_script() {
        let spawn = TerminalSpawn {
            command: "/bin/bash".into(),
            args: vec!["-c".into(), "cargo mutants --in-diff diff".into()],
            env: Vec::new(),
            cwd: PathBuf::from("/workspace"),
            output_byte_limit: 1024,
        };

        assert_eq!(spawn.display_command(), "cargo mutants --in-diff diff");
    }

    #[test]
    fn display_command_preserves_a_non_interpreter_invocation() {
        let spawn = TerminalSpawn {
            command: "/usr/bin/cargo".into(),
            args: vec!["test".into(), "--workspace".into()],
            env: Vec::new(),
            cwd: PathBuf::from("/workspace"),
            output_byte_limit: 1024,
        };

        assert_eq!(
            spawn.display_command(),
            "/usr/bin/cargo 'test' '--workspace'"
        );
    }

    /// Register a terminal whose supervisor never finishes. Its exit is already
    /// published, so teardown signals no process group: the PID is never used.
    fn register_stuck_terminal(registry: &TerminalRegistry, terminal_id: &str) {
        let (_exit, exit_rx) = watch::channel(Some(TerminalExit::default()));
        registry
            .terminals
            .lock()
            .expect("terminal registry lock poisoned")
            .insert(
                terminal_id.to_owned(),
                TerminalEntry {
                    group: ProcessGroup {
                        pid: i32::MAX,
                        exit: exit_rx.clone(),
                    },
                    buffer: Arc::new(Mutex::new(TerminalBuffer::new(1024))),
                    exit: exit_rx,
                    supervisor: tokio::spawn(std::future::pending::<()>()),
                },
            );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_reaps_stuck_terminals_under_one_shared_deadline() {
        let registry = TerminalRegistry::new();
        let (events, mut reports) = mpsc::channel(16);
        for index in 0..4 {
            register_stuck_terminal(&registry, &format!("term-{index}"));
        }

        let started = tokio::time::Instant::now();
        registry.shutdown(&events).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < TERMINAL_REAP_TIMEOUT * 2,
            "teardown must be bounded by one reap timeout, took {elapsed:?}"
        );
        let mut reported = Vec::new();
        while let Ok(RuntimeEvent::Warning { message }) = reports.try_recv() {
            reported.push(message);
        }
        assert_eq!(reported.len(), 4, "{reported:?}");
        for index in 0..4 {
            assert!(
                reported
                    .iter()
                    .any(|message| message.contains(&format!("term-{index} did not finish"))),
                "every stuck terminal must still be reported: {reported:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_reports_a_supervisor_that_finished() {
        let registry = TerminalRegistry::new();
        let (events, mut reports) = mpsc::channel(16);
        register_stuck_terminal(&registry, "stuck");
        let (_exit, exit_rx) = watch::channel(Some(TerminalExit::default()));
        registry
            .terminals
            .lock()
            .expect("terminal registry lock poisoned")
            .insert(
                "finished".to_owned(),
                TerminalEntry {
                    group: ProcessGroup {
                        pid: i32::MAX,
                        exit: exit_rx.clone(),
                    },
                    buffer: Arc::new(Mutex::new(TerminalBuffer::new(1024))),
                    exit: exit_rx,
                    supervisor: tokio::spawn(async {}),
                },
            );

        registry.shutdown(&events).await;

        let mut reported = Vec::new();
        while let Ok(RuntimeEvent::Warning { message }) = reports.try_recv() {
            reported.push(message);
        }
        assert_eq!(
            reported.len(),
            1,
            "a supervisor that finished is not a failure: {reported:?}"
        );
        assert!(reported[0].contains("stuck did not finish"), "{reported:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_killed_child_reports_its_signal_by_name() {
        assert_eq!(signal_name(libc::SIGKILL), "SIGKILL");
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
        // An exotic signal keeps its number rather than being renamed.
        assert_eq!(signal_name(64), "SIG64");
    }
}
