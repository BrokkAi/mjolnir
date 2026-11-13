//! Shared, display-only projections for live session and lifecycle views.
//!
//! The dashboard and workspace views consume the same bounded runtime
//! snapshots.  Keeping these setters here prevents either surface from
//! turning a display update into persistence or chat-side behavior.

use std::collections::BTreeMap;

use mj_controller::session_manager::ManagedSessionView;
use mj_tui::{DashboardState, SessionOperationKind};

use crate::daemon::{RuntimeLifecycleKind, RuntimeLifecycleView};

/// Apply the bounded activity and connectivity facts from one managed view.
///
/// The relay snapshot contains all of the live facts needed by both surfaces:
/// the current step clock, operational activity, and whether the relay can be
/// reached.  A missing snapshot deliberately clears activity to its default.
pub(crate) fn apply_session_activity(
    dashboard: &mut DashboardState,
    session_id: &str,
    view: &ManagedSessionView,
) {
    dashboard.set_session_attention_for(session_id, view.attention);
    let current_step_started_at_ms = view
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.operational.current_step_started_at_ms);
    dashboard.set_current_step_start(session_id, current_step_started_at_ms);
    dashboard.set_session_activity(
        session_id,
        view.snapshot.as_ref().map_or_else(
            mj_client::usage_format::SessionActivity::default,
            |snapshot| mj_client::usage_format::SessionActivity::of(&snapshot.operational),
        ),
    );
    dashboard.set_question_message(
        session_id,
        view.snapshot
            .as_ref()
            .filter(|_| view.attention == mj_core::attention::SessionAttention::NeedsInput)
            .and_then(|snapshot| snapshot.operational.question_message().map(str::to_owned)),
    );
    dashboard.set_session_connectivity(session_id, view.connected);
}

/// Apply one worker report and hand the open conversations the sub-agent
/// counts it can change.
///
/// A child's worker reporting a turn changes no record, so the parent's
/// composer footer (`Subagents · N`) would otherwise keep the count from the
/// last record change and read `0/M` for the child's whole turn (I2-7).
pub(crate) fn apply_worker_activity(
    dashboard: &mut DashboardState,
    chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>,
    session_id: &str,
    view: &ManagedSessionView,
) {
    apply_session_activity(dashboard, session_id, view);
    crate::dashboard::refresh_subagent_counts(chats, dashboard);
}

/// Map a daemon lifecycle to the operation kind rendered by a surface.
pub(crate) const fn lifecycle_kind(kind: RuntimeLifecycleKind) -> SessionOperationKind {
    match kind {
        RuntimeLifecycleKind::Create => SessionOperationKind::Launching,
        RuntimeLifecycleKind::Suspend | RuntimeLifecycleKind::ForceStop => {
            SessionOperationKind::Suspending
        }
        RuntimeLifecycleKind::Resume => SessionOperationKind::Resuming,
        RuntimeLifecycleKind::Move => SessionOperationKind::Moving,
        RuntimeLifecycleKind::DestroyStopped
        | RuntimeLifecycleKind::ForceDestroy
        | RuntimeLifecycleKind::Cleanup => SessionOperationKind::Destroying,
        RuntimeLifecycleKind::StopSubagent => SessionOperationKind::Stopping,
    }
}

/// Project one daemon lifecycle into the display operation overlay.
///
/// This helper intentionally has no notice, persistence, or chat behavior.
/// The caller owns those surface-specific concerns and may skip this helper
/// when a local lifecycle operation takes precedence over the daemon view.
pub(crate) fn apply_lifecycle_display(
    dashboard: &mut DashboardState,
    lifecycle: &RuntimeLifecycleView,
) {
    let kind = lifecycle_kind(lifecycle.kind);
    if dashboard.session_operation_kind(&lifecycle.session_id) != Some(kind) {
        dashboard.begin_session_operation_at_with_id(
            lifecycle.session_id.clone(),
            kind,
            None,
            lifecycle.started_at_epoch_seconds,
            Some(lifecycle.operation_id.clone()),
            lifecycle.cancellable,
        );
    } else {
        dashboard.set_session_operation_identity(
            &lifecycle.session_id,
            Some(lifecycle.operation_id.clone()),
            lifecycle.cancellable,
        );
    }
    dashboard.replace_session_operation_stages(
        &lifecycle.session_id,
        lifecycle.active_stages.iter().cloned(),
    );
    if let Some((profile_id, target_id)) = lifecycle.resume_destination.as_ref() {
        dashboard.set_resume_destination(
            &lifecycle.session_id,
            profile_id.clone(),
            target_id.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use mj_core::config::{Config, HarnessKind, HarnessProfile, TargetTemplate};
    use mj_core::state::{
        ManagedSessionSnapshot, MaterializedExecutionState, MaterializedSession, ProjectionWindow,
        SessionRecord, SessionState, State,
    };

    use mj_controller::session_manager::ManagedSessionView;
    use mj_core::relay::{RELAY_EVENT_GENESIS_DIGEST, RelayExecutionState, RelayOperationalState};
    use mj_tui::{DashboardState, PaneSize, SupportPane, render_combined_with_theme};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::{apply_session_activity, apply_worker_activity};

    fn dashboard() -> DashboardState {
        let (config, state) = dashboard_parts();
        DashboardState::new(config, state, BTreeMap::new())
    }

    fn dashboard_parts() -> (Config, State) {
        let mut config = Config::default();
        config.profiles.insert(
            "profile-1".into(),
            HarnessProfile {
                ..mj_core::config::HarnessProfile::new(
                    HarnessKind::Codex,
                    PathBuf::from("/profiles/profile-1"),
                )
            },
        );
        config
            .targets
            .insert("target-1".into(), TargetTemplate::LocalBare);
        config.bundles.insert(
            "bundle-1".into(),
            mj_core::config::ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![mj_core::config::ProjectRepository {
                    id: "project".into(),
                    github: Some("owner/project".into()),
                    local: None,
                    destination: "project".into(),
                    git_ref: None,
                }],
            },
        );
        let mut state = State::default();
        state.sessions.insert(
            "session-1".into(),
            SessionRecord {
                oracle_choice: None,
                oracle: true,
                project: None,
                target_runtime: None,
                launch_base: None,
                launch_branch: None,
                checkout: None,
                publication: None,
                build_cache: None,
                container_workspace: None,
                subagents: None,
                create_managed_worktree: None,
                id: "session-1".into(),
                workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.into(),
                title: "Presentation test".into(),
                harness_kind: HarnessKind::Codex,
                last_profile: "profile-1".into(),
                bundle_id: "bundle-1".into(),
                project_directory: None,
                managed_worktree: None,
                review: None,
                no_project_memory: false,
                no_github_events: false,
                no_github_credentials: false,
                target_template_id: "target-1".into(),
                resource_allocation: None,
                additional_mounts: Vec::new(),
                container_cpus: None,
                container_memory: None,
                state: SessionState::Running,
                archived: false,
                target: None,
                native_session_id: None,
                acp_session_title: None,
                session_title_override: None,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                viewed_through_event_ordinal: 0,
                draft_input: String::new(),
                last_error: None,
                last_checkpoint_error: None,
                checkpoint: None,
            },
        );
        (config, state)
    }

    fn operational(
        execution: RelayExecutionState,
        current_step_started_at_ms: Option<i64>,
    ) -> RelayOperationalState {
        RelayOperationalState {
            assessment: None,
            open_request: None,
            person_input_through: 0,
            assessment_context: None,
            turn_completion: None,
            continuation: Default::default(),
            relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
            native_agents: Vec::new(),
            steering: None,
            cancelling_prompt_id: None,
            clear_context: false,
            clear_context_started_at_ms: None,
            native_agent_count: 0,
            expected_continuation: None,
            inferred_idle_since_ms: None,
            task_settled_at_ms: None,
            background_needed: None,
            goal: Default::default(),
            capacity_retry: None,
            retry_assessment_pending: false,
            activity_turn_started_at_ms: None,
            idle_since_ms: None,
            tools_in_flight: Vec::new(),
            activity: None,
            session_id: "session-1".into(),
            execution,
            latest_ordinal: 0,
            latest_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            acknowledged_through: 0,
            acknowledged_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            store_id: None,
            native_session_id: None,
            native_continuity_lost: false,
            replaced_unused_native_session_id: None,
            checkpoint_only: false,
            acp_ready: None,
            harness_preparation: None,
            agent_capabilities: None,
            agent_info: None,
            runtime: None,
            steering_supported: None,
            mailbox_delivery: None,
            turn_controls: None,
            config_options: Vec::new().into(),
            modes: None,
            available_commands: Vec::new().into(),
            config: BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: None,
            checkpoint_ready: None,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms,
            foreground_tool_started_at_ms: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
            background_work_known: None,
        }
    }

    fn managed_view(operational: RelayOperationalState, connected: bool) -> ManagedSessionView {
        let materialized = MaterializedSession {
            active_turn: None,
            last_turn_outcome: None,
            session_id: "session-1".into(),
            applied_event_ordinal: 0,
            applied_event_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            last_activity_at_ms: None,
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: Default::default(),
            transcript: Vec::new(),
            queued_prompts: Vec::new(),
            pending_elicitations: Vec::new(),
        };
        ManagedSessionView {
            attention: mj_core::attention::SessionAttention::Unreachable,
            snapshot: Some(ManagedSessionSnapshot {
                window: ProjectionWindow::of(&materialized),
                materialized,
                operational: operational.into(),
                latest_credential_sync_signal: None,
                worker_build: None,
                subagent_requests: Default::default(),
                subagent_completions: Default::default(),
                subagent_queue_error: None,
            }),
            connected,
            error: None,
        }
    }

    fn dashboard_text(dashboard: &mut DashboardState) -> (String, Vec<Color>) {
        let mut terminal = Terminal::new(TestBackend::new(240, 30)).expect("test terminal");
        dashboard.set_pane_size(SupportPane::Sessions, PaneSize::Maximized);
        terminal
            .draw(|frame| {
                render_combined_with_theme(
                    frame,
                    dashboard,
                    &mut BTreeMap::new(),
                    &BTreeMap::new(),
                    false,
                    mj_core::config::UiTheme::Midnight,
                );
            })
            .expect("render dashboard");
        let buffer = terminal.backend().buffer();
        let text = (buffer.area.y..buffer.area.bottom())
            .map(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let colors = (buffer.area.y..buffer.area.bottom())
            .flat_map(|y| (buffer.area.x..buffer.area.right()).map(move |x| buffer[(x, y)].fg))
            .collect();
        (text, colors)
    }

    #[test]
    fn activity_projection_retains_open_request_target_across_later_assessments() {
        use mj_core::activity::verdict::CompletionEvidence;
        use mj_core::assessment::{Action, Status, TurnAssessment};

        let mut dashboard = dashboard();
        let mut operational = operational(RelayExecutionState::Idle, None);
        let mut assessment = TurnAssessment::pending(
            "turn-1".into(),
            1,
            1,
            CompletionEvidence {
                stop_reason: "EndTurn".into(),
                diagnostic: None,
            },
        );
        assessment.status = Status::Assessed;
        assessment.action = Some(Action::AwaitInput);
        assessment.question_message = Some("agent:msg_1".into());
        operational.open_request = Some(mj_core::assessment::OpenRequest {
            question: assessment.inferred_question().unwrap(),
            question_message: assessment.question_message.clone(),
        });
        operational.assessment = Some(std::sync::Arc::new(assessment.clone()));
        let mut view = managed_view(operational, true);
        view.attention = mj_core::attention::SessionAttention::NeedsInput;
        apply_session_activity(&mut dashboard, "session-1", &view);
        assert_eq!(dashboard.question_message("session-1"), Some("agent:msg_1"));

        assessment.turn_id = "later-turn".into();
        assessment.completed_ordinal = 2;
        assessment.action = Some(Action::Finished);
        assessment.question_message = None;
        view.snapshot.as_mut().unwrap().operational.assessment =
            Some(std::sync::Arc::new(assessment));
        apply_session_activity(&mut dashboard, "session-1", &view);
        assert_eq!(dashboard.question_message("session-1"), Some("agent:msg_1"));

        view.attention = mj_core::attention::SessionAttention::Working;
        apply_session_activity(&mut dashboard, "session-1", &view);
        assert_eq!(dashboard.question_message("session-1"), None);

        view.attention = mj_core::attention::SessionAttention::Idle;
        view.snapshot.as_mut().unwrap().operational.open_request = None;
        apply_session_activity(&mut dashboard, "session-1", &view);
        assert_eq!(dashboard.question_message("session-1"), None);
    }

    #[test]
    fn activity_projection_captures_current_step_and_connectivity() {
        let mut dashboard = dashboard();
        let mut view = managed_view(
            operational(RelayExecutionState::Running, Some(1_700_000_000_000)),
            false,
        );

        apply_session_activity(&mut dashboard, "session-1", &view);

        let (text, colors) = dashboard_text(&mut dashboard);
        assert!(
            text.contains("Unreachable"),
            "connection status missing from {text:?}"
        );
        assert!(
            !text.contains("Working "),
            "stale activity must not hide the connection failure"
        );
        assert!(
            colors.contains(&mj_chat::theme::palette().error),
            "disconnected row was not marked red"
        );
        view.connected = true;
        view.attention = mj_core::attention::SessionAttention::Working;
        apply_session_activity(&mut dashboard, "session-1", &view);
        let (text, _) = dashboard_text(&mut dashboard);
        assert!(
            text.contains("Working "),
            "live SDK step clock missing from {text:?}"
        );
        assert!(!text.contains("Unreachable"));
    }

    /// I2-7: the composer footer read `Subagents · 0/3` for a child's whole
    /// turn, because the working count was refreshed only with records.
    // Hard-won: 6b8110e29c: Launch finding I2-7 left the parent footer at 0/M through a child turn; this checks a child worker report refreshes the displayed count.
    #[tokio::test]
    async fn a_child_worker_report_updates_the_parent_footer_working_count() {
        let (config, mut state) = dashboard_parts();
        let mut dashboard = DashboardState::new(config, state.clone(), BTreeMap::new());
        let mut child = state.sessions["session-1"].clone();
        child.id = "child-1".into();
        child.title = "child".into();
        state.subagents.insert(
            child.id.clone(),
            mj_core::subagent::SubagentMetadata {
                kind: mj_core::subagent::SubagentKind::Worker,
                child_session_id: child.id.clone(),
                parent_session_id: "session-1".into(),
                task_name: "Inspect parser".into(),
                profile_id: child.last_profile.clone(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                request_key: "request-1".into(),
                created_at: child.created_at.clone(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: false,
            },
        );
        state.sessions.insert(child.id.clone(), child);
        dashboard.set_state(state);
        let fixture = mj_client::session::replacement_session_test_fixture("session-1", 1);
        let mut chats = BTreeMap::from([(
            "session-1".to_owned(),
            mj_chat::chat::ActiveChat::open(
                fixture.stopped,
                "bundle-1",
                None,
                fixture.control,
                mj_chat::chat::SessionHeaderIdentity::default(),
                String::new(),
                mj_chat::chat::Notices::default(),
            ),
        )]);
        crate::dashboard::refresh_subagent_counts(&mut chats, &dashboard);
        assert_eq!(chats["session-1"].subagent_working_count(), 0);

        let running = managed_view(
            operational(RelayExecutionState::Running, Some(1_700_000_000_000)),
            true,
        );
        apply_worker_activity(&mut dashboard, &mut chats, "child-1", &running);

        assert_eq!(chats["session-1"].subagent_working_count(), 1);
    }
}
