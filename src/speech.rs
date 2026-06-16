//! Prompt dictation support.
//!
//! Non-Android platforms use local microphone capture plus OpenAI's
//! transcription endpoint. Audio stays in memory as mono 16 kHz WAV chunks,
//! and completed utterances are inserted into the prompt as they are
//! transcribed.

use anyhow::Result;
#[cfg(target_os = "android")]
use anyhow::bail;

#[cfg(not(target_os = "android"))]
mod backend {
    use anyhow::{Context, Result, bail};
    use cpal::SampleFormat;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use reqwest::blocking::multipart::{Form, Part};
    use serde::Deserialize;
    use std::{
        io::Write,
        sync::mpsc,
        time::{Duration, Instant},
    };

    const DEFAULT_TRANSCRIPTION_MODEL: &str = "gpt-4o-transcribe";
    const TRANSCRIPTIONS_URL: &str = "https://api.openai.com/v1/audio/transcriptions";

    const DICTATION_TIMEOUT: Duration = Duration::from_secs(600);
    const DICTATION_SILENCE: Duration = Duration::from_secs(20);
    const END_OF_UTTERANCE_SILENCE: Duration = Duration::from_millis(900);
    const MIN_SPEECH_DURATION: Duration = Duration::from_millis(250);
    const LEVEL_EMIT_INTERVAL: Duration = Duration::from_millis(80);

    const SAMPLE_RATE: i32 = 16000;
    const SILENCE_RMS_THRESHOLD: f32 = 0.01;
    const PRE_SPEECH_SECONDS: usize = 1;

    #[derive(Debug, Deserialize)]
    struct JsonTranscription {
        text: String,
    }

    fn openai_api_key() -> Result<String> {
        std::env::var("OPENAI_API_KEY")
            .map(|key| key.trim().to_string())
            .ok()
            .filter(|key| !key.is_empty())
            .context("voice dictation requires OPENAI_API_KEY for OpenAI transcription")
    }

    fn transcription_model() -> String {
        std::env::var("MJ_VOICE_TRANSCRIPTION_MODEL")
            .ok()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| DEFAULT_TRANSCRIPTION_MODEL.to_string())
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

    pub(super) fn resample_linear(samples: &[f32], from_rate: i32, to_rate: i32) -> Vec<f32> {
        if samples.is_empty() || from_rate == to_rate {
            return samples.to_vec();
        }
        let ratio = to_rate as f64 / from_rate as f64;
        let output_len = ((samples.len() as f64) * ratio).ceil() as usize;
        let mut output = Vec::with_capacity(output_len);
        for index in 0..output_len {
            let source = index as f64 / ratio;
            let left = source.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let frac = (source - left as f64) as f32;
            output.push(samples[left] + (samples[right] - samples[left]) * frac);
        }
        output
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

    fn seconds_to_samples(seconds: usize) -> usize {
        seconds * SAMPLE_RATE as usize
    }

    fn duration_for_samples(samples: usize) -> Duration {
        Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64)
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

    pub(super) fn wav_bytes(samples: &[f32]) -> Result<Vec<u8>> {
        let data_len = samples
            .len()
            .checked_mul(2)
            .context("voice sample buffer is too large")?;
        let riff_len = 36usize
            .checked_add(data_len)
            .context("voice wav buffer is too large")?;
        let data_len = u32::try_from(data_len).context("voice wav data is too large")?;
        let riff_len = u32::try_from(riff_len).context("voice wav file is too large")?;

        let mut out = Vec::with_capacity(44 + samples.len() * 2);
        out.write_all(b"RIFF")?;
        out.write_all(&riff_len.to_le_bytes())?;
        out.write_all(b"WAVE")?;
        out.write_all(b"fmt ")?;
        out.write_all(&16u32.to_le_bytes())?;
        out.write_all(&1u16.to_le_bytes())?;
        out.write_all(&1u16.to_le_bytes())?;
        out.write_all(&(SAMPLE_RATE as u32).to_le_bytes())?;
        out.write_all(&((SAMPLE_RATE as u32) * 2).to_le_bytes())?;
        out.write_all(&2u16.to_le_bytes())?;
        out.write_all(&16u16.to_le_bytes())?;
        out.write_all(b"data")?;
        out.write_all(&data_len.to_le_bytes())?;

        for sample in samples {
            let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            out.write_all(&scaled.to_le_bytes())?;
        }
        Ok(out)
    }

    pub(super) fn transcribe_samples(
        client: &reqwest::blocking::Client,
        samples: &[f32],
    ) -> Result<String> {
        let api_key = openai_api_key()?;
        let model = transcription_model();
        let wav = wav_bytes(samples)?;
        let file = Part::bytes(wav)
            .file_name("dictation.wav")
            .mime_str("audio/wav")
            .context("set dictation wav MIME type")?;
        let form = Form::new()
            .text("model", model)
            .text("response_format", "json")
            .part("file", file);

        let response = client
            .post(TRANSCRIPTIONS_URL)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .context("send audio transcription request")?;
        let status = response.status();
        let body = response
            .text()
            .context("read audio transcription response")?;
        if !status.is_success() {
            bail!("audio transcription request failed ({status}): {body}");
        }
        let transcription: JsonTranscription =
            serde_json::from_str(&body).context("parse audio transcription response")?;
        Ok(transcription.text.trim().to_string())
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
        let _ = openai_api_key()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent("mjolnir-voice-transcription")
            .build()
            .context("build transcription client")?;

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("no microphone input device was found")?;
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>();
        let (stream, mic_sample_rate) = build_input_stream(&device, audio_tx)?;
        stream.play().context("start microphone capture")?;
        on_status(format!("listening with {}...", transcription_model()));

        let started_at = Instant::now();
        let mut last_activity_at = Instant::now();
        let mut last_level_at = Instant::now() - LEVEL_EMIT_INTERVAL;
        let mut speech_started_at: Option<Instant> = None;
        let mut last_speech_at: Option<Instant> = None;

        let mut buffer = Vec::<f32>::new();
        let mut finalized = Vec::<String>::new();
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

                    let samples = resample_linear(&samples, mic_sample_rate, SAMPLE_RATE);
                    let rms = chunk_rms(&samples);
                    buffer.extend_from_slice(&samples);

                    if rms >= SILENCE_RMS_THRESHOLD {
                        last_activity_at = Instant::now();
                        let now = Instant::now();
                        speech_started_at.get_or_insert(now);
                        last_speech_at = Some(now);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("microphone capture stopped unexpectedly")
                }
            }

            if speech_started_at.is_none() && buffer.len() > seconds_to_samples(PRE_SPEECH_SECONDS)
            {
                let keep_from = buffer.len() - seconds_to_samples(PRE_SPEECH_SECONDS);
                buffer.drain(..keep_from);
            }

            let has_minimum_speech =
                speech_started_at.is_some_and(|started| started.elapsed() >= MIN_SPEECH_DURATION);
            let utterance_closed = last_speech_at
                .is_some_and(|last_speech| last_speech.elapsed() >= END_OF_UTTERANCE_SILENCE);
            if has_minimum_speech && utterance_closed {
                on_status("transcribing...".to_string());
                let text = transcribe_samples(&client, &buffer)?;
                if !text.is_empty() {
                    finalized.push(text);
                    last_activity_at = Instant::now();
                }
                buffer.clear();
                speech_started_at = None;
                last_speech_at = None;
                on_status(format!("listening with {}...", transcription_model()));
            }

            let transcript = compose_transcript(&finalized, "");
            if !transcript.is_empty() && last_emitted.as_deref() != Some(transcript.as_str()) {
                on_partial(transcript.clone());
                last_emitted = Some(transcript);
            }
        }

        drop(stream);

        if !cancelled
            && speech_started_at.is_some()
            && duration_for_samples(buffer.len()) >= MIN_SPEECH_DURATION
        {
            on_status("transcribing...".to_string());
            let text = transcribe_samples(&client, &buffer)?;
            if !text.is_empty() {
                finalized.push(text);
            }
        }

        let text = compose_transcript(&finalized, "");
        if !cancelled && text.is_empty() {
            bail!("no speech was recognized");
        }
        Ok(text)
    }
}

/// Capture microphone audio and return the recognized transcript.
///
/// `on_partial` receives the cumulative transcript as utterances complete,
/// `on_level` receives normalized microphone levels for the input meter, and
/// `on_status` receives transient progress messages. Sending on `cancel_rx`
/// stops capture and returns whatever was recognized so far.
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
    #[test]
    fn compose_transcript_joins_finalized_and_interim() {
        let finalized = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(
            backend::compose_transcript(&finalized, "I am"),
            "hello world I am"
        );
        assert_eq!(backend::compose_transcript(&finalized, "  "), "hello world");
        assert_eq!(backend::compose_transcript(&[], ""), "");
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn normalized_level_clamps_meter_range() {
        assert_eq!(backend::normalized_level(0.0), 0.0);
        assert_eq!(backend::normalized_level(1.0), 1.0);
        assert!(backend::normalized_level(0.02) > 0.0);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn resample_linear_preserves_empty_and_same_rate_input() {
        assert_eq!(
            backend::resample_linear(&[], 48_000, 16_000),
            Vec::<f32>::new()
        );
        assert_eq!(
            backend::resample_linear(&[0.0, 0.5, 1.0], 16_000, 16_000),
            vec![0.0, 0.5, 1.0]
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn resample_linear_downsamples() {
        let samples = backend::resample_linear(&[0.0, 1.0, 0.0, 1.0], 4, 2);

        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[1], 0.0);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn wav_bytes_writes_mono_pcm_header() {
        let wav = backend::wav_bytes(&[0.0, 1.0]).expect("wav");

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 4);
    }

    #[test]
    fn dictation_error_message_preserves_voice_errors() {
        let err = anyhow::anyhow!("voice dictation is not supported on this platform");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation is not supported on this platform"
        );
        let err = anyhow::anyhow!("no speech was recognized");
        assert_eq!(dictation_error_message(&err), "no speech was recognized");
        let err = anyhow::anyhow!("voice dictation is not supported on Android");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation is not supported on Android"
        );
    }

    #[test]
    fn dictation_error_message_mentions_microphone() {
        let err = anyhow::anyhow!("no microphone input device was found");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation could not use the microphone: no microphone input device was found"
        );
    }

    #[test]
    fn dictation_error_message_wraps_backend_errors() {
        let err = anyhow::anyhow!("some backend exploded");
        assert_eq!(
            dictation_error_message(&err),
            "voice dictation failed: some backend exploded"
        );
    }
}
