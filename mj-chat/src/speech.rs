//! Prompt dictation support.
//!
//! Non-Android platforms run an `mj-voice-worker` sidecar. The worker receives
//! the Codex authentication path as a command-line argument and streams
//! progress to the parent as JSON lines on stdout. A newline on stdin ends
//! capture and starts upload/transcription; EOF cancels the request without
//! uploading it.

use anyhow::Result;
#[cfg(target_os = "android")]
use anyhow::bail;
use std::path::Path;

/// Command sent from the chat input to the dictation transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceCommand {
    /// End microphone capture and let the worker upload and transcribe it.
    Finish,
    /// Cancel capture. The worker must not upload or return a transcript.
    Cancel,
}

#[cfg(not(target_os = "android"))]
mod worker {
    use anyhow::{Context, Result, anyhow};
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::VoiceCommand;

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

    /// How long after cancellation the worker gets to exit before it is
    /// forcefully stopped.
    const CANCEL_GRACE: Duration = Duration::from_secs(5);
    /// Bound capture and worker startup even if the sidecar never reports a
    /// command-ready state. The worker applies the same capture limit.
    const CAPTURE_TIMEOUT: Duration = Duration::from_secs(600);
    /// How long the upload/transcription phase may run. The worker's own
    /// timeout is independent; this extra slack prevents a hung worker from
    /// keeping the chat task alive forever.
    const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(120);
    const WORKER_GRACE: Duration = Duration::from_secs(30);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    #[cfg(unix)]
    const SIGKILL: i32 = 9;

    /// Parent-side dictation: spawn the sidecar and relay its events.
    pub(super) fn run<F, G, H>(
        auth_path: &Path,
        on_partial: F,
        on_level: G,
        on_status: H,
        cancel_rx: mpsc::Receiver<VoiceCommand>,
    ) -> Result<String>
    where
        F: FnMut(String),
        G: FnMut(f32),
        H: FnMut(String),
    {
        let exe = voice_worker_executable()?;
        let mut command = Command::new(&exe);
        command
            .arg("--codex-auth")
            .arg(auth_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .with_context(|| format!("start voice worker {}", exe.display()))?;
        drive_worker(child, on_partial, on_level, on_status, cancel_rx)
    }

    pub(super) fn voice_worker_executable() -> Result<PathBuf> {
        if let Some(path) = hel::hel_config::env_override_os("VOICE_WORKER") {
            let path = PathBuf::from(path);
            anyhow::ensure!(
                path.is_file(),
                "MJ_VOICE_WORKER does not exist: {}",
                path.display()
            );
            return Ok(path);
        }

        let hel = std::env::current_exe().context("locate the mj executable")?;
        let worker = hel.with_file_name(if cfg!(windows) {
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
    /// `Result`. Non-protocol stdout lines are ignored.
    pub(super) fn drive_worker<F, G, H>(
        mut child: Child,
        mut on_partial: F,
        mut on_level: G,
        mut on_status: H,
        cancel_rx: mpsc::Receiver<VoiceCommand>,
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
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        tracing::warn!(%error, "could not read voice worker output");
                        break;
                    }
                };
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
        let mut finish_started_at: Option<Instant> = None;
        let mut cancelled_at: Option<Instant> = None;
        let outcome = loop {
            if cancelled_at.is_none() {
                match cancel_rx.try_recv() {
                    Ok(VoiceCommand::Finish) if finish_started_at.is_none() => {
                        let Some(worker_stdin) = stdin.as_mut() else {
                            break Some(Err(anyhow!(
                                "voice worker stdin closed before transcription started"
                            )));
                        };
                        if let Err(error) = worker_stdin
                            .write_all(b"\n")
                            .and_then(|()| worker_stdin.flush())
                        {
                            break Some(Err(anyhow!(
                                "send voice capture completion to worker: {error}"
                            )));
                        }
                        // Keep stdin open while the worker uploads and
                        // transcribes. EOF is reserved for cancellation.
                        finish_started_at = Some(Instant::now());
                    }
                    Ok(VoiceCommand::Finish) => {}
                    Ok(VoiceCommand::Cancel) | Err(mpsc::TryRecvError::Disconnected) => {
                        cancelled_at = Some(Instant::now());
                        // EOF is the cancellation signal. Do not allow any
                        // result the worker may have already queued to reach
                        // the canceled chat input.
                        stdin = None;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if let Some(at) = cancelled_at
                && at.elapsed() >= CANCEL_GRACE
            {
                stop_worker(&mut child);
                break Some(Err(anyhow!("voice dictation cancelled")));
            }
            if let Some(at) = finish_started_at {
                if at.elapsed() >= TRANSCRIPTION_TIMEOUT + WORKER_GRACE {
                    stop_worker(&mut child);
                    break Some(Err(anyhow!("voice dictation transcription timed out")));
                }
            } else if started_at.elapsed() >= CAPTURE_TIMEOUT + TRANSCRIPTION_TIMEOUT + WORKER_GRACE
            {
                // The worker also finishes capture automatically at its limit,
                // so reserve a complete transcription deadline without a click.
                stop_worker(&mut child);
                break Some(Err(anyhow!("voice dictation capture timed out")));
            }
            match event_rx.recv_timeout(POLL_INTERVAL) {
                Ok(Some(event)) if cancelled_at.is_some() => {
                    // Cancellation owns the result even if the worker raced
                    // EOF and emitted one more event.
                    drop(event);
                }
                Ok(Some(WorkerEvent::Partial { text })) => {
                    on_partial(text);
                }
                Ok(Some(WorkerEvent::Level { value })) => on_level(value),
                Ok(Some(WorkerEvent::Status { message })) => on_status(message),
                Ok(Some(WorkerEvent::Result { text })) => break Some(Ok(text)),
                Ok(Some(WorkerEvent::Error { message })) => break Some(Err(anyhow!(message))),
                Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if cancelled_at.is_some() {
                        break Some(Err(anyhow!("voice dictation cancelled")));
                    }
                    break None;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        drop(stdin);

        let status = reap(child);
        let stderr_tail = stderr_reader
            .and_then(|reader| match reader.join() {
                Ok(tail) => Some(tail),
                Err(error) => {
                    tracing::warn!("voice worker stderr reader panicked: {error:?}");
                    None
                }
            })
            .unwrap_or_default();
        if !stderr_tail.trim().is_empty() {
            match &outcome {
                Some(Err(_)) | None => tracing::warn!("voice worker stderr: {stderr_tail}"),
                Some(Ok(_)) => tracing::debug!("voice worker stderr: {stderr_tail}"),
            }
        }
        match outcome {
            Some(result) => result,
            None => Err(worker_crash_error(status, &stderr_tail)),
        }
    }

    /// Stop the worker and any subprocesses it owns.
    fn stop_worker(child: &mut Child) {
        #[cfg(unix)]
        {
            // The shared helper also handles a group that exited between the
            // timeout check and this call.
            hel::hel_subprocess::terminate_process_group(child.id() as i32, SIGKILL);
        }
        #[cfg(not(unix))]
        if let Err(error) = child.kill()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "could not stop the voice worker");
        }
    }

    /// Wait briefly for the worker to exit, killing it if it lingers.
    fn reap(mut child: Child) -> Option<ExitStatus> {
        let deadline = Instant::now() + CANCEL_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                _ => {
                    stop_worker(&mut child);
                    return match child.wait() {
                        Ok(status) => Some(status),
                        Err(error) => {
                            tracing::warn!(%error, "could not reap the voice worker");
                            None
                        }
                    };
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

    /// The worker vanished without reporting a result or an error. Explain
    /// what happened without taking the session with it.
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
        message.push_str(" — the voice worker runs separately, so your session is unaffected");
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
/// receives transient progress messages. `VoiceCommand::Finish` stops capture
/// and starts transcription; `VoiceCommand::Cancel`, or a dropped command
/// sender, closes the worker's stdin and returns a cancellation error without
/// a transcript.
#[cfg(not(target_os = "android"))]
pub fn run_dictation<F, G, H>(
    auth_path: &Path,
    on_partial: F,
    on_level: G,
    on_status: H,
    cancel_rx: std::sync::mpsc::Receiver<VoiceCommand>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    worker::run(auth_path, on_partial, on_level, on_status, cancel_rx)
}

#[cfg(target_os = "android")]
pub fn run_dictation<F, G, H>(
    _auth_path: &Path,
    _on_partial: F,
    _on_level: G,
    _on_status: H,
    _cancel_rx: std::sync::mpsc::Receiver<VoiceCommand>,
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
                message: "uploading audio...".to_string(),
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
        assert_eq!(parse_event("worker startup log line"), None);
        assert_eq!(parse_event("{\"event\":\"unknown\"}"), None);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn crash_error_includes_stderr_line_and_isolated_worker_hint() {
        let err = super::worker::worker_crash_error(
            None,
            "worker startup failed\ntranscription helper exited unexpectedly\n",
        );
        let message = err.to_string();
        assert!(message.contains("voice dictation stopped unexpectedly"));
        assert!(message.contains("transcription helper exited unexpectedly"));
        assert!(message.contains("session is unaffected"));
    }

    /// Fake-worker tests: drive_worker against short shell scripts standing
    /// in for the real worker, covering each way the child can end.
    #[cfg(all(unix, not(target_os = "android")))]
    mod fake_worker {
        use super::super::VoiceCommand;
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
            cancel_rx: mpsc::Receiver<VoiceCommand>,
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

        fn never_cancelled() -> mpsc::Receiver<VoiceCommand> {
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
                printf 'worker log noise\n'
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
        fn worker_abort_is_contained_and_described() {
            let script = r#"
                echo 'voice worker crashed during transcription' >&2
                kill -ABRT $$
            "#;
            let (result, _, _) = drive(script, never_cancelled());
            let message = result.expect_err("crash error").to_string();
            assert!(message.contains("signal 6"), "got: {message}");
            assert!(message.contains("SIGABRT"), "got: {message}");
            assert!(
                message.contains("voice worker crashed during transcription"),
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
        fn cancel_closes_stdin_and_discards_worker_result() {
            // EOF is cancellation. Even if a buggy worker emits a result
            // after that signal, it must never reach the canceled chat.
            let script = r#"
                if read -r _; then
                    exit 9
                fi
                i=0
                while [ "$i" -lt 70000 ]; do
                    printf 'worker noise\n'
                    i=$((i + 1))
                done
                printf '%s\n' '{"event":"result","text":"must-not-reach-ui"}'
            "#;
            let (cancel_tx, cancel_rx) = mpsc::channel();
            cancel_tx.send(VoiceCommand::Cancel).expect("queue cancel");
            let (result, _, _) = drive(script, cancel_rx);
            let error = result.expect_err("cancel must not return a transcript");
            assert!(error.to_string().contains("cancelled"));
        }

        #[test]
        fn finish_sends_newline_and_drains_large_upload_output() {
            // The worker sees a blank line as Finish and then performs the
            // upload/transcription phase while stdin remains open. The large
            // noise stream proves stdout is drained concurrently with that
            // phase instead of deadlocking at the pipe buffer boundary.
            let script = r#"
                if ! read -r command; then
                    exit 9
                fi
                if [ -n "$command" ]; then
                    exit 10
                fi
                i=0
                while [ "$i" -lt 70000 ]; do
                    printf 'worker noise\n'
                    i=$((i + 1))
                done
                printf '%s\n' '{"event":"result","text":"uploaded transcript"}'
            "#;
            let (finish_tx, finish_rx) = mpsc::channel();
            finish_tx.send(VoiceCommand::Finish).expect("queue finish");
            let (result, _, _) = drive(script, finish_rx);
            assert_eq!(result.expect("uploaded transcript"), "uploaded transcript");
        }
    }
}
