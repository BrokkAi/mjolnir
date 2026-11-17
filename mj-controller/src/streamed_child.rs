//! A child process that streams: the caller sees a writer, two readers and an
//! exit it can wait for, try, or force. The relay proxy uses it, and so do
//! the quota and capacity probes that need a live stdin/stdout exchange.

use anyhow::{Context, Result};
use mj_core::subprocess::ProcessGroupGuard;
use mj_core::targets::{CommandSpec, SshSessionLease, StartGate, StartScan};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::process::Command;

pub(crate) type ProxyWriter = Box<dyn AsyncWrite + Unpin + Send>;
pub(crate) type ProxyReader = Box<dyn AsyncRead + Unpin + Send>;

/// Whether a failed local launch means the program was not there.
pub(crate) fn is_missing_program(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

pub(crate) struct ProxyStdio {
    pub(crate) child: ProxyChild,
    pub(crate) stdin: ProxyWriter,
    pub(crate) stdout: ProxyReader,
    pub(crate) stderr: ProxyReader,
}

/// How a proxy ended: its exit code, or `None` when a signal ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProxyExit {
    code: Option<i32>,
}

impl ProxyExit {
    pub(crate) fn code(self) -> Option<i32> {
        self.code
    }

    pub(crate) fn success(self) -> bool {
        self.code == Some(0)
    }
}

impl std::fmt::Display for ProxyExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "exit status: {code}"),
            None => formatter.write_str("terminated without an exit status"),
        }
    }
}

/// The child and its process group. The group is killed once the child has
/// exited, when the child is killed, and when this is dropped, so nothing the
/// proxy started outlives it.
pub(crate) struct ProxyChild {
    child: tokio::process::Child,
    group: Option<ProcessGroupGuard>,
}

impl ProxyChild {
    fn kill_group(&self) {
        if let Some(group) = &self.group {
            group.kill();
        }
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<ProxyExit> {
        let status = self.child.wait().await?;
        self.kill_group();
        Ok(ProxyExit {
            code: status.code(),
        })
    }

    /// Synchronous, because the relay's drop reaper runs without a runtime.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ProxyExit>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.kill_group();
        }
        Ok(status.map(|status| ProxyExit {
            code: status.code(),
        }))
    }

    pub(crate) fn start_kill(&mut self) -> std::io::Result<()> {
        self.kill_group();
        self.child.start_kill()
    }
}

/// Read a child's output to end, keeping at most `limit` bytes.
///
/// The reader always drains to EOF so a child never blocks on a full pipe
/// after the retained output reaches the limit.
pub(crate) async fn collect_bounded(
    mut reader: ProxyReader,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(collected);
        }
        let room = limit.saturating_sub(collected.len());
        collected.extend_from_slice(&buffer[..read.min(room)]);
    }
}

/// Start the relay proxy `spec` describes.
pub(crate) async fn spawn_proxy(spec: &CommandSpec) -> Result<ProxyStdio> {
    spawn_session_proxy(spec, None).await
}

/// Removes the SSH start marker from a proxy's stdout and reports the
/// session's start to its lease's gate.
struct StripStart<R> {
    inner: R,
    scan: StartScan,
    gate: Arc<StartGate>,
    /// Bytes the scan released that the reader has not taken yet.
    ready: Vec<u8>,
    offset: usize,
    finished: bool,
}

impl<R: AsyncRead + Unpin> AsyncRead for StripStart<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.offset < this.ready.len() {
                let count = buf.remaining().min(this.ready.len() - this.offset);
                buf.put_slice(&this.ready[this.offset..this.offset + count]);
                this.offset += count;
                if this.offset == this.ready.len() {
                    this.ready.clear();
                    this.offset = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if this.finished {
                return Poll::Ready(Ok(()));
            }
            let mut chunk = [0_u8; 8192];
            let mut read = ReadBuf::new(&mut chunk);
            match Pin::new(&mut this.inner).poll_read(cx, &mut read) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => {
                    this.finished = true;
                    this.gate.mark_ended();
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(())) if read.filled().is_empty() => {
                    this.finished = true;
                    this.scan.finish(&mut this.ready);
                    this.gate.mark_ended();
                }
                Poll::Ready(Ok(())) => {
                    if this.scan.push(read.filled(), &mut this.ready) {
                        this.gate.mark_started();
                    }
                }
            }
        }
    }
}

impl<R> Drop for StripStart<R> {
    fn drop(&mut self) {
        // A reader that goes away before the end saw no more than it saw.
        self.gate.mark_ended();
    }
}

/// [`spawn_proxy`] for a command that may run as an SSH session the daemon
/// leased itself (`lease`). A command that prints the SSH start marker has it
/// removed from the proxy's stdout, and the lease's gate learns when the
/// session starts.
pub(crate) async fn spawn_session_proxy(
    spec: &CommandSpec,
    lease: Option<&SshSessionLease>,
) -> Result<ProxyStdio> {
    let mut command = Command::new(&spec.program);
    if spec.clear_env {
        command.env_clear();
    }
    command.envs(&spec.env);
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    // Its own process group, so the whole tree can be torn down at once.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never inherit: the controller owns a TUI alternate screen, so a
        // child writing to the shared stderr corrupts the display outside
        // the renderer's buffer. Drain it into the log instead.
        .stderr(Stdio::piped())
        .kill_on_drop(!spec.detaches)
        .spawn()
        .with_context(|| format!("start session relay proxy for {}", spec.purpose))?;
    let group = (!spec.detaches).then(|| ProcessGroupGuard::new(child.id()));
    let stdin = child
        .stdin
        .take()
        .context("relay proxy stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("relay proxy stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("relay proxy stderr unavailable")?;
    let stdout: ProxyReader = match lease.filter(|_| spec.expects_ssh_start_marker()) {
        Some(lease) => Box::new(StripStart {
            inner: stdout,
            scan: StartScan::new(),
            gate: lease.gate().clone(),
            ready: Vec::new(),
            offset: 0,
            finished: false,
        }),
        None => Box::new(stdout),
    };
    Ok(ProxyStdio {
        child: ProxyChild { child, group },
        stdin: Box::new(stdin),
        stdout,
        stderr: Box::new(stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::targets::{SSH_START_MARKER, StartPhase};
    use tokio::io::AsyncWriteExt;

    /// The relay's stdout reader strips the marker even when it arrives in
    /// pieces, passes the protocol bytes after it through whole (past a pipe's
    /// worth), and reports the start before any protocol byte is read.
    #[tokio::test]
    async fn the_start_marker_is_stripped_from_a_relay_stream_split_across_writes() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let gate = StartGate::standalone();
        let mut stripped = StripStart {
            inner: reader,
            scan: StartScan::new(),
            gate: gate.clone(),
            ready: Vec::new(),
            offset: 0,
            finished: false,
        };
        let payload: Vec<u8> = (0..200_000_u32).map(|n| (n % 251) as u8).collect();
        let expected = payload.clone();
        let feed = tokio::spawn(async move {
            let (head, tail) = SSH_START_MARKER.as_bytes().split_at(9);
            writer.write_all(head).await.unwrap();
            tokio::task::yield_now().await;
            writer.write_all(tail).await.unwrap();
            writer.write_all(&payload).await.unwrap();
        });
        let mut received = Vec::new();
        stripped.read_to_end(&mut received).await.unwrap();
        feed.await.unwrap();
        assert_eq!(received, expected);
        assert_eq!(gate.phase(), StartPhase::Started);
    }
}
