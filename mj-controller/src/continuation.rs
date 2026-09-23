//! Bounded classifier transport and verbatim authorization evidence.
use anyhow::{Context, Result, ensure};
use mj_core::continuation::{
    ASSISTANT_BYTES, ContinuationEvidence, ContinuationVerdict, EvidenceMessage, MAX_BODY_BYTES,
};
use mj_core::state::MaterializedSession;
use mj_core::transcript::{TranscriptBody, materialized_chunks_text, materialized_content_text};

pub(crate) fn evidence(session: &MaterializedSession) -> Result<ContinuationEvidence> {
    evidence_from_items(session.transcript.iter().rev().cloned().map(Ok))
}

pub(crate) fn evidence_from_items(
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

pub(crate) async fn classify(
    evidence: &ContinuationEvidence,
    diagnostic: Option<&mj_core::jev::Attempt>,
) -> Result<ContinuationVerdict> {
    let key = tokio::task::spawn_blocking(mj_core::activity::verdict::api_key)
        .await
        .context("resolve Jev key")?;
    ask_logged(
        evidence,
        key.as_deref(),
        "https://api.typesafe.ai/v1/systemone",
        "https://mj-jev-proxy.eng-admin-a63.workers.dev/v2/continuation-verdict",
        diagnostic,
    )
    .await
}

async fn ask_logged(
    evidence: &ContinuationEvidence,
    key: Option<&str>,
    direct: &str,
    hosted: &str,
    diagnostic: Option<&mj_core::jev::Attempt>,
) -> Result<ContinuationVerdict> {
    evidence.validate()?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let body = if key.is_some() {
        evidence.upstream_body()
    } else {
        serde_json::json!(evidence)
    };
    if let Some(diagnostic) = diagnostic {
        diagnostic.update(None, serde_json::json!({"request":body, "source":if key.is_some() { "direct" } else { "hosted" }, "contract":"continuation-verdict-v2", "questions":serde_json::from_str::<serde_json::Value>(mj_core::continuation::QUESTIONS)?, "model":"jev-latest", "unfinished_threshold":mj_core::continuation::CONFIDENCE, "no_input_needed_threshold":mj_core::continuation::CONFIDENCE, "maximum_continuations":mj_core::continuation::MAX_NUDGES}));
    }
    let request = if let Some(key) = key {
        client.post(direct).bearer_auth(key).json(&body)
    } else {
        client.post(hosted).json(&body)
    };
    let mut response = request
        .send()
        .await
        .context("request continuation verdict")?
        .error_for_status()?;
    ensure!(
        response
            .content_length()
            .is_none_or(|n| n <= MAX_BODY_BYTES as u64),
        "oversized continuation response"
    );
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= MAX_BODY_BYTES,
            "oversized continuation response"
        );
        body.extend_from_slice(&chunk);
    }
    let mut verdict = ContinuationVerdict::parse(&serde_json::from_slice(&body)?)?;
    if evidence.messages.is_empty() {
        verdict.unfinished = 0.0;
        verdict.no_input_needed = 0.0;
    }
    Ok(verdict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{TranscriptBody, TranscriptItem};
    use serde_json::json;
    use std::sync::Arc;

    fn session(messages: &[(&str, &str)]) -> MaterializedSession {
        let mut session = MaterializedSession::empty("isolated-continuation-evidence");
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

    #[tokio::test]
    async fn continuation_diagnostics_capture_exact_direct_and_hosted_inputs() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        for direct in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let log = mj_core::jev::DecisionLog::open(directory.path().into()).unwrap();
            let attempt = log.start(
                "s",
                "continuation",
                "Work left?",
                "Earlier user instructions",
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse::<usize>().unwrap();
                    }
                }
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                let response = r#"{"answers":{"quota_limit":{"type":"noul","noul":0.0},"unfinished":{"type":"noul","noul":0.99},"no_input_needed":{"type":"noul","noul":0.97}}}"#;
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
            });
            let mut evidence = evidence(&session(&[
                ("user", "Implement résumé 🛠 and test"),
                ("assistant", "Implemented; shall I test?"),
            ]))
            .unwrap();
            evidence.assistant_history_omitted = true;
            let verdict = ask_logged(
                &evidence,
                direct.then_some("secret-test-key"),
                &endpoint,
                &endpoint,
                Some(&attempt),
            )
            .await
            .unwrap();
            assert!(verdict.should_continue());
            let submitted = server.await.unwrap();
            attempt.finish("unchanged", "Test did not submit work");
            let page = mj_core::jev::read(directory.path(), "s", Some(&attempt.id())).unwrap();
            assert_eq!(
                page.decisions[0].technical.as_ref().unwrap()["request"],
                submitted
            );
            assert!(
                !serde_json::to_string(&page)
                    .unwrap()
                    .contains("secret-test-key")
            );
        }
    }

    #[test]
    fn continuation_preserves_authorization_across_three_exchanges() {
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
    fn continuation_never_clips_user_consent_and_marks_omitted_assistant_context() {
        let oversized = "x".repeat(mj_core::continuation::USER_BYTES + 1);
        assert!(evidence(&session(&[("user", &oversized), ("assistant", "Done")])).is_err());
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
    fn continuation_excludes_generated_input_and_prior_context() {
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
