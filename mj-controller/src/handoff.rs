//! The shared decision that turns a session snapshot into a handoff message.
//!
//! Both the cross-harness resume and the native-continuity recovery need the
//! same choice: resolve a utility model and summarize the transcript, or, when
//! no model is available, hand the most recent transcript over verbatim. That
//! decision lives here once so the two callers cannot drift apart.
//!
//! This function makes only the resolve/summarize/verbatim decision. The
//! executor and cancellation-poll wrapper that keeps a cancelled operation from
//! waiting out several network requests belongs to each caller.

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use mj_checkpoint::archive::CanonicalSessionSnapshot;
use mj_core::config::{Config, HarnessProfile};

/// The handoff byte budget for a target profile: its configured context window,
/// or [`crate::compaction::DEFAULT_CONTEXT_BYTES`] when it names none. This is
/// the target harness's budget for its first message, not the summarizer's page
/// size.
pub(crate) fn profile_handoff_bytes(profile: Option<&HarnessProfile>) -> usize {
    profile
        .and_then(|profile| profile.context_window_bytes)
        .unwrap_or(crate::compaction::DEFAULT_CONTEXT_BYTES)
}

/// Resolve a utility model and summarize the snapshot into a handoff, falling
/// back to a verbatim recent-transcript handoff when no model is available.
///
/// A cancelled discovery is the caller's own doing and propagates as an `Err`.
/// Any other resolve failure means no utility model is configured,
/// credentialed, or in quota; the handoff still has to happen, so it is logged
/// and answered with the verbatim handoff (an `Ok`).
pub(crate) async fn build_handoff_context(
    session_id: &str,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String> {
    let candidates = match crate::utility_llm::UtilityLlmRuntime::shared()
        .resolve(config, cancel)
        .await
    {
        Ok(candidates) => candidates,
        // A cancelled discovery is the caller's own doing; report it.
        Err(error) if cancel.is_cancelled() => return Err(error),
        // No utility model is configured, credentialed, or in quota. The
        // handoff still has to happen, so send the recent transcript verbatim
        // instead of failing.
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "no utility model is available for the handoff; handing over the most recent transcript verbatim"
            );
            return Ok(crate::compaction::render_recent_snapshot(snapshot, context_bytes));
        }
    };
    let backend = crate::utility_llm::UtilityCompactionBackend::new(candidates, cancel.clone());
    let page_bytes = backend.page_bytes();
    summarize_or_verbatim(session_id, snapshot, context_bytes, &backend, page_bytes, cancel).await
}

/// Summarize the snapshot through an already-resolved backend, falling back to
/// the verbatim recent transcript when the summarizer fails. This is the seam
/// the branch tests drive with a fake backend.
///
/// A summarizer that resolved but then failed must not cost the whole handoff:
/// the verbatim tail is the same floor used when no model was available, so a
/// caller never loses the transcript to a summarizer error. The native-continuity
/// path in particular installed a verbatim tail before this code was shared, and
/// must keep that floor. A cancellation is the caller's own doing and propagates.
async fn summarize_or_verbatim(
    session_id: &str,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    backend: &impl crate::compaction::CompactionBackend,
    page_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String> {
    // Pages are sized by what the summarizer can read; the handoff is sized by
    // what the target harness accepts. They are unrelated numbers, and using
    // the target's for both is what made one incident shard a transcript into
    // 33 pages.
    let budget = crate::compaction::CompactionBudget {
        page_bytes,
        handoff_bytes: context_bytes,
    };
    match crate::compaction::compact_snapshot(snapshot, budget, backend).await {
        Ok(handoff) => Ok(handoff),
        Err(error) if cancel.is_cancelled() => Err(error),
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "utility summarizer failed; handing over the most recent transcript verbatim"
            );
            Ok(crate::compaction::render_recent_snapshot(snapshot, context_bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::{CompactionBackend, HANDOFF_PREAMBLE};
    use mj_checkpoint::archive::{
        CanonicalExecutionState, CanonicalSessionState, CanonicalTranscriptBody,
        CanonicalTranscriptItem,
    };
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;

    /// A summarizer that returns a recognizable snapshot for any prompt.
    struct FakeBackend;

    impl CompactionBackend for FakeBackend {
        fn compact<'a>(
            &'a self,
            _prompt: String,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async { Ok("<state_snapshot>kept</state_snapshot>".into()) })
        }
    }

    /// A summarizer that resolves but fails every request with a fatal error.
    struct FailingBackend;

    impl CompactionBackend for FailingBackend {
        fn compact<'a>(
            &'a self,
            _prompt: String,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async { Err(anyhow::anyhow!("summarizer exploded")) })
        }
    }

    fn one_exchange_snapshot() -> CanonicalSessionSnapshot {
        let bodies = vec![
            CanonicalTranscriptBody::User {
                content: vec![serde_json::json!({"type": "text", "text": "fix the bug"})],
            },
            CanonicalTranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content": {"type": "text", "text": "done"}})],
                streaming: false,
            },
        ];
        let transcript = bodies
            .into_iter()
            .enumerate()
            .map(|(index, body)| CanonicalTranscriptItem {
                stable_id: format!("item-{index}"),
                position: index as u64 + 1,
                latest_content_event_ordinal: None,
                created_at_ms: 0,
                last_changed_at_ms: 0,
                body,
            })
            .collect();
        CanonicalSessionSnapshot {
            event_frontier: 0,
            event_frontier_digest: "0".repeat(64),
            session: CanonicalSessionState {
                execution: CanonicalExecutionState::Idle,
                last_activity_at_ms: None,
                session_title: None,
                configuration: BTreeMap::new(),
            },
            transcript,
            queued_prompts: Vec::new(),
        }
    }

    /// When a backend resolves, the handoff is the summarizer's snapshot under
    /// the shared preamble, not a verbatim transcript.
    #[tokio::test]
    async fn a_resolved_backend_produces_a_summarized_handoff() {
        let handoff = summarize_or_verbatim(
            "session-under-test",
            &one_exchange_snapshot(),
            64 * 1024,
            &FakeBackend,
            64 * 1024,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(handoff.starts_with(HANDOFF_PREAMBLE), "{handoff}");
        assert!(handoff.contains("<state_snapshot>kept</state_snapshot>"), "{handoff}");
        assert!(
            !handoff.contains("No summarizer was available"),
            "a summarized handoff must not carry the verbatim preamble: {handoff}"
        );
    }

    /// A resolved summarizer that then fails does not cost the transcript: the
    /// handoff falls back to the verbatim recent tail instead of erroring.
    #[tokio::test]
    async fn a_failing_summarizer_falls_back_to_verbatim() {
        let handoff = summarize_or_verbatim(
            "session-under-test",
            &one_exchange_snapshot(),
            64 * 1024,
            &FailingBackend,
            64 * 1024,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(handoff.starts_with(HANDOFF_PREAMBLE), "{handoff}");
        assert!(handoff.contains("fix the bug"), "{handoff}");
    }

    /// With no utility model configured, discovery fails without a network call
    /// and the real decision path falls back to the verbatim handoff.
    #[tokio::test]
    async fn no_model_falls_back_to_a_verbatim_handoff() {
        let handoff = build_handoff_context(
            "session-under-test",
            &Config::default(),
            &one_exchange_snapshot(),
            64 * 1024,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(handoff.starts_with(HANDOFF_PREAMBLE), "{handoff}");
        assert!(
            handoff.contains("No summarizer was available"),
            "the fallback handoff names its lack of a summarizer: {handoff}"
        );
        assert!(handoff.contains("fix the bug"), "{handoff}");
    }
}
