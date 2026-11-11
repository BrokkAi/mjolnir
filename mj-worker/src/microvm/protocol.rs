//! The vsock protocol between the host engine and the guest agent.
//!
//! One connection carries one request. Both sides open with a hello that
//! names their protocol version, so an engine can talk to the agent of a VM
//! that was launched by an older engine build, and the reverse. After the
//! hello the client sends a request frame, then data frames flow in both
//! directions until the agent sends an exit frame (or an error frame).
//!
//! A frame is one kind byte, a four-byte big-endian payload length, and the
//! payload. Control payloads are JSON; data payloads are raw bytes.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// The version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// The oldest version this build can still speak. Raising it strands VMs that
/// were launched by older engines, so it only rises with a migration for them.
pub const MIN_PROTOCOL_VERSION: u32 = 1;
/// The guest vsock port the agent listens on.
pub const AGENT_PORT: u32 = 1024;
/// Writers split data into frames of at most this many bytes.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Readers accept frames up to this size. It is the protocol's own limit, not
/// a guard against load: a corrupt length must not allocate gigabytes.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// What a frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 1,
    Request = 2,
    /// A detached or started process was created; the payload names its pid.
    Accepted = 3,
    Stdin = 0x10,
    StdinEof = 0x11,
    Stdout = 0x12,
    Stderr = 0x13,
    Tar = 0x14,
    TarEnd = 0x15,
    Exit = 0x20,
    Error = 0x21,
}

impl FrameKind {
    fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            1 => Self::Hello,
            2 => Self::Request,
            3 => Self::Accepted,
            0x10 => Self::Stdin,
            0x11 => Self::StdinEof,
            0x12 => Self::Stdout,
            0x13 => Self::Stderr,
            0x14 => Self::Tar,
            0x15 => Self::TarEnd,
            0x20 => Self::Exit,
            0x21 => Self::Error,
            other => bail!("unknown microvm protocol frame kind {other:#x}"),
        })
    }
}

/// One decoded frame.
#[derive(Debug)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

/// Both peers announce what they speak.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub min_version: u32,
}

impl Hello {
    pub fn current() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            min_version: MIN_PROTOCOL_VERSION,
        }
    }
}

/// The version two hellos agree on, or why they cannot.
pub fn negotiate(ours: &Hello, theirs: &Hello) -> Result<u32> {
    let version = ours.version.min(theirs.version);
    let floor = ours.min_version.max(theirs.min_version);
    if version < floor {
        bail!(
            "microvm protocol versions do not overlap: this side speaks {}..={}, the peer speaks {}..={}",
            ours.min_version,
            ours.version,
            theirs.min_version,
            theirs.version
        );
    }
    Ok(version)
}

/// How a process ended.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitInfo {
    /// The exit status a shell would report: the code, or 128 plus the signal.
    pub fn shell_status(&self) -> i32 {
        match (self.code, self.signal) {
            (Some(code), _) => code,
            (None, Some(signal)) => 128 + signal,
            (None, None) => 1,
        }
    }
}

/// One volume the agent finds mounted by the initrd.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeSpec {
    pub destination: String,
    /// Podman's `U` volume option: the volume root belongs to the container user.
    #[serde(default)]
    pub chown: bool,
}

/// The container's main process and the defaults its exec sessions inherit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StartRequest {
    pub argv: Vec<String>,
    /// `KEY=VALUE` entries.
    pub env: Vec<String>,
    pub cwd: String,
    pub user: String,
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    /// `KEY=VALUE` entries added to the container environment.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// The client will send stdin frames; otherwise stdin is `/dev/null`.
    #[serde(default)]
    pub stdin: bool,
    /// Start in a new session, answer at once, and leave the process running.
    #[serde(default)]
    pub detach: bool,
}

/// The first frame after the hello.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Start(StartRequest),
    Exec(ExecRequest),
    /// Receive a tar stream and unpack it at `destination`.
    CopyIn { destination: String },
    /// Send `source` as a tar stream.
    CopyOut { source: String },
    /// Flush every filesystem, answer, and power the VM off.
    Stop,
}

/// What the agent answers to a detached exec or a start.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Accepted {
    pub pid: u32,
}

/// A byte stream the protocol can run over: a vsock socket in the guest, a
/// Unix socket on the host.
pub trait Transport: Read + Write + Send + Sized + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    /// Wake any thread blocked reading this connection.
    fn shutdown(&self) -> io::Result<()>;
}

impl Transport for std::os::unix::net::UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        std::os::unix::net::UnixStream::try_clone(self)
    }

    fn shutdown(&self) -> io::Result<()> {
        std::os::unix::net::UnixStream::shutdown(self, std::net::Shutdown::Both)
    }
}

/// Serializes writers: several threads (stdout, stderr, exit) share one connection.
pub struct FrameSender<T: Transport> {
    inner: Arc<Mutex<T>>,
}

impl<T: Transport> Clone for FrameSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Transport> FrameSender<T> {
    pub fn new(transport: T) -> Self {
        Self {
            inner: Arc::new(Mutex::new(transport)),
        }
    }

    pub fn send(&self, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "microvm protocol frame is too large",
            ));
        }
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(kind as u8);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        let mut transport = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("microvm protocol writer lock was poisoned"))?;
        transport.write_all(&frame)?;
        transport.flush()
    }

    pub fn send_json<V: Serialize>(&self, kind: FrameKind, value: &V) -> Result<()> {
        let payload = serde_json::to_vec(value).context("encode microvm protocol message")?;
        self.send(kind, &payload)
            .context("write microvm protocol frame")
    }

    /// Send `data` as data frames of at most [`CHUNK_SIZE`] bytes.
    pub fn send_data(&self, kind: FrameKind, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(CHUNK_SIZE) {
            self.send(kind, chunk)?;
        }
        Ok(())
    }

    pub fn shutdown(&self) -> io::Result<()> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("microvm protocol writer lock was poisoned"))?
            .shutdown()
    }
}

/// Reads frames from one connection.
pub struct FrameReader<T: Read> {
    inner: io::BufReader<T>,
}

impl<T: Read> FrameReader<T> {
    pub fn new(transport: T) -> Self {
        Self {
            inner: io::BufReader::with_capacity(CHUNK_SIZE, transport),
        }
    }

    /// The next frame, or `None` on a clean end of stream between frames.
    pub fn next(&mut self) -> Result<Option<Frame>> {
        let mut header = [0u8; 5];
        match self.inner.read_exact(&mut header[..1]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error).context("read microvm protocol frame"),
        }
        self.inner
            .read_exact(&mut header[1..])
            .context("read microvm protocol frame header")?;
        let kind = FrameKind::from_byte(header[0])?;
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if length > MAX_FRAME_SIZE {
            bail!("microvm protocol frame of {length} bytes exceeds the {MAX_FRAME_SIZE} byte limit");
        }
        let mut payload = vec![0u8; length];
        self.inner
            .read_exact(&mut payload)
            .context("read microvm protocol frame payload")?;
        Ok(Some(Frame { kind, payload }))
    }

    /// The next frame; end of stream is an error because the conversation
    /// is not over.
    pub fn expect(&mut self, what: &str) -> Result<Frame> {
        self.next()?
            .ok_or_else(|| anyhow!("connection closed while waiting for {what}"))
    }
}

/// Reads the tar stream of a copy: tar frames until `TarEnd`.
pub struct TarFrameReader<'a, T: Read> {
    frames: &'a mut FrameReader<T>,
    current: Vec<u8>,
    offset: usize,
    finished: bool,
}

impl<'a, T: Read> TarFrameReader<'a, T> {
    pub fn new(frames: &'a mut FrameReader<T>) -> Self {
        Self {
            frames,
            current: Vec::new(),
            offset: 0,
            finished: false,
        }
    }

    /// Consume whatever remains of the stream, so the connection is left at
    /// the frame after `TarEnd`.
    pub fn finish(&mut self) -> Result<()> {
        let mut sink = [0u8; 4096];
        while !self.finished {
            if self.read(&mut sink)? == 0 {
                break;
            }
        }
        Ok(())
    }
}

impl<T: Read> Read for TarFrameReader<'_, T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.offset < self.current.len() {
                let count = buffer.len().min(self.current.len() - self.offset);
                buffer[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
                self.offset += count;
                return Ok(count);
            }
            if self.finished {
                return Ok(0);
            }
            let frame = self
                .frames
                .expect("tar data")
                .map_err(|error| io::Error::other(format!("{error:#}")))?;
            match frame.kind {
                FrameKind::Tar => {
                    self.current = frame.payload;
                    self.offset = 0;
                }
                FrameKind::TarEnd => {
                    self.finished = true;
                    return Ok(0);
                }
                FrameKind::Error => {
                    return Err(io::Error::other(String::from_utf8_lossy(&frame.payload).into_owned()));
                }
                other => {
                    return Err(io::Error::other(format!(
                        "unexpected {other:?} frame inside a tar stream"
                    )));
                }
            }
        }
    }
}

/// Writes a tar stream as tar frames.
pub struct TarFrameWriter<T: Transport> {
    sender: FrameSender<T>,
    pending: Vec<u8>,
}

impl<T: Transport> TarFrameWriter<T> {
    pub fn new(sender: FrameSender<T>) -> Self {
        Self {
            sender,
            pending: Vec::with_capacity(CHUNK_SIZE),
        }
    }

    /// Flush the last partial frame and end the stream.
    pub fn finish(mut self) -> io::Result<()> {
        self.flush_pending()?;
        self.sender.send(FrameKind::TarEnd, &[])
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.sender.send(FrameKind::Tar, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

impl<T: Transport> Write for TarFrameWriter<T> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let room = CHUNK_SIZE - self.pending.len();
        let count = data.len().min(room);
        self.pending.extend_from_slice(&data[..count]);
        if self.pending.len() >= CHUNK_SIZE {
            self.flush_pending()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Frame boundaries are not stream boundaries; `finish` ends the stream.
        Ok(())
    }
}
