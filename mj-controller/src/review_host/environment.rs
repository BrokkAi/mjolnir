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

    /// Holds the session against background work for as long as a review
    /// needs its worker: waits until no recovery copy or worker upgrade is
    /// running (up to `deadline`), then keeps new ones from starting until
    /// the returned hold is dropped.
    ///
    /// A recovery copy takes the session's lease, which cancels reviewer
    /// actions in flight (R4-9), and a checkpoint or worker replacement makes
    /// the worker refuse reviewer work. Both are started by the same finished
    /// turn that starts the review, so waiting for them to settle was not
    /// enough: one could start a moment later, while the review chose its
    /// reviewer (Series 34 #3444, 2026-10-03). The hold closes that window.
    /// An environment with no background work holds nothing.
    fn hold_background_work<'a>(
        &'a self,
        _session_id: &'a str,
        _deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, Option<BackgroundHold>> {
        Box::pin(async { None })
    }
}

/// Keeps background work off a session while a review needs its worker.
/// Dropping it lets recovery copies and worker upgrades start again.
pub type BackgroundHold = Box<dyn Send + Sync>;

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
    /// The daemon's profile catalog, once it is installed. A pinned review
    /// model is matched against it before any profile is staged.
    pub(crate) catalog: Option<SharedProfileCatalog>,
}

/// The daemon installs its profile catalog after the review host starts.
pub(crate) type SharedProfileCatalog =
    Arc<std::sync::OnceLock<Arc<crate::server_runtime::profile_catalog::ProfileCatalog>>>;

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
            let offered = self
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.get())
                .map(|catalog| catalog.snapshot());
            crate::review_selection::resolve(handle, Some(config), specialists, cancelled, offered)
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

    fn hold_background_work<'a>(
        &'a self,
        session_id: &'a str,
        deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, Option<BackgroundHold>> {
        Box::pin(async move {
            let gate = self.background.as_ref()?;
            // A deadline that passes leaves the review to try anyway; the
            // running work's own refusal then says what held the session.
            let hold: BackgroundHold = Box::new(gate.reserve_when_idle(session_id, deadline).await);
            Some(hold)
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
