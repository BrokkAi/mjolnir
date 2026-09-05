mod backend;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WorkerEvent {
    Status { message: String },
    Partial { text: String },
    Level { value: f32 },
    Result { text: String },
    Error { message: String },
}

fn emit(event: &WorkerEvent) {
    if let Ok(line) = serde_json::to_string(event) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    match run_with_args(std::env::args_os().skip(1)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("voice dictation failed: {error:#}");
            emit(&WorkerEvent::Error {
                message: format!("{error:#}"),
            });
            1
        }
    }
}

fn run_with_args<I>(args: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let auth_path = parse_auth_path(args)?;
    let (command_tx, command_rx) = mpsc::channel();
    thread::spawn(move || read_commands(command_tx));

    match backend::run(
        |text| emit(&WorkerEvent::Partial { text }),
        |value| emit(&WorkerEvent::Level { value }),
        |message| emit(&WorkerEvent::Status { message }),
        auth_path,
        command_rx,
    )? {
        backend::RunOutcome::Transcribed(text) => emit(&WorkerEvent::Result { text }),
        // Keep the result event in the protocol on cancellation. The parent
        // supervisor treats an empty result as a clean cancelled run.
        backend::RunOutcome::Cancelled => emit(&WorkerEvent::Result {
            text: String::new(),
        }),
    }
    Ok(())
}

fn parse_auth_path<I>(args: I) -> Result<PathBuf>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut auth_path = None;
    while let Some(arg) = args.next() {
        if arg == "--codex-auth" {
            let value = args.next().context("--codex-auth requires a path")?;
            auth_path = Some(PathBuf::from(value));
        } else if let Some(value) = arg
            .to_str()
            .and_then(|value| value.strip_prefix("--codex-auth="))
        {
            if value.is_empty() {
                bail!("--codex-auth requires a path");
            }
            auth_path = Some(PathBuf::from(value));
        } else {
            bail!("unknown argument {arg:?}; usage: mj-voice-worker --codex-auth PATH");
        }
    }
    auth_path.context("missing required --codex-auth PATH argument")
}

fn read_commands(command_tx: mpsc::Sender<backend::StdinCommand>) {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) => {
            let _ = command_tx.send(backend::StdinCommand::Cancel);
        }
        Ok(_) => {
            if command_tx.send(backend::StdinCommand::Finish).is_err() {
                return;
            }
            // Keep the pipe open after the finish newline. The parent closes
            // it to cancel a request that is already being uploaded.
            line.clear();
            loop {
                match stdin.read_line(&mut line) {
                    Ok(0) => {
                        let _ = command_tx.send(backend::StdinCommand::Cancel);
                        break;
                    }
                    Ok(_) => line.clear(),
                    Err(error) => {
                        let _ =
                            command_tx.send(backend::StdinCommand::InputError(error.to_string()));
                        break;
                    }
                }
            }
        }
        Err(error) => {
            let _ = command_tx.send(backend::StdinCommand::InputError(error.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auth_path_requires_explicit_profile() {
        assert_eq!(
            parse_auth_path([OsString::from("--codex-auth"), OsString::from("/tmp/a")])
                .expect("auth path"),
            PathBuf::from("/tmp/a")
        );
        assert!(parse_auth_path(std::iter::empty()).is_err());
    }

    #[test]
    fn parse_auth_path_accepts_equals_form() {
        assert_eq!(
            parse_auth_path([OsString::from("--codex-auth=/tmp/a")]).expect("auth path"),
            PathBuf::from("/tmp/a")
        );
    }
}
