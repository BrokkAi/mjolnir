//! Microphone capture for prompt dictation.
//!
//! Records a temporary WAV file on a background thread so microphone I/O never
//! blocks the ratatui event loop. When recording stops, `mj` prefers a local
//! platform transcription path when available. If that path is unavailable or
//! fails, `mj` falls back to an ACP audio prompt block only when the active
//! agent advertises audio prompt support.

use std::fs::File;
use std::io::BufWriter;
#[cfg(target_os = "macos")]
use std::os::raw::{c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;

use crate::event::{PromptAudio, UiEvent};

const RECORDED_AUDIO_MIME_TYPE: &str = "audio/wav";

#[derive(Debug)]
pub enum VoiceCommand {
    ToggleRecording { allow_audio_fallback: bool },
    Shutdown,
}

struct ActiveRecording {
    stream: cpal::Stream,
    writer: SharedWriter,
    path: PathBuf,
    allow_audio_fallback: bool,
}

type SharedWriter = Arc<Mutex<Option<hound::WavWriter<BufWriter<File>>>>>;

struct VoiceRuntime {
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    recording: Option<ActiveRecording>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum PreparedVoicePrompt {
    Text(String),
    Audio(PromptAudio),
}

impl VoiceRuntime {
    fn new(ui_tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            ui_tx,
            recording: None,
        }
    }

    fn handle_toggle(&mut self, allow_audio_fallback: bool) {
        if self.recording.is_some() {
            if let Err(err) = self.stop_and_prepare_prompt() {
                let _ = self.ui_tx.send(UiEvent::VoicePromptFailed {
                    message: format!("voice recording failed: {err:#}"),
                });
            }
            return;
        }

        if let Err(err) = self.start_recording(allow_audio_fallback) {
            let _ = self.ui_tx.send(UiEvent::VoicePromptFailed {
                message: format!("voice recording failed: {err:#}"),
            });
        }
    }

    fn start_recording(&mut self, allow_audio_fallback: bool) -> Result<()> {
        let recording = start_recording(self.ui_tx.clone(), allow_audio_fallback)?;
        self.recording = Some(recording);
        let _ = self.ui_tx.send(UiEvent::VoiceRecordingStarted);
        Ok(())
    }

    fn stop_and_prepare_prompt(&mut self) -> Result<()> {
        let Some(recording) = self.recording.take() else {
            return Ok(());
        };

        let path = recording.path.clone();
        let allow_audio_fallback = recording.allow_audio_fallback;
        drop(recording.stream);
        finalize_writer(recording.writer)?;
        let _ = self.ui_tx.send(UiEvent::VoicePromptPreparing);

        match prepare_prompt(&path, allow_audio_fallback) {
            Ok(PreparedVoicePrompt::Text(text)) => {
                let _ = std::fs::remove_file(&path);
                let _ = self.ui_tx.send(UiEvent::VoiceTranscriptionReady { text });
                Ok(())
            }
            Ok(PreparedVoicePrompt::Audio(audio)) => {
                let _ = std::fs::remove_file(&path);
                let _ = self.ui_tx.send(UiEvent::VoicePromptReady { audio });
                Ok(())
            }
            Err(err) => {
                let _ = std::fs::remove_file(&path);
                Err(err)
            }
        }
    }

    fn cleanup(&mut self) {
        if let Some(recording) = self.recording.take() {
            drop(recording.stream);
            let _ = finalize_writer(recording.writer);
            let _ = std::fs::remove_file(recording.path);
        }
    }
}

pub const fn can_start_voice_input(prompt_audio_supported: bool) -> bool {
    cfg!(target_os = "macos") || prompt_audio_supported
}

pub fn spawn(ui_tx: mpsc::UnboundedSender<UiEvent>) -> mpsc::UnboundedSender<VoiceCommand> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut runtime = VoiceRuntime::new(ui_tx);
        while let Some(cmd) = cmd_rx.blocking_recv() {
            match cmd {
                VoiceCommand::ToggleRecording {
                    allow_audio_fallback,
                } => runtime.handle_toggle(allow_audio_fallback),
                VoiceCommand::Shutdown => break,
            }
        }
        runtime.cleanup();
    });
    cmd_tx
}

fn start_recording(
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    allow_audio_fallback: bool,
) -> Result<ActiveRecording> {
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
                    let _ = err_tx.send(UiEvent::VoicePromptFailed {
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
                    let _ = ui_tx.send(UiEvent::VoicePromptFailed {
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
                    let _ = ui_tx.send(UiEvent::VoicePromptFailed {
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
        allow_audio_fallback,
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

fn prepare_prompt(path: &Path, allow_audio_fallback: bool) -> Result<PreparedVoicePrompt> {
    #[cfg(target_os = "macos")]
    if let Some(text) = try_local_transcription(path)? {
        return Ok(PreparedVoicePrompt::Text(text));
    }

    if allow_audio_fallback {
        return Ok(PreparedVoicePrompt::Audio(read_recorded_audio(path)?));
    }

    anyhow::bail!(
        "local voice transcription is unavailable, and this agent does not advertise ACP audio prompt support"
    )
}

fn read_recorded_audio(path: &Path) -> Result<PromptAudio> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(PromptAudio {
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: RECORDED_AUDIO_MIME_TYPE.to_string(),
    })
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

fn temp_voice_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("mj-voice-{}-{nanos}.wav", std::process::id()))
}

#[cfg(target_os = "macos")]
fn try_local_transcription(path: &Path) -> Result<Option<String>> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .context("voice path contained an unexpected NUL byte")?;
    let raw = unsafe { mj_transcribe_wav_file(c_path.as_ptr()) };
    let text = unsafe { take_c_string(raw.text) };
    let message = unsafe { take_c_string(raw.message) };
    match raw.kind {
        0 => Ok(text.filter(|text| !text.trim().is_empty())),
        1 => {
            if let Some(message) = message {
                tracing::info!("local macOS speech transcription unavailable: {message}");
            }
            Ok(None)
        }
        _ => Err(anyhow::anyhow!(
            "{}",
            message.unwrap_or_else(|| "local macOS speech transcription failed".to_string())
        )),
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MjSpeechTranscriptionResult {
    kind: c_int,
    text: *mut c_char,
    message: *mut c_char,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mj_transcribe_wav_file(path: *const c_char) -> MjSpeechTranscriptionResult;
    fn mj_free_c_string(ptr: *mut c_char);
}

#[cfg(target_os = "macos")]
unsafe fn take_c_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let value = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { mj_free_c_string(ptr) };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_recorded_audio_encodes_wav_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("voice.wav");
        std::fs::write(&path, b"wav-data").expect("write wav");

        let audio = read_recorded_audio(&path).expect("read audio");

        assert_eq!(audio.mime_type, RECORDED_AUDIO_MIME_TYPE);
        assert_eq!(audio.data_base64, "d2F2LWRhdGE=");
    }

    #[test]
    fn can_start_voice_input_accepts_audio_support_anywhere() {
        assert!(can_start_voice_input(true));
    }
}
