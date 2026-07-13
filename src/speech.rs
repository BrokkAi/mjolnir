//! Prompt dictation support.
//!
//! Non-Android platforms run a fully local `mj-voice-worker` sidecar built on
//! sherpa-onnx. Keeping the native speech stack in its own workspace package
//! means ordinary `mj` builds never compile or link ONNX Runtime.
//!
//! The native speech stack can raise foreign C++ exceptions across the FFI boundary or
//! abort outright when system libraries are incompatible or model files are
//! corrupt; Rust cannot catch those, so in-process use would `SIGABRT` the
//! whole TUI. Isolating dictation in a child process turns any such crash
//! into a status-line warning while the chat session keeps running. The
//! worker streams progress to the parent as JSON lines on stdout, and stops
//! when its stdin reaches EOF (cancellation or parent exit).

use anyhow::Result;
#[cfg(target_os = "android")]
use anyhow::bail;
#[cfg(not(target_os = "android"))]
mod worker {
    use anyhow::{Context, Result, anyhow};
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Read};
    use std::path::PathBuf;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// One JSON line on the worker's stdout.
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    #[serde(tag = "event", rename_all = "snake_case")]
    pub(super) enum WorkerEvent {
        Status { message: String },
        Partial { text: String },
        Level { value: f32 },
        Result { text: String },
        Error { message: String },
    }

    pub(super) fn parse_event(line: &str) -> Option<WorkerEvent> {
        serde_json::from_str(line.trim()).ok()
    }

    /// How long after cancellation the worker gets to flush a final
    /// transcript before it is killed.
    const CANCEL_GRACE: Duration = Duration::from_secs(10);
    /// The worker enforces the dictation timeout itself; the parent allows
    /// some slack on top before declaring it hung.
    const WORKER_GRACE: Duration = Duration::from_secs(30);
    const DICTATION_TIMEOUT: Duration = Duration::from_secs(600);

    /// Parent-side dictation: spawn the native sidecar and relay its events, so
    /// a native-library crash cannot take down the TUI process.
    pub(super) fn run<F, G, H>(
        on_partial: F,
        on_level: G,
        on_status: H,
        cancel_rx: mpsc::Receiver<()>,
    ) -> Result<String>
    where
        F: FnMut(String),
        G: FnMut(f32),
        H: FnMut(String),
    {
        let exe = voice_worker_executable()?;
        let child = Command::new(&exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start voice worker {}", exe.display()))?;
        drive_worker(child, on_partial, on_level, on_status, cancel_rx)
    }

    pub(super) fn voice_worker_executable() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("MJ_VOICE_WORKER") {
            let path = PathBuf::from(path);
            anyhow::ensure!(
                path.is_file(),
                "MJ_VOICE_WORKER does not exist: {}",
                path.display()
            );
            return Ok(path);
        }

        let mj = std::env::current_exe().context("locate the mj executable")?;
        let worker = mj.with_file_name(if cfg!(windows) {
            "mj-voice-worker.exe"
        } else {
            "mj-voice-worker"
        });
        anyhow::ensure!(
            worker.is_file(),
            "voice dictation helper is missing: {}; install it beside mj or set MJ_VOICE_WORKER",
            worker.display()
        );
        Ok(worker)
    }

    /// Relay worker events to the UI callbacks and translate every way the
    /// worker can end — result, reported error, crash, or hang — into a
    /// `Result`. Non-protocol stdout lines (native-library noise) are ignored.
    pub(super) fn drive_worker<F, G, H>(
        mut child: Child,
        mut on_partial: F,
        mut on_level: G,
        mut on_status: H,
        cancel_rx: mpsc::Receiver<()>,
    ) -> Result<String>
    where
        F: FnMut(String),
        G: FnMut(f32),
        H: FnMut(String),
    {
        let mut stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .context("voice worker stdout was not captured")?;
        let stderr = child.stderr.take();

        // None marks stdout EOF: the worker is gone without a verdict.
        let (event_tx, event_rx) = mpsc::channel::<Option<WorkerEvent>>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(event) = parse_event(&line)
                    && event_tx.send(Some(event)).is_err()
                {
                    return;
                }
            }
            let _ = event_tx.send(None);
        });
        let stderr_reader = stderr.map(|stderr| thread::spawn(move || read_tail(stderr)));

        let started_at = Instant::now();
        let mut cancelled_at: Option<Instant> = None;
        let mut last_partial = String::new();
        let outcome = loop {
            if cancelled_at.is_none() && cancel_rx.try_recv().is_ok() {
                cancelled_at = Some(Instant::now());
                // Closing stdin is the cancellation signal; the worker then
                // flushes and reports whatever it recognized so far.
                stdin = None;
            }
            if let Some(at) = cancelled_at
                && at.elapsed() >= CANCEL_GRACE
            {
                let _ = child.kill();
                break Some(Ok(last_partial.clone()));
            }
            if started_at.elapsed() >= DICTATION_TIMEOUT + WORKER_GRACE {
                let _ = child.kill();
                break Some(Err(anyhow!("voice dictation timed out")));
            }
            match event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Some(WorkerEvent::Partial { text })) => {
                    last_partial.clone_from(&text);
                    on_partial(text);
                }
                Ok(Some(WorkerEvent::Level { value })) => on_level(value),
                Ok(Some(WorkerEvent::Status { message })) => on_status(message),
                Ok(Some(WorkerEvent::Result { text })) => break Some(Ok(text)),
                Ok(Some(WorkerEvent::Error { message })) => break Some(Err(anyhow!(message))),
                Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        drop(stdin);

        let status = reap(child);
        let stderr_tail = stderr_reader
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        match outcome {
            Some(result) => result,
            None => {
                if !stderr_tail.trim().is_empty() {
                    tracing::warn!("voice worker stderr: {stderr_tail}");
                }
                Err(worker_crash_error(status, &stderr_tail))
            }
        }
    }

    /// Wait briefly for the worker to exit, killing it if it lingers.
    fn reap(mut child: Child) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = child.kill();
                    return child.wait().ok();
                }
            }
        }
    }

    /// Keep the last few KB of the worker's stderr for crash diagnostics.
    fn read_tail<R: Read>(mut reader: R) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > 8192 {
                let cut = buffer.len() - 4096;
                buffer.drain(..cut);
            }
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// The worker vanished without reporting a result or an error: a native
    /// crash (foreign exception, abort) or an outright kill. Explain what
    /// happened without taking the session with it.
    pub(super) fn worker_crash_error(
        status: Option<ExitStatus>,
        stderr_tail: &str,
    ) -> anyhow::Error {
        let mut message = match status {
            Some(status) => format!("voice dictation {}", describe_exit(status)),
            None => "voice dictation stopped unexpectedly".to_string(),
        };
        if let Some(line) = last_meaningful_line(stderr_tail) {
            message.push_str(&format!(": {line}"));
        }
        let cache = dirs::cache_dir()
            .map(|path| path.join("mj/voice").display().to_string())
            .unwrap_or_else(|| "the voice model cache".to_string());
        message.push_str(&format!(
            " — the dictation engine runs in a separate process, so your session is unaffected; \
             this usually means an incompatible or outdated system library (try updating system \
             packages, or delete {cache} and retry)"
        ));
        anyhow!(message)
    }

    #[cfg(unix)]
    fn describe_exit(status: ExitStatus) -> String {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                4 => " (SIGILL)",
                6 => " (SIGABRT)",
                8 => " (SIGFPE)",
                11 => " (SIGSEGV)",
                _ => "",
            };
            return format!("crashed with signal {signal}{name}");
        }
        describe_exit_code(status)
    }

    #[cfg(not(unix))]
    fn describe_exit(status: ExitStatus) -> String {
        describe_exit_code(status)
    }

    fn describe_exit_code(status: ExitStatus) -> String {
        match status.code() {
            Some(code) => format!("exited unexpectedly (code {code})"),
            None => "stopped unexpectedly".to_string(),
        }
    }

    fn last_meaningful_line(stderr_tail: &str) -> Option<String> {
        let line = stderr_tail
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        let truncated: String = line.chars().take(200).collect();
        Some(truncated)
    }
}

pub fn voice_input_supported() -> bool {
    #[cfg(not(target_os = "android"))]
    {
        worker::voice_worker_executable().is_ok()
    }
    #[cfg(target_os = "android")]
    {
        false
    }
}

/// Capture microphone audio and return the recognized transcript.
///
/// `on_partial` receives the cumulative transcript as it grows, `on_level`
/// receives normalized microphone levels for the input meter, and `on_status`
/// receives transient progress messages (model download, loading). Sending on
/// `cancel_rx` stops capture and returns whatever was recognized so far.
///
/// Dictation runs in a separate worker process (see the module docs); a crash
/// in the native speech stack surfaces here as an error instead of aborting
/// the TUI.
#[cfg(not(target_os = "android"))]
pub fn run_dictation<F, G, H>(
    on_partial: F,
    on_level: G,
    on_status: H,
    cancel_rx: std::sync::mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    worker::run(on_partial, on_level, on_status, cancel_rx)
}

#[cfg(target_os = "android")]
pub fn run_dictation<F, G, H>(
    _on_partial: F,
    _on_level: G,
    _on_status: H,
    _cancel_rx: std::sync::mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    bail!("voice dictation is not supported on Android")
}

pub fn dictation_error_message(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.starts_with("voice") || message.starts_with("no speech") {
        return message;
    }
    if message.contains("microphone") {
        return format!("voice dictation could not use the microphone: {message}");
    }
    format!("voice dictation failed: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_prefixed_for_context() {
        let err = anyhow::anyhow!("some backend exploded");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation failed: some backend exploded"
        );
        let err = anyhow::anyhow!("no speech was recognized");
        assert_eq!(dictation_error_message(&err), "no speech was recognized");
        let err = anyhow::anyhow!("voice dictation is not supported on Android");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation is not supported on Android"
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn worker_events_round_trip_as_json_lines() {
        use super::worker::{WorkerEvent, parse_event};
        let events = [
            WorkerEvent::Status {
                message: "loading voice model...".to_string(),
            },
            WorkerEvent::Partial {
                text: "hello".to_string(),
            },
            WorkerEvent::Level { value: 0.25 },
            WorkerEvent::Result {
                text: "hello world".to_string(),
            },
            WorkerEvent::Error {
                message: "microphone capture failed".to_string(),
            },
        ];
        for event in events {
            let line = serde_json::to_string(&event).expect("serialize event");
            assert!(!line.contains('\n'), "protocol lines must be single-line");
            assert_eq!(parse_event(&line), Some(event));
        }
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn parse_event_ignores_non_protocol_output() {
        use super::worker::parse_event;
        assert_eq!(parse_event(""), None);
        assert_eq!(parse_event("onnxruntime init log line"), None);
        assert_eq!(parse_event("{\"event\":\"unknown\"}"), None);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn crash_error_includes_stderr_line_and_recovery_hint() {
        let err = super::worker::worker_crash_error(
            None,
            "onnx init\nfatal runtime error: Rust cannot catch foreign exceptions, aborting\n",
        );
        let message = err.to_string();
        assert!(message.contains("voice dictation stopped unexpectedly"));
        assert!(message.contains("Rust cannot catch foreign exceptions"));
        assert!(message.contains("session is unaffected"));
    }

    /// Fake-worker tests: drive_worker against short shell scripts standing
    /// in for the real worker, covering each way the child can end.
    #[cfg(all(unix, not(target_os = "android")))]
    mod fake_worker {
        use super::super::worker::drive_worker;
        use std::process::{Command, Stdio};
        use std::sync::mpsc;

        fn spawn_fake(script: &str) -> std::process::Child {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn fake worker")
        }

        fn drive(
            script: &str,
            cancel_rx: mpsc::Receiver<()>,
        ) -> (anyhow::Result<String>, Vec<String>, Vec<String>) {
            let mut partials = Vec::new();
            let mut statuses = Vec::new();
            let result = drive_worker(
                spawn_fake(script),
                |text| partials.push(text),
                |_level| {},
                |message| statuses.push(message),
                cancel_rx,
            );
            (result, partials, statuses)
        }

        fn never_cancelled() -> mpsc::Receiver<()> {
            let (tx, rx) = mpsc::channel();
            std::mem::forget(tx);
            rx
        }

        #[test]
        fn forwards_events_and_returns_result() {
            let script = r#"
                printf '%s\n' '{"event":"status","message":"listening..."}'
                printf '%s\n' '{"event":"level","value":0.5}'
                printf '%s\n' '{"event":"partial","text":"hello"}'
                printf 'native library noise\n'
                printf '%s\n' '{"event":"result","text":"hello world"}'
            "#;
            let (result, partials, statuses) = drive(script, never_cancelled());
            assert_eq!(result.expect("transcript"), "hello world");
            assert_eq!(partials, vec!["hello".to_string()]);
            assert_eq!(statuses, vec!["listening...".to_string()]);
        }

        #[test]
        fn error_event_surfaces_as_error() {
            let script = r#"
                printf '%s\n' '{"event":"error","message":"microphone capture failed: boom"}'
                exit 1
            "#;
            let (result, _, _) = drive(script, never_cancelled());
            let message = result.expect_err("error").to_string();
            assert_eq!(message, "microphone capture failed: boom");
        }

        #[test]
        fn abort_is_contained_and_described() {
            let script = r#"
                echo 'fatal runtime error: Rust cannot catch foreign exceptions, aborting' >&2
                kill -ABRT $$
            "#;
            let (result, _, _) = drive(script, never_cancelled());
            let message = result.expect_err("crash error").to_string();
            assert!(message.contains("signal 6"), "got: {message}");
            assert!(message.contains("SIGABRT"), "got: {message}");
            assert!(
                message.contains("Rust cannot catch foreign exceptions"),
                "got: {message}"
            );
            assert!(message.contains("session is unaffected"), "got: {message}");
        }

        #[test]
        fn silent_exit_is_reported_with_code() {
            let (result, _, _) = drive("exit 3", never_cancelled());
            let message = result.expect_err("exit error").to_string();
            assert!(
                message.contains("exited unexpectedly (code 3)"),
                "got: {message}"
            );
        }

        #[test]
        fn cancel_closes_stdin_and_returns_flushed_result() {
            // The fake worker mirrors the real cancellation handshake: wait
            // for stdin EOF, then flush a final transcript.
            let script = r#"
                while read -r _; do :; done
                printf '%s\n' '{"event":"result","text":"flushed"}'
            "#;
            let (cancel_tx, cancel_rx) = mpsc::channel();
            cancel_tx.send(()).expect("queue cancel");
            let (result, _, _) = drive(script, cancel_rx);
            assert_eq!(result.expect("flushed transcript"), "flushed");
        }
    }
}
