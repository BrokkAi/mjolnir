//! The node protocol.
//!
//! A node (`mj node`) runs on the computer that holds the user's files. It
//! opens one TCP connection to the daemon, authenticates through the daemon's
//! ordinary request protocol, and then serves HTTP/2 on that same connection.
//! The daemon is the HTTP/2 client: every exec it wants on the node's computer
//! is one HTTP/2 stream. The node dialed, but TCP carries both directions, so
//! a node behind NAT or outside a cluster works whenever it can reach the
//! daemon.
//!
//! One stream is one child process. The request body is a length-prefixed
//! JSON [`NodeCommand`] followed by the child's stdin; the end of the request
//! body closes stdin. The response body is a sequence of frames, each a tag
//! byte, a big-endian `u32` length and that many bytes: [`FRAME_STDOUT`] and
//! [`FRAME_STDERR`] carry output, and one final [`FRAME_EXIT`] carries a JSON
//! [`NodeExit`]. HTTP/2 flow control is per stream, so a reader that stops
//! draining one child never stalls another, and a child is never fed faster
//! than the daemon reads its output. Resetting a stream kills the child's
//! process group unless the command detaches.

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::{Buf, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::poll_fn;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A raw stream: the child's stdin, stdout and stderr flow as they happen.
/// The relay proxy to a worker runs this way.
pub const EXEC_PATH: &str = "/exec";
/// A command run to completion exactly as the daemon's own executors run one
/// locally (SSH admission, process groups, the output drain after the leader
/// exits), with its output sent once it finishes.
pub const RUN_PATH: &str = "/run";

/// A frame of the child's standard output.
pub const FRAME_STDOUT: u8 = 1;
/// A frame of the child's standard error.
pub const FRAME_STDERR: u8 = 2;
/// The last frame: the child's [`NodeExit`] as JSON.
pub const FRAME_EXIT: u8 = 0;

/// The largest command description a node accepts. Commands carry scripts,
/// not data; data travels as stdin.
const MAX_COMMAND_BYTES: usize = 4 * 1024 * 1024;

/// How much output one read takes from a child pipe.
const READ_CHUNK: usize = 64 * 1024;

/// HTTP/2 also counts unread bytes against the whole connection. If one exec
/// nobody reads could fill that shared window, it would stall every other
/// exec on the node. The connection window therefore covers every stream's
/// window at once: [`MAX_STREAMS`] streams of [`STREAM_WINDOW`] bytes each.
/// A stream past its own window waits alone; a caller past `MAX_STREAMS`
/// waits for a stream to finish.
const STREAM_WINDOW: u32 = 256 * 1024;
const MAX_STREAMS: u32 = 256;
const CONNECTION_WINDOW: u32 = STREAM_WINDOW * MAX_STREAMS;

/// A command the daemon asks a node to run, as the node's computer sees it.
/// Paths are text in the node's own conventions; only the node turns them
/// into `PathBuf`s.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCommand {
    pub program: String,
    /// Replaces `argv[0]` on Unix when the child reads its mode from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg0: Option<String>,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_env: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The child deliberately leaves processes running after it exits, so
    /// ending the stream must not kill its process group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detaches: bool,
}

/// How a child ended. `status` is its exit code, or `None` when it was killed
/// by a signal; `error` says why it never ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExit {
    pub status: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One decoded response frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeFrame {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(NodeExit),
}

/// Incrementally decodes response frames from body chunks.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: BytesMut,
}

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// The next complete frame, or `None` until more bytes arrive.
    pub fn next_frame(&mut self) -> Result<Option<NodeFrame>> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }
        let tag = self.buffer[0];
        let length = u32::from_be_bytes(self.buffer[1..5].try_into().expect("four bytes")) as usize;
        if self.buffer.len() < 5 + length {
            return Ok(None);
        }
        self.buffer.advance(5);
        let payload = self.buffer.split_to(length).freeze();
        Ok(Some(match tag {
            FRAME_STDOUT => NodeFrame::Stdout(payload),
            FRAME_STDERR => NodeFrame::Stderr(payload),
            FRAME_EXIT => {
                NodeFrame::Exit(serde_json::from_slice(&payload).context("decode node exit frame")?)
            }
            other => bail!("unknown node frame tag {other}"),
        }))
    }

    /// Whether bytes of an incomplete frame remain.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

fn frame(tag: u8, payload: &[u8]) -> Bytes {
    let mut framed = BytesMut::with_capacity(5 + payload.len());
    framed.extend_from_slice(&[tag]);
    framed.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("frame fits u32")
            .to_be_bytes(),
    );
    framed.extend_from_slice(payload);
    framed.freeze()
}

/// Send all of `data`, waiting for the peer's flow-control window. This is
/// where backpressure happens: a reader that stops draining stops the sender.
async fn send_all(stream: &mut h2::SendStream<Bytes>, mut data: Bytes) -> Result<()> {
    while !data.is_empty() {
        stream.reserve_capacity(data.len());
        let capacity = poll_fn(|cx| stream.poll_capacity(cx))
            .await
            .context("node stream closed while sending")??;
        if capacity == 0 {
            continue;
        }
        let chunk = data.split_to(capacity.min(data.len()));
        stream.send_data(chunk, false)?;
    }
    Ok(())
}

/// The next body chunk, with its flow-control credit returned to the peer.
async fn next_chunk(body: &mut h2::RecvStream) -> Result<Option<Bytes>> {
    match body.data().await {
        None => Ok(None),
        Some(chunk) => {
            let chunk = chunk?;
            body.flow_control()
                .release_capacity(chunk.len())
                .context("release node stream capacity")?;
            Ok(Some(chunk))
        }
    }
}

// ---------------------------------------------------------------------------
// Node side
// ---------------------------------------------------------------------------

/// Serve execs on `io` until the daemon closes the connection. Each stream
/// runs on its own task, so one slow child never holds up another.
pub async fn serve<T>(io: T) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut connection = h2::server::Builder::new()
        .initial_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .max_concurrent_streams(MAX_STREAMS)
        .handshake(io)
        .await
        .context("node HTTP/2 handshake")?;
    while let Some(accepted) = connection.accept().await {
        let (request, respond) = accepted.context("accept node stream")?;
        tokio::spawn(async move {
            if let Err(error) = serve_exec(request, respond).await {
                tracing::warn!(error = %format!("{error:#}"), "node exec failed");
            }
        });
    }
    Ok(())
}

async fn serve_exec(
    request: http::Request<h2::RecvStream>,
    respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    let path = request.uri().path().to_owned();
    let mut body = request.into_body();
    let (description, stdin_prefix) = read_description(&mut body).await?;
    match path.as_str() {
        EXEC_PATH => serve_stream(description, stdin_prefix, body, respond).await,
        RUN_PATH => serve_run(description, stdin_prefix, body, respond).await,
        other => bail!("unknown node path {other}"),
    }
}

/// The length-prefixed JSON description at the start of a request body, and
/// whatever stdin bytes arrived with it.
async fn read_description(body: &mut h2::RecvStream) -> Result<(Bytes, Bytes)> {
    let mut pending = BytesMut::new();
    while pending.len() < 4 {
        let chunk = next_chunk(body)
            .await?
            .context("node request ended before its command")?;
        pending.extend_from_slice(&chunk);
    }
    let length = u32::from_be_bytes(pending[..4].try_into().expect("four bytes")) as usize;
    ensure!(
        length <= MAX_COMMAND_BYTES,
        "node command description is {length} bytes; the limit is {MAX_COMMAND_BYTES}"
    );
    while pending.len() < 4 + length {
        let chunk = next_chunk(body)
            .await?
            .context("node request ended inside its command")?;
        pending.extend_from_slice(&chunk);
    }
    pending.advance(4);
    let description = pending.split_to(length).freeze();
    Ok((description, pending.freeze()))
}

async fn serve_stream(
    description: Bytes,
    stdin_prefix: Bytes,
    body: h2::RecvStream,
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    let command: NodeCommand =
        serde_json::from_slice(&description).context("decode node command")?;
    let response = http::Response::builder()
        .status(200)
        .body(())
        .expect("static response");
    let mut send = respond.send_response(response, false)?;
    match spawn(&command) {
        Ok(child) => run_child(child, &command, stdin_prefix, body, &mut send).await,
        Err(error) => {
            let exit = NodeExit {
                status: None,
                error: Some(format!("{error:#}")),
            };
            send_all(&mut send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
            send.send_data(Bytes::new(), true)?;
            Ok(())
        }
    }
}

/// Run a command with this computer's own executor, which is what makes the
/// node a faithful stand-in for local execution, and send its output once it
/// finishes. A reset cancels it exactly as a local cancellation would.
async fn serve_run(
    description: Bytes,
    stdin_prefix: Bytes,
    mut body: h2::RecvStream,
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    use crate::targets::{CancellableProcessExecutor, CommandExecutor, NodeRun};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let run: NodeRun = serde_json::from_slice(&description).context("decode node run")?;
    let streamed = run.streamed_stdin;
    let command = run.into_spec();
    tracing::debug!(program = %command.program, purpose = %command.purpose, "node runs a command");
    let response = http::Response::builder()
        .status(200)
        .body(())
        .expect("static response");
    let mut send = respond.send_response(response, false)?;

    let cancelled = Arc::new(AtomicBool::new(false));
    let (stdin_tx, stdin_rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let executor = CancellableProcessExecutor::new(cancelled.clone());
    let mut work = tokio::task::spawn_blocking(move || {
        if streamed {
            let mut input = ChannelReader::new(stdin_rx);
            executor.execute_with_stdin(&command, &mut input)
        } else {
            executor.execute(&command)
        }
    });
    let feed = async {
        if streamed {
            if !stdin_prefix.is_empty() && stdin_tx.send(stdin_prefix).await.is_err() {
                return anyhow::Ok(());
            }
            while let Some(chunk) = next_chunk(&mut body).await? {
                if stdin_tx.send(chunk).await.is_err() {
                    break;
                }
            }
        }
        drop(stdin_tx);
        // Keep the request open until the command finishes; it carries no
        // more data.
        std::future::pending::<()>().await;
        anyhow::Ok(())
    };
    let result = tokio::select! {
        joined = &mut work => joined.context("node run task failed")?,
        reset = poll_fn(|cx| send.poll_reset(cx)) => {
            cancelled.store(true, Ordering::Release);
            // The executor kills the command's group and returns promptly.
            let _ = work.await;
            bail!("the daemon reset the run: {reset:?}");
        }
        fed = feed => {
            cancelled.store(true, Ordering::Release);
            let _ = work.await;
            return fed.context("feed node run stdin");
        }
    };
    match result {
        Ok(output) => {
            for (tag, bytes) in [
                (FRAME_STDOUT, &output.stdout),
                (FRAME_STDERR, &output.stderr),
            ] {
                for chunk in bytes.chunks(READ_CHUNK) {
                    send_all(&mut send, frame(tag, chunk)).await?;
                }
            }
            let exit = NodeExit {
                status: Some(output.status),
                error: None,
            };
            send_all(&mut send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
        }
        Err(error) => {
            let exit = NodeExit {
                status: None,
                error: Some(format!("{error:#}")),
            };
            send_all(&mut send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
        }
    }
    send.send_data(Bytes::new(), true)?;
    Ok(())
}

/// A blocking `Read` over chunks a task delivers, for executors that read
/// stdin synchronously.
struct ChannelReader {
    chunks: tokio::sync::mpsc::Receiver<Bytes>,
    current: Bytes,
}

impl ChannelReader {
    fn new(chunks: tokio::sync::mpsc::Receiver<Bytes>) -> Self {
        Self {
            chunks,
            current: Bytes::new(),
        }
    }
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        while self.current.is_empty() {
            match self.chunks.blocking_recv() {
                Some(chunk) => self.current = chunk,
                None => return Ok(0),
            }
        }
        let count = buffer.len().min(self.current.len());
        buffer[..count].copy_from_slice(&self.current.split_to(count));
        Ok(count)
    }
}

fn spawn(command: &NodeCommand) -> Result<tokio::process::Child> {
    let mut process = tokio::process::Command::new(&command.program);
    #[cfg(unix)]
    if let Some(arg0) = &command.arg0 {
        process.arg0(arg0);
    }
    process.args(&command.args);
    if command.clear_env {
        process.env_clear();
    }
    process.envs(&command.env);
    if let Some(cwd) = &command.cwd {
        process.current_dir(cwd);
    }
    // Its own process group, so the whole tree can be torn down at once.
    #[cfg(unix)]
    process.process_group(0);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(!command.detaches)
        .spawn()
        .with_context(|| format!("run {}", command.program))
}

/// Feed stdin, drain stdout and stderr, and report the exit, all at once.
/// Output is sent as it arrives; stdin is written only as fast as the child
/// reads it. Pipes are never written before they are drained.
async fn run_child(
    mut child: tokio::process::Child,
    command: &NodeCommand,
    stdin_prefix: Bytes,
    mut body: h2::RecvStream,
    send: &mut h2::SendStream<Bytes>,
) -> Result<()> {
    let group = (!command.detaches).then(|| crate::subprocess::ProcessGroupGuard::new(child.id()));
    let mut stdin = child.stdin.take().context("node child stdin missing")?;
    let stdout = child.stdout.take().context("node child stdout missing")?;
    let stderr = child.stderr.take().context("node child stderr missing")?;

    let feed = async move {
        if !stdin_prefix.is_empty() {
            stdin.write_all(&stdin_prefix).await?;
        }
        while let Some(chunk) = next_chunk(&mut body).await? {
            if let Err(error) = stdin.write_all(&chunk).await {
                // A child that closed its stdin has stopped reading; that is
                // its business, and its output still matters.
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        stdin.shutdown().await.ok();
        anyhow::Ok(())
    };

    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel::<Bytes>(4);
    let pump = |mut pipe: Box<dyn AsyncRead + Unpin + Send>, tag: u8| {
        let frames = frames_tx.clone();
        async move {
            let mut buffer = vec![0; READ_CHUNK];
            loop {
                let read = pipe.read(&mut buffer).await?;
                if read == 0 {
                    return anyhow::Ok(());
                }
                if frames.send(frame(tag, &buffer[..read])).await.is_err() {
                    return Ok(());
                }
            }
        }
    };
    let out = pump(Box::new(stdout), FRAME_STDOUT);
    let err = pump(Box::new(stderr), FRAME_STDERR);
    drop(frames_tx);
    // Forward output, and notice a reset even while the child prints nothing:
    // a quiet child must still die when the daemon gives up on it.
    let forward = async {
        loop {
            let next = poll_fn(|cx| {
                if let std::task::Poll::Ready(reset) = send.poll_reset(cx) {
                    return std::task::Poll::Ready(Err(match reset {
                        Ok(reason) => anyhow!("the daemon reset the exec ({reason})"),
                        Err(error) => anyhow!(error).context("node stream failed"),
                    }));
                }
                frames_rx.poll_recv(cx).map(Ok)
            })
            .await?;
            match next {
                Some(framed) => send_all(send, framed).await?,
                None => return anyhow::Ok(()),
            }
        }
    };

    let streamed = tokio::try_join!(
        async { feed.await.context("feed node child stdin") },
        async { out.await.context("read node child stdout") },
        async { err.await.context("read node child stderr") },
        forward,
    );
    if let Err(error) = streamed {
        // The daemon reset the stream or a pipe failed: nobody will read this
        // child's output, so it must not keep running.
        if let Some(group) = &group {
            group.kill();
        }
        child.kill().await.ok();
        return Err(error);
    }
    let status = tokio::select! {
        status = child.wait() => status.context("wait for node child")?,
        reset = poll_fn(|cx| send.poll_reset(cx)) => {
            if let Some(group) = &group {
                group.kill();
            }
            child.kill().await.ok();
            bail!("the daemon reset the exec while its child ran: {reset:?}");
        }
    };
    let exit = NodeExit {
        status: status.code(),
        error: None,
    };
    send_all(send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
    send.send_data(Bytes::new(), true)?;
    // A non-detaching command owns its whole group: the guard kills what the
    // child left behind, as local execution does.
    drop(group);
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon side
// ---------------------------------------------------------------------------

/// The daemon's handle on one node connection. Cloning it is cheap; every
/// clone opens streams on the same connection.
#[derive(Clone)]
pub struct NodeClient {
    send_request: h2::client::SendRequest<Bytes>,
}

impl NodeClient {
    /// Start the daemon's side of the node protocol on `io`. The returned
    /// future drives the connection and ends when the node goes away; the
    /// caller owns it.
    pub async fn handshake<T>(
        io: T,
    ) -> Result<(Self, impl std::future::Future<Output = Result<()>> + Send)>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (send_request, connection) = h2::client::Builder::new()
            .initial_window_size(STREAM_WINDOW)
            .initial_connection_window_size(CONNECTION_WINDOW)
            .handshake(io)
            .await
            .context("node HTTP/2 handshake")?;
        let driver = async move { connection.await.context("node connection") };
        Ok((Self { send_request }, driver))
    }

    /// Start `command` on the node. Dropping the returned [`NodeExec`] before
    /// it finishes resets the stream, which kills the child.
    pub async fn exec(&self, command: &NodeCommand) -> Result<NodeExec> {
        self.open(EXEC_PATH, &serde_json::to_vec(command)?, false)
            .await
    }

    /// Run `run` on the node with the node's own executor. The caller feeds
    /// stdin when `run.streamed_stdin`, then collects the output; dropping it
    /// first cancels the command.
    pub async fn run(&self, run: &crate::targets::NodeRun) -> Result<NodeExec> {
        // Without streamed stdin the request ends with its description, so a
        // command that finishes at once never races an open request body.
        self.open(RUN_PATH, &serde_json::to_vec(run)?, !run.streamed_stdin)
            .await
    }

    async fn open(&self, path: &str, description: &[u8], complete: bool) -> Result<NodeExec> {
        let mut send_request = self
            .send_request
            .clone()
            .ready()
            .await
            .context("node connection is gone")?;
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://node{path}"))
            .body(())
            .expect("static request");
        let (response, mut stdin) = send_request.send_request(request, false)?;
        let mut prefix = BytesMut::with_capacity(4 + description.len());
        prefix.extend_from_slice(
            &u32::try_from(description.len())
                .map_err(|_| anyhow!("node command description is too large"))?
                .to_be_bytes(),
        );
        prefix.extend_from_slice(description);
        send_all(&mut stdin, prefix.freeze()).await?;
        let stdin = if complete {
            stdin.send_data(Bytes::new(), true)?;
            None
        } else {
            Some(stdin)
        };
        let response = response.await.context("node refused the exec")?;
        ensure!(
            response.status() == 200,
            "node answered the exec with {}",
            response.status()
        );
        Ok(NodeExec {
            stdin,
            output: response.into_body(),
            decoder: FrameDecoder::default(),
        })
    }
}

/// One running exec on a node.
pub struct NodeExec {
    stdin: Option<h2::SendStream<Bytes>>,
    output: h2::RecvStream,
    decoder: FrameDecoder,
}

impl NodeExec {
    /// Write to the child's stdin, waiting while the child is not reading.
    /// A node that has already finished the exec stops taking input; that is
    /// not an error, and the output says how the exec ended.
    pub async fn write_stdin(&mut self, data: Bytes) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        match send_all(stdin, data).await {
            Err(error) if finished_by_peer(&error) => {
                self.stdin = None;
                Ok(())
            }
            result => result,
        }
    }

    /// Close the child's stdin.
    pub fn close_stdin(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            match stdin.send_data(Bytes::new(), true) {
                Err(error) if error.reason() == Some(h2::Reason::NO_ERROR) => {}
                result => result?,
            }
        }
        Ok(())
    }

    /// The next frame of output, or the exit. `None` after the exit.
    pub async fn next(&mut self) -> Result<Option<NodeFrame>> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            match next_chunk(&mut self.output).await? {
                Some(chunk) => self.decoder.push(&chunk),
                None => {
                    ensure!(self.decoder.is_empty(), "node stream ended inside a frame");
                    return Ok(None);
                }
            }
        }
    }

    /// Feed `input`, close stdin, and collect everything the child printed.
    /// Stdin is fed while output drains, so a child that prints before it
    /// has read all its input cannot deadlock the exchange.
    pub async fn collect(mut self, input: Bytes) -> Result<NodeOutput> {
        let mut stdin = self.stdin.take();
        let feed = async {
            if let Some(stdin) = stdin.as_mut() {
                let fed = async {
                    send_all(stdin, input).await?;
                    stdin.send_data(Bytes::new(), true)?;
                    anyhow::Ok(())
                }
                .await;
                match fed {
                    Err(error) if finished_by_peer(&error) => {}
                    result => result?,
                }
            }
            anyhow::Ok(())
        };
        let drain = async {
            let mut output = NodeOutput::default();
            while let Some(frame) = self.next().await? {
                match frame {
                    NodeFrame::Stdout(bytes) => output.stdout.extend_from_slice(&bytes),
                    NodeFrame::Stderr(bytes) => output.stderr.extend_from_slice(&bytes),
                    NodeFrame::Exit(exit) => output.exit = Some(exit),
                }
            }
            anyhow::Ok(output)
        };
        let ((), output) = tokio::try_join!(feed, drain)?;
        ensure!(output.exit.is_some(), "node stream ended without an exit");
        Ok(output)
    }
}

/// The node ended the stream cleanly before reading all input: it has its
/// answer and needs no more.
fn finished_by_peer(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<h2::Error>()
            .is_some_and(|error| error.reason() == Some(h2::Reason::NO_ERROR))
    })
}

/// Everything a finished exec printed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NodeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit: Option<NodeExit>,
}

/// The daemon's [`crate::targets::CommandHost`]: every command goes to the
/// node that serves this computer's work. Which node that is has one owner,
/// the [`NodeSlot`] the daemon's node registry publishes into.
pub struct NodeCommandHost {
    slot: tokio::sync::watch::Receiver<Option<NodeClient>>,
    runtime: tokio::runtime::Handle,
}

/// The connected node, or `None` between connections. The daemon's registry
/// is the only writer.
pub type NodeSlot = tokio::sync::watch::Sender<Option<NodeClient>>;

/// How long a command waits for a node to connect before it fails. A node is
/// started beside the daemon and redials within seconds of losing it.
pub const NODE_CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl NodeCommandHost {
    pub fn new(
        slot: tokio::sync::watch::Receiver<Option<NodeClient>>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self { slot, runtime }
    }

    async fn connected(&self) -> Result<NodeClient> {
        let mut slot = self.slot.clone();
        let waited = tokio::time::timeout(NODE_CONNECT_WAIT, slot.wait_for(Option::is_some)).await;
        match waited {
            Ok(Ok(client)) => Ok(client.clone().expect("waited for a node")),
            Ok(Err(_)) => bail!("the daemon's node registry has stopped"),
            Err(_) => bail!(
                "no node from this computer is connected, so nothing can run here; \
                 `mj` starts one beside the daemon"
            ),
        }
    }
}

impl crate::targets::CommandHost for NodeCommandHost {
    fn run(
        &self,
        command: &crate::targets::CommandSpec,
        input: Option<&mut (dyn std::io::Read + Send)>,
        is_cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<crate::targets::CommandOutput> {
        let streamed = input.is_some();
        let run = command.to_node_run(streamed);
        // Called from a thread `off_async_worker` has taken off the runtime,
        // so blocking here holds no async worker.
        self.runtime.block_on(async {
            let work = async {
                let client = self.connected().await?;
                let mut exec = client.run(&run).await?;
                if let Some(input) = input {
                    // Stdin is read synchronously on this thread, a chunk at
                    // a time, and each chunk waits for the node's window.
                    let mut buffer = vec![0; READ_CHUNK];
                    loop {
                        let read = input.read(&mut buffer).context("read command stdin")?;
                        if read == 0 {
                            break;
                        }
                        exec.write_stdin(Bytes::copy_from_slice(&buffer[..read]))
                            .await?;
                    }
                    exec.close_stdin()?;
                }
                exec.collect(Bytes::new()).await
            };
            let cancelled = async {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
                loop {
                    tick.tick().await;
                    if is_cancelled() {
                        return;
                    }
                }
            };
            tokio::select! {
                output = work => {
                    let output = output.with_context(|| {
                        format!("run {} for {} on the node", command.program, command.purpose)
                    })?;
                    let exit = output.exit.expect("collect requires an exit");
                    match (exit.status, exit.error) {
                        (Some(status), None) => Ok(crate::targets::CommandOutput {
                            status,
                            stdout: output.stdout,
                            stderr: output.stderr,
                        }),
                        // The node's own error text, so callers that look for
                        // "operation cancelled" still find it.
                        (_, Some(error)) => Err(anyhow!(error)),
                        (None, None) => bail!("the node reported no exit status"),
                    }
                }
                () = cancelled => bail!("operation cancelled"),
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn connected() -> NodeClient {
        let (daemon_side, node_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move { serve(node_side).await });
        let (client, driver) = NodeClient::handshake(daemon_side).await.unwrap();
        tokio::spawn(driver);
        client
    }

    fn sh(script: &str) -> NodeCommand {
        NodeCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            ..NodeCommand::default()
        }
    }

    // A stream protocol over bounded windows: drive far more than 64KB both
    // ways at once, through children that write output before they have read
    // their input, while another exec on the same connection is not read at
    // all. Any write-before-drain or shared-window mistake deadlocks here.
    #[tokio::test]
    async fn concurrent_execs_stream_megabytes_both_ways_past_a_stalled_reader() {
        let client = connected().await;

        // Prints 2MB before reading anything, and nobody reads it.
        let mut stalled = client
            .exec(&sh("head -c 2000000 /dev/zero; cat >/dev/null"))
            .await
            .unwrap();
        stalled.close_stdin().unwrap();

        let input: Bytes = (0..3_000_000u32)
            .map(|n| (n % 251) as u8)
            .collect::<Vec<_>>()
            .into();
        let mut runs = Vec::new();
        for _ in 0..3 {
            let exec = client
                .exec(&sh("head -c 1000000 /dev/urandom >&2; cat"))
                .await
                .unwrap();
            runs.push(tokio::spawn(exec.collect(input.clone())));
        }
        for run in runs {
            let output = tokio::time::timeout(std::time::Duration::from_secs(60), run)
                .await
                .expect("streams finish while another stream is stalled")
                .unwrap()
                .unwrap();
            assert_eq!(
                output.exit,
                Some(NodeExit {
                    status: Some(0),
                    error: None
                })
            );
            assert_eq!(output.stdout.len(), input.len());
            assert!(
                output.stdout == input.as_ref(),
                "stdout echoes stdin exactly"
            );
            assert_eq!(output.stderr.len(), 1_000_000);
        }
        drop(stalled);
    }

    // Dropping an exec resets its stream; the node must kill the child's whole
    // process group rather than leave it running unread.
    #[tokio::test]
    async fn dropping_an_exec_kills_its_process_group() {
        let client = connected().await;
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("grandchild");
        let script = format!("(sleep 3; touch {}) & echo started; wait", marker.display());
        let mut exec = client.exec(&sh(&script)).await.unwrap();
        exec.close_stdin().unwrap();
        assert_eq!(
            exec.next().await.unwrap(),
            Some(NodeFrame::Stdout(Bytes::from_static(b"started\n")))
        );
        drop(exec);
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the grandchild was killed with its group");
    }

    // The daemon's executors reach the node through `NodeCommandHost`, which
    // feeds stdin from a synchronous reader and checks cancellation on its
    // own clock. A cancellation must stop the command on the node, process
    // group included, even while megabytes of stdin are still in flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_run_stops_its_process_group_on_the_node_mid_stdin() {
        use crate::targets::{CommandHost, CommandSpec};
        use std::sync::atomic::{AtomicBool, Ordering};

        let client = connected().await;
        let (slot, receiver) = tokio::sync::watch::channel(Some(client));
        let host = NodeCommandHost::new(receiver, tokio::runtime::Handle::current());
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("grandchild");
        let script = format!(
            "(sleep 3; touch {}) & head -c 100000 >/dev/null; sleep 30",
            marker.display()
        );
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let started = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let command = CommandSpec::new("sh", ["-c", script.as_str()]);
            // Far more than the child reads: the feed is still blocked on the
            // node's window when the cancellation lands.
            let mut input = std::io::Cursor::new(vec![7u8; 8_000_000]);
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                flag.store(true, Ordering::Release);
            });
            let result = host.run(&command, Some(&mut input), &|| {
                cancelled.load(Ordering::Acquire)
            });
            canceller.join().unwrap();
            result
        })
        .await
        .unwrap();
        let error = result.expect_err("the cancelled run fails");
        assert!(
            error.to_string().starts_with("operation cancelled"),
            "{error:#}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the node killed the command's group");
        drop(slot);
    }

    // Hard-won: live node smoke: a program missing on the node finished the stream before the daemon closed stdin, masking the error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_command_that_cannot_start_reports_its_own_error_through_the_host() {
        use crate::targets::{CommandHost, CommandSpec};

        let client = connected().await;
        let (_slot, receiver) = tokio::sync::watch::channel(Some(client));
        let host = NodeCommandHost::new(receiver, tokio::runtime::Handle::current());
        let errors = tokio::task::spawn_blocking(move || {
            let command = CommandSpec::new("/nonexistent/podman", ["ps"]);
            let closed = host.run(&command, None, &|| false);
            let mut input = std::io::Cursor::new(vec![0u8; 1_000_000]);
            let streamed = host.run(&command, Some(&mut input), &|| false);
            [closed, streamed]
        })
        .await
        .unwrap();
        for error in errors {
            let error = format!("{:#}", error.expect_err("the program does not exist"));
            assert!(error.contains("/nonexistent/podman"), "{error}");
            assert!(!error.contains("stream error"), "{error}");
        }
    }
}
