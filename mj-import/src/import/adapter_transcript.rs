use std::sync::Arc;

use chrono::{DateTime, Utc};
use mj_core::relay::{RELAY_EVENT_GENESIS_DIGEST, strip_hidden_prompt_context};
use mj_core::state::{MaterializedExecutionState, MaterializedSession};
use mj_core::transcript::{TranscriptBody, TranscriptItem};
use sessionwiki::adapters::{Adapter, ClaudeCode, Codex, GrokBuild, KimiCode, Muse};
use sessionwiki::model::{Role, Session};
use sha2::{Digest, Sha256};

use super::*;

pub(super) fn import_adapter(harness: HarnessKind) -> Option<Box<dyn Adapter>> {
    Some(match harness {
        HarnessKind::Claude => Box::new(ClaudeCode::default()),
        HarnessKind::Codex => Box::new(Codex::default()),
        HarnessKind::Kimi => Box::new(KimiCode::default()),
        HarnessKind::Grok => Box::new(GrokBuild::default()),
        HarnessKind::Muse => Box::new(Muse::default()),
        HarnessKind::Antigravity | HarnessKind::Dsh | HarnessKind::OpenCode => return None,
    })
}

pub fn read_claude_transcript(path: &Path) -> Result<ClaudeTranscript> {
    read_projection(HarnessKind::Claude, path, path)
}

pub fn read_codex_transcript(path: &Path) -> Result<CodexTranscript> {
    if let Some(issue) = codex::codex_session_summary(path)?
        .history_mode
        .import_issue()
    {
        bail!("{issue}");
    }
    read_projection(HarnessKind::Codex, path, path)
}

pub fn read_kimi_transcript(path: &Path) -> Result<KimiTranscript> {
    let wire = path.join("agents/main/wire.jsonl");
    let source = if wire.is_file() {
        wire
    } else {
        path.join("wire.jsonl")
    };
    read_projection(HarnessKind::Kimi, path, &source)
}

pub fn read_grok_transcript(path: &Path) -> Result<GrokTranscript> {
    let history = path.join(CHAT_HISTORY);
    let source = if history.is_file() {
        history
    } else {
        path.join("history.jsonl")
    };
    read_projection(HarnessKind::Grok, path, &source)
}

pub(super) fn read_muse_transcript(path: &Path, original_key: &Path) -> Result<ClaudeTranscript> {
    let cwd = muse::validate_transcript(path, original_key)?;
    let transcript = read_projection(HarnessKind::Muse, path, path)?;
    ensure!(
        transcript.cwd.as_os_str() == cwd.as_os_str(),
        "Muse session workspace changed while it was being read"
    );
    Ok(transcript)
}

fn read_projection(harness: HarnessKind, key: &Path, source: &Path) -> Result<ClaudeTranscript> {
    let adapter = import_adapter(harness).context("native harness does not support import")?;
    let session = adapter
        .parse(key)
        .with_context(|| format!("read {} session {}", harness.display_name(), key.display()))?;
    project_session(session, source)
}

fn project_session(session: Session, source: &Path) -> Result<ClaudeTranscript> {
    let cwd = session
        .raw_cwd
        .filter(|cwd| !cwd.trim().is_empty())
        .map(PathBuf::from)
        .context("native session has no recorded working directory")?;
    ensure!(
        cwd.is_absolute(),
        "native session working directory is not absolute"
    );

    let mut dialogue = Vec::new();
    if session.compacted_prefix {
        dialogue.push((
            TranscriptBody::System {
                text: "Earlier turns were compacted by the native harness.".into(),
            },
            None,
        ));
    }
    let mut saw_prompt = false;
    for message in session.messages {
        let body = match message.role {
            Role::Tool => continue,
            Role::User => {
                let text = strip_hidden_prompt_context(&message.text);
                if text.trim().is_empty() {
                    continue;
                }
                saw_prompt = true;
                TranscriptBody::User {
                    content: vec![serde_json::json!({"type": "text", "text": text})],
                }
            }
            Role::Assistant if message.text.trim().is_empty() => continue,
            Role::Assistant => {
                let chunks = vec![serde_json::json!({
                    "content": {"type": "text", "text": message.text}
                })];
                if message.is_thought {
                    TranscriptBody::Thought {
                        chunks,
                        streaming: false,
                    }
                } else {
                    TranscriptBody::Agent {
                        chunks,
                        streaming: false,
                    }
                }
            }
        };
        dialogue.push((body, message.ts.map(|time| time.timestamp_millis())));
    }
    ensure!(
        saw_prompt || session.compacted_prefix,
        "native session contains no importable user prompt"
    );

    // Untimed leading items inherit the first visible native timestamp. The
    // source artifact supplies a clock for older streams with no timed dialogue.
    let mut clock = match dialogue.iter().find_map(|(_, time)| *time) {
        Some(time) => time,
        None => DateTime::<Utc>::from(fs::metadata(source)?.modified()?).timestamp_millis(),
    };
    let items = dialogue
        .into_iter()
        .enumerate()
        .map(|(index, (body, time))| {
            clock = clock.max(time.unwrap_or(clock));
            let position = index as u64 + 1;
            Arc::new(TranscriptItem {
                stable_id: format!("import-{position}"),
                position,
                latest_content_event_ordinal: matches!(body, TranscriptBody::Agent { .. })
                    .then_some(position),
                created_at_ms: clock,
                last_changed_at_ms: clock,
                body,
            })
        })
        .collect();
    Ok(ClaudeTranscript {
        cwd,
        edited_paths: session
            .operational_edited_paths
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        items,
        last_activity_at_ms: Some(
            session
                .ended
                .map_or(clock, |end| clock.max(end.timestamp_millis())),
        ),
    })
}

impl ClaudeTranscript {
    pub(super) fn materialize(&self, session_id: &str) -> MaterializedSession {
        let applied_event_ordinal = self.items.len() as u64;
        let applied_event_digest = if applied_event_ordinal == 0 {
            RELAY_EVENT_GENESIS_DIGEST.to_owned()
        } else {
            let mut digest = Sha256::new();
            digest.update(b"hel-imported-transcript-frontier-v1\0");
            digest.update(session_id.as_bytes());
            digest.update(applied_event_ordinal.to_le_bytes());
            mj_core::hex::lower_hex(digest.finalize())
        };
        MaterializedSession {
            session_id: session_id.to_owned(),
            applied_event_ordinal,
            applied_event_digest,
            last_activity_at_ms: self.last_activity_at_ms,
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: Default::default(),
            transcript: self.items.clone(),
            queued_prompts: Vec::new(),
            pending_elicitations: Vec::new(),
            active_turn: None,
            last_turn_outcome: None,
        }
    }
}
