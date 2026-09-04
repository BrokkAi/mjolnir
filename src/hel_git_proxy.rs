//! Authenticated, path-confined Git smart-protocol bridge for local bundles.

#[cfg(feature = "controller")]
use std::collections::BTreeMap;
#[cfg(feature = "controller")]
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
#[cfg(feature = "controller")]
use std::io::Write as _;
use std::path::Path;
#[cfg(feature = "controller")]
use std::path::PathBuf;
#[cfg(feature = "controller")]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(feature = "controller")]
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "controller")]
use crate::hel_local_git::canonical_repository;
#[cfg(feature = "controller")]
use crate::hel_targets::CommandSpec;

const BRIDGE_MAGIC: &[u8] = b"HEL-GIT-BRIDGE-2\n";
const MAX_FRAME: usize = 1024 * 1024;
const MAX_OPEN: usize = 16 * 1024;
/// How long a broker waits for its target bridge to exit once the frame
/// stream between them has closed.
#[cfg(feature = "controller")]
const BRIDGE_EXIT_GRACE: Duration = Duration::from_secs(5);
/// How long a client may take to name its repository and service.
///
/// The proxy writes its open line as soon as it connects, so a connection that
/// is still silent this much later is one that will never speak. Exchanges are
/// served one at a time, so without this deadline such a client holds the
/// session's whole bridge.
#[cfg(unix)]
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);
/// How long one exchange may move nothing in either direction before the
/// bridge gives up on it.
///
/// The deadline is idle rather than total because a legitimate transfer can
/// run for a very long time while still making progress — a huge clone is slow
/// but never silent. Only the longest legitimately quiet phase of a Git
/// service has to fit inside the window: counting and compressing objects
/// before the first pack byte, or indexing a pushed pack before the report.
/// Five minutes covers that for very large repositories while still turning a
/// wedged client into a reported failure in minutes instead of never.
const EXCHANGE_IDLE_DEADLINE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "controller")]
pub struct GitBrokerSpec {
    pub session_id: String,
    pub bridge: CommandSpec,
    pub repositories: BTreeMap<String, PathBuf>,
    pub ready_path: PathBuf,
    pub pid_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitOpen {
    repository: String,
    service: String,
}

#[cfg(feature = "controller")]
impl GitBrokerSpec {
    pub fn write(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        crate::hel_config::atomic_write(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read Git broker spec {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse Git broker spec {}", path.display()))
    }
}

/// What a PID file says about the broker that wrote it.
#[cfg(feature = "controller")]
enum BrokerLock {
    /// A broker holds the lock; the file names its process when it is
    /// readable, which it is for every broker past its own startup.
    Held(Option<i32>),
    /// Nobody holds the lock, so no broker is serving this session.
    Free,
}

/// Read a broker's PID file through the lock its owner holds.
///
/// A live broker holds an exclusive advisory lock on its PID file for as long
/// as it runs, so liveness is the lock rather than the number written in the
/// file: a PID file left behind by a killed broker, or one whose PID the
/// system has since handed to an unrelated process, reads as dead and is
/// restarted instead of being trusted, blocking the session forever, or —
/// worse — being signalled.
#[cfg(feature = "controller")]
fn broker_lock(pid_path: &Path) -> BrokerLock {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(pid_path) else {
        return BrokerLock::Free;
    };
    match file.try_lock() {
        // Nobody holds the lock, so the broker that wrote this file is gone.
        Ok(()) => {
            let _ = file.unlock();
            BrokerLock::Free
        }
        Err(TryLockError::WouldBlock) => BrokerLock::Held(
            std::fs::read_to_string(pid_path)
                .ok()
                .and_then(|pid| pid.trim().parse().ok()),
        ),
        Err(TryLockError::Error(error)) => {
            tracing::warn!(
                path = %pid_path.display(),
                error = %error,
                "could not test the Git broker lock; treating the broker as gone"
            );
            BrokerLock::Free
        }
    }
}

/// Whether a broker process still owns this session's bridge.
#[cfg(feature = "controller")]
pub fn broker_is_alive(pid_path: &Path) -> bool {
    matches!(broker_lock(pid_path), BrokerLock::Held(_))
}

/// The process ID of the broker serving this session, when one is running.
///
/// Only a broker that still holds its lock is named, so a caller that stops a
/// broker can never signal a PID the system has reassigned.
#[cfg(feature = "controller")]
pub fn running_broker_pid(pid_path: &Path) -> Option<i32> {
    match broker_lock(pid_path) {
        BrokerLock::Held(pid) => pid,
        BrokerLock::Free => None,
    }
}

/// Take ownership of this session's broker slot for the life of the process.
///
/// Visible to the crate so tests can stand in for a broker exactly as one
/// behaves: holding this lock is what makes a process the session's broker.
#[doc(hidden)]
#[cfg(feature = "controller")]
pub fn claim_broker_pid_file(pid_path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(pid_path)
        .with_context(|| format!("open Git broker lock {}", pid_path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!(
            "another local Git broker already owns {}",
            pid_path.display()
        ),
        Err(TryLockError::Error(error)) => {
            return Err(error)
                .with_context(|| format!("lock Git broker file {}", pid_path.display()));
        }
    }
    file.set_len(0)?;
    file.write_all(std::process::id().to_string().as_bytes())?;
    file.flush()?;
    Ok(file)
}

#[cfg(feature = "controller")]
pub async fn run_broker(spec_path: &Path) -> Result<()> {
    let spec = GitBrokerSpec::read(spec_path)?;
    let repositories = spec
        .repositories
        .iter()
        .map(|(id, path)| Ok((id.clone(), canonical_repository(path)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    if let Some(parent) = spec.ready_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Held until this process exits: it is what `broker_is_alive` observes.
    let pid_file = claim_broker_pid_file(&spec.pid_path)?;
    let result = run_bridge_process(&spec, repositories).await;
    for path in [&spec.ready_path, &spec.pid_path] {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove Git broker lifecycle marker"
            );
        }
    }
    drop(pid_file);
    result
}

#[cfg(feature = "controller")]
async fn run_bridge_process(
    spec: &GitBrokerSpec,
    repositories: BTreeMap<String, PathBuf>,
) -> Result<()> {
    let mut child = Command::new(&spec.bridge.program)
        .args(&spec.bridge.args)
        .envs(&spec.bridge.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start {}", spec.bridge.purpose))?;
    let mut input = child.stdin.take().context("Git bridge stdin is missing")?;
    let mut output = child
        .stdout
        .take()
        .context("Git bridge stdout is missing")?;
    let mut magic = vec![0; BRIDGE_MAGIC.len()];
    output
        .read_exact(&mut magic)
        .await
        .context("read Git bridge greeting")?;
    ensure!(magic == BRIDGE_MAGIC, "target Git bridge version mismatch");
    crate::hel_config::atomic_write(&spec.ready_path, b"ready\n")?;

    let outcome = serve_bridge(
        &mut input,
        &mut output,
        &repositories,
        &spec.session_id,
        EXCHANGE_IDLE_DEADLINE,
    )
    .await;
    // Closing the frame stream is how an idle target bridge learns that its
    // broker is finished with it.
    drop(input);
    drop(output);
    let status = match tokio::time::timeout(BRIDGE_EXIT_GRACE, child.wait()).await {
        Ok(status) => Some(status.context("wait for target Git bridge")?),
        Err(_) => {
            if let Err(error) = child.kill().await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    session_id = %spec.session_id,
                    %error,
                    "could not terminate a timed-out target Git bridge"
                );
            }
            None
        }
    };
    outcome?;
    let status = status.context("target Git bridge did not exit after its stream closed")?;
    ensure!(status.success(), "target Git bridge exited with {status}");
    Ok(())
}

/// Outcome of one bridged Git exchange whose frame stream is still in sync.
///
/// A failed exchange costs its own connection and nothing else, so both loops
/// report it and keep serving. A frame stream that can no longer be
/// interpreted is returned as `Err` instead, because no later exchange could
/// be framed correctly after it.
#[must_use]
enum Exchange {
    Completed,
    Failed(anyhow::Error),
}

/// Idle watchdog for one exchange, shared by both halves of its transfer.
///
/// Every frame and every byte moved in either direction marks progress. When
/// nothing moves for a whole idle window the exchange is aborted, which is how
/// a client that wedges mid-transfer becomes its own connection's failure
/// instead of a bridge that serves nobody again.
struct ExchangeWatch {
    idle: Duration,
    start: Instant,
    /// Milliseconds after `start` at which progress was last marked.
    progress: AtomicU64,
    abort: CancellationToken,
}

impl ExchangeWatch {
    fn new(idle: Duration) -> Self {
        Self {
            idle,
            start: Instant::now(),
            progress: AtomicU64::new(0),
            abort: CancellationToken::new(),
        }
    }

    /// Record that this exchange moved something, in either direction.
    fn mark(&self) {
        let elapsed = self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.progress.store(elapsed, Ordering::Relaxed);
    }

    /// Resolve once no progress has been marked for a whole idle window.
    async fn stalled(&self) {
        loop {
            let progress = self.progress.load(Ordering::Relaxed);
            let deadline = self.start + Duration::from_millis(progress) + self.idle;
            tokio::time::sleep_until(deadline.into()).await;
            if self.progress.load(Ordering::Relaxed) == progress {
                return;
            }
        }
    }

    /// Give up on this exchange: both halves stop touching their own endpoint
    /// and finish their framing. The idle window restarts, so the peer still
    /// gets a full one to end its half of the exchange.
    fn abort(&self) {
        self.mark();
        self.abort.cancel();
    }

    fn is_aborted(&self) -> bool {
        self.abort.is_cancelled()
    }

    /// Resolve once this exchange has been given up on.
    async fn aborted(&self) {
        self.abort.cancelled().await;
    }

    fn stall(&self) -> anyhow::Error {
        anyhow!(
            "a bridged Git exchange moved nothing for {} seconds",
            self.idle.as_secs_f64()
        )
    }
}

/// Run one exchange's transfer under its idle watchdog.
///
/// The watchdog never drops the transfer: a half dropped mid-frame would leave
/// a partial frame in the stream and desynchronise every exchange after it.
/// The first idle window aborts the transfer instead, which makes both halves
/// let go of their own endpoint and still write the framing the peer is
/// reading for. Only a second idle window is fatal, because a peer that never
/// ends its half of an aborted exchange leaves a frame stream that can no
/// longer be interpreted.
///
/// Returns the transfer's own result together with the stall that aborted it,
/// if one did.
async fn watch_exchange<T>(
    watch: &ExchangeWatch,
    transfer: impl Future<Output = Result<T>>,
) -> Result<(T, Option<anyhow::Error>)> {
    tokio::pin!(transfer);
    tokio::select! {
        result = &mut transfer => Ok((result?, None)),
        () = watch.stalled() => {
            watch.abort();
            tokio::select! {
                result = &mut transfer => Ok((result?, Some(watch.stall()))),
                () = watch.stalled() => Err(watch
                    .stall()
                    .context("a stalled Git bridge exchange was never ended by its peer")),
            }
        }
    }
}

/// Serve bridged Git exchanges until the target bridge closes its stream.
#[cfg(feature = "controller")]
async fn serve_bridge(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncRead + Unpin),
    repositories: &BTreeMap<String, PathBuf>,
    session_id: &str,
    idle: Duration,
) -> Result<()> {
    loop {
        let open = match read_frame(output, MAX_OPEN).await? {
            Frame::Data(open) => open,
            // The target bridge is gone: no further exchange is possible.
            Frame::Closed => return Ok(()),
            Frame::End => {
                tracing::warn!(
                    session_id,
                    "target Git bridge ended an exchange that was not open"
                );
                continue;
            }
        };
        match serve_exchange(input, output, repositories, &open, idle).await? {
            Exchange::Completed => {}
            Exchange::Failed(error) => tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "bridged Git exchange failed"
            ),
        }
    }
}

#[cfg(feature = "controller")]
async fn serve_exchange(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncRead + Unpin),
    repositories: &BTreeMap<String, PathBuf>,
    open: &[u8],
    idle: Duration,
) -> Result<Exchange> {
    let request: GitOpen = match serde_json::from_slice(open) {
        Ok(request) => request,
        Err(error) => {
            let error = anyhow!(error).context("decode Git bridge request");
            return refuse_exchange(input, output, error).await;
        }
    };
    let Some(repository) = repositories.get(&request.repository) else {
        let error = anyhow!(
            "Git bridge requested unknown repository {:?}",
            request.repository
        );
        return refuse_exchange(input, output, error).await;
    };
    let command = match git_service(&request.service) {
        Ok(command) => command,
        Err(error) => return refuse_exchange(input, output, error).await,
    };
    serve_git(input, output, repository, command, idle).await
}

#[cfg(feature = "controller")]
fn git_service(service: &str) -> Result<&'static str> {
    match service {
        "git-upload-pack" => Ok("upload-pack"),
        "git-receive-pack" => Ok("receive-pack"),
        _ => bail!("unsupported Git service {service:?}"),
    }
}

/// Refuse one exchange without losing the frame stream: end this side of it,
/// then read the target's side through to its end frame.
#[cfg(feature = "controller")]
async fn refuse_exchange(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncRead + Unpin),
    error: anyhow::Error,
) -> Result<Exchange> {
    write_frame(input, &[]).await?;
    drain_exchange(output).await?;
    Ok(Exchange::Failed(error))
}

/// Read the peer's remaining frames for the exchange in progress.
#[cfg(feature = "controller")]
async fn drain_exchange(output: &mut (impl AsyncRead + Unpin)) -> Result<()> {
    loop {
        match read_frame(output, MAX_FRAME).await? {
            Frame::Data(_) => {}
            Frame::End => return Ok(()),
            Frame::Closed => bail!("the target Git bridge closed during an exchange"),
        }
    }
}

#[cfg(feature = "controller")]
async fn serve_git<W, R>(
    bridge_input: &mut W,
    bridge_output: &mut R,
    repository: &Path,
    command: &str,
    idle: Duration,
) -> Result<Exchange>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let mut git = Command::new("git");
    git.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "receive.denyCurrentBranch=updateInstead",
        "-c",
        "receive.denyNonFastForwards=true",
        "-c",
        "receive.denyDeletes=true",
        command,
    ]);
    git.arg(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut git = match git.spawn().with_context(|| format!("start git {command}")) {
        Ok(git) => git,
        Err(error) => return refuse_exchange(bridge_input, bridge_output, error).await,
    };
    let (Some(mut git_input), Some(mut git_output)) = (git.stdin.take(), git.stdout.take()) else {
        if let Err(kill_error) = git.kill().await {
            tracing::warn!(%kill_error, "could not stop Git service with missing pipes");
        }
        let error = anyhow!("git {command} was started without both pipes");
        return refuse_exchange(bridge_input, bridge_output, error).await;
    };

    let watch = ExchangeWatch::new(idle);
    let watch = &watch;
    let to_target = async {
        let mut buffer = vec![0; 64 * 1024];
        let mut failure = None;
        loop {
            let count = tokio::select! {
                biased;
                // The watchdog gave up on this exchange: let go of a service
                // that has stopped moving and end our half of the stream.
                () = watch.aborted() => break,
                // `read` is cancel-safe, and every frame is written outside
                // the select, so no partial frame can reach the target.
                result = git_output.read(&mut buffer) => match result {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) => {
                        failure = Some(anyhow!(error).context(format!("read git {command} output")));
                        break;
                    }
                },
            };
            watch.mark();
            write_frame(bridge_input, &buffer[..count]).await?;
        }
        // Exactly one end frame per exchange, on every path: the target reads
        // until it arrives.
        write_frame(bridge_input, &[]).await?;
        Ok::<_, anyhow::Error>(failure)
    };
    let from_target = async move {
        loop {
            match read_frame(bridge_output, MAX_FRAME).await? {
                // A service that has already exited must not stop the drain:
                // the frame stream stays in sync only if every frame of this
                // exchange is read. Writing to the service is local to this
                // exchange, so the watchdog may abandon a write; reading the
                // shared frame stream never stops short of the end frame.
                Frame::Data(frame) => {
                    watch.mark();
                    if !watch.is_aborted() {
                        tokio::select! {
                            biased;
                            () = watch.aborted() => {}
                            result = git_input.write_all(&frame) => {
                                let _ = result;
                            }
                        }
                    }
                }
                Frame::End => break,
                Frame::Closed => bail!("the target Git bridge closed mid-exchange"),
            }
        }
        // Closing the handle is what ends the service's input: shutting a
        // child's stdin down leaves the pipe open, and Git would wait on it
        // forever.
        drop(git_input);
        Ok::<_, anyhow::Error>(())
    };
    // Both halves always run to completion, so one interrupted transfer can
    // never leave unread frames in front of the next exchange.
    let ((service_failure, ()), stalled) =
        watch_exchange(watch, async { tokio::try_join!(to_target, from_target) }).await?;
    if let Some(stalled) = stalled {
        // A service that outlived its exchange would hold the repository and
        // its pipes for as long as it liked.
        if let Err(kill_error) = git.kill().await {
            tracing::warn!(
                service = command,
                %kill_error,
                "could not stop stalled Git service"
            );
        }
        return Ok(Exchange::Failed(
            stalled.context(format!("git {command} stalled")),
        ));
    }
    if let Some(failure) = service_failure {
        if let Err(kill_error) = git.kill().await {
            tracing::warn!(
                service = command,
                %kill_error,
                "could not stop failed Git service"
            );
        }
        return Ok(Exchange::Failed(failure));
    }
    match git.wait().await {
        Ok(status) if status.success() => Ok(Exchange::Completed),
        Ok(status) => Ok(Exchange::Failed(anyhow!(
            "git {command} exited with {status}"
        ))),
        Err(error) => Ok(Exchange::Failed(
            anyhow!(error).context("wait for Git service"),
        )),
    }
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), data: &[u8]) -> Result<()> {
    ensure!(data.len() <= MAX_FRAME, "Git bridge frame is too large");
    writer.write_u32(data.len() as u32).await?;
    writer.write_all(data).await?;
    writer.flush().await?;
    Ok(())
}

/// One read from a bridge frame stream.
enum Frame {
    /// Payload bytes belonging to the exchange in progress.
    Data(Vec<u8>),
    /// The peer finished its half of the exchange in progress.
    End,
    /// The peer closed the stream: no further exchange is possible.
    Closed,
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin), maximum: usize) -> Result<Frame> {
    let length = match reader.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(Frame::Closed);
        }
        Err(error) => return Err(error.into()),
    };
    if length == 0 {
        return Ok(Frame::End);
    }
    ensure!(length <= maximum, "Git bridge frame is too large");
    let mut data = vec![0; length];
    reader.read_exact(&mut data).await?;
    Ok(Frame::Data(data))
}

#[cfg(unix)]
pub async fn run_worker_bridge(root: &Path) -> Result<()> {
    run_worker_bridge_over(
        root,
        tokio::io::stdin(),
        tokio::io::stdout(),
        HANDSHAKE_DEADLINE,
        EXCHANGE_IDLE_DEADLINE,
    )
    .await
}

#[cfg(unix)]
async fn run_worker_bridge_over(
    root: &Path,
    mut broker_input: impl AsyncRead + Unpin,
    mut broker_output: impl AsyncWrite + Unpin,
    handshake: Duration,
    idle: Duration,
) -> Result<()> {
    use tokio::net::UnixListener;

    std::fs::create_dir_all(root)?;
    let socket = root.join("git.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("remove stale Git proxy socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind Git proxy socket {}", socket.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    broker_output.write_all(BRIDGE_MAGIC).await?;
    broker_output.flush().await?;
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => accepted.context("accept Git proxy client")?.0,
            // Between exchanges the broker sends nothing, so the only thing
            // this read can report is the broker going away. Nothing is in
            // flight to lose when the branch that is not taken is dropped.
            frame = read_frame(&mut broker_input, MAX_FRAME) => match frame? {
                Frame::Closed => return Ok(()),
                _ => bail!("Git broker sent a frame between exchanges"),
            },
        };
        match serve_client(
            stream,
            &mut broker_input,
            &mut broker_output,
            handshake,
            idle,
        )
        .await?
        {
            Exchange::Completed => {}
            Exchange::Failed(error) => {
                tracing::warn!(error = format!("{error:#}"), "bridged Git client failed")
            }
        }
    }
}

/// Bridge one accepted client through the broker's frame stream.
#[cfg(unix)]
async fn serve_client(
    stream: tokio::net::UnixStream,
    broker_input: &mut (impl AsyncRead + Unpin),
    broker_output: &mut (impl AsyncWrite + Unpin),
    handshake: Duration,
    idle: Duration,
) -> Result<Exchange> {
    let (mut read, mut write) = stream.into_split();
    // Nothing has been framed upstream yet, so a client that dies — or never
    // speaks — during its handshake costs the bridge nothing but this wait.
    let open = match tokio::time::timeout(handshake, read_handshake(&mut read)).await {
        Ok(Ok(open)) => open,
        Ok(Err(error)) => return Ok(Exchange::Failed(error)),
        Err(_) => {
            return Ok(Exchange::Failed(anyhow!(
                "a Git proxy client sent no handshake within {} seconds",
                handshake.as_secs_f64()
            )));
        }
    };
    write_frame(broker_output, &open).await?;

    let watch = ExchangeWatch::new(idle);
    let watch = &watch;
    let served = tokio::sync::Notify::new();
    let to_broker = async {
        let mut buffer = vec![0; 64 * 1024];
        let mut failure = None;
        loop {
            let count = tokio::select! {
                biased;
                // The service finished, so stop reading a client that may
                // never hang up instead of holding the exchange open.
                () = served.notified() => break,
                // The watchdog gave up on this exchange: let go of a client
                // that has stopped moving and end our half of the stream.
                () = watch.aborted() => break,
                // `read` is cancel-safe, and every frame is written outside
                // the select, so no partial frame can reach the broker.
                result = read.read(&mut buffer) => match result {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) => {
                        failure = Some(anyhow!(error).context("read Git proxy client"));
                        break;
                    }
                },
            };
            watch.mark();
            write_frame(broker_output, &buffer[..count]).await?;
        }
        // Exactly one end frame per exchange, on every path.
        write_frame(broker_output, &[]).await?;
        Ok::<_, anyhow::Error>(failure)
    };
    let from_broker = async {
        loop {
            match read_frame(broker_input, MAX_FRAME).await? {
                // A client that has gone away must not stop the drain: the
                // frame stream stays in sync only if every frame of this
                // exchange is read. Writing to the client is local to this
                // exchange, so the watchdog may abandon a write; reading the
                // shared frame stream never stops short of the end frame.
                Frame::Data(frame) => {
                    watch.mark();
                    if !watch.is_aborted() {
                        tokio::select! {
                            biased;
                            () = watch.aborted() => {}
                            result = write.write_all(&frame) => {
                                let _ = result;
                            }
                        }
                    }
                }
                Frame::End => break,
                Frame::Closed => bail!("the Git broker closed the bridge"),
            }
        }
        if let Err(error) = write.shutdown().await {
            tracing::debug!(%error, "Git proxy client closed before its response was flushed");
        }
        served.notify_one();
        Ok::<_, anyhow::Error>(())
    };
    let (client_failure, stalled) = watch_exchange(watch, async {
        let (failure, ()) = tokio::try_join!(to_broker, from_broker)?;
        Ok(failure)
    })
    .await?;
    Ok(match stalled.or(client_failure) {
        Some(error) => Exchange::Failed(error),
        None => Exchange::Completed,
    })
}

#[cfg(unix)]
pub async fn run_worker_proxy(root: &Path, repository: &str, service: &str) -> Result<()> {
    use tokio::net::UnixStream;

    crate::hel_config::validate_id("repository", repository)?;
    ensure!(
        matches!(service, "git-upload-pack" | "git-receive-pack"),
        "unsupported Git service"
    );
    let mut socket = UnixStream::connect(root.join("git.sock"))
        .await
        .with_context(|| format!("connect Git bridge at {}", root.display()))?;
    let open = serde_json::to_vec(&GitOpen {
        repository: repository.into(),
        service: service.into(),
    })?;
    socket.write_all(&open).await?;
    socket.write_all(b"\n").await?;
    let (mut socket_read, mut socket_write) = socket.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let from_git = async {
        tokio::io::copy(&mut stdin, &mut socket_write).await?;
        socket_write.shutdown().await
    };
    let to_git = async {
        tokio::io::copy(&mut socket_read, &mut stdout).await?;
        stdout.shutdown().await
    };
    tokio::pin!(from_git);
    tokio::pin!(to_git);
    tokio::select! {
        result = &mut to_git => {
            result?;
        }
        result = &mut from_git => {
            result?;
            to_git.await?;
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn read_handshake(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    loop {
        ensure!(data.len() < MAX_OPEN, "Git proxy handshake is too large");
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        data.push(byte);
    }
    Ok(data)
}

#[cfg(not(unix))]
pub async fn run_worker_bridge(_root: &Path) -> Result<()> {
    bail!("Git proxy workers require Unix")
}

#[cfg(not(unix))]
pub async fn run_worker_proxy(_root: &Path, _repository: &str, _service: &str) -> Result<()> {
    bail!("Git proxy workers require Unix")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one exchange's frames through the peer's end frame.
    async fn read_exchange(reader: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
        let mut received = Vec::new();
        loop {
            match read_frame(reader, MAX_FRAME).await.unwrap() {
                Frame::Data(frame) => received.extend_from_slice(&frame),
                Frame::End => return received,
                Frame::Closed => panic!("the bridge stream closed mid-exchange"),
            }
        }
    }

    async fn read_data_frame(reader: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
        match read_frame(reader, MAX_FRAME).await.unwrap() {
            Frame::Data(frame) => frame,
            Frame::End => panic!("expected a data frame, not the end of an exchange"),
            Frame::Closed => panic!("expected a data frame, not a closed stream"),
        }
    }

    #[cfg(unix)]
    async fn write_all_framed(writer: &mut (impl AsyncWrite + Unpin), data: &[u8]) {
        for chunk in data.chunks(64 * 1024) {
            write_frame(writer, chunk).await.unwrap();
        }
        write_frame(writer, &[]).await.unwrap();
    }

    /// What a client writes to the proxy socket to open its exchange.
    #[cfg(unix)]
    fn handshake_line(repository: &str) -> Vec<u8> {
        let mut line = open_frame(repository);
        line.push(b'\n');
        line
    }

    fn open_frame(repository: &str) -> Vec<u8> {
        serde_json::to_vec(&GitOpen {
            repository: repository.into(),
            service: "git-upload-pack".into(),
        })
        .unwrap()
    }

    fn git(directory: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(directory)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// A repository whose ref advertisement is far larger than one pipe
    /// buffer, so a bridged transfer really has to stream.
    fn repository_with_large_advertisement(root: &Path) -> PathBuf {
        let repository = root.join("main");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        git(&repository, &["config", "user.name", "Hel Test"]);
        git(&repository, &["config", "user.email", "hel@example.test"]);
        std::fs::write(repository.join("tracked"), "content").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-qm", "base"]);
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repository)
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap().trim().to_owned();
        let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
        packed.push_str(&format!("{head} refs/heads/main\n"));
        for index in 0..3000 {
            packed.push_str(&format!("{head} refs/tags/advertised-{index:04}\n"));
        }
        std::fs::write(repository.join(".git/packed-refs"), packed).unwrap();
        repository
    }

    /// A refused request, a failing Git service, and a transfer the target
    /// abandons midway are all one exchange's failure: the broker keeps
    /// serving the exchanges that follow them.
    #[tokio::test]
    async fn the_broker_serves_later_exchanges_after_one_fails_mid_transfer() {
        let directory = tempfile::tempdir().unwrap();
        let repository = repository_with_large_advertisement(directory.path());
        let not_a_repository = directory.path().join("not-a-repository");
        std::fs::create_dir_all(&not_a_repository).unwrap();
        let repositories = BTreeMap::from([
            ("main".to_owned(), repository),
            ("broken".to_owned(), not_a_repository),
        ]);

        let (broker_end, target_end) = tokio::io::duplex(16 * 1024);
        let (mut broker_read, mut broker_write) = tokio::io::split(broker_end);
        let (mut target_read, mut target_write) = tokio::io::split(target_end);
        let serving = tokio::spawn(async move {
            serve_bridge(
                &mut broker_write,
                &mut broker_read,
                &repositories,
                "test",
                EXCHANGE_IDLE_DEADLINE,
            )
            .await
        });

        // An unknown repository is refused, not fatal.
        write_frame(&mut target_write, &open_frame("absent"))
            .await
            .unwrap();
        write_frame(&mut target_write, &[]).await.unwrap();
        assert!(read_exchange(&mut target_read).await.is_empty());

        // A Git service that exits non-zero is refused the same way.
        write_frame(&mut target_write, &open_frame("broken"))
            .await
            .unwrap();
        write_frame(&mut target_write, &[]).await.unwrap();
        assert!(read_exchange(&mut target_read).await.is_empty());

        // A client that hangs up midway through a large transfer leaves the
        // frame stream in sync for the next exchange.
        write_frame(&mut target_write, &open_frame("main"))
            .await
            .unwrap();
        let mut abandoned = read_data_frame(&mut target_read).await.len();
        while abandoned <= 64 * 1024 {
            abandoned += read_data_frame(&mut target_read).await.len();
        }
        write_frame(&mut target_write, &[]).await.unwrap();
        let remainder = read_exchange(&mut target_read).await;
        assert!(abandoned + remainder.len() > 64 * 1024);

        // The bridge still serves a complete exchange afterwards.
        write_frame(&mut target_write, &open_frame("main"))
            .await
            .unwrap();
        // A client that wants nothing sends a flush packet and hangs up.
        write_frame(&mut target_write, b"0000").await.unwrap();
        write_frame(&mut target_write, &[]).await.unwrap();
        let advertisement = read_exchange(&mut target_read).await;
        assert!(
            advertisement.len() > 64 * 1024,
            "advertisement was {} bytes",
            advertisement.len()
        );
        assert!(String::from_utf8_lossy(&advertisement).contains("refs/heads/main"));

        // Both halves have to go for the stream itself to close.
        drop((target_read, target_write));
        serving.await.unwrap().unwrap();
    }

    /// A client that dies mid-transfer must cost its own connection only.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_worker_bridge_serves_the_next_client_after_one_dies_mid_transfer() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let (worker_end, broker_end) = tokio::io::duplex(16 * 1024);
        let (worker_read, worker_write) = tokio::io::split(worker_end);
        let (mut broker_read, mut broker_write) = tokio::io::split(broker_end);
        let serving = tokio::spawn(async move {
            run_worker_bridge_over(
                &root,
                worker_read,
                worker_write,
                HANDSHAKE_DEADLINE,
                EXCHANGE_IDLE_DEADLINE,
            )
            .await
        });

        let mut magic = vec![0; BRIDGE_MAGIC.len()];
        broker_read.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic, BRIDGE_MAGIC);
        let socket = directory.path().join("git.sock");
        let request = vec![b'q'; 256 * 1024];
        let reply = vec![b'r'; 256 * 1024];

        // The first client streams a large request and vanishes without ever
        // reading its reply.
        let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
        client
            .write_all(b"{\"repository\":\"main\",\"service\":\"git-upload-pack\"}\n")
            .await
            .unwrap();
        let open = read_data_frame(&mut broker_read).await;
        assert!(String::from_utf8_lossy(&open).contains("git-upload-pack"));
        let abandoning = tokio::spawn({
            let request = request.clone();
            async move {
                let _ = client.write_all(&request).await;
                drop(client);
            }
        });
        let abandoned = read_exchange(&mut broker_read).await;
        abandoning.await.unwrap();
        assert!(!abandoned.is_empty());
        write_all_framed(&mut broker_write, &reply).await;

        // The next client is served in full, both ways.
        let (mut client_read, mut client_write) = tokio::net::UnixStream::connect(&socket)
            .await
            .unwrap()
            .into_split();
        client_write
            .write_all(b"{\"repository\":\"main\",\"service\":\"git-upload-pack\"}\n")
            .await
            .unwrap();
        let open = read_data_frame(&mut broker_read).await;
        assert!(String::from_utf8_lossy(&open).contains("main"));
        let sending = tokio::spawn(async move {
            client_write.write_all(&request).await.unwrap();
            client_write.shutdown().await.unwrap();
        });
        let receiving = tokio::spawn(async move {
            let mut received = Vec::new();
            client_read.read_to_end(&mut received).await.unwrap();
            received
        });
        let received_request = read_exchange(&mut broker_read).await;
        sending.await.unwrap();
        write_all_framed(&mut broker_write, &reply).await;
        let received_reply = receiving.await.unwrap();

        assert_eq!(received_request.len(), 256 * 1024);
        assert_eq!(received_reply.len(), 256 * 1024);

        // Closing the frame stream stops an idle bridge; both halves have to
        // go for the stream itself to close.
        drop((broker_read, broker_write));
        serving.await.unwrap().unwrap();
    }

    /// A client that connects and then says nothing must not hold the socket
    /// the whole session shares.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_that_never_finishes_its_handshake_is_timed_out() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let handshake = Duration::from_millis(500);
        let (worker_end, broker_end) = tokio::io::duplex(16 * 1024);
        let (worker_read, worker_write) = tokio::io::split(worker_end);
        let (mut broker_read, mut broker_write) = tokio::io::split(broker_end);
        let serving = tokio::spawn(async move {
            run_worker_bridge_over(
                &root,
                worker_read,
                worker_write,
                handshake,
                EXCHANGE_IDLE_DEADLINE,
            )
            .await
        });
        let mut magic = vec![0; BRIDGE_MAGIC.len()];
        broker_read.read_exact(&mut magic).await.unwrap();
        let socket = directory.path().join("git.sock");

        // A client that connects and stalls before its newline.
        let mut silent = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let line = handshake_line("silent");
        silent.write_all(&line[..line.len() - 1]).await.unwrap();

        // The next client is served once the silent one runs out of time, and
        // the silent one never reaches the broker at all.
        let (mut client_read, mut client_write) = tokio::net::UnixStream::connect(&socket)
            .await
            .unwrap()
            .into_split();
        client_write
            .write_all(&handshake_line("served"))
            .await
            .unwrap();
        let open = tokio::time::timeout(Duration::from_secs(30), read_data_frame(&mut broker_read))
            .await
            .expect("a silent client held the bridge");
        assert!(
            String::from_utf8_lossy(&open).contains("served"),
            "first framed request was {}",
            String::from_utf8_lossy(&open)
        );

        // A timed-out client is disconnected rather than left hanging.
        let mut byte = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), silent.read(&mut byte))
                .await
                .expect("a timed-out client was left connected")
                .unwrap(),
            0
        );

        // The served exchange still completes normally. Its reply is far
        // larger than one pipe buffer, so the client has to be read while the
        // broker writes.
        let reply = vec![b'r'; 256 * 1024];
        let receiving = tokio::spawn(async move {
            let mut received = Vec::new();
            client_read.read_to_end(&mut received).await.unwrap();
            received
        });
        write_all_framed(&mut broker_write, &reply).await;
        assert!(read_exchange(&mut broker_read).await.is_empty());
        assert_eq!(receiving.await.unwrap().len(), 256 * 1024);
        drop(client_write);

        drop((broker_read, broker_write));
        serving.await.unwrap().unwrap();
    }

    /// A client that wedges mid-transfer must lose its own exchange and
    /// nothing else: the frame stream stays in sync for the next one.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_client_that_stalls_mid_transfer_loses_only_its_own_exchange() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let idle = Duration::from_millis(500);
        let (worker_end, broker_end) = tokio::io::duplex(16 * 1024);
        let (worker_read, worker_write) = tokio::io::split(worker_end);
        let (mut broker_read, mut broker_write) = tokio::io::split(broker_end);
        let serving = tokio::spawn(async move {
            run_worker_bridge_over(&root, worker_read, worker_write, HANDSHAKE_DEADLINE, idle).await
        });
        let mut magic = vec![0; BRIDGE_MAGIC.len()];
        broker_read.read_exact(&mut magic).await.unwrap();
        let socket = directory.path().join("git.sock");
        let request = vec![b'q'; 256 * 1024];
        let reply = vec![b'r'; 256 * 1024];

        // This client streams a large request and then wedges: it sends no
        // more, reads nothing, and never hangs up.
        let mut wedged = tokio::net::UnixStream::connect(&socket).await.unwrap();
        wedged.write_all(&handshake_line("wedged")).await.unwrap();
        let open = read_data_frame(&mut broker_read).await;
        assert!(String::from_utf8_lossy(&open).contains("wedged"));
        let started = Instant::now();
        let draining = tokio::spawn(async move {
            let received =
                tokio::time::timeout(Duration::from_secs(30), read_exchange(&mut broker_read))
                    .await
                    .expect("a wedged client held the bridge");
            (broker_read, received)
        });
        wedged.write_all(&request).await.unwrap();
        let (mut broker_read, received) = draining.await.unwrap();
        assert_eq!(received.len(), 256 * 1024);
        assert!(
            started.elapsed() >= idle,
            "the exchange ended after {:?}, before its idle window",
            started.elapsed()
        );

        // The broker still owes its half of the aborted exchange, which the
        // bridge reads through without touching the client it gave up on.
        write_all_framed(&mut broker_write, &reply).await;
        let mut byte = [0u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(30), wedged.read(&mut byte))
                .await
                .expect("an abandoned client was left connected")
                .unwrap(),
            0
        );

        // The next client is served in full, both ways.
        let (mut client_read, mut client_write) = tokio::net::UnixStream::connect(&socket)
            .await
            .unwrap()
            .into_split();
        client_write
            .write_all(&handshake_line("main"))
            .await
            .unwrap();
        let open = read_data_frame(&mut broker_read).await;
        assert!(String::from_utf8_lossy(&open).contains("main"));
        let sending = tokio::spawn(async move {
            client_write.write_all(&request).await.unwrap();
            client_write.shutdown().await.unwrap();
        });
        let receiving = tokio::spawn(async move {
            let mut received = Vec::new();
            client_read.read_to_end(&mut received).await.unwrap();
            received
        });
        let received_request = read_exchange(&mut broker_read).await;
        sending.await.unwrap();
        write_all_framed(&mut broker_write, &reply).await;
        assert_eq!(received_request.len(), 256 * 1024);
        assert_eq!(receiving.await.unwrap().len(), 256 * 1024);

        drop((broker_read, broker_write));
        serving.await.unwrap().unwrap();
    }

    /// A Git service that stops moving must not hold the broker either.
    #[tokio::test]
    async fn the_broker_gives_up_on_a_service_that_stops_moving() {
        let directory = tempfile::tempdir().unwrap();
        let repository = repository_with_large_advertisement(directory.path());
        let repositories = BTreeMap::from([("main".to_owned(), repository)]);
        let idle = Duration::from_secs(1);
        let (broker_end, target_end) = tokio::io::duplex(16 * 1024);
        let (mut broker_read, mut broker_write) = tokio::io::split(broker_end);
        let (mut target_read, mut target_write) = tokio::io::split(target_end);
        let serving = tokio::spawn(async move {
            serve_bridge(
                &mut broker_write,
                &mut broker_read,
                &repositories,
                "test",
                idle,
            )
            .await
        });

        // `upload-pack` advertises its refs and then waits for a client that
        // never answers and never hangs up.
        write_frame(&mut target_write, &open_frame("main"))
            .await
            .unwrap();
        let started = Instant::now();
        let advertisement =
            tokio::time::timeout(Duration::from_secs(60), read_exchange(&mut target_read))
                .await
                .expect("a stalled Git service held the broker");
        assert!(
            advertisement.len() > 64 * 1024,
            "advertisement was {} bytes",
            advertisement.len()
        );
        assert!(
            started.elapsed() >= idle,
            "the exchange ended after {:?}, before its idle window",
            started.elapsed()
        );
        // The target still owes its half of the aborted exchange.
        write_frame(&mut target_write, &[]).await.unwrap();

        // The broker serves the next exchange over the same stream.
        write_frame(&mut target_write, &open_frame("main"))
            .await
            .unwrap();
        write_frame(&mut target_write, b"0000").await.unwrap();
        write_frame(&mut target_write, &[]).await.unwrap();
        let advertisement = read_exchange(&mut target_read).await;
        assert!(advertisement.len() > 64 * 1024);
        assert!(String::from_utf8_lossy(&advertisement).contains("refs/heads/main"));

        drop((target_read, target_write));
        serving.await.unwrap().unwrap();
    }

    /// A PID file whose broker is gone must read as dead, however it was
    /// left behind.
    #[test]
    fn broker_liveness_follows_the_lock_and_not_the_written_pid() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("session.pid");

        assert!(!broker_is_alive(&pid_path));
        assert_eq!(running_broker_pid(&pid_path), None);

        // A PID file naming this very much alive process still reads as dead
        // while no broker holds its lock, and never names a process to signal.
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        assert!(!broker_is_alive(&pid_path));
        assert_eq!(running_broker_pid(&pid_path), None);

        let claimed = claim_broker_pid_file(&pid_path).unwrap();
        assert!(broker_is_alive(&pid_path));
        assert_eq!(
            running_broker_pid(&pid_path),
            Some(std::process::id() as i32)
        );
        assert!(claim_broker_pid_file(&pid_path).is_err());
        let written = std::fs::read_to_string(&pid_path).unwrap();
        assert_eq!(written.trim(), std::process::id().to_string());

        drop(claimed);
        // The lock lives on the open file description, so it survives until
        // every copy of that description is closed. A sibling thread that
        // forks while this one is open hands a copy to its child until the
        // child execs, which is why an owner that let go is observed free
        // shortly rather than instantly. Measured here: a few hundred
        // microseconds at worst.
        let released = std::time::Instant::now();
        while broker_is_alive(&pid_path) {
            assert!(
                released.elapsed() < Duration::from_secs(5),
                "a released broker lock was never observed free"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(running_broker_pid(&pid_path), None);
    }
}
