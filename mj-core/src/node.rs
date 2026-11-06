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
//! JSON [`crate::targets::NodeRun`] followed by the child's stdin; the end of the request
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
use std::future::poll_fn;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A raw stream: the child's stdin, stdout and stderr flow as they happen.
/// The relay proxy to a worker runs this way.
pub const EXEC_PATH: &str = "/exec";
/// A command run to completion exactly as the daemon's own executors run one
/// locally (SSH session leases, process groups, the output drain after the leader
/// exits), with its output sent once it finishes.
pub const RUN_PATH: &str = "/run";
/// The daemon is stopping for good, not handing off: the node has nothing
/// left to serve and exits. A daemon that is replaced never sends this, so
/// its node keeps running and serves the next one.
pub const SHUTDOWN_PATH: &str = "/shutdown";
/// Read a host file as a stream of [`FRAME_STDOUT`] bytes.
pub const FS_READ_PATH: &str = "/fs/read";
/// Write the request body to a host file.
pub const FS_WRITE_PATH: &str = "/fs/write";
/// List a host directory as JSON.
pub const FS_LIST_PATH: &str = "/fs/list";
/// Read host path metadata as JSON.
pub const FS_METADATA_PATH: &str = "/fs/metadata";

/// A host path the node should act on. Paths cross as UTF-8 text; only the
/// node turns one into a `PathBuf`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsRequest {
    pub path: String,
}

/// One entry in a host directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsEntry {
    pub name: String,
    pub kind: FsEntryKind,
}

/// Host path metadata, as the daemon records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsMetadata {
    pub len: u64,
    pub modified_ms: Option<i64>,
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub readonly: bool,
}

/// Why [`serve`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeEnd {
    /// The connection ended; a daemon may come back.
    Closed,
    /// The daemon dismissed this node.
    Dismissed,
}

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
pub async fn serve<T>(io: T) -> Result<ServeEnd>
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
        let (request, mut respond) = accepted.context("accept node stream")?;
        if request.uri().path() == SHUTDOWN_PATH {
            let response = http::Response::builder()
                .status(200)
                .body(())
                .expect("static response");
            respond.send_response(response, true)?;
            // Let the answer leave before the connection is dropped.
            connection.graceful_shutdown();
            while let Some(accepted) = connection.accept().await {
                drop(accepted);
            }
            return Ok(ServeEnd::Dismissed);
        }
        tokio::spawn(async move {
            if let Err(error) = serve_exec(request, respond).await {
                tracing::warn!(error = %format!("{error:#}"), "node exec failed");
            }
        });
    }
    Ok(ServeEnd::Closed)
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
        FS_READ_PATH => serve_fs_read(description, respond).await,
        FS_WRITE_PATH => serve_fs_write(description, stdin_prefix, body, respond).await,
        FS_LIST_PATH => serve_fs_list(description, respond).await,
        FS_METADATA_PATH => serve_fs_metadata(description, respond).await,
        other => bail!("unknown node path {other}"),
    }
}

fn fs_request(description: &Bytes) -> Result<std::path::PathBuf> {
    let request: FsRequest =
        serde_json::from_slice(description).context("decode node fs request")?;
    Ok(std::path::PathBuf::from(request.path))
}

async fn fs_response(
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<h2::SendStream<Bytes>> {
    let response = http::Response::builder()
        .status(200)
        .body(())
        .expect("static response");
    Ok(respond.send_response(response, false)?)
}

async fn finish_fs(send: &mut h2::SendStream<Bytes>, exit: NodeExit) -> Result<()> {
    send_all(send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
    send.send_data(Bytes::new(), true)?;
    Ok(())
}

async fn fail_fs(send: &mut h2::SendStream<Bytes>, error: &std::io::Error) -> Result<()> {
    fail_fs_text(send, &error.to_string()).await
}

async fn fail_fs_text(send: &mut h2::SendStream<Bytes>, message: &str) -> Result<()> {
    send_all(send, frame(FRAME_STDERR, message.as_bytes())).await?;
    finish_fs(
        send,
        NodeExit {
            status: None,
            error: Some(message.to_owned()),
        },
    )
    .await
}

async fn serve_fs_read(description: Bytes, respond: h2::server::SendResponse<Bytes>) -> Result<()> {
    let path = fs_request(&description)?;
    let mut send = fs_response(respond).await?;
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) => return fail_fs(&mut send, &error).await,
    };
    let mut buffer = vec![0; READ_CHUNK];
    loop {
        let read = match file.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => return fail_fs(&mut send, &error).await,
        };
        send_all(&mut send, frame(FRAME_STDOUT, &buffer[..read])).await?;
    }
    finish_fs(
        &mut send,
        NodeExit {
            status: Some(0),
            error: None,
        },
    )
    .await
}

async fn serve_fs_write(
    description: Bytes,
    stdin_prefix: Bytes,
    mut body: h2::RecvStream,
    respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    let path = fs_request(&description)?;
    let mut send = fs_response(respond).await?;
    let mut file = match tokio::fs::File::create(&path).await {
        Ok(file) => file,
        Err(error) => return fail_fs(&mut send, &error).await,
    };
    let write: Result<()> = async {
        if !stdin_prefix.is_empty() {
            file.write_all(&stdin_prefix).await?;
        }
        while let Some(chunk) = next_chunk(&mut body).await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }
    .await;
    match write {
        Ok(()) => {
            finish_fs(
                &mut send,
                NodeExit {
                    status: Some(0),
                    error: None,
                },
            )
            .await
        }
        Err(error) => fail_fs_text(&mut send, &format!("{error:#}")).await,
    }
}

async fn serve_fs_list(description: Bytes, respond: h2::server::SendResponse<Bytes>) -> Result<()> {
    let path = fs_request(&description)?;
    let mut send = fs_response(respond).await?;
    let mut directory = match tokio::fs::read_dir(&path).await {
        Ok(directory) => directory,
        Err(error) => return fail_fs(&mut send, &error).await,
    };
    let mut entries = Vec::new();
    loop {
        let entry = match directory.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => return fail_fs(&mut send, &error).await,
        };
        let kind = match entry.file_type().await {
            Ok(file_type) if file_type.is_dir() => FsEntryKind::Directory,
            Ok(file_type) if file_type.is_file() => FsEntryKind::File,
            Ok(file_type) if file_type.is_symlink() => FsEntryKind::Symlink,
            Ok(_) => FsEntryKind::Other,
            Err(error) => return fail_fs(&mut send, &error).await,
        };
        entries.push(FsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
        });
    }
    send_all(
        &mut send,
        frame(FRAME_STDOUT, &serde_json::to_vec(&entries)?),
    )
    .await?;
    finish_fs(
        &mut send,
        NodeExit {
            status: Some(0),
            error: None,
        },
    )
    .await
}

async fn serve_fs_metadata(
    description: Bytes,
    respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    let path = fs_request(&description)?;
    let mut send = fs_response(respond).await?;
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => return fail_fs(&mut send, &error).await,
    };
    let metadata = FsMetadata {
        len: metadata.len(),
        modified_ms: metadata.modified().ok().and_then(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        }),
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        readonly: metadata.permissions().readonly(),
    };
    send_all(
        &mut send,
        frame(FRAME_STDOUT, &serde_json::to_vec(&metadata)?),
    )
    .await?;
    finish_fs(
        &mut send,
        NodeExit {
            status: Some(0),
            error: None,
        },
    )
    .await
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
    mut body: h2::RecvStream,
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<()> {
    use crate::targets::{CancellableProcessExecutor, SshRefusalRetry};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let run: crate::targets::NodeRun =
        serde_json::from_slice(&description).context("decode node exec")?;
    let command = run.into_spec();
    tracing::debug!(program = %command.program, purpose = %command.purpose, "node streams a command");
    let response = http::Response::builder()
        .status(200)
        .body(())
        .expect("static response");
    let mut send = respond.send_response(response, false)?;
    // A reset of this stream, or an end of this function, cancels the SSH
    // preparation below.
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_exit = CancelOnDrop(cancelled.clone());
    let mut retry = SshRefusalRetry::for_command(&command);
    loop {
        // A command over a shared SSH connection leases its session here,
        // where SSH runs, exactly as a local launch does. The executor opens
        // a master that the server keeps refusing until the stream is reset.
        let preparation = {
            let command = command.clone();
            let executor = CancellableProcessExecutor::new(cancelled.clone());
            let mut walked = std::mem::take(retry.walk());
            tokio::task::spawn_blocking(move || {
                let prepared = command
                    .open_ssh_session(&executor, &mut walked)
                    .map(|session| {
                        let (command, lease) = session.into_parts();
                        SshPrepared { command, lease }
                    });
                (prepared, walked)
            })
        };
        tokio::pin!(preparation);
        let prepared = tokio::select! {
            joined = &mut preparation => {
                let (prepared, walked) = joined.context("node SSH preparation task failed")?;
                *retry.walk() = walked;
                prepared
            }
            reset = poll_fn(|cx| send.poll_reset(cx)) => {
                cancelled.store(true, Ordering::Release);
                let _ = preparation.await;
                bail!("the daemon reset the exec during SSH preparation: {reset:?}");
            }
        };
        let spawned =
            prepared.and_then(|prepared| spawn(&prepared.command).map(|child| (child, prepared)));
        let (child, prepared) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                let exit = NodeExit {
                    status: None,
                    error: Some(format!("{error:#}")),
                };
                send_all(&mut send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
                send.send_data(Bytes::new(), true)?;
                return Ok(());
            }
        };
        let destination = command.ssh_destination.clone().unwrap_or_default();
        match run_child(child, prepared, stdin_prefix.clone(), &mut body, &mut send).await? {
            ChildEnd::Exited => return Ok(()),
            // The server turned the session away before its command started,
            // so nothing of the daemon's input was read and nothing was
            // printed: the whole attempt is discarded and retried here, where
            // the lease lives, instead of being reported to the daemon.
            ChildEnd::Refused(refused) => {
                let stderr = String::from_utf8_lossy(&refused.stderr_tail).into_owned();
                let delay = match retry.next(
                    refused.refusal,
                    refused.walks_on,
                    &destination,
                    &command.purpose,
                    &stderr,
                ) {
                    Ok(delay) => delay,
                    Err(persisted) => {
                        // The refusal lasted its whole time bound: the daemon
                        // gets the explanation instead of the last attempt's
                        // stderr, which the error carries.
                        let exit = NodeExit {
                            status: None,
                            error: Some(persisted.to_string()),
                        };
                        send_all(&mut send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
                        send.send_data(Bytes::new(), true)?;
                        return Ok(());
                    }
                };
                // The refused attempt's stderr is the retry's business, not the
                // daemon's: a later failure must not look like a refusal.
                tracing::debug!(
                    purpose = %command.purpose,
                    discarded_stderr_frames = refused.held_stderr.len(),
                    stderr = %stderr.trim(),
                    "discarded the stderr of a refused SSH attempt"
                );
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    reset = poll_fn(|cx| send.poll_reset(cx)) => {
                        bail!("the daemon reset the exec while it waited to retry SSH: {reset:?}");
                    }
                }
            }
        }
    }
}

/// Cancels the SSH preparation of one node stream when the stream ends.
struct CancelOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
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

/// A command ready to spawn, with the SSH session it runs on.
struct SshPrepared {
    command: crate::targets::CommandSpec,
    lease: Option<crate::targets::SshSessionLease>,
}

fn spawn(command: &crate::targets::CommandSpec) -> Result<tokio::process::Child> {
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
    for name in &command.env_remove {
        process.env_remove(name);
    }
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
        .with_context(|| format!("run {} for {}", command.program, command.purpose))
}

/// How one run of a node child ended.
enum ChildEnd {
    /// The exit was sent to the daemon and the stream closed.
    Exited,
    /// The server refused the SSH session before its command started.
    Refused(Refused),
}

/// An attempt the SSH server refused, which the node retries instead of
/// telling the daemon.
struct Refused {
    refusal: crate::targets::SshRefusal,
    /// Whether the refusal is a step of the walk to another master.
    walks_on: bool,
    stderr_tail: Vec<u8>,
    held_stderr: Vec<Bytes>,
}

/// Feed stdin, drain stdout and stderr, and report the exit, all at once.
/// Output is sent as it arrives; stdin is written only as fast as the child
/// reads it. Pipes are never written before they are drained.
///
/// A session whose command prints the SSH start marker is held to it: stdin,
/// including `stdin_prefix`, is not read or written until the marker arrives;
/// the marker is removed from stdout; and stderr is withheld until the session
/// starts or ends. If the server refused the session (exit 255 with no stdout
/// byte), nothing has reached the daemon and nothing of its input was taken,
/// so the caller retries with the same `body`.
async fn run_child(
    mut child: tokio::process::Child,
    prepared: SshPrepared,
    stdin_prefix: Bytes,
    body: &mut h2::RecvStream,
    send: &mut h2::SendStream<Bytes>,
) -> Result<ChildEnd> {
    use crate::targets::{StartGate, StartScan};
    let SshPrepared { command, lease } = prepared;
    let command = &command;
    let gate: Option<std::sync::Arc<StartGate>> = lease
        .as_ref()
        .filter(|_| command.expects_ssh_start_marker())
        .map(|lease| lease.gate().clone());
    let stderr_tail = std::sync::Mutex::new(Vec::<u8>::new());
    let group = (!command.detaches).then(|| crate::subprocess::ProcessGroupGuard::new(child.id()));
    let mut stdin = child.stdin.take().context("node child stdin missing")?;
    let stdout = child.stdout.take().context("node child stdout missing")?;
    let stderr = child.stderr.take().context("node child stderr missing")?;

    let feed_gate = gate.clone();
    let body = &mut *body;
    // The feed owns the child's stdin: dropping it is what closes the pipe,
    // which `shutdown` on a child's stdin does not.
    let feed = async move {
        if let Some(gate) = &feed_gate
            && !gate.wait().await
        {
            // The session ended without starting. The daemon's input was not
            // touched, and the exit says why.
            return anyhow::Ok(());
        }
        if !stdin_prefix.is_empty() {
            stdin.write_all(&stdin_prefix).await?;
        }
        while let Some(chunk) = next_chunk(body).await? {
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
        // Only stdout carries the start marker.
        let gate = gate.clone().filter(|_| tag == FRAME_STDOUT);
        async move {
            let mut buffer = vec![0; READ_CHUNK];
            let mut scan = gate.as_ref().map(|_| StartScan::new());
            let mut passed = Vec::new();
            let result = async {
                loop {
                    let read = pipe.read(&mut buffer).await?;
                    if read == 0 {
                        return anyhow::Ok(());
                    }
                    let bytes = match (&mut scan, &gate) {
                        (Some(scan), Some(gate)) => {
                            passed.clear();
                            if scan.push(&buffer[..read], &mut passed) {
                                gate.mark_started();
                            }
                            if passed.is_empty() {
                                continue;
                            }
                            &passed[..]
                        }
                        _ => &buffer[..read],
                    };
                    if frames.send(frame(tag, bytes)).await.is_err() {
                        return Ok(());
                    }
                }
            }
            .await;
            if let (Some(scan), Some(gate)) = (&mut scan, &gate) {
                passed.clear();
                scan.finish(&mut passed);
                if !passed.is_empty() {
                    let _ = frames.send(frame(tag, &passed)).await;
                }
                gate.mark_ended();
            }
            result
        }
    };
    let out = pump(Box::new(stdout), FRAME_STDOUT);
    let err = pump(Box::new(stderr), FRAME_STDERR);
    // Forward output, and notice a reset even while the child prints nothing:
    // a quiet child must still die when the daemon gives up on it. Stderr
    // frames wait here while the session has not started, because a refused
    // attempt's stderr belongs to the retry, not to the daemon.
    let forward = async {
        let mut held = Vec::<Bytes>::new();
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
                Some(framed) => {
                    let is_stderr = framed.first() == Some(&FRAME_STDERR);
                    if is_stderr {
                        let mut tail = stderr_tail
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tail.extend_from_slice(&framed[5..]);
                        let excess = tail.len().saturating_sub(4096);
                        tail.drain(..excess);
                    }
                    // Stderr waits until the session is known to have started.
                    // A session whose stdout ended with no byte never started:
                    // its stderr is classified with its exit, and the caller
                    // sends it only if the exit is not a refusal.
                    let started = gate
                        .as_ref()
                        .is_none_or(|gate| gate.phase() == crate::targets::StartPhase::Started);
                    if is_stderr && !started {
                        held.push(framed);
                        continue;
                    }
                    for earlier in held.drain(..) {
                        send_all(send, earlier).await?;
                    }
                    send_all(send, framed).await?
                }
                None => return anyhow::Ok(held),
            }
        }
    };

    // The child owns the lifetime of its input. A relay keeps stdin open
    // between requests, including when the proxy has already failed to start.
    // Waiting for request EOF first would hide that exit until the caller's
    // handshake deadline. Keep draining output after the process ends.
    let exited = async {
        let status = tokio::select! {
            status = child.wait() => status.context("wait for node child")?,
            fed = feed => {
                fed.context("feed node child stdin")?;
                child.wait().await.context("wait for node child")?
            }
        };
        if let Some(group) = &group {
            group.kill();
        }
        // Keep the output channel open until the child exits, even if both
        // pipes close first. Its receiver owns reset detection throughout.
        drop(frames_tx);
        anyhow::Ok(status)
    };
    let streamed = tokio::try_join!(
        exited,
        async { out.await.context("read node child stdout") },
        async { err.await.context("read node child stderr") },
        forward,
    );
    let (status, held) = match streamed {
        Ok((status, (), (), held)) => (status, held),
        Err(error) => {
            // A reset or pipe failure must stop the whole process group.
            if let Some(group) = &group {
                group.kill();
            }
            child.kill().await.ok();
            return Err(error);
        }
    };
    let tail = std::mem::take(
        &mut *stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let exit = NodeExit {
        status: status.code(),
        error: None,
    };
    // A session the server turned away tells the ledger what it means: a dead
    // master is rechecked, and a full one sends the retry down the walk. A
    // session that printed the start marker is not refused, whatever its exit
    // status.
    if let (Some(lease), Some(code)) = (&lease, status.code()) {
        let text = String::from_utf8_lossy(&tail);
        let refusal = match &gate {
            Some(gate) => gate.refusal(code, &text),
            None => crate::targets::ssh_refusal(code, &text),
        };
        if let Some(refusal) = refusal {
            let walks_on = lease.note_refusal(refusal);
            if gate.is_some() {
                // Nothing reached the daemon: the caller retries the attempt.
                // The lease is released first, then the process group's
                // leftovers (none for a refused session) are gone with it.
                drop(group);
                return Ok(ChildEnd::Refused(Refused {
                    refusal,
                    walks_on,
                    stderr_tail: tail,
                    held_stderr: held,
                }));
            }
        }
    }
    for held in held {
        send_all(send, held).await?;
    }
    send_all(send, frame(FRAME_EXIT, &serde_json::to_vec(&exit)?)).await?;
    send.send_data(Bytes::new(), true)?;
    // A non-detaching command owns its whole group: the guard kills what the
    // child left behind, as local execution does. The SSH session, a local
    // here, is held until this returns, after the group is gone. (Without
    // connection sharing, on Windows, the lease has no destructor at all.)
    drop(group);
    Ok(ChildEnd::Exited)
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
    pub async fn exec(&self, run: &crate::targets::NodeRun) -> Result<NodeExec> {
        self.open(EXEC_PATH, &serde_json::to_vec(run)?, false).await
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

    /// Read a host file through the node.
    pub async fn fs_read(&self, path: &std::path::Path) -> Result<Vec<u8>> {
        let request = serde_json::to_vec(&FsRequest {
            path: crate::target_path::text(path),
        })?;
        let exec = self.open(FS_READ_PATH, &request, true).await?;
        let output = exec.collect(Bytes::new()).await?;
        fs_stdout(output)
    }

    /// Replace a host file with `data`, streaming through the node.
    pub async fn fs_write(&self, path: &std::path::Path, data: Bytes) -> Result<()> {
        let request = serde_json::to_vec(&FsRequest {
            path: crate::target_path::text(path),
        })?;
        let mut exec = self.open(FS_WRITE_PATH, &request, false).await?;
        exec.write_stdin(data).await?;
        exec.close_stdin()?;
        let output = exec.collect(Bytes::new()).await?;
        fs_stdout(output).map(|_| ())
    }

    /// List a host directory through the node.
    pub async fn fs_list(&self, path: &std::path::Path) -> Result<Vec<FsEntry>> {
        let request = serde_json::to_vec(&FsRequest {
            path: crate::target_path::text(path),
        })?;
        let exec = self.open(FS_LIST_PATH, &request, true).await?;
        let output = exec.collect(Bytes::new()).await?;
        serde_json::from_slice(&fs_stdout(output)?).context("decode node fs listing")
    }

    /// Read host path metadata through the node.
    pub async fn fs_metadata(&self, path: &std::path::Path) -> Result<FsMetadata> {
        let request = serde_json::to_vec(&FsRequest {
            path: crate::target_path::text(path),
        })?;
        let exec = self.open(FS_METADATA_PATH, &request, true).await?;
        let output = exec.collect(Bytes::new()).await?;
        serde_json::from_slice(&fs_stdout(output)?).context("decode node fs metadata")
    }

    /// Tell the node the daemon is stopping for good, and wait for it to
    /// acknowledge.
    pub async fn dismiss(&self) -> Result<()> {
        let mut send_request = self
            .send_request
            .clone()
            .ready()
            .await
            .context("node connection is gone")?;
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://node{SHUTDOWN_PATH}"))
            .body(())
            .expect("static request");
        let (response, _) = send_request.send_request(request, true)?;
        let response = response.await.context("node did not acknowledge")?;
        ensure!(
            response.status() == 200,
            "node answered {}",
            response.status()
        );
        Ok(())
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

impl NodeExec {
    /// Separate stdin from output, so one task can feed the child while
    /// another reads it.
    pub fn split(self) -> (NodeStdin, NodeOutputStream) {
        (
            NodeStdin { stdin: self.stdin },
            NodeOutputStream {
                output: self.output,
                decoder: self.decoder,
            },
        )
    }
}

/// The stdin half of a split [`NodeExec`].
pub struct NodeStdin {
    stdin: Option<h2::SendStream<Bytes>>,
}

impl NodeStdin {
    pub async fn write(&mut self, data: Bytes) -> Result<()> {
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

    pub fn close(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            match stdin.send_data(Bytes::new(), true) {
                Err(error) if error.reason() == Some(h2::Reason::NO_ERROR) => {}
                result => result?,
            }
        }
        Ok(())
    }
}

/// The output half of a split [`NodeExec`].
pub struct NodeOutputStream {
    output: h2::RecvStream,
    decoder: FrameDecoder,
}

impl NodeOutputStream {
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
}

/// Everything a finished exec printed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NodeOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit: Option<NodeExit>,
}

fn fs_stdout(output: NodeOutput) -> Result<Vec<u8>> {
    let exit = output
        .exit
        .context("node fs stream ended without an exit")?;
    if let Some(error) = exit.error {
        bail!("{error}");
    }
    ensure!(
        exit.status == Some(0),
        "node fs operation failed with status {:?}: {}",
        exit.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

/// The daemon's [`crate::targets::CommandHost`]: every command goes to the
/// node that serves this computer's work. Which node that is has one owner,
/// the [`NodeSlot`] the daemon's node registry publishes into.
pub struct NodeCommandHost {
    slot: tokio::sync::watch::Receiver<Option<NodeClient>>,
    runtime: tokio::runtime::Handle,
    shutdown: tokio_util::sync::CancellationToken,
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
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            slot,
            runtime,
            shutdown,
        }
    }

    async fn connected(&self) -> Result<NodeClient> {
        let mut slot = self.slot.clone();
        let connected = async {
            loop {
                // Cleanup may still use a connected node during shutdown,
                // but must never wait for an absent node to arrive.
                if let Some(client) = slot.borrow_and_update().clone() {
                    return Ok(client);
                }
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => {
                        bail!("daemon shutdown cancelled the wait for a node connection");
                    }
                    changed = slot.changed() => {
                        changed.context("the daemon's node registry has stopped")?;
                    }
                }
            }
        };
        let waited = tokio::time::timeout(NODE_CONNECT_WAIT, connected).await;
        match waited {
            Ok(result) => result,
            Err(_) => bail!(
                "no node from this computer is connected, so nothing can run here; \
                 `mj` starts one beside the daemon"
            ),
        }
    }
}

impl crate::targets::CommandHost for NodeCommandHost {
    fn open_stream<'a>(
        &'a self,
        command: &crate::targets::CommandSpec,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<NodeExec>> + Send + 'a>> {
        let run = command.to_node_run(true);
        Box::pin(async move { self.connected().await?.exec(&run).await })
    }

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
        tokio::spawn(async move { serve(node_side).await.map(|_| ()) });
        let (client, driver) = NodeClient::handshake(daemon_side).await.unwrap();
        tokio::spawn(driver);
        client
    }

    fn sh(script: &str) -> crate::targets::NodeRun {
        crate::targets::CommandSpec::new("sh", ["-c", script]).to_node_run(true)
    }

    // Hard-won: 413864386: an exited relay proxy waited for stdin EOF before reporting its failure.
    #[tokio::test]
    async fn failed_exec_reports_all_output_and_exit_while_stdin_remains_open() {
        let client = connected().await;
        let mut exec = client
            .exec(&sh("head -c 300000 /dev/zero >&2; exit 23"))
            .await
            .unwrap();
        let mut stderr = Vec::new();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match exec.next().await.unwrap().expect("exit frame") {
                    NodeFrame::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                    NodeFrame::Stdout(_) => panic!("the failing proxy only writes stderr"),
                    NodeFrame::Exit(exit) => break exit,
                }
            }
        })
        .await
        .expect("an exited proxy must not wait for its caller to close stdin");
        assert_eq!(exit.status, Some(23));
        assert_eq!(exit.error, None);
        assert_eq!(stderr, vec![0; 300000]);
    }

    /// Host files cross the node as streams, not as one oversized frame, and
    /// metadata and listings use the same path conversion.
    #[tokio::test]
    async fn host_files_stream_through_the_node_in_both_directions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.bin");
        let payload = (0..300_000_u32)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        tokio::fs::write(&path, &payload).await.unwrap();

        let client = connected().await;
        assert_eq!(client.fs_read(&path).await.unwrap(), payload);

        let written = directory.path().join("written.bin");
        client
            .fs_write(&written, Bytes::from(payload.clone()))
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&written).await.unwrap(), payload);

        let entries = client.fs_list(directory.path()).await.unwrap();
        assert!(entries.iter().any(|entry| entry.name == "payload.bin"));
        assert!(entries.iter().all(|entry| entry.name != ".."));

        let metadata = client.fs_metadata(&path).await.unwrap();
        assert_eq!(metadata.len, payload.len() as u64);
        assert!(metadata.is_file);
        assert!(!metadata.is_dir);
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
        for closed_pipes in [false, true] {
            let marker = directory.path().join(format!("grandchild-{closed_pipes}"));
            let close = if closed_pipes { "exec 1>&- 2>&-;" } else { "" };
            let script = format!(
                "(sleep 3; touch {}) >/dev/null 2>&1 & echo started; {close} wait",
                marker.display()
            );
            let mut exec = client.exec(&sh(&script)).await.unwrap();
            exec.close_stdin().unwrap();
            assert_eq!(
                exec.next().await.unwrap(),
                Some(NodeFrame::Stdout(Bytes::from_static(b"started\n")))
            );
            // Allow both output pumps to reach EOF before resetting a child
            // that is still running with closed pipes.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(exec);
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            assert!(!marker.exists(), "the grandchild was killed with its group");
        }
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
        let host = NodeCommandHost::new(
            receiver,
            tokio::runtime::Handle::current(),
            tokio_util::sync::CancellationToken::new(),
        );
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
        let shutdown = tokio_util::sync::CancellationToken::new();
        let host = NodeCommandHost::new(
            receiver,
            tokio::runtime::Handle::current(),
            shutdown.clone(),
        );
        let errors = tokio::task::spawn_blocking(move || {
            let command = CommandSpec::new("/nonexistent/podman", ["ps"]);
            let closed = host.run(&command, None, &|| false);
            // Shutdown stops waiting for an absent node, while cleanup on
            // the already connected node still reports its own result.
            shutdown.cancel();
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

    /// A relay on the node is refused by the SSH server several times. The
    /// node, where the lease lives, retries; it holds the daemon's input and
    /// the refused attempts' stderr back, so the daemon sees one clean run.
    // Hard-won: 5cb64e019: the daemon counted each refusal of a node-hosted relay as unproductive and gave up after three.
    // Hard-won: 75da0c2b0: stderr that arrived after stdout had ended was flushed to the daemon before the refused attempt was classified.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // The sharing override is process-wide.
    async fn a_node_stream_refused_repeatedly_runs_once_without_the_daemon_seeing_it() {
        use crate::targets::fake_ssh::{FakeSsh, FastRetries};
        use crate::targets::{SshSharingForTest, set_ssh_connection_sharing_for_test};

        let _guard = crate::targets::SHARING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_ssh_connection_sharing_for_test(Some(SshSharingForTest::Disabled));
        let _fast_retries = FastRetries::new();
        let fake = FakeSsh::new(&[
            "refuse-session-late-stderr",
            "refuse-session",
            "refuse-auth",
            "refuse-auth",
            "refuse-auth",
        ]);
        let client = connected().await;
        let exec = client
            .exec(&fake.command(&["wc", "-c"]).to_node_run(true))
            .await
            .unwrap();
        let (mut stdin, mut output) = exec.split();
        // More than the pipes and the stream window buffer.
        let payload = Bytes::from(vec![b'y'; 600 * 1024]);
        let length = payload.len();
        let feed = tokio::spawn(async move {
            for chunk in payload.chunks(64 * 1024) {
                stdin.write(Bytes::copy_from_slice(chunk)).await.unwrap();
            }
            stdin.close().unwrap();
        });
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let exit = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match output.next().await.unwrap().expect("exit frame") {
                    NodeFrame::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                    NodeFrame::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                    NodeFrame::Exit(exit) => break exit,
                }
            }
        })
        .await
        .expect("the node retries until the session starts");
        feed.await.unwrap();
        set_ssh_connection_sharing_for_test(None);
        assert_eq!(exit.status, Some(0), "{}", String::from_utf8_lossy(&stderr));
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), length.to_string());
        assert_eq!(stderr, b"", "refused attempts print nothing to the daemon");
        assert_eq!(fake.attempts(), 6);
    }
}
