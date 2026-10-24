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

    /// Whether this session is a Mjolnir-managed sub-agent. Its changes are
    /// reviewed through the parent's turn, so it is never reviewed on its
    /// own. Blocking: it reads the controller's database.
    fn is_subagent(&self, _session_id: &str) -> bool {
        false
    }

    /// Refuses a session whose project source cannot be reviewed even before
    /// asking the worker to capture a delta. Blocking: production reads the
    /// accepted project snapshot from the controller database.
    fn review_refusal(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
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

    fn register_child(
        &self,
        session_id: &str,
        reviewer: &ReviewerIdentity,
        prompt: String,
        request_key: String,
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
    /// on the Tokio task that owns review state.
    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String>;

    /// Clears the in-flight flag of every review a restart interrupted, and
    /// reports whose they were. Baselines are deliberately left alone: the
    /// interrupted review never advanced one, so the next review covers the
    /// same change and nothing is lost.
    fn clear_interrupted(&self) -> Result<Vec<String>, String>;

    fn enqueue_verdict(
        &self,
        _session_id: &str,
        _event: &mj_core::mailbox::MailboxEvent,
    ) -> Result<(), String> {
        Err("review verdict mailbox is unavailable".into())
    }

    fn orphaned_children(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

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
    pub(super) runtime: SharedReviewerRuntime,
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

pub(crate) type SharedReviewerRuntime =
    Arc<std::sync::OnceLock<std::sync::Weak<crate::daemon::RuntimeState>>>;

impl ControllerEnvironment {
    async fn runtime(&self) -> Result<Arc<crate::daemon::RuntimeState>, String> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(runtime) = self.runtime.get() {
                    return runtime
                        .upgrade()
                        .ok_or_else(|| "reviewer runtime stopped".to_owned());
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| "reviewer runtime did not start".to_owned())?
    }
}

impl ReviewEnvironment for ControllerEnvironment {
    fn review_refusal(&self, session_id: &str) -> Result<(), String> {
        let controller =
            crate::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        let session = controller
            .state
            .sessions
            .get(session_id)
            .ok_or_else(|| format!("unknown session {session_id:?}"))?;
        let recorded_plain = session
            .project
            .as_ref()
            .is_some_and(mj_core::repository::ProjectBundleSnapshot::primary_is_plain_directory);
        // Sessions created before plain directories became an accepted project
        // shape have no snapshot to consult. Classify their current directory
        // when they still name a local bare target, so they cannot silently
        // bypass review either.
        let legacy_plain = session.project.is_none()
            && matches!(
                controller.config.targets.get(&session.target_template_id),
                Some(mj_core::config::TargetTemplate::LocalBare)
            )
            && if let Some(directory) = session.project_directory.as_deref() {
                mj_core::repository::resolve_local_directory(
                    directory,
                    &crate::targets::BoundedProcessExecutor::new(std::time::Duration::from_secs(5)),
                )
                .map_err(|error| format!("{error:#}"))?
                .plain_directory
            } else {
                false
            };
        if recorded_plain || legacy_plain {
            return Err(
                "this session works in a directory without a committed Git checkout, so Mjolnir cannot capture changes to review"
                    .into(),
            );
        }
        Ok(())
    }

    fn enqueue_verdict(
        &self,
        session_id: &str,
        event: &mj_core::mailbox::MailboxEvent,
    ) -> Result<(), String> {
        let json = serde_json::to_string(event).map_err(|error| error.to_string())?;
        crate::database::enqueue_mailbox_event(&event.key, session_id, &json, true, false)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }

    fn orphaned_children(&self) -> Result<Vec<String>, String> {
        let controller =
            crate::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        Ok(controller
            .state
            .subagents
            .values()
            .filter(|child| child.kind == mj_core::subagent::SubagentKind::Reviewer)
            .filter(|child| {
                controller
                    .state
                    .sessions
                    .get(&child.child_session_id)
                    .is_some_and(|session| session.state.is_active())
            })
            .map(|child| child.child_session_id.clone())
            .collect())
    }
    fn register_child(
        &self,
        session_id: &str,
        reviewer: &ReviewerIdentity,
        prompt: String,
        request_key: String,
    ) -> Result<String, String> {
        let mut controller = crate::controller::Controller::load().map_err(|e| format!("{e:#}"))?;
        let mut launch = controller
            .reviewer_launch_config(session_id, &reviewer.profile, 0, &[])
            .map_err(|e| format!("{e:#}"))?;
        launch.model = reviewer.main.model.clone();
        launch.effort = reviewer.main.effort.clone();
        launch.fast_mode = reviewer.main.fast_mode.then_some(true);
        controller
            .register_subagent(crate::controller::RegisterSubagentRequest {
                kind: mj_core::subagent::SubagentKind::Reviewer,
                reviewer_launch: Some(launch),
                parent_session_id: session_id.to_owned(),
                task_name: "review".to_owned(),
                profile_id: reviewer.profile.clone(),
                model: reviewer.main.model.clone(),
                effort: reviewer.main.effort.clone(),
                working_directory: Default::default(),
                initial_prompt: prompt,
                request_key,
                report_root: None,
            })
            .map(|child| child.child_session_id)
            .map_err(|e| format!("{e:#}"))
    }

    fn start_child<'a>(
        &'a self,
        child: &'a str,
    ) -> mj_client::session::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let runtime = self.runtime().await?;
            runtime
                .start_reviewer_child(child)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn close_child<'a>(
        &'a self,
        child: &'a str,
    ) -> mj_client::session::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let runtime = self.runtime().await?;
            runtime
                .suspend_session(child.to_owned())
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

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
        validate_review_profile(
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
            let catalog = self
                .catalog
                .as_ref()
                .and_then(|catalog| catalog.get())
                .cloned();
            crate::review_selection::resolve(handle, Some(config), cancelled, catalog)
                .await
                .map_err(|e| format!("{e:#}"))
        })
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

pub(crate) fn validate_review_profile(
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
