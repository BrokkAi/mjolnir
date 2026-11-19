use super::*;

/// The controller operations a review needs to select and run its child.
///
/// It is a trait so the host's own tests can drive a whole review without a
/// container, a harness, or the developer's own `config.toml`. The daemon
/// installs [`ControllerEnvironment`], which loads the real controller.
pub trait ReviewEnvironment: Send + Sync {
    /// Refuses, with a sentence for a person, when this session cannot be
    /// reviewed under `profile`.
    fn check(&self, session_id: &str, profile: &str) -> Result<(), String>;

    /// Refuses a session whose project source cannot be reviewed even before
    /// asking the worker to capture a delta. Blocking: production reads the
    /// accepted project snapshot from the controller database.
    fn review_refusal(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// Chooses the reviewers. A cancelled review drops the returned future.
    fn resolve<'a>(
        &'a self,
        handle: ManagedSessionHandle,
        config: ReviewConfig,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<Vec<mj_core::review::settings::ResolvedReviewSettings>, String>,
    >;

    /// Resolves one model choice, such as the brief writer's, to a profile.
    fn resolve_choice<'a>(
        &'a self,
        choice: mj_core::config::SubagentChoice,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    >;

    fn register_child(
        &self,
        session_id: &str,
        reviewer: &ReviewerIdentity,
        role: mj_core::worker_launch::ReviewChildRole,
        prompt: String,
        request_key: String,
        working_directory: Option<std::path::PathBuf>,
    ) -> Result<String, String>;

    fn start_child<'a>(
        &'a self,
        child: &'a str,
    ) -> mj_client::session::BoxFuture<'a, Result<(), String>>;

    fn close_child<'a>(
        &'a self,
        child: &'a str,
    ) -> mj_client::session::BoxFuture<'a, Result<(), String>>;

    /// How far this session has been reviewed. Blocking: it reads the
    /// controller's database.
    fn load_state(&self, session_id: &str) -> Result<TurnReviewState, String>;

    /// Records how far this session has been reviewed. Blocking: the host
    /// routes it through its ordered persistence lane rather than calling it
    /// on the Tokio task that owns review state. Newly settled findings and
    /// their partial mailbox messages commit together while peers remain.
    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String>;

    /// Records review state together with the text the host settled roles from.
    ///
    /// The host owns that decision, so the store must not re-derive it: a
    /// reviewer child's handback row can be replaced by a later command, and
    /// reading it here would pair unrelated text with a settled verdict.
    fn save_reviewer_outcome(
        &self,
        session_id: &str,
        state: &TurnReviewState,
        reports: &[mj_store::database::AcceptedReport],
    ) -> Result<(), String> {
        let _ = reports;
        self.save_state(session_id, state)
    }

    /// Clears an active review and records its per-role output in one durable
    /// write. The default keeps a test environment that has no store.
    fn close_review(
        &self,
        session_id: &str,
        state: &TurnReviewState,
        _resolution: mj_store::database::FinishedReviewResolution,
    ) -> Result<(), String> {
        self.save_state(session_id, state)
    }

    /// Clears the in-flight flag of every review a restart interrupted, and
    /// reports whose they were. Baselines are deliberately left alone: the
    /// interrupted review never advanced one, so the next review covers the
    /// same change and nothing is lost.
    fn clear_interrupted(&self) -> Result<Vec<String>, String>;

    /// Durable groups are resumed rather than swept as interrupted reviews.
    fn active_groups(&self) -> Result<Vec<(String, TurnReviewState)>, String> {
        Ok(Vec::new())
    }

    fn enqueue_verdict(
        &self,
        _session_id: &str,
        _event: &mj_core::mailbox::MailboxEvent,
    ) -> Result<(), String> {
        Err("review verdict mailbox is unavailable".into())
    }

    fn reviewer_children(&self) -> Result<Vec<ReviewerChildRelation>, String> {
        Ok(Vec::new())
    }

    /// What the store holds about one reviewer child: whether its session has
    /// ended, and the handback it recorded, if any. Blocking: production reads
    /// the controller's database.
    fn reviewer_child_standing(&self, _child: &str) -> Result<ChildStanding, String> {
        Ok(ChildStanding::default())
    }

    /// Whether the store still has this session's record. A destroyed
    /// session has none, and nothing about its reviews can be saved again.
    /// Blocking: production reads the controller's database.
    fn session_exists(&self, _session_id: &str) -> Result<bool, String> {
        Ok(true)
    }

    /// Holds the session against background work for as long as a review
    /// needs its worker: waits until no recovery copy or worker upgrade is
    /// running (bounded by the caller's review preparation deadline), then keeps new ones from starting until
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

/// The durable sub-agent index owns a child before its ID reaches review state.
#[derive(Clone)]
pub struct ReviewerChildRelation {
    pub parent_session_id: String,
    pub request_key: String,
    pub child_session_id: String,
}

/// What the store holds about a reviewer child, read at one moment.
#[derive(Clone, Debug, Default)]
pub struct ChildStanding {
    /// Why its session can no longer run, or `None` while it still can.
    pub ended: Option<String>,
    /// The report it handed back, whether or not the review has taken it yet.
    pub handback: Option<mj_core::subagent::SubagentHandback>,
}

impl ChildStanding {
    /// The handback text, when the report answers the turn `command_id`
    /// names: the same rule delegation applies to a finished reviewer turn.
    pub fn answer_to(self, command_id: &str) -> Option<String> {
        self.handback
            .filter(|handback| handback.answers(command_id))
            .map(|handback| handback.message)
    }
}

/// Keeps background work off a session while a review needs its worker.
/// Dropping it lets recovery copies and worker upgrades start again.
pub type BackgroundHold = Box<dyn Send + Sync>;
