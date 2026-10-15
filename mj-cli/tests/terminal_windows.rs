#![cfg(windows)]

//! The Windows counterpart of the Unix PTY tests: `mj` runs under a real
//! ConPTY pseudo console against an isolated instance, starts its daemon,
//! renders the dashboard, and quits through the detach chord.

mod common;
mod pty_screen;

use std::{
    fs,
    io::{Read, Write},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use pty_screen::{PTY_COLUMNS, PTY_ROWS, PtyOutput};

/// Empty-session text that appears after the combined dashboard is ready.
const READY_MARKER: &str = "No live session";
const REATTACH_MESSAGE: &str = "Active sessions will continue working";
// Startup includes the daemon's first initialization of the store.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(15);
/// The detach chord: `ctrl+b` (byte 0x02) then `q`.
const QUIT_KEY: &[u8] = b"\x02q";
/// Queries the console host or the dashboard may send, and the answers a
/// terminal gives. An unanswered cursor-position request stalls ConPTY.
const TERMINAL_QUERIES: [(&[u8], &[u8]); 2] =
    [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[c", b"\x1b[?1;2c")];

struct Terminal {
    output: PtyOutput,
    chunks: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    answered: usize,
}

impl Terminal {
    /// Read what the console host wrote until `done` holds or the deadline
    /// passes, answering terminal queries as they appear.
    fn wait(
        &mut self,
        child: &mut dyn Child,
        deadline: Instant,
        expected: &str,
        mut done: impl FnMut(&PtyOutput, &mut dyn Child) -> bool,
    ) {
        loop {
            if done(&self.output, child) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "mj did not reach {expected}; screen:\n{}\noutput: {:?}",
                self.output.screen.contents(),
                String::from_utf8_lossy(&self.output)
            );
            if let Ok(chunk) = self.chunks.recv_timeout(Duration::from_millis(50)) {
                self.output.extend_from_slice(&chunk);
                self.answer_queries();
            }
        }
    }

    /// Answer each query once, when its last byte arrives, so a query split
    /// across reads is answered exactly once.
    fn answer_queries(&mut self) {
        let output: &[u8] = &self.output;
        let longest = TERMINAL_QUERIES
            .iter()
            .map(|(query, _)| query.len())
            .max()
            .unwrap();
        let mut answers = Vec::new();
        for start in self.answered.saturating_sub(longest)..output.len() {
            for (query, answer) in TERMINAL_QUERIES {
                let end = start + query.len();
                if end > self.answered && output[start..].starts_with(query) {
                    answers.push(answer);
                }
            }
        }
        self.answered = output.len();
        for answer in answers {
            self.writer
                .write_all(answer)
                .expect("answer a terminal query");
        }
    }

    fn visible(&self, text: &str) -> bool {
        self.output.screen.contents().contains(text)
    }
}

#[test]
fn dashboard_starts_its_daemon_renders_and_quits_under_conpty() {
    let directory = tempfile::tempdir().expect("create test storage");
    let config_directory = directory.path().join("config");
    let data_directory = directory.path().join("data");
    let storage =
        common::DaemonStorage::new(directory, config_directory.clone(), data_directory.clone());
    fs::create_dir_all(&config_directory).unwrap();
    // A configured profile keeps the first-run welcome out of the way. Its
    // home exists but holds no agent, and a TOML literal string keeps the
    // Windows path's backslashes.
    let codex_home = storage.path().join(".codex");
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(
        config_directory.join("config.toml"),
        format!(
            "version = 1\n\n[phone]\nenabled = false\n\n[profiles.codex]\nkind = \"codex\"\nhome = '{}'\n",
            codex_home.display()
        ),
    )
    .unwrap();

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: PTY_ROWS as u16,
            cols: PTY_COLUMNS as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a ConPTY pseudo console");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_mj"));
    command.args(["--instance", "windows-terminal"]);
    command.cwd(storage.path());
    command.env(common::DAEMON_OWNER_ENV, std::process::id().to_string());
    command.env("MJ_CONFIG_DIR", &config_directory);
    command.env("MJ_DATA_DIR", &data_directory);
    command.env("MJ_DAEMON_EXIT_WHEN_IDLE", "1");
    command.env("MJOLNIR_NO_UPDATE_CHECK", "1");
    // Profile discovery reads agent homes and commands, so the dashboard
    // must see neither the host's homes nor its installed agents.
    for home in ["HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA"] {
        command.env(home, storage.path());
    }
    command.env("CODEX_HOME", storage.path().join(".codex"));
    command.env("CLAUDE_CONFIG_DIR", storage.path().join(".claude"));
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set on Windows");
    command.env("PATH", std::path::Path::new(&system_root).join("System32"));
    for variable in ["MJ_BIFROST_BIN", "KIMI_CODE_HOME", "GROK_HOME", "MUSE_HOME"] {
        command.env_remove(variable);
    }
    // Each fixture has a dashboard and daemon. Host-sized pools multiply
    // into many threads on large CI machines during parallel runs.
    command.env("TOKIO_WORKER_THREADS", "2");
    command.env("RAYON_NUM_THREADS", "2");
    let mut child = pty
        .slave
        .spawn_command(command)
        .expect("spawn mj in the pseudo console");
    // The child holds its own handles to the console now.
    drop(pty.slave);

    // ConPTY reads block, and reach end of file only once the pseudo console
    // closes, so a thread forwards what it reads until then.
    let mut reader = pty.master.try_clone_reader().expect("read the console");
    let (sender, chunks) = mpsc::channel();
    let forwarder = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    if sender.send(buffer[..read].to_vec()).is_err() {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
        }
    });
    let mut terminal = Terminal {
        output: PtyOutput::new(),
        chunks,
        writer: pty.master.take_writer().expect("write to the console"),
        answered: 0,
    };

    terminal.wait(
        &mut *child,
        Instant::now() + STARTUP_TIMEOUT,
        "the dashboard",
        |output, child| {
            if let Some(status) = child.try_wait().expect("poll mj") {
                panic!(
                    "mj exited during startup with {status:?}; screen:\n{}",
                    output.screen.contents()
                );
            }
            output.screen.contents().contains(READY_MARKER)
        },
    );
    assert!(
        storage.wait_until_owned(Instant::now() + TIMEOUT),
        "the dashboard never started a daemon for its isolated instance"
    );

    terminal
        .writer
        .write_all(QUIT_KEY)
        .expect("send the quit key");
    let mut status = None;
    terminal.wait(
        &mut *child,
        Instant::now() + TIMEOUT,
        "exit after the quit key",
        |_, child| {
            status = child.try_wait().expect("poll mj");
            status.is_some()
        },
    );
    let status = status.unwrap();
    assert!(status.success(), "mj exit: {status:?}");
    // The final message is written before exit; collect what remains, then
    // close the console so the forwarder sees end of file.
    while let Ok(chunk) = terminal.chunks.recv_timeout(Duration::from_millis(500)) {
        terminal.output.extend_from_slice(&chunk);
    }
    assert!(
        terminal.visible(REATTACH_MESSAGE),
        "missing the reattachment message; screen:\n{}",
        terminal.output.screen.contents()
    );
    drop(terminal);
    drop(pty.master);
    forwarder
        .join()
        .expect("the console reader panicked")
        .expect("read the console");
}
