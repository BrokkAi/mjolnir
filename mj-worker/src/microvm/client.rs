//! The host side of the vsock protocol.
//!
//! Firecracker exposes the guest's vsock as a Unix socket on the host. A
//! client connects to it, writes `CONNECT <port>\n`, and reads `OK <n>\n`
//! once the guest accepts; the stream is then the guest connection.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::protocol::{
    Accepted, CHUNK_SIZE, ExecRequest, ExitInfo, FrameKind, FrameReader, FrameSender, Hello,
    Request, StartRequest, TarFrameReader, TarFrameWriter, Transport, negotiate,
};
use super::tarcopy;

/// How long one connection attempt may wait for the guest's answers. The
/// readiness loop that calls this keeps trying while the VM is running.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to the guest agent through Firecracker's vsock socket.
pub fn connect_unix(socket: &Path, port: u32) -> Result<UnixStream> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    stream.set_read_timeout(Some(ATTEMPT_TIMEOUT))?;
    stream.set_write_timeout(Some(ATTEMPT_TIMEOUT))?;
    stream
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .context("ask Firecracker for the guest connection")?;
    // Read one byte at a time: bytes after the newline belong to the protocol.
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let count = stream
            .read(&mut byte)
            .context("read Firecracker's answer")?;
        if count == 0 {
            bail!("the guest agent is not listening on vsock port {port}");
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > 64 {
            bail!("Firecracker's vsock answer is too long");
        }
    }
    let line = String::from_utf8_lossy(&line);
    if !line.starts_with("OK ") {
        bail!("Firecracker refused the vsock connection: {line}");
    }
    Ok(stream)
}

/// A connection past its hello, ready for one request.
pub struct Client<T: Transport> {
    sender: FrameSender<T>,
    frames: FrameReader<T>,
    pub version: u32,
}

/// Exchange hellos on a fresh connection.
pub fn handshake<T: Transport>(transport: T) -> Result<Client<T>> {
    let sender = FrameSender::new(transport.try_clone().context("clone connection")?);
    let mut frames = FrameReader::new(transport);
    let ours = Hello::current();
    sender.send_json(FrameKind::Hello, &ours)?;
    let reply = frames.expect("the agent's hello")?;
    let theirs: Hello = match reply.kind {
        FrameKind::Hello => {
            serde_json::from_slice(&reply.payload).context("decode the agent's hello")?
        }
        FrameKind::Error => bail!("{}", String::from_utf8_lossy(&reply.payload)),
        other => bail!("expected a hello frame, received {other:?}"),
    };
    let version = negotiate(&ours, &theirs)?;
    Ok(Client {
        sender,
        frames,
        version,
    })
}

fn error_text(payload: &[u8]) -> anyhow::Error {
    anyhow!("{}", String::from_utf8_lossy(payload))
}

impl<T: Transport> Client<T> {
    fn request(&self, request: &Request) -> Result<()> {
        self.sender.send_json(FrameKind::Request, request)
    }

    /// Wait for the agent to accept a started or detached process.
    fn accepted(&mut self) -> Result<u32> {
        let frame = self.frames.expect("the agent's answer")?;
        match frame.kind {
            FrameKind::Accepted => Ok(serde_json::from_slice::<Accepted>(&frame.payload)
                .context("decode the agent's answer")?
                .pid),
            FrameKind::Error => Err(error_text(&frame.payload)),
            other => bail!("expected an accepted frame, received {other:?}"),
        }
    }

    /// Start the container's main process.
    pub fn start(mut self, start: StartRequest) -> Result<u32> {
        self.request(&Request::Start(start))?;
        self.accepted()
    }

    /// Start a detached process and return its pid in the guest.
    pub fn exec_detached(mut self, mut request: ExecRequest) -> Result<u32> {
        request.detach = true;
        request.stdin = false;
        self.request(&Request::Exec(request))?;
        self.accepted()
    }

    /// Run a command, relaying `stdin` to it and its output to `stdout` and
    /// `stderr`, and return how it ended.
    ///
    /// Input is sent from its own thread while this one reads output, so a
    /// command that writes before it reads cannot deadlock the pair.
    pub fn exec(
        mut self,
        mut request: ExecRequest,
        stdin: Option<Box<dyn Read + Send>>,
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> Result<ExitInfo> {
        request.detach = false;
        request.stdin = stdin.is_some();
        self.request(&Request::Exec(request))?;
        let (input_errors, input_failures) = mpsc::channel::<anyhow::Error>();
        if let Some(mut input) = stdin {
            let sender = self.sender.clone();
            std::thread::Builder::new()
                .name("exec-stdin".to_owned())
                .spawn(move || {
                    let mut buffer = vec![0u8; CHUNK_SIZE];
                    loop {
                        match input.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(count) => {
                                if sender.send(FrameKind::Stdin, &buffer[..count]).is_err() {
                                    // The agent closed the connection: the
                                    // process has finished, so unsent input
                                    // is moot and the exit frame has the rest.
                                    return;
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(error) => {
                                let _ = input_errors.send(
                                    anyhow!(error).context("read the command's standard input"),
                                );
                                break;
                            }
                        }
                    }
                    // Same reasoning for a failed end-of-input frame.
                    let _ = sender.send(FrameKind::StdinEof, &[]);
                })
                .context("start stdin thread")?;
        }
        loop {
            let frame = self.frames.expect("the command's output or exit")?;
            match frame.kind {
                FrameKind::Stdout => stdout
                    .write_all(&frame.payload)
                    .context("write command output")?,
                FrameKind::Stderr => stderr
                    .write_all(&frame.payload)
                    .context("write command error output")?,
                FrameKind::Exit => {
                    stdout.flush().context("flush command output")?;
                    if let Ok(error) = input_failures.try_recv() {
                        return Err(error);
                    }
                    return serde_json::from_slice(&frame.payload).context("decode exit status");
                }
                FrameKind::Error => return Err(error_text(&frame.payload)),
                other => bail!("unexpected {other:?} frame during exec"),
            }
        }
    }

    fn expect_exit(&mut self) -> Result<()> {
        let frame = self.frames.expect("the copy result")?;
        match frame.kind {
            FrameKind::Exit => Ok(()),
            FrameKind::Error => Err(error_text(&frame.payload)),
            other => bail!("unexpected {other:?} frame after a copy"),
        }
    }

    /// Copy a host file or directory into the guest at `destination`.
    pub fn copy_in(mut self, source: &Path, destination: &str) -> Result<()> {
        self.request(&Request::CopyIn {
            destination: destination.to_owned(),
        })?;
        let writer = TarFrameWriter::new(self.sender.clone());
        let packed = tarcopy::pack(source, writer)
            .and_then(|writer| writer.finish().context("finish tar stream"));
        match packed {
            Ok(()) => self.expect_exit(),
            Err(packing) => {
                // A write that failed because the agent refused the copy has
                // the agent's reason waiting in the connection.
                match self.expect_exit() {
                    Err(reason) if self.frames_have_error(&reason) => Err(reason),
                    _ => Err(packing),
                }
            }
        }
    }

    fn frames_have_error(&self, reason: &anyhow::Error) -> bool {
        // A closed connection reports "connection closed"; a refusal carries text.
        !reason.to_string().starts_with("connection closed")
    }

    /// Copy `source` out of the guest to `destination` on the host.
    pub fn copy_out(
        mut self,
        source: &str,
        destination: &Path,
        owner: Option<(u32, u32)>,
    ) -> Result<()> {
        self.request(&Request::CopyOut {
            source: source.to_owned(),
        })?;
        let mut reader = TarFrameReader::new(&mut self.frames);
        tarcopy::unpack(&mut reader, destination, owner)?;
        reader.finish()?;
        self.expect_exit()
    }

    /// Ask the guest to flush its filesystems and power off. Returns once the
    /// flush has finished.
    pub fn stop(mut self) -> Result<()> {
        self.request(&Request::Stop)?;
        self.expect_exit()
    }
}
