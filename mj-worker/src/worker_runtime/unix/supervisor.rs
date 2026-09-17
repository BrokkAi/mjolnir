use super::*;

pub(crate) const PROXY_INITIAL_INPUT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

pub(crate) async fn abort_peer_and_return<T>(
    peer: &mut tokio::task::JoinHandle<T>,
    error: anyhow::Error,
    context: &'static str,
) -> Result<()> {
    peer.abort();
    let _ = peer.await;
    Err(error.context(context))
}

pub(crate) async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    maximum_bytes: usize,
) -> Result<Option<String>> {
    use mj_core::bounded_frame::{BoundedFrame, BoundedFrameError};
    let line = match mj_core::bounded_frame::read_bounded_frame(reader, maximum_bytes).await {
        Ok(BoundedFrame::Line(line) | BoundedFrame::Truncated(line)) => line,
        Ok(BoundedFrame::End) => return Ok(None),
        Err(BoundedFrameError::TooLarge) => bail!("relay request frame is too large"),
        Err(BoundedFrameError::Io(error)) => {
            return Err(anyhow::Error::new(error).context("read relay request"));
        }
    };
    String::from_utf8(line)
        .context("relay request is not UTF-8")
        .map(Some)
}

/// A durable write can only fail permanently because the worker root is
/// gone: session teardown removed it under this daemon. Nothing served
/// afterwards could ever be persisted, so the daemon has to stop.
pub(crate) fn worker_root_was_removed(body: &RelayResponseBody, root: &std::path::Path) -> bool {
    matches!(
        body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                ..
            }
        }
    ) && !root.is_dir()
}

pub(crate) async fn forward_proxy_streams(
    mut client_read: impl tokio::io::AsyncRead + Unpin,
    mut client_write: impl tokio::io::AsyncWrite + Unpin,
    mut relay_read: impl tokio::io::AsyncRead + Unpin,
    mut relay_write: impl tokio::io::AsyncWrite + Unpin,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The proxy must die with its client. Joining both copy directions
    // left the process alive forever after stdin EOF (a killed `podman
    // exec` client), leaking one thread-heavy process per poll inside the
    // container. Exit as soon as either side closes. An idle connection
    // is intentional: it may own a checkpoint barrier while the
    // controller transfers a large archive. Before the first request,
    // however, a bounded deadline prevents a detached Podman conmon from
    // holding the target-side proxy forever after the controller kills a
    // launcher whose handshake timed out.
    let mut client_buf = [0_u8; 64 * 1024];
    let mut relay_buf = [0_u8; 64 * 1024];
    let first_count = match tokio::time::timeout(
        PROXY_INITIAL_INPUT_TIMEOUT,
        client_read.read(&mut client_buf),
    )
    .await
    {
        Ok(read) => read.context("read initial proxy stdin")?,
        Err(_) => {
            tracing::debug!(
                operation = "proxy_initial_input",
                "relay proxy client sent no initial frame before the idle deadline"
            );
            return Ok(());
        }
    };
    if first_count == 0 {
        if let Err(error) = relay_write.shutdown().await {
            tracing::debug!(
                operation = "proxy_shutdown",
                %error,
                "could not close relay socket after proxy client EOF"
            );
        }
        return Ok(());
    }
    relay_write
        .write_all(&client_buf[..first_count])
        .await
        .context("forward initial request to worker")?;

    loop {
        tokio::select! {
            read = client_read.read(&mut client_buf) => {
                let count = read.context("read proxy stdin")?;
                if count == 0 {
                    // Client is gone; flush any final in-flight response
                    // briefly, then exit.
                    if let Err(error) = relay_write.shutdown().await {
                        tracing::debug!(
                            operation = "proxy_shutdown",
                            %error,
                            "could not close relay socket after proxy client EOF"
                        );
                    }
                    if let Err(error) = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        tokio::io::copy(&mut relay_read, &mut client_write),
                    )
                    .await
                    {
                        tracing::debug!(
                            operation = "proxy_final_response",
                            %error,
                            "could not forward the final relay response before proxy shutdown"
                        );
                    }
                    return Ok(());
                }
                relay_write
                    .write_all(&client_buf[..count])
                    .await
                    .context("forward request to worker")?;
            }
            read = relay_read.read(&mut relay_buf) => {
                let count = read.context("read worker socket")?;
                if count == 0 {
                    return Ok(());
                }
                client_write
                    .write_all(&relay_buf[..count])
                    .await
                    .context("forward response to client")?;
                client_write.flush().await.context("flush proxy stdout")?;
            }
        }
    }
}

pub async fn proxy(root: PathBuf) -> Result<()> {
    let socket = root.join("control.sock");
    let stream = connect_unix_stream(&socket)
        .with_context(|| format!("connect worker socket {}", socket.display()))?;
    stream
        .set_nonblocking(true)
        .with_context(|| format!("set worker socket {} nonblocking", socket.display()))?;
    let stream = UnixStream::from_std(stream)
        .with_context(|| format!("register worker socket {}", socket.display()))?;
    let (socket_read, socket_write) = stream.into_split();
    let outcome = forward_proxy_streams(
        tokio::io::stdin(),
        tokio::io::stdout(),
        socket_read,
        socket_write,
    )
    .await;

    // Leave now instead of returning into runtime shutdown. Tokio's global
    // stdin reader holds a blocking pool thread in a read that cannot be
    // cancelled, and a client that is waiting for a response keeps the
    // proxy's stdin open, so dropping the runtime would wait forever. A
    // proxy whose worker socket is gone has nothing left to carry: the
    // client must see this process exit and its stdout close, not a live
    // proxy with no worker behind it.
    let mut stdout = tokio::io::stdout();
    if let Err(error) = stdout.flush().await {
        tracing::debug!(
            operation = "proxy_flush",
            %error,
            "could not flush proxy stdout before exit"
        );
    }
    match outcome {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            // The client reads this proxy's stderr into its own log.
            tracing::error!(
                operation = "proxy",
                error = format!("{error:#}"),
                "relay proxy stopped with an error"
            );
            std::process::exit(1);
        }
    }
}

/// Own the ACP bridge's process group.  The daemon communicates only with
/// this supervisor; if the daemon is killed, stdin reaches EOF and the
/// complete bridge process tree is terminated and reaped.
pub async fn run_acp_supervisor(spec: AcpSupervisorSpec) -> Result<()> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // This hidden subcommand exclusively owns its stdio pipes. Tokio's global
    // stdin reader uses a non-cancellable blocking read, which can keep the
    // supervisor process alive after its ACP child has already been reaped.
    // SAFETY: ownership of each process fd is transferred exactly once here.
    let stdin = unsafe { OwnedFd::from_raw_fd(libc::STDIN_FILENO) };
    // SAFETY: stdout is likewise owned exclusively by this subcommand.
    let stdout = unsafe { OwnedFd::from_raw_fd(libc::STDOUT_FILENO) };
    let stdin = tokio::net::unix::pipe::Receiver::from_owned_fd(stdin)
        .context("open ACP supervisor stdin pipe")?;
    let stdout = tokio::net::unix::pipe::Sender::from_owned_fd(stdout)
        .context("open ACP supervisor stdout pipe")?;
    run_acp_supervisor_with_streams(spec, stdin, stdout).await
}

pub(crate) async fn run_acp_supervisor_with_streams<R, W>(
    spec: AcpSupervisorSpec,
    mut parent_stdin: R,
    mut parent_stdout: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let _harness_lease = spec
        .harness_lease
        .as_deref()
        .map(crate::worker_runtime::harness::acquire_supervisor_lease)
        .transpose()?;
    let environment = mj_core::login_environment::with_overrides(&spec.environment).await?;
    let mut command = tokio::process::Command::new(&spec.command);
    command
        .args(&spec.args)
        .env_clear()
        .envs(&environment)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("launch supervised ACP bridge {}", spec.command.display()))?;
    let pid = child
        .id()
        .context("supervised ACP bridge has no process ID")? as i32;
    let mut child_stdin = child.stdin.take().context("ACP bridge stdin unavailable")?;
    let mut child_stdout = child
        .stdout
        .take()
        .context("ACP bridge stdout unavailable")?;
    enum SupervisorCompletion {
        InputEnded,
        OutputEnded,
        ChildExited(std::process::ExitStatus),
    }

    let completion = {
        let input = tokio::io::copy(&mut parent_stdin, &mut child_stdin);
        let output = tokio::io::copy(&mut child_stdout, &mut parent_stdout);
        let exited = child.wait();
        tokio::pin!(input, output, exited);
        tokio::select! {
            result = &mut input => {
                result.context("forward ACP supervisor input").map(|_| SupervisorCompletion::InputEnded)
            }
            result = &mut output => {
                result.context("forward ACP supervisor output").map(|_| SupervisorCompletion::OutputEnded)
            }
            result = &mut exited => {
                result.context("wait for supervised ACP bridge").map(SupervisorCompletion::ChildExited)
            }
        }
    };
    // A broken parent pipe must still terminate the owned harness group.
    let (completion, forwarding_error) = match completion {
        Ok(completion) => (completion, None),
        Err(error) => (SupervisorCompletion::InputEnded, Some(error)),
    };
    if !matches!(&completion, SupervisorCompletion::ChildExited(_)) {
        match tokio::time::timeout(std::time::Duration::from_secs(1), child_stdin.shutdown()).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::debug!(
                    operation = "acp_supervisor_shutdown",
                    %error,
                    "could not close ACP bridge stdin before termination"
                );
            }
            Err(_) => {
                tracing::warn!(
                    operation = "acp_supervisor_shutdown",
                    "timed out closing ACP bridge stdin before termination"
                );
            }
        }
    }
    drop(child_stdin);
    terminate_process_group(pid, libc::SIGTERM);
    let (bridge_ended, status) = match completion {
        SupervisorCompletion::ChildExited(status) => (true, Some(status)),
        SupervisorCompletion::InputEnded => {
            match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(status) => {
                    let status = status.context("wait for supervised ACP bridge")?;
                    (false, Some(status))
                }
                Err(_) => {
                    terminate_process_group(pid, libc::SIGKILL);
                    child.wait().await.context("reap supervised ACP bridge")?;
                    (false, None)
                }
            }
        }
        SupervisorCompletion::OutputEnded => {
            match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(status) => {
                    let status = status.context("wait for supervised ACP bridge")?;
                    (true, Some(status))
                }
                Err(_) => {
                    terminate_process_group(pid, libc::SIGKILL);
                    child.wait().await.context("reap supervised ACP bridge")?;
                    (true, None)
                }
            }
        }
    };
    // The leader can exit on TERM while a descendant ignores it.
    terminate_process_group(pid, libc::SIGKILL);
    if let Some(error) = forwarding_error {
        return Err(error);
    }
    if bridge_ended
        && let Some(status) = status
        && !status.success()
    {
        bail!("supervised ACP bridge exited with {status}");
    }
    Ok(())
}

/// Make this process lead its own session, so session teardown can stop
/// the whole worker tree with a single process-group signal. Failing means
/// the process already leads its group, which is the state we wanted.
///
/// Only the real daemon entry point may call this: it detaches the caller
/// from its controlling terminal.
pub fn lead_process_group() {
    // SAFETY: setsid takes no arguments and changes only this process's
    // own session and process-group membership.
    unsafe {
        libc::setsid();
    }
}
