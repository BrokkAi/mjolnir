//! Whole-message authorization evidence shared by workers and controllers.
use anyhow::{Result, ensure};
use mj_core::continuation::{ASSISTANT_BYTES, ContinuationEvidence, EvidenceMessage};
use mj_core::state::MaterializedSession;
use mj_core::transcript::{TranscriptBody, materialized_chunks_text, materialized_content_text};

pub fn evidence(session: &MaterializedSession) -> Result<ContinuationEvidence> {
    evidence_from_items(session.transcript.iter().rev().cloned().map(Ok))
}

pub fn evidence_from_items(
    items: impl IntoIterator<Item = Result<std::sync::Arc<mj_core::state::TranscriptItem>>>,
) -> Result<ContinuationEvidence> {
    let mut messages = Vec::new();
    let mut user_bytes = 0;
    let mut assistant_bytes = 0;
    let mut assistant_history_omitted = false;
    // Whole recent assistant messages, all real user messages. Reverse then
    // restore chronological order; never turn a clipped instruction into consent.
    for item in items {
        let item = item?;
        if mj_core::archive::is_context_boundary(&item.stable_id) {
            break;
        }
        let (role, text) = match &item.body {
            TranscriptBody::User { content } => {
                let id = item
                    .stable_id
                    .strip_prefix("user:")
                    .unwrap_or(&item.stable_id);
                if mj_core::continuation::is_generated_prompt(id) {
                    continue;
                }
                ensure!(
                    content.iter().all(|v| v["type"] == "text"),
                    "authorization depends on non-text context"
                );
                let text = materialized_content_text(content);
                if mj_core::continuation::is_generated_prompt_text(&text) {
                    continue;
                }
                if matches!(
                    mj_core::acp::context_command_text(&text),
                    Some((mj_core::acp::ContextCommand::Clear, _))
                ) {
                    break;
                }
                user_bytes += text.len();
                ensure!(
                    user_bytes <= mj_core::continuation::USER_BYTES,
                    "user history exceeds evidence budget"
                );
                ("user", text)
            }
            TranscriptBody::Agent { chunks, streaming } => {
                ensure!(!streaming, "assistant reply is still streaming");
                let text = materialized_chunks_text(chunks);
                if text.trim().is_empty() {
                    continue;
                }
                if assistant_history_omitted || assistant_bytes + text.len() > ASSISTANT_BYTES {
                    ensure!(
                        assistant_bytes > 0,
                        "final assistant reply exceeds evidence budget"
                    );
                    assistant_history_omitted = true;
                    continue;
                }
                assistant_bytes += text.len();
                ("assistant", text)
            }
            _ => continue,
        };
        if !text.trim().is_empty() {
            ensure!(messages.len() < 256, "too many continuation messages");
            messages.push(EvidenceMessage {
                id: item.stable_id.clone(),
                role: role.into(),
                text,
            });
        }
    }
    messages.reverse();
    let evidence = ContinuationEvidence {
        quota_message: None,
        messages,
        assistant_history_omitted,
    };
    evidence.validate()?;
    Ok(evidence)
}
