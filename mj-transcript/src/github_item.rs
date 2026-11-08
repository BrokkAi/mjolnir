//! Compact GitHub relevance evidence from the materialized transcript.
use mj_core::github_item::*;
use mj_core::state::{MaterializedSession, TranscriptBody};
use mj_core::transcript::{materialized_chunks_text, materialized_content_text};

fn prefix(text: &str, bytes: usize) -> String {
    text[..text.floor_char_boundary(bytes.min(text.len()))].to_owned()
}

fn distinct(values: impl Iterator<Item = String>, count: usize) -> Vec<String> {
    let mut result = Vec::new();
    for value in values {
        if !result.contains(&value) {
            result.push(value);
        }
        if result.len() == count {
            break;
        }
    }
    result
}

pub fn session_digest(session: &MaterializedSession) -> SessionContext {
    let boundary = session
        .transcript
        .iter()
        .rposition(|i| mj_core::archive::is_context_boundary(&i.stable_id))
        .map_or(0, |i| i + 1);
    let items = &session.transcript[boundary..];
    let start = items
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, i)| matches!(i.body, TranscriptBody::User { .. }))
        .nth(SESSION_TURNS - 1)
        .map_or(0, |(i, _)| i);
    let items = &items[start..];
    let mut digest = SessionContext::default();
    let mut files = Vec::new();
    let mut refs = Vec::new();
    for item in items {
        let text = match &item.body {
            TranscriptBody::User { content } => {
                let text = materialized_content_text(content);
                digest.requests.push(prefix(&text, REQUEST_BYTES));
                digest.latest_reply.clear();
                text
            }
            TranscriptBody::Agent { chunks, .. } => {
                let text = materialized_chunks_text(chunks);
                digest.latest_reply = prefix(&text, REPLY_BYTES);
                text
            }
            TranscriptBody::Tool { call, .. } => {
                if let Ok(call) = serde_json::from_value::<
                    agent_client_protocol::v1::schema::ToolCall,
                >(call.clone())
                {
                    files.extend(
                        call.locations
                            .iter()
                            .map(|l| prefix(&l.path.to_string_lossy(), FILE_BYTES)),
                    );
                    call.title.to_string()
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        refs.extend(
            mj_core::github_reference::github_references(&text)
                .into_iter()
                .map(|r| prefix(&r.text, REFERENCE_BYTES)),
        );
    }
    digest.files = distinct(files.into_iter().rev(), MAX_FILES);
    digest.references = distinct(refs.into_iter().rev(), MAX_REFERENCES);
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::TranscriptItem;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn digest_preserves_recency_and_empty_reply_across_turn_boundaries() {
        let mut session = MaterializedSession::empty("digest-test");
        let bodies = [
            TranscriptBody::User {
                content: vec![json!({"type":"text","text":"old #1"})],
            },
            TranscriptBody::User {
                content: vec![json!({"type":"text","text":"first #2 owner/repo#3"})],
            },
            TranscriptBody::User {
                content: vec![json!({"type":"text","text":"second #2"})],
            },
            TranscriptBody::Tool {
                call: json!({"toolCallId":"t","title":"Read https://github.com/owner/repo/pull/4", "status":"completed","locations":[{"path":"src/lib.rs"}]}),
                terminal_outputs: vec![],
                terminal_refs: vec![],
                presentation: None,
            },
            TranscriptBody::User {
                content: vec![json!({"type":"text","text":"x".repeat(511) + "🦀"})],
            },
        ];
        for (i, body) in bodies.into_iter().enumerate() {
            session.transcript.push(Arc::new(TranscriptItem {
                stable_id: format!("item:{i}"),
                position: i as u64 + 1,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body,
            }));
        }
        let digest = session_digest(&session);
        assert_eq!(
            digest.requests,
            ["first #2 owner/repo#3", "second #2", &"x".repeat(511)]
        );
        assert!(digest.latest_reply.is_empty());
        assert_eq!(digest.files, ["src/lib.rs"]);
        assert_eq!(
            digest.references,
            ["https://github.com/owner/repo/pull/4", "#2", "owner/repo#3"]
        );
    }
}
