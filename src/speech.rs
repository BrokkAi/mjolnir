//! Prompt dictation support.
//!
//! macOS ships the Speech framework in Swift, so the TUI shells out to a tiny
//! helper rather than binding Objective-C APIs from Rust.

use anyhow::{Context, Result, bail};
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;

#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_MODEL_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-bilingual-zh-en-2023-02-20.tar.bz2";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_MODEL_DIR: &str = "sherpa-onnx-streaming-zipformer-bilingual-zh-en-2023-02-20";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_ENCODER: &str = "encoder-epoch-99-avg-1.onnx";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_DECODER: &str = "decoder-epoch-99-avg-1.onnx";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_JOINER: &str = "joiner-epoch-99-avg-1.onnx";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_TOKENS: &str = "tokens.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DictationBackendKind {
    AppleSpeech,
    SherpaOnnx,
}

impl DictationBackendKind {
    fn label(self) -> &'static str {
        match self {
            Self::AppleSpeech => "Apple Speech",
            Self::SherpaOnnx => "sherpa-onnx",
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SherpaOnnxModelPaths {
    dir: PathBuf,
    encoder: PathBuf,
    decoder: PathBuf,
    joiner: PathBuf,
    tokens: PathBuf,
}

#[cfg(not(target_os = "macos"))]
impl SherpaOnnxModelPaths {
    fn in_cache(cache_root: PathBuf) -> Self {
        let dir = cache_root.join("voice").join(SHERPA_ONNX_MODEL_DIR);
        Self {
            encoder: dir.join(SHERPA_ONNX_ENCODER),
            decoder: dir.join(SHERPA_ONNX_DECODER),
            joiner: dir.join(SHERPA_ONNX_JOINER),
            tokens: dir.join(SHERPA_ONNX_TOKENS),
            dir,
        }
    }

    fn is_installed(&self) -> bool {
        self.encoder.is_file()
            && self.decoder.is_file()
            && self.joiner.is_file()
            && self.tokens.is_file()
    }
}

fn default_backend_kind() -> DictationBackendKind {
    if cfg!(target_os = "macos") {
        DictationBackendKind::AppleSpeech
    } else {
        DictationBackendKind::SherpaOnnx
    }
}

#[cfg(not(target_os = "macos"))]
fn mjolnir_cache_dir() -> Result<PathBuf> {
    dirs::cache_dir()
        .map(|dir| dir.join("mjolnir"))
        .context("locate user cache directory")
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_model_paths() -> Result<SherpaOnnxModelPaths> {
    Ok(SherpaOnnxModelPaths::in_cache(mjolnir_cache_dir()?))
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_auto_setup_message(paths: &SherpaOnnxModelPaths) -> String {
    format!(
        "voice dictation uses sherpa-onnx on this platform. Mjolnir can set this up automatically by downloading the default model from {SHERPA_ONNX_MODEL_URL} into {}.",
        paths.dir.display()
    )
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_cli_hint(paths: &SherpaOnnxModelPaths) -> String {
    format!(
        "expected sherpa-onnx model files were not found under {}. Mjolnir will auto-install them once the bundled sherpa-onnx runtime is available.",
        paths.dir.display()
    )
}

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
    run_sherpa_onnx_dictation()
}

#[cfg(not(target_os = "macos"))]
fn run_sherpa_onnx_dictation() -> Result<String> {
    let paths = sherpa_onnx_model_paths()?;
    if !paths.is_installed() {
        bail!(
            "{} {}",
            sherpa_onnx_auto_setup_message(&paths),
            sherpa_onnx_cli_hint(&paths)
        );
    }

    bail!(
        "sherpa-onnx dictation runtime is not bundled yet; model cache is ready at {}",
        paths.dir.display()
    )
}

pub fn dictation_error_message(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("No such file or directory") {
        return match default_backend_kind() {
            DictationBackendKind::AppleSpeech => {
                "voice dictation requires the swift command on PATH".to_string()
            }
            DictationBackendKind::SherpaOnnx => {
                "voice dictation requires the bundled sherpa-onnx runtime".to_string()
            }
        };
    }
    if message.contains("sherpa-onnx") {
        return message;
    }
    format!(
        "voice dictation failed using {}: {message}",
        default_backend_kind().label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backend_uses_apple_speech_only_on_macos() {
        let expected = if cfg!(target_os = "macos") {
            DictationBackendKind::AppleSpeech
        } else {
            DictationBackendKind::SherpaOnnx
        };
        assert_eq!(default_backend_kind(), expected);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sherpa_model_paths_are_under_voice_cache() {
        let paths = SherpaOnnxModelPaths::in_cache(PathBuf::from("/cache/mjolnir"));
        assert_eq!(
            paths.dir,
            PathBuf::from("/cache/mjolnir")
                .join("voice")
                .join(SHERPA_ONNX_MODEL_DIR)
        );
        assert_eq!(paths.encoder, paths.dir.join(SHERPA_ONNX_ENCODER));
        assert_eq!(paths.decoder, paths.dir.join(SHERPA_ONNX_DECODER));
        assert_eq!(paths.joiner, paths.dir.join(SHERPA_ONNX_JOINER));
        assert_eq!(paths.tokens, paths.dir.join(SHERPA_ONNX_TOKENS));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sherpa_setup_message_is_actionable_without_yaml() {
        let paths = SherpaOnnxModelPaths::in_cache(PathBuf::from("/cache/mjolnir"));
        let message = sherpa_onnx_auto_setup_message(&paths);
        assert!(message.contains("sherpa-onnx"));
        assert!(message.contains("automatically"));
        assert!(message.contains(SHERPA_ONNX_MODEL_URL));
        assert!(message.contains("/cache/mjolnir"));
        assert!(!message.to_lowercase().contains("yaml"));
    }
}
