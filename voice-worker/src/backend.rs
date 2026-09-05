use anvil_llm::{codex_client::CodexClient, transcribe::TranscribeRequest};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr,
};

#[cfg(target_os = "linux")]
use libloading::{Library, Symbol};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

/// The worker accepts one newline to finish recording. The parent closes
/// stdin when the dictation is cancelled; that signal is also watched while
/// the transcription request is in flight.
#[derive(Debug)]
pub(super) enum StdinCommand {
    Finish,
    Cancel,
    InputError(String),
}

pub(super) enum RunOutcome {
    Transcribed(String),
    Cancelled,
}

pub(super) const DICTATION_TIMEOUT: Duration = Duration::from_secs(600);
const NO_AUDIO_TIMEOUT: Duration = Duration::from_secs(5);
const SAMPLE_RATE: u32 = 16_000;
const LEVEL_EMIT_INTERVAL: Duration = Duration::from_millis(80);
const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;

type AudioMessage = std::result::Result<Vec<f32>, String>;

#[derive(Default)]
struct AudioQueueState {
    overflowed: AtomicBool,
    disconnected: AtomicBool,
}

#[derive(Clone)]
struct AudioSender {
    tx: mpsc::SyncSender<AudioMessage>,
    state: Arc<AudioQueueState>,
}

impl AudioSender {
    fn try_send(&self, message: AudioMessage) {
        match self.tx.try_send(message) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                self.state.overflowed.store(true, Ordering::Relaxed);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.state.disconnected.store(true, Ordering::Relaxed);
            }
        }
    }
}

struct CapturedAudio {
    samples: Vec<f32>,
    sample_rate: u32,
    command_rx: mpsc::Receiver<StdinCommand>,
}

enum CaptureOutcome {
    Ready(CapturedAudio),
    Cancelled,
}

enum InputSource {
    Cpal(cpal::Stream),
    #[cfg(target_os = "linux")]
    Pulse {
        _input: PulseInput,
    },
}

#[cfg(target_os = "linux")]
struct PulseInput {
    stop: Arc<AtomicBool>,
    // The Pulse read may be blocked in a foreign library call. Detach this
    // thread on teardown; the worker process remains isolated from it and
    // exits promptly when its supervisor cancels the worker.
    _reader: thread::JoinHandle<()>,
}

#[cfg(target_os = "linux")]
impl Drop for PulseInput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct PulseSampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[cfg(target_os = "linux")]
type PulseSimpleNew = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const PulseSampleSpec,
    *const c_void,
    *const c_void,
    *mut c_int,
) -> *mut c_void;

#[cfg(target_os = "linux")]
type PulseSimpleRead = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;

#[cfg(target_os = "linux")]
type PulseSimpleFree = unsafe extern "C" fn(*mut c_void);

#[cfg(target_os = "linux")]
type PulseStrError = unsafe extern "C" fn(c_int) -> *const c_char;

#[cfg(target_os = "linux")]
const PA_STREAM_RECORD: c_int = 2;

#[cfg(target_os = "linux")]
const PA_SAMPLE_S16NE: c_int = if cfg!(target_endian = "little") { 3 } else { 4 };

/// Build a cpal input stream that forwards mono f32 samples at the device's
/// native sample rate. Conversion to the upload format happens after capture,
/// outside the realtime callback.
fn build_input_stream(device: &cpal::Device, tx: AudioSender) -> Result<(cpal::Stream, u32)> {
    let supported = device
        .default_input_config()
        .context("query microphone input format")?;
    let config = supported.config();
    let sample_format = supported.sample_format();
    let channels = config.channels.max(1) as usize;
    let sample_rate = config.sample_rate.0;
    eprintln!(
        "voice capture: device={:?}, rate={sample_rate} Hz, channels={channels}, format={sample_format:?}",
        device.name()
    );
    let make_err_fn = || {
        let tx = tx.clone();
        move |err: cpal::StreamError| {
            tx.try_send(Err(err.to_string()));
        }
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            let err_fn = make_err_fn();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    tx.try_send(Ok(downmix(data.iter().copied(), channels)));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::I16 => {
            let err_fn = make_err_fn();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let samples = data.iter().map(|&sample| sample as f32 / i16::MAX as f32);
                    tx.try_send(Ok(downmix(samples, channels)));
                },
                err_fn,
                None,
            )
        }
        SampleFormat::U16 => {
            let err_fn = make_err_fn();
            device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    let samples = data
                        .iter()
                        .map(|&sample| (sample as f32 - 32768.0) / 32768.0);
                    tx.try_send(Ok(downmix(samples, channels)));
                },
                err_fn,
                None,
            )
        }
        other => bail!("unsupported microphone sample format: {other:?}"),
    }
    .context("open microphone input stream")?;
    Ok((stream, sample_rate))
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|version| {
            let version = version.to_ascii_lowercase();
            version.contains("microsoft") || version.contains("wsl")
        })
        .unwrap_or(false)
        || std::env::var_os("WSL_INTEROP").is_some()
}

fn capture_backend_error(message: impl AsRef<str>) {
    eprintln!("voice capture: {}", message.as_ref());
    if is_wsl() {
        eprintln!(
            "voice capture: WSL detected; PULSE_SERVER={} and the WSLg Pulse server must be reachable for microphone input",
            if std::env::var_os("PULSE_SERVER").is_some() {
                "set"
            } else {
                "unset"
            }
        );
    }
}

fn build_audio_source(tx: AudioSender) -> Result<(InputSource, u32)> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("PULSE_SERVER").is_some() {
        let source = build_pulse_input(tx).map_err(|error| {
            capture_backend_error(format!("PulseAudio backend unavailable: {error:#}"));
            error
        })?;
        return Ok((InputSource::Pulse { _input: source }, SAMPLE_RATE));
    }

    let host = cpal::default_host();
    eprintln!(
        "voice capture: backend=CPAL, host={:?}, WSL={}",
        host.id(),
        is_wsl()
    );
    let device = host.default_input_device().ok_or_else(|| {
        let message = if is_wsl() {
            "no microphone input device was found through the WSL audio backend; set PULSE_SERVER to the WSLg Pulse server"
        } else {
            "no microphone input device was found"
        };
        capture_backend_error(message);
        anyhow::anyhow!(message)
    })?;
    let (stream, sample_rate) = build_input_stream(&device, tx).map_err(|error| {
        capture_backend_error(format!("CPAL microphone stream failed: {error:#}"));
        error
    })?;
    Ok((InputSource::Cpal(stream), sample_rate))
}

#[cfg(target_os = "linux")]
fn build_pulse_input(tx: AudioSender) -> Result<PulseInput> {
    let server = std::env::var_os("PULSE_SERVER")
        .map(|value| CString::new(value.as_bytes()).context("PULSE_SERVER contains NUL"))
        .transpose()?;
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let reader_server = server.clone();
    let reader = thread::spawn(move || {
        if let Err(error) = pulse_capture_loop(reader_server.as_ref(), &reader_stop, &tx)
            && !reader_stop.load(Ordering::Relaxed)
        {
            tx.try_send(Err(format!("{error:#}")));
        }
    });

    // Opening the connection in the reader thread lets the main capture loop
    // remain responsive while a WSLg Pulse server is slow or unavailable.
    Ok(PulseInput {
        stop,
        _reader: reader,
    })
}

#[cfg(target_os = "linux")]
fn pulse_capture_loop(server: Option<&CString>, stop: &AtomicBool, tx: &AudioSender) -> Result<()> {
    let library =
        unsafe { Library::new("libpulse-simple.so.0") }.context("load libpulse-simple.so.0")?;
    let new: Symbol<'_, PulseSimpleNew> = unsafe {
        library
            .get(b"pa_simple_new")
            .context("load pa_simple_new")?
    };
    let read: Symbol<'_, PulseSimpleRead> = unsafe {
        library
            .get(b"pa_simple_read")
            .context("load pa_simple_read")?
    };
    let free: Symbol<'_, PulseSimpleFree> = unsafe {
        library
            .get(b"pa_simple_free")
            .context("load pa_simple_free")?
    };
    let strerror: Symbol<'_, PulseStrError> =
        unsafe { library.get(b"pa_strerror").context("load pa_strerror")? };
    let application = CString::new("mj-voice-worker").expect("static string has no NUL");
    let stream_name = CString::new("dictation").expect("static string has no NUL");
    let spec = PulseSampleSpec {
        format: PA_SAMPLE_S16NE,
        rate: SAMPLE_RATE,
        channels: 1,
    };
    eprintln!(
        "voice capture: backend=PulseAudio, server={}, device=default, rate={SAMPLE_RATE} Hz, channels=1, format=PCM16",
        server.map_or_else(|| "default".into(), |server| server.to_string_lossy())
    );
    let mut error_code = 0;
    let handle = unsafe {
        new(
            server.map_or(ptr::null(), |server| server.as_ptr()),
            application.as_ptr(),
            PA_STREAM_RECORD,
            ptr::null(),
            stream_name.as_ptr(),
            &spec,
            ptr::null(),
            ptr::null(),
            &mut error_code,
        )
    };
    if handle.is_null() {
        let description = unsafe { CStr::from_ptr(strerror(error_code)) }.to_string_lossy();
        bail!("pa_simple_new failed with PulseAudio error {error_code}: {description}");
    }

    let mut bytes = [0u8; 640]; // 20 ms of 16 kHz mono PCM16.
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let result = unsafe {
            read(
                handle,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut error_code,
            )
        };
        if result < 0 {
            unsafe { free(handle) };
            let description = unsafe { CStr::from_ptr(strerror(error_code)) }.to_string_lossy();
            bail!("pa_simple_read failed with PulseAudio error {error_code}: {description}");
        }
        let samples = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_ne_bytes([pair[0], pair[1]]) as f32 / i16::MAX as f32)
            .collect();
        tx.try_send(Ok(samples));
    }
    unsafe { free(handle) };
    Ok(())
}

fn downmix<I>(samples: I, channels: usize) -> Vec<f32>
where
    I: Iterator<Item = f32>,
{
    let channels = channels.max(1);
    let frames: Vec<f32> = samples.collect();
    frames
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Normalize an RMS value into the 0.0..=1.0 meter range used by the UI.
pub(super) fn normalized_level(rms: f32) -> f32 {
    (rms * 18.0).clamp(0.0, 1.0)
}

fn chunk_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|sample| sample * sample).sum();
    (sum / samples.len() as f32).sqrt()
}

fn capture_audio<G, H>(
    mut on_level: G,
    mut on_status: H,
    command_rx: mpsc::Receiver<StdinCommand>,
) -> Result<CaptureOutcome>
where
    G: FnMut(f32),
    H: FnMut(String),
{
    // An EOF that arrived before the microphone was opened is still a clean
    // cancellation. This keeps process shutdown independent of audio devices.
    let mut finished = match command_rx.try_recv() {
        Ok(StdinCommand::Cancel) | Err(mpsc::TryRecvError::Disconnected) => {
            return Ok(CaptureOutcome::Cancelled);
        }
        Ok(StdinCommand::InputError(message)) => bail!("stdin failed: {message}"),
        Ok(StdinCommand::Finish) => true,
        Err(mpsc::TryRecvError::Empty) => false,
    };

    let (audio_tx, audio_rx) = mpsc::sync_channel::<AudioMessage>(64);
    let audio_state = Arc::new(AudioQueueState::default());
    let audio_sender = AudioSender {
        tx: audio_tx,
        state: Arc::clone(&audio_state),
    };
    let (source, mic_sample_rate) = build_audio_source(audio_sender)?;
    if let InputSource::Cpal(stream) = &source {
        stream.play().map_err(|error| {
            capture_backend_error(format!("start microphone capture failed: {error}"));
            anyhow::anyhow!("start microphone capture: {error}")
        })?;
    }
    on_status("listening...".to_string());

    let started_at = Instant::now();
    let mut received_audio = false;
    let mut last_level_at = Instant::now() - LEVEL_EMIT_INTERVAL;
    let mut samples = Vec::new();

    while !finished {
        if audio_state.overflowed.load(Ordering::Relaxed) {
            capture_backend_error("microphone callback queue overflowed");
            bail!("microphone produced audio faster than the worker could process it");
        }
        if audio_state.disconnected.load(Ordering::Relaxed) {
            bail!("microphone capture callback lost its output channel");
        }
        if started_at.elapsed() >= DICTATION_TIMEOUT {
            break;
        }
        match audio_rx.recv_timeout(Duration::from_millis(30)) {
            Ok(Ok(chunk)) => {
                received_audio = true;
                if last_level_at.elapsed() >= LEVEL_EMIT_INTERVAL {
                    on_level(normalized_level(chunk_rms(&chunk)));
                    last_level_at = Instant::now();
                }
                if samples
                    .len()
                    .saturating_add(chunk.len())
                    .saturating_mul(std::mem::size_of::<f32>())
                    > MAX_CAPTURE_BYTES
                {
                    capture_backend_error("microphone capture exceeded its memory limit");
                    bail!(
                        "microphone capture exceeded the {} MiB memory limit",
                        MAX_CAPTURE_BYTES / (1024 * 1024)
                    );
                }
                samples.extend(chunk);
            }
            Ok(Err(message)) => {
                capture_backend_error(format!("microphone capture failed: {message}"));
                bail!("microphone capture failed: {message}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("microphone capture stopped unexpectedly")
            }
        }

        match command_rx.try_recv() {
            Ok(StdinCommand::Finish) => finished = true,
            Ok(StdinCommand::Cancel) | Err(mpsc::TryRecvError::Disconnected) => {
                drop(source);
                return Ok(CaptureOutcome::Cancelled);
            }
            Ok(StdinCommand::InputError(message)) => bail!("stdin failed: {message}"),
            Err(mpsc::TryRecvError::Empty) => {}
        }

        if !received_audio && started_at.elapsed() >= NO_AUDIO_TIMEOUT {
            bail!(
                "microphone delivered no audio within {} seconds; it may be muted, in use by another application, or the audio backend may be incompatible",
                NO_AUDIO_TIMEOUT.as_secs()
            );
        }
    }

    // Finish and EOF can arrive back-to-back when a supervisor is shutting
    // down. Drain that second command before turning captured bytes into an
    // upload, so EOF always wins the upload decision.
    match command_rx.try_recv() {
        Ok(StdinCommand::Cancel) | Err(mpsc::TryRecvError::Disconnected) => {
            drop(source);
            return Ok(CaptureOutcome::Cancelled);
        }
        Ok(StdinCommand::InputError(message)) => bail!("stdin failed: {message}"),
        Ok(StdinCommand::Finish) | Err(mpsc::TryRecvError::Empty) => {}
    }

    drop(source);
    anyhow::ensure!(
        !audio_state.overflowed.load(Ordering::Relaxed),
        "microphone capture queue overflowed; retry with less system load"
    );
    eprintln!(
        "voice capture: recorded {} samples at {mic_sample_rate} Hz ({:.2} seconds)",
        samples.len(),
        samples.len() as f64 / f64::from(mic_sample_rate)
    );
    if samples.is_empty() {
        bail!("microphone capture produced no audio");
    }
    Ok(CaptureOutcome::Ready(CapturedAudio {
        samples,
        sample_rate: mic_sample_rate,
        command_rx,
    }))
}

/// Resample mono capture data with linear interpolation. Dictation uploads
/// are normalized to the 16 kHz format accepted by the subscription endpoint.
pub(super) fn resample_linear(
    samples: &[f32],
    input_rate: u32,
    output_rate: u32,
) -> Result<Vec<f32>> {
    if samples.is_empty() {
        return Ok(samples.to_vec());
    }
    if input_rate == 0 || output_rate == 0 {
        bail!("audio sample rates must be non-zero");
    }
    if input_rate == output_rate {
        return Ok(samples.to_vec());
    }
    let output_len =
        ((samples.len() as u64 * output_rate as u64) / input_rate as u64).max(1) as usize;
    let scale = input_rate as f64 / output_rate as f64;
    Ok((0..output_len)
        .map(|index| {
            let position = index as f64 * scale;
            let left = position.floor() as usize;
            let fraction = (position - left as f64) as f32;
            let left_sample = samples[left.min(samples.len() - 1)];
            let right_sample = samples[(left + 1).min(samples.len() - 1)];
            left_sample + (right_sample - left_sample) * fraction
        })
        .collect())
}

/// Encode mono f32 samples as a PCM16 RIFF/WAVE file.
pub(super) fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .context("audio capture is too large for a WAV file")?;
    let data_len_u32 = u32::try_from(data_len).context("audio capture exceeds WAV size limit")?;
    let riff_len = 36u32
        .checked_add(data_len_u32)
        .context("audio capture exceeds RIFF size limit")?;
    let byte_rate = sample_rate
        .checked_mul(2)
        .context("sample rate is too large for a WAV file")?;

    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len_u32.to_le_bytes());
    for &sample in samples {
        let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(wav)
}

fn monitor_cancellation(command_rx: mpsc::Receiver<StdinCommand>, cancellation: CancellationToken) {
    thread::spawn(move || {
        while let Ok(command) = command_rx.recv() {
            match command {
                StdinCommand::Finish => {}
                StdinCommand::Cancel | StdinCommand::InputError(_) => {
                    cancellation.cancel();
                    return;
                }
            }
        }
        cancellation.cancel();
    });
}

fn transcribe<F>(
    auth_path: PathBuf,
    captured: CapturedAudio,
    mut on_status: F,
) -> Result<RunOutcome>
where
    F: FnMut(String),
{
    let cancellation = CancellationToken::new();
    monitor_cancellation(captured.command_rx, cancellation.clone());
    if cancellation.is_cancelled() {
        return Ok(RunOutcome::Cancelled);
    }

    let samples = resample_linear(&captured.samples, captured.sample_rate, SAMPLE_RATE)?;
    let wav = encode_wav(&samples, SAMPLE_RATE)?;
    eprintln!(
        "voice transcription: uploading {} bytes of mono PCM16 WAV at {SAMPLE_RATE} Hz",
        wav.len()
    );
    if cancellation.is_cancelled() {
        return Ok(RunOutcome::Cancelled);
    }

    on_status("transcribing...".to_string());
    let mut request = TranscribeRequest::new(Bytes::from(wav), "audio.wav", "audio/wav");
    request.timeout = TRANSCRIPTION_TIMEOUT;
    request.cancel = cancellation.clone();
    let client = CodexClient::with_auth_path(auth_path);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create transcription runtime")?;
    let response = runtime.block_on(client.transcribe(request));
    match response {
        Ok(_response) if cancellation.is_cancelled() => Ok(RunOutcome::Cancelled),
        Ok(response) => {
            let text = response.text.trim().to_string();
            if text.is_empty() {
                bail!("transcription returned no text");
            }
            Ok(RunOutcome::Transcribed(text))
        }
        Err(_error) if cancellation.is_cancelled() => Ok(RunOutcome::Cancelled),
        Err(error) => Err(error).context("transcribe captured audio"),
    }
}

pub(super) fn run<F, G, H>(
    _on_partial: F,
    on_level: G,
    mut on_status: H,
    auth_path: PathBuf,
    command_rx: mpsc::Receiver<StdinCommand>,
) -> Result<RunOutcome>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    if !auth_path.is_file() {
        bail!("Codex auth file does not exist: {}", auth_path.display());
    }
    let captured = match capture_audio(on_level, &mut on_status, command_rx)? {
        CaptureOutcome::Ready(captured) => captured,
        CaptureOutcome::Cancelled => return Ok(RunOutcome::Cancelled),
    };
    transcribe(auth_path, captured, on_status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_queue_reports_overload_without_blocking_the_audio_callback() {
        let (tx, rx) = mpsc::sync_channel(4);
        let state = Arc::new(AudioQueueState::default());
        let sender = AudioSender {
            tx,
            state: Arc::clone(&state),
        };
        for _ in 0..5 {
            sender.try_send(Ok(vec![0.25; 4096]));
        }
        assert!(state.overflowed.load(Ordering::Relaxed));
        assert_eq!(rx.try_recv().unwrap().unwrap(), vec![0.25; 4096]);
    }

    #[test]
    fn wav_preserves_audio_larger_than_a_pipe_buffer() {
        let samples = vec![0.5; 65_536];
        let wav = encode_wav(&samples, 16_000).unwrap();
        assert_eq!(wav.len(), 44 + samples.len() * 2);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 131_072);
        assert_eq!(&wav[wav.len() - 2..], &16384_i16.to_le_bytes());
    }

    #[test]
    fn downmix_averages_each_channel_frame() {
        assert_eq!(
            downmix([1.0, -1.0, 0.25, 0.75].into_iter(), 2),
            vec![0.0, 0.5]
        );
    }

    #[test]
    fn normalized_level_clamps_to_meter_range() {
        assert_eq!(normalized_level(0.0), 0.0);
        assert_eq!(normalized_level(1.0), 1.0);
        assert!(normalized_level(0.01) > 0.0);
        assert!(normalized_level(0.01) < 1.0);
    }

    #[test]
    fn resample_linear_changes_rate_and_preserves_endpoints() {
        let output = resample_linear(&[0.0, 1.0], 2, 4).expect("resample");
        assert_eq!(output.len(), 4);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.5);
        assert_eq!(output[2], 1.0);
        assert_eq!(output[3], 1.0);
    }

    #[test]
    fn resample_linear_rejects_zero_sample_rates_without_panicking() {
        assert!(resample_linear(&[0.25], 0, 16_000).is_err());
        assert!(resample_linear(&[0.25], 16_000, 0).is_err());
    }

    #[test]
    fn encode_wav_writes_mono_pcm16_header_and_samples() {
        let wav = encode_wav(&[-1.0, 0.0, 1.0], 16_000).expect("encode WAV");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 42);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes(wav[44..46].try_into().unwrap()), -32767);
        assert_eq!(i16::from_le_bytes(wav[46..48].try_into().unwrap()), 0);
        assert_eq!(i16::from_le_bytes(wav[48..50].try_into().unwrap()), 32767);
        assert_eq!(wav.len(), 50);
    }
}
