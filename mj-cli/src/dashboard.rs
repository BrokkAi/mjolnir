//! The interactive session dashboard.
//!
//! One loop owns the terminal. It waits on every background feed at once, then
//! batches whatever queued behind the message that woke it, so a burst of
//! updates costs one draw. Nothing in this loop blocks: filesystem, database,
//! process, and network work all run as the tasks in [`io`] and
//! [`crate::pollers`], and answer over channels.
//!
//! [`DashboardContext`] holds everything the loop owns, which is what lets the
//! wait, the drains, and the action handling in [`actions`] be separate
//! functions over the same state.

pub(crate) mod actions;
mod attachment;
mod composer_drafts;
pub(crate) mod io;
mod workspace_settings;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use mj_controller::database::DetachedSessionDraft;
use mj_core::config::{Config, config_path};
use mj_core::credentials::CredentialSyncHandle;
use mj_core::state::{MaterializedSession, SessionRecord, SessionResourceAllocation};
use mj_core::subagent::SubagentRecord;

use mj_chat::chat::ChatElicitationDraft;
use mj_chat::selection::{
    FrameSurfaces, SelectionAction, SelectionRange, SelectionState, SurfaceId,
};
use mj_controller::controller::Controller;
use mj_controller::session_manager::{
    SessionManagerControl, SessionManagerShutdown, SessionManagerUpdates, ViewError,
};
use mj_controller::targets::DeploymentCapacityTarget;
use mj_controller::worker_client::CredentialSyncCoordinator;
use mj_core::workspace::{ConversationLayout, PaneSizes};
use mj_tui::tile_layout::PaneId;
use mj_tui::{
    CommandId, DashboardAction, DashboardState, ImportProfileOption,
    PreparedMaterializedSessionDetail, SessionOperationKind, render_combined,
    resume_profile_placeholders,
};
use ratatui::layout::Direction;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_stream::StreamExt as _;

use crate::dashboard::composer_drafts::ComposerDraftCache;
use crate::dashboard::io::{
    ActiveLifecycleOperation, DashboardIoUpdate, LifecycleReload, checkpoint_archive_targets,
    report, spawn_checkpoint_archive_size_refresh, spawn_clipboard_write, spawn_io,
    spawn_lifecycle_reload, spawn_materialized_session_projection, spawn_project_source_resolution,
    spawn_stored_session_summary,
};
use crate::import::{
    DashboardImportRequest, DashboardImportTaskResult, DashboardImportUpdate,
    PendingDashboardImport, spawn_dashboard_import,
};
use crate::pollers::{
    CapacityPollUpdate, CredentialSyncNotices, CredentialSyncSignalTracker, Feed, LifecycleUpdate,
    QuotaRefreshBatch, QuotaUpdate, ResourcePollTarget, ResourcePollUpdate, RuntimeStateUpdate,
    WorkerDiagnosisTracker, WorkerPollTarget, apply_worker_poll_update,
    complete_manual_quota_refresh, dashboard_worker_targets, projected_queued_prompts,
    quota_refresh_profiles, refresh_dashboard_poll_targets, schedule_due_credential_syncs,
    session_target_is_pollable, spawn_dashboard_capacity_poller, spawn_dashboard_resource_poller,
    spawn_quota_refresher, spawn_remote_dashboard_worker_poller, spawn_worker_diagnosis,
};
use crate::session_presentation::{
    apply_lifecycle_display, apply_session_activity, lifecycle_kind,
};
use crate::{TerminalGuard, short_id};

/// Redraw cadence for displays that move with the wall clock: turn timers,
/// countdowns, and elapsed times.
const DASHBOARD_CLOCK_TICK: Duration = Duration::from_secs(1);

/// How long the surface waits for stored session summaries before it opens a
/// conversation anyway. The summaries decide which session has the newest
/// activity; a stalled read must not leave the screen without a conversation.
const STARTUP_SESSION_WAIT: Duration = Duration::from_secs(2);

/// Whether the surface still gets to choose which conversation it opens on.
///
/// It waits for the stored summaries, because those carry the activity times
/// the choice compares — but only for [`STARTUP_SESSION_WAIT`], and only until
/// the user makes the choice themselves.
#[derive(Debug)]
struct StartupSession {
    /// Live sessions whose stored summary has not come back yet.
    pending_summaries: BTreeSet<String>,
    /// False once the choice has been made, or taken away.
    open_pending: bool,
    deadline: std::time::Instant,
}

impl StartupSession {
    /// Nothing to choose: the workspace has no live session, or one is
    /// already open.
    fn idle() -> Self {
        let now = std::time::Instant::now();
        Self {
            pending_summaries: BTreeSet::new(),
            open_pending: false,
            deadline: now,
        }
    }

    fn begin(session_ids: impl IntoIterator<Item = String>, now: std::time::Instant) -> Self {
        let pending_summaries = session_ids.into_iter().collect::<BTreeSet<_>>();
        Self {
            open_pending: !pending_summaries.is_empty(),
            pending_summaries,
            deadline: now + STARTUP_SESSION_WAIT,
        }
    }

    /// One session has answered, whether its summary loaded or failed.
    fn summary_arrived(&mut self, session_id: &str) {
        self.pending_summaries.remove(session_id);
    }

    /// The user acted, so the choice is theirs now.
    fn cancel(&mut self) {
        self.open_pending = false;
    }

    /// Whether the pick should run now. Answering `true` once retires the
    /// choice, so a later tick cannot open a second conversation.
    fn ready(&mut self, now: std::time::Instant) -> bool {
        if !self.open_pending {
            return false;
        }
        if !self.pending_summaries.is_empty() && now < self.deadline {
            return false;
        }
        self.open_pending = false;
        true
    }
}

/// The live session the surface should open on: the one with the newest
/// recorded activity.
///
/// Ties break by newest creation time and then by the larger session id, so
/// the choice is the same on every run. When no summary carried an activity
/// time — none is stored yet, or every read failed — every session ranks equal
/// on the first key and creation time decides, which is the intended fallback
/// rather than an accident.
fn startup_session_choice<'a>(
    workspace_id: Option<&str>,
    sessions: impl IntoIterator<Item = &'a SessionRecord>,
    activity_at_ms: impl Fn(&str) -> Option<u64>,
) -> Option<String> {
    sessions
        .into_iter()
        .filter(|session| {
            Some(session.workspace_id.as_str()) == workspace_id && session.state.is_active()
        })
        .max_by(|left, right| {
            activity_at_ms(&left.id)
                .unwrap_or(0)
                .cmp(&activity_at_ms(&right.id).unwrap_or(0))
                // `compare_by_creation` orders oldest first, so the newer of
                // the two is the greater under `max_by`.
                .then_with(|| left.compare_by_creation(right))
                .then_with(|| left.id.cmp(&right.id))
        })
        .map(|session| session.id.clone())
}
/// Fastest activity animation; individual styles advance from monotonic time.
const ANIMATION_TICK: Duration =
    Duration::from_millis(mj_chat::spinner::SPINNER_REDRAW_INTERVAL_MS as u64);
/// Scroll cadence while a drag is held past a scrollable surface's edge.
const SELECTION_AUTOSCROLL_TICK: Duration = Duration::from_millis(80);
pub(crate) const QUOTA_REFRESH_NOTICE: &str = "Refreshing targets and quotas…";
pub(crate) const QUOTA_REFRESHED_NOTICE: &str = "Targets and quotas refreshed.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DashboardExit {
    Normal,
    Detached,
    Interrupted,
}

pub(crate) use mj_client::operations::CriticalOperationTracker;

fn shutdown_wait_notice(blockers: &[String]) -> Option<String> {
    match blockers {
        [] => None,
        [blocker] => Some(format!("Waiting for {blocker} to complete before exiting")),
        blockers => Some(format!(
            "Waiting for {} operations to complete before exiting",
            blockers.len()
        )),
    }
}

pub(crate) struct ActiveDashboardImport {
    task_id: u64,
    pub(crate) cancelled: Arc<AtomicBool>,
}

/// A local form snapshot together with the relay frontier at which it was
/// captured. A projection older than that frontier must not invalidate a
/// freshly saved draft while an attachment is still settling.
#[derive(Debug)]
struct CachedQuestionDraft {
    draft: ChatElicitationDraft,
    captured_event_ordinal: u64,
}

/// Everything one dashboard run owns.
pub(crate) struct DashboardContext {
    terminal: TerminalGuard,
    pub(crate) controller: Controller,
    pub(crate) workspace_id: String,
    pub(crate) client_id: String,
    pub(crate) dashboard: DashboardState,
    pane_size_persistence: workspace_settings::WorkspaceSettingPersistence<PaneSizes>,
    layout_persistence: workspace_settings::WorkspaceSettingPersistence<ConversationLayout>,
    known_workspace_layouts: BTreeSet<String>,
    /// The conversation pane arrangement loaded for each workspace. M4 hands
    /// these to the terminal UI; until then they are only stored and saved.
    workspace_layouts: BTreeMap<String, ConversationLayout>,
    /// One notifications bar for the whole process: the dashboard and every
    /// chat view opened from it report through this shared handle.
    notices: mj_chat::chat::Notices,
    /// Terminal input remains owned by this event loop, including during Setup.
    events: Option<event::EventStream>,
    /// Every warm conversation, keyed by session id: one for each pane that
    /// shows a session. All of them are pumped on every loop iteration, so a
    /// conversation stays current whether or not it is the one with the
    /// keyboard, and moving between panes is a redraw rather than a rebuild.
    pub(crate) chats: BTreeMap<String, mj_chat::chat::ActiveChat>,
    /// In-memory form drafts for sessions that are not currently attached.
    /// Each value retains its complete request identity (and may contain one
    /// primary and one deferred reviewer form), so an id reused by a changed
    /// form cannot inherit old answers.
    question_drafts: BTreeMap<String, Vec<CachedQuestionDraft>>,
    /// Composer text is owned by this terminal once a session is opened. The
    /// inherited shared value is retained only as a compare-and-clear baseline
    /// for the detach persistence task.
    composer_drafts: ComposerDraftCache,
    /// Retain save failures so quitting cannot erase their notice before it is read.
    draft_save_failures: BTreeMap<String, String>,
    /// Session-manager attachment is asynchronous: an actor may need to
    /// answer from a worker or relay before a chat can be built. Each pane
    /// runs its own attach, so opening one conversation never cancels
    /// another.
    pub(crate) opening_chat_sessions: BTreeMap<PaneId, String>,
    attachments: BTreeMap<PaneId, attachment::SessionAttachment>,
    /// The sessions the last frame drew a conversation for. Read receipts
    /// follow what was on screen, which is one pane today and every drawn
    /// pane once the panes render themselves.
    drawn_chat_sessions: Vec<String>,
    /// Which conversation the surface opens on, and whether it is still the
    /// surface's choice to make.
    startup: StartupSession,
    go_context_refresh: Option<(String, std::time::Instant)>,
    go_context_in_flight: bool,
    go_selection_requested: Option<String>,
    go_selection_in_flight: bool,
    /// The notice generation the frame on screen was drawn from. Background
    /// work writes the shared notice slot without waking the loop, so the
    /// once-a-second clock compares against this to notice one.
    drawn_notice_generation: u64,
    /// The poll targets are recomputed only after the controller may have
    /// changed.
    pub(crate) controller_changed: bool,
    pub(crate) quit_detached: bool,
    shutdown_requested: bool,
    pub(crate) critical_operations: CriticalOperationTracker,
    critical_operations_changed: watch::Receiver<u64>,
    /// What the two-second keep-alive last saw. It never starts a daemon, so
    /// a daemon that goes away stays away until the user asks for it back.
    daemon_presence: watch::Receiver<crate::daemon::DaemonPresence>,

    quota_profiles_tx: watch::Sender<QuotaRefreshBatch>,
    quota: Feed<Receiver<QuotaUpdate>>,
    pub(crate) manual_quota_refresh_generation: Option<u64>,
    pub(crate) target_test_cancel: Option<Arc<AtomicBool>>,
    /// The active global review choice discovery. New profile/model
    /// selections cancel the old request before starting another one.
    pub(crate) path_input_job: Option<(String, Arc<AtomicBool>)>,
    pub(crate) review_discovery_cancel: Option<Arc<AtomicBool>>,
    /// The cancellable worker resolving an isolated session's network clone
    /// plan, keyed by the TUI generation that requested it.
    pub(crate) session_preflight_cancel: Option<(u64, Arc<AtomicBool>)>,

    worker_targets_tx: watch::Sender<Vec<WorkerPollTarget>>,
    worker: Feed<SessionManagerUpdates>,
    runtime_state: Feed<watch::Receiver<RuntimeStateUpdate>>,
    /// Reviews the daemon is running. The chat renders one of these rather
    /// than driving a review of its own.
    runtime_reviews: Feed<watch::Receiver<Vec<mj_controller::review_host::RuntimeReviewView>>>,
    /// Background events the daemon reported. Only ones newer than
    /// `reported_notice_id` reach the notice bar, so a surface that attaches
    /// late does not replay a backlog.
    runtime_notices: Feed<watch::Receiver<Vec<crate::daemon::RuntimeNotice>>>,
    reported_notice_id: Option<u64>,
    /// Last complete review projection, retained even while the session list
    /// is on screen so a subsequently opened chat starts in the right state.
    runtime_review_views: BTreeMap<String, mj_controller::review_host::RuntimeReviewView>,
    runtime_config: Feed<watch::Receiver<Config>>,
    config_reload_in_flight: bool,
    remote_lifecycle_sessions: BTreeSet<String>,
    remote_lifecycle_operations: BTreeMap<String, String>,
    runtime_state_revision: u64,
    pub(crate) worker_commands_tx: SessionManagerControl,
    worker_shutdown: Option<SessionManagerShutdown>,
    worker_diagnoses: WorkerDiagnosisTracker,

    pub(crate) lifecycle_updates_tx: UnboundedSender<LifecycleUpdate>,
    lifecycle: Feed<UnboundedReceiver<LifecycleUpdate>>,
    pub(crate) lifecycle_operations: BTreeMap<String, ActiveLifecycleOperation>,

    credential_sync: Feed<CredentialSyncCoordinator>,
    credential_sync_handle: CredentialSyncHandle,
    credential_sync_signals: CredentialSyncSignalTracker,
    credential_sync_notices: CredentialSyncNotices,

    resource_targets_tx: watch::Sender<Vec<ResourcePollTarget>>,
    resource_triggers_tx: Sender<String>,
    resource: Feed<Receiver<ResourcePollUpdate>>,

    capacity_targets_tx: watch::Sender<Vec<DeploymentCapacityTarget>>,
    capacity_triggers_tx: Sender<()>,
    capacity: Feed<Receiver<CapacityPollUpdate>>,

    pub(crate) aws_resource_options_tx: AwsResourceOptionsSender,
    aws_options: Feed<UnboundedReceiver<AwsResourceOptions>>,
    pub(crate) resolving_aws_resource_options: BTreeSet<String>,

    import_updates_tx: Sender<(u64, ImportProfileOption)>,
    import_profiles: Feed<Receiver<(u64, ImportProfileOption)>>,
    pub(crate) import_task_tx: Sender<DashboardImportUpdate>,
    import_tasks: Feed<Receiver<DashboardImportUpdate>>,
    pub(crate) pending_import: Option<PendingDashboardImport>,
    pub(crate) import_discovery_id: u64,
    /// One scan cache for the process, so reopening the resume dialog
    /// reparses only the native session files that changed.
    pub(crate) native_scan_cache: mj_controller::import::NativeScanCache,
    pub(crate) next_import_task_id: u64,
    pub(crate) active_import: Option<ActiveDashboardImport>,
    /// At most one desktop clipboard IPC request is allowed at a time. The
    /// request itself runs on a blocking worker; this flag keeps repeated
    /// Ctrl-V key repeats from creating an unbounded queue of reads.
    pub(crate) clipboard_read_in_flight: bool,
    /// Pane-scoped text selection, driven by the mouse events this loop
    /// intercepts before the views see them.
    selection: SelectionState,
    /// Text under the selection, read out of the buffer the last draw
    /// produced. Copy redraws first, so it never reads a stale frame.
    selection_text: Option<String>,

    /// The newest archive search this dialog asked for. The debounced task
    /// reads it when it wakes and gives up when a later keystroke has since
    /// replaced it.
    pub(crate) wiki_search_request: Arc<std::sync::atomic::AtomicU64>,

    pub(crate) dashboard_io_tx: UnboundedSender<DashboardIoUpdate>,
    dashboard_io: Feed<UnboundedReceiver<DashboardIoUpdate>>,
    pub(crate) web_request_generation: u64,
    pub(crate) web_request_cancel: Option<tokio_util::sync::DropGuard>,
    materialized_projection_permits: Arc<tokio::sync::Semaphore>,
    materialized_projections_in_flight: BTreeSet<String>,
    pending_materialized_projections: BTreeMap<String, (MaterializedSession, u64)>,
    project_sources_in_flight: BTreeSet<String>,
    read_receipt_in_flight: Option<String>,
    pending_read_receipts: BTreeMap<String, u64>,

    checkpoint_archive_targets_seen: BTreeMap<String, std::path::PathBuf>,
    checkpoint_archive_generation: u64,
}

/// One deployment target's resolved instance sizes, or why they could not be
/// resolved.
type AwsResourceOptions = (
    String,
    std::result::Result<Vec<SessionResourceAllocation>, String>,
);
type AwsResourceOptionsSender = UnboundedSender<AwsResourceOptions>;

fn enqueue_materialized_projection(
    in_flight: &mut BTreeSet<String>,
    pending: &mut BTreeMap<String, (MaterializedSession, u64)>,
    materialized: MaterializedSession,
    viewed_through_event_ordinal: u64,
) -> Option<(MaterializedSession, u64)> {
    let session_id = materialized.session_id.clone();
    if !in_flight.insert(session_id.clone()) {
        let replace = pending.get(&session_id).is_none_or(|(queued, _)| {
            materialized.applied_event_ordinal >= queued.applied_event_ordinal
        });
        if replace {
            pending.insert(session_id, (materialized, viewed_through_event_ordinal));
        }
        return None;
    }
    Some((materialized, viewed_through_event_ordinal))
}

pub(super) fn retain_workspace_sessions(
    controller: &mut Controller,
    _workspace_id: &str,
    client_id: &str,
) -> Result<()> {
    for session in controller
        .state
        .sessions
        .values_mut()
        .filter(|session| session.state.is_active())
    {
        let frontier = mj_controller::database::client_read_frontier(
            client_id,
            &session.workspace_id,
            &session.id,
        )?;
        session.viewed_through_event_ordinal = frontier;
    }
    Ok(())
}

pub(crate) async fn run_dashboard_for_workspace(
    workspace_id: &str,
    client_id: &str,
    open_workspace_manager: bool,
    go: Option<(mj_tui::GoMode, bool)>,
    daemon_presence: watch::Receiver<crate::daemon::DaemonPresence>,
) -> Result<DashboardExit> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        println!("Welcome to Mjolnir");
        println!("Run `mj doctor` for non-interactive validation.");
        return Ok(DashboardExit::Normal);
    }

    tokio::task::spawn_blocking(|| {
        mj_controller::setup::initialize_local_startup_config(&config_path())
    })
    .await
    .context("initialize startup configuration task failed")??;
    let Some(mut context) = DashboardContext::open(workspace_id, client_id, daemon_presence)?
    else {
        return Ok(DashboardExit::Normal);
    };
    if let Some((mode, setup)) = go {
        let modes = tokio::task::spawn_blocking(crate::go::saved_workspace_modes)
            .await
            .context("load project workspace settings task failed")??;
        context.dashboard.register_go_workspaces(modes);
        context.cancel_startup_session();
        let action = context.dashboard.begin_go(mode, setup);
        actions::apply_dashboard_action(&mut context, action).await?;
    }
    if open_workspace_manager {
        let action = context.dashboard.begin_workspace_manager();
        actions::apply_dashboard_action(&mut context, action).await?;
    }
    let termination = mj_controller::termination::Coordinator::install().token();
    // `interval_at` so the first tick is a period away rather than immediate,
    // and `Delay` so a tick that was gated off does not fire a burst to catch
    // up when it comes back.
    let mut clock_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + DASHBOARD_CLOCK_TICK,
        DASHBOARD_CLOCK_TICK,
    );
    clock_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut animation_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + ANIMATION_TICK, ANIMATION_TICK);
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Only armed while a held pointer sits at a scrollable surface's edge, so
    // an idle dashboard never ticks on it.
    let mut autoscroll_tick = tokio::time::interval(SELECTION_AUTOSCROLL_TICK);
    autoscroll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // One wakeup is one frame. `ratatui` writes only the cells that differ
    // from the previous frame, so an unconditional rebuild costs CPU time and
    // never terminal output. The two timer arms below are the only wakeups
    // that can decline a frame, because they fire whether or not anything they
    // display has moved.
    let mut redraw = true;
    loop {
        if !context.shutdown_requested {
            context.refresh_controller_derived_state();
        }
        if redraw {
            context.draw()?;
        }
        redraw = true;
        let mut action = DashboardAction::None;
        let mut chat_outcome = mj_chat::chat::ChatEventOutcome::None;
        // The winning arm takes the message that woke the loop; the drains
        // below batch whatever is queued behind it, so one wakeup is one draw.
        tokio::select! {
            () = context.pane_size_persistence.wait(), if context.pane_size_persistence.is_running() => {}
            () = context.layout_persistence.wait(), if context.layout_persistence.is_running() => {}
            _ = termination.cancelled(), if !context.shutdown_requested => {
                context.begin_shutdown(false);
            }
            event = next_terminal_event(&mut context.events) => {
                let Some(event) = event else { break };
                if context.shutdown_requested {
                    continue;
                }
                let mut event = event?;
                // The user acting is the strongest signal there is about which
                // conversation they want, so it takes the choice away from the
                // startup pick before that pick can override their input.
                if matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
                    context.cancel_startup_session();
                }
                // Key repeats and pastes arrive as several ready events, and
                // they coalesce into one frame; the first event that asks for
                // work ends the batch so that dispatch still follows input
                // order. Within the batch, only an event that is resolved
                // against the frame needs that frame rebuilt first: a pointer
                // is hit-tested against the controls the last frame registered,
                // and a modal opened by the previous key registers its controls
                // on the frame after it opens. An event that changed nothing
                // leaves the frame current, so it needs no rebuild.
                let mut previous_consumed = false;
                loop {
                    if previous_consumed
                        && (matches!(event, Event::Mouse(_)) || context.dashboard.modal_open())
                    {
                        context.draw()?;
                    }
                    if opening_cancel_event(&event, context.focused_pane_is_opening(), context.dashboard.modal_open()) {
                        context.cancel_chat_open();
                        context.dashboard.focus_sessions();
                        context.dashboard.set_notice("Session opening cancelled. Press Enter in Sessions to retry.");
                        break;
                    }
                    let (consumed, batched) = if let Some(command) =
                        global_chord_event(&context.dashboard, &event).filter(|command| {
                            !context.visible_chat().is_some_and(|chat| chat.component_modal_open())
                                || matches!(command, CommandId::Help | CommandId::QuitDetach | CommandId::TogglePanePreset | CommandId::Refresh)
                        })
                    {
                        context.dashboard.cancel_component_pointer();
                        if let Some(chat) = context.visible_chat() { chat.cancel_component_pointer(); }
                        // Detaching from an open conversation has to save the
                        // draft and the read cursor, which is the chat's own
                        // bookkeeping rather than the dashboard's.
                        if command == CommandId::QuitDetach
                            && let Some(chat) = context.visible_chat()
                        {
                            chat_outcome = chat.detach();
                        } else if apply_global_focus_cycle(&mut context.dashboard, &event, command) {
                            action = DashboardAction::None;
                        } else {
                            action = context.dashboard.dispatch_command(command);
                        }
                        (true, false)
                    } else {
                        // Selection sees mouse and Esc first: a drag inside a
                        // pane belongs to the engine, and everything else
                        // comes back out for the view.
                        match context.route_selection(event) {
                            SelectionRouting::Consumed => (true, true),
                            SelectionRouting::Copy { surface, range } => {
                                context.copy_selection(surface, range)?;
                                (true, true)
                            }
                            SelectionRouting::Forward(event) => dispatch_event(
                                &mut context,
                                event,
                                &mut action,
                                &mut chat_outcome,
                            ),
                        }
                    };
                    previous_consumed = consumed;
                    if !batched {
                        break;
                    }
                    // A zero timeout rather than a no-op waker: `EventStream`
                    // arms its reader thread with the waker it was last polled
                    // with, so a no-op waker here would swallow the next wakeup.
                    let Ok(Some(next)) = tokio::time::timeout(
                        Duration::ZERO,
                        next_terminal_event(&mut context.events),
                    )
                    .await
                    else {
                        break;
                    };
                    event = next?;
                }
            }
            // The warm chat's own feeds: remote command results, its clipboard
            // and history I/O, dictation, and the session view. They run
            // whether or not the chat is on screen, which is what keeps an
            // off-screen chat current.
            () = pump_chats(&mut context.chats) => {
                // A warm chat may be hidden by another tab or selection.
                // Only conversations that were on screen advance their read
                // receipts.
                context.acknowledge_visible_chats();
            }
            update = context.quota.wait(), if context.quota.is_open() => {
                context.quota.accept(update);
            }
            update = context.worker.wait(), if context.worker.is_open() => {
                context.worker.accept(update);
            }
            update = context.runtime_reviews.wait(), if context.runtime_reviews.is_open() => {
                context.runtime_reviews.accept(update);
            }
            update = context.runtime_notices.wait(), if context.runtime_notices.is_open() => {
                context.runtime_notices.accept(update);
            }
            update = context.runtime_config.wait(), if context.runtime_config.is_open() => {
                context.runtime_config.accept(update);
            }
            update = context.runtime_state.wait(), if context.runtime_state.is_open() => {
                context.runtime_state.accept(update);
            }
            result = context.credential_sync.wait(), if context.credential_sync.is_open() => {
                context.credential_sync.accept(result);
            }
            update = context.resource.wait(), if context.resource.is_open() => {
                context.resource.accept(update);
            }
            update = context.capacity.wait(), if context.capacity.is_open() => {
                context.capacity.accept(update);
            }
            options = context.aws_options.wait(), if context.aws_options.is_open() => {
                context.aws_options.accept(options);
            }
            profile = context.import_profiles.wait(), if context.import_profiles.is_open() => {
                context.import_profiles.accept(profile);
            }
            update = context.import_tasks.wait(), if context.import_tasks.is_open() => {
                context.import_tasks.accept(update);
            }
            update = context.lifecycle.wait(), if context.lifecycle.is_open() => {
                context.lifecycle.accept(update);
            }
            update = context.dashboard_io.wait(), if context.dashboard_io.is_open() => {
                context.dashboard_io.accept(update);
            }
            _ = context.critical_operations_changed.changed(),
                if context.shutdown_requested => {}
            // The keep-alive no longer starts a daemon, so a daemon that is
            // gone has to be visible instead of silently replaced.
            changed = context.daemon_presence.changed() => {
                if changed.is_ok() {
                    let presence = context.daemon_presence.borrow_and_update().clone();
                    match presence {
                        crate::daemon::DaemonPresence::Attached => context
                            .dashboard
                            .set_notice("Mjolnir daemon is running again."),
                        // The reason is already in the log. The notice bar is
                        // one line, so it carries the action instead.
                        crate::daemon::DaemonPresence::Missing(reason) => {
                            tracing::warn!(%reason, "the Mjolnir daemon is not running");
                            context.dashboard.set_failure_notice(
                                "Mjolnir daemon is not running. Press F2 and run \"Restart the Mjolnir daemon\".",
                            );
                        }
                    }
                }
            }
            // Poll displayed time values without forcing a frame when none
            // of the visible clocks or countdowns changed.
            _ = clock_tick.tick() => {
                // Resume search covers the moving "Last active" text, so the
                // same clock that redraws the dialog rebuilds its rows.
                context.dashboard.rebuild_resume_rows();
                redraw = context.clock_tick_redraws();
                // The startup pick fires at most once, and opening the
                // conversation it chooses has to reach the screen.
                redraw |= context.maybe_open_startup_session();
            }
            // Input redraws never advance animations. Only visible activity
            // arms this timer; settled conversations keep the slow clock.
            _ = animation_tick.tick(), if context.needs_animation() => {
                redraw = context.dashboard.animation_changed()
                    || context.visible_chat().is_some_and(|chat| chat.animation_changed());
            }
            // A drag held past a scrollable surface's edge keeps scrolling it
            // and keeps extending the selection, the way a held pointer does
            // in a terminal's own selection.
            _ = autoscroll_tick.tick(), if context.autoscroll_request().is_some() => {
                context.apply_autoscroll()?;
            }
        }
        // A background message can be queued behind a timer that won the
        // select, and the drain applies it here. Its result has to reach the
        // screen even when the timer itself had nothing to show.
        redraw |= context.drain_feeds();
        if let Some(workspace_id) = context.dashboard.active_workspace_id()
            && context.known_workspace_layouts.contains(workspace_id)
            && context
                .dashboard
                .workspace_pane_sizes_modified(workspace_id)
        {
            context
                .pane_size_persistence
                .update(workspace_id, context.dashboard.pane_sizes());
        }
        context.save_active_workspace_layout();
        if !context.shutdown_requested {
            context.apply_chat_outcome(chat_outcome).await;
            actions::apply_dashboard_action(&mut context, action).await?;
            // Input or a background result can open an isolated creation
            // review; either way its prerequisite check starts here.
            if let Some(check) = context.dashboard.take_prerequisite_check() {
                actions::apply_dashboard_action(&mut context, check).await?;
            }
            // The Sessions pane is a list of conversations, not a list of
            // things to go and open, so the transcript follows its selection.
            context.follow_selected_session();
            context.refresh_go_context();
        }
        context.remember_go_selection();
        if context.shutdown_requested && context.refresh_shutdown_notice() {
            break;
        }
    }
    context.cancel_background_work();
    let quit_detached = context.quit_detached;
    // Hand the terminal back before saying anything on it; the warm chat and
    // the background feeds are torn down after, as the rest of the context
    // drops.
    drop(context.terminal);
    for error in context.draft_save_failures.values() {
        eprintln!("{error}");
    }
    if let Err(error) = context.pane_size_persistence.finish().await {
        tracing::warn!(%error, "workspace pane-size final flush failed");
        eprintln!("{error:#}");
    }
    if let Err(error) = context.layout_persistence.finish().await {
        tracing::warn!(%error, "workspace layout final flush failed");
        eprintln!("{error:#}");
    }
    if let Some(shutdown) = context.worker_shutdown.take() {
        shutdown
            .shutdown()
            .await
            .context("shut down dashboard session manager")?;
    }
    Ok(if quit_detached {
        DashboardExit::Detached
    } else if context.shutdown_requested {
        DashboardExit::Interrupted
    } else {
        DashboardExit::Normal
    })
}

mod chat_tasks;
mod drafts;
mod drains;
mod session_state;
mod surface;

impl DashboardContext {
    pub(crate) fn cancel_session_preflight(&mut self) {
        if let Some((_, cancelled)) = self.session_preflight_cancel.take() {
            cancelled.store(true, Ordering::Release);
        }
    }

    /// The conversation the focused pane holds, whether or not it is on
    /// screen. Per-pane bookkeeping uses this; the keyboard and the drawn
    /// conversation use [`Self::visible_chat`], which also answers for the
    /// standby composers that stand in front of a session.
    pub(crate) fn focused_chat(&self) -> Option<&mj_chat::chat::ActiveChat> {
        self.chats.get(self.dashboard.current_session_id()?)
    }

    pub(crate) fn focused_chat_mut(&mut self) -> Option<&mut mj_chat::chat::ActiveChat> {
        let Self {
            chats, dashboard, ..
        } = self;
        chats.get_mut(dashboard.current_session_id()?)
    }

    /// Whether the focused pane is still waiting for an attach, which is what
    /// Escape cancels.
    fn focused_pane_is_opening(&self) -> bool {
        self.opening_chat_sessions
            .contains_key(&self.dashboard.focused_pane())
    }

    /// Keeps the dashboard's single in-flight-attach report on the focused
    /// pane, which is the pane whose empty conversation is drawn.
    fn sync_opening_session(&mut self) {
        let opening = self
            .opening_chat_sessions
            .get(&self.dashboard.focused_pane())
            .cloned();
        self.dashboard.set_opening_session(opening.as_deref());
    }

    /// Stop the attach a pane is running for `session_id`, if one is.
    fn cancel_chat_open_for(&mut self, session_id: &str) {
        for pane in self.panes_opening(session_id) {
            self.opening_chat_sessions.remove(&pane);
            if let Some(attachment) = self.attachments.get_mut(&pane) {
                attachment.cancel();
            }
        }
        self.sync_opening_session();
    }

    /// Give up the attach a pane is running for `session_id` in a way that
    /// allows one fresh attempt once whatever owns the session is done.
    fn defer_chat_open_for(&mut self, session_id: &str) {
        for pane in self.panes_opening(session_id) {
            self.opening_chat_sessions.remove(&pane);
            if let Some(attachment) = self.attachments.get_mut(&pane) {
                attachment.defer();
            }
        }
        self.sync_opening_session();
    }

    fn panes_opening(&self, session_id: &str) -> Vec<PaneId> {
        self.opening_chat_sessions
            .iter()
            .filter(|(_, opening)| opening.as_str() == session_id)
            .map(|(pane, _)| *pane)
            .collect()
    }

    pub(crate) fn defer_all_chat_opens(&mut self) {
        for attachment in self.attachments.values_mut() {
            attachment.defer();
        }
        self.opening_chat_sessions.clear();
        self.dashboard.set_opening_session(None);
    }

    /// Persists how far a warm chat has been read and the draft it holds.
    pub(crate) fn record_chat_detach(&mut self, session_id: &str) {
        let Some(ordinal) = self
            .chats
            .get(session_id)
            .map(mj_chat::chat::ActiveChat::latest_event_ordinal)
        else {
            return;
        };
        self.record_detach(session_id, ordinal);
    }

    /// Drops the warm chats no pane shows any more, saving each one first.
    pub(crate) fn retire_chats_outside_the_layout(&mut self) {
        let shown = self.dashboard.pane_session_ids();
        for session_id in self
            .chats
            .keys()
            .filter(|session_id| !shown.contains(*session_id))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.record_chat_detach(&session_id);
            self.chats.remove(&session_id);
        }
    }

    /// Opens a session in a new pane beside the focused one. A session
    /// already open somewhere moves the focus there instead of appearing
    /// twice, and a conversation area with no room for two panes says so.
    #[allow(dead_code, reason = "the pane commands that call this land in M4")]
    pub(crate) fn open_session_in_split(&mut self, session_id: &str, direction: Direction) {
        if let Some(pane) = self.dashboard.pane_for_session(session_id) {
            self.dashboard.focus_pane(pane);
            self.sync_opening_session();
            self.save_active_workspace_layout();
            return;
        }
        if self.dashboard.split_focused_pane(direction, None).is_none() {
            self.dashboard.set_notice("Not enough room to split");
            return;
        }
        self.sync_opening_session();
        self.open_chat_session(session_id);
        self.save_active_workspace_layout();
    }

    /// Closes the focused pane, saving and dropping the conversation it held.
    /// The last pane is emptied rather than removed.
    #[allow(
        dead_code,
        reason = "the Close pane command that calls this lands in M4"
    )]
    pub(crate) fn close_focused_pane(&mut self) {
        let pane = self.dashboard.focused_pane();
        self.opening_chat_sessions.remove(&pane);
        self.attachments.remove(&pane);
        if let Some(session_id) = self.dashboard.close_focused_pane() {
            self.record_chat_detach(&session_id);
            self.chats.remove(&session_id);
            self.selection.clear();
        }
        self.sync_opening_session();
        self.save_active_workspace_layout();
    }

    /// Queues the active workspace's arrangement for saving once this client
    /// has changed it.
    pub(crate) fn save_active_workspace_layout(&mut self) {
        let Some(workspace_id) = self.dashboard.active_workspace_id().map(str::to_owned) else {
            return;
        };
        if !self.known_workspace_layouts.contains(&workspace_id)
            || !self.dashboard.workspace_layout_modified(&workspace_id)
        {
            return;
        }
        let layout = self.dashboard.conversation_layout_for(&workspace_id);
        self.set_workspace_layout(&workspace_id, layout);
    }

    /// Record a workspace's conversation pane arrangement and queue its save.
    pub(crate) fn set_workspace_layout(&mut self, workspace_id: &str, layout: ConversationLayout) {
        if self.workspace_layouts.get(workspace_id) == Some(&layout) {
            return;
        }
        self.workspace_layouts
            .insert(workspace_id.to_owned(), layout.clone());
        self.layout_persistence.update(workspace_id, layout);
    }

    pub(crate) fn request_shutdown(&mut self) {
        self.begin_shutdown(true);
    }

    pub(crate) fn select_workspace(&mut self, workspace_id: Option<String>) {
        if self.dashboard.active_workspace_id() == workspace_id.as_deref() {
            return;
        }
        if let Some(id) = self.dashboard.active_workspace_id()
            && self.known_workspace_layouts.contains(id)
            && self.dashboard.workspace_pane_sizes_modified(id)
        {
            self.pane_size_persistence
                .update(id, self.dashboard.pane_sizes());
        }
        if let Some(id) = self.dashboard.active_workspace_id().map(str::to_owned)
            && self.known_workspace_layouts.contains(&id)
            && self.dashboard.workspace_layout_modified(&id)
        {
            let layout = self.dashboard.conversation_layout_for(&id);
            self.set_workspace_layout(&id, layout);
        }
        self.acknowledge_visible_chats();
        for session_id in self.chats.keys().cloned().collect::<Vec<_>>() {
            self.capture_composer_draft(&session_id);
            self.save_question_draft(&session_id);
        }
        self.cancel_startup_session();
        self.defer_all_chat_opens();
        self.workspace_id = workspace_id.clone().unwrap_or_default();
        self.selection.clear();
        self.dashboard.set_active_workspace(workspace_id);
        self.go_selection_requested = None;
        // The tab switch swapped in the other workspace's arrangement, so
        // the conversations the previous one held are no longer in any pane.
        self.retire_chats_outside_the_layout();
        self.follow_selected_session();
    }

    pub(crate) fn session_in_active_workspace(&self, session_id: &str) -> bool {
        self.controller
            .state
            .sessions
            .get(session_id)
            .is_some_and(|session| {
                Some(session.workspace_id.as_str()) == self.dashboard.active_workspace_id()
            })
    }

    fn begin_shutdown(&mut self, detached: bool) {
        if self.shutdown_requested {
            return;
        }
        // A warm chat may be hidden while another session opens. Every
        // shutdown path must save every one of them, including global quit
        // and workspace switching, before the process-local composer cache
        // goes away.
        for session_id in self.chats.keys().cloned().collect::<Vec<_>>() {
            self.record_chat_detach(&session_id);
        }
        self.shutdown_requested = true;
        self.web_request_cancel = None;
        self.web_request_generation = self.web_request_generation.wrapping_add(1);
        self.quit_detached = detached;
        self.cancel_background_work();
        self.refresh_shutdown_notice();
    }

    /// Returns true once every user-authored mutation has reached a durable
    /// boundary. Pure reads and projections are deliberately not blockers.
    fn refresh_shutdown_notice(&mut self) -> bool {
        let blockers = self.critical_operations.blockers();
        if let Some(notice) = shutdown_wait_notice(&blockers) {
            self.dashboard.set_notice(notice);
            false
        } else {
            true
        }
    }

    /// Advances the read receipt of every conversation that was on screen.
    /// The frame records which those were; before the first frame it is the
    /// one the focused pane would draw.
    pub(super) fn acknowledge_visible_chats(&mut self) {
        let mut sessions = self.drawn_chat_sessions.clone();
        if let Some(session_id) = self.visible_chat().map(|chat| chat.session_id().to_owned())
            && !sessions.contains(&session_id)
        {
            sessions.push(session_id);
        }
        for session_id in sessions {
            self.acknowledge_chat(&session_id);
        }
    }

    fn acknowledge_chat(&mut self, session_id: &str) {
        let Some(through) = self
            .chats
            .get(session_id)
            .map(mj_chat::chat::ActiveChat::latest_event_ordinal)
        else {
            return;
        };
        let session_id = session_id.to_owned();
        let Some(session) = self.controller.state.sessions.get_mut(&session_id) else {
            return;
        };
        if through <= session.viewed_through_event_ordinal {
            return;
        }
        session.viewed_through_event_ordinal = through;
        self.dashboard.set_state(self.controller.state.clone());
        self.reconcile_question_drafts();
        if self.read_receipt_in_flight.is_some() {
            self.pending_read_receipts
                .entry(session_id)
                .and_modify(|pending| *pending = (*pending).max(through))
                .or_insert(through);
            return;
        }
        self.spawn_read_receipt(session_id, through);
    }

    fn acknowledge_dashboard_sessions(&mut self, receipts: Vec<(String, u64)>) {
        for (session_id, through) in receipts {
            let Some(session) = self.controller.state.sessions.get_mut(&session_id) else {
                continue;
            };
            if through <= session.viewed_through_event_ordinal {
                continue;
            }
            session.viewed_through_event_ordinal = through;
            self.pending_read_receipts
                .entry(session_id)
                .and_modify(|pending| *pending = (*pending).max(through))
                .or_insert(through);
        }
        self.dashboard.set_state(self.controller.state.clone());
        self.reconcile_question_drafts();
        if self.read_receipt_in_flight.is_none()
            && let Some((session_id, through)) = self.pending_read_receipts.pop_first()
        {
            self.spawn_read_receipt(session_id, through);
        }
    }

    fn spawn_read_receipt(&mut self, session_id: String, through: u64) {
        self.read_receipt_in_flight = Some(session_id.clone());
        io::spawn_read_receipt_persist(
            self.client_id.clone(),
            self.controller
                .state
                .sessions
                .get(&session_id)
                .map(|session| session.workspace_id.clone())
                .unwrap_or_else(|| self.workspace_id.clone()),
            session_id,
            through,
            self.dashboard_io_tx.clone(),
            self.critical_operations.clone(),
        );
    }

    pub(super) fn finish_read_receipt(
        &mut self,
        session_id: String,
        result: std::result::Result<u64, String>,
    ) {
        self.read_receipt_in_flight = None;
        if let Err(error) = result {
            self.dashboard.set_notice(format!(
                "Could not save read status for {}: {error}",
                short_id(&session_id)
            ));
        }
        if let Some((next_session, through)) = self.pending_read_receipts.pop_first() {
            self.spawn_read_receipt(next_session, through);
        }
    }

    /// Loads state, takes the terminal, and starts every background feed.
    /// `Ok(None)` means first-run setup was cancelled and there is nothing to
    /// run.
    fn open(
        workspace_id: &str,
        client_id: &str,
        daemon_presence: watch::Receiver<crate::daemon::DaemonPresence>,
    ) -> Result<Option<Self>> {
        let mut controller = Controller::load()?;
        retain_workspace_sessions(&mut controller, workspace_id, client_id)?;
        let workspaces = mj_controller::database::list_workspaces()?;
        let workspace_names = workspaces
            .iter()
            .map(|workspace| (workspace.id.clone(), workspace.name.clone()))
            .collect();
        let layouts = workspaces
            .iter()
            .map(|workspace| {
                mj_controller::database::load_workspace_pane_sizes(&workspace.id)
                    .map(|sizes| (workspace.id.clone(), sizes))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let conversation_layouts = workspaces
            .iter()
            .map(|workspace| {
                mj_controller::database::load_workspace_layout(&workspace.id)
                    .map(|layout| (workspace.id.clone(), layout))
            })
            .collect::<Result<BTreeMap<String, ConversationLayout>>>()?;
        let mut dashboard = DashboardState::new(
            controller.config.clone(),
            controller.state.clone(),
            BTreeMap::new(),
        );
        dashboard.set_workspace_names(workspace_names);
        for (id, sizes) in &layouts {
            dashboard.cache_workspace_pane_sizes(id, *sizes);
        }
        for (id, layout) in &conversation_layouts {
            dashboard.cache_workspace_layout(id, layout.clone());
        }
        dashboard.set_active_workspace(Some(workspace_id.to_owned()));
        let notices = mj_chat::chat::Notices::default();
        dashboard.share_notices(notices.clone());
        for (session_id, queued) in projected_queued_prompts(&controller)? {
            dashboard.apply_queued_prompts(&session_id, queued);
        }
        let terminal = TerminalGuard::enter()?;
        if configuration_needs_setup(&controller.config) {
            dashboard.begin_setup();
        }

        let (quota_profiles_tx, quota_updates_rx) = spawn_quota_refresher();
        let remote_worker = spawn_remote_dashboard_worker_poller(workspace_id.to_owned())?;
        let worker_targets_tx = remote_worker.targets;
        let worker_updates_rx = remote_worker.updates;
        let worker_commands_tx = remote_worker.control;
        let worker_shutdown = remote_worker.shutdown;
        let runtime_state_rx = remote_worker.state;
        dashboard.set_move_operations(runtime_state_rx.borrow().moves.clone());
        let runtime_reviews_rx = remote_worker.reviews;
        let runtime_review_views: BTreeMap<String, mj_controller::review_host::RuntimeReviewView> =
            runtime_reviews_rx
                .borrow()
                .iter()
                .cloned()
                .map(|review| (review.session_id.clone(), review))
                .collect();
        dashboard.set_session_reviews(runtime_review_views.values().cloned());
        let runtime_notices_rx = remote_worker.notices;
        // Notices already queued when this surface attaches belong to whatever
        // was on screen before it, so start after them rather than replaying
        // them into the notice bar.
        let reported_notice_id = runtime_notices_rx
            .borrow()
            .iter()
            .map(|notice| notice.id)
            .max();
        let runtime_config_rx = remote_worker.config;
        worker_targets_tx.send_replace(dashboard_worker_targets(&controller));
        let (lifecycle_updates_tx, lifecycle_updates_rx) =
            tokio::sync::mpsc::unbounded_channel::<LifecycleUpdate>();
        let (critical_operations, critical_operations_changed) = CriticalOperationTracker::new();
        let lifecycle_operations = BTreeMap::<String, ActiveLifecycleOperation>::new();
        let credential_sync = CredentialSyncCoordinator::spawn();
        let credential_sync_handle = credential_sync.handle();
        let (resource_targets_tx, resource_triggers_tx, resource_updates_rx) =
            spawn_dashboard_resource_poller();
        let (capacity_targets_tx, capacity_triggers_tx, capacity_updates_rx) =
            spawn_dashboard_capacity_poller();
        let (aws_resource_options_tx, aws_resource_options_rx) =
            tokio::sync::mpsc::unbounded_channel::<AwsResourceOptions>();
        refresh_dashboard_poll_targets(
            &controller,
            &worker_targets_tx,
            &resource_targets_tx,
            &credential_sync_handle,
            &lifecycle_operations.keys().cloned().collect(),
        );
        let capacity_targets = controller.deployment_capacity_targets();
        capacity_targets_tx.send_replace(capacity_targets.clone());
        dashboard.set_deployment_capacity_targets(capacity_targets);
        let (import_updates_tx, import_updates_rx) =
            tokio::sync::mpsc::channel::<(u64, ImportProfileOption)>(32);
        let (import_task_tx, import_task_rx) =
            tokio::sync::mpsc::channel::<DashboardImportUpdate>(8);
        let (dashboard_io_tx, dashboard_io_rx) =
            tokio::sync::mpsc::unbounded_channel::<DashboardIoUpdate>();
        let known_workspace_layouts = layouts.keys().cloned().collect();
        let pane_size_persistence =
            workspace_settings::WorkspaceSettingPersistence::start_pane_sizes(
                layouts,
                notices.clone(),
            );
        let layout_persistence = workspace_settings::WorkspaceSettingPersistence::start_layouts(
            conversation_layouts.clone(),
            notices.clone(),
        );

        let mut context = Self {
            terminal,
            controller,
            workspace_id: workspace_id.to_owned(),
            client_id: client_id.to_owned(),
            dashboard,
            pane_size_persistence,
            layout_persistence,
            known_workspace_layouts,
            workspace_layouts: conversation_layouts,
            notices,
            events: Some(event::EventStream::new()),
            chats: BTreeMap::new(),
            question_drafts: BTreeMap::new(),
            composer_drafts: ComposerDraftCache::default(),
            draft_save_failures: BTreeMap::new(),
            opening_chat_sessions: BTreeMap::new(),
            attachments: BTreeMap::new(),
            drawn_chat_sessions: Vec::new(),
            startup: StartupSession::idle(),
            go_context_refresh: None,
            go_context_in_flight: false,
            go_selection_requested: None,
            go_selection_in_flight: false,
            drawn_notice_generation: 0,
            controller_changed: true,
            quit_detached: false,
            shutdown_requested: false,
            critical_operations,
            critical_operations_changed,
            daemon_presence,
            quota_profiles_tx,
            quota: Feed::new(quota_updates_rx),
            manual_quota_refresh_generation: None,
            target_test_cancel: None,
            path_input_job: None,
            review_discovery_cancel: None,
            session_preflight_cancel: None,
            worker_targets_tx,
            worker: Feed::new(worker_updates_rx),
            runtime_state: Feed::new(runtime_state_rx),
            runtime_reviews: Feed::new(runtime_reviews_rx),
            runtime_notices: Feed::new(runtime_notices_rx),
            reported_notice_id,
            runtime_review_views,
            runtime_config: Feed::new(runtime_config_rx),
            config_reload_in_flight: false,
            remote_lifecycle_sessions: BTreeSet::new(),
            remote_lifecycle_operations: BTreeMap::new(),
            runtime_state_revision: 0,
            worker_commands_tx,
            worker_shutdown: Some(worker_shutdown),
            worker_diagnoses: WorkerDiagnosisTracker::default(),
            lifecycle_updates_tx,
            lifecycle: Feed::new(lifecycle_updates_rx),
            lifecycle_operations,
            credential_sync: Feed::new(credential_sync),
            credential_sync_handle,
            credential_sync_signals: CredentialSyncSignalTracker::default(),
            credential_sync_notices: CredentialSyncNotices::default(),
            resource_targets_tx,
            resource_triggers_tx,
            resource: Feed::new(resource_updates_rx),
            capacity_targets_tx,
            capacity_triggers_tx,
            capacity: Feed::new(capacity_updates_rx),
            aws_resource_options_tx,
            aws_options: Feed::new(aws_resource_options_rx),
            resolving_aws_resource_options: BTreeSet::new(),
            wiki_search_request: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            import_updates_tx,
            import_profiles: Feed::new(import_updates_rx),
            import_task_tx,
            import_tasks: Feed::new(import_task_rx),
            pending_import: None,
            import_discovery_id: 0,
            native_scan_cache: mj_controller::import::NativeScanCache::new(),
            next_import_task_id: 0,
            active_import: None,
            clipboard_read_in_flight: false,
            selection: SelectionState::new(),
            selection_text: None,
            dashboard_io_tx,
            dashboard_io: Feed::new(dashboard_io_rx),
            web_request_generation: 0,
            web_request_cancel: None,
            materialized_projection_permits: Arc::new(tokio::sync::Semaphore::new(2)),
            materialized_projections_in_flight: BTreeSet::new(),
            pending_materialized_projections: BTreeMap::new(),
            project_sources_in_flight: BTreeSet::new(),
            read_receipt_in_flight: None,
            pending_read_receipts: BTreeMap::new(),
            checkpoint_archive_targets_seen: BTreeMap::new(),
            checkpoint_archive_generation: 0,
        };
        context.resolve_project_sources();
        context.hydrate_stored_session_summaries();
        context.request_quota_refresh();
        Ok(Some(context))
    }
}

/// Background events in a warm, hidden chat have not been read.
fn detach_read_frontier(visible: bool, latest: u64, acknowledged: u64) -> u64 {
    if visible {
        latest.max(acknowledged)
    } else {
        acknowledged
    }
}

fn question_draft_projection_is_current(
    captured_event_ordinal: u64,
    projection_ordinal: u64,
) -> bool {
    projection_ordinal >= captured_event_ordinal
}

/// Whether a left press belongs to the visible question rather than to a
/// genuine dashboard modal. `ElicitationMessage` is the question's marker;
/// its presence lets the generic `ModalBody` surface cover the answer area
/// without making every dashboard dialog focus the composer.
fn question_click_focuses(
    dashboard_modal_open: bool,
    surfaces: &FrameSurfaces,
    mouse: &MouseEvent,
) -> bool {
    !dashboard_modal_open
        && surfaces.surface(SurfaceId::ElicitationMessage).is_some()
        && surfaces
            .surface_at(mouse.column, mouse.row)
            .is_some_and(|surface| {
                matches!(
                    surface.id,
                    SurfaceId::ElicitationMessage | SurfaceId::ModalBody
                )
            })
}

/// Prompt focus follows the press, while selection still owns the gesture.
fn pointer_press_focuses_prompt(
    dashboard_modal_open: bool,
    surfaces: &FrameSurfaces,
    mouse: &MouseEvent,
) -> bool {
    question_click_focuses(dashboard_modal_open, surfaces, mouse)
        || (!dashboard_modal_open
            && surfaces
                .surface_at(mouse.column, mouse.row)
                .is_some_and(|surface| surface.id == SurfaceId::PromptInput))
}

fn route_prompt_selection(
    selection: &mut SelectionState,
    dashboard: &mut DashboardState,
    event: Event,
) -> SelectionRouting {
    if let Event::Mouse(mouse) = &event
        && mouse.kind == MouseEventKind::Down(MouseButton::Left)
        && pointer_press_focuses_prompt(dashboard.modal_open(), dashboard.frame_surfaces(), mouse)
    {
        dashboard.focus_prompt();
    }
    route_selection_event(selection, dashboard.frame_surfaces(), event)
}

/// A lifecycle started by another attached surface reaches this UI through
/// the daemon feed rather than its local action path. Give the open chat the
/// same early retirement signal so its closing relay feed is not reported as
/// a lost connection while the session row already says Stopping.
fn mark_active_chat_retiring_for_remote_lifecycle(
    active_chat: Option<&mut mj_chat::chat::ActiveChat>,
    session_id: &str,
    kind: SessionOperationKind,
) {
    if matches!(
        kind,
        SessionOperationKind::Stopping
            | SessionOperationKind::Destroying
            | SessionOperationKind::Moving
    ) {
        actions::mark_active_chat_retiring(active_chat, session_id);
    }
}

/// The next terminal event. Cancel-safe, so losing the `select!` race cannot
/// drop one. A missing stream is never ready: it is absent only while the setup
/// dialog owns the terminal, which is not while this loop is waiting.
async fn next_terminal_event(
    events: &mut Option<event::EventStream>,
) -> Option<std::io::Result<Event>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// Builds a chat view for one session: its identity, the other sessions it
/// reports activity for, and its recovery context.
/// Records what leaving a chat produced — how far the user has read and the
/// input they left unsent — and persists both in the background. A missing
/// session is reported rather than fatal: the session itself is unaffected.
///
/// The returned handle lets the quit path wait for the write. `None` means
/// nothing was queued.
struct DetachedChatState<'a> {
    client_id: &'a str,
    session_id: &'a str,
    event_ordinal: u64,
    draft: DetachedSessionDraft,
}

fn record_chat_detach_state(
    controller: &mut Controller,
    dashboard: &mut DashboardState,
    detached: DetachedChatState<'_>,
    updates: &UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(session) = controller.state.sessions.get_mut(detached.session_id) else {
        dashboard.set_notice(format!(
            "Could not save draft and read status for {}: unknown session",
            short_id(detached.session_id)
        ));
        return None;
    };
    session.viewed_through_event_ordinal = session
        .viewed_through_event_ordinal
        .max(detached.event_ordinal);
    let workspace_id = session.workspace_id.clone();
    dashboard.set_state(controller.state.clone());
    dashboard.clear_notice();
    Some(io::spawn_detached_session_state_persist(
        detached.client_id.to_owned(),
        workspace_id,
        detached.session_id.to_owned(),
        detached.event_ordinal,
        detached.draft,
        updates.clone(),
        tracker,
    ))
}

fn opening_cancel_event(event: &Event, opening: bool, modal: bool) -> bool {
    opening
        && !modal
        && matches!(event, Event::Key(key) if key.code == KeyCode::Esc && key.kind != KeyEventKind::Release)
}

/// Whether the warm chat belongs on screen, given the session an attach is
/// running for.
///
/// Only an attach for a *different* session hides it. An attach for the chat
/// already loaded is a reattach of the same conversation, and blanking the
/// transcript for that would be a flicker rather than a correction.
/// Waits for the next background message of any warm conversation.
///
/// [`mj_chat::chat::ActiveChat::pump`] is cancel safe, so the futures that
/// lose this race are dropped without losing what they were waiting for. With
/// no warm chat there is nothing to wait for, and the arm never fires.
async fn pump_chats(chats: &mut BTreeMap<String, mj_chat::chat::ActiveChat>) {
    if chats.is_empty() {
        return std::future::pending().await;
    }
    let pumps = chats
        .values_mut()
        .map(|chat| Box::pin(mj_chat::chat::ActiveChat::pump(Some(chat))))
        .collect::<Vec<_>>();
    futures::future::select_all(pumps).await;
}

fn chat_is_visible(opening: Option<&str>, chat_session_id: &str) -> bool {
    !matches!(opening, Some(opening) if opening != chat_session_id)
}

/// Hands one event to the part of the surface it belongs to, and reports
/// whether the loop may keep batching, which it may while the event asked for
/// no work.
///
/// A mouse event goes where the pointer is, not where the keyboard is: the
/// wheel over the transcript scrolls the transcript even while a pane has
/// focus, and a click there hands the keyboard back to the composer. Keys go
/// to the modal if one is open, then to the composer if it has focus, and
/// otherwise to the panes.
///
/// Reports whether the event was consumed, and whether the input batch may
/// continue: an event that produced work for the loop to run ends the batch so
/// dispatch still follows input order.
fn dispatch_event(
    context: &mut DashboardContext,
    event: Event,
    action: &mut DashboardAction,
    chat_outcome: &mut mj_chat::chat::ChatEventOutcome,
) -> (bool, bool) {
    let chat_modal = !context.dashboard.modal_open()
        && context
            .visible_chat()
            .is_some_and(|chat| chat.component_modal_open());
    let dashboard_pointer = !chat_modal
        && matches!(&event, Event::Mouse(mouse) if context.dashboard.component_handles_mouse(*mouse));
    let to_chat = !dashboard_pointer
        && (chat_modal
            || match &event {
                Event::Mouse(mouse) if !context.dashboard.modal_open() => {
                    let over_chat = context
                        .dashboard
                        .chat_region_contains(mouse.column, mouse.row);
                    if over_chat && mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                        context.dashboard.focus_prompt();
                    }
                    over_chat
                        || context
                            .visible_chat()
                            .is_some_and(|chat| chat.component_handles_mouse(*mouse))
                        || (matches!(
                            mouse.kind,
                            MouseEventKind::Drag(MouseButton::Left)
                                | MouseEventKind::Up(MouseButton::Left)
                        ) && context
                            .visible_chat()
                            .is_some_and(|chat| chat.transcript_scrollbar_dragging()))
                }
                _ => !context.dashboard.modal_open() && context.dashboard.prompt_has_focus(),
            });
    match context.visible_chat().filter(|_| to_chat) {
        Some(chat) => {
            let result = chat.handle_event_result(event);
            *chat_outcome = result
                .action
                .unwrap_or(mj_chat::chat::ChatEventOutcome::None);
            (
                result.consumed,
                matches!(*chat_outcome, mj_chat::chat::ChatEventOutcome::None),
            )
        }
        None => {
            let preflight_generation = context.dashboard.session_preflight_generation();
            let result = context.dashboard.handle_event_result(event);
            *action = result.action.unwrap_or(DashboardAction::None);
            if context.dashboard.session_preflight_generation() != preflight_generation {
                context.cancel_session_preflight();
            }
            context.controller_changed |= !matches!(*action, DashboardAction::None);
            (result.consumed, matches!(*action, DashboardAction::None))
        }
    }
}

/// Highlights the live selection on the frame the view just drew and returns
/// the text it covers.
///
/// Both halves read the same frame, so the copied text is exactly what the
/// highlight marks. Surfaces that scroll their own content extract from their
/// row cache instead: only the visible band of such a selection is on this
/// frame, and the highlight is all that band is good for.
fn draw_selection(
    frame: &mut ratatui::Frame,
    selection: &SelectionState,
    surfaces: &FrameSurfaces,
) -> Option<String> {
    let id = selection.active_surface()?;
    let range = selection.range()?;
    let surface = *surfaces.surface(id)?;
    mj_chat::selection::highlight(frame.buffer_mut(), &surface, &range);
    if matches!(
        id,
        SurfaceId::Transcript | SurfaceId::ElicitationMessage | SurfaceId::ReviewerTranscript
    ) {
        return None;
    }
    Some(mj_chat::selection::extract_rows(
        frame.buffer_mut(),
        &surface,
        &range,
    ))
}

/// What the selection engine decided about one terminal event.
#[derive(Debug, PartialEq, Eq)]
enum SelectionRouting {
    /// The engine took the event; the frame only needs redrawing.
    Consumed,
    /// The engine wants nothing to do with this event; hand it to the view.
    /// A release that never dragged arrives here as the press the view's
    /// click handling expects.
    Forward(Event),
    /// A drag finished. The caller extracts the selected text and copies it.
    Copy {
        surface: SurfaceId,
        range: SelectionRange,
    },
}

/// Routes one terminal event between the selection engine and the view.
///
/// The engine only claims left-button gestures that start on a registered
/// surface: a press elsewhere, the wheel, and every other mouse kind stay the
/// view's. Esc drops a finished selection instead of reaching the view, so the
/// key that clears the highlight cannot also quit or cancel.
fn route_selection_event(
    selection: &mut SelectionState,
    surfaces: &FrameSurfaces,
    event: Event,
) -> SelectionRouting {
    match event {
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                selection.on_mouse_down(mouse.column, mouse.row, surfaces);
                if selection.active_surface().is_some() {
                    SelectionRouting::Consumed
                } else {
                    SelectionRouting::Forward(Event::Mouse(mouse))
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if selection.active_surface().is_some() => {
                selection.on_mouse_drag(mouse.column, mouse.row, surfaces);
                SelectionRouting::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) if selection.active_surface().is_some() => {
                match selection.on_mouse_up(mouse.column, mouse.row, surfaces) {
                    // The views key their click and double-click handling on
                    // presses, so a click that the engine held back is
                    // replayed as one when the button comes up.
                    SelectionAction::Click { column, row } => {
                        SelectionRouting::Forward(Event::Mouse(MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column,
                            row,
                            modifiers: KeyModifiers::NONE,
                        }))
                    }
                    SelectionAction::CopyRequested { surface, range } => {
                        SelectionRouting::Copy { surface, range }
                    }
                    SelectionAction::None => SelectionRouting::Consumed,
                }
            }
            _ => SelectionRouting::Forward(Event::Mouse(mouse)),
        },
        Event::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key.code == KeyCode::Esc
                && key.modifiers == KeyModifiers::NONE
                && selection.range().is_some() =>
        {
            selection.clear();
            SelectionRouting::Consumed
        }
        event => SelectionRouting::Forward(event),
    }
}

#[cfg(test)]
fn dashboard_event_action(dashboard: &mut DashboardState, event: Event) -> DashboardAction {
    dashboard
        .handle_event_result(event)
        .action
        .unwrap_or(DashboardAction::None)
}

/// The global chord this event runs, if any.
///
/// A handful of commands answer from every surface, including while the
/// composer owns the keyboard, so they are caught here before the event is
/// routed to a pane or to the chat. Which chords survive an open dialog is
/// [`DashboardState::global_chord_allowed`]'s question, not this one's.
fn global_chord_event(dashboard: &DashboardState, event: &Event) -> Option<CommandId> {
    let Event::Key(key) = event else {
        return None;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let id = mj_tui::global_chord(key)?;
    dashboard.global_chord_allowed(id).then_some(id)
}

/// Applies the one global command whose direction depends on the pressed
/// event. Keeping this branch shared with the event loop makes Shift-F6 test
/// the same reverse path as the live dashboard, rather than only testing key
/// registration in `mj-tui`.
fn apply_global_focus_cycle(
    dashboard: &mut DashboardState,
    event: &Event,
    command: CommandId,
) -> bool {
    if command != CommandId::CycleFocus {
        return false;
    }
    let reverse = matches!(
        event,
        Event::Key(key) if key.modifiers.contains(KeyModifiers::SHIFT)
    );
    dashboard.cycle_focus(reverse);
    true
}

pub(crate) fn resume_progress_notice(
    session_id: &str,
    profile_id: &str,
    target_id: &str,
) -> String {
    format!(
        "Preparing {}: verifying checkpoint, provisioning {target_id}, and restoring {profile_id}…",
        short_id(session_id)
    )
}

fn configuration_needs_setup(config: &Config) -> bool {
    config.is_unconfigured()
}

#[cfg(test)]
mod tests;
