//! Reconciling a session record with the native session its worker actually
//! opened.
//!
//! A harness whose checkpoint captures no native session — zcode keeps every
//! conversation in one shared live database — can fail to reload the session
//! the record names. The worker then opens a fresh one and reports that it did.
//! Both the controller's own worker restart and the session actor's recovery
//! reconnect land here, so the record, the notice, and the handover are decided
//! in one place.

use anyhow::{Context, Result};

use crate::session_manager::StandaloneSession;
use mj_core::config::{Config, HarnessKind};
use mj_core::relay::RelayCommand;
use mj_core::state::SessionRecord;

/// What a reconnected worker's native session means for the session record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeContinuityAction {
    /// Nothing to do: the record already names the worker's native session, or
    /// the harness owns its native state and a differing id is the restore's
    /// business rather than a lost conversation.
    Unchanged,
    /// The worker opened a different native session and nothing reports the
    /// conversation was lost with it. Adopt the id and say so.
    Adopt,
    /// The worker could not reload the recorded native session and opened a
    /// fresh one. The conversation only survives if Hel hands it over.
    AdoptAndHandOff,
}

/// Decide what a reconnected worker's reported native session means.
///
/// Only a harness whose checkpoint captures no native session can lose
/// continuity this way; for every other harness the recorded id is restored
/// with the checkpoint and a mismatch is handled where the restore is checked.
pub(crate) fn native_continuity_action(
    harness: HarnessKind,
    recorded: Option<&str>,
    reported: &str,
    continuity_lost: bool,
) -> NativeContinuityAction {
    if recorded == Some(reported) || harness.captures_native_session() {
        return NativeContinuityAction::Unchanged;
    }
    if continuity_lost {
        NativeContinuityAction::AdoptAndHandOff
    } else {
        NativeContinuityAction::Adopt
    }
}

/// What the session record and its profile say about a possible handover.
pub(crate) struct NativeContinuityInputs {
    pub harness: HarnessKind,
    pub recorded_native_session_id: Option<String>,
    pub context_bytes: usize,
    /// The configuration the handoff summarizer resolves a utility model from.
    /// Both constructors already hold a `Config`, so it travels with the inputs
    /// to the recovery caller that has no configuration of its own in scope.
    config: Config,
}

impl NativeContinuityInputs {
    pub(crate) fn from_record(config: &Config, record: &SessionRecord) -> Self {
        Self {
            harness: record.harness_kind,
            recorded_native_session_id: record.native_session_id.clone(),
            context_bytes: crate::handoff::profile_handoff_bytes(
                config.profiles.get(&record.last_profile),
            ),
            config: config.clone(),
        }
    }

    /// Read the inputs from durable state, for callers that hold no controller.
    pub(crate) fn load(session_id: &str) -> Result<Self> {
        let state = crate::database::load_state()
            .context("read the durable session before recovering native continuity")?;
        let record = state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let config = Config::load().unwrap_or_default();
        Ok(Self::from_record(&config, record))
    }
}

/// Reconcile a reconnected worker's native session with the record, handing
/// the conversation over when the worker had to open a fresh one.
///
/// The id update and the notice are the contract; a failed handover is
/// reported and does not fail the restart, because a session that lost its
/// transcript context is still far better than no worker at all.
///
/// Returns the adopted native session id when the record followed the worker
/// (whether or not the conversation was handed over), so a caller that holds
/// the launch configuration can keep it naming the live session. Returns
/// `None` when nothing changed or the worker reported no native session.
pub(crate) async fn recover_native_continuity(
    session_id: &str,
    inputs: &NativeContinuityInputs,
    connection: &mut StandaloneSession,
) -> Result<Option<String>> {
    let snapshot = connection
        .sync()
        .await
        .context("read the reconnected worker state before checking native continuity")?;
    let Some(reported) = snapshot.operational.native_session_id.clone() else {
        return Ok(None);
    };
    let action = native_continuity_action(
        inputs.harness,
        inputs.recorded_native_session_id.as_deref(),
        &reported,
        snapshot.operational.native_continuity_lost,
    );
    if action == NativeContinuityAction::Unchanged {
        return Ok(None);
    }
    // The write lane applies backpressure synchronously, so it must not run
    // on the async thread that owns this connection.
    {
        let session_id = session_id.to_owned();
        let reported = reported.clone();
        tokio::task::spawn_blocking(move || {
            crate::database::adopt_native_session_id(&session_id, &reported)
        })
        .await
        .context("join the native session id write")?
        .context("record the native session the restarted worker opened")?;
    }
    tracing::warn!(
        session_id,
        native_session_id = %reported,
        recorded = ?inputs.recorded_native_session_id,
        handing_over = action == NativeContinuityAction::AdoptAndHandOff,
        "the restarted worker opened a different native session"
    );
    if action == NativeContinuityAction::Adopt {
        return Ok(Some(reported));
    }
    push_session_notice(
        session_id,
        connection,
        "The agent could not reload its own session, so it was restarted fresh. \
         The conversation so far is being handed to it as context.",
    )
    .await;
    let installed = async {
        let canonical = mj_transcript::projection::canonical_session_from_materialized(
            &snapshot.materialized,
        )?;
        // This recovery path carries no external cancellation, so a fresh token
        // that is never cancelled lets the handoff run to completion.
        let cancel = tokio_util::sync::CancellationToken::new();
        let text = crate::handoff::build_handoff_context(
            session_id,
            &inputs.config,
            &canonical,
            inputs.context_bytes,
            &cancel,
        )
        .await?;
        connection.install_prompt_context(text).await
    }
    .await;
    if let Err(error) = installed {
        tracing::warn!(
            session_id,
            error = format!("{error:#}"),
            "could not hand the conversation to the restarted native session"
        );
        push_session_notice(
            session_id,
            connection,
            "The conversation could not be handed to the restarted agent; it starts without prior context.",
        )
        .await;
    }
    Ok(Some(reported))
}

/// Record something in the conversation for every attached surface to show.
/// A relay that refuses the notice has not damaged anything that matters here.
async fn push_session_notice(session_id: &str, connection: &mut StandaloneSession, text: &str) {
    let submitted = async {
        let command_id = crate::session_manager::new_command_id("native-continuity")?;
        connection
            .submit(
                command_id,
                RelayCommand::RecordNotice {
                    text: text.to_owned(),
                },
            )
            .await
    }
    .await;
    if let Err(error) = submitted {
        tracing::warn!(
            session_id,
            error = format!("{error:#}"),
            "could not record a native continuity notice in the conversation"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zcode worker that could not reload its own session reports a new id
    /// and the lost flag; the record follows it and the conversation has to be
    /// handed over. A harness that owns its native state is left alone, so the
    /// restore's own identity check still governs it.
    #[test]
    fn only_a_harness_without_native_state_adopts_a_restarted_workers_session() {
        assert_eq!(
            native_continuity_action(HarnessKind::Zcode, Some("old"), "new", true),
            NativeContinuityAction::AdoptAndHandOff
        );
        // A different id with no reported loss still has to be recorded, but
        // there is nothing to say the conversation went with it.
        assert_eq!(
            native_continuity_action(HarnessKind::Zcode, Some("old"), "new", false),
            NativeContinuityAction::Adopt
        );
        // A session recorded with no native id at all is the first open.
        assert_eq!(
            native_continuity_action(HarnessKind::Zcode, None, "new", true),
            NativeContinuityAction::AdoptAndHandOff
        );
        // The worker came back on the session the record already names.
        assert_eq!(
            native_continuity_action(HarnessKind::Zcode, Some("same"), "same", true),
            NativeContinuityAction::Unchanged
        );
        assert_eq!(
            native_continuity_action(HarnessKind::Codex, Some("old"), "new", true),
            NativeContinuityAction::Unchanged
        );
    }
}
