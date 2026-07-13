mod backend;

use serde::Serialize;
use std::io::{BufRead, Write};
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
        cancel_rx,
    ) {
        Ok(text) => {
            emit(&WorkerEvent::Result { text });
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
