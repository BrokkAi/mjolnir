//! Shared, display-only projections for live session and lifecycle views.
//!
//! The dashboard and the workspace selector consume the same bounded runtime
//! snapshots.  Keeping these setters here prevents either surface from
//! turning a display update into persistence or chat-side behavior.

use hel_tui::{DashboardState, SessionOperationKind};
use mj_controller::hel_session_manager::ManagedSessionView;

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
    let current_step_started_at_ms = view
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.operational.current_step_started_at_ms);
    dashboard.set_current_step_start(session_id, current_step_started_at_ms);
    dashboard.set_session_activity(
        session_id,
        view.snapshot.as_ref().map_or_else(
            mj_chat::usage_format::SessionActivity::default,
            |snapshot| mj_chat::usage_format::SessionActivity::of(&snapshot.operational),
        ),
    );
    dashboard.set_session_connectivity(session_id, view.connected);
}

/// Map a daemon lifecycle to the operation kind rendered by a surface.
pub(crate) const fn lifecycle_kind(kind: RuntimeLifecycleKind) -> SessionOperationKind {
    match kind {
        RuntimeLifecycleKind::Create => SessionOperationKind::Launching,
        RuntimeLifecycleKind::Close | RuntimeLifecycleKind::ForceStop => {
            SessionOperationKind::Stopping
        }
        RuntimeLifecycleKind::Resume => SessionOperationKind::Resuming,
        RuntimeLifecycleKind::DestroyStopped
        | RuntimeLifecycleKind::ForceDestroy
        | RuntimeLifecycleKind::Cleanup => SessionOperationKind::Destroying,
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
        dashboard.begin_session_operation_at(
            lifecycle.session_id.clone(),
            kind,
            None,
            lifecycle.started_at_epoch_seconds,
        );
    }
    dashboard.replace_session_operation_stages(
        &lifecycle.session_id,
        lifecycle.active_stages.iter().copied(),
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

    use hel::hel_config::{HarnessKind, HarnessProfile, HelConfig, TargetTemplate};
    use hel::hel_state::{
        HelState, ManagedSessionSnapshot, MaterializedExecutionState, MaterializedSession,
        ProjectionWindow, SessionRecord, SessionState,
    };
    use hel::hel_targets::ProvisionStage;
    use hel::hel_worker::{RELAY_EVENT_GENESIS_DIGEST, RelayExecutionState, RelayOperationalState};
    use hel_tui::{
        DashboardState, SessionOperationKind, SessionsPreviewState, render_sessions_preview,
    };
    use mj_controller::hel_session_manager::ManagedSessionView;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    use super::{apply_lifecycle_display, apply_session_activity, lifecycle_kind};
    use crate::daemon::{RuntimeLifecycleKind, RuntimeLifecycleView};

    fn dashboard() -> DashboardState {
        let mut config = HelConfig::default();
        config.profiles.insert(
            "profile-1".into(),
            HarnessProfile {
                kind: HarnessKind::Codex,
                home: PathBuf::from("/profiles/profile-1"),
                environment: BTreeMap::new(),
                context_window_bytes: None,
            },
        );
        config
            .targets
            .insert("target-1".into(), TargetTemplate::LocalBare);
        let mut state = HelState::default();
        state.sessions.insert(
            "session-1".into(),
            SessionRecord {
                id: "session-1".into(),
                workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.into(),
                title: "Presentation test".into(),
                harness_kind: HarnessKind::Codex,
                last_profile: "profile-1".into(),
                bundle_id: "bundle-1".into(),
                project_directory: None,
                managed_worktree: None,
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
        DashboardState::new(config, state, BTreeMap::new())
    }

    fn operational(
        execution: RelayExecutionState,
        current_step_started_at_ms: Option<i64>,
    ) -> RelayOperationalState {
        RelayOperationalState {
            session_id: "session-1".into(),
            execution,
            latest_ordinal: 0,
            latest_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            acknowledged_through: 0,
            acknowledged_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            native_session_id: None,
            agent_capabilities: None,
            agent_info: None,
            config_options: Vec::new(),
            modes: None,
            available_commands: Vec::new(),
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
        }
    }

    fn managed_view(operational: RelayOperationalState, connected: bool) -> ManagedSessionView {
        let materialized = MaterializedSession {
            session_id: "session-1".into(),
            applied_event_ordinal: 0,
            applied_event_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            last_activity_at_ms: None,
            execution: MaterializedExecutionState::Idle,
            session_title: None,
            configuration: BTreeMap::new(),
            transcript: Vec::new(),
            queued_prompts: Vec::new(),
            pending_elicitations: Vec::new(),
        };
        ManagedSessionView {
            snapshot: Some(ManagedSessionSnapshot {
                window: ProjectionWindow::of(&materialized),
                materialized,
                operational,
                latest_credential_sync_signal: None,
                worker_build: None,
            }),
            connected,
            error: None,
        }
    }

    fn preview_text(dashboard: &DashboardState) -> (String, Vec<Color>) {
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).expect("test terminal");
        let mut preview = SessionsPreviewState::default();
        terminal
            .draw(|frame| {
                render_sessions_preview(frame, Rect::new(0, 0, 120, 12), dashboard, &mut preview);
            })
            .expect("render session preview");
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

    fn lifecycle(kind: RuntimeLifecycleKind) -> RuntimeLifecycleView {
        RuntimeLifecycleView {
            session_id: "session-1".into(),
            kind,
            started_at_epoch_seconds: 123,
            active_stages: vec![(ProvisionStage::Booting, 124)],
            resume_destination: Some(("profile-2".into(), "target-2".into())),
            notice: Some("daemon notice".into()),
        }
    }

    #[test]
    fn activity_projection_captures_current_step_and_connectivity() {
        let mut dashboard = dashboard();
        let view = managed_view(
            operational(RelayExecutionState::Running, Some(1_700_000_000_000)),
            false,
        );

        apply_session_activity(&mut dashboard, "session-1", &view);

        let (text, colors) = preview_text(&dashboard);
        assert!(
            text.contains("Turn"),
            "activity clock missing from {text:?}"
        );
        assert!(text.contains("Step"), "step clock missing from {text:?}");
        assert!(
            colors.contains(&Color::Red),
            "disconnected row was not marked red"
        );
    }

    #[test]
    fn lifecycle_projection_maps_kind_and_replaces_a_changed_operation() {
        let mut dashboard = dashboard();
        let create = lifecycle(RuntimeLifecycleKind::Create);
        apply_lifecycle_display(&mut dashboard, &create);
        assert_eq!(
            dashboard.session_operation_kind("session-1"),
            Some(SessionOperationKind::Launching)
        );
        let (text, _) = preview_text(&dashboard);
        assert!(text.contains("Boot"), "stage missing from {text:?}");
        assert!(
            text.contains("profile-2"),
            "resume profile missing from {text:?}"
        );
        assert!(
            text.contains("target-2"),
            "resume target missing from {text:?}"
        );

        let resume = lifecycle(RuntimeLifecycleKind::Resume);
        apply_lifecycle_display(&mut dashboard, &resume);
        assert_eq!(
            dashboard.session_operation_kind("session-1"),
            Some(SessionOperationKind::Resuming)
        );
    }

    #[test]
    fn lifecycle_kinds_cover_stopping_and_destroying_variants() {
        assert_eq!(
            lifecycle_kind(RuntimeLifecycleKind::ForceStop),
            SessionOperationKind::Stopping
        );
        assert_eq!(
            lifecycle_kind(RuntimeLifecycleKind::Cleanup),
            SessionOperationKind::Destroying
        );
    }
}
