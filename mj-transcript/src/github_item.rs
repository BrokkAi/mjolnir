//! Compact GitHub relevance evidence from the materialized transcript.
use mj_core::github_item::*;
use mj_core::state::{MaterializedSession, TranscriptBody};
use mj_core::transcript::{materialized_chunks_text, materialized_content_text};

fn prefix(text: &str, bytes: usize) -> String {
    text[..text.floor_char_boundary(bytes.min(text.len()))].to_owned()
}

const MENTION_ELISION: &str = "…";
/// Bytes kept before the reference, so the subject that relates the session
/// to it ("reviewing", "blocked on") usually survives.
const MENTION_LEAD_BYTES: usize = 96;

/// The part of `line` around the reference parsed at byte offset `at`, at
/// most [`MENTION_BYTES`]. Agent paragraphs are often one long line, so a
/// plain prefix would drop the reference the mention was chosen for. The
/// parser's offset, not a text search, places the window: `#12` must not
/// land on `#123`, and each occurrence keeps its own context.
fn mention_window(line: &str, at: usize, reference_len: usize) -> String {
    if line.len() <= MENTION_BYTES {
        return line.to_owned();
    }
    // Shrink the lead so the whole reference fits: a URL with long owner and
    // repository names can take most of the window by itself.
    let lead_bytes = MENTION_LEAD_BYTES
        .min(MENTION_BYTES.saturating_sub(reference_len + 2 * MENTION_ELISION.len()));
    let start = line.floor_char_boundary(at.saturating_sub(lead_bytes));
    let lead = if start > 0 { MENTION_ELISION } else { "" };
    let rest = &line[start..];
    let room = MENTION_BYTES - lead.len();
    if rest.len() <= room {
        return format!("{lead}{rest}");
    }
    let end = rest.floor_char_boundary(room - MENTION_ELISION.len());
    format!("{lead}{}{MENTION_ELISION}", &rest[..end])
}

const MESSAGE_TRUNCATION_MARKER: &str = "\n[Message truncated.]";

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
    let mut messages = Vec::new();
    let mut files = Vec::new();
    let mut refs = Vec::new();
    for item in items {
        let text = match &item.body {
            TranscriptBody::User { content } => {
                let text = materialized_content_text(content);
                if text.trim() != mj_core::continuation::PROMPT
                    && mj_core::acp::context_command_text(&text).is_none()
                    && !text.trim().is_empty()
                {
                    messages.push(("User: ", text.clone()));
                }
                text
            }
            TranscriptBody::Message { event } => {
                if event.message_text().is_some() {
                    messages.push(("User: ", mj_core::mailbox::render_mailbox_event(event)));
                }
                if event.message_text().is_some() {
                    mj_core::mailbox::render_mailbox_event(event)
                } else {
                    String::new()
                }
            }
            TranscriptBody::Agent { chunks, .. } => {
                let text = materialized_chunks_text(chunks);
                if !text.trim().is_empty() {
                    messages.push(("Agent: ", text.clone()));
                }
                text
            }
            TranscriptBody::Tool { call, .. } => {
                if let Ok(call) = call.decode() {
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
        // Index full lines before conversation truncation or the reference-list cap.
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            for reference in mj_core::github_reference::github_references(line) {
                let window = mention_window(line, reference.start, reference.text.len());
                digest.mention_candidates.push((reference, window));
            }
        }
        refs.extend(
            mj_core::github_reference::github_references(&text)
                .into_iter()
                .map(|r| prefix(&r.text, REFERENCE_BYTES)),
        );
    }
    // Reserve newest messages first, then restore their chronological order.
    // Labels, newlines and truncation markers all count against the budget.
    let mut remaining = CONVERSATION_BYTES;
    let mut selected = Vec::new();
    for (role, text) in messages.into_iter().rev() {
        let Some(available) = remaining.checked_sub(role.len() + 1) else {
            break;
        };
        let cap = MESSAGE_BYTES.min(available);
        let message = if text.len() > cap {
            let Some(head) = cap.checked_sub(MESSAGE_TRUNCATION_MARKER.len()) else {
                break;
            };
            format!("{}{}", prefix(&text, head), MESSAGE_TRUNCATION_MARKER)
        } else {
            text
        };
        let rendered = format!("{role}{message}\n");
        remaining -= rendered.len();
        selected.push(rendered);
    }
    digest.conversation = selected.into_iter().rev().collect();
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
    fn mentions_survive_reference_and_conversation_truncation_with_utf8() {
        let mut session = MaterializedSession::empty("mention-index");
        let text = format!(
            "pending #1\n{}\n{}",
            "é".repeat(MESSAGE_BYTES),
            (2..50)
                .map(|n| format!("pending #{n}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        session.transcript.push(Arc::new(TranscriptItem {
            stable_id: "user:1".into(),
            position: 1,
            latest_content_event_ordinal: None,
            created_at_ms: 0,
            last_changed_at_ms: 0,
            body: TranscriptBody::User {
                content: vec![json!({"type":"text","text":text})],
            },
        }));
        let digest = session_digest(&session);
        assert!(!digest.references.contains(&"#1".to_owned()));
        let item = GithubItem {
            repo: "owner/repo".into(),
            kind: GithubItemKind::Issue,
            number: 49,
            title: "Fix #1".into(),
            body: String::new(),
            author: String::new(),
            url: "https://github.com/owner/repo/issues/49".into(),
        };
        assert_eq!(
            digest.for_item(&item, true).mentions,
            ["pending #49", "pending #1"]
        );
        let long = format!(
            "#123 first {} reviewing #12 next {}",
            "é".repeat(200),
            "x".repeat(400)
        );
        let at = mj_core::github_reference::github_references(&long)
            .into_iter()
            .find(|r| r.number == 12)
            .unwrap()
            .start;
        let window = mention_window(&long, at, "#12".len());
        assert!(window.len() <= MENTION_BYTES && window.contains("reviewing #12 next"));
        // GitHub allows 39-byte owners and 100-byte repository names.
        let url = format!(
            "https://github.com/{}/{}/pull/123456",
            "o".repeat(39),
            "r".repeat(100)
        );
        let long = format!("{} {url} {}", "x".repeat(300), "y".repeat(300));
        let at = long.find(&url).unwrap();
        let window = mention_window(&long, at, url.len());
        assert!(window.len() <= MENTION_BYTES && window.contains(&url));
        assert!(window.starts_with(MENTION_ELISION) && window.ends_with(MENTION_ELISION));
    }

    #[test]
    fn digest_preserves_recency_and_utf8_across_turn_boundaries() {
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
                call: json!({"toolCallId":"t","title":"Read https://github.com/owner/repo/pull/4", "status":"completed","locations":[{"path":"src/lib.rs"}]}).into(),
                terminal_outputs: vec![],
                terminal_refs: vec![],
                presentation: None,
            },
            TranscriptBody::User {
                content: vec![
                    json!({"type":"text","text":"x".repeat(MESSAGE_BYTES - MESSAGE_TRUNCATION_MARKER.len() - 1) + "🦀" + &"tail".repeat(20)}),
                ],
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
            digest.conversation,
            format!(
                "User: first #2 owner/repo#3\nUser: second #2\nUser: {}{}\n",
                "x".repeat(MESSAGE_BYTES - MESSAGE_TRUNCATION_MARKER.len() - 1),
                MESSAGE_TRUNCATION_MARKER
            )
        );
        assert_eq!(digest.files, ["src/lib.rs"]);
        assert_eq!(
            digest.references,
            ["https://github.com/owner/repo/pull/4", "#2", "owner/repo#3"]
        );
        Arc::make_mut(session.transcript.last_mut().unwrap()).stable_id =
            format!("{}clear", mj_core::archive::CONTEXT_BOUNDARY_PREFIX);
        let cleared = session_digest(&session);
        assert!(cleared.conversation.is_empty());
        assert!(cleared.files.is_empty());
        assert!(cleared.references.is_empty());
    }

    #[test]
    fn digest_keeps_peer_relationships_and_newest_messages_within_budget() {
        let mut session = MaterializedSession::empty("digest-budget");
        let mut add = |stable_id: String, body| {
            session.transcript.push(Arc::new(TranscriptItem {
                stable_id,
                position: session.transcript.len() as u64 + 1,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body,
            }));
        };
        let user = |text: String| TranscriptBody::User {
            content: vec![json!({"type":"text","text":text})],
        };
        add("user:old".into(), user("discard before clear".into()));
        add("context-cleared:1".into(), user("/clear".into()));
        add("user:task".into(), user("Review the pending fix".into()));
        add(
            "user:continuation".into(),
            user(mj_core::continuation::PROMPT.into()),
        );
        add(
            "user:compact".into(),
            user(" /compact retain pending reviews".into()),
        );
        // Enough assistant messages to exhaust even the largest replay budget.
        for i in 0..CONVERSATION_BYTES / MESSAGE_BYTES + 2 {
            add(
                format!("agent:{i}"),
                TranscriptBody::Agent {
                    chunks: vec![
                        json!({"content":{"type":"text","text":format!("reply {i}: {}", "é".repeat(MESSAGE_BYTES))}}),
                    ],
                    streaming: false,
                },
            );
        }
        add(
            "user:peer".into(),
            user("Message from session reviewer: awaiting #42 final head".into()),
        );
        let peer = mj_core::mailbox::MailboxEvent {
            key: "peer-review".into(),
            source: "peer".into(),
            wake: false,
            created_at_ms: 0,
            body: mj_core::mailbox::MailboxEventBody::ParentMessage {
                text: "The review for issue 42 is pending".into(),
            },
        };
        let peer_text = mj_core::mailbox::render_mailbox_event(&peer);
        add(
            "message:peer-review".into(),
            TranscriptBody::Message {
                event: Box::new(peer),
            },
        );
        let digest = session_digest(&session);
        assert!(digest.conversation.len() <= CONVERSATION_BYTES);
        assert!(!digest.conversation.contains("discard before clear"));
        assert!(!digest.conversation.contains("Review the pending fix"));
        assert!(!digest.conversation.contains(mj_core::continuation::PROMPT));
        assert!(!digest.conversation.contains("/compact"));
        assert!(!digest.conversation.contains("reply 0:"));
        assert!(
            digest
                .conversation
                .ends_with(&format!("User: {peer_text}\n"))
        );
        assert!(
            digest
                .conversation
                .contains("User: Message from session reviewer: awaiting #42 final head\n")
        );
        assert!(digest.conversation.contains(MESSAGE_TRUNCATION_MARKER));
        assert_eq!(digest.references, ["#42"]);
    }
}
