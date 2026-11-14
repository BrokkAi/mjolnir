//! Daemon-owned disposable login terminals. No worker state or handoff admission.
use anyhow::{Context, Result};
use mj_client::daemon::login_terminal::{TerminalInput, TerminalOutput};
use mj_client::daemon::{read_frame, write_frame};
use portable_pty::{CommandBuilder, PtySize};
use std::io::{Read, Write};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Process {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    armed: bool,
}
impl Process {
    fn terminate(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        // The root is not reaped until cleanup, so its process-group id cannot
        // be recycled while this owner still has terminal reader/writer tasks.
        #[cfg(unix)]
        if let Some(pid) = self.child.as_ref().and_then(|child| child.process_id()) {
            mj_core::subprocess::terminate_process_group(pid as i32, libc::SIGKILL);
        }
        if let Err(error) = self.child.as_mut().unwrap().kill() {
            tracing::debug!(%error, "terminate login terminal child");
        }
    }
}
impl Process {
    fn hangup(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.as_ref().and_then(|child| child.process_id()) {
            // Reader/writer duplicates keep the master open, so explicitly
            // wake the root's cleanup owner before waiting for its exit.
            if unsafe { libc::kill(pid as i32, libc::SIGHUP) } != 0 {
                tracing::debug!(error = %std::io::Error::last_os_error(), "hang up login root");
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.hangup();
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Connection tasks are disposable during handoff. Transfer cleanup to
        // an independent owner; never block the daemon or kill the root first.
        std::thread::spawn(move || {
            #[cfg(unix)]
            if let Some(pid) = child.process_id() {
                if let Err(error) = wait_for_exit(pid) {
                    tracing::warn!(%error, "observe abandoned login cleanup");
                }
                mj_core::subprocess::terminate_process_group(pid as i32, libc::SIGKILL);
            }
            if let Err(error) = child.wait() {
                tracing::warn!(%error, "reap abandoned login child");
            }
        });
    }
}

// Observe exit without reaping: the process id remains reserved until the
// terminal owner has signaled the group and drained the PTY.
#[cfg(unix)]
fn observe_exit(process: &Process) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let pid = process
        .child
        .as_ref()
        .and_then(|child| child.process_id())
        .context("login child has no process id")?;
    Ok(tokio::task::spawn_blocking(move || wait_for_exit(pid)))
}

#[cfg(unix)]
fn wait_for_exit(pid: u32) -> Result<()> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
        // SAFETY: this is our unreaped child; WNOWAIT reserves its group id.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        } == 0
        {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

#[cfg(windows)]
fn observe_exit(process: &Process) -> Result<tokio::task::JoinHandle<Result<()>>> {
    use std::os::windows::io::{AsRawHandle, BorrowedHandle};
    let raw = process
        .child
        .as_ref()
        .context("login child missing")?
        .as_raw_handle()
        .context("login child has no process handle")?;
    // SAFETY: the child owns raw for this call. The observer receives its own
    // handle, so aborting the connection cannot close a handle being waited on.
    let handle = unsafe { BorrowedHandle::borrow_raw(raw) }.try_clone_to_owned()?;
    Ok(tokio::task::spawn_blocking(move || {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
        }
        // SAFETY: the owner retains the process handle until this observer
        // joins. INFINITE waits for process termination, not an elapsed budget.
        if unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), u32::MAX) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    }))
}

struct Master(std::sync::Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>);
impl std::ops::Deref for Master {
    type Target = std::sync::Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for Master {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(master) = self.0.get_mut().unwrap().take() {
            // A connection task may be aborted during handoff. ConPTY close
            // can wait for the login close handler, so keep it off this loop.
            std::thread::spawn(move || drop(master));
        }
    }
}

fn close_console_on_exit(master: &Master) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(windows) {
        // Closing ConPTY sends CTRL_CLOSE_EVENT. The login handler retains
        // cleanup ownership until preparation groups stop. Drain output
        // concurrently: ClosePseudoConsole can emit a final frame and block.
        // https://learn.microsoft.com/en-us/windows/console/closepseudoconsole
        let master = master.lock().unwrap().take();
        Some(tokio::task::spawn_blocking(move || drop(master)))
    } else {
        None
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Serve a prepared command. Tests pass a real fixture command through this
/// same connection executor; production passes this daemon's login entrypoint.
pub(crate) async fn serve(
    mut stream: TcpStream,
    command: CommandBuilder,
    cols: u16,
    rows: u16,
    cancellation: CancellationToken,
) -> Result<()> {
    let started = tokio::task::spawn_blocking(move || {
        let pair = portable_pty::native_pty_system().openpty(size(cols, rows))?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let child = pair.slave.spawn_command(command)?;
        Ok::<_, anyhow::Error>((
            pair,
            Process {
                child: Some(child),
                armed: true,
            },
            reader,
            writer,
        ))
    })
    .await
    .context("start login PTY task")?;
    let (pair, mut process, mut reader, mut writer) = match started {
        Ok(started) => started,
        Err(error) => {
            write_frame(
                &mut stream,
                &TerminalOutput::Failed(format!("Start login terminal: {error:#}")),
            )
            .await?;
            return Ok(());
        }
    };
    drop(pair.slave);
    let master = Master(std::sync::Mutex::new(Some(pair.master)));
    // A one-chunk rendezvous keeps output backpressure at the PTY instead of
    // accumulating the entire login transcript in daemon memory.
    let (output_tx, mut output_rx) = mpsc::channel(1);
    let output = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut bytes = [0u8; 8192];
        let mut forwarding = true;
        loop {
            let count = match reader.read(&mut bytes) {
                Ok(count) => count,
                // Linux reports EIO when the last PTY slave closes.
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if count == 0 {
                return Ok(());
            }
            if forwarding && output_tx.blocking_send(bytes[..count].to_vec()).is_err() {
                // ConPTY close may emit output until its close handlers finish.
                // Keep draining after connection cancellation so closing the
                // console never depends on a client that has disconnected.
                forwarding = false;
            }
        }
    });
    let (mut read, mut write) = stream.into_split();
    // Keep reading TCP while the harness is not reading its terminal. Otherwise
    // input backpressure hides EOF and a disconnected login cannot be killed.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    let disconnected = cancellation.child_token();
    let guard = disconnected.clone().drop_guard();
    let input = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        let _guard = guard;
        loop {
            let frame: TerminalInput = read_frame(&mut read).await?;
            if input_tx.send(frame).is_err() {
                return Ok::<(), anyhow::Error>(());
            }
        }
    }));
    let (bytes_tx, mut bytes_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writing = tokio::task::spawn_blocking(move || -> Result<()> {
        while let Some(bytes) = bytes_rx.blocking_recv() {
            writer.write_all(&bytes)?;
            writer.flush()?;
        }
        Ok(())
    });
    let mut exited = observe_exit(&process)?;
    let mut observed = false;
    let mut closing = None;
    let mut interrupted_write = false;
    let result: Result<bool> = async {
        loop {
            tokio::select! {
                result = &mut exited, if !observed => {
                    observed = true;
                    result.context("observe login exit")??;
                    process.terminate();
                    closing = close_console_on_exit(&master);
                }
                _ = disconnected.cancelled() => {
                    // Connection cancellation is the retry boundary, not login success.
                    return Ok(false);
                }
                frame = input_rx.recv() => match frame {
                    Some(TerminalInput::Bytes(bytes)) => {
                        bytes_tx.send(bytes).context("login input writer stopped")?;
                    }
                    Some(TerminalInput::Resize { cols, rows }) => {
                        if let Some(master) = master.lock().unwrap().as_ref() { master.resize(size(cols, rows))?; }
                    },
                    None => return Ok(false),
                },
                bytes = output_rx.recv() => match bytes {
                    Some(bytes) => {
                        let frame = TerminalOutput::Bytes(bytes);
                        tokio::select! {
                            _ = disconnected.cancelled() => { interrupted_write = true; return Ok(false); },
                            result = write_frame(&mut write, &frame) => result?,
                        }
                    },
                    None => return Ok(true),
                }
            }
        }
    }.await;
    // The connection has stopped forwarding output. Release the receiver now so
    // the reader discards PTY output from here on: the root writes to the
    // terminal while it exits (its cancellation error goes to stderr), and a
    // reader parked on a full channel would leave it blocked on a full PTY
    // while cleanup below waits for that exit.
    drop(output_rx);
    if !observed {
        process.hangup();
        closing = close_console_on_exit(&master);
        exited.await.context("join login exit observer")??;
    }
    process.terminate();
    input.abort();
    match input.await {
        Ok(Err(error)) => tracing::debug!(%error, "login terminal input ended"),
        Err(error) if !error.is_cancelled() => {
            tracing::warn!(%error, "login terminal input task failed")
        }
        _ => {}
    }
    drop(bytes_tx);
    if let Some(closing) = closing {
        closing.await.context("close login console")?;
    }
    let status = tokio::task::spawn_blocking(move || {
        drop(master);
        process.child.as_mut().unwrap().wait()
    })
    .await
    .context("join login child wait")??;
    output.await.context("join login output reader")??;
    writing.await.context("join login input writer")??;
    if result? {
        write_frame(
            &mut write,
            &TerminalOutput::Exited {
                code: status.exit_code(),
            },
        )
        .await?;
    } else if cancellation.is_cancelled() && !interrupted_write {
        write_frame(
            &mut write,
            &TerminalOutput::Failed("The daemon stopped or handed off; retry Login.".into()),
        )
        .await?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Hard-won: 29354e991: a disconnect left the root blocked writing to a full PTY while cleanup waited for its exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disconnect_cleanup_drains_output_the_exiting_root_writes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        // On hangup the root writes far more than the one-chunk channel and the
        // PTY buffer hold, as a root's exit output can when nobody reads it.
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "trap 'head -c 1048576 /dev/zero | tr \"\\000\" x; exit 0' HUP; \
             printf ready; while :; do sleep 0.05; done",
        ]);
        let serving = tokio::spawn(serve(server, command, 80, 24, CancellationToken::new()));
        let mut transcript = Vec::new();
        while !transcript.ends_with(b"ready") {
            match read_frame::<TerminalOutput>(&mut client).await.unwrap() {
                TerminalOutput::Bytes(bytes) => transcript.extend(bytes),
                other => panic!("login ended before it was ready: {other:?}"),
            }
        }
        drop(client);
        // Bounds only the hang this test exists to catch; cleanup itself takes
        // milliseconds.
        tokio::time::timeout(std::time::Duration::from_secs(30), serving)
            .await
            .expect("disconnect cleanup waited on a root blocked writing its output")
            .unwrap()
            .unwrap();
    }
}
