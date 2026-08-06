//! Cross-platform "keep the system awake while working" guard.
//!
//! mj holds a sleep assertion while it is actively working: the whole time
//! `mj server` runs, for the duration of a headless (`mj -p`) run, and while
//! an interactive turn is in flight. macOS uses a native IOKit power
//! assertion (no `caffeinate` child), Linux shells out to `systemd-inhibit`
//! plus `gnome-session-inhibit` under GNOME (whose power manager ignores
//! logind idle inhibitors), Windows pins `SetThreadExecutionState` on a
//! dedicated thread, and other targets are a no-op. Every backend prevents
//! idle *system* sleep only — the display may still turn off — and every
//! assertion is released on drop, or by the OS when the process exits.

const REASON: &str = "mj is working";

/// Two-input latch around one OS sleep assertion: the config switch
/// (`enabled`) and the activity signal (`active`). The assertion is held only
/// while both are true, and every transition re-syncs, so flipping the config
/// switch mid-turn releases or acquires immediately.
#[derive(Debug, Default)]
pub struct KeepAwake {
    enabled: bool,
    active: bool,
    assertion: Option<backend::Assertion>,
}

impl KeepAwake {
    /// Disabled and idle. Callers wire the config switch afterwards, so
    /// constructing state (including in tests) never touches the OS.
    pub fn new() -> Self {
        Self::default()
    }

    /// A guard that is already holding (config permitting) — for the server
    /// and headless runs, which count as "working" for their whole lifetime.
    pub fn hold(enabled: bool) -> Self {
        let mut guard = Self::new();
        guard.enabled = enabled;
        guard.set_active(true);
        guard
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.sync();
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
        self.sync();
    }

    /// Whether the current state calls for an assertion. The OS-level hold
    /// can still be absent (e.g. no inhibitor helper on Linux); callers and
    /// tests observe intent, which is platform-independent.
    pub fn wants_hold(&self) -> bool {
        self.enabled && self.active
    }

    fn sync(&mut self) {
        if self.wants_hold() {
            if self.assertion.is_none() {
                self.assertion = backend::Assertion::acquire(REASON);
            }
        } else {
            self.assertion = None;
        }
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use std::ffi::{CString, c_char, c_void};

    /// `PreventUserIdleSystemSleep` blocks idle system sleep but lets the
    /// display turn off, matching `caffeinate -i` without the child process.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";
    const IOPM_ASSERTION_LEVEL_ON: u32 = 255;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    type CFStringRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: u32,
            assertion_name: CFStringRef,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
    }

    #[derive(Debug)]
    pub(super) struct Assertion {
        id: u32,
    }

    impl Assertion {
        pub(super) fn acquire(reason: &str) -> Option<Self> {
            let assertion_type = CString::new(ASSERTION_TYPE).ok()?;
            let name = CString::new(reason).ok()?;
            unsafe {
                let type_ref = CFStringCreateWithCString(
                    std::ptr::null(),
                    assertion_type.as_ptr(),
                    CF_STRING_ENCODING_UTF8,
                );
                let name_ref = CFStringCreateWithCString(
                    std::ptr::null(),
                    name.as_ptr(),
                    CF_STRING_ENCODING_UTF8,
                );
                let mut id = 0u32;
                let status = if type_ref.is_null() || name_ref.is_null() {
                    -1
                } else {
                    IOPMAssertionCreateWithName(
                        type_ref,
                        IOPM_ASSERTION_LEVEL_ON,
                        name_ref,
                        &mut id,
                    )
                };
                if !type_ref.is_null() {
                    CFRelease(type_ref);
                }
                if !name_ref.is_null() {
                    CFRelease(name_ref);
                }
                if status == 0 {
                    Some(Self { id })
                } else {
                    tracing::debug!(
                        "IOPMAssertionCreateWithName failed ({status}); system may sleep while mj works"
                    );
                    None
                }
            }
        }
    }

    impl Drop for Assertion {
        fn drop(&mut self) {
            unsafe {
                IOPMAssertionRelease(self.id);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use std::process::{Child, Command, Stdio};

    /// `i32::MAX` seconds (~68 years): the helpers never exit on their own;
    /// they are killed on release and die with mj via `PDEATHSIG` on a crash.
    const FOREVER: &str = "2147483647";

    /// One assertion can hold several inhibitor helpers. A logind idle lock
    /// alone does not stop GNOME's automatic suspend — gsd-power consults
    /// only gnome-session inhibitors before arming its sleep timeout — and a
    /// session lock alone does not cover logind `IdleAction=` setups, so
    /// inside a GNOME session both are held.
    #[derive(Debug)]
    pub(super) struct Assertion {
        children: Vec<Child>,
    }

    impl Assertion {
        pub(super) fn acquire(reason: &str) -> Option<Self> {
            let mut children = Vec::new();
            match spawn_inhibitor(
                "systemd-inhibit",
                &[
                    "--what=idle",
                    "--mode=block",
                    "--who=mj",
                    "--why",
                    reason,
                    "--",
                    "sleep",
                    FOREVER,
                ],
            ) {
                Ok(child) => children.push(child),
                Err(error) => {
                    tracing::debug!("keep-awake helper systemd-inhibit unavailable: {error}");
                }
            }
            // `idle` stops gsd-power's idle machinery while the screen is
            // unlocked; `suspend` blocks the sleep action once the
            // screensaver is active (idle inhibition is bypassed then), e.g.
            // when the user locks the screen and walks away mid-turn.
            if in_gnome_session() || children.is_empty() {
                match spawn_inhibitor(
                    "gnome-session-inhibit",
                    &[
                        "--inhibit",
                        "idle",
                        "--inhibit",
                        "suspend",
                        "--reason",
                        reason,
                        "sleep",
                        FOREVER,
                    ],
                ) {
                    Ok(child) => children.push(child),
                    Err(error) => {
                        tracing::debug!(
                            "keep-awake helper gnome-session-inhibit unavailable: {error}"
                        );
                    }
                }
            }
            if children.is_empty() {
                None
            } else {
                Some(Self { children })
            }
        }
    }

    fn in_gnome_session() -> bool {
        std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktops| desktop_list_has_gnome(&desktops))
            || std::env::var_os("GNOME_DESKTOP_SESSION_ID").is_some()
    }

    /// `XDG_CURRENT_DESKTOP` is a colon-separated list such as
    /// `ubuntu:GNOME` or `GNOME-Classic:GNOME`. Cinnamon and other forks run
    /// their own session managers, so only real GNOME entries count.
    fn desktop_list_has_gnome(desktops: &str) -> bool {
        desktops.split(':').any(|desktop| {
            let desktop = desktop.trim();
            desktop.eq_ignore_ascii_case("gnome")
                || desktop.to_ascii_lowercase().starts_with("gnome-")
        })
    }

    fn spawn_inhibitor(program: &str, args: &[&str]) -> std::io::Result<Child> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                // Die with mj even if it is SIGKILLed; re-check the parent to
                // close the race where mj exits between fork and prctl.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
                Ok(())
            });
        }
        command.spawn()
    }

    impl Drop for Assertion {
        fn drop(&mut self) {
            for child in &mut self.children {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::desktop_list_has_gnome;

        #[test]
        fn gnome_detection_reads_the_desktop_list() {
            assert!(desktop_list_has_gnome("GNOME"));
            assert!(desktop_list_has_gnome("gnome"));
            assert!(desktop_list_has_gnome("ubuntu:GNOME"));
            assert!(desktop_list_has_gnome("GNOME-Classic:GNOME"));
            assert!(!desktop_list_has_gnome("KDE"));
            assert!(!desktop_list_has_gnome("X-Cinnamon"));
            assert!(!desktop_list_has_gnome(""));
        }
    }
}

#[cfg(windows)]
mod backend {
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(es_flags: u32) -> u32;
    }

    /// `ES_CONTINUOUS` is per-thread state, so a dedicated thread owns it and
    /// clears it before exiting when the assertion is dropped.
    #[derive(Debug)]
    pub(super) struct Assertion {
        stop_tx: mpsc::Sender<()>,
        thread: Option<JoinHandle<()>>,
    }

    impl Assertion {
        pub(super) fn acquire(_reason: &str) -> Option<Self> {
            let (stop_tx, stop_rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("mj-keep-awake".into())
                .spawn(move || {
                    if unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } == 0 {
                        tracing::debug!(
                            "SetThreadExecutionState failed; system may sleep while mj works"
                        );
                        return;
                    }
                    let _ = stop_rx.recv();
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                })
                .ok()?;
            Some(Self {
                stop_tx,
                thread: Some(thread),
            })
        }
    }

    impl Drop for Assertion {
        fn drop(&mut self) {
            let _ = self.stop_tx.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod backend {
    #[derive(Debug)]
    pub(super) struct Assertion;

    impl Assertion {
        pub(super) fn acquire(_reason: &str) -> Option<Self> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertion_wanted_only_when_enabled_and_active() {
        let mut guard = KeepAwake::new();
        assert!(!guard.wants_hold());
        guard.set_active(true);
        assert!(!guard.wants_hold());
        guard.set_enabled(true);
        assert!(guard.wants_hold());
        guard.set_active(false);
        assert!(!guard.wants_hold());
        assert!(guard.assertion.is_none());
        guard.set_active(true);
        guard.set_enabled(false);
        assert!(!guard.wants_hold());
        assert!(guard.assertion.is_none());
    }

    #[test]
    fn hold_respects_the_config_switch() {
        assert!(KeepAwake::hold(true).wants_hold());
        let disabled = KeepAwake::hold(false);
        assert!(!disabled.wants_hold());
        assert!(disabled.assertion.is_none());
    }

    /// Really acquires (and drop-releases) an IOKit power assertion, so a
    /// silent API failure cannot hide behind the intent-only checks above.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_assertion_actually_acquires() {
        let guard = KeepAwake::hold(true);
        assert!(guard.assertion.is_some());
    }
}
