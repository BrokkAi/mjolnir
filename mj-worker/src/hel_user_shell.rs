//! Supervised user-authored shell commands run by the target worker.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use hel::hel_acp::RuntimeEvent;
use hel::hel_worker::{UserShellResult, UserShellStatus};

pub const MAX_CONCURRENT_USER_SHELLS: usize = 4;
pub const USER_SHELL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const OUTPUT_HEAD_BYTES: usize = 16 * 1024;
const OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const LIVE_OUTPUT_BYTES: usize = 16 * 1024;
const READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct UserShellSpec {
    pub command: String,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, String>,
}

pub struct UserShellRegistry {
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
    events: mpsc::Sender<RuntimeEvent>,
    cancellations: BTreeMap<String, Option<oneshot::Sender<()>>>,
    tasks: JoinSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserShellCancelOutcome {
    Requested,
    AlreadyRequested,
    NotRunning,
}

impl UserShellRegistry {
    pub fn new(
        cwd: PathBuf,
        environment: BTreeMap<String, String>,
        events: mpsc::Sender<RuntimeEvent>,
    ) -> Self {
        Self {
            cwd,
            environment,
            events,
            cancellations: BTreeMap::new(),
            tasks: JoinSet::new(),
        }
    }

    pub fn available_slots(&self) -> usize {
        MAX_CONCURRENT_USER_SHELLS.saturating_sub(self.cancellations.len())
    }

    pub fn start(&mut self, request_id: String, command: String) -> Result<()> {
        let spec = UserShellSpec {
            command,
            cwd: self.cwd.clone(),
            environment: self.environment.clone(),
        };
        let (cancel, cancelled) = oneshot::channel();
        let task_id = request_id.clone();
        let task_events = self.events.clone();
        let task = spawn_user_shell(request_id.clone(), spec, cancelled, task_events).map_err(
            |error| {
                tracing::warn!(
                    %request_id,
                    operation = "user_shell_start",
                    %error,
                    "could not start user shell"
                );
                error
            },
        )?;
        self.cancellations.insert(task_id.clone(), Some(cancel));
        self.tasks.spawn(async move {
            if let Err(error) = task.await {
                tracing::error!(request_id = %task_id, %error, "user shell task stopped");
            }
            task_id
        });
        Ok(())
    }

    pub fn cancel(&mut self, request_id: &str) -> UserShellCancelOutcome {
        let Some(cancel) = self.cancellations.get_mut(request_id) else {
            return UserShellCancelOutcome::NotRunning;
        };
        let Some(cancel) = cancel.take() else {
            return UserShellCancelOutcome::AlreadyRequested;
        };
        if cancel.send(()).is_ok() {
            UserShellCancelOutcome::Requested
        } else {
            // The child already finished and its terminal event is waiting to
            // be folded. Interrupting its durable dispatch here would make
            // that legitimate completion look like a duplicate.
            tracing::debug!(
                %request_id,
                operation = "user_shell_cancel",
                "user shell cancellation receiver was already closed"
            );
            UserShellCancelOutcome::AlreadyRequested
        }
    }

    pub fn completed(&mut self, request_id: &str) {
        self.cancellations.remove(request_id);
        while let Some(joined) = self.tasks.try_join_next() {
            if let Err(error) = joined {
                tracing::error!(%error, "user shell supervisor failed");
            }
        }
    }
}

fn spawn_user_shell(
    request_id: String,
    spec: UserShellSpec,
    cancelled: oneshot::Receiver<()>,
    events: mpsc::Sender<RuntimeEvent>,
) -> Result<tokio::task::JoinHandle<()>> {
    let mut command = tokio::process::Command::new("bash");
    command
        .arg("-lc")
        .arg(crate::hel_worker_runtime::github_cli_login_shell_command(
            &spec.command,
        ))
        .current_dir(&spec.cwd)
        .envs(&spec.environment)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn user shell: {}", spec.command))?;
    let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
    let stdout = child
        .stdout
        .take()
        .context("user shell stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("user shell stderr unavailable")?;
    Ok(tokio::spawn(async move {
        let started = Instant::now();
        let mut group = ProcessGroupGuard { pid };
        let stdout_buffer = Arc::new(Mutex::new(HeadTailBuffer::default()));
        let stderr_buffer = Arc::new(Mutex::new(HeadTailBuffer::default()));
        let stdout_task = tokio::spawn(drain_pipe(
            stdout,
            request_id.clone(),
            spec.command.clone(),
            stdout_buffer.clone(),
            stderr_buffer.clone(),
            true,
            events.clone(),
        ));
        let stderr_task = tokio::spawn(drain_pipe(
            stderr,
            request_id.clone(),
            spec.command.clone(),
            stdout_buffer.clone(),
            stderr_buffer.clone(),
            false,
            events.clone(),
        ));

        let mut cancelled = cancelled;
        let mut timed_out = false;
        let mut was_cancelled = false;
        let waited = tokio::select! {
            status = child.wait() => status,
            _ = &mut cancelled => {
                was_cancelled = true;
                group.kill();
                child.wait().await
            }
            _ = tokio::time::sleep(USER_SHELL_TIMEOUT) => {
                timed_out = true;
                group.kill();
                child.wait().await
            }
        };
        group.disarm();

        let stdout_read = stdout_task.await;
        let stderr_read = stderr_task.await;
        let mut read_errors = Vec::new();
        for (name, read) in [("stdout", stdout_read), ("stderr", stderr_read)] {
            match read {
                Ok(Ok(())) => {}
                Ok(Err(error)) => read_errors.push(format!("read {name}: {error:#}")),
                Err(error) => read_errors.push(format!("{name} reader task failed: {error}")),
            }
        }
        let (stdout, stdout_truncated) = stdout_buffer
            .lock()
            .expect("user shell stdout lock poisoned")
            .final_text();
        let (stderr, stderr_truncated) = stderr_buffer
            .lock()
            .expect("user shell stderr lock poisoned")
            .final_text();

        let (exit_code, signal, wait_error) = match waited {
            Ok(status) => status_parts(status),
            Err(error) => (None, None, Some(format!("wait for user shell: {error}"))),
        };
        if let Some(error) = wait_error {
            read_errors.push(error);
        }
        let status = if !read_errors.is_empty() {
            UserShellStatus::Failed
        } else if timed_out {
            UserShellStatus::TimedOut
        } else if was_cancelled {
            UserShellStatus::Cancelled
        } else if signal.is_some() {
            UserShellStatus::Signaled
        } else {
            UserShellStatus::Exited
        };
        let result = UserShellResult {
            command: spec.command,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            exit_code,
            signal,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            status,
            error: (!read_errors.is_empty()).then(|| read_errors.join("; ")),
        };
        if let Err(error) = events
            .send(RuntimeEvent::UserShellFinished {
                request_id: request_id.clone(),
                result,
            })
            .await
        {
            tracing::warn!(
                %request_id,
                operation = "user_shell_finished",
                %error,
                "user shell result was lost because the relay coordinator stopped"
            );
        }
    }))
}

async fn drain_pipe<R>(
    mut pipe: R,
    request_id: String,
    command: String,
    stdout: Arc<Mutex<HeadTailBuffer>>,
    stderr: Arc<Mutex<HeadTailBuffer>>,
    is_stdout: bool,
    events: mpsc::Sender<RuntimeEvent>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = vec![0; READ_CHUNK_BYTES];
    loop {
        let read = pipe
            .read(&mut chunk)
            .await
            .context("read user shell pipe")?;
        if read == 0 {
            return Ok(());
        }
        let target = if is_stdout { &stdout } else { &stderr };
        target
            .lock()
            .expect("user shell output lock poisoned")
            .append(&chunk[..read]);
        let (stdout_text, stdout_truncated) = stdout
            .lock()
            .expect("user shell stdout lock poisoned")
            .live_text();
        let (stderr_text, stderr_truncated) = stderr
            .lock()
            .expect("user shell stderr lock poisoned")
            .live_text();
        match events.try_send(RuntimeEvent::UserShellOutput {
            request_id: request_id.clone(),
            command: command.clone(),
            stdout: stdout_text,
            stderr: stderr_text,
            stdout_truncated,
            stderr_truncated,
        }) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(
                    %request_id,
                    operation = "user_shell_output",
                    "user shell output could not reach the relay coordinator"
                );
                return Ok(());
            }
        }
    }
}

#[derive(Default)]
struct HeadTailBuffer {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl HeadTailBuffer {
    fn append(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head_remaining = OUTPUT_HEAD_BYTES.saturating_sub(self.head.len());
        let head_bytes = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        for byte in &bytes[head_bytes..] {
            self.tail.push_back(*byte);
            if self.tail.len() > OUTPUT_TAIL_BYTES {
                self.tail.pop_front();
            }
        }
    }

    fn live_text(&self) -> (String, bool) {
        let visible = &self.head[..self.head.len().min(LIVE_OUTPUT_BYTES)];
        (
            String::from_utf8_lossy(visible).into_owned(),
            self.total > LIVE_OUTPUT_BYTES,
        )
    }

    fn final_text(&self) -> (String, bool) {
        let truncated = self.total > OUTPUT_HEAD_BYTES + OUTPUT_TAIL_BYTES;
        let mut bytes = self.head.clone();
        if truncated {
            let dropped = self
                .total
                .saturating_sub(OUTPUT_HEAD_BYTES + OUTPUT_TAIL_BYTES);
            bytes.extend_from_slice(format!("\n[mj dropped {dropped} middle bytes]\n").as_bytes());
        }
        bytes.extend(self.tail.iter().copied());
        (String::from_utf8_lossy(&bytes).into_owned(), truncated)
    }
}

struct ProcessGroupGuard {
    pid: Option<i32>,
}

impl ProcessGroupGuard {
    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            hel::hel_subprocess::terminate_process_group(pid, libc::SIGKILL);
        }
        #[cfg(not(unix))]
        let _ = self.pid;
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn status_parts(status: std::process::ExitStatus) -> (Option<i32>, Option<String>, Option<String>) {
    use std::os::unix::process::ExitStatusExt;
    (
        status.code(),
        status.signal().map(|signal| format!("SIG{signal}")),
        None,
    )
}

#[cfg(not(unix))]
fn status_parts(status: std::process::ExitStatus) -> (Option<i32>, Option<String>, Option<String>) {
    (status.code(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn user_shell_restores_github_wrapper_after_login_profile() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = tempfile::tempdir().unwrap();
        let worker = tempfile::tempdir().unwrap();
        let bin = worker.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let wrapper = bin.join("gh");
        std::fs::write(&wrapper, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut environment = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("GH_TOKEN".into(), "stale-token".into()),
            ("GITHUB_TOKEN".into(), "also-stale".into()),
        ]);
        environment.insert(
            crate::hel_worker_runtime::GITHUB_CLI_BIN_ENV.into(),
            bin.to_string_lossy().into_owned(),
        );
        let (events, mut received) = mpsc::channel(16);
        let mut shells = UserShellRegistry::new(cwd.path().to_path_buf(), environment, events);
        shells
            .start(
                "shell-github-path".into(),
                "printf '%s\\n%s|%s\\n' \"$(command -v gh)\" \"${GH_TOKEN-unset}\" \"${GITHUB_TOKEN-unset}\""
                    .into(),
            )
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(RuntimeEvent::UserShellFinished { result, .. }) = received.recv().await
                {
                    break result;
                }
            }
        })
        .await
        .expect("user shell did not finish");
        assert_eq!(result.status, UserShellStatus::Exited);
        assert_eq!(
            result.stdout,
            format!("{}\nunset|unset\n", wrapper.display())
        );
    }

    #[test]
    fn output_keeps_head_and_tail_without_unbounded_growth() {
        let mut output = HeadTailBuffer::default();
        output.append(&vec![b'a'; OUTPUT_HEAD_BYTES]);
        output.append(&vec![b'b'; 70 * 1024]);
        let (text, truncated) = output.final_text();
        assert!(truncated);
        assert!(text.starts_with(&"a".repeat(OUTPUT_HEAD_BYTES)));
        assert!(text.ends_with(&"b".repeat(OUTPUT_TAIL_BYTES)));
        assert!(text.contains("mj dropped"));
        assert_eq!(output.head.len(), OUTPUT_HEAD_BYTES);
        assert_eq!(output.tail.len(), OUTPUT_TAIL_BYTES);
    }

    #[tokio::test]
    async fn user_shell_drains_more_than_a_pipe_buffer_from_both_streams() {
        let cwd = tempfile::tempdir().unwrap();
        let (events, mut received) = mpsc::channel(64);
        let mut shells = UserShellRegistry::new(cwd.path().to_path_buf(), BTreeMap::new(), events);
        shells
            .start(
                "shell-large-output".into(),
                r#"awk 'BEGIN { for (i = 0; i < 70000; i++) printf "x" }'; printf '\nCWD_MARKER\n'; : > user-shell-cwd-marker; awk 'BEGIN { for (i = 0; i < 70000; i++) printf "y" }' >&2"#
                    .into(),
            )
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(RuntimeEvent::UserShellFinished { result, .. }) = received.recv().await
                {
                    break result;
                }
            }
        })
        .await
        .expect("large-output shell deadlocked");
        assert_eq!(result.status, UserShellStatus::Exited);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout_truncated);
        assert!(result.stderr_truncated);
        assert!(result.stdout.contains("CWD_MARKER"));
        assert!(cwd.path().join("user-shell-cwd-marker").is_file());
        assert!(result.stdout.starts_with(&"x".repeat(OUTPUT_HEAD_BYTES)));
        assert!(result.stderr.starts_with(&"y".repeat(OUTPUT_HEAD_BYTES)));
    }

    #[tokio::test]
    async fn cancelling_a_user_shell_kills_its_process_group() {
        let cwd = tempfile::tempdir().unwrap();
        let (events, mut received) = mpsc::channel(16);
        let mut shells = UserShellRegistry::new(cwd.path().to_path_buf(), BTreeMap::new(), events);
        shells
            .start("shell-cancel-test".into(), "sleep 60 & wait".into())
            .unwrap();
        assert_eq!(
            shells.cancel("shell-cancel-test"),
            UserShellCancelOutcome::Requested
        );
        assert_eq!(
            shells.cancel("shell-cancel-test"),
            UserShellCancelOutcome::AlreadyRequested
        );

        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(RuntimeEvent::UserShellFinished { result, .. }) = received.recv().await
                {
                    break result;
                }
            }
        })
        .await
        .expect("cancelled shell process group was not reaped");
        assert_eq!(result.status, UserShellStatus::Cancelled);
    }
}
