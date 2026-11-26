//! The shared decision that turns a session snapshot into a handoff message.
//!
//! Both the cross-harness resume and the native-continuity recovery need the
//! same choice: resolve a utility model and summarize the transcript, or, when
//! no model is available, hand the shared recent transcript view over. That
//! decision lives here once so the two callers cannot drift apart.
//!
//! This function makes only the resolve/summarize/recent-context decision. The
//! executor and cancellation-poll wrapper that keeps a cancelled operation from
//! waiting out several network requests belongs to each caller.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use mj_transcript::handoff::reusable_handoff;
use tokio_util::sync::CancellationToken;

use mj_checkpoint::archive::{ArchiveManifest, CanonicalSessionSnapshot, CanonicalTranscriptBody};
use mj_core::config::{Config, HarnessKind, HarnessProfile};

pub mod antigravity;
pub mod claude;

#[allow(clippy::large_enum_variant)] // The archive source owns its manifest.
pub enum NativePriorSource {
    Archive {
        archive_path: PathBuf,
        manifest: ArchiveManifest,
    },
    Home {
        harness_kind: HarnessKind,
        harness_home: PathBuf,
        native_session_id: String,
    },
    File {
        harness_kind: HarnessKind,
        file: File,
    },
}

impl NativePriorSource {
    fn harness_kind(&self) -> HarnessKind {
        match self {
            Self::Archive { manifest, .. } => manifest.session.harness_kind,
            Self::Home { harness_kind, .. } | Self::File { harness_kind, .. } => *harness_kind,
        }
    }
}

/// Read optional native compaction evidence for a canonical handoff.
pub async fn native_prior(
    session_id: &str,
    source: NativePriorSource,
    snapshot: &Arc<CanonicalSessionSnapshot>,
    cancel: CancellationToken,
) -> Result<Option<crate::compaction::PriorCompaction>> {
    let harness_kind = source.harness_kind();
    if !matches!(harness_kind, HarnessKind::Claude | HarnessKind::Antigravity) {
        return Ok(None);
    }
    let session_id = session_id.to_owned();
    let snapshot = Arc::clone(snapshot);
    mj_core::runtime::spawn_blocking(move || {
        let prior = match (harness_kind, source) {
            (
                HarnessKind::Claude,
                NativePriorSource::Archive {
                    archive_path,
                    manifest,
                },
            ) => {
                claude::prior_compaction_from_archive(&archive_path, &manifest, &snapshot, &cancel)
            }
            (
                _,
                NativePriorSource::Archive {
                    archive_path,
                    manifest,
                },
            ) => antigravity::prior_compaction_from_archive(
                &archive_path,
                &manifest,
                &snapshot,
                &cancel,
            ),
            (
                HarnessKind::Claude,
                NativePriorSource::Home {
                    harness_home,
                    native_session_id,
                    ..
                },
            ) => claude::prior_compaction_from_home(
                &harness_home,
                &native_session_id,
                &snapshot,
                &cancel,
            ),
            (
                _,
                NativePriorSource::Home {
                    harness_home,
                    native_session_id,
                    ..
                },
            ) => antigravity::prior_compaction_from_home(
                &harness_home,
                &native_session_id,
                &snapshot,
                &cancel,
            ),
            (HarnessKind::Claude, NativePriorSource::File { file, .. }) => {
                claude::prior_compaction_from_reader(file, &snapshot, &cancel)
            }
            (_, NativePriorSource::File { file, .. }) => {
                antigravity::prior_compaction_from_reader(file, &snapshot, &cancel)
            }
        };
        match prior {
            Ok(prior) => Ok(prior),
            Err(error) if cancel.is_cancelled() => Err(error),
            Err(error) => {
                tracing::warn!(
                    session_id,
                    harness = ?harness_kind,
                    error = format!("{error:#}"),
                    "could not use native compaction; using canonical history"
                );
                Ok(None)
            }
        }
    })
    .await
    .context("join native compaction read")?
}

fn check_cancel(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        // Interrupted errors are retryable by I/O helpers.
        return Err(io::Error::other("native compaction preparation cancelled"));
    }
    Ok(())
}

/// Splits a streamed native transcript into non-blank lines. The caller's
/// handler sees each line and its 1-based number; cancellation is checked on
/// every chunk and line so a huge line cannot delay it.
struct LineScanner<'a, F> {
    cancel: &'a CancellationToken,
    line_buf: Vec<u8>,
    line_number: usize,
    on_line: F,
}

impl<'a, F: FnMut(&[u8], usize) -> io::Result<()>> LineScanner<'a, F> {
    fn new(cancel: &'a CancellationToken, on_line: F) -> Self {
        Self {
            cancel,
            line_buf: Vec::new(),
            line_number: 0,
            on_line,
        }
    }

    /// Handle an unterminated final line.
    fn finish(mut self) -> io::Result<()> {
        check_cancel(self.cancel)?;
        if !self.line_buf.is_empty() {
            self.end_line()?;
        }
        Ok(())
    }

    fn end_line(&mut self) -> io::Result<()> {
        check_cancel(self.cancel)?;
        self.line_number += 1;
        if !self.line_buf.iter().all(u8::is_ascii_whitespace) {
            (self.on_line)(&self.line_buf, self.line_number)?;
        }
        self.line_buf.clear();
        Ok(())
    }
}

impl<F: FnMut(&[u8], usize) -> io::Result<()>> Write for LineScanner<'_, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancel(self.cancel)?;
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            if let Some(line) = line.strip_suffix(b"\n") {
                self.line_buf.extend_from_slice(line);
                self.end_line()?;
            } else {
                self.line_buf.extend_from_slice(line);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The last position that a native compaction must not reach back across: the
/// current context start or a validated generic handoff, whichever is later.
fn lineage_start(snapshot: &CanonicalSessionSnapshot) -> u64 {
    let context_start = snapshot.current_context_start();
    reusable_handoff(snapshot).map_or(context_start, |state| {
        context_start.max(state.through_position)
    })
}

/// Canonical positions before `anchor_position` that a native summary does not
/// cover and the handoff must replay: tools it does not cover, agent text that
/// changed at or after the anchor, and the latest plan.
fn collect_retained_positions(
    snapshot: &CanonicalSessionSnapshot,
    lineage_start: u64,
    anchor_position: u64,
    is_covered_tool: impl Fn(&serde_json::Value) -> bool,
) -> Vec<u64> {
    let mut retained_positions = Vec::new();
    let mut latest_plan = None;
    for item in snapshot
        .transcript
        .iter()
        .filter(|item| lineage_start < item.position && item.position < anchor_position)
    {
        match &item.body {
            CanonicalTranscriptBody::Tool { call, .. } if !is_covered_tool(call) => {
                retained_positions.push(item.position);
            }
            CanonicalTranscriptBody::Agent { .. }
                if item
                    .latest_content_event_ordinal
                    .is_none_or(|ordinal| ordinal >= anchor_position) =>
            {
                retained_positions.push(item.position);
            }
            CanonicalTranscriptBody::Plan { .. } => {
                latest_plan = latest_plan.max(Some(item.position));
            }
            _ => {}
        }
    }
    retained_positions.extend(latest_plan);
    retained_positions.sort_unstable();
    retained_positions
}

/// Whether `id` is exactly one ordinary path component without control
/// characters, so joining it to a directory cannot leave that directory.
fn is_single_path_component(id: &str) -> bool {
    let mut components = Path::new(id).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && !id.chars().any(char::is_control)
}

/// The handoff byte budget for a target profile: its configured context window,
/// or [`crate::compaction::DEFAULT_CONTEXT_BYTES`] when it names none. This is
/// the target harness's budget for its first message, not the summarizer's page
/// size.
pub fn profile_handoff_bytes(profile: Option<&HarnessProfile>) -> usize {
    profile
        .and_then(|profile| profile.context_window_bytes)
        .unwrap_or(crate::compaction::DEFAULT_CONTEXT_BYTES)
}

/// Resolve a utility model and summarize the snapshot into a handoff, falling
/// back to a bounded recent-transcript handoff when no model is available.
///
/// A cancelled discovery is the caller's own doing and propagates as an `Err`.
/// Any other resolve failure means no utility model is configured,
/// credentialed, or in quota; the handoff still has to happen, so it is logged
/// and answered with the bounded recent handoff (an `Ok`).
pub async fn build_handoff_context(
    session_id: &str,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    cancel: &CancellationToken,
) -> Result<String> {
    build_handoff_context_with_prior(session_id, config, snapshot, context_bytes, None, cancel)
        .await
}

pub async fn build_handoff_context_with_prior(
    session_id: &str,
    config: &Config,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    prior: Option<&crate::compaction::PriorCompaction>,
    cancel: &CancellationToken,
) -> Result<String> {
    let candidates = match crate::utility_llm::UtilityLlmRuntime::shared()
        .resolve(config, cancel)
        .await
    {
        Ok(candidates) => candidates,
        // A cancelled discovery is the caller's own doing; report it.
        Err(error) if cancel.is_cancelled() => return Err(error),
        // No utility model is configured, credentialed, or in quota. A Claude
        // profile can still summarize through the Claude Code CLI; failing
        // that, the handoff still has to happen, so send the shared recent
        // transcript view instead of failing.
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "no utility model is available for the handoff; trying Claude Code print mode"
            );
            return match crate::claude_compaction::ClaudePrintCompactionBackend::resolve(
                config, cancel,
            )
            .await
            {
                Ok(Some(backend)) => {
                    summarize_or_recent(
                        session_id,
                        snapshot,
                        context_bytes,
                        &backend,
                        backend.page_bytes(),
                        prior,
                        cancel,
                    )
                    .await
                }
                Ok(None) => {
                    tracing::warn!(
                        session_id,
                        "no Claude profile can summarize the handoff; handing over the shared recent transcript view"
                    );
                    Ok(crate::compaction::render_recent_snapshot(
                        snapshot,
                        context_bytes,
                    ))
                }
                Err(error) if cancel.is_cancelled() => Err(error),
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "Claude Code print mode is unavailable for the handoff; handing over the shared recent transcript view"
                    );
                    Ok(crate::compaction::render_recent_snapshot(
                        snapshot,
                        context_bytes,
                    ))
                }
            };
        }
    };
    let backend = crate::utility_llm::UtilityCompactionBackend::new(candidates, cancel.clone());
    let page_bytes = backend.page_bytes();
    summarize_or_recent(
        session_id,
        snapshot,
        context_bytes,
        &backend,
        page_bytes,
        prior,
        cancel,
    )
    .await
}

/// Summarize the snapshot through an already-resolved backend, falling back to
/// the bounded recent transcript when the summarizer fails. This is the seam
/// the branch tests drive with a fake backend.
///
/// A summarizer that resolved but then failed must not cost the whole handoff:
/// the bounded recent context is the same floor used when no model was available, so a
/// caller never loses the transcript to a summarizer error. The native-continuity
/// path in particular installed bounded recent context before this code was shared, and
/// must keep that floor. A cancellation is the caller's own doing and propagates.
pub(crate) async fn summarize_or_recent(
    session_id: &str,
    snapshot: &CanonicalSessionSnapshot,
    context_bytes: usize,
    backend: &impl crate::compaction::CompactionBackend,
    page_bytes: usize,
    prior: Option<&crate::compaction::PriorCompaction>,
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
    match crate::compaction::compact_snapshot_since(snapshot, budget, backend, prior).await {
        Ok(handoff) => Ok(handoff),
        Err(error) if cancel.is_cancelled() => Err(error),
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "utility summarizer failed; handing over the shared recent transcript view"
            );
            Ok(crate::compaction::render_recent_snapshot(
                snapshot,
                context_bytes,
            ))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::compaction::{CompactionBackend, HANDOFF_PREAMBLE};
    use mj_checkpoint::archive::{
        CanonicalExecutionState, CanonicalSessionState, CanonicalTranscriptBody,
        CanonicalTranscriptItem,
    };
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

    pub(crate) fn one_exchange_snapshot() -> CanonicalSessionSnapshot {
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
            command_ledger: None,
            assessment_state: None,
            notification_stack: None,
            native_use_evidence: None,
            pending_prompt_context: None,
            event_frontier: 0,
            event_frontier_digest: "0".repeat(64),
            session: CanonicalSessionState {
                execution: CanonicalExecutionState::Idle,
                last_activity_at_ms: None,
                session_title: None,
                configuration: Default::default(),
            },
            transcript,
            queued_prompts: Vec::new(),
            mailbox: None,
        }
    }

    /// When a backend resolves, the handoff is the summarizer's snapshot under
    /// the shared preamble, not a bounded transcript.
    // Hard-won: 781a40ea: native-continuity recovery handed over only the recent transcript and lost all pre-cutoff context despite an available utility model.
    #[tokio::test]
    async fn a_resolved_backend_produces_a_summarized_handoff() {
        let handoff = summarize_or_recent(
            "session-under-test",
            &one_exchange_snapshot(),
            64 * 1024,
            &FakeBackend,
            64 * 1024,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(handoff.starts_with(HANDOFF_PREAMBLE), "{handoff}");
        assert!(
            handoff.contains("<state_snapshot>kept</state_snapshot>"),
            "{handoff}"
        );
        assert!(
            !handoff.contains("No summarizer was available"),
            "a summarized handoff must not carry the fallback preamble: {handoff}"
        );
    }

    /// A resolved summarizer that then fails does not cost the transcript: the
    /// handoff falls back to the bounded recent context instead of erroring.
    #[tokio::test]
    async fn a_failing_summarizer_falls_back_to_verbatim() {
        let handoff = summarize_or_recent(
            "session-under-test",
            &one_exchange_snapshot(),
            64 * 1024,
            &FailingBackend,
            64 * 1024,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(handoff.starts_with(HANDOFF_PREAMBLE), "{handoff}");
        assert!(handoff.contains("fix the bug"), "{handoff}");
    }
}
