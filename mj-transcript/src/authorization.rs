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
    let mut messages: Vec<EvidenceMessage> = Vec::new();
    let mut user_bytes = 0;
    let mut assistant_bytes = 0;
    let mut assistant_history_omitted = false;
    // Where the window ends when the budget stops it: replies older than the
    // oldest kept user message answered an evicted one and go with it.
    let mut window = 0;
    // Whole recent assistant messages and the newest whole real user messages
    // that fit the budget, the same window the worker keeps live. Reverse then
    // restore chronological order; never turn a clipped instruction into consent.
    for item in items {
        let item = item?;
        if mj_core::archive::is_context_boundary(&item.stable_id) {
            break;
        }
        let (role, text) = match &item.body {
            TranscriptBody::User { content } => {
                let text = materialized_content_text(content);
                if !mj_core::continuation::is_user_authored_message(Some(&item.stable_id), &text) {
                    continue;
                }
                ensure!(
                    content.iter().all(|v| v["type"] == "text"),
                    "authorization depends on non-text context"
                );
                if matches!(
                    mj_core::acp::context_command_text(&text),
                    Some((mj_core::acp::ContextCommand::Clear, _))
                ) {
                    break;
                }
                let text = mj_core::assessment::trim_middle(&text);
                if user_bytes + text.len() > mj_core::continuation::USER_BYTES {
                    assistant_history_omitted |= messages.len() > window;
                    messages.truncate(window);
                    break;
                }
                user_bytes += text.len();
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
            if messages.len() >= mj_core::assessment::MAX_MESSAGES {
                assistant_history_omitted |= messages.len() > window;
                messages.truncate(window);
                break;
            }
            messages.push(EvidenceMessage {
                id: item.stable_id.clone(),
                role: role.into(),
                text,
            });
            if role == "user" {
                window = messages.len();
            }
        }
    }
    messages.reverse();
    let evidence = ContinuationEvidence {
        messages,
        assistant_history_omitted,
    };
    evidence.validate()?;
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{TranscriptBody, TranscriptItem};
    use serde_json::json;
    use std::sync::Arc;

    fn session(messages: &[(&str, &str)]) -> MaterializedSession {
        let mut session = MaterializedSession::empty("isolated-authorization-evidence");
        session.transcript = messages
            .iter()
            .enumerate()
            .map(|(i, (role, text))| {
                Arc::new(TranscriptItem {
                    stable_id: format!("{role}:{i}"),
                    position: i as u64,
                    latest_content_event_ordinal: Some(i as u64),
                    created_at_ms: 0,
                    last_changed_at_ms: 0,
                    body: if *role == "user" {
                        TranscriptBody::User {
                            content: vec![json!({"type":"text","text":text})],
                        }
                    } else {
                        TranscriptBody::Agent {
                            chunks: vec![json!({"content":{"type":"text","text":text}})],
                            streaming: false,
                        }
                    },
                })
            })
            .collect();
        session
    }

    #[test]
    fn evidence_preserves_authorization_across_three_exchanges() {
        let s = session(&[
            ("user", "Implement the parser, add tests, and commit."),
            ("assistant", "I will use the existing parser interface."),
            ("user", "Also handle empty files."),
            ("assistant", "That fits the plan."),
            ("user", "Go ahead."),
            (
                "assistant",
                "Implemented with empty-file support. Shall I add tests?",
            ),
        ]);
        let e = evidence(&s).unwrap();
        assert_eq!(e.messages.len(), 6);
        assert_eq!(
            e.messages[0].text,
            "Implement the parser, add tests, and commit."
        );
        assert!(!e.assistant_history_omitted);
    }

    #[test]
    fn evidence_keeps_the_newest_user_window_and_marks_omitted_assistant_context() {
        // A pasted document is kept by its head and tail, as the worker keeps it.
        let oversized = "x".repeat(mj_core::continuation::USER_BYTES + 1);
        let e = evidence(&session(&[("user", &oversized), ("assistant", "Done")])).unwrap();
        assert!(e.messages[0].text.contains("bytes omitted from the middle"));
        // A long session keeps the newest whole user messages that fit.
        let mut long = Vec::new();
        for i in 0..40 {
            long.push(("user", format!("step {i} {}", "u".repeat(1024))));
            long.push(("assistant", format!("done {i}")));
        }
        let long: Vec<(&str, &str)> = long.iter().map(|(r, t)| (*r, t.as_str())).collect();
        let e = evidence(&session(&long)).unwrap();
        let users: Vec<_> = e.messages.iter().filter(|m| m.role == "user").collect();
        assert!(users.len() < 40);
        assert!(users.last().unwrap().text.starts_with("step 39 "));
        assert!(e.messages.first().unwrap().role == "user");
        let long_reply = "x".repeat(ASSISTANT_BYTES);
        let e = evidence(&session(&[
            ("user", "Implement and test"),
            ("assistant", &long_reply),
            ("user", "Continue"),
            ("assistant", "Implemented; tests remain"),
        ]))
        .unwrap();
        assert!(e.assistant_history_omitted);
        assert_eq!(e.messages.len(), 3);
    }

    #[test]
    fn evidence_excludes_generated_input_and_prior_context() {
        let mut s = session(&[
            ("user", "Old authorization"),
            ("user", "New request"),
            ("user", "Generated prompt"),
            ("assistant", "Finished"),
        ]);
        s.transcript[0] = Arc::new(TranscriptItem {
            stable_id: "context-cleared:1".into(),
            ..(*s.transcript[0]).clone()
        });
        s.transcript[2] = Arc::new(TranscriptItem {
            stable_id: "user:auto-continue-1-1".into(),
            ..(*s.transcript[2]).clone()
        });
        let e = evidence(&s).unwrap();
        assert_eq!(e.messages.len(), 2);
        assert_eq!(e.messages[0].text, "New request");
        s.transcript[1] = Arc::new(TranscriptItem {
            body: TranscriptBody::User {
                content: vec![json!({"type":"image","data":"not-text"})],
            },
            ..(*s.transcript[1]).clone()
        });
        assert!(evidence(&s).is_err());
    }
}
