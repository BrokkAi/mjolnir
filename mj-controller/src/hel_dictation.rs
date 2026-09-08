//! Shared subscription-backed web and native prompt dictation support.
//!
//! The controller owns credential discovery because both the terminal chat and
//! the authenticated web server need to make the same decision about which
//! Codex profile may transcribe audio. HTTP handlers send typed requests here;
//! filesystem access and the provider call stay off the controller event loop.

use std::path::PathBuf;
use std::time::Duration;

use anvil_client::codex_auth::read_auth_dot_json_at;
use anvil_client::codex_client::CodexClient;
use anvil_client::transcribe::TranscribeRequest;
use axum::body::Bytes;
use hel::hel_config::{HarnessKind, HelConfig};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Maximum complete WAV upload accepted by the web endpoint.
pub const MAX_AUDIO_BYTES: usize = 20 * 1024 * 1024;
/// Maximum audio duration accepted by the web endpoint.
pub const MAX_AUDIO_DURATION: Duration = Duration::from_secs(600);
/// Maximum audio bytes in 16 kHz mono PCM16. This is stricter than the whole
/// RIFF envelope limit and avoids accepting a file whose header claims a
/// shorter duration than its sample payload actually contains.
const MAX_PCM_DATA_BYTES: u64 = 16_000 * 2 * MAX_AUDIO_DURATION.as_secs();
/// Whole-request deadline for credential inspection and provider work.
pub const DICTATION_TIMEOUT: Duration = Duration::from_secs(120);

/// Find candidate Codex subscription auth files, preferring the session's
/// current profile and then sorting all remaining profile IDs.
pub fn auth_paths(config: &HelConfig, preferred: &str) -> Vec<PathBuf> {
    let mut profiles = config
        .profiles
        .iter()
        .filter(|(_, profile)| profile.kind == HarnessKind::Codex)
        .collect::<Vec<_>>();
    profiles.sort_by_key(|(id, _)| (*id != preferred, *id));
    profiles
        .into_iter()
        .map(|(_, profile)| profile.home.join("auth.json"))
        .collect()
}

/// Return the first auth file containing complete ChatGPT-subscription OAuth
/// tokens. API-key auth files are deliberately skipped.
pub fn available_auth(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths
        .into_iter()
        .find(|path| match read_auth_dot_json_at(path) {
            Ok(Some(auth)) => auth.tokens.is_some_and(|tokens| {
                !tokens.access_token.trim().is_empty()
                    && !tokens.refresh_token.trim().is_empty()
                    && !tokens.account_id.trim().is_empty()
            }),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "could not inspect Codex dictation credentials");
                false
            }
        })
}

/// An operation submitted by an authenticated HTTP surface.
#[derive(Debug)]
pub enum DictationOperation {
    /// Probe the selected session's profile set without contacting the provider.
    Availability,
    /// Validate and transcribe one complete WAV upload.
    Transcribe(Bytes),
}

/// A request crossing the HTTP/controller boundary.
#[derive(Debug)]
pub struct DictationRequest {
    pub session_id: String,
    pub operation: DictationOperation,
    pub cancel: CancellationToken,
    pub reply: oneshot::Sender<Result<DictationResponse, DictationError>>,
}

/// Result returned to an HTTP handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationResponse {
    Availability {
        available: bool,
        reason: Option<String>,
    },
    Transcript {
        text: String,
    },
}

/// Errors that cross the controller boundary. Provider details are retained
/// for logs and diagnostics, while the HTTP layer maps them to safe messages.
#[derive(Debug)]
pub enum DictationError {
    SessionNotFound,
    CredentialsUnavailable,
    InvalidAudio(&'static str),
    Cancelled,
    TimedOut,
    CredentialProbe,
    Provider(String),
}

impl std::fmt::Display for DictationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound => formatter.write_str("unknown session"),
            Self::CredentialsUnavailable => {
                formatter.write_str("no Codex subscription credentials are available")
            }
            Self::InvalidAudio(message) => write!(formatter, "invalid WAV audio: {message}"),
            Self::Cancelled => formatter.write_str("dictation was cancelled"),
            Self::TimedOut => formatter.write_str("dictation timed out"),
            Self::CredentialProbe => formatter.write_str("could not inspect dictation credentials"),
            Self::Provider(message) => {
                write!(formatter, "transcription provider failed: {message}")
            }
        }
    }
}

impl std::error::Error for DictationError {}

/// Validate a complete RIFF/WAVE file before any credentials or provider work
/// is started. Unknown chunks are allowed, but every chunk length and padding
/// byte must fit inside the declared RIFF envelope.
pub fn validate_wav(audio: &Bytes) -> Result<(), DictationError> {
    validate_wav_bytes(audio)
}

fn validate_wav_bytes(audio: &[u8]) -> Result<(), DictationError> {
    if audio.is_empty() {
        return Err(DictationError::InvalidAudio(
            "audio upload must not be empty",
        ));
    }
    if audio.len() > MAX_AUDIO_BYTES {
        return Err(DictationError::InvalidAudio("audio upload is too large"));
    }
    if audio.len() < 12 || &audio[..4] != b"RIFF" || &audio[8..12] != b"WAVE" {
        return Err(DictationError::InvalidAudio("expected a RIFF/WAVE file"));
    }
    let declared_size = u32::from_le_bytes(audio[4..8].try_into().unwrap()) as usize;
    if declared_size != audio.len().saturating_sub(8) {
        return Err(DictationError::InvalidAudio(
            "RIFF length does not match the upload",
        ));
    }

    let mut cursor = 12_usize;
    let mut fmt_seen = false;
    let mut data_bytes = 0_u64;
    let mut data_seen = false;
    while cursor < audio.len() {
        if audio.len() - cursor < 8 {
            return Err(DictationError::InvalidAudio("truncated WAV chunk header"));
        }
        let id = &audio[cursor..cursor + 4];
        let chunk_size = u32::from_le_bytes(
            audio[cursor + 4..cursor + 8]
                .try_into()
                .expect("WAV chunk size is four bytes"),
        ) as usize;
        let data_start = cursor + 8;
        let data_end = data_start
            .checked_add(chunk_size)
            .ok_or(DictationError::InvalidAudio("WAV chunk length overflows"))?;
        let padded_end = data_end
            .checked_add(chunk_size & 1)
            .ok_or(DictationError::InvalidAudio("WAV chunk padding overflows"))?;
        if data_end > audio.len() || padded_end > audio.len() {
            return Err(DictationError::InvalidAudio("truncated WAV chunk data"));
        }
        match id {
            b"fmt " => {
                if fmt_seen || chunk_size < 16 {
                    return Err(DictationError::InvalidAudio("invalid WAV format chunk"));
                }
                fmt_seen = true;
                let format =
                    u16::from_le_bytes(audio[data_start..data_start + 2].try_into().unwrap());
                let channels =
                    u16::from_le_bytes(audio[data_start + 2..data_start + 4].try_into().unwrap());
                let rate =
                    u32::from_le_bytes(audio[data_start + 4..data_start + 8].try_into().unwrap());
                let bytes_per_second =
                    u32::from_le_bytes(audio[data_start + 8..data_start + 12].try_into().unwrap());
                let alignment =
                    u16::from_le_bytes(audio[data_start + 12..data_start + 14].try_into().unwrap());
                let bits =
                    u16::from_le_bytes(audio[data_start + 14..data_start + 16].try_into().unwrap());
                if format != 1
                    || channels != 1
                    || rate != 16_000
                    || alignment != 2
                    || bits != 16
                    || bytes_per_second != 32_000
                {
                    return Err(DictationError::InvalidAudio(
                        "WAV must be mono 16-bit PCM at 16 kHz",
                    ));
                }
            }
            b"data" => {
                if data_seen || !chunk_size.is_multiple_of(2) {
                    return Err(DictationError::InvalidAudio(
                        "WAV PCM data is not one even-sized sample chunk",
                    ));
                }
                data_seen = true;
                data_bytes = data_bytes
                    .checked_add(chunk_size as u64)
                    .ok_or(DictationError::InvalidAudio("WAV sample length overflows"))?;
            }
            _ => {}
        }
        cursor = padded_end;
    }
    if !fmt_seen {
        return Err(DictationError::InvalidAudio("WAV format chunk is missing"));
    }
    if data_bytes == 0 {
        return Err(DictationError::InvalidAudio("WAV sample data is missing"));
    }
    if data_bytes > MAX_PCM_DATA_BYTES {
        return Err(DictationError::InvalidAudio(
            "audio duration exceeds 600 seconds",
        ));
    }
    Ok(())
}

/// Execute one request. This future is supervised by the controller's
/// `JoinSet`; no provider task is detached when the HTTP client disconnects or
/// the daemon shuts down.
pub async fn execute(
    request: DictationRequest,
    auth_paths: Option<Vec<PathBuf>>,
    shutdown: CancellationToken,
) {
    let DictationRequest {
        session_id: _,
        operation,
        cancel,
        mut reply,
    } = request;
    let result = tokio::select! {
        biased;
        _ = reply.closed() => return,
        _ = shutdown.cancelled() => Err(DictationError::Cancelled),
        _ = cancel.cancelled() => Err(DictationError::Cancelled),
        result = tokio::time::timeout(DICTATION_TIMEOUT, execute_operation(operation, auth_paths, cancel.clone())) => {
            result.unwrap_or(Err(DictationError::TimedOut))
        },
    };
    let _ = reply.send(result);
}

async fn execute_operation(
    operation: DictationOperation,
    auth_paths: Option<Vec<PathBuf>>,
    cancel: CancellationToken,
) -> Result<DictationResponse, DictationError> {
    let paths = auth_paths.ok_or(DictationError::SessionNotFound)?;
    let audio = match &operation {
        DictationOperation::Transcribe(audio) => Some(audio.clone()),
        DictationOperation::Availability => None,
    };
    let probe = tokio::task::spawn_blocking(move || {
        if let Some(audio) = audio {
            validate_wav(&audio)?;
        }
        Ok::<_, DictationError>(available_auth(paths))
    });
    let auth_path = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(DictationError::Cancelled),
        result = probe => result.map_err(|error| {
            tracing::warn!(%error, "Codex dictation credential probe task failed");
            DictationError::CredentialProbe
        })??,
    };

    match operation {
        DictationOperation::Availability => Ok(DictationResponse::Availability {
            available: auth_path.is_some(),
            reason: auth_path
                .is_none()
                .then(|| "no Codex subscription credentials are available".to_owned()),
        }),
        DictationOperation::Transcribe(audio) => {
            let auth_path = auth_path.ok_or(DictationError::CredentialsUnavailable)?;
            let mut request = TranscribeRequest::new(audio, "audio.wav", "audio/wav");
            request.cancel = cancel.clone();
            request.timeout = DICTATION_TIMEOUT;
            let transcription = CodexClient::with_auth_path(auth_path)
                .transcribe(request)
                .await
                .map_err(|error| {
                    let message = error.to_string();
                    if message.to_ascii_lowercase().contains("timed out") {
                        DictationError::TimedOut
                    } else if cancel.is_cancelled() {
                        DictationError::Cancelled
                    } else {
                        DictationError::Provider(message)
                    }
                })?;
            Ok(DictationResponse::Transcript {
                text: transcription.text,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn wav(sample_bytes: usize) -> Bytes {
        let padded = sample_bytes + (sample_bytes & 1);
        let riff_size = 36 + padded;
        let mut bytes = Vec::with_capacity(8 + riff_size);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(riff_size as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&32_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(sample_bytes as u32).to_le_bytes());
        bytes.resize(bytes.len() + sample_bytes, 0);
        if sample_bytes & 1 != 0 {
            bytes.push(0);
        }
        Bytes::from(bytes)
    }

    #[test]
    fn accepts_wav_with_unknown_padded_chunk() {
        let mut audio = wav(64).to_vec();
        audio.splice(12..12, b"JUNK\x01\x00\x00\x00x\x00".iter().copied());
        let size = audio.len() - 8;
        audio[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        assert!(validate_wav(&Bytes::from(audio)).is_ok());
    }

    #[test]
    fn rejects_truncated_and_wrong_format_audio() {
        assert!(matches!(
            validate_wav(&Bytes::from_static(b"RIFF")),
            Err(DictationError::InvalidAudio(_))
        ));
        let mut audio = wav(64).to_vec();
        audio[22..24].copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            validate_wav(&Bytes::from(audio)),
            Err(DictationError::InvalidAudio(_))
        ));
    }

    #[test]
    fn rejects_data_longer_than_six_hundred_seconds() {
        let audio = wav((MAX_PCM_DATA_BYTES + 2) as usize);
        assert!(matches!(
            validate_wav(&audio),
            Err(DictationError::InvalidAudio(_))
        ));
    }

    #[test]
    fn profile_order_prefers_current_then_sorts() {
        let mut config = HelConfig {
            profiles: BTreeMap::new(),
            ..HelConfig::default()
        };
        for id in ["z", "a", "m"] {
            config.profiles.insert(
                id.into(),
                hel::hel_config::HarnessProfile {
                    kind: HarnessKind::Codex,
                    home: PathBuf::from(id),
                    environment: Default::default(),
                    context_window_bytes: None,
                },
            );
        }
        let mut claude = config.profiles["m"].clone();
        claude.kind = HarnessKind::Claude;
        config.profiles.insert("claude".into(), claude);
        assert_eq!(auth_paths(&config, "claude").len(), 3);
        assert_eq!(
            auth_paths(&config, "m"),
            vec![
                PathBuf::from("m/auth.json"),
                PathBuf::from("a/auth.json"),
                PathBuf::from("z/auth.json")
            ]
        );
    }

    #[tokio::test]
    async fn availability_and_transcription_without_credentials_do_not_call_provider() {
        for operation in [
            DictationOperation::Availability,
            DictationOperation::Transcribe(wav(96_000)),
        ] {
            let availability = matches!(operation, DictationOperation::Availability);
            let (reply, answer) = oneshot::channel();
            execute(
                DictationRequest {
                    session_id: "session".into(),
                    operation,
                    cancel: CancellationToken::new(),
                    reply,
                },
                Some(vec![]),
                CancellationToken::new(),
            )
            .await;
            let result = answer.await.unwrap();
            if availability {
                assert!(matches!(
                    result,
                    Ok(DictationResponse::Availability {
                        available: false,
                        reason: Some(_)
                    })
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(DictationError::CredentialsUnavailable)
                ));
            }
        }
    }

    #[tokio::test]
    async fn shutdown_and_request_cancellation_preempt_credential_work() {
        for shutdown_cancelled in [false, true] {
            let cancel = CancellationToken::new();
            let shutdown = CancellationToken::new();
            if shutdown_cancelled {
                shutdown.cancel();
            } else {
                cancel.cancel();
            }
            let (reply, answer) = oneshot::channel();
            execute(
                DictationRequest {
                    session_id: "session".into(),
                    operation: DictationOperation::Availability,
                    cancel,
                    reply,
                },
                None,
                shutdown,
            )
            .await;
            assert!(matches!(
                answer.await.unwrap(),
                Err(DictationError::Cancelled)
            ));
        }
    }

    #[test]
    fn duplicate_or_odd_data_chunks_and_wrong_riff_lengths_are_rejected() {
        let mut duplicate = wav(2).to_vec();
        duplicate.extend_from_slice(b"data\x02\x00\x00\x00\x00\x00");
        let length = duplicate.len() as u32 - 8;
        duplicate[4..8].copy_from_slice(&length.to_le_bytes());
        assert!(validate_wav(&Bytes::from(duplicate)).is_err());
        assert!(validate_wav(&wav(3)).is_err());
        let mut wrong_length = wav(2).to_vec();
        wrong_length[4..8].copy_from_slice(&0_u32.to_le_bytes());
        assert!(validate_wav(&Bytes::from(wrong_length)).is_err());
        assert!(validate_wav(&wav(MAX_PCM_DATA_BYTES as usize)).is_ok());
    }

    #[test]
    fn available_auth_skips_api_keys_and_malformed_or_empty_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let api_key = directory.path().join("api-key.json");
        let malformed = directory.path().join("malformed.json");
        let oauth = directory.path().join("oauth.json");
        std::fs::write(&api_key, r#"{"OPENAI_API_KEY":"test"}"#).unwrap();
        std::fs::write(&malformed, "{").unwrap();
        assert_eq!(
            available_auth(vec![api_key.clone(), malformed.clone()]),
            None
        );
        std::fs::write(
            &oauth,
            r#"{"tokens":{"id_token":"id","access_token":"access","refresh_token":"refresh","account_id":"account"}}"#,
        )
        .unwrap();
        assert_eq!(
            available_auth(vec![api_key, malformed, oauth.clone()]),
            Some(oauth)
        );
    }
}
