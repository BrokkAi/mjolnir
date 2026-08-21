mod backend;

use serde::Serialize;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WorkerEvent {
    Status { message: String },
    Partial { text: String },
    Level { value: f32 },
    Result { text: String, finish: &'static str },
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
    let auto_send_silence = match parse_auto_send_silence(std::env::args().skip(1)) {
        Ok(delay) => delay,
        Err(message) => {
            emit(&WorkerEvent::Error { message });
            return 2;
        }
    };
    let (cancel_tx, cancel_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let _ = cancel_tx.send(());
    });
    match backend::run(
        |text| emit(&WorkerEvent::Partial { text }),
        |value| emit(&WorkerEvent::Level { value }),
        |message| emit(&WorkerEvent::Status { message }),
        auto_send_silence,
        cancel_rx,
    ) {
        Ok(result) => {
            emit(&WorkerEvent::Result {
                text: result.text,
                finish: result.finish.as_str(),
            });
            0
        }
        Err(error) => {
            emit(&WorkerEvent::Error {
                message: format!("{error:#}"),
            });
            1
        }
    }
}

fn parse_auto_send_silence(
    args: impl IntoIterator<Item = String>,
) -> Result<Option<Duration>, String> {
    let mut args = args.into_iter();
    let mut auto_send_silence = None;
    while let Some(arg) = args.next() {
        if arg != "--auto-send-silence-ms" {
            return Err(format!("unknown voice worker argument: {arg}"));
        }
        if auto_send_silence.is_some() {
            return Err("voice auto-send delay was supplied more than once".to_string());
        }
        let Some(value) = args.next() else {
            return Err(
                "--auto-send-silence-ms requires a whole number of milliseconds".to_string(),
            );
        };
        let milliseconds = value
            .parse::<u64>()
            .map_err(|_| format!("--auto-send-silence-ms must be a whole number, got {value:?}"))?;
        if milliseconds == 0 {
            return Err("--auto-send-silence-ms must be greater than zero".to_string());
        }
        auto_send_silence = Some(Duration::from_millis(milliseconds));
    }
    Ok(auto_send_silence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_optional_auto_send_silence() {
        assert_eq!(parse_auto_send_silence([]).expect("no delay"), None);
        assert_eq!(
            parse_auto_send_silence(["--auto-send-silence-ms".to_string(), "6000".to_string()])
                .expect("delay"),
            Some(Duration::from_secs(6))
        );
    }

    #[test]
    fn rejects_invalid_auto_send_silence() {
        for args in [
            vec!["--auto-send-silence-ms".to_string()],
            vec!["--auto-send-silence-ms".to_string(), "0".to_string()],
            vec!["--auto-send-silence-ms".to_string(), "many".to_string()],
            vec!["--unexpected".to_string()],
        ] {
            assert!(parse_auto_send_silence(args).is_err());
        }
    }
}
