//! Prompt dictation support.
//!
//! Non-Android platforms use an in-process, fully local pipeline built on
//! sherpa-onnx: cpal microphone capture, Silero VAD speech segmentation, and
//! the multilingual NeMo Parakeet TDT 0.6b v3 offline recognizer. Models are
//! downloaded once into the user cache and then reused without any API key or
//! network call during dictation.

use anyhow::Result;
#[cfg(target_os = "android")]
use anyhow::bail;

#[cfg(not(target_os = "android"))]
mod backend {
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
        path::{Component, Path, PathBuf},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    const ASR_MODEL_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2";
    const ASR_MODEL_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
    const ASR_ENCODER: &str = "encoder.int8.onnx";
    const ASR_DECODER: &str = "decoder.int8.onnx";
    const ASR_JOINER: &str = "joiner.int8.onnx";
    const ASR_TOKENS: &str = "tokens.txt";
    const VAD_MODEL_URL: &str =
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";
    const VAD_MODEL_FILE: &str = "silero_vad.onnx";

    const DICTATION_TIMEOUT: Duration = Duration::from_secs(600);
    const DICTATION_SILENCE: Duration = Duration::from_secs(20);

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
            self.encoder.is_file()
                && self.decoder.is_file()
                && self.joiner.is_file()
                && self.tokens.is_file()
                && self.vad.is_file()
        }
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

    fn extract_tar_bz2_file(archive: &Path, dest: &Path) -> Result<()> {
        let file =
            fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
        let decoder = bzip2::read::BzDecoder::new(file);
        let mut tar = tar::Archive::new(decoder);
        for entry in tar.entries().context("read tar.bz2 entries")? {
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
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            entry
                .unpack(&out_path)
                .with_context(|| format!("unpack {}", out_path.display()))?;
        }
        Ok(())
    }

    pub(super) fn safe_archive_path(path: &Path) -> Result<PathBuf> {
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

    fn megabytes(bytes: u64) -> u64 {
        bytes / (1024 * 1024)
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

        if !paths.vad.is_file() {
            on_status("downloading voice activity model...".to_string());
            let tmp = paths.vad.with_extension("onnx.part");
            download_to_file(VAD_MODEL_URL, &tmp, |_, _| {})
                .context("download silero VAD model")?;
            fs::rename(&tmp, &paths.vad)
                .with_context(|| format!("install {}", paths.vad.display()))?;
        }

        if !(paths.encoder.is_file()
            && paths.decoder.is_file()
            && paths.joiner.is_file()
            && paths.tokens.is_file())
        {
            let archive = voice_dir.join(format!("{ASR_MODEL_DIR}.tar.bz2.part"));
            let mut last_percent = u64::MAX;
            download_to_file(ASR_MODEL_URL, &archive, |downloaded, total| {
                let message = match total {
                    Some(total) if total > 0 => {
                        let percent = downloaded * 100 / total;
                        if percent == last_percent {
                            return;
                        }
                        last_percent = percent;
                        format!(
                            "downloading voice model (one-time): {percent}% of {} MB",
                            megabytes(total)
                        )
                    }
                    _ => format!(
                        "downloading voice model (one-time): {} MB",
                        megabytes(downloaded)
                    ),
                };
                on_status(message);
            })
            .context("download voice recognition model")?;
            on_status("unpacking voice model...".to_string());
            extract_tar_bz2_file(&archive, voice_dir).context("extract voice model")?;
            let _ = fs::remove_file(&archive);
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

    /// Build a cpal input stream that forwards mono f32 samples at the
    /// device's native rate.
    fn build_input_stream(
        device: &cpal::Device,
        tx: mpsc::Sender<Vec<f32>>,
    ) -> Result<(cpal::Stream, i32)> {
        let supported = device
            .default_input_config()
            .context("query microphone input format")?;
        let config = supported.config();
        let sample_format = supported.sample_format();
        let channels = config.channels.max(1) as usize;
        let sample_rate = config.sample_rate.0 as i32;
        let err_fn = |_err| {};

        let stream = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    let _ = tx.send(downmix(data.iter().copied(), channels));
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let samples = data.iter().map(|&s| s as f32 / i16::MAX as f32);
                    let _ = tx.send(downmix(samples, channels));
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    let samples = data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0);
                    let _ = tx.send(downmix(samples, channels));
                },
                err_fn,
                None,
            ),
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
        let vad = create_vad(&paths)?;
        let recognizer = create_recognizer(&paths)?;

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no microphone input device was found")?;
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>();
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
                Ok(samples) => {
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
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("microphone capture stopped unexpectedly")
                }
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
}

/// Capture microphone audio and return the recognized transcript.
///
/// `on_partial` receives the cumulative transcript as it grows, `on_level`
/// receives normalized microphone levels for the input meter, and `on_status`
/// receives transient progress messages (model download, loading). Sending on
/// `cancel_rx` stops capture and returns whatever was recognized so far.
#[cfg(not(target_os = "android"))]
pub fn run_dictation<F, G, H>(
    on_partial: F,
    on_level: G,
    on_status: H,
    cancel_rx: std::sync::mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    backend::run(on_partial, on_level, on_status, cancel_rx)
}

#[cfg(target_os = "android")]
pub fn run_dictation<F, G, H>(
    _on_partial: F,
    _on_level: G,
    _on_status: H,
    _cancel_rx: std::sync::mpsc::Receiver<()>,
) -> Result<String>
where
    F: FnMut(String),
    G: FnMut(f32),
    H: FnMut(String),
{
    bail!("voice dictation is not supported on Android")
}

pub fn dictation_error_message(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.starts_with("voice") || message.starts_with("no speech") {
        return message;
    }
    if message.contains("microphone") {
        return format!("voice dictation could not use the microphone: {message}");
    }
    format!("voice dictation failed: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "android"))]
    use std::path::PathBuf;

    /// End-to-end check of the local model pipeline: downloads the real models
    /// into the user cache, loads the recognizer and VAD, and decodes a test
    /// wav shipped with the model archive.
    #[cfg(not(target_os = "android"))]
    #[test]
    #[ignore = "downloads ~0.7 GB of models; run with: cargo test -- --ignored"]
    fn dictation_models_install_and_decode_test_wav() {
        let paths = backend::model_paths().expect("resolve model paths");
        backend::ensure_models_installed(&paths, &mut |status| eprintln!("{status}"))
            .expect("install voice models");

        backend::create_vad(&paths).expect("load VAD model");
        let recognizer = backend::create_recognizer(&paths).expect("load recognizer");

        let wav_dir = paths.dir.join("test_wavs");
        let wav = std::fs::read_dir(&wav_dir)
            .expect("list test_wavs")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "wav"))
            .expect("find a test wav");
        let wave =
            sherpa_onnx::Wave::read(wav.to_str().expect("utf-8 wav path")).expect("read test wav");

        let text = backend::decode_segment(&recognizer, wave.sample_rate(), wave.samples());
        eprintln!("decoded {}: {text}", wav.display());
        assert!(!text.is_empty(), "expected a non-empty transcript");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn model_paths_are_under_voice_cache() {
        let paths = backend::ModelPaths::in_cache(PathBuf::from("/cache/mj"));
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

    #[cfg(not(target_os = "android"))]
    #[test]
    fn compose_transcript_joins_finalized_and_interim() {
        let finalized = vec!["Hello there.".to_string(), "How are you?".to_string()];
        assert_eq!(
            backend::compose_transcript(&finalized, "I am"),
            "Hello there. How are you? I am"
        );
        assert_eq!(
            backend::compose_transcript(&finalized, "  "),
            "Hello there. How are you?"
        );
        assert_eq!(backend::compose_transcript(&[], ""), "");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn safe_archive_path_rejects_escapes() {
        assert!(backend::safe_archive_path(std::path::Path::new("a/b.onnx")).is_ok());
        assert!(backend::safe_archive_path(std::path::Path::new("../evil")).is_err());
        assert!(backend::safe_archive_path(std::path::Path::new("/abs/evil")).is_err());
        assert!(backend::safe_archive_path(std::path::Path::new("")).is_err());
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn normalized_level_clamps_to_meter_range() {
        assert_eq!(backend::normalized_level(0.0), 0.0);
        assert_eq!(backend::normalized_level(1.0), 1.0);
        assert!(backend::normalized_level(0.01) > 0.0);
        assert!(backend::normalized_level(0.01) < 1.0);
    }

    #[test]
    fn error_messages_are_prefixed_for_context() {
        let err = anyhow::anyhow!("some backend exploded");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation failed: some backend exploded"
        );
        let err = anyhow::anyhow!("no speech was recognized");
        assert_eq!(dictation_error_message(&err), "no speech was recognized");
        let err = anyhow::anyhow!("voice dictation is not supported on Android");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation is not supported on Android"
        );
    }
}
