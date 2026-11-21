//! The review host's environment in the daemon: reviews run against the
//! controller's store and the daemon's own runtime state.

use super::*;
use mj_controller::database::TurnReviewState;
use mj_controller::session_manager::ManagedSessionHandle;
use mj_core::config::ReviewConfig;

/// The production environment: the controller as it is on disk right now.
///
/// It is reloaded per call rather than held, because a review is rare and the
/// answer must reflect the config as it stands when the review starts -- the
/// daemon reloads config.toml every 500 ms for the same reason.
#[derive(Default)]
pub struct ControllerEnvironment {
    pub(super) manager: Option<SessionManagerControl>,
    pub(super) runtime: SharedReviewerRuntime,
    /// The daemon's gate for recovery copies and worker upgrades, which a
    /// review waits behind instead of racing for the session's lease.
    pub background: Option<Arc<mj_controller::recovery_gate::RecoveryGate>>,
}

/// The daemon installs its profile catalog after the review host starts.
pub(crate) type SharedProfileCatalog =
    Arc<std::sync::OnceLock<Arc<mj_controller::profile_catalog::ProfileCatalog>>>;

pub(crate) type SharedReviewerRuntime =
    Arc<std::sync::OnceLock<std::sync::Weak<crate::daemon::RuntimeState>>>;

impl ControllerEnvironment {
    /// The daemon's reviewer profile service, once it has started.
    async fn profile_backend(&self) -> Result<Arc<crate::server_runtime::api::ApiBackend>, String> {
        let runtime = self.runtime().await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(backend) = runtime.report_backend.get() {
                    return backend
                        .upgrade()
                        .ok_or_else(|| "reviewer profile service stopped".to_owned());
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| "reviewer profile service did not start".to_owned())?
    }

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
            mj_controller::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        let session = controller
            .state
            .sessions
            .get(session_id)
            .ok_or_else(|| format!("unknown session {session_id:?}"))?;
        let recorded_plain = session.review_unavailable().is_some();
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
                    &mj_controller::targets::BoundedProcessExecutor::new(
                        std::time::Duration::from_secs(5),
                    ),
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
        // A sub-agent that handed back and parked while its review ran is
        // woken by the verdict; a top-level session is never parked.
        let unpark = mj_controller::database::load_subagent(session_id)
            .map_err(|error| format!("{error:#}"))?
            .is_some();
        mj_controller::database::enqueue_mailbox_event(&event.key, session_id, &json, true, unpark)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }

    fn reviewer_children(&self) -> Result<Vec<ReviewerChildRelation>, String> {
        let controller =
            mj_controller::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        Ok(controller
            .state
            .subagents
            .values()
            .filter(|child| child.kind == mj_core::subagent::SubagentKind::Reviewer)
            .map(|child| ReviewerChildRelation {
                parent_session_id: child.parent_session_id.clone(),
                request_key: child.kind.request_key(&child.request_key),
                child_session_id: child.child_session_id.clone(),
            })
            .collect())
    }

    fn reviewer_child_standing(&self, child: &str) -> Result<ChildStanding, String> {
        let ended = match mj_controller::database::load_session_record(child)
            .map_err(|error| format!("{error:#}"))?
        {
            None => Some("removed".to_owned()),
            Some(record) if mj_controller::session_manager::session_has_ended(record.state) => {
                Some(match record.last_error {
                    Some(cause) => format!("{}: {cause}", record.state.as_str()),
                    None => record.state.as_str().to_owned(),
                })
            }
            Some(_) => None,
        };
        let handback = mj_controller::database::load_subagent_report(child)
            .map_err(|error| format!("{error:#}"))?
            .handback;
        Ok(ChildStanding { ended, handback })
    }

    fn session_exists(&self, session_id: &str) -> Result<bool, String> {
        mj_controller::database::load_session_record(session_id)
            .map(|record| record.is_some())
            .map_err(|error| format!("{error:#}"))
    }

    fn register_child(
        &self,
        session_id: &str,
        reviewer: &ReviewerIdentity,
        role: mj_core::worker_launch::ReviewChildRole,
        prompt: String,
        request_key: String,
        working_directory: Option<std::path::PathBuf>,
    ) -> Result<String, String> {
        let mut controller =
            mj_controller::controller::Controller::load().map_err(|e| format!("{e:#}"))?;
        let mut launch = controller
            .reviewer_launch_config(session_id, &reviewer.profile, 0, &[])
            .map_err(|e| format!("{e:#}"))?;
        launch.role = role;
        launch.model = reviewer.main.model.clone();
        launch.effort = reviewer.main.effort.clone();
        launch.fast_mode = reviewer.main.fast_mode.then_some(true);
        controller
            .register_subagent_with_manager(
                mj_controller::controller::RegisterSubagentRequest {
                    no_github_credentials: false,
                    kind: mj_core::subagent::SubagentKind::Reviewer,
                    reviewer_launch: Some(launch),
                    parent_session_id: session_id.to_owned(),
                    task_name: "review".to_owned(),
                    profile_id: reviewer.profile.clone(),
                    model: reviewer.main.model.clone(),
                    effort: reviewer.main.effort.clone(),
                    fast_mode: reviewer.main.fast_mode,
                    working_directory: working_directory.unwrap_or_default(),
                    initial_prompt: prompt,
                    request_key,
                    report_root: None,
                    category: match role {
                        mj_core::worker_launch::ReviewChildRole::Reviewer => "reviewer",
                        mj_core::worker_launch::ReviewChildRole::BriefWriter => "brief_writer",
                    }
                    .to_owned(),
                },
                self.manager
                    .as_ref()
                    .ok_or("review session manager is unavailable")?,
            )
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
            // A child that is already gone is closed; retrying an unknown
            // session would never end.
            if runtime.session_record(child).is_none() {
                return Ok(());
            }
            // A reviewer's verdict lives in the review's own state, and its
            // usage in the accounting table, so nothing needs the child's
            // session once its role has an outcome. Destroying it, rather
            // than suspending it, keeps finished reviewers from piling up as
            // sub-agents of the session that asked for the review.
            runtime
                .destroy_reviewer_child(child.to_owned())
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn check(&self, session_id: &str, profile: &str) -> Result<(), String> {
        let controller =
            mj_controller::controller::Controller::load().map_err(|error| format!("{error:#}"))?;
        let Some(reviewer) = controller.config.profiles.get(profile) else {
            return Err(format!(
                "code review needs a reviewer: [review] profile {profile:?} is not a profile in config.toml"
            ));
        };
        if !reviewer.enabled {
            return Err(format!(
                "code review needs an enabled reviewer: [review] profile {profile:?} is disabled"
            ));
        }
        validate_review_profile(
            session_id,
            controller.state.sessions.get(session_id),
            profile,
        )
    }

    fn resolve<'a>(
        &'a self,
        handle: ManagedSessionHandle,
        config: ReviewConfig,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<Vec<mj_core::review::settings::ResolvedReviewSettings>, String>,
    > {
        Box::pin(async move {
            let backend = self.profile_backend().await?;
            super::selection::resolve(handle, Some(config), backend)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn resolve_choice<'a>(
        &'a self,
        choice: mj_core::config::SubagentChoice,
    ) -> mj_client::session::BoxFuture<
        'a,
        Result<mj_core::review::settings::ResolvedReviewSettings, String>,
    > {
        Box::pin(async move {
            let backend: Arc<dyn crate::server::api::SubagentBackend> =
                self.profile_backend().await?;
            super::selection::resolve_choice(&backend, choice)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn load_state(&self, session_id: &str) -> Result<TurnReviewState, String> {
        mj_controller::database::turn_review_state(session_id).map_err(|error| format!("{error:#}"))
    }

    fn save_state(&self, session_id: &str, state: &TurnReviewState) -> Result<(), String> {
        mj_controller::database::save_turn_review_state(session_id, state)
            .map_err(|error| format!("{error:#}"))
    }

    fn save_reviewer_outcome(
        &self,
        session_id: &str,
        state: &TurnReviewState,
        reports: &[mj_controller::database::AcceptedReport],
    ) -> Result<(), String> {
        mj_controller::database::save_reviewer_outcome(session_id, state, reports.to_vec())
            .map_err(|error| format!("{error:#}"))
    }

    fn close_review(
        &self,
        session_id: &str,
        review_id: &str,
        state: &TurnReviewState,
        resolution: mj_controller::database::FinishedReviewResolution,
    ) -> Result<(), String> {
        mj_controller::database::close_turn_review_state(session_id, review_id, state, resolution)
            .map_err(|error| format!("{error:#}"))
    }

    fn clear_interrupted(&self) -> Result<Vec<String>, String> {
        mj_controller::database::clear_interrupted_turn_reviews()
            .map_err(|error| format!("{error:#}"))
    }

    fn active_groups(&self) -> Result<Vec<(String, TurnReviewState)>, String> {
        mj_controller::database::active_review_groups().map_err(|error| format!("{error:#}"))
    }

    fn hold_background_work<'a>(
        &'a self,
        session_id: &'a str,
        _deadline: tokio::time::Instant,
    ) -> mj_client::session::BoxFuture<'a, Option<BackgroundHold>> {
        Box::pin(async move {
            let gate = self.background.as_ref()?;
            // The caller bounds this wait by its preparation deadline. On
            // expiry it drops the pending reservation and reports a refusal.
            let hold: BackgroundHold = Box::new(gate.reserve_when_idle(session_id).await);
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
