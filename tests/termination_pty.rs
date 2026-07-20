#![cfg(unix)]

use std::{
    fs::File,
    io::{self, ErrorKind, Read},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const READY_MARKER: &[u8] = b"MJ_TERMINATION_PTY_READY";
const FIRST_SIGNAL_ACK_MARKER: &[u8] = b"MJ_TERMINATION_PTY_FIRST_SIGNAL_ACK";
const TIMEOUT: Duration = Duration::from_secs(5);

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

fn wait_for_exit(child: &mut Child, master: &mut File, output: &mut Vec<u8>) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        drain(master, output);
        if let Some(status) = child.try_wait().expect("poll PTY child") {
            return status;
        }
        if Instant::now() >= deadline {
            panic!(
                "PTY child did not exit after SIGTERM; output: {:?}",
                String::from_utf8_lossy(output)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sigterm_restores_real_pty_terminal() {
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
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let before = termios(slave.as_raw_fd());
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "read PTY master flags");
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "make PTY master nonblocking"
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    command
        .env("MJ_TERMINATION_PTY_INTEGRATION", "1")
        .stdin(Stdio::from(duplicate(slave.as_raw_fd())))
        .stdout(Stdio::from(duplicate(slave.as_raw_fd())))
        .stderr(Stdio::from(duplicate(slave.as_raw_fd())));
    // Libtest may alter its signal mask. A real `mj` invocation should start
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
    let child = command.spawn().expect("spawn mj PTY helper");
    let mut child = ReapChild(Some(child));

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
    let status = wait_for_exit(child.child_mut(), &mut master, &mut output);
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
    assert!(
        output.contains("\x1b[?25h"),
        "missing cursor restoration: {output:?}"
    );
}

#[test]
fn repeated_sigterm_forces_real_pty_child_exit() {
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
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "read PTY master flags");
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "make PTY master nonblocking"
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    command
        .env("MJ_TERMINATION_PTY_INTEGRATION", "force")
        .stdin(Stdio::from(duplicate(slave.as_raw_fd())))
        .stdout(Stdio::from(duplicate(slave.as_raw_fd())))
        .stderr(Stdio::from(duplicate(slave.as_raw_fd())));
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
    let child = command.spawn().expect("spawn mj PTY helper");
    let mut child = ReapChild(Some(child));

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
        "send first SIGTERM to PTY child"
    );
    wait_for_output(
        &mut master,
        &mut output,
        FIRST_SIGNAL_ACK_MARKER,
        Instant::now() + TIMEOUT,
    );
    assert_eq!(
        unsafe { libc::kill(child.child_mut().id() as i32, libc::SIGTERM) },
        0,
        "send second SIGTERM to PTY child"
    );
    let status = wait_for_exit(child.child_mut(), &mut master, &mut output);
    drop(child.take());

    assert_eq!(status.code(), Some(143), "PTY child exit: {status}");
}
