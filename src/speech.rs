//! Prompt dictation support.
//!
//! macOS uses the system Speech framework through a tiny Swift helper. Other
//! platforms use sherpa-onnx's microphone example binary when it is available.

use anyhow::{Context, Result, bail};
#[cfg(not(target_os = "macos"))]
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

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
const SHERPA_ONNX_MICROPHONE_BIN: &str = "sherpa-onnx-microphone";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_RUNTIME_VERSION: &str = "v1.13.2";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_RELEASE_BASE_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download";
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_DICTATION_TIMEOUT: Duration = Duration::from_secs(600);
#[cfg(not(target_os = "macos"))]
const SHERPA_ONNX_DICTATION_SILENCE: Duration = Duration::from_secs(20);

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
    fn in_dir(dir: PathBuf) -> Self {
        Self {
            encoder: dir.join(SHERPA_ONNX_ENCODER),
            decoder: dir.join(SHERPA_ONNX_DECODER),
            joiner: dir.join(SHERPA_ONNX_JOINER),
            tokens: dir.join(SHERPA_ONNX_TOKENS),
            dir,
        }
    }

    fn in_cache(cache_root: PathBuf) -> Self {
        Self::in_dir(cache_root.join("voice").join(SHERPA_ONNX_MODEL_DIR))
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
        .map(|dir| dir.join("mj"))
        .context("locate user cache directory")
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_model_paths() -> Result<SherpaOnnxModelPaths> {
    Ok(SherpaOnnxModelPaths::in_cache(mjolnir_cache_dir()?))
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_model_setup_message(paths: &SherpaOnnxModelPaths) -> String {
    format!(
        "voice dictation uses sherpa-onnx on this platform. Mjolnir could not install the default model automatically under {}. Missing files: {}, {}, {}, and {}.",
        paths.dir.display(),
        SHERPA_ONNX_ENCODER,
        SHERPA_ONNX_DECODER,
        SHERPA_ONNX_JOINER,
        SHERPA_ONNX_TOKENS
    )
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_runtime_setup_message() -> String {
    format!(
        "Mjolnir could not install the sherpa-onnx runtime automatically, and {SHERPA_ONNX_MICROPHONE_BIN} was not available on PATH."
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
    on_partial: F,
    _on_level: G,
    cancel_rx: mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
{
    run_sherpa_onnx_dictation(on_partial, cancel_rx)
}

#[cfg(not(target_os = "macos"))]
fn run_sherpa_onnx_dictation<F>(mut on_partial: F, cancel_rx: mpsc::Receiver<()>) -> Result<String>
where
    F: FnMut(String),
{
    let paths = sherpa_onnx_model_paths()?;
    ensure_sherpa_onnx_model_installed(&paths)?;
    if !paths.is_installed() {
        bail!("{}", sherpa_onnx_model_setup_message(&paths));
    }

    let mut child = spawn_sherpa_onnx_microphone(&paths)?;
    let stdout = child
        .stdout
        .take()
        .context("open sherpa-onnx microphone stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("open sherpa-onnx microphone stderr")?;

    let (line_tx, line_rx) = mpsc::channel::<Result<String>>();
    thread::spawn(move || read_sherpa_output_lines(stdout, line_tx));
    let stderr_reader = thread::spawn(move || {
        let mut output = String::new();
        let mut reader = stderr;
        reader
            .read_to_string(&mut output)
            .context("read sherpa-onnx microphone stderr")?;
        Ok::<_, anyhow::Error>(output)
    });

    let started_at = Instant::now();
    let mut last_result_at = Instant::now();
    let mut last_text: Option<String> = None;
    let mut cancelled = false;
    let mut killed_by_timeout = false;
    let status = loop {
        while let Ok(line) = line_rx.try_recv() {
            if let Some(text) = parse_sherpa_transcript_line(&line?) {
                if last_text.as_deref() != Some(text.as_str()) {
                    on_partial(text.clone());
                    last_text = Some(text);
                }
                last_result_at = Instant::now();
            }
        }

        if cancel_rx.try_recv().is_ok() {
            cancelled = true;
            break kill_and_wait(&mut child)?;
        }

        if last_result_at.elapsed() >= SHERPA_ONNX_DICTATION_SILENCE {
            killed_by_timeout = true;
            break kill_and_wait(&mut child)?;
        }

        if started_at.elapsed() >= SHERPA_ONNX_DICTATION_TIMEOUT {
            killed_by_timeout = true;
            break kill_and_wait(&mut child)?;
        }

        if let Some(status) = child.try_wait().context("poll sherpa-onnx microphone")? {
            break status;
        }

        thread::sleep(Duration::from_millis(30));
    };

    while let Ok(line) = line_rx.try_recv() {
        if let Some(text) = parse_sherpa_transcript_line(&line?)
            && last_text.as_deref() != Some(text.as_str())
        {
            on_partial(text.clone());
            last_text = Some(text);
        }
    }

    let stderr = stderr_reader
        .join()
        .unwrap_or_else(|_| Ok("sherpa-onnx stderr reader panicked".to_string()))?;

    if !cancelled && !killed_by_timeout && !status.success() {
        let stderr = stderr.trim();
        bail!(
            "{}",
            if stderr.is_empty() {
                "sherpa-onnx microphone failed".to_string()
            } else {
                stderr.to_string()
            }
        );
    }

    let text = last_text.unwrap_or_default().trim().to_string();
    if !cancelled && text.is_empty() {
        bail!("no speech was recognized");
    }
    Ok(text)
}

#[cfg(not(target_os = "macos"))]
fn spawn_sherpa_onnx_microphone(paths: &SherpaOnnxModelPaths) -> Result<std::process::Child> {
    let program = sherpa_onnx_microphone_program()?;
    let mut command = Command::new(&program);
    command
        .arg(format!("--tokens={}", paths.tokens.display()))
        .arg(format!("--encoder={}", paths.encoder.display()))
        .arg(format!("--decoder={}", paths.decoder.display()))
        .arg(format!("--joiner={}", paths.joiner.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().with_context(|| {
        format!(
            "start sherpa-onnx microphone backend {}; {}",
            display_program(&program),
            sherpa_onnx_runtime_setup_message()
        )
    })
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_microphone_program() -> Result<PathBuf> {
    if let Some(path) = find_command_on_path(SHERPA_ONNX_MICROPHONE_BIN) {
        return Ok(path);
    }
    ensure_sherpa_onnx_runtime_installed()?.context(sherpa_onnx_runtime_setup_message())
}

#[cfg(not(target_os = "macos"))]
fn display_program(program: &std::path::Path) -> String {
    program.display().to_string()
}

#[cfg(not(target_os = "macos"))]
fn ensure_sherpa_onnx_model_installed(paths: &SherpaOnnxModelPaths) -> Result<()> {
    if paths.is_installed() {
        return Ok(());
    }
    let parent = paths
        .dir
        .parent()
        .context("resolve sherpa-onnx model cache parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let archive = download_bytes(SHERPA_ONNX_MODEL_URL).context("download sherpa-onnx model")?;
    extract_tar_bz2(&archive, parent).context("extract sherpa-onnx model")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn ensure_sherpa_onnx_runtime_installed() -> Result<Option<PathBuf>> {
    let Some(asset) = sherpa_onnx_runtime_asset_name() else {
        return Ok(None);
    };
    let dir = mjolnir_cache_dir()?
        .join("voice")
        .join("sherpa-onnx-runtime")
        .join(SHERPA_ONNX_RUNTIME_VERSION);
    if let Some(path) = find_microphone_binary_under(&dir) {
        return Ok(Some(path));
    }

    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let url = format!("{SHERPA_ONNX_RELEASE_BASE_URL}/{SHERPA_ONNX_RUNTIME_VERSION}/{asset}");
    let archive = download_bytes(&url).context("download sherpa-onnx runtime")?;
    extract_tar_bz2(&archive, &dir).context("extract sherpa-onnx runtime")?;
    let Some(path) = find_microphone_binary_under(&dir) else {
        return Ok(None);
    };
    ensure_executable(&path)?;
    Ok(Some(path))
}

#[cfg(not(target_os = "macos"))]
fn sherpa_onnx_runtime_asset_name() -> Option<String> {
    let v = SHERPA_ONNX_RUNTIME_VERSION;
    let suffix = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64-static-no-tts",
        ("linux", "aarch64") => "linux-aarch64-static",
        ("windows", "x86_64") => "win-x64-static-MT-Release-no-tts",
        ("windows", "aarch64") => "win-arm64-static-MT-Release-no-tts",
        _ => return None,
    };
    Some(format!("sherpa-onnx-{v}-{suffix}.tar.bz2"))
}

#[cfg(not(target_os = "macos"))]
fn download_bytes(url: &str) -> Result<Vec<u8>> {
    // reqwest::blocking creates its own Tokio runtime, which panics when called
    // from within an existing Tokio context. Spawning a plain OS thread
    // guarantees there is no ambient runtime, regardless of the call site.
    let url = url.to_string();
    std::thread::spawn(move || {
        let response = reqwest::blocking::Client::builder()
            .user_agent("mjolnir-voice-setup")
            .build()
            .context("build download client")?
            .get(&url)
            .send()
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("download {url}"))?;
        response
            .bytes()
            .context("read download body")
            .map(|b| b.to_vec())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("download thread panicked"))?
}

#[cfg(not(target_os = "macos"))]
fn extract_tar_bz2(bytes: &[u8], dest: &Path) -> Result<()> {
    let decoder = bzip2::read::BzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("read tar.bz2 entries")? {
        let mut entry = entry.context("read tar.bz2 entry")?;
        let entry_path = entry.path().context("read tar.bz2 entry path")?;
        let relative = safe_archive_path(&entry_path).with_context(|| {
            format!(
                "archive entry escapes destination: {}",
                entry_path.display()
            )
        })?;
        let out_path = dest.join(relative);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        entry
            .unpack(&out_path)
            .with_context(|| format!("unpack {}", out_path.display()))?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn safe_archive_path(path: &Path) -> Result<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            _ => bail!("unsafe archive path {}", path.display()),
        }
    }
    if relative.as_os_str().is_empty() {
        bail!("empty archive path")
    }
    Ok(relative)
}

#[cfg(not(target_os = "macos"))]
fn find_command_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(executable_name(name)))
        .find(|candidate| candidate.is_file())
}

#[cfg(not(target_os = "macos"))]
fn find_microphone_binary_under(dir: &Path) -> Option<PathBuf> {
    let executable = executable_name(SHERPA_ONNX_MICROPHONE_BIN);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name == executable.as_str())
            {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(all(not(target_os = "macos"), unix))]
fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    if mode & 0o111 == 0 {
        permissions.set_mode(mode | 0o755);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("make {} executable", path.display()))?;
    }
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(unix)))]
fn ensure_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn executable_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[cfg(not(target_os = "macos"))]
fn kill_and_wait(child: &mut std::process::Child) -> Result<ExitStatus> {
    let _ = child.kill();
    child.wait().context("wait for sherpa-onnx microphone")
}

#[cfg(not(target_os = "macos"))]
fn read_sherpa_output_lines<R>(mut reader: R, line_tx: mpsc::Sender<Result<String>>)
where
    R: Read,
{
    let mut pending = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                for byte in &buffer[..read] {
                    match *byte {
                        b'\n' | b'\r' => {
                            if !pending.is_empty() {
                                let line = String::from_utf8(pending.clone())
                                    .context("decode sherpa-onnx microphone output");
                                if line_tx.send(line).is_err() {
                                    return;
                                }
                                pending.clear();
                            }
                        }
                        byte => pending.push(byte),
                    }
                }
            }
            Err(error) => {
                let _ = line_tx.send(Err(error).context("read sherpa-onnx microphone output"));
                return;
            }
        }
    }
    if !pending.is_empty() {
        let line = String::from_utf8(pending).context("decode sherpa-onnx microphone output");
        let _ = line_tx.send(line);
    }
}

#[cfg(not(target_os = "macos"))]
fn parse_sherpa_transcript_line(line: &str) -> Option<String> {
    let text = line.trim();
    if text.is_empty() || sherpa_line_is_status(text) {
        return None;
    }

    let text = text
        .split_once(':')
        .and_then(|(prefix, value)| prefix.trim().parse::<usize>().ok().map(|_| value.trim()))
        .unwrap_or(text)
        .trim();
    if text.is_empty() || sherpa_line_is_status(text) {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
fn sherpa_line_is_status(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("sherpa-onnx/csrc/")
        || lower.contains("parse-options.cc")
        || lower.contains("please speak")
        || lower.starts_with("started")
        || lower.starts_with("creating")
        || lower.starts_with("usage:")
        || lower.starts_with("options:")
        || lower.starts_with("--")
}

pub fn dictation_error_message(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("No such file or directory") {
        return match default_backend_kind() {
            DictationBackendKind::AppleSpeech => {
                "voice dictation requires the swift command on PATH".to_string()
            }
            DictationBackendKind::SherpaOnnx => {
                format!(
                    "voice dictation requires {SHERPA_ONNX_MICROPHONE_BIN}; Mjolnir installs the default sherpa-onnx runtime and model automatically on supported platforms"
                )
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
        let paths = SherpaOnnxModelPaths::in_cache(PathBuf::from("/cache/mj"));
        assert_eq!(
            paths.dir,
            PathBuf::from("/cache/mj")
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
        let paths = SherpaOnnxModelPaths::in_cache(PathBuf::from("/cache/mj"));
        let message = sherpa_onnx_model_setup_message(&paths);
        assert!(message.contains("sherpa-onnx"));
        assert!(message.contains("install"));
        assert!(message.contains("/cache/mj"));
        assert!(!message.to_lowercase().contains("yaml"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sherpa_runtime_message_names_path_fallback() {
        let message = sherpa_onnx_runtime_setup_message();
        assert!(message.contains(SHERPA_ONNX_MICROPHONE_BIN));
        assert!(message.contains("PATH"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sherpa_output_parser_ignores_status_lines() {
        assert_eq!(parse_sherpa_transcript_line("Started! Please speak"), None);
        assert_eq!(
            parse_sherpa_transcript_line(
                "sherpa-onnx/csrc/parse-options.cc:Read:361 --tokens=model/tokens.txt"
            ),
            None
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sherpa_output_parser_accepts_plain_and_numbered_transcripts() {
        assert_eq!(
            parse_sherpa_transcript_line("hello world"),
            Some("hello world".to_string())
        );
        assert_eq!(
            parse_sherpa_transcript_line("0: hello world"),
            Some("hello world".to_string())
        );
    }
}
