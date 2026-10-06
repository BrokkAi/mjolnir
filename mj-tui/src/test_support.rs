//! Shared fixtures for the dashboard unit tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use mj_core::config::{
    CONFIG_VERSION, Config, ContainerTemplate, HarnessKind, HarnessProfile, ProjectBundle,
    ProjectRepository, TargetTemplate,
};
use mj_core::state::{
    CheckpointMetadata, MaterializedExecutionState, MaterializedSession, STATE_VERSION,
    SessionRecord, SessionState, State, TranscriptBody, TranscriptItem,
};

use mj_core::targets::{DeploymentCapacityKind, DeploymentCapacityTarget, ProvisionStage};

use mj_core::config::Trigger;

use crate::ingest::SessionOperationDisplay;
use crate::keybinds::{KeyRoute, combo_key_event};
use crate::{CommandId, DashboardAction, DashboardState, SessionOperationKind};

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The drawn buffer as one string per row.
pub(crate) use mj_chat::golden::buffer_lines;

pub(crate) fn assert_dialog_spacing(lines: &[String], title: &str, action: &str) {
    let cells: Vec<Vec<char>> = lines.iter().map(|line| line.chars().collect()).collect();
    let top = lines
        .iter()
        .position(|line| line.contains(title))
        .expect("dialog title");
    let title_column = lines[top][..lines[top].find(title).unwrap()]
        .chars()
        .count();
    let left = cells[top][..title_column]
        .iter()
        .rposition(|c| matches!(c, '╭' | '┌'))
        .expect("left border");
    let right = title_column
        + cells[top][title_column..]
            .iter()
            .position(|c| matches!(c, '╮' | '┐'))
            .expect("right border");
    let bottom = (top + 1..cells.len())
        .find(|&row| matches!(cells[row][left], '╰' | '└'))
        .expect("bottom border");
    let text = lines.join("\n");
    for row in [top + 1, bottom - 2] {
        assert!(
            cells[row][left + 1..right].iter().all(|c| *c == ' '),
            "row {row}:\n{text}"
        );
    }
    assert!(
        lines[bottom - 1].contains(action),
        "buttons touch bottom border:\n{text}"
    );
    for row in &cells[top + 1..bottom] {
        assert_eq!(row[left + 1], ' ', "left padding:\n{text}");
        assert_eq!(row[right - 1], ' ', "right padding:\n{text}");
    }
}

/// The whole dashboard surface drawn into a terminal of the given size, as one
/// string per row.
pub(crate) fn drawn(dashboard: &mut DashboardState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    terminal
        .draw(|frame| crate::render::render(frame, dashboard))
        .expect("draw the surface");
    buffer_lines(terminal.backend().buffer())
}

/// The cell position of the first drawn row containing `label`, as the column
/// where the label starts and the row it is on.
pub(crate) fn point(lines: &[String], label: &str) -> (u16, u16) {
    let (row, line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains(label))
        .unwrap_or_else(|| panic!("missing {label:?}: {lines:#?}"));
    (cell_column(line, label), row as u16)
}

/// A mouse event at one cell position, with no modifiers held.
pub(crate) fn mouse_at(kind: MouseEventKind, (column, row): (u16, u16)) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// Column of `needle` within a drawn row, counted in cells rather than bytes.
pub(crate) fn cell_column(line: &str, needle: &str) -> u16 {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("missing {needle} in {line:?}"));
    line[..byte].chars().count() as u16
}

/// A drawn summary cell that holds session text rather than the rule fill.
pub(crate) fn summary_text_cell(cell: &ratatui::buffer::Cell) -> bool {
    let symbol = cell.symbol();
    !symbol.trim().is_empty() && symbol != "─"
}

pub(crate) fn ctrl_key(character: char) -> KeyEvent {
    KeyEvent::new(
        KeyCode::Char(character),
        if cfg!(target_os = "macos") {
            KeyModifiers::SUPER
        } else {
            KeyModifiers::CONTROL
        },
    )
}

/// An Alt chord, for the composer's readline keys. Alt is the same modifier
/// everywhere, so this must not go through [`ctrl_key`], which reports SUPER
/// on macOS.
pub(crate) fn alt_key(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)
}

/// The default prefix key exactly as a terminal delivers it.
///
/// Deliberately literal rather than [`ctrl_key`]: that helper reports SUPER on
/// macOS, and the prefix is matched as Control on every platform.
pub(crate) fn prefix_key() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
}

/// Presses a sequence of keys through the prefix router and dispatches what it
/// decides exactly as the mj-cli event loop does, so a test drives the same
/// path the live terminal does.
pub(crate) fn route(dashboard: &mut DashboardState, keys: &[KeyEvent]) -> DashboardAction {
    let mut action = DashboardAction::None;
    for key in keys {
        match dashboard.route_bound_key(key) {
            KeyRoute::Command { id, index } => {
                if !dashboard.command_allowed_now(id) {
                    continue;
                }
                action = if id == CommandId::SwitchWorkspace {
                    index
                        .map(|index| dashboard.select_workspace_index(index))
                        .unwrap_or(DashboardAction::None)
                } else {
                    dashboard.dispatch_command(id)
                };
            }
            KeyRoute::Consumed => {}
            KeyRoute::Forward => action = dashboard.handle_key(*key),
        }
    }
    action
}

/// The key presses that run a command through its first live binding.
pub(crate) fn chord_keys(dashboard: &DashboardState, id: CommandId) -> Vec<KeyEvent> {
    let action = crate::actions::spec(id)
        .action
        .unwrap_or_else(|| panic!("{id:?} has no bindable action"));
    let binding = *dashboard
        .keybinds()
        .bindings(action)
        .first()
        .unwrap_or_else(|| panic!("{id:?} is not bound to any key"));
    let key = combo_key_event(binding.combo);
    match binding.trigger {
        Trigger::Prefix => vec![combo_key_event(dashboard.keybinds().prefix), key],
        Trigger::Direct => vec![key],
    }
}

/// Runs a command the way a person does: its first live binding, through the
/// router, dispatched as mj-cli would.
pub(crate) fn chord(dashboard: &mut DashboardState, id: CommandId) -> DashboardAction {
    let keys = chord_keys(dashboard, id);
    route(dashboard, &keys)
}

pub(crate) fn open_new_session_wizard(dashboard: &mut DashboardState) -> DashboardAction {
    chord(dashboard, CommandId::NewSessionWizard)
}

pub(crate) fn open_palette(dashboard: &mut DashboardState) -> DashboardAction {
    chord(dashboard, CommandId::Palette)
}

pub(crate) fn mouse_in(kind: MouseEventKind, area: Rect) -> MouseEvent {
    MouseEvent {
        kind,
        column: area.x.saturating_add(1),
        row: area.y.saturating_add(1),
        modifiers: KeyModifiers::NONE,
    }
}

/// Like `mouse_in`, but clicks a specific row within `area` instead of
/// always the top-left cell, so tests can hit tail lines further down a
/// multi-line hitbox.
pub(crate) fn mouse_at_row(kind: MouseEventKind, area: Rect, row_offset: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: area.x.saturating_add(1),
        row: area.y.saturating_add(row_offset),
        modifiers: KeyModifiers::NONE,
    }
}

pub(crate) fn config() -> Config {
    // SetupDialog sizes itself from the NO_COLOR-aware summary before drawing.
    crate::pin_no_color_override_for_test(true);
    Config {
        keys: Default::default(),
        jev: Default::default(),
        subagents: Default::default(),
        version: CONFIG_VERSION,
        sessions_side: Default::default(),
        advanced: Default::default(),
        notify: Default::default(),
        spinner: Default::default(),
        theme: Default::default(),
        phone: Default::default(),
        continuation: Default::default(),
        review: Default::default(),
        sessionwiki: Default::default(),
        legacy_startup: (),
        default_targets: Default::default(),
        machines: Default::default(),
        profiles: BTreeMap::from([
            (
                "claude-1".into(),
                HarnessProfile {
                    enabled: true,
                    context_window_bytes: None,
                    subagents: Default::default(),
                    guardian_review_model: None,
                    kind: HarnessKind::Claude,
                    home: PathBuf::from("/profiles/claude"),
                    environment: Default::default(),
                },
            ),
            (
                "codex-1".into(),
                HarnessProfile {
                    enabled: true,
                    context_window_bytes: None,
                    subagents: Default::default(),
                    guardian_review_model: None,
                    kind: HarnessKind::Codex,
                    home: PathBuf::from("/profiles/codex"),
                    environment: Default::default(),
                },
            ),
            (
                "codex-2".into(),
                HarnessProfile {
                    enabled: true,
                    context_window_bytes: None,
                    subagents: Default::default(),
                    guardian_review_model: None,
                    kind: HarnessKind::Codex,
                    home: PathBuf::from("/profiles/codex-two"),
                    environment: Default::default(),
                },
            ),
        ]),
        bundles: BTreeMap::from([(
            "hel".into(),
            ProjectBundle {
                primary_repo: "hel".into(),
                repositories: vec![ProjectRepository {
                    id: "hel".into(),
                    github: Some("BrokkAi/hel".into()),
                    local: None,
                    destination: PathBuf::from("hel"),
                    git_ref: None,
                }],
            },
        )]),
        targets: BTreeMap::from([(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: ContainerTemplate {
                    build_cache: None,
                    image: "ubuntu:24.04".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: Default::default(),
                    workspace_storage: Default::default(),
                },
            },
        )]),
    }
}

pub(crate) fn stopped_session() -> SessionRecord {
    SessionRecord {
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
        workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
        archived: false,
        container_cpus: None,
        container_memory: None,
        id: "session-1".into(),
        title: "Raise the dead".into(),
        harness_kind: HarnessKind::Codex,
        last_profile: "codex-1".into(),
        bundle_id: "hel".into(),
        project_directory: None,
        managed_worktree: None,
        review: None,
        target_template_id: "podman".into(),
        resource_allocation: None,
        additional_mounts: vec![],
        state: SessionState::Stopped,
        target: None,
        native_session_id: Some("native-1".into()),
        acp_session_title: Some("ACP pretty name".into()),
        session_title_override: None,
        created_at: "2026-08-09T00:00:00Z".into(),
        updated_at: "2026-08-09T01:00:00Z".into(),
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        last_checkpoint_error: None,
        checkpoint: Some(CheckpointMetadata {
            archive_path: PathBuf::from("sessions/session-1.hel.zip"),
            sha256: "a".repeat(64),
            created_at: "2026-08-09T01:00:00Z".into(),
            event_frontier: 2,
        }),
    }
}

/// The same fixture as [`stopped_session`], but live. Dashboard interactions
/// such as rename and the container editor only apply to a session that is
/// actually on the dashboard.
pub(crate) fn running_session() -> SessionRecord {
    SessionRecord {
        project: None,
        state: SessionState::Running,
        ..stopped_session()
    }
}

pub(crate) fn legacy_managed_session(mut session: SessionRecord) -> SessionRecord {
    let root = PathBuf::from(format!("/srv/project/.mj/worktrees/{}", session.id));
    session.project_directory = Some(root.clone());
    session.managed_worktree = Some(mj_core::state::ManagedWorktree {
        kind: mj_core::state::ManagedCheckoutKind::Worktree,
        source_project_directory: "/srv/project".into(),
        source_repository: "/srv/project".into(),
        worktree_root: root,
        branch: format!("mj/{}", session.id),
        target: mj_core::state::ManagedWorktreeTarget::Local,
        base_commit: Some("1".repeat(40)),
    });
    session
}

/// A form question the agent is waiting on, as the daemon projects it.
pub(crate) fn question(session_id: &str) -> mj_core::elicitation::ElicitationRequest {
    mj_core::elicitation::ElicitationRequest::from_acp_params(
        format!("{session_id}-question"),
        serde_json::json!({
            "mode": "form",
            "sessionId": session_id,
            "message": "Choose a path",
            "requestedSchema": {"type": "object", "properties": {"path": {"type": "string"}}}
        }),
    )
    .expect("valid test question")
}

/// Moves the open resume dialog to the Mjolnir tab with its list focused. The
/// dialog opens on the running sessions with the list itself focused, and a
/// stopped record is one tab to the right of them.
/// What the daemon's `ResumeCandidates` answer holds for `state`: the
/// inactive top-level records, and what every record says about native
/// sessions and local checkouts.
pub(crate) fn resume_candidates_for(
    config: &Config,
    state: &mj_core::state::State,
) -> mj_client::daemon::ResumeCandidates {
    let mut candidates = mj_client::daemon::ResumeCandidates::default();
    for (id, record) in &state.sessions {
        if let Some(native) = &record.native_session_id {
            candidates
                .adopted_native_sessions
                .push((record.harness_kind, native.clone()));
        }
        if let Some(checkout) = &record.managed_worktree
            && checkout.target == mj_core::state::ManagedWorktreeTarget::Local
        {
            candidates
                .local_checkout_roots
                .push(checkout.worktree_root.clone());
        }
        if !record.state.is_active() && !state.is_subagent_session(id) {
            candidates
                .candidates
                .push(mj_client::daemon::ResumeCandidate::of(record, config));
        }
    }
    candidates
}

/// Open the resume dialog and answer its request for stopped sessions from
/// the dashboard's own records, as the daemon would.
pub(crate) fn open_resume_dialog(
    dashboard: &mut DashboardState,
    discovery_id: u64,
    profiles: Vec<crate::ImportProfileOption>,
) {
    dashboard.show_resume_dialog(discovery_id, profiles);
    let candidates = resume_candidates_for(&dashboard.config, &dashboard.state);
    assert!(dashboard.apply_resume_candidates(discovery_id, Ok(candidates)));
}

pub(crate) fn focus_resume_hel_rows(dashboard: &mut DashboardState) {
    if let crate::Mode::ResumeDialog(dialog) = &mut dashboard.mode {
        dialog.tab = crate::resume::ResumeTab::Hel;
        dialog
            .form
            .get_mut()
            .focus(crate::resume::ResumeFocus::Sessions);
    }
    dashboard.rebuild_resume_rows();
}

/// Reaches the resume wizard the way the UI does: open the resume dialog, move
/// to the Mjolnir tab, then activate the first row, which is the session's own
/// stopped record.
pub(crate) fn open_resume_wizard(dashboard: &mut DashboardState) -> crate::DashboardAction {
    open_resume_dialog(dashboard, 1, Vec::new());
    focus_resume_hel_rows(dashboard);
    dashboard.handle_key(key(KeyCode::Enter))
}

pub(crate) fn dashboard_with_session(mut session: SessionRecord) -> DashboardState {
    session.updated_at = "2026-08-09T01:00:00Z".into();
    let session_id = session.id.clone();
    let mut dashboard = DashboardState::new(
        config(),
        State {
            last_subagent_policy: Default::default(),
            subagents: Default::default(),
            version: STATE_VERSION,
            sessions: [(session.id.clone(), session)].into_iter().collect(),
            mount_history: Default::default(),
            container_sizes: Default::default(),
        },
        BTreeMap::new(),
    );
    // This fixture represents a conversation already opened by the host.
    dashboard
        .navigation
        .assign(dashboard.focused_pane(), Some(&session_id));
    dashboard
}

/// Marks a session's turn as running, so it reads as working.
pub(crate) fn set_working(dashboard: &mut DashboardState, session_id: &str) {
    dashboard
        .session_details
        .get_mut(session_id)
        .expect("the session has details")
        .current_turn_started_at = Some(1);
}

/// A dashboard showing one running parent session with one running
/// sub-agent. Returns the parent's id.
pub(crate) fn dashboard_with_one_subagent() -> (DashboardState, String) {
    let parent = running_session();
    let mut child = running_session();
    child.id = "child-session".into();
    let relation = mj_core::subagent::SubagentRecord {
        child_session_id: child.id.clone(),
        parent_session_id: parent.id.clone(),
        task_name: "Inspect parser".into(),
        profile_id: child.last_profile.clone(),
        model: None,
        effort: None,
        working_directory: Default::default(),
        initial_prompt: "Inspect the parser".into(),
        request_key: "request-1".into(),
        created_at: child.created_at.clone(),
        noticed_turn: None,
        handback_tool: false,
    };
    let mut dashboard = dashboard_with_session(parent.clone());
    let mut state = dashboard.state.clone();
    state.sessions.insert(child.id.clone(), child.clone());
    state.subagents.insert(child.id.clone(), relation);
    dashboard.set_state(state);
    (dashboard, parent.id)
}

/// A dashboard with one running parent and one finished harness-native
/// child, as R10 saw one: Codex's "Review calc", completed, availability
/// unknown, with its transcript in the store. Returns the parent's id and
/// the child's presentation id.
pub(crate) fn dashboard_with_finished_native_child() -> (DashboardState, String, String) {
    use mj_core::native_agent::*;
    let parent = running_session();
    let mut dashboard = dashboard_with_session(parent.clone());
    let agent = NativeAgent {
        availability: NativeAgentAvailability::Unknown,
        availability_reason: None,
        stable_id: None,
        owner_session_id: parent.id.clone(),
        session_id: "01a0d8d0-76ad-7e03-917d-8416c910f32c".into(),
        parent_session_id: None,
        name: "Review calc".into(),
        task: "Delegated task for Review calc".into(),
        capabilities: NativeAgentCapabilities {
            cancel: true,
            close: false,
        },
        state: NativeAgentState::Completed,
    };
    let id = agent.view_id();
    let mut projection = mj_core::state::MaterializedSession::empty(&id);
    projection.transcript = vec![
        thought(87, "Reading calc.py"),
        agent_message(153, "calc.py adds and subtracts correctly."),
    ];
    dashboard.set_native_agents(vec![NativeAgentView {
        generation_ordinal: 81,
        agent,
        projection,
    }]);
    (dashboard, parent.id, id)
}

/// precision-3260 as the daemon's storage owner reports it: `/` holds the
/// worker roots and profile homes, `~/Projects` is a filesystem of its own.
pub(crate) fn precision_storage(
    root_free: u64,
    projects_free: u64,
) -> mj_core::targets::storage::TargetStorageView {
    use mj_core::targets::storage::{FilesystemSpace, TargetStorageView};
    TargetStorageView::evaluate(
        "precision-3260",
        Some("/home/jonathan".into()),
        &[
            FilesystemSpace {
                mount: "/".into(),
                available_bytes: root_free,
                total_bytes: 500 << 30,
                reserved_bytes: 25 << 30,
                paths: vec![
                    "/home/jonathan/.local/share/hel/workers".into(),
                    "/home/jonathan/.local/share/hel/profiles".into(),
                    "/tmp".into(),
                ],
            },
            FilesystemSpace {
                mount: "/home/jonathan/Projects".into(),
                available_bytes: projects_free,
                total_bytes: 1000 << 30,
                reserved_bytes: 0,
                paths: vec!["/home/jonathan/Projects".into()],
            },
        ],
        Some(1),
        |_| None,
        None,
    )
}

/// A running session on precision-3260 whose managed clone is under
/// `~/Projects`.
pub(crate) fn precision_session() -> SessionRecord {
    let mut session = running_session();
    session.target = Some(mj_core::state::TargetLocator::SshBare {
        host: "precision-3260".into(),
        workspace: format!("/home/jonathan/Projects/app/.mj/clones/{}", session.id).into(),
        worker_id: None,
    });
    session
}

pub(crate) fn test_capacity_target() -> DeploymentCapacityTarget {
    DeploymentCapacityTarget {
        id: "local".into(),
        host: "local".into(),
        target_ids: vec!["podman".into()],
        kind: DeploymentCapacityKind::Host,
        local: true,
        probes: Vec::new(),
        local_storage_paths: Vec::new(),
        probe_error: None,
    }
}

pub(crate) fn transcript_item(position: u64, body: TranscriptBody) -> Arc<TranscriptItem> {
    let at_ms = i64::try_from(position).unwrap() * 1_000;
    let latest_content_event_ordinal =
        matches!(&body, TranscriptBody::Agent { .. }).then_some(position);
    Arc::new(TranscriptItem {
        stable_id: format!("item-{position}"),
        position,
        latest_content_event_ordinal,
        created_at_ms: at_ms,
        last_changed_at_ms: at_ms,
        body,
    })
}

pub(crate) fn agent_message(position: u64, text: impl Into<String>) -> Arc<TranscriptItem> {
    transcript_item(
        position,
        TranscriptBody::Agent {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text.into()}
            })],
            streaming: false,
        },
    )
}

pub(crate) fn thought(position: u64, text: impl Into<String>) -> Arc<TranscriptItem> {
    transcript_item(
        position,
        TranscriptBody::Thought {
            chunks: vec![serde_json::json!({
                "content": {"type": "text", "text": text.into()}
            })],
            streaming: false,
        },
    )
}

pub(crate) fn work_interruption(position: u64) -> Arc<TranscriptItem> {
    let mut item = transcript_item(
        position,
        TranscriptBody::System {
            text: "Work interrupted".into(),
        },
    );
    Arc::make_mut(&mut item).stable_id = format!(
        "{}{}",
        mj_core::transcript::WORK_INTERRUPTED_ITEM_PREFIX,
        position
    );
    item
}

pub(crate) fn materialized_session_for(
    session_id: &str,
    transcript: Vec<Arc<TranscriptItem>>,
) -> MaterializedSession {
    let frontier = transcript
        .iter()
        .map(|item| item.position)
        .max()
        .unwrap_or(0);
    let mut session = MaterializedSession::empty(session_id);
    session.applied_event_ordinal = frontier;
    if frontier > 0 {
        session.applied_event_digest = "a".repeat(64);
    }
    session.execution = MaterializedExecutionState::Running {
        started_at_ms: 100_000,
    };
    session.last_activity_at_ms = transcript
        .iter()
        .map(|item| item.last_changed_at_ms)
        .max()
        .or(Some(100_000));
    session.transcript = transcript;
    session
}

pub(crate) fn apply_materialized_transcript(
    dashboard: &mut DashboardState,
    transcript: Vec<Arc<TranscriptItem>>,
) {
    apply_materialized_transcript_for(dashboard, "session-1", transcript);
}

pub(crate) fn apply_materialized_transcript_for(
    dashboard: &mut DashboardState,
    session_id: &str,
    transcript: Vec<Arc<TranscriptItem>>,
) {
    dashboard.apply_materialized_session(&materialized_session_for(session_id, transcript));
}

/// A conversation of `count` numbered exchanges, so preview scroll
/// assertions can name the message they expect to see. Agent chunks
/// coalesce unless separated, so each pairs with its own prompt.
pub(crate) fn numbered_conversation(count: u64) -> Vec<Arc<TranscriptItem>> {
    (0..count)
        .flat_map(|index| {
            [
                transcript_item(
                    index * 2 + 1,
                    TranscriptBody::User {
                        content: vec![serde_json::json!({
                            "type": "text",
                            "text": format!("question {index}"),
                        })],
                    },
                ),
                agent_message(index * 2 + 2, format!("answer {index}")),
            ]
        })
        .collect()
}

pub(crate) fn operation(
    kind: SessionOperationKind,
    stage: Option<ProvisionStage>,
) -> SessionOperationDisplay {
    let active_stages = stage
        .map(|stage| [(stage, 1_000)].into_iter().collect())
        .unwrap_or_default();
    SessionOperationDisplay {
        kind,
        operation_id: None,
        cancellable: true,
        started_at_epoch_seconds: 1_000,
        placeholder: None,
        active_stages,
        resume_destination: None,
    }
}

/// A published daemon catalog, independent of any open dialog or selection.
pub(crate) fn profile_capabilities_fixture(
    config: &mj_core::config::Config,
    models: &[(&str, &[&str])],
) -> mj_core::profile_capabilities::ProfileCapabilitiesSnapshot {
    use mj_core::profile_capabilities::{
        CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
    };
    let choice = |value: &str| mj_core::acp::SessionConfigChoice {
        value: value.into(),
        name: value.into(),
        description: None,
    };
    ProfileCapabilitiesSnapshot {
        profiles: config
            .enabled_profiles()
            .map(|(id, profile)| {
                (
                    profile.capabilities_key(id),
                    ProfileCapabilities {
                        choices: CapabilityState::Ready(mj_core::worker_launch::ProfileConfig {
                            model: models.first().map(|(model, _)| (*model).into()),
                            models: models.iter().map(|(model, _)| choice(model)).collect(),
                            efforts: models
                                .first()
                                .map(|(_, efforts)| {
                                    efforts.iter().map(|effort| choice(effort)).collect()
                                })
                                .unwrap_or_default(),
                            observed_at: 1,
                        }),
                        efforts: models
                            .iter()
                            .map(|(model, efforts)| {
                                (
                                    (*model).into(),
                                    CapabilityState::Ready(
                                        efforts.iter().map(|effort| choice(effort)).collect(),
                                    ),
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}
