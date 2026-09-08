#![cfg(unix)]

use std::{
    fs,
    fs::File,
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Empty-session text that appears after the combined dashboard is ready.
const READY_MARKER: &[u8] = b"Prompt (no live session)";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Alt-Q, which a terminal sends as Escape followed by the letter. Alt is the
/// same modifier on every platform, so unlike the old Ctrl-Q this needs no
/// per-platform encoding.
const QUIT_KEY: &[u8] = b"\x1bq";

/// The DECSET pair crossterm 0.29 writes for `EnableMouseCapture` and
/// `DisableMouseCapture`. Both are single writes, so any one sequence stands
/// for the whole switch.
const MOUSE_CAPTURE_ENABLE: [&str; 5] = [
    "\x1b[?1000h",
    "\x1b[?1002h",
    "\x1b[?1003h",
    "\x1b[?1015h",
    "\x1b[?1006h",
];
const MOUSE_CAPTURE_DISABLE: [&str; 5] = [
    "\x1b[?1006l",
    "\x1b[?1015l",
    "\x1b[?1003l",
    "\x1b[?1002l",
    "\x1b[?1000l",
];

/// The last offset at which any of `sequences` appears, so ordering against
/// the alternate-screen leave does not depend on which one the write ended on.
fn last_index(output: &str, sequences: [&str; 5]) -> usize {
    sequences
        .iter()
        .filter_map(|sequence| output.rfind(sequence))
        .max()
        .unwrap_or_else(|| panic!("missing mouse capture disable: {output:?}"))
}

struct ReapChild(Option<Child>);

impl ReapChild {
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already reaped")
    }

    fn take(&mut self) -> Child {
        self.0.take().expect("child already reaped")
    }
}

impl Drop for ReapChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn termios(fd: RawFd) -> libc::termios {
    let mut value = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(fd, &mut value) },
        0,
        "read termios"
    );
    value
}

fn stable_local_flags(flags: libc::tcflag_t) -> libc::tcflag_t {
    #[cfg(target_vendor = "apple")]
    {
        // PENDIN is transient kernel state, not a terminal mode preference.
        flags & !libc::PENDIN
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        flags
    }
}

fn duplicate(fd: RawFd) -> File {
    let copy = unsafe { libc::dup(fd) };
    assert!(copy >= 0, "duplicate PTY fd");
    unsafe { File::from_raw_fd(copy) }
}

fn drain(master: &mut File, output: &mut Vec<u8>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            // Linux returns EIO when the slave side closes.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("read PTY output: {error}"),
        }
    }
}

fn wait_for_output(master: &mut File, output: &mut Vec<u8>, marker: &[u8], deadline: Instant) {
    while !output.windows(marker.len()).any(|window| window == marker) {
        drain(master, output);
        assert!(
            Instant::now() < deadline,
            "PTY child did not emit {marker:?}; output: {:?}",
            String::from_utf8_lossy(output)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(
    child: &mut Child,
    master: &mut File,
    output: &mut Vec<u8>,
    reason: &str,
) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        drain(master, output);
        if let Some(status) = child.try_wait().expect("poll PTY child") {
            return status;
        }
        if Instant::now() >= deadline {
            let signal_status = fs::read_to_string(format!("/proc/{}/status", child.id()))
                .ok()
                .map(|status| {
                    status
                        .lines()
                        .filter(|line| {
                            [
                                "State:", "Threads:", "SigQ:", "SigPnd:", "ShdPnd:", "SigBlk:",
                                "SigIgn:", "SigCgt:",
                            ]
                            .iter()
                            .any(|prefix| line.starts_with(prefix))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "unavailable".into());
            panic!(
                "PTY child did not exit after {reason}; process status: {signal_status}; output: {:?}",
                String::from_utf8_lossy(output)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct DashboardStorage {
    directory: Option<tempfile::TempDir>,
    stop_daemon: bool,
}

impl DashboardStorage {
    fn path(&self) -> &std::path::Path {
        self.directory.as_ref().expect("fixture storage").path()
    }
}

impl Drop for DashboardStorage {
    fn drop(&mut self) {
        if self.stop_daemon {
            // Stop the writer before removing its database and working files.
            match Command::new(env!("CARGO_BIN_EXE_mj"))
                .args(["daemon", "stop"])
                .env("MJ_CONFIG_DIR", self.path().join("config/hel"))
                .env("MJ_DATA_DIR", self.path().join("data/hel"))
                .status()
            {
                Ok(status) if status.success() => {}
                result => {
                    let retained = self.directory.take().expect("fixture storage").keep();
                    eprintln!(
                        "Could not stop PTY fixture daemon: {result:?}; retained {}",
                        retained.display()
                    );
                }
            }
        }
    }
}

struct DashboardPty {
    _storage: DashboardStorage,
    master: File,
    slave: File,
    original_termios: libc::termios,
    child: ReapChild,
}

fn spawn_dashboard_pty() -> DashboardPty {
    spawn_dashboard_pty_with_idle_exit(true)
}

fn spawn_dashboard_pty_with_idle_exit(exit_when_idle: bool) -> DashboardPty {
    spawn_dashboard_pty_fixture(exit_when_idle, false)
}

fn spawn_dashboard_pty_fixture(exit_when_idle: bool, pending_session: bool) -> DashboardPty {
    spawn_dashboard_pty_with_startup(exit_when_idle, pending_session, false)
}

fn spawn_dashboard_pty_with_startup(
    exit_when_idle: bool,
    pending_session: bool,
    local_startup: bool,
) -> DashboardPty {
    let storage = DashboardStorage {
        directory: Some(tempfile::tempdir().expect("create Hel test storage")),
        stop_daemon: !exit_when_idle,
    };
    let config_root = storage.path().join("config");
    fs::create_dir_all(config_root.join("hel")).expect("create Hel config directory");
    fs::write(
        config_root.join("hel/config.toml"),
        r#"version = 1

[phone]
enabled = false

[profiles.codex]
kind = "codex"
home = "/profiles/codex"
# Keep this terminal test independent from any Codex installation on the host.
environment = { PATH = "/hel-termination-test-no-executables" }

[bundles.hel]
primary_repo = "hel"

[[bundles.hel.repositories]]
id = "hel"
github = "BrokkAi/hel"
destination = "hel"

[targets.podman]
kind = "local-podman"
image = "ubuntu:24.04"
"#,
    )
    .expect("write Hel test config");
    if local_startup {
        let path = config_root.join("hel/config.toml");
        let mut config = hel::hel_config::HelConfig::load_from(&path).unwrap();
        let home = storage.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        config.profiles.get_mut("codex").unwrap().home = home;
        config.targets.clear();
        config.targets.insert(
            "localhost".into(),
            hel::hel_config::TargetTemplate::LocalBare,
        );
        config.save_to(&path).unwrap();
    }
    if !local_startup {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(config_root.join("hel/config.toml"))
            .unwrap();
        file.write_all(b"\n[startup]\nenabled = false\n").unwrap();
    }
    let seeded_workspace = pending_session.then(|| {
        let database = storage.path().join("data/hel/mj.sqlite3");
        let workspace = hel::hel_database::create_workspace_at(&database, "Pending session test").unwrap();
        // A durable session that the manager cannot yet adopt. No provider or
        // container process is needed to exercise the dashboard wait path.
        let session: hel::hel_state::SessionRecord = serde_json::from_value(serde_json::json!({
            "id": "pending-session", "workspace_id": workspace.id,
            "title": "Pending session", "harness_kind": "codex",
            "last_profile": "codex", "bundle_id": "hel", "target_template_id": "podman",
            "state": "running", "created_at": "2026-09-07T00:00:00Z", "updated_at": "2026-09-07T00:00:00Z"
        })).unwrap();
        let mut state = hel::hel_state::HelState::default();
        state.sessions.insert(session.id.clone(), session);
        hel::hel_database::save_state_to(&database, &state).unwrap();
        workspace.name
    });
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let window_size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::from_ref(&window_size).cast_mut(),
            )
        },
        0,
        "create PTY"
    );
    let master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let original_termios = termios(slave.as_raw_fd());
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "read PTY master flags");
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "make PTY master nonblocking"
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    if let Some(workspace) = &seeded_workspace {
        command.args(["--workspace", workspace]);
    }
    command
        .current_dir(storage.path())
        .stdin(Stdio::from(duplicate(slave.as_raw_fd())))
        .stdout(Stdio::from(duplicate(slave.as_raw_fd())))
        .stderr(Stdio::from(duplicate(slave.as_raw_fd())))
        .env("MJ_CONFIG_DIR", config_root.join("hel"))
        .env("MJ_DATA_DIR", storage.path().join("data/hel"));
    if exit_when_idle {
        command.env("MJ_DAEMON_EXIT_WHEN_IDLE", "1");
    } else {
        command
            .env_remove("MJ_DAEMON_EXIT_WHEN_IDLE")
            .env_remove("HEL_DAEMON_EXIT_WHEN_IDLE");
    }
    // Libtest may alter its signal mask. A real `hel` invocation should start
    // with SIGTERM unmasked, so establish that condition across exec.
    unsafe {
        command.pre_exec(|| {
            let mut mask = std::mem::zeroed();
            if libc::sigemptyset(&mut mask) != 0
                || libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) != 0
            {
                return Err(io::Error::last_os_error());
            }
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("spawn hel PTY helper");
    DashboardPty {
        _storage: storage,
        master,
        slave,
        original_termios,
        child: ReapChild(Some(child)),
    }
}

#[test]
fn first_launch_and_new_key_create_in_the_launch_context_without_task_input() {
    let DashboardPty {
        _storage: storage,
        mut master,
        mut child,
        ..
    } = spawn_dashboard_pty_with_startup(false, false, true);
    let database = storage.path().join("data/hel/mj.sqlite3");
    let mut output = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    let session = loop {
        drain(&mut master, &mut output);
        if database.exists()
            && let Ok(state) = hel::hel_database::load_state_from(&database)
            && let Some(session) = state.sessions.values().next()
        {
            assert_eq!(state.sessions.len(), 1);
            break session.clone();
        }
        assert!(
            Instant::now() < deadline,
            "first session was not created: {}",
            String::from_utf8_lossy(&output)
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(session.last_profile, "codex");
    assert_eq!(session.target_template_id, "localhost");
    let workspace_root = storage.path().canonicalize().unwrap();
    assert_eq!(
        session.project_directory.as_deref(),
        Some(workspace_root.as_path())
    );
    let workspaces = hel::hel_database::list_workspaces_from(&database).unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, session.workspace_id);
    assert_eq!(
        workspaces[0].name,
        storage.path().file_name().unwrap().to_string_lossy()
    );
    let rendered = String::from_utf8_lossy(&output);
    assert!(!rendered.contains("Welcome to Mjolnir setup"));
    assert!(!rendered.contains("Workspaces"));

    // New requires only the shortcut, even while the first launch is pending.
    master.write_all(b"\x1bn").unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let created = loop {
        drain(&mut master, &mut output);
        let state = hel::hel_database::load_state_from(&database).unwrap();
        if let Some(created) = state.sessions.values().find(|other| other.id != session.id) {
            assert_eq!(state.sessions.len(), 2);
            break created.clone();
        }
        assert!(
            Instant::now() < deadline,
            "one New key did not create a session: {}",
            String::from_utf8_lossy(&output)
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(created.workspace_id, session.workspace_id);
    assert_eq!(created.project_directory, session.project_directory);
    assert_eq!(created.last_profile, "codex");
    assert_eq!(created.target_template_id, "localhost");
    assert!(created.draft_input.is_empty());
    assert!(!String::from_utf8_lossy(&output).contains("What would you like to do?"));

    // This fake profile cannot launch a real agent. Quitting during its
    // background launch must still release the terminal promptly.
    master.write_all(QUIT_KEY).unwrap();
    assert!(wait_for_exit(child.child_mut(), &mut master, &mut output, "startup quit").success());
}

#[test]
fn sigterm_restores_real_pty_terminal() {
    let DashboardPty {
        _storage,
        mut master,
        slave,
        original_termios: before,
        mut child,
    } = spawn_dashboard_pty();

    let mut output = Vec::new();
    wait_for_output(
        &mut master,
        &mut output,
        READY_MARKER,
        Instant::now() + TIMEOUT,
    );
    assert_eq!(
        unsafe { libc::kill(child.child_mut().id() as i32, libc::SIGTERM) },
        0,
        "send SIGTERM to PTY child"
    );
    let status = wait_for_exit(child.child_mut(), &mut master, &mut output, "SIGTERM");
    drain(&mut master, &mut output);
    drop(child.take());

    assert!(status.success(), "PTY child exit: {status}");
    let after = termios(slave.as_raw_fd());
    assert_eq!(after.c_iflag, before.c_iflag, "restore input flags");
    assert_eq!(after.c_oflag, before.c_oflag, "restore output flags");
    assert_eq!(
        stable_local_flags(after.c_lflag),
        stable_local_flags(before.c_lflag),
        "restore local flags"
    );

    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains("\x1b[?1049h"),
        "missing alternate-screen enter: {output:?}"
    );
    assert!(
        output.contains("\x1b[?1049l"),
        "missing alternate-screen leave: {output:?}"
    );
    // Mouse capture is unconditional, and crossterm 0.29 turns it on with
    // button, drag, and SGR tracking in one write.
    for sequence in MOUSE_CAPTURE_ENABLE {
        assert!(
            output.contains(sequence),
            "missing mouse capture enable {sequence:?}: {output:?}"
        );
    }
    let disable_mouse_capture = last_index(&output, MOUSE_CAPTURE_DISABLE);
    let leave_screen = output
        .rfind("\x1b[?1049l")
        .unwrap_or_else(|| panic!("missing alternate-screen leave: {output:?}"));
    assert!(
        disable_mouse_capture < leave_screen,
        "mouse capture was disabled after leaving the alternate screen: {output:?}"
    );
    assert!(
        output.contains("\x1b[?25h"),
        "missing cursor restoration: {output:?}"
    );
}

#[test]
fn dashboard_detach_restores_terminal_then_exits_promptly_with_final_message() {
    const REATTACH_MESSAGE: &str = "Active sessions will continue working; Mjolnir will reattach to them on your next invocation.";
    let DashboardPty {
        _storage,
        mut master,
        slave,
        original_termios: before,
        mut child,
    } = spawn_dashboard_pty();
    let mut output = Vec::new();
    wait_for_output(
        &mut master,
        &mut output,
        READY_MARKER,
        Instant::now() + TIMEOUT,
    );

    let quit_started = Instant::now();
    // Alt-Q. Escape belongs to the composer and to modals now; it no longer
    // quits, so the Escape in this sequence is only the Alt prefix.
    master.write_all(QUIT_KEY).expect("send the quit key");
    let status = wait_for_exit(
        child.child_mut(),
        &mut master,
        &mut output,
        "dashboard quit",
    );
    let quit_elapsed = quit_started.elapsed();
    drain(&mut master, &mut output);
    drop(child.take());

    assert!(status.success(), "PTY child exit: {status}");
    assert!(
        quit_elapsed < Duration::from_secs(1),
        "dashboard detach took {quit_elapsed:?}"
    );
    let after = termios(slave.as_raw_fd());
    assert_eq!(after.c_iflag, before.c_iflag, "restore input flags");
    assert_eq!(after.c_oflag, before.c_oflag, "restore output flags");
    assert_eq!(
        stable_local_flags(after.c_lflag),
        stable_local_flags(before.c_lflag),
        "restore local flags"
    );

    let output = String::from_utf8_lossy(&output);
    let leave_screen = output
        .rfind("\x1b[?1049l")
        .unwrap_or_else(|| panic!("missing alternate-screen leave: {output:?}"));
    let disable_mouse_capture = last_index(&output, MOUSE_CAPTURE_DISABLE);
    assert!(
        disable_mouse_capture < leave_screen,
        "mouse capture was disabled after leaving the alternate screen: {output:?}"
    );
    let message = output
        .find(REATTACH_MESSAGE)
        .unwrap_or_else(|| panic!("missing reattachment message: {output:?}"));
    assert!(
        leave_screen < message,
        "reattachment message appeared before terminal restoration: {output:?}"
    );
    assert_eq!(
        output[message..].trim_end_matches(['\r', '\n']),
        REATTACH_MESSAGE,
        "reattachment message was not the final output"
    );
}

#[test]
fn live_workspace_preview_terminates_without_reopening_the_fallback_dashboard() {
    let DashboardPty {
        _storage,
        mut master,
        slave,
        original_termios: before,
        mut child,
    } = spawn_dashboard_pty_with_idle_exit(false);
    let mut output = Vec::new();
    wait_for_output(
        &mut master,
        &mut output,
        READY_MARKER,
        Instant::now() + TIMEOUT,
    );
    output.clear();
    // F3 leaves the dashboard and opens the picker for its existing workspace.
    master.write_all(b"\x1bOR").expect("open workspace picker");
    wait_for_output(
        &mut master,
        &mut output,
        b"Workspaces",
        Instant::now() + TIMEOUT,
    );
    wait_for_output(
        &mut master,
        &mut output,
        // The settled preview has its final geometry after the loading row.
        b"No sessions",
        Instant::now() + TIMEOUT,
    );
    // PageDown and End are harmless even when there is no session to scroll.
    master
        .write_all(b"\x1b[6~\x1b[F")
        .expect("scroll empty preview");
    let started = Instant::now();
    assert_eq!(
        unsafe { libc::kill(child.child_mut().id() as i32, libc::SIGTERM) },
        0
    );
    let status = wait_for_exit(
        child.child_mut(),
        &mut master,
        &mut output,
        "workspace selector SIGTERM",
    );
    assert!(status.success(), "selector exit: {status}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "selector shutdown was not bounded"
    );
    let after = termios(slave.as_raw_fd());
    assert_eq!(after.c_iflag, before.c_iflag);
    assert_eq!(after.c_oflag, before.c_oflag);
    assert_eq!(
        stable_local_flags(after.c_lflag),
        stable_local_flags(before.c_lflag)
    );
    let output = String::from_utf8_lossy(&output);
    let disable_mouse_capture = last_index(&output, MOUSE_CAPTURE_DISABLE);
    let leave_screen = output
        .rfind("\x1b[?1049l")
        .expect("restore alternate screen");
    assert!(disable_mouse_capture < leave_screen);
}

#[test]
fn pending_session_open_can_be_cancelled_retried_and_quit_from_a_real_terminal() {
    let DashboardPty {
        _storage,
        mut master,
        slave,
        original_termios: before,
        mut child,
    } = spawn_dashboard_pty_fixture(false, true);
    let mut output = Vec::new();
    wait_for_output(
        &mut master,
        &mut output,
        b"Alt-Q quits.",
        Instant::now() + TIMEOUT,
    );
    master.write_all(b"\x1b").expect("cancel opening");
    output.clear();
    wait_for_output(
        &mut master,
        &mut output,
        // Ratatui writes only changed cells; the preceding words share cells
        // with the opening notice, but this new suffix is written together.
        b"retry.",
        Instant::now() + TIMEOUT,
    );
    // A background tick must not restart the cancelled request.
    thread::sleep(Duration::from_millis(1100));
    drain(&mut master, &mut output);
    assert!(!String::from_utf8_lossy(&output).contains("Alt-Q quits."));
    output.clear();
    master.write_all(b"\r").expect("retry opening");
    wait_for_output(
        &mut master,
        &mut output,
        b"Alt-Q quits.",
        Instant::now() + TIMEOUT,
    );
    let quit_started = Instant::now();
    master.write_all(QUIT_KEY).expect("quit while opening");
    let status = wait_for_exit(
        child.child_mut(),
        &mut master,
        &mut output,
        "quit during session opening",
    );
    drop(child.take());
    assert!(status.success());
    assert!(
        quit_started.elapsed() < Duration::from_secs(1),
        "opening delayed quit"
    );
    assert_eq!(
        stable_local_flags(termios(slave.as_raw_fd()).c_lflag),
        stable_local_flags(before.c_lflag)
    );
}
