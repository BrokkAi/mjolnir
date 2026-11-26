//! Harness-hook client for draining the worker-owned session mailbox.

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};
use serde_json::json;
use std::io::{Read, Write};
use std::path::Path;

/// Let a harness retain leased input before acknowledging it. Reuse the same
/// framed IPC, request deadline, and long Unix socket path handling as hooks.
pub fn request(socket: &Path) -> Result<()> {
    let request: mj_core::relay::RelayRequest = serde_json::from_reader(std::io::stdin().lock())?;
    anyhow::ensure!(
        matches!(
            request,
            mj_core::relay::RelayRequest::DrainMailbox { .. }
                | mj_core::relay::RelayRequest::AckMailbox { .. }
        ),
        "expected a mailbox drain or acknowledgement"
    );
    write_output(
        &mut std::io::stdout().lock(),
        &serde_json::to_value(send_request(socket, request)?)?,
    )
}

/// Output format expected by the calling harness's hook contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookOutputFormat {
    #[default]
    Standard,
    Antigravity,
}

/// Consume one harness hook request, write its response, then acknowledge the
/// relay lease. Relay unavailability is deliberately fail-open: an unacked
/// lease returns to the mailbox after its timeout or a worker restart.
pub fn run(socket: &Path, hook_event: &str) -> Result<()> {
    run_with_format(socket, hook_event, HookOutputFormat::Standard)
}

/// Consume one harness hook request using the specified hook output format.
pub fn run_with_format(socket: &Path, hook_event: &str, format: HookOutputFormat) -> Result<()> {
    run_hook(
        socket,
        hook_event,
        format,
        std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
    )
}

fn run_hook(
    socket: &Path,
    hook_event: &str,
    format: HookOutputFormat,
    mut stdin: impl Read,
    stdout: &mut impl Write,
) -> Result<()> {
    let hook_input = match serde_json::from_reader::<_, HookInput>(&mut stdin) {
        Ok(input) => input,
        Err(error) if error.is_io() => {
            return write_empty_with_diagnostic(
                stdout,
                &format!("could not consume hook input: {error}"),
            );
        }
        Err(_) => HookInput::default(),
    };
    if let Err(error) = std::io::copy(&mut stdin, &mut std::io::sink()) {
        return write_empty_with_diagnostic(
            stdout,
            &format!("could not consume hook input: {error}"),
        );
    }
    if is_subagent_hook_input(&hook_input) {
        return write_output(stdout, &json!({}));
    }

    if format == HookOutputFormat::Antigravity && hook_event == "Stop" {
        return match send_request(
            socket,
            mj_core::relay::RelayRequest::AntigravityStop {
                conversation_id: hook_input.conversation_id,
                fully_idle: hook_input.fully_idle,
                termination_reason: hook_input.termination_reason,
            },
        ) {
            Ok(mj_core::relay::RelayResponsePayload::AntigravityStopObserved) => {
                write_output(stdout, &json!({}))
            }
            Ok(other) => {
                write_empty_with_diagnostic(stdout, &format!("worker refused Stop: {other:?}"))
            }
            Err(error) => {
                write_empty_with_diagnostic(stdout, &format!("could not observe Stop: {error:#}"))
            }
        };
    }

    let (conversation_id, initial_num_steps) = match format {
        HookOutputFormat::Standard => (None, None),
        HookOutputFormat::Antigravity => (
            hook_input.conversation_id.as_deref(),
            hook_input.initial_num_steps,
        ),
    };

    match drain(socket, hook_event, conversation_id, initial_num_steps) {
        Ok((Some(text), count, Some(lease_id))) if count > 0 => {
            let output = match format {
                HookOutputFormat::Standard => json!({
                    "hookSpecificOutput": {
                        "hookEventName": hook_event,
                        "additionalContext": text,
                    }
                }),
                HookOutputFormat::Antigravity => json!({
                    "injectSteps": [
                        {
                            "systemMessage": {
                                "systemMessage": text,
                            }
                        }
                    ]
                }),
            };
            write_output(stdout, &output)?;
            if let Err(error) = acknowledge(socket, &lease_id) {
                mj_core::diagnostic_stderr!(
                    "Mjolnir mailbox hook: could not acknowledge delivered events: {error:#}"
                );
            }
            Ok(())
        }
        Ok((None, 0, None)) => write_output(stdout, &json!({})),
        Ok(_) => write_empty_with_diagnostic(stdout, "worker returned an incomplete mailbox lease"),
        Err(error) => {
            write_empty_with_diagnostic(stdout, &format!("could not drain mailbox: {error:#}"))
        }
    }
}

#[derive(Default, serde::Deserialize)]
struct HookInput {
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "initialNumSteps")]
    initial_num_steps: Option<i64>,
    #[serde(default, rename = "fullyIdle")]
    fully_idle: Option<bool>,
    #[serde(default, rename = "terminationReason")]
    termination_reason: Option<String>,
}

fn is_subagent_hook_input(input: &HookInput) -> bool {
    input
        .agent_id
        .as_deref()
        .is_some_and(|agent_id| !agent_id.is_empty())
}

fn drain(
    socket: &Path,
    hook_event: &str,
    conversation_id: Option<&str>,
    initial_num_steps: Option<i64>,
) -> Result<(Option<String>, usize, Option<String>)> {
    anyhow::ensure!(
        matches!(hook_event, "PostToolUse" | "PostToolBatch"),
        "unsupported mailbox hook event"
    );
    match send_request(
        socket,
        mj_core::relay::RelayRequest::DrainMailbox {
            hook_event: hook_event.to_owned(),
            conversation_id: conversation_id.map(str::to_owned),
            initial_num_steps,
        },
    )? {
        mj_core::relay::RelayResponsePayload::MailboxDrained {
            lease_id,
            text,
            count,
            ..
        } => Ok((text, count, lease_id)),
        other => bail!("worker refused mailbox hook request: {other:?}"),
    }
}

fn acknowledge(socket: &Path, lease_id: &str) -> Result<()> {
    match send_request(
        socket,
        mj_core::relay::RelayRequest::AckMailbox {
            lease_id: lease_id.to_owned(),
        },
    )? {
        mj_core::relay::RelayResponsePayload::MailboxAcknowledged { acknowledged: true } => Ok(()),
        mj_core::relay::RelayResponsePayload::MailboxAcknowledged {
            acknowledged: false,
        } => bail!("worker no longer holds the mailbox lease; it may be delivered again"),
        other => bail!("worker refused mailbox acknowledgement: {other:?}"),
    }
}

#[cfg(unix)]
fn send_request(
    socket: &Path,
    request: mj_core::relay::RelayRequest,
) -> Result<mj_core::relay::RelayResponsePayload> {
    const REQUEST_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(mj_core::mailbox::MAILBOX_HOOK_REQUEST_TIMEOUT_SECS);
    let request_id = mj_core::state::new_session_id()?;
    let mut stream = mj_core::local_sockets::connect_unix_stream(socket)
        .with_context(|| format!("connect to worker control socket {}", socket.display()))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let envelope = mj_core::relay::RelayRequestEnvelope {
        request_id: request_id.clone(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request,
    };
    serde_json::to_writer(&mut stream, &envelope)?;
    stream.write_all(b"\n")?;

    let mut reader = std::io::BufReader::new(stream);
    let mut line = Vec::new();
    let (read, complete) =
        mj_core::relay::read_bounded_line(&mut reader, &mut line, mj_core::relay::MAX_FRAME_BYTES)?;
    anyhow::ensure!(read > 0, "worker closed the mailbox hook connection");
    anyhow::ensure!(complete, "worker returned an incomplete mailbox hook frame");
    let response: mj_core::relay::RelayResponseEnvelope = serde_json::from_slice(&line)?;
    anyhow::ensure!(
        response.request_id == request_id,
        "mailbox hook reply identity mismatch"
    );
    match response.body {
        mj_core::relay::RelayResponseBody::Ok { payload } => Ok(payload),
        other => bail!("worker refused mailbox hook request: {other:?}"),
    }
}

/// Workers run on Unix; the hook command compiles everywhere so the CLI stays
/// one shape, and says plainly where it cannot run.
#[cfg(not(unix))]
fn send_request(
    socket: &Path,
    _request: mj_core::relay::RelayRequest,
) -> Result<mj_core::relay::RelayResponsePayload> {
    anyhow::bail!(
        "the mailbox hook socket {} needs a Unix platform",
        socket.display()
    )
}

fn write_output(stdout: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    let mut output = serde_json::to_vec(value)?;
    output.push(b'\n');
    stdout.write_all(&output)?;
    stdout.flush()?;
    Ok(())
}

fn write_empty_with_diagnostic(stdout: &mut impl Write, message: &str) -> Result<()> {
    mj_core::diagnostic_stderr!("Mjolnir mailbox hook: {message}");
    write_output(stdout, &json!({}))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mj_core::relay::{
        RelayRequest, RelayRequestEnvelope, RelayResponseBody, RelayResponseEnvelope,
        RelayResponsePayload,
    };
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    // Hard-won: eab4bed08: macOS inherited nonblocking mode on the fake Stop socket.
    #[test]
    fn mailbox_hook_antigravity_stop_observes_main_conversation_and_stays_passive() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "no Stop request arrived"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept Stop request: {error}"),
                }
            };
            // macOS accepted sockets inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let envelope: RelayRequestEnvelope = serde_json::from_str(&line).unwrap();
            assert!(matches!(
                envelope.request,
                RelayRequest::AntigravityStop {
                    conversation_id: Some(id),
                    fully_idle: Some(false),
                    termination_reason: Some(reason),
                } if id == "main" && reason == "USER_CANCELED"
            ));
            serde_json::to_writer(
                &mut stream,
                &RelayResponseEnvelope {
                    request_id: envelope.request_id,
                    protocol_version: envelope.protocol_version,
                    body: RelayResponseBody::Ok {
                        payload: RelayResponsePayload::AntigravityStopObserved,
                    },
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            listener
        });

        let mut output = Vec::new();
        run_hook(
            &socket,
            "Stop",
            HookOutputFormat::Antigravity,
            br#"{"conversationId":"main","fullyIdle":false,"terminationReason":"USER_CANCELED"}"#
                .as_slice(),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"{}\n");

        let listener = server.join().unwrap();
        output.clear();
        run_hook(
            &socket,
            "Stop",
            HookOutputFormat::Antigravity,
            br#"{"conversationId":"main","fullyIdle":true,"agent_id":"subagent"}"#.as_slice(),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"{}\n");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        drop(listener);
        output.clear();
        run_hook(
            &socket,
            "Stop",
            HookOutputFormat::Antigravity,
            br#"{"conversationId":"main","fullyIdle":false}"#.as_slice(),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"{}\n");
    }
}
