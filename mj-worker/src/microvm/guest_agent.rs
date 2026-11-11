//! The guest agent: PID 1 inside a microVM, serving the vsock protocol.
//!
//! The initrd's `/init` prepares the machine (modules, mounts, network) and
//! executes this binary as `mj-worker microvm guest-agent` in the new root.
//! The agent reaps children, listens on vsock port [`AGENT_PORT`], runs exec
//! sessions and copies for the host engine, and powers the VM off when the
//! container's main process exits or the engine asks it to stop.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};

use super::protocol::{
    Accepted, CHUNK_SIZE, ExecRequest, ExitInfo, FrameKind, FrameReader, FrameSender, Hello,
    Request, StartRequest, TarFrameReader, TarFrameWriter, Transport, negotiate,
};
use super::tarcopy;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// ---------------------------------------------------------------------------
// System operations the agent performs on the machine it runs in.

/// Whole-machine operations. The real implementation acts on the VM; tests
/// record the calls.
pub trait GuestSystem: Send + Sync + 'static {
    /// Kill every other process, write all data to disk, and unmount.
    fn quiesce(&self);
    /// Turn the machine off. The real implementation does not return.
    fn power_off(&self);
}

/// A finished-or-failed wait on a child.
pub type WaitResult = Result<ExitStatus, String>;

/// A process the agent started.
pub struct Spawned {
    pub pid: u32,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub exit: Receiver<WaitResult>,
}

/// How the agent starts processes and learns how they ended.
///
/// As PID 1 the agent must reap orphans too, so a single reaper owns every
/// `waitpid` and hands exit statuses to whoever started the process.
pub trait ProcessSpawner: Send + Sync + 'static {
    fn spawn(&self, command: &mut Command) -> io::Result<Spawned>;
}

/// Waits for each child on its own thread. Used where the process is not PID 1.
pub struct DirectSpawner;

impl ProcessSpawner for DirectSpawner {
    fn spawn(&self, command: &mut Command) -> io::Result<Spawned> {
        let mut child = command.spawn()?;
        let pid = child.id();
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (sender, exit) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("wait-{pid}"))
            .spawn(move || {
                let result = child.wait().map_err(|error| error.to_string());
                // The receiver is gone when nobody waits for this exit (a
                // detached process); there is then nothing to report.
                let _ = sender.send(result);
            })?;
        Ok(Spawned {
            pid,
            stdin,
            stdout,
            stderr,
            exit,
        })
    }
}

/// The PID 1 reaper: one thread calls `waitpid(-1)` and routes statuses by pid.
pub struct Reaper {
    waiters: Mutex<HashMap<i32, mpsc::Sender<WaitResult>>>,
}

impl Reaper {
    /// Block SIGCHLD in the calling thread (and so in every thread it spawns
    /// afterwards), then start the reaper thread. Call before any other thread exists.
    pub fn start() -> Result<Arc<Self>> {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is a valid sigset_t for the sigemptyset/sigaddset/pthread_sigmask calls.
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result)).context("block SIGCHLD");
            }
        }
        let reaper = Arc::new(Self {
            waiters: Mutex::new(HashMap::new()),
        });
        let thread_reaper = Arc::clone(&reaper);
        std::thread::Builder::new()
            .name("reaper".to_owned())
            .spawn(move || thread_reaper.run(set))
            .context("start reaper thread")?;
        Ok(reaper)
    }

    fn run(&self, set: libc::sigset_t) {
        loop {
            let mut signal = 0;
            // SAFETY: `set` is initialized and `signal` is a valid out pointer.
            let result = unsafe { libc::sigwait(&set, &mut signal) };
            if result != 0 {
                klog(&format!("reaper: sigwait failed: errno {result}"));
                continue;
            }
            // Reap under the spawn lock. A spawn that fails to exec waits for
            // its own half-started child, and std panics if this thread has
            // already collected it.
            let mut waiters = self.lock();
            loop {
                let mut status = 0;
                // SAFETY: `status` is a valid out pointer.
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid <= 0 {
                    break;
                }
                if let Some(waiter) = waiters.remove(&pid) {
                    // The starter may have stopped listening (a detached exec).
                    let _ = waiter.send(Ok(ExitStatus::from_raw(status)));
                }
            }
        }
    }

    /// The table is a plain map, so a panic elsewhere cannot leave it inconsistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i32, mpsc::Sender<WaitResult>>> {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ProcessSpawner for Reaper {
    fn spawn(&self, command: &mut Command) -> io::Result<Spawned> {
        // Holding the lock across the spawn means the reaper cannot see the
        // child's exit before its waiter is registered.
        let mut waiters = self.lock();
        let mut child: Child = command.spawn()?;
        let pid = child.id();
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (sender, exit) = mpsc::channel();
        waiters.insert(pid as i32, sender);
        Ok(Spawned {
            pid,
            stdin,
            stdout,
            stderr,
            exit,
        })
    }
}

// ---------------------------------------------------------------------------
// Users

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedUser {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: String,
    pub name: Option<String>,
}

/// Resolve `name`, `uid`, `name:group` or `uid:gid` against passwd and group files.
pub fn resolve_user(spec: &str, passwd: &str, group: &str) -> Result<ResolvedUser> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((user, group)) => (user, Some(group)),
        None => (spec, None),
    };
    let user_part = if user_part.is_empty() { "0" } else { user_part };
    let entry = |matches: &dyn Fn(&[&str]) -> bool| -> Option<Vec<String>> {
        passwd
            .lines()
            .map(|line| line.split(':').collect::<Vec<_>>())
            .find(|fields| fields.len() >= 6 && matches(fields))
            .map(|fields| fields.iter().map(|field| (*field).to_owned()).collect())
    };
    let numeric = user_part.parse::<u32>().ok();
    let found = match numeric {
        Some(uid) => entry(&|fields| fields[2].parse::<u32>().ok() == Some(uid)),
        None => entry(&|fields| fields[0] == user_part),
    };
    let (uid, primary_gid, home, name) = match (&found, numeric) {
        (Some(fields), _) => (
            fields[2].parse::<u32>().context("passwd uid")?,
            fields[3].parse::<u32>().context("passwd gid")?,
            fields[5].clone(),
            Some(fields[0].clone()),
        ),
        (None, Some(uid)) => (uid, uid, "/".to_owned(), None),
        (None, None) => {
            bail!("unable to find user {user_part}: no matching entries in passwd file")
        }
    };
    let group_entries: Vec<Vec<&str>> = group
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .filter(|fields| fields.len() >= 3)
        .collect();
    let gid = match group_part {
        None => primary_gid,
        Some(spec) => match spec.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => group_entries
                .iter()
                .find(|fields| fields[0] == spec)
                .and_then(|fields| fields[2].parse::<u32>().ok())
                .ok_or_else(|| anyhow!("unable to find group {spec}"))?,
        },
    };
    let mut groups = Vec::new();
    if let Some(name) = &name {
        for fields in &group_entries {
            if fields
                .get(3)
                .is_some_and(|members| members.split(',').any(|m| m == name))
            {
                if let Ok(gid) = fields[2].parse::<u32>() {
                    groups.push(gid);
                }
            }
        }
    }
    Ok(ResolvedUser {
        uid,
        gid,
        groups,
        home,
        name,
    })
}

fn resolve_user_in_guest(spec: &str) -> Result<ResolvedUser> {
    // No user named means the agent's own identity (root in a real guest).
    // SAFETY: geteuid has no preconditions.
    let own = unsafe { libc::geteuid() }.to_string();
    let spec = if spec.is_empty() { own.as_str() } else { spec };
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let group = fs::read_to_string("/etc/group").unwrap_or_default();
    resolve_user(spec, &passwd, &group)
}

// ---------------------------------------------------------------------------
// The agent

pub struct Agent {
    defaults: Mutex<StartRequest>,
    started: AtomicBool,
    spawner: Arc<dyn ProcessSpawner>,
    system: Arc<dyn GuestSystem>,
    /// Where `/run/user/<uid>` directories are made.
    run_user_root: PathBuf,
}

/// A spawn failure that maps to a shell exit status.
struct SpawnFailure {
    status: i32,
    message: String,
}

impl Agent {
    pub fn new(spawner: Arc<dyn ProcessSpawner>, system: Arc<dyn GuestSystem>) -> Arc<Self> {
        Arc::new(Self {
            defaults: Mutex::new(StartRequest::default()),
            started: AtomicBool::new(false),
            spawner,
            system,
            run_user_root: PathBuf::from("/run/user"),
        })
    }

    /// Serve one connection: hello, one request, its data frames.
    pub fn serve<T: Transport>(self: &Arc<Self>, transport: T) -> Result<()> {
        let sender = FrameSender::new(transport.try_clone().context("clone connection")?);
        let mut frames = FrameReader::new(transport);
        let hello = frames.expect("the client hello")?;
        if hello.kind != FrameKind::Hello {
            bail!("expected a hello frame, received {:?}", hello.kind);
        }
        let theirs: Hello =
            serde_json::from_slice(&hello.payload).context("decode client hello")?;
        let ours = Hello::current();
        if let Err(error) = negotiate(&ours, &theirs) {
            // Tell the peer why, then close; a peer too old to read this still
            // sees the connection end.
            let _ = sender.send(FrameKind::Error, format!("{error:#}").as_bytes());
            let _ = sender.shutdown();
            return Err(error);
        }
        sender.send_json(FrameKind::Hello, &ours)?;
        let request = frames.expect("a request")?;
        if request.kind != FrameKind::Request {
            bail!("expected a request frame, received {:?}", request.kind);
        }
        let request: Request =
            serde_json::from_slice(&request.payload).context("decode request")?;
        let result = match request {
            Request::Start(start) => self.handle_start(&sender, start),
            Request::Exec(exec) => self.handle_exec(&sender, frames, exec),
            Request::CopyIn { destination } => {
                self.handle_copy_in(&sender, &mut frames, &destination)
            }
            Request::CopyOut { source } => self.handle_copy_out(&sender, &source),
            Request::Stop => self.handle_stop(&sender),
        };
        if let Err(error) = &result {
            // The client may already be gone; its absence is not a second failure.
            let _ = sender.send(FrameKind::Error, format!("{error:#}").as_bytes());
        }
        result
    }

    fn handle_start<T: Transport>(
        self: &Arc<Self>,
        sender: &FrameSender<T>,
        start: StartRequest,
    ) -> Result<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            bail!("the container has already been started");
        }
        let started = self.start_main(&start);
        if started.is_err() {
            self.started.store(false, Ordering::SeqCst);
        }
        let pid = started?;
        sender.send_json(FrameKind::Accepted, &Accepted { pid })?;
        Ok(())
    }

    fn start_main(self: &Arc<Self>, start: &StartRequest) -> Result<u32> {
        let user = resolve_user_in_guest(&start.user)?;
        for volume in start.volumes.iter().filter(|volume| volume.chown) {
            lchown(Path::new(&volume.destination), user.uid, user.gid)
                .with_context(|| format!("give {} to the container user", volume.destination))?;
        }
        *self
            .defaults
            .lock()
            .map_err(|_| anyhow!("agent state lock was poisoned"))? = start.clone();
        let request = ExecRequest {
            argv: start.argv.clone(),
            detach: true,
            ..ExecRequest::default()
        };
        let spawned = self
            .spawn_process(&request, Stdio::null(), Stdio::null(), Stdio::null())
            .map_err(|failure| anyhow!("start the container command: {}", failure.message))?;
        let pid = spawned.pid;
        let agent = Arc::clone(self);
        std::thread::Builder::new()
            .name("main-process".to_owned())
            .spawn(move || {
                match spawned.exit.recv() {
                    Ok(Ok(status)) => klog(&format!("container command exited: {status}")),
                    Ok(Err(error)) => klog(&format!(
                        "waiting for the container command failed: {error}"
                    )),
                    Err(_) => klog("the container command's exit was lost"),
                }
                agent.system.quiesce();
                agent.system.power_off();
            })
            .context("start main-process monitor")?;
        Ok(pid)
    }

    fn handle_stop<T: Transport>(&self, sender: &FrameSender<T>) -> Result<()> {
        self.system.quiesce();
        sender.send_json(
            FrameKind::Exit,
            &ExitInfo {
                code: Some(0),
                signal: None,
            },
        )?;
        self.system.power_off();
        Ok(())
    }

    /// Build and spawn the command for an exec request (or the main process).
    fn spawn_process(
        &self,
        request: &ExecRequest,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Spawned, SpawnFailure> {
        let defaults = self
            .defaults
            .lock()
            .map_err(|_| failure(125, "agent state lock was poisoned"))?
            .clone();
        let user_spec = request
            .user
            .clone()
            .unwrap_or_else(|| defaults.user.clone());
        let user = resolve_user_in_guest(&user_spec)
            .map_err(|error| failure(126, format!("{error:#}")))?;
        let mut environment: Vec<(String, String)> = Vec::new();
        for entry in defaults.env.iter().chain(request.env.iter()) {
            let (key, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            environment.retain(|(existing, _)| existing != key);
            environment.push((key.to_owned(), value.to_owned()));
        }
        let lookup = |key: &str, environment: &[(String, String)]| {
            environment.iter().any(|(existing, _)| existing == key)
        };
        if !lookup("PATH", &environment) {
            environment.push(("PATH".to_owned(), DEFAULT_PATH.to_owned()));
        }
        if !lookup("HOME", &environment) {
            environment.push(("HOME".to_owned(), user.home.clone()));
        }
        // SAFETY: geteuid has no preconditions.
        let current_uid = unsafe { libc::geteuid() };
        if user.uid != 0 && user.uid != current_uid && !lookup("XDG_RUNTIME_DIR", &environment) {
            // Rootless nested containers expect this directory; no logind makes it here.
            let directory = self.run_user_root.join(user.uid.to_string());
            ensure_runtime_directory(&directory, &user).map_err(|error| {
                failure(126, format!("prepare {}: {error:#}", directory.display()))
            })?;
            environment.push((
                "XDG_RUNTIME_DIR".to_owned(),
                directory.display().to_string(),
            ));
        }
        let path = environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let cwd = request
            .cwd
            .clone()
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| Some(defaults.cwd.clone()).filter(|cwd| !cwd.is_empty()))
            .unwrap_or_else(|| "/".to_owned());
        let program = request
            .argv
            .first()
            .ok_or_else(|| failure(125, "the command is empty"))?;
        let program = resolve_program(program, &path).ok_or_else(|| {
            failure(
                127,
                format!("executable file `{program}` not found in $PATH"),
            )
        })?;
        let mut command = Command::new(program);
        command
            .args(&request.argv[1..])
            .env_clear()
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .current_dir(&cwd)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        let detach = request.detach;
        let (uid, gid, groups) = (user.uid, user.gid, user.groups.clone());
        let switch_user = uid != current_uid;
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions (setsid, setgroups, setgid, setuid).
        unsafe {
            command.pre_exec(move || {
                if detach && libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if switch_user {
                    if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        self.spawner
            .spawn(&mut command)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound => failure(127, format!("{}: {error}", request.argv[0])),
                io::ErrorKind::PermissionDenied => {
                    failure(126, format!("{}: {error}", request.argv[0]))
                }
                _ => failure(126, format!("start {}: {error}", request.argv[0])),
            })
    }

    fn handle_exec<T: Transport>(
        self: &Arc<Self>,
        sender: &FrameSender<T>,
        mut frames: FrameReader<T>,
        request: ExecRequest,
    ) -> Result<()> {
        if request.detach {
            return match self.spawn_process(&request, Stdio::null(), Stdio::null(), Stdio::null()) {
                Ok(spawned) => {
                    sender.send_json(FrameKind::Accepted, &Accepted { pid: spawned.pid })?;
                    Ok(())
                }
                Err(failure) => bail!("{}", failure.message),
            };
        }
        let stdin_mode = if request.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        };
        let mut spawned =
            match self.spawn_process(&request, stdin_mode, Stdio::piped(), Stdio::piped()) {
                Ok(spawned) => spawned,
                Err(failure) => {
                    let _ = sender.send(
                        FrameKind::Stderr,
                        format!("Error: {}\n", failure.message).as_bytes(),
                    );
                    sender.send_json(
                        FrameKind::Exit,
                        &ExitInfo {
                            code: Some(failure.status),
                            signal: None,
                        },
                    )?;
                    return Ok(());
                }
            };
        let wake = Arc::new(eventfd()?);
        let mut pumps: Vec<JoinHandle<()>> = Vec::new();
        for (source, kind, name) in [
            (
                spawned.stdout.take().map(Source::Stdout),
                FrameKind::Stdout,
                "stdout",
            ),
            (
                spawned.stderr.take().map(Source::Stderr),
                FrameKind::Stderr,
                "stderr",
            ),
        ] {
            let Some(source) = source else { continue };
            let (sender, wake) = (sender.clone(), Arc::clone(&wake));
            pumps.push(
                std::thread::Builder::new()
                    .name(format!("pump-{name}"))
                    .spawn(move || {
                        if let Err(error) = pump(source, &wake, kind, &sender) {
                            klog(&format!("exec {name} pump failed: {error:#}"));
                        }
                    })
                    .context("start output pump")?,
            );
        }
        let monitor_sender = sender.clone();
        let exit = spawned.exit;
        let monitor_wake = Arc::clone(&wake);
        std::thread::Builder::new()
            .name("exec-monitor".to_owned())
            .spawn(move || {
                let status = exit.recv();
                wake_pumps(&monitor_wake);
                for pump in pumps {
                    if pump.join().is_err() {
                        klog("an output pump panicked");
                    }
                }
                let info = match status {
                    Ok(Ok(status)) => ExitInfo {
                        code: status.code(),
                        signal: status.signal(),
                    },
                    Ok(Err(error)) => {
                        klog(&format!("waiting for an exec process failed: {error}"));
                        ExitInfo {
                            code: Some(125),
                            signal: None,
                        }
                    }
                    Err(_) => ExitInfo {
                        code: Some(125),
                        signal: None,
                    },
                };
                // A client that went away cannot be told; that is not an error.
                let _ = monitor_sender.send_json(FrameKind::Exit, &info);
                let _ = monitor_sender.shutdown();
            })
            .context("start exec monitor")?;
        // Feed stdin until the client says it is done, or goes away.
        let mut stdin = spawned.stdin.take();
        loop {
            let frame = match frames.next() {
                Ok(Some(frame)) => frame,
                // The monitor shut the connection down after the exit frame, or
                // the client left. Either way the child's stdin ends here.
                Ok(None) | Err(_) => break,
            };
            match frame.kind {
                FrameKind::Stdin => {
                    if let Some(pipe) = stdin.as_mut()
                        && let Err(error) = pipe.write_all(&frame.payload)
                    {
                        // The process closed its stdin or exited; later input
                        // has nowhere to go and is dropped.
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            klog(&format!("writing exec stdin failed: {error}"));
                        }
                        stdin = None;
                    }
                }
                FrameKind::StdinEof => stdin = None,
                other => bail!("unexpected {other:?} frame during exec"),
            }
        }
        Ok(())
    }

    fn handle_copy_in<T: Transport>(
        &self,
        sender: &FrameSender<T>,
        frames: &mut FrameReader<T>,
        destination: &str,
    ) -> Result<()> {
        let defaults = self
            .defaults
            .lock()
            .map_err(|_| anyhow!("agent state lock was poisoned"))?
            .clone();
        let user = resolve_user_in_guest(&defaults.user)?;
        // SAFETY: geteuid has no preconditions.
        let owner = (unsafe { libc::geteuid() } == 0).then_some((user.uid, user.gid));
        let mut reader = TarFrameReader::new(frames);
        tarcopy::unpack(&mut reader, Path::new(destination), owner)?;
        reader.finish()?;
        sender.send_json(
            FrameKind::Exit,
            &ExitInfo {
                code: Some(0),
                signal: None,
            },
        )?;
        Ok(())
    }

    fn handle_copy_out<T: Transport>(&self, sender: &FrameSender<T>, source: &str) -> Result<()> {
        let writer = TarFrameWriter::new(sender.clone());
        let writer = tarcopy::pack(Path::new(source), writer)?;
        writer.finish().context("finish tar stream")?;
        sender.send_json(
            FrameKind::Exit,
            &ExitInfo {
                code: Some(0),
                signal: None,
            },
        )?;
        Ok(())
    }
}

fn failure(status: i32, message: impl Into<String>) -> SpawnFailure {
    SpawnFailure {
        status,
        message: message.into(),
    }
}

fn resolve_program(program: &str, path: &str) -> Option<OsString> {
    if program.contains('/') {
        return Some(program.into());
    }
    path.split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(program))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(PathBuf::into_os_string)
}

fn ensure_runtime_directory(directory: &Path, user: &ResolvedUser) -> Result<()> {
    if !directory.is_dir() {
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        lchown(directory, user.uid, user.gid)?;
    }
    Ok(())
}

fn lchown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `path_bytes` is a valid NUL-terminated string for the call.
    if unsafe { libc::lchown(path_bytes.as_ptr(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("change owner of {}", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output pumps

enum Source {
    Stdout(ChildStdout),
    Stderr(ChildStderr),
}

impl Source {
    fn fd(&self) -> RawFd {
        match self {
            Source::Stdout(pipe) => pipe.as_raw_fd(),
            Source::Stderr(pipe) => pipe.as_raw_fd(),
        }
    }
}

impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Source::Stdout(pipe) => pipe.read(buffer),
            Source::Stderr(pipe) => pipe.read(buffer),
        }
    }
}

fn eventfd() -> Result<OwnedFd> {
    // SAFETY: eventfd returns a new descriptor or -1.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("create eventfd");
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn wake_pumps(wake: &OwnedFd) {
    let one: u64 = 1;
    // SAFETY: writes 8 bytes from a valid u64 to an eventfd.
    let written = unsafe { libc::write(wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
    if written != 8 {
        klog("could not wake the output pumps");
    }
}

/// Copy a child's output into frames until the pipe closes, or until the child
/// has exited and what it left in the pipe is drained. A descendant that keeps
/// the pipe open after the process exits must not hold the exec session open.
fn pump<T: Transport>(
    mut source: Source,
    wake: &OwnedFd,
    kind: FrameKind,
    sender: &FrameSender<T>,
) -> Result<()> {
    let fd = source.fd();
    // SAFETY: fcntl on a descriptor this function owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error()).context("make output pipe non-blocking");
        }
    }
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut client_gone = false;
    let mut exited = false;
    loop {
        if !exited {
            let mut fds = [
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: `fds` is a valid array of two pollfd.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("poll exec output");
            }
            exited = fds[1].revents != 0;
        }
        // Drain what is readable now.
        loop {
            match source.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    if !client_gone && sender.send(kind, &buffer[..count]).is_err() {
                        // The client left. Keep draining so the process is
                        // not blocked on a full pipe.
                        client_gone = true;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("read exec output"),
            }
        }
        if exited {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Logging

/// Write a line to the kernel log, which the VM's console shows.
pub fn klog(message: &str) {
    // Level 3 (error) shows on the console, which init limits to errors and worse.
    let line = format!("<3>mj-microvm-agent: {message}\n");
    let written = fs::OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .and_then(|mut kmsg| kmsg.write_all(line.as_bytes()));
    if written.is_err() {
        // No kernel log (not in a VM): stderr is the next best place.
        eprint!("{line}");
    }
}

// ---------------------------------------------------------------------------
// vsock

/// A connected AF_VSOCK stream socket.
pub struct VsockStream(OwnedFd);

impl Read for VsockStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: reads into a valid buffer of the stated length.
            let count =
                unsafe { libc::read(self.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if count >= 0 {
                return Ok(count as usize);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            // SAFETY: writes from a valid buffer of the stated length.
            let count =
                unsafe { libc::write(self.0.as_raw_fd(), buffer.as_ptr().cast(), buffer.len()) };
            if count >= 0 {
                return Ok(count as usize);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Transport for VsockStream {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    fn shutdown(&self) -> io::Result<()> {
        // SAFETY: shutdown on a descriptor this value owns.
        if unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_RDWR) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn vsock_listen(port: u32) -> Result<OwnedFd> {
    // SAFETY: socket returns a new descriptor or -1.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("create vsock socket");
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    address.svm_port = port;
    address.svm_cid = libc::VMADDR_CID_ANY;
    // SAFETY: `address` is a valid sockaddr_vm of the stated size.
    let bound = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&address as *const libc::sockaddr_vm).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if bound != 0 {
        return Err(io::Error::last_os_error()).context("bind vsock port");
    }
    // SAFETY: listen on a bound socket.
    if unsafe { libc::listen(fd.as_raw_fd(), 64) } != 0 {
        return Err(io::Error::last_os_error()).context("listen on vsock port");
    }
    Ok(fd)
}

fn vsock_accept(listener: &OwnedFd) -> io::Result<VsockStream> {
    loop {
        // SAFETY: accept4 with null address pointers is permitted.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` is a fresh descriptor this function owns.
            return Ok(VsockStream(unsafe { OwnedFd::from_raw_fd(fd) }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

// ---------------------------------------------------------------------------
// PID 1

static POWERING_OFF: AtomicBool = AtomicBool::new(false);

struct MachineSystem;

impl GuestSystem for MachineSystem {
    fn quiesce(&self) {
        if POWERING_OFF.swap(true, Ordering::SeqCst) {
            // Another path (stop request or main-process exit) is already
            // shutting the machine down.
            loop {
                std::thread::park();
            }
        }
        // SAFETY: SIGKILL to every process except this one (PID 1 is exempt).
        unsafe {
            libc::kill(-1, libc::SIGKILL);
        }
        // Give the killed processes a moment to release their mounts. Their
        // exit is observed by the reaper, but nothing needs to wait on it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: sync has no preconditions.
        unsafe { libc::sync() };
        unmount_data_filesystems();
        // SAFETY: remount the root read-only so its journal is clean.
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_RDONLY,
                std::ptr::null(),
            );
            libc::sync();
        }
    }

    fn power_off(&self) {
        // SAFETY: powers the machine off; the call does not return on success.
        unsafe {
            libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
        }
        klog(&format!("power off failed: {}", io::Error::last_os_error()));
        loop {
            std::thread::park();
        }
    }
}

/// Unmount every ext4 filesystem except the root, newest first.
fn unmount_data_filesystems() {
    let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") else {
        klog("cannot read mountinfo; leaving filesystems mounted");
        return;
    };
    let mut points: Vec<String> = mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let fstype = after.split(' ').next()?;
            let mount_point = before.split(' ').nth(4)?;
            (fstype == "ext4" && mount_point != "/").then(|| mount_point.replace("\\040", " "))
        })
        .collect();
    points.reverse();
    for point in points {
        let Ok(path) = std::ffi::CString::new(point.clone()) else {
            continue;
        };
        // SAFETY: plain umount of a NUL-terminated path.
        if unsafe { libc::umount2(path.as_ptr(), 0) } != 0 {
            klog(&format!(
                "unmount {point} failed: {}",
                io::Error::last_os_error()
            ));
        }
    }
}

/// `mj-worker microvm guest-agent`: run as PID 1 in the guest.
pub fn run_guest_agent() -> Result<()> {
    let reaper = Reaper::start()?;
    let agent = Agent::new(reaper, Arc::new(MachineSystem));
    let listener = vsock_listen(super::protocol::AGENT_PORT)?;
    klog("listening");
    loop {
        match vsock_accept(&listener) {
            Ok(connection) => {
                let agent = Arc::clone(&agent);
                let spawned = std::thread::Builder::new()
                    .name("connection".to_owned())
                    .spawn(move || {
                        if let Err(error) = agent.serve(connection) {
                            klog(&format!("connection failed: {error:#}"));
                        }
                    });
                if let Err(error) = spawned {
                    klog(&format!("could not start a connection thread: {error}"));
                }
            }
            Err(error) => klog(&format!("accept failed: {error}")),
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    /// Records stop calls instead of powering anything off.
    #[derive(Default)]
    pub struct RecordingSystem {
        pub quiesced: AtomicBool,
        pub powered_off: AtomicBool,
    }

    impl GuestSystem for RecordingSystem {
        fn quiesce(&self) {
            self.quiesced.store(true, Ordering::SeqCst);
        }

        fn power_off(&self) {
            self.powered_off.store(true, Ordering::SeqCst);
        }
    }
}
