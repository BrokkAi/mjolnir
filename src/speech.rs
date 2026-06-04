//! Prompt dictation support.
//!
//! macOS ships the Speech framework in Swift, so the TUI shells out to a tiny
//! helper rather than binding Objective-C APIs from Rust.

use anyhow::{Result, bail};

#[cfg(target_os = "macos")]
use anyhow::Context;

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
const DICTATION_TIMEOUT_SECONDS: &str = "600";
#[cfg(target_os = "macos")]
const DICTATION_SILENCE_SECONDS: &str = "20";

#[cfg(target_os = "macos")]
const SWIFT_HELPER: &str = r#"
import AVFoundation
import Darwin
import Foundation
import Speech

struct Options {
    var timeout: TimeInterval = 600
    var silence: TimeInterval = 20
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
        case "--silence" where index + 1 < args.count:
            options.silence = TimeInterval(args[index + 1]) ?? options.silence
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

final class LevelEmitter {
    private var lastEmitAt = Date.distantPast

    func emitLevel(from buffer: AVAudioPCMBuffer) {
        let now = Date()
        guard now.timeIntervalSince(lastEmitAt) >= 0.08 else {
            return
        }
        lastEmitAt = now

        guard let channelData = buffer.floatChannelData else {
            emit("LEVEL", "0")
            return
        }
        let channelCount = Int(buffer.format.channelCount)
        let frameLength = Int(buffer.frameLength)
        guard channelCount > 0 && frameLength > 0 else {
            emit("LEVEL", "0")
            return
        }

        var sum: Float = 0
        for channel in 0..<channelCount {
            let samples = channelData[channel]
            for frame in 0..<frameLength {
                let sample = samples[frame]
                sum += sample * sample
            }
        }
        let rms = sqrt(sum / Float(channelCount * frameLength))
        let normalized = min(max(rms * 18, 0), 1)
        emit("LEVEL", String(format: "%.3f", normalized))
    }
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
let levelEmitter = LevelEmitter()
inputNode.installTap(onBus: 0, bufferSize: 1024, format: format) { buffer, _ in
    request.append(buffer)
    levelEmitter.emitLevel(from: buffer)
}

var bestText = ""
var lastResultAt = Date()
var finished = false
let startedAt = Date()

let task = speechRecognizer.recognitionTask(with: request) { result, error in
    if let result {
        bestText = result.bestTranscription.formattedString
        emit("PARTIAL", bestText)
        lastResultAt = Date()
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
    if !bestText.isEmpty && Date().timeIntervalSince(lastResultAt) >= options.silence {
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
    Level(f32),
}

#[cfg(target_os = "macos")]
pub fn run_dictation<F, G>(
    mut on_partial: F,
    mut on_level: G,
    cancel_rx: mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
{
    let mut child = Command::new("swift")
        .arg("-")
        .arg("--")
        .arg("--timeout")
        .arg(DICTATION_TIMEOUT_SECONDS)
        .arg("--silence")
        .arg(DICTATION_SILENCE_SECONDS)
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
                HelperLine::Level(level) => on_level(level),
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
            HelperLine::Level(level) => on_level(level),
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
        "LEVEL" => {
            let level = text
                .parse::<f32>()
                .context("parse swift speech helper level")?;
            Ok(HelperLine::Level(level.clamp(0.0, 1.0)))
        }
        _ => bail!("unexpected swift speech helper output kind: {kind}"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn run_dictation<F, G>(
    _on_partial: F,
    _on_level: G,
    _cancel_rx: std::sync::mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
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
