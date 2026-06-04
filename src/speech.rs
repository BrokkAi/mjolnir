//! Prompt dictation support.
//!
//! macOS ships the Speech framework in Swift, so the TUI shells out to a tiny
//! helper rather than binding Objective-C APIs from Rust.

use anyhow::{Context, Result, bail};

#[cfg(target_os = "macos")]
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

#[cfg(target_os = "macos")]
use base64::{Engine as _, engine::general_purpose};

#[cfg(target_os = "macos")]
const SWIFT_HELPER: &str = r#"
import AVFoundation
import Darwin
import Foundation
import Speech

struct Options {
    var timeout: TimeInterval = 600
    var localeIdentifier: String? = nil
}

func parseOptions(_ args: [String]) -> Options {
    var options = Options()
    var index = 0
    while index < args.count {
        switch args[index] {
        case "--timeout" where index + 1 < args.count:
            options.timeout = TimeInterval(args[index + 1]) ?? options.timeout
            index += 2
        case "--locale" where index + 1 < args.count:
            options.localeIdentifier = args[index + 1]
            index += 2
        default:
            index += 1
        }
    }
    return options
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

func emit(_ kind: String, _ text: String) {
    let encoded = Data(text.utf8).base64EncodedString()
    print("\(kind)\t\(encoded)")
    fflush(stdout)
}

func requestSpeechAuthorization() {
    let semaphore = DispatchSemaphore(value: 0)
    var status = SFSpeechRecognizerAuthorizationStatus.notDetermined
    SFSpeechRecognizer.requestAuthorization { nextStatus in
        status = nextStatus
        semaphore.signal()
    }
    semaphore.wait()
    guard status == .authorized else {
        fail("speech recognition permission was not granted")
    }
}

func requestMicrophoneAuthorization() {
    let semaphore = DispatchSemaphore(value: 0)
    var granted = false
    AVCaptureDevice.requestAccess(for: .audio) { nextGranted in
        granted = nextGranted
        semaphore.signal()
    }
    semaphore.wait()
    guard granted else {
        fail("microphone permission was not granted")
    }
}

let options = parseOptions(Array(CommandLine.arguments.dropFirst()))
requestSpeechAuthorization()
requestMicrophoneAuthorization()

let locale = options.localeIdentifier.map(Locale.init(identifier:))
let recognizer: SFSpeechRecognizer?
if let locale {
    recognizer = SFSpeechRecognizer(locale: locale)
} else {
    recognizer = SFSpeechRecognizer()
}
guard let speechRecognizer = recognizer, speechRecognizer.isAvailable else {
    fail("speech recognizer is not available")
}

let engine = AVAudioEngine()
let request = SFSpeechAudioBufferRecognitionRequest()
request.shouldReportPartialResults = true

let inputNode = engine.inputNode
let format = inputNode.outputFormat(forBus: 0)
inputNode.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in
    request.append(buffer)
}

var bestText = ""
var finished = false
let startedAt = Date()

let task = speechRecognizer.recognitionTask(with: request) { result, error in
    if let result {
        bestText = result.bestTranscription.formattedString
        emit("PARTIAL", bestText)
        if result.isFinal {
            finished = true
        }
    }
    if error != nil {
        finished = true
    }
}

do {
    engine.prepare()
    try engine.start()
} catch {
    fail("could not start microphone capture: \(error.localizedDescription)")
}

while !finished {
    RunLoop.current.run(mode: .default, before: Date(timeIntervalSinceNow: 0.05))
    if Date().timeIntervalSince(startedAt) >= options.timeout {
        break
    }
}

engine.stop()
inputNode.removeTap(onBus: 0)
request.endAudio()
task.cancel()

emit("FINAL", bestText.trimmingCharacters(in: .whitespacesAndNewlines))
"#;

#[cfg(target_os = "macos")]
enum HelperLine {
    Partial(String),
    Final(String),
}

#[cfg(target_os = "macos")]
pub fn run_dictation<F>(mut on_partial: F, cancel_rx: mpsc::Receiver<()>) -> Result<String>
where
    F: FnMut(String),
{
    let mut child = Command::new("swift")
        .arg("-")
        .arg("--")
        .arg("--timeout")
        .arg("600")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start swift speech helper")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .context("open swift speech helper stdin")?;
        stdin
            .write_all(SWIFT_HELPER.as_bytes())
            .context("write swift speech helper")?;
    }

    let stdout = child
        .stdout
        .take()
        .context("open swift speech helper stdout")?;
    let (line_tx, line_rx) = mpsc::channel::<Result<HelperLine>>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let parsed = line
                .context("read swift speech helper output")
                .and_then(|line| decode_helper_line(&line));
            if line_tx.send(parsed).is_err() {
                break;
            }
        }
    });

    let mut final_text = None;
    let mut last_partial = None;
    let mut cancelled = false;
    let status = loop {
        while let Ok(line) = line_rx.try_recv() {
            match line? {
                HelperLine::Partial(text) => {
                    if last_partial.as_deref() != Some(text.as_str()) {
                        on_partial(text.clone());
                        last_partial = Some(text);
                    }
                }
                HelperLine::Final(text) => final_text = Some(text),
            }
        }

        if cancel_rx.try_recv().is_ok() {
            cancelled = true;
            let _ = child.kill();
        }

        if let Some(status) = child.try_wait().context("poll swift speech helper")? {
            break status;
        }

        thread::sleep(Duration::from_millis(30));
    };

    while let Ok(line) = line_rx.try_recv() {
        match line? {
            HelperLine::Partial(text) => {
                if last_partial.as_deref() != Some(text.as_str()) {
                    on_partial(text.clone());
                    last_partial = Some(text);
                }
            }
            HelperLine::Final(text) => final_text = Some(text),
        }
    }

    let mut stderr = String::new();
    if let Some(mut stderr_pipe) = child.stderr.take() {
        stderr_pipe
            .read_to_string(&mut stderr)
            .context("read swift speech helper stderr")?;
    }

    if !cancelled && !status.success() {
        let stderr = stderr.trim().to_string();
        bail!(
            "{}",
            if stderr.is_empty() {
                "speech helper failed".to_string()
            } else {
                stderr
            }
        );
    }

    let text = final_text
        .or(last_partial)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !cancelled && text.is_empty() {
        bail!("no speech was recognized");
    }
    Ok(text)
}

#[cfg(target_os = "macos")]
fn decode_helper_line(line: &str) -> Result<HelperLine> {
    let Some((kind, encoded)) = line.split_once('\t') else {
        bail!("unexpected swift speech helper output");
    };
    let bytes = general_purpose::STANDARD
        .decode(encoded)
        .context("decode swift speech helper line")?;
    let text = String::from_utf8(bytes).context("decode swift speech helper text")?;
    match kind {
        "PARTIAL" => Ok(HelperLine::Partial(text)),
        "FINAL" => Ok(HelperLine::Final(text)),
        _ => bail!("unexpected swift speech helper output kind: {kind}"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn run_dictation<F>(_on_partial: F, _cancel_rx: std::sync::mpsc::Receiver<()>) -> Result<String>
where
    F: FnMut(String),
{
    bail!("voice dictation is only available on macOS")
}

pub fn dictation_error_message(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("No such file or directory") {
        return "voice dictation requires the swift command on PATH".to_string();
    }
    format!("voice dictation failed: {message}")
}
