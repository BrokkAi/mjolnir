//! Microphone capture and speech-to-text integration for prompt dictation.
//!
//! Runs on a background task so audio capture and HTTP transcription never
//! block the ratatui event loop.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use reqwest::multipart::{Form, Part};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::event::UiEvent;

const DEFAULT_VOICE_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_VOICE_MODEL: &str = "gpt-4o-mini-transcribe";

#[derive(Debug)]
pub enum VoiceCommand {
    ToggleRecording,
    Shutdown,
}

#[derive(Debug)]
struct VoiceConfig {
    api_key: String,
    api_base: String,
    model: String,
}

impl VoiceConfig {
    fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("set OPENAI_API_KEY to enable voice transcription")?;
        let api_base = std::env::var("MJ_VOICE_API_BASE")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| DEFAULT_VOICE_API_BASE.to_string());
        let model =
            std::env::var("MJ_VOICE_MODEL").unwrap_or_else(|_| DEFAULT_VOICE_MODEL.to_string());
        Ok(Self {
            api_key,
            api_base: normalize_api_base(&api_base),
            model,
        })
    }
}

struct ActiveRecording {
    stream: cpal::Stream,
    writer: SharedWriter,
    path: PathBuf,
}

type SharedWriter = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;

struct VoiceRuntime {
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    runtime_handle: Handle,
    recording: Option<ActiveRecording>,
    transcribing: bool,
}

impl VoiceRuntime {
    fn new(ui_tx: mpsc::UnboundedSender<UiEvent>, runtime_handle: Handle) -> Self {
        Self {
            ui_tx,
            runtime_handle,
            recording: None,
            transcribing: false,
        }
    }

    fn handle_toggle(&mut self) {
        if self.transcribing {
            let _ = self.ui_tx.send(UiEvent::Warning(
                "voice transcription already in progress".to_string(),
            ));
            return;
        }

        if self.recording.is_some() {
            if let Err(err) = self.stop_and_transcribe() {
                let _ = self.ui_tx.send(UiEvent::VoiceTranscriptionFailed {
                    message: format!("voice transcription failed: {err:#}"),
                });
            }
            return;
        }

        if let Err(err) = self.start_recording() {
            let _ = self.ui_tx.send(UiEvent::VoiceTranscriptionFailed {
                message: format!("voice recording failed: {err:#}"),
            });
        }
    }

    fn start_recording(&mut self) -> Result<()> {
        let _ = VoiceConfig::from_env()?;
        let recording = start_recording(self.ui_tx.clone())?;
        self.recording = Some(recording);
        let _ = self.ui_tx.send(UiEvent::VoiceRecordingStarted);
        Ok(())
    }

    fn stop_and_transcribe(&mut self) -> Result<()> {
        let cfg = VoiceConfig::from_env()?;
        let Some(recording) = self.recording.take() else {
            return Ok(());
        };

        let path = recording.path.clone();
        drop(recording.stream);
        finalize_writer(recording.writer)?;

        self.transcribing = true;
        let _ = self.ui_tx.send(UiEvent::VoiceTranscribing);

        let result = self.runtime_handle.block_on(transcribe_file(&cfg, &path));
        self.transcribing = false;
        let _ = std::fs::remove_file(&path);

        let text = result?.trim().to_string();
        if text.is_empty() {
            anyhow::bail!("transcription returned no text");
        }

        let _ = self.ui_tx.send(UiEvent::VoiceTranscriptionReady { text });
        Ok(())
    }

    fn cleanup(&mut self) {
        self.transcribing = false;
        if let Some(recording) = self.recording.take() {
            drop(recording.stream);
            let _ = finalize_writer(recording.writer);
            let _ = std::fs::remove_file(recording.path);
        }
    }
}

pub fn spawn(ui_tx: mpsc::UnboundedSender<UiEvent>) -> mpsc::UnboundedSender<VoiceCommand> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let runtime_handle = Handle::current();
    std::thread::spawn(move || {
        let mut runtime = VoiceRuntime::new(ui_tx, runtime_handle);
        while let Some(cmd) = cmd_rx.blocking_recv() {
            match cmd {
                VoiceCommand::ToggleRecording => runtime.handle_toggle(),
                VoiceCommand::Shutdown => break,
            }
        }
        runtime.cleanup();
    });
    cmd_tx
}

fn normalize_api_base(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/').to_string();
    match url::Url::parse(&trimmed) {
        Ok(url) if url.path().is_empty() || url.path() == "/" => format!("{trimmed}/v1"),
        _ => trimmed,
    }
}

fn start_recording(ui_tx: mpsc::UnboundedSender<UiEvent>) -> Result<ActiveRecording> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default microphone was found")?;
    let supported = device
        .default_input_config()
        .context("failed to read the default microphone format")?;
    let config: cpal::StreamConfig = supported.clone().into();
    let path = temp_voice_path();
    let writer = create_wav_writer(&path, &config)?;
    let err_tx = ui_tx.clone();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            let writer = writer.clone();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| write_f32_samples(&writer, data),
                move |err| {
                    let _ = err_tx.send(UiEvent::VoiceTranscriptionFailed {
                        message: format!("voice recording failed: microphone stream error: {err}"),
                    });
                },
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let writer = writer.clone();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| write_i16_samples(&writer, data),
                move |err| {
                    let _ = ui_tx.send(UiEvent::VoiceTranscriptionFailed {
                        message: format!("voice recording failed: microphone stream error: {err}"),
                    });
                },
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let writer = writer.clone();
            device.build_input_stream(
                &config,
                move |data: &[u16], _| write_u16_samples(&writer, data),
                move |err| {
                    let _ = ui_tx.send(UiEvent::VoiceTranscriptionFailed {
                        message: format!("voice recording failed: microphone stream error: {err}"),
                    });
                },
                None,
            )?
        }
        other => anyhow::bail!("unsupported microphone sample format: {other:?}"),
    };
    stream
        .play()
        .context("failed to start the microphone stream")?;
    Ok(ActiveRecording {
        stream,
        writer,
        path,
    })
}

fn create_wav_writer(path: &Path, config: &cpal::StreamConfig) -> Result<SharedWriter> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let spec = hound::WavSpec {
        channels: config.channels,
        sample_rate: config.sample_rate.0,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let writer = hound::WavWriter::new(BufWriter::new(file), spec)
        .context("create wav writer for voice recording")?;
    Ok(Arc::new(Mutex::new(Some(writer))))
}

fn finalize_writer(writer: SharedWriter) -> Result<()> {
    let mut guard = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("voice recording writer lock was poisoned"))?;
    if let Some(writer) = guard.take() {
        writer.finalize().context("finalize recorded audio")?;
    }
    Ok(())
}

fn write_f32_samples(writer: &SharedWriter, data: &[f32]) {
    write_samples(
        writer,
        data.iter().map(|sample| {
            let scaled = sample.clamp(-1.0, 1.0) * f32::from(i16::MAX);
            scaled.round() as i16
        }),
    );
}

fn write_i16_samples(writer: &SharedWriter, data: &[i16]) {
    write_samples(writer, data.iter().copied());
}

fn write_u16_samples(writer: &SharedWriter, data: &[u16]) {
    write_samples(
        writer,
        data.iter()
            .map(|sample| (*sample as i32 - i32::from(u16::MAX) / 2) as i16),
    );
}

fn write_samples<I>(writer: &SharedWriter, samples: I)
where
    I: IntoIterator<Item = i16>,
{
    let Ok(mut guard) = writer.lock() else {
        tracing::warn!("voice recording writer lock poisoned");
        return;
    };
    let Some(writer) = guard.as_mut() else {
        return;
    };
    for sample in samples {
        if let Err(err) = writer.write_sample(sample) {
            tracing::warn!("voice recording write failed: {err}");
            break;
        }
    }
}

async fn transcribe_file(cfg: &VoiceConfig, path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let file_part = Part::bytes(bytes)
        .file_name("voice.wav")
        .mime_str("audio/wav")
        .context("set voice upload mime type")?;
    let form = Form::new()
        .part("file", file_part)
        .text("model", cfg.model.clone())
        .text("response_format", "text");
    let endpoint = format!(
        "{}/audio/transcriptions",
        cfg.api_base.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build voice transcription client")?;
    let response = client
        .post(&endpoint)
        .bearer_auth(&cfg.api_key)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("POST {endpoint}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read transcription response body")?;
    if !status.is_success() {
        anyhow::bail!("transcription API returned HTTP {status}: {body}");
    }
    Ok(body)
}

fn temp_voice_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("mj-voice-{}-{nanos}.wav", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::normalize_api_base;

    #[test]
    fn normalize_api_base_adds_v1_for_bare_hosts() {
        assert_eq!(
            normalize_api_base("https://api.openai.com"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_api_base("https://api.openai.com/"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn normalize_api_base_preserves_versioned_paths() {
        assert_eq!(
            normalize_api_base("https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_api_base("https://example.com/custom/api"),
            "https://example.com/custom/api"
        );
    }
}
