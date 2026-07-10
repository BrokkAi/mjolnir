use anyhow::{Context, Result, bail};
use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sherpa_onnx::{
    LinearResampler, OfflineRecognizer, OfflineRecognizerConfig, VadModelConfig,
    VoiceActivityDetector,
};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const ASR_MODEL_BASE_URL: &str =
    "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/main";
const ASR_MODEL_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
const ASR_ENCODER: &str = "encoder.int8.onnx";
const ASR_DECODER: &str = "decoder.int8.onnx";
const ASR_JOINER: &str = "joiner.int8.onnx";
const ASR_TOKENS: &str = "tokens.txt";
const VAD_MODEL_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";
const VAD_MODEL_FILE: &str = "silero_vad.onnx";

pub(super) const DICTATION_TIMEOUT: Duration = Duration::from_secs(600);
const DICTATION_SILENCE: Duration = Duration::from_secs(20);
/// cpal delivers callbacks continuously (silence arrives as zeros), so a
/// stream that produces no frames at all is broken, not quiet.
const NO_AUDIO_TIMEOUT: Duration = Duration::from_secs(5);

const SAMPLE_RATE: i32 = 16000;
const VAD_WINDOW_SIZE: usize = 512;
const INTERIM_DECODE_INTERVAL: Duration = Duration::from_millis(250);
const LEVEL_EMIT_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ModelPaths {
    pub dir: PathBuf,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    pub vad: PathBuf,
}

impl ModelPaths {
    pub(super) fn in_cache(cache_root: PathBuf) -> Self {
        let voice = cache_root.join("voice");
        let dir = voice.join(ASR_MODEL_DIR);
        Self {
            encoder: dir.join(ASR_ENCODER),
            decoder: dir.join(ASR_DECODER),
            joiner: dir.join(ASR_JOINER),
            tokens: dir.join(ASR_TOKENS),
            vad: voice.join(VAD_MODEL_FILE),
            dir,
        }
    }

    pub(super) fn is_installed(&self) -> bool {
        has_model_data(&self.encoder)
            && has_model_data(&self.decoder)
            && has_model_data(&self.joiner)
            && has_model_data(&self.tokens)
            && has_model_data(&self.vad)
    }
}

/// A zero-byte model file is as unusable as a missing one; an interrupted
/// download or extraction can leave either behind.
pub(super) fn has_model_data(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

fn mjolnir_cache_dir() -> Result<PathBuf> {
    dirs::cache_dir()
        .map(|dir| dir.join("mj"))
        .context("locate user cache directory")
}

pub(super) fn model_paths() -> Result<ModelPaths> {
    Ok(ModelPaths::in_cache(mjolnir_cache_dir()?))
}

/// Stream a URL to `dest`, reporting (downloaded, total) byte counts.
///
/// reqwest::blocking creates its own Tokio runtime, which panics when
/// called from within an existing Tokio context. Downloading on a plain OS
/// thread guarantees there is no ambient runtime, regardless of call site;
/// progress flows back over a channel so the callback need not be Send.
fn download_to_file<F>(url: &str, dest: &Path, mut on_progress: F) -> Result<()>
where
    F: FnMut(u64, Option<u64>),
{
    let url = url.to_string();
    let dest = dest.to_path_buf();
    let (progress_tx, progress_rx) = mpsc::channel::<(u64, Option<u64>)>();
    let worker = thread::spawn(move || -> Result<()> {
        let mut response = reqwest::blocking::Client::builder()
            .user_agent("mjolnir-voice-setup")
            .build()
            .context("build download client")?
            .get(&url)
            .send()
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("download {url}"))?;
        let total = response.content_length();
        let mut file =
            fs::File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        let mut buffer = [0u8; 64 * 1024];
        let mut downloaded = 0u64;
        loop {
            let read = response.read(&mut buffer).context("read download body")?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])
                .with_context(|| format!("write {}", dest.display()))?;
            downloaded += read as u64;
            let _ = progress_tx.send((downloaded, total));
        }
        Ok(())
    });
    for (downloaded, total) in progress_rx {
        on_progress(downloaded, total);
    }
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("download thread panicked"))?
}

fn megabytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn asr_model_url(file_name: &str) -> String {
    format!("{ASR_MODEL_BASE_URL}/{file_name}")
}

fn download_model_file<H>(file_name: &str, dest: &Path, on_status: &mut H) -> Result<()>
where
    H: FnMut(String),
{
    let tmp = dest.with_extension("part");
    let _ = fs::remove_file(&tmp);
    let mut last_percent = u64::MAX;
    if let Err(error) = download_to_file(&asr_model_url(file_name), &tmp, |downloaded, total| {
        let message = match total {
            Some(total) if total > 0 => {
                let percent = downloaded * 100 / total;
                if percent == last_percent {
                    return;
                }
                last_percent = percent;
                format!(
                    "downloading voice model: {file_name} {percent}% of {} MB",
                    megabytes(total)
                )
            }
            _ => format!(
                "downloading voice model: {file_name} {} MB",
                megabytes(downloaded)
            ),
        };
        on_status(message);
    }) {
        let _ = fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("download {file_name}"));
    }
    fs::rename(&tmp, dest).with_context(|| format!("install {}", dest.display()))
}

pub(super) fn ensure_models_installed<H>(paths: &ModelPaths, on_status: &mut H) -> Result<()>
where
    H: FnMut(String),
{
    if paths.is_installed() {
        return Ok(());
    }

    let voice_dir = paths
        .dir
        .parent()
        .context("resolve voice model cache parent")?;
    fs::create_dir_all(voice_dir).with_context(|| format!("create {}", voice_dir.display()))?;

    if !has_model_data(&paths.vad) {
        on_status("downloading voice activity model...".to_string());
        let tmp = paths.vad.with_extension("onnx.part");
        if let Err(error) = download_to_file(VAD_MODEL_URL, &tmp, |_, _| {}) {
            let _ = fs::remove_file(&tmp);
            return Err(error).context("download silero VAD model");
        }
        fs::rename(&tmp, &paths.vad).with_context(|| format!("install {}", paths.vad.display()))?;
    }

    if !(has_model_data(&paths.encoder)
        && has_model_data(&paths.decoder)
        && has_model_data(&paths.joiner)
        && has_model_data(&paths.tokens))
    {
        let archive = voice_dir.join(format!("{ASR_MODEL_DIR}.tar.bz2.part"));
        let staging = voice_dir.join(format!("{ASR_MODEL_DIR}.extracting"));
        if archive.exists() || staging.exists() {
            on_status("clearing incomplete voice model archive setup...".to_string());
            let _ = fs::remove_file(&archive);
            let _ = fs::remove_dir_all(&staging);
        }
        fs::create_dir_all(&paths.dir)
            .with_context(|| format!("create {}", paths.dir.display()))?;
        if !has_model_data(&paths.encoder) {
            download_model_file(ASR_ENCODER, &paths.encoder, on_status)?;
        }
        if !has_model_data(&paths.decoder) {
            download_model_file(ASR_DECODER, &paths.decoder, on_status)?;
        }
        if !has_model_data(&paths.joiner) {
            download_model_file(ASR_JOINER, &paths.joiner, on_status)?;
        }
        if !has_model_data(&paths.tokens) {
            download_model_file(ASR_TOKENS, &paths.tokens, on_status)?;
        }
    }

    if !paths.is_installed() {
        bail!(
            "voice model installation under {} is incomplete; delete the directory and retry",
            paths.dir.display()
        );
    }
    Ok(())
}

pub(super) fn create_vad(paths: &ModelPaths) -> Result<VoiceActivityDetector> {
    let mut config = VadModelConfig::default();
    config.silero_vad.model = Some(paths.vad.display().to_string());
    config.silero_vad.threshold = 0.5;
    config.silero_vad.min_silence_duration = 0.25;
    config.silero_vad.min_speech_duration = 0.25;
    config.silero_vad.max_speech_duration = 5.0;
    config.silero_vad.window_size = VAD_WINDOW_SIZE as i32;
    config.sample_rate = SAMPLE_RATE;
    VoiceActivityDetector::create(&config, 60.0).context("create voice activity detector")
}

pub(super) fn create_recognizer(paths: &ModelPaths) -> Result<OfflineRecognizer> {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(paths.encoder.display().to_string());
    config.model_config.transducer.decoder = Some(paths.decoder.display().to_string());
    config.model_config.transducer.joiner = Some(paths.joiner.display().to_string());
    config.model_config.tokens = Some(paths.tokens.display().to_string());
    config.model_config.model_type = Some("nemo_transducer".to_string());
    config.model_config.num_threads = decode_threads();
    OfflineRecognizer::create(&config).context("load voice recognition model")
}

fn decode_threads() -> i32 {
    thread::available_parallelism()
        .map(|n| n.get().min(4) as i32)
        .unwrap_or(2)
}

/// Samples from the capture callback, or the stream error that ended it.
/// cpal reports stream failures through a separate callback; carrying them
/// on the same channel lets the capture loop fail loudly instead of
/// waiting out the silence timeout with a dead microphone.
type AudioMessage = std::result::Result<Vec<f32>, String>;

/// Build a cpal input stream that forwards mono f32 samples at the
/// device's native rate.
fn build_input_stream(
    device: &cpal::Device,
    tx: mpsc::Sender<AudioMessage>,
) -> Result<(cpal::Stream, i32)> {
    let supported = device
        .default_input_config()
        .context("query microphone input format")?;
    let config = supported.config();
    let sample_format = supported.sample_format();
    let channels = config.channels.max(1) as usize;
    let sample_rate = config.sample_rate.0 as i32;
    let make_err_fn = || {
        let tx = tx.clone();
        move |err: cpal::StreamError| {
            let _ = tx.send(Err(err.to_string()));
        }
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            let err_fn = make_err_fn();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    let _ = tx.send(Ok(downmix(data.iter().copied(), channels)));
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
                    let samples = data.iter().map(|&s| s as f32 / i16::MAX as f32);
                    let _ = tx.send(Ok(downmix(samples, channels)));
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
                    let samples = data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0);
                    let _ = tx.send(Ok(downmix(samples, channels)));
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

fn downmix<I>(samples: I, channels: usize) -> Vec<f32>
where
    I: Iterator<Item = f32>,
{
    let frames: Vec<f32> = samples.collect();
    frames
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Normalize a raw RMS value into the 0.0..=1.0 meter range used by the UI.
pub(super) fn normalized_level(rms: f32) -> f32 {
    (rms * 18.0).clamp(0.0, 1.0)
}

fn chunk_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Join finalized utterances and the in-progress interim transcript.
pub(super) fn compose_transcript(finalized: &[String], interim: &str) -> String {
    let mut parts: Vec<&str> = finalized
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let interim = interim.trim();
    if !interim.is_empty() {
        parts.push(interim);
    }
    parts.join(" ")
}

pub(super) fn run<F, G, H>(
    mut on_partial: F,
    mut on_level: G,
    mut on_status: H,
    cancel_rx: mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    let paths = model_paths()?;
    ensure_models_installed(&paths, &mut on_status)?;

    on_status("loading voice model...".to_string());
    let load_hint = || {
        format!(
            "load voice models (if this keeps failing, delete {} and retry)",
            paths.dir.display()
        )
    };
    let vad = create_vad(&paths).with_context(load_hint)?;
    let recognizer = create_recognizer(&paths).with_context(load_hint)?;

    // Model loading takes a moment; honor a cancellation that arrived in
    // the meantime without ever opening the microphone.
    if cancel_rx.try_recv().is_ok() {
        return Ok(String::new());
    }

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no microphone input device was found")?;
    let (audio_tx, audio_rx) = mpsc::channel::<AudioMessage>();
    let (stream, mic_sample_rate) = build_input_stream(&device, audio_tx)?;
    let resampler = if mic_sample_rate != SAMPLE_RATE {
        Some(
            LinearResampler::create(mic_sample_rate, SAMPLE_RATE)
                .context("create microphone resampler")?,
        )
    } else {
        None
    };
    stream.play().context("start microphone capture")?;
    on_status("listening...".to_string());

    let started_at = Instant::now();
    let mut received_audio = false;
    let mut last_activity_at = Instant::now();
    let mut last_level_at = Instant::now() - LEVEL_EMIT_INTERVAL;
    let mut last_interim_decode_at = Instant::now();

    let mut buffer = Vec::<f32>::new();
    let mut vad_offset = 0usize;
    let mut speech_started = false;

    let mut finalized = Vec::<String>::new();
    let mut interim = String::new();
    let mut last_emitted: Option<String> = None;
    let mut cancelled = false;

    loop {
        if cancel_rx.try_recv().is_ok() {
            cancelled = true;
            break;
        }
        if started_at.elapsed() >= DICTATION_TIMEOUT {
            break;
        }
        if last_activity_at.elapsed() >= DICTATION_SILENCE {
            break;
        }

        match audio_rx.recv_timeout(Duration::from_millis(30)) {
            Ok(Ok(samples)) => {
                received_audio = true;
                if last_level_at.elapsed() >= LEVEL_EMIT_INTERVAL {
                    on_level(normalized_level(chunk_rms(&samples)));
                    last_level_at = Instant::now();
                }
                match &resampler {
                    Some(resampler) => {
                        buffer.extend_from_slice(&resampler.resample(&samples, false))
                    }
                    None => buffer.extend_from_slice(&samples),
                }
            }
            Ok(Err(message)) => bail!("microphone capture failed: {message}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("microphone capture stopped unexpectedly")
            }
        }

        if !received_audio && started_at.elapsed() >= NO_AUDIO_TIMEOUT {
            bail!(
                "microphone delivered no audio within {} seconds; it may be muted, in use \
                 by another application, or the audio backend may be incompatible",
                NO_AUDIO_TIMEOUT.as_secs()
            );
        }

        while vad_offset + VAD_WINDOW_SIZE <= buffer.len() {
            vad.accept_waveform(&buffer[vad_offset..vad_offset + VAD_WINDOW_SIZE]);
            if vad.detected() {
                last_activity_at = Instant::now();
                if !speech_started {
                    speech_started = true;
                    last_interim_decode_at = Instant::now();
                }
            }
            vad_offset += VAD_WINDOW_SIZE;
        }

        if !speech_started && buffer.len() > 10 * VAD_WINDOW_SIZE {
            let keep_from = buffer.len() - 10 * VAD_WINDOW_SIZE;
            buffer.drain(..keep_from);
            vad_offset = vad_offset.saturating_sub(keep_from);
        }

        if speech_started && last_interim_decode_at.elapsed() >= INTERIM_DECODE_INTERVAL {
            interim = decode_segment(&recognizer, SAMPLE_RATE, &buffer);
            last_interim_decode_at = Instant::now();
        }

        while !vad.is_empty() {
            if let Some(segment) = vad.front() {
                let text = decode_segment(&recognizer, SAMPLE_RATE, segment.samples());
                if !text.is_empty() {
                    finalized.push(text);
                    last_activity_at = Instant::now();
                }
            }
            vad.pop();
            buffer.clear();
            vad_offset = 0;
            speech_started = false;
            interim.clear();
        }

        let transcript = compose_transcript(&finalized, &interim);
        if !transcript.is_empty() && last_emitted.as_deref() != Some(transcript.as_str()) {
            on_partial(transcript.clone());
            last_emitted = Some(transcript);
        }
    }

    drop(stream);

    if !cancelled {
        vad.flush();
        while !vad.is_empty() {
            if let Some(segment) = vad.front() {
                let text = decode_segment(&recognizer, SAMPLE_RATE, segment.samples());
                if !text.is_empty() {
                    finalized.push(text);
                    interim.clear();
                }
            }
            vad.pop();
        }
    }

    let text = compose_transcript(&finalized, &interim);
    if !cancelled && text.is_empty() {
        bail!("no speech was recognized");
    }
    Ok(text)
}

pub(super) fn decode_segment(
    recognizer: &OfflineRecognizer,
    sample_rate: i32,
    samples: &[f32],
) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let stream = recognizer.create_stream();
    stream.accept_waveform(sample_rate, samples);
    recognizer.decode(&stream);
    stream
        .get_result()
        .map(|result| result.text.trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "downloads ~0.7 GB of models; run with: cargo test -p brokk-mj-voice-worker -- --ignored"]
    fn models_install_and_decode_test_wav() {
        let paths = model_paths().expect("resolve model paths");
        ensure_models_installed(&paths, &mut |status| eprintln!("{status}"))
            .expect("install voice models");
        create_vad(&paths).expect("load VAD model");
        let recognizer = create_recognizer(&paths).expect("load recognizer");

        let wav = std::fs::read_dir(paths.dir.join("test_wavs"))
            .expect("list test_wavs")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "wav"))
            .expect("find a test wav");
        let wave =
            sherpa_onnx::Wave::read(wav.to_str().expect("utf-8 wav path")).expect("read test wav");
        let text = decode_segment(&recognizer, wave.sample_rate(), wave.samples());

        assert!(!text.is_empty(), "expected a non-empty transcript");
    }

    #[test]
    fn model_paths_are_under_voice_cache() {
        let paths = ModelPaths::in_cache(PathBuf::from("/cache/mj"));
        let voice = PathBuf::from("/cache/mj").join("voice");
        assert_eq!(
            paths.dir,
            voice.join("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8")
        );
        assert_eq!(paths.encoder, paths.dir.join("encoder.int8.onnx"));
        assert_eq!(paths.decoder, paths.dir.join("decoder.int8.onnx"));
        assert_eq!(paths.joiner, paths.dir.join("joiner.int8.onnx"));
        assert_eq!(paths.tokens, paths.dir.join("tokens.txt"));
        assert_eq!(paths.vad, voice.join("silero_vad.onnx"));
    }

    #[test]
    fn compose_transcript_joins_finalized_and_interim() {
        let finalized = vec!["Hello there.".to_string(), "How are you?".to_string()];
        assert_eq!(
            compose_transcript(&finalized, "I am"),
            "Hello there. How are you? I am"
        );
        assert_eq!(
            compose_transcript(&finalized, "  "),
            "Hello there. How are you?"
        );
        assert_eq!(compose_transcript(&[], ""), "");
    }

    #[test]
    fn normalized_level_clamps_to_meter_range() {
        assert_eq!(normalized_level(0.0), 0.0);
        assert_eq!(normalized_level(1.0), 1.0);
        assert!(normalized_level(0.01) > 0.0);
        assert!(normalized_level(0.01) < 1.0);
    }

    #[test]
    fn zero_byte_model_files_are_not_installed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = ModelPaths::in_cache(tmp.path().to_path_buf());
        std::fs::create_dir_all(&paths.dir).expect("create model dir");
        let files = [
            &paths.encoder,
            &paths.decoder,
            &paths.joiner,
            &paths.tokens,
            &paths.vad,
        ];
        for path in files {
            std::fs::write(path, b"").expect("write empty file");
        }
        assert!(!paths.is_installed());
        for path in files {
            std::fs::write(path, b"model data").expect("write file");
        }
        assert!(paths.is_installed());
    }
}
