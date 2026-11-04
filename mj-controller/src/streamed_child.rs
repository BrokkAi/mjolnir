//! A child process that streams: a local child, or a stream on the node.
//!
//! `mj_core::targets::command_host` decides which, the same way every other
//! command the daemon starts is decided: the `mj daemon-run` process runs its
//! children on its node, and daemons that tests run inside their own process
//! spawn them here. Either way the caller sees a writer, two readers and an
//! exit it can wait for, try, or force. The relay proxy uses it, and so do
//! the quota and capacity probes that need a live stdin/stdout exchange.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use mj_core::node::NodeFrame;
use mj_core::targets::CommandSpec;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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

/// How far the relay may fall behind a node proxy's output before the node is
/// asked to wait. The node's own stream window bounds it as well.
const NODE_PROXY_BUFFER: usize = 256 * 1024;

pub(crate) struct ProxyStdio {
    pub(crate) child: ProxyChild,
    pub(crate) stdin: ProxyWriter,
    pub(crate) stdout: ProxyReader,
    pub(crate) stderr: ProxyReader,
}

/// How a proxy ended: its exit code, or `None` when it was killed or its
/// node stream broke.
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

pub(crate) enum ProxyChild {
    // Boxed: `tokio::process::Child` is over 200 bytes larger on Windows than
    // the node variant, which trips `large_enum_variant` there.
    Local(Box<tokio::process::Child>),
    Node(NodeProxy),
}

pub(crate) struct NodeProxy {
    exit: tokio::sync::watch::Receiver<Option<ProxyExit>>,
    /// Records an exit when the proxy is killed before it reported one.
    exit_tx: tokio::sync::watch::Sender<Option<ProxyExit>>,
    /// Aborting these drops the stream's two halves, which resets it, and the
    /// node then kills the proxy's process group.
    tasks: [tokio::task::AbortHandle; 2],
}

impl ProxyChild {
    pub(crate) async fn wait(&mut self) -> std::io::Result<ProxyExit> {
        match self {
            Self::Local(child) => child.wait().await.map(|status| ProxyExit {
                code: status.code(),
            }),
            Self::Node(node) => {
                let exit = node
                    .exit
                    .wait_for(Option::is_some)
                    .await
                    .map_err(|_| std::io::Error::other("the node proxy task ended"))?;
                Ok(exit.expect("waited for an exit"))
            }
        }
    }

    /// Synchronous, because the relay's drop reaper runs without a runtime.
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ProxyExit>> {
        match self {
            Self::Local(child) => child.try_wait().map(|status| {
                status.map(|status| ProxyExit {
                    code: status.code(),
                })
            }),
            Self::Node(node) => Ok(*node.exit.borrow()),
        }
    }

    pub(crate) fn start_kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Local(child) => child.start_kill(),
            Self::Node(node) => {
                for task in &node.tasks {
                    task.abort();
                }
                node.exit_tx.send_if_modified(|exit| {
                    let unset = exit.is_none();
                    if unset {
                        *exit = Some(ProxyExit { code: None });
                    }
                    unset
                });
                Ok(())
            }
        }
    }
}

/// Start the relay proxy `spec` describes.
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

pub(crate) async fn spawn_proxy(spec: &CommandSpec) -> Result<ProxyStdio> {
    if let Some(host) = mj_core::targets::command_host(spec) {
        return spawn_on_node(host.as_ref(), spec).await;
    }
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
    let mut child = command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never inherit: the controller owns a TUI alternate screen, so a
        // child writing to the shared stderr corrupts the display outside
        // the renderer's buffer. Drain it into the log instead.
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start session relay proxy for {}", spec.purpose))?;
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
    Ok(ProxyStdio {
        child: ProxyChild::Local(Box::new(child)),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
    })
}

async fn spawn_on_node(
    host: &dyn mj_core::targets::CommandHost,
    spec: &CommandSpec,
) -> Result<ProxyStdio> {
    let exec = host
        .open_stream(spec)
        .await
        .with_context(|| format!("start session relay proxy for {} on the node", spec.purpose))?;
    let (mut node_stdin, mut node_output) = exec.split();
    let (stdin, mut stdin_source) = tokio::io::duplex(NODE_PROXY_BUFFER);
    let (mut stdout_sink, stdout) = tokio::io::duplex(NODE_PROXY_BUFFER);
    let (mut stderr_sink, stderr) = tokio::io::duplex(NODE_PROXY_BUFFER);
    let (exit_tx, exit) = tokio::sync::watch::channel(None);

    let purpose = spec.purpose.clone();
    let feed = tokio::spawn(async move {
        let fed = async {
            let mut buffer = vec![0; NODE_PROXY_BUFFER];
            loop {
                let read = stdin_source.read(&mut buffer).await?;
                if read == 0 {
                    return node_stdin.close();
                }
                node_stdin
                    .write(Bytes::copy_from_slice(&buffer[..read]))
                    .await?;
            }
        }
        .await;
        // The relay notices a dead proxy through its output; this says why.
        if let Err(error) = fed {
            tracing::debug!(%purpose, error = format!("{error:#}"), "node relay proxy stdin failed");
        }
    });
    let pump_exit = exit_tx.clone();
    let pump = tokio::spawn(async move {
        let ended = async {
            while let Some(frame) = node_output.next().await? {
                match frame {
                    // A reader that has gone away no longer wants output, but
                    // the exit still has to be read and reported.
                    NodeFrame::Stdout(bytes) => {
                        let _ = stdout_sink.write_all(&bytes).await;
                    }
                    NodeFrame::Stderr(bytes) => {
                        let _ = stderr_sink.write_all(&bytes).await;
                    }
                    NodeFrame::Exit(exit) => {
                        if let Some(error) = exit.error {
                            let _ = stderr_sink.write_all(error.as_bytes()).await;
                        }
                        return anyhow::Ok(ProxyExit { code: exit.status });
                    }
                }
            }
            bail!("the node proxy stream ended without an exit")
        }
        .await;
        let exit = ended.unwrap_or_else(|error| {
            tracing::debug!(
                error = format!("{error:#}"),
                "node relay proxy stream failed"
            );
            ProxyExit { code: None }
        });
        // Closing the sinks gives the relay end-of-file after the last byte.
        drop(stdout_sink);
        drop(stderr_sink);
        pump_exit.send_if_modified(|current| {
            let unset = current.is_none();
            if unset {
                *current = Some(exit);
            }
            unset
        });
    });
    Ok(ProxyStdio {
        child: ProxyChild::Node(NodeProxy {
            exit,
            exit_tx,
            tasks: [feed.abort_handle(), pump.abort_handle()],
        }),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
    })
}
