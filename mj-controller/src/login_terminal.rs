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
    child: Box<dyn portable_pty::Child + Send + Sync>,
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
        if let Some(pid) = self.child.process_id() {
            mj_core::subprocess::terminate_process_group(pid as i32, libc::SIGKILL);
        }
        if let Err(error) = self.child.kill() {
            tracing::debug!(%error, "terminate login terminal child");
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}

// Observe exit without reaping: the process id remains reserved until the
// terminal owner has signaled the group and drained the PTY.
#[cfg(unix)]
fn observe_exit(process: &Process) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let pid = process
        .child
        .process_id()
        .context("login child has no process id")?;
    Ok(tokio::task::spawn_blocking(move || {
        loop {
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
            // SAFETY: info points to writable siginfo storage; pid is our
            // unreaped child. WNOWAIT leaves reaping to the sole owner.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
    }))
}

#[cfg(windows)]
fn observe_exit(process: &Process) -> Result<tokio::task::JoinHandle<Result<()>>> {
    use std::os::windows::io::{AsRawHandle, BorrowedHandle};
    let raw = process
        .child
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

type Master = std::sync::Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>;

fn close_console_on_exit(master: &Master) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(windows) {
        // Closing ConPTY terminates its attached descendants. Drain output
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
        Ok::<_, anyhow::Error>((pair, Process { child, armed: true }, reader, writer))
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
    let master = std::sync::Mutex::new(Some(pair.master));
    // A one-chunk rendezvous keeps output backpressure at the PTY instead of
    // accumulating the entire login transcript in daemon memory.
    let (output_tx, mut output_rx) = mpsc::channel(1);
    let output = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut bytes = [0u8; 8192];
        loop {
            let count = match reader.read(&mut bytes) {
                Ok(count) => count,
                // Linux reports EIO when the last PTY slave closes.
                #[cfg(unix)]
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if count == 0 || output_tx.blocking_send(bytes[..count].to_vec()).is_err() {
                return Ok(());
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
    process.terminate();
    if !observed {
        exited.await.context("join login exit observer")??;
    }
    input.abort();
    match input.await {
        Ok(Err(error)) => tracing::debug!(%error, "login terminal input ended"),
        Err(error) if !error.is_cancelled() => {
            tracing::warn!(%error, "login terminal input task failed")
        }
        _ => {}
    }
    drop(bytes_tx);
    drop(output_rx);
    if let Some(closing) = closing {
        closing.await.context("close login console")?;
    }
    let status = tokio::task::spawn_blocking(move || {
        drop(master);
        process.child.wait()
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
