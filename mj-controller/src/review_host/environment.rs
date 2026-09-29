use super::*;

/// Everything a review needs from the controller: whether it can review this
/// session at all, and a staged reviewer profile to launch a role from.
///
/// It is a trait so the host's own tests can drive a whole review without a
/// container, a harness, or the developer's own `config.toml`. The daemon
/// installs [`ControllerEnvironment`], which loads the real controller.
pub trait ReviewEnvironment: Send + Sync {
    /// Refuses, with a sentence for a person, when this session cannot be
    /// reviewed under `profile`.
    fn check(&self, session_id: &str, profile: &str) -> Result<(), String>;

    /// Whether this session is a Mjolnir-managed sub-agent. Its changes are
    /// reviewed through the parent's turn, so it is never reviewed on its
    /// own. Blocking: it reads the controller's database.
    fn is_subagent(&self, _session_id: &str) -> bool {
        false
    }

    fn resolve<'a>(
        &'a self,
        handle: ManagedSessionHandle,
        config: ReviewConfig,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    >;

    /// Stages the reviewer profile for one role and describes how to launch
    /// it. Blocking: it copies a profile onto the session's target.
    fn stage(
        &self,
        session_id: &str,
        profile: &str,
        generation: u64,
        mcp_servers: &[mj_core::worker_launch::ReviewMcpServer],
        dispatch_tool: bool,
    ) -> Result<mj_core::worker_launch::ReviewerLaunchConfig, String>;

    /// How far this session has been reviewed. Blocking: it reads the
    /// controller's database.
    fn load_state(&self, session_id: &str) -> Result<TurnReviewState, String>;

    /// Records how far this session has been reviewed. Blocking: the host
    /// routes it through its ordered persistence lane rather than calling it
    /// on the Tokio task that owns review state.
    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String>;

    /// Clears the in-flight flag of every review a restart interrupted, and
    /// reports whose they were. Baselines are deliberately left alone: the
    /// interrupted review never advanced one, so the next review covers the
    /// same change and nothing is lost.
    fn clear_interrupted(&self) -> Result<Vec<String>, String>;

    /// Waits until no background work holds this session, or until
    /// `deadline`. The automatic recovery copy a finished turn starts takes
    /// the session's lease, and a lease cancels reviewer actions in flight
    /// (R4-9). An environment with no background work returns at once.
    fn background_work_settled<'a>(
        &'a self,
        _session_id: &'a str,
        _deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// The production environment: the controller as it is on disk right now.
///
/// It is reloaded per call rather than held, because a review is rare and the
/// answer must reflect the config as it stands when the review starts -- the
/// daemon reloads config.toml every 500 ms for the same reason.
#[derive(Default)]
pub struct ControllerEnvironment {
    /// The daemon's gate for recovery copies and worker upgrades, which a
    /// review waits behind instead of racing for the session's lease.
    pub background: Option<Arc<crate::recovery_gate::RecoveryGate>>,
}

impl ReviewEnvironment for ControllerEnvironment {
    fn check(&self, session_id: &str, profile: &str) -> Result<(), String> {
        let controller =
            crate::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        let Some(reviewer) = controller.config.profiles.get(profile) else {
            return Err(format!(
                "turn review needs a reviewer: [review] profile {profile:?} is not a profile in config.toml"
            ));
        };
        if !reviewer.enabled {
            return Err(format!(
                "turn review needs an enabled reviewer: [review] profile {profile:?} is disabled"
            ));
        }
        validate_reviewer_assignment(
            session_id,
            controller.state.sessions.get(session_id),
            profile,
        )
    }

    fn is_subagent(&self, session_id: &str) -> bool {
        crate::database::load_subagent(session_id).is_ok_and(|record| record.is_some())
    }

    fn resolve<'a>(
        &'a self,
        handle: ManagedSessionHandle,
        config: ReviewConfig,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    > {
        Box::pin(async move {
            let specialists = config.tier == ReviewTier::Extended;
            crate::review_selection::resolve(handle, Some(config), specialists, cancelled)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn stage(
        &self,
        session_id: &str,
        profile: &str,
        generation: u64,
        mcp_servers: &[mj_core::worker_launch::ReviewMcpServer],
        dispatch_tool: bool,
    ) -> Result<mj_core::worker_launch::ReviewerLaunchConfig, String> {
        let controller =
            crate::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        controller
            .stage_reviewer_profile_with_mcp(
                session_id,
                profile,
                generation,
                mcp_servers,
                dispatch_tool,
            )
            .map_err(|error| format!("{error:#}"))
    }

    fn load_state(&self, session_id: &str) -> Result<TurnReviewState, String> {
        crate::database::turn_review_state(session_id).map_err(|error| format!("{error:#}"))
    }

    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String> {
        crate::database::save_turn_review_state(session_id, state)
            .map_err(|error| format!("{error:#}"))
    }

    fn clear_interrupted(&self) -> Result<Vec<String>, String> {
        crate::database::clear_interrupted_turn_reviews().map_err(|error| format!("{error:#}"))
    }

    fn background_work_settled<'a>(
        &'a self,
        session_id: &'a str,
        deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, ()> {
        Box::pin(async move {
            let Some(gate) = &self.background else {
                return;
            };
            let mut busy = gate.subscribe();
            // A deadline that passes leaves the review to try anyway; its
            // own refusal then says what held the session.
            let _ = tokio::time::timeout_at(
                deadline,
                busy.wait_for(|sessions| !sessions.contains(session_id)),
            )
            .await;
        })
    }
}

pub(crate) fn validate_reviewer_assignment(
    session_id: &str,
    session: Option<&mj_core::state::SessionRecord>,
    _profile: &str,
) -> Result<(), String> {
    let Some(session) = session else {
        return Err(format!(
            "session {session_id:?} is not in the controller store"
        ));
    };
    if session.archived {
        return Err("this session is archived".to_owned());
    }
    Ok(())
}
