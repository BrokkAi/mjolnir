//! Process-wide graceful termination coordination.
//!
//! The first termination signal cancels all subscribers so callers can unwind
//! through their normal cleanup paths. A second signal exits immediately. On
//! Unix the immediate exit status follows the conventional `128 + signal`
//! convention (SIGINT 130, SIGHUP 129, SIGTERM 143); Windows uses 1.

use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use tokio_util::sync::CancellationToken;

#[cfg(windows)]
use tokio::signal::windows::{CtrlBreak, CtrlC};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalAction {
    Graceful,
    Force,
}

static SUPPRESSED_INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

/// Keeps a foreground child process's Ctrl-C from also terminating Hel.
///
/// The child remains in the terminal's foreground process group and receives
/// the signal normally; only Mjolnir's process-wide graceful shutdown is
/// suspended until the guard is dropped.
pub struct SuppressInterruptGuard;

pub fn suppress_interrupts() -> SuppressInterruptGuard {
    SUPPRESSED_INTERRUPTS.fetch_add(1, Ordering::AcqRel);
    SuppressInterruptGuard
}

impl Drop for SuppressInterruptGuard {
    fn drop(&mut self) {
        SUPPRESSED_INTERRUPTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Pure, testable signal transition. The side effect for `Force` belongs to
/// the listener task, never to this state machine.
fn next_signal_action(signals_seen: &AtomicU8) -> SignalAction {
    match signals_seen.fetch_add(1, Ordering::AcqRel) {
        0 => SignalAction::Graceful,
        _ => SignalAction::Force,
    }
}

#[derive(Clone, Debug)]
pub struct Coordinator {
    token: CancellationToken,
    signals_seen: Arc<AtomicU8>,
}

impl Coordinator {
    pub fn install() -> Self {
        let coordinator = Self {
            token: CancellationToken::new(),
            signals_seen: Arc::new(AtomicU8::new(0)),
        };
        #[cfg(unix)]
        install_unix_signals(&coordinator);
        #[cfg(windows)]
        {
            // Register both handlers before returning from `install`. Signals
            // arriving before the spawned task is first polled are then held
            // by the initialized streams instead of bypassing coordination.
            let (ctrl_c, ctrl_break) = install_windows_signals();
            let listener = coordinator.clone();
            tokio::spawn(async move { listener.listen(ctrl_c, ctrl_break).await });
        }
        coordinator
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    fn received_signal(&self, signal: i32) {
        #[cfg(unix)]
        if signal == libc::SIGINT && SUPPRESSED_INTERRUPTS.load(Ordering::Acquire) > 0 {
            return;
        }
        #[cfg(windows)]
        if signal == 0 && SUPPRESSED_INTERRUPTS.load(Ordering::Acquire) > 0 {
            return;
        }
        // SIGHUP means the controlling terminal is gone. A graceful cancel
        // cannot work then: crossterm 0.29 busy-loops inside event::read on
        // the dead tty's EOF, so the UI thread never observes the token and
        // the process survives as a headless CPU spinner. Exit immediately;
        // detached workers are unaffected and child proxies exit on EOF.
        #[cfg(unix)]
        if signal == libc::SIGHUP {
            std::process::exit(exit_code(signal));
        }
        match next_signal_action(&self.signals_seen) {
            SignalAction::Graceful => self.token.cancel(),
            SignalAction::Force => std::process::exit(exit_code(signal)),
        }
    }

    #[cfg(windows)]
    async fn listen(self, mut ctrl_c: CtrlC, mut ctrl_break: CtrlBreak) {
        loop {
            tokio::select! {
                _ = ctrl_c.recv() => self.received_signal(0),
                _ = ctrl_break.recv() => self.received_signal(1),
            }
        }
    }
}

#[cfg(unix)]
fn install_unix_signals(coordinator: &Coordinator) {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    // Self-pipe that wakes the listener thread. `std::io::pipe` creates both
    // ends close-on-exec, so spawned programs do not inherit them. The write
    // end is non-blocking so a signal handler never stalls on a full pipe; a
    // full pipe already holds a wake-up the listener has not read yet.
    let (mut wake_reader, wake_writer) =
        std::io::pipe().expect("create termination signal wake pipe");
    // SAFETY: F_GETFL and F_SETFL on a descriptor this function owns.
    let nonblocking = unsafe {
        let fd = wake_writer.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags != -1 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) != -1
    };
    assert!(
        nonblocking,
        "make the termination signal wake pipe non-blocking: {}",
        std::io::Error::last_os_error()
    );
    // The registered handlers own the write end for the rest of the process,
    // so the listener never sees end-of-file.
    let wake_writer = Arc::new(wake_writer);

    let requested = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // Registration order matters: the first handler exits only when a
        // previous signal armed the flag; the second one arms it. A signal
        // that arrives after the flag is armed exits before any pipe write.
        signal_hook::flag::register_conditional_shutdown(
            signal,
            exit_code(signal),
            requested.clone(),
        )
        .expect("install forced termination signal handler");
        let requested = requested.clone();
        let wake_writer = wake_writer.clone();
        // SAFETY: the handler only touches lock-free atomics and calls
        // `write(2)`, all async-signal-safe, and signal-hook preserves errno.
        // The suppression decision is made here, when the signal is
        // delivered. The flag is stored before the write, so the listener
        // sees it once the byte arrives. The write result is ignored: EAGAIN
        // means the pipe is full, so the listener has an unread wake-up.
        unsafe {
            signal_hook::low_level::register(signal, move || {
                if signal != libc::SIGINT || SUPPRESSED_INTERRUPTS.load(Ordering::Acquire) == 0 {
                    requested.store(true, Ordering::SeqCst);
                    libc::write(wake_writer.as_raw_fd(), b"x".as_ptr().cast(), 1);
                }
            })
        }
        .expect("install graceful termination signal handler");
    }

    signal_hook::flag::register_conditional_shutdown(
        libc::SIGHUP,
        exit_code(libc::SIGHUP),
        Arc::new(AtomicBool::new(true)),
    )
    .expect("install hangup signal handler");

    let listener = coordinator.clone();
    std::thread::Builder::new()
        .name("hel-termination".to_string())
        .spawn(move || {
            // The flag decides, not the byte. A child forked from this process
            // runs these handlers until it execs, and its write to the
            // inherited pipe wakes this thread without setting this process's
            // flag. Checking the flag before the first read also covers a
            // signal that arrived before this thread started.
            let mut wake = [0_u8; 64];
            while !requested.load(Ordering::Acquire) {
                match wake_reader.read(&mut wake) {
                    Ok(0) => {
                        tracing::error!(
                            "termination wake pipe closed; a signal can no longer start graceful shutdown"
                        );
                        return;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        tracing::error!(
                            %error,
                            "termination wake pipe failed; a signal can no longer start graceful shutdown"
                        );
                        return;
                    }
                }
            }
            listener.received_signal(0);
        })
        .expect("spawn termination signal listener");
}

#[cfg(windows)]
fn install_windows_signals() -> (CtrlC, CtrlBreak) {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    (
        ctrl_c().expect("install Ctrl-C listener"),
        ctrl_break().expect("install Ctrl-Break listener"),
    )
}

#[cfg(unix)]
const fn exit_code(signal: i32) -> i32 {
    128 + signal
}

#[cfg(not(unix))]
const fn exit_code(_signal: i32) -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    static INTERRUPT_SUPPRESSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn first_then_repeated_signal_transitions_to_force() {
        let signals_seen = AtomicU8::new(0);
        assert_eq!(next_signal_action(&signals_seen), SignalAction::Graceful);
        assert_eq!(next_signal_action(&signals_seen), SignalAction::Force);
    }

    #[cfg(unix)]
    #[test]
    fn suppressed_interrupt_does_not_advance_shutdown() {
        let _lock = INTERRUPT_SUPPRESSION_TEST_LOCK.lock().unwrap();
        let coordinator = Coordinator {
            token: CancellationToken::new(),
            signals_seen: Arc::new(AtomicU8::new(0)),
        };

        let guard = suppress_interrupts();
        coordinator.received_signal(libc::SIGINT);
        assert!(!coordinator.token().is_cancelled());
        assert_eq!(coordinator.signals_seen.load(Ordering::Acquire), 0);

        drop(guard);
        coordinator.received_signal(libc::SIGINT);
        assert!(coordinator.token().is_cancelled());
        assert_eq!(coordinator.signals_seen.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn coordinator_cancellation_fans_out_to_late_subscribers() {
        let lock = INTERRUPT_SUPPRESSION_TEST_LOCK.lock().unwrap();
        let coordinator = Coordinator {
            token: CancellationToken::new(),
            signals_seen: Arc::new(AtomicU8::new(0)),
        };
        let early = coordinator.token().child_token();
        coordinator.received_signal(0);
        drop(lock);
        let late = coordinator.token().child_token();
        early.cancelled().await;
        late.cancelled().await;
    }
}
