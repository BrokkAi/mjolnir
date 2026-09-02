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
pub(crate) mod io;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use hel::hel_config::{HelConfig, config_path};
use hel::hel_controller::Controller;
use hel::hel_credentials::CredentialSyncHandle;
use hel::hel_selection::{
    FrameSurfaces, SelectionAction, SelectionRange, SelectionState, SurfaceId,
};
use hel::hel_session_manager::{
    SessionManagerControl, SessionManagerShutdown, SessionManagerUpdates, ViewError,
};
use hel::hel_setup::{SetupOutcome, run_setup_dialog};
use hel::hel_state::{MaterializedSession, SessionRecord, SessionResourceAllocation, SessionState};
use hel::hel_targets::DeploymentCapacityTarget;
use hel::hel_worker_client::CredentialSyncCoordinator;
use hel_tui::{
    CommandId, DashboardAction, DashboardState, ImportProfileOption,
    PreparedMaterializedSessionDetail, SessionOperationKind, render_combined,
    resume_profile_placeholders,
};
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_stream::StreamExt as _;

use crate::dashboard::io::{
    ActiveLifecycleOperation, DashboardIoUpdate, LifecycleReload, checkpoint_archive_targets,
    spawn_checkpoint_archive_size_refresh, spawn_clipboard_write, spawn_io, spawn_lifecycle_reload,
    spawn_materialized_session_projection, spawn_project_source_resolution,
    spawn_stored_session_summary,
};
use crate::import::{
    DashboardImportRequest, DashboardImportTaskResult, DashboardImportUpdate,
    PendingDashboardImport, spawn_dashboard_import,
};
use crate::pollers::{
    CapacityPollUpdate, CredentialSyncNotices, CredentialSyncSignalTracker, Feed, LifecycleUpdate,
    QuotaRefreshBatch, QuotaUpdate, ResourcePollTarget, ResourcePollUpdate, WorkerDiagnosisTracker,
    WorkerPollTarget, apply_worker_poll_update, complete_manual_quota_refresh,
    dashboard_worker_targets, projected_queued_prompts, quota_refresh_profiles,
    refresh_dashboard_poll_targets, schedule_due_credential_syncs, session_target_is_pollable,
    spawn_dashboard_capacity_poller, spawn_dashboard_resource_poller, spawn_quota_refresher,
    spawn_remote_dashboard_worker_poller, spawn_worker_diagnosis,
};
use crate::{TerminalGuard, short_id};

/// Redraw cadence for displays that move with the wall clock: turn timers,
/// countdowns, and elapsed times.
const DASHBOARD_CLOCK_TICK: Duration = Duration::from_secs(1);

/// How long the surface waits for stored session summaries before it opens a
/// conversation anyway. The summaries decide which session has the newest
/// activity; a stalled read must not leave the screen without a conversation.
const STARTUP_SESSION_WAIT: Duration = Duration::from_secs(2);

/// How long the startup pick keeps trying to open the conversation it chose.
///
/// Attaching needs the session manager to be managing that session, and the
/// manager adopts sessions asynchronously after the surface starts, so the
/// first attempt usually lands before it is ready and fails with "not
/// managed". Retrying on the clock tick rides that out; the window bounds it
/// so a session that never becomes managed cannot retry for ever.
const STARTUP_SESSION_ATTACH_WINDOW: Duration = Duration::from_secs(30);

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
    /// The conversation the pick chose and is waiting on, so an attach that
    /// lands before the session manager is ready can be tried again.
    attempting: Option<String>,
    /// When to stop retrying that attach.
    give_up_at: std::time::Instant,
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
            attempting: None,
            give_up_at: now,
        }
    }

    fn begin(session_ids: impl IntoIterator<Item = String>, now: std::time::Instant) -> Self {
        let pending_summaries = session_ids.into_iter().collect::<BTreeSet<_>>();
        Self {
            open_pending: !pending_summaries.is_empty(),
            pending_summaries,
            deadline: now + STARTUP_SESSION_WAIT,
            attempting: None,
            give_up_at: now + STARTUP_SESSION_ATTACH_WINDOW,
        }
    }

    /// One session has answered, whether its summary loaded or failed.
    fn summary_arrived(&mut self, session_id: &str) {
        self.pending_summaries.remove(session_id);
    }

    /// The user acted, so the choice is theirs now.
    fn cancel(&mut self) {
        self.open_pending = false;
        self.attempting = None;
    }

    /// Records which conversation the pick is opening, so a failed attach can
    /// be recognised as this one's.
    fn attempting(&mut self, session_id: &str) {
        self.attempting = Some(session_id.to_owned());
    }

    /// One attach failed. Returns whether the pick will try again, which is
    /// also whether the failure is worth reporting: a manager that has not
    /// adopted the session yet resolves itself, and saying so every second
    /// would bury the notice bar in noise.
    fn attach_failed(&mut self, session_id: &str, now: std::time::Instant) -> bool {
        if self.attempting.as_deref() != Some(session_id) {
            return false;
        }
        if now >= self.give_up_at {
            self.attempting = None;
            return false;
        }
        self.open_pending = true;
        true
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
    sessions: impl IntoIterator<Item = &'a SessionRecord>,
    activity_at_ms: impl Fn(&str) -> Option<u64>,
) -> Option<String> {
    sessions
        .into_iter()
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
/// Redraw cadence while a dialog animates on its own.
const IMPORT_PROGRESS_TICK: Duration = Duration::from_millis(125);
/// Scroll cadence while a drag is held past a scrollable surface's edge.
const SELECTION_AUTOSCROLL_TICK: Duration = Duration::from_millis(80);
pub(crate) const QUOTA_REFRESH_NOTICE: &str = "Refreshing profile quotas…";
pub(crate) const QUOTA_REFRESHED_NOTICE: &str = "Profile quotas refreshed.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DashboardExit {
    Normal,
    Detached,
    Interrupted,
    WorkspacePicker,
}

#[derive(Clone)]
pub(crate) struct CriticalOperationTracker {
    inner: Arc<CriticalOperationTrackerInner>,
}

struct CriticalOperationTrackerInner {
    next_id: AtomicU64,
    operations: Mutex<BTreeMap<u64, CriticalOperation>>,
    changed: watch::Sender<u64>,
}

struct CriticalOperation {
    label: String,
    cancelled: Option<Arc<AtomicBool>>,
}

pub(crate) struct CriticalOperationGuard {
    id: u64,
    tracker: CriticalOperationTracker,
}

impl CriticalOperationTracker {
    fn new() -> (Self, watch::Receiver<u64>) {
        let (changed, receiver) = watch::channel(0);
        (
            Self {
                inner: Arc::new(CriticalOperationTrackerInner {
                    next_id: AtomicU64::new(1),
                    operations: Mutex::new(BTreeMap::new()),
                    changed,
                }),
            },
            receiver,
        )
    }

    pub(crate) fn begin(&self, label: impl Into<String>) -> CriticalOperationGuard {
        self.begin_inner(label.into(), None)
    }

    pub(crate) fn begin_cancellable(
        &self,
        label: impl Into<String>,
        cancelled: Arc<AtomicBool>,
    ) -> CriticalOperationGuard {
        self.begin_inner(label.into(), Some(cancelled))
    }

    fn begin_inner(
        &self,
        label: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> CriticalOperationGuard {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .insert(id, CriticalOperation { label, cancelled });
        self.inner.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
        CriticalOperationGuard {
            id,
            tracker: self.clone(),
        }
    }

    fn blockers(&self) -> Vec<String> {
        self.inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .values()
            .map(|operation| operation.label.clone())
            .collect()
    }

    fn cancel_all(&self) {
        for operation in self
            .inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .values()
        {
            if let Some(cancelled) = operation.cancelled.as_ref() {
                cancelled.store(true, Ordering::Release);
            }
        }
    }
}

impl Drop for CriticalOperationGuard {
    fn drop(&mut self) {
        self.tracker
            .inner
            .operations
            .lock()
            .expect("critical operation tracker lock")
            .remove(&self.id);
        self.tracker.inner.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

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

/// Everything one dashboard run owns.
pub(crate) struct DashboardContext {
    terminal: TerminalGuard,
    pub(crate) controller: Controller,
    pub(crate) workspace_id: String,
    pub(crate) client_id: String,
    pub(crate) dashboard: DashboardState,
    /// One notifications bar for the whole process: the dashboard and every
    /// chat view opened from it report through this shared handle.
    notices: hel::hel_chat::Notices,
    /// Absent only while the setup dialog owns the terminal; see
    /// [`DashboardContext::run_setup_dialog`].
    events: Option<event::EventStream>,
    /// The conversation on screen. One chat stays warm at a time; its feeds
    /// keep running while another pane has the keyboard, so switching back is
    /// a redraw rather than a rebuild.
    pub(crate) active_chat: Option<hel::hel_chat::ActiveChat>,
    /// Session-manager attachment is asynchronous: an actor may need to
    /// answer from a worker or relay before a chat can be built.
    pub(crate) opening_chat_session: Option<String>,
    /// The conversation the selection has moved on to while an attach is still
    /// in flight. Only the newest is kept: walking the session list must not
    /// queue an attach per row it passes through.
    pending_chat_session: Option<String>,
    /// Which conversation the surface opens on, and whether it is still the
    /// surface's choice to make.
    startup: StartupSession,
    /// The first pass always draws; after that a redraw needs a wakeup.
    pub(crate) dirty: bool,
    /// The notice generation the frame on screen was drawn from. Background
    /// work writes the shared slot without touching `dirty`, so the draw
    /// compares against this rather than trusting every setter to ask for a
    /// frame.
    drawn_notice_generation: u64,
    /// The poll targets are recomputed only after the controller may have
    /// changed.
    pub(crate) controller_changed: bool,
    pub(crate) quit_detached: bool,
    workspace_switch_requested: bool,
    shutdown_requested: bool,
    pub(crate) critical_operations: CriticalOperationTracker,
    critical_operations_changed: watch::Receiver<u64>,

    quota_profiles_tx: watch::Sender<QuotaRefreshBatch>,
    quota: Feed<Receiver<QuotaUpdate>>,
    pub(crate) manual_quota_refresh_generation: Option<u64>,
    pub(crate) target_test_cancel: Option<Arc<AtomicBool>>,

    worker_targets_tx: watch::Sender<Vec<WorkerPollTarget>>,
    worker: Feed<SessionManagerUpdates>,
    runtime_lifecycles: Feed<watch::Receiver<Vec<crate::daemon::RuntimeLifecycleView>>>,
    /// Reviews the daemon is running. The chat renders one of these rather
    /// than driving a review of its own.
    runtime_reviews: Feed<watch::Receiver<Vec<hel::hel_review::host::RuntimeReviewView>>>,
    /// Last complete review projection, retained even while the session list
    /// is on screen so a subsequently opened chat starts in the right state.
    runtime_review_views: BTreeMap<String, hel::hel_review::host::RuntimeReviewView>,
    runtime_config: Feed<watch::Receiver<HelConfig>>,
    runtime_records: Feed<watch::Receiver<Vec<SessionRecord>>>,
    config_reload_in_flight: bool,
    remote_lifecycle_sessions: BTreeSet<String>,
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

    pub(crate) dashboard_io_tx: UnboundedSender<DashboardIoUpdate>,
    dashboard_io: Feed<UnboundedReceiver<DashboardIoUpdate>>,
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
    workspace_id: &str,
    client_id: &str,
) -> Result<()> {
    let session_ids = hel::hel_database::session_ids_for_workspace(workspace_id)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    controller
        .state
        .sessions
        .retain(|session_id, _| session_ids.contains(session_id));
    for session in controller.state.sessions.values_mut() {
        let frontier =
            hel::hel_database::client_read_frontier(client_id, workspace_id, &session.id)?;
        session.viewed_through_event_ordinal = frontier;
    }
    Ok(())
}

pub(crate) async fn run_dashboard_for_workspace(
    workspace_id: &str,
    client_id: &str,
) -> Result<DashboardExit> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        println!("Welcome to Mjolnir");
        println!("Run `mj doctor` for non-interactive validation.");
        return Ok(DashboardExit::Normal);
    }

    let Some(mut context) = DashboardContext::open(workspace_id, client_id)? else {
        return Ok(DashboardExit::Normal);
    };
    let termination = hel::termination::Coordinator::install().token();
    // `interval_at` so the first tick is a period away rather than immediate,
    // and `Delay` so a tick that was gated off does not fire a burst to catch
    // up when it comes back.
    let mut clock_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + DASHBOARD_CLOCK_TICK,
        DASHBOARD_CLOCK_TICK,
    );
    clock_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut import_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + IMPORT_PROGRESS_TICK,
        IMPORT_PROGRESS_TICK,
    );
    import_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Only armed while a held pointer sits at a scrollable surface's edge, so
    // an idle dashboard never ticks on it.
    let mut autoscroll_tick = tokio::time::interval(SELECTION_AUTOSCROLL_TICK);
    autoscroll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if !context.shutdown_requested {
            context.refresh_controller_derived_state();
        }
        context.draw()?;
        let mut action = DashboardAction::None;
        let mut chat_outcome = hel::hel_chat::ChatEventOutcome::None;
        // The winning arm takes the message that woke the loop; the drains
        // below batch whatever is queued behind it, so one wakeup is one draw.
        tokio::select! {
            _ = termination.cancelled(), if !context.shutdown_requested => {
                context.begin_shutdown(false);
            }
            event = next_terminal_event(&mut context.events) => {
                let Some(event) = event else { break };
                if context.shutdown_requested {
                    context.dirty = true;
                    continue;
                }
                let mut event = event?;
                // The user acting is the strongest signal there is about which
                // conversation they want, so it takes the choice away from the
                // startup pick before that pick can override their input.
                if matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
                    context.cancel_startup_session();
                }
                // Key repeats and pastes arrive as several ready events. The
                // buffered ones are handled before drawing, but the first event
                // that asks for work ends the batch so that dispatch still
                // follows input order.
                loop {
                    let batched = if let Some(command) =
                        global_chord_event(&context.dashboard, &event)
                    {
                        // Detaching from an open conversation has to save the
                        // draft and the read cursor, which is the chat's own
                        // bookkeeping rather than the dashboard's.
                        if command == CommandId::QuitDetach
                            && let Some(chat) = context.active_chat.as_mut()
                        {
                            chat_outcome = chat.detach();
                        } else {
                            action = context.dashboard.dispatch_command(command);
                        }
                        false
                    } else {
                        // Selection sees mouse and Esc first: a drag inside a
                        // pane belongs to the engine, and everything else
                        // comes back out for the view.
                        match context.route_selection(event) {
                            SelectionRouting::Consumed => true,
                            SelectionRouting::Copy { surface, range } => {
                                context.copy_selection(surface, range)?;
                                true
                            }
                            SelectionRouting::Forward(event) => dispatch_event(
                                &mut context,
                                event,
                                &mut action,
                                &mut chat_outcome,
                            ),
                        }
                    };
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
                context.dirty = true;
            }
            // The warm chat's own feeds: remote command results, its clipboard
            // and history I/O, dictation, and the session view. They run
            // whether or not the chat is on screen, which is what keeps an
            // off-screen chat current.
            () = hel::hel_chat::ActiveChat::pump(context.active_chat.as_mut()) => {
                // The conversation is always on screen, so its own feeds
                // always redraw it and always advance its read receipt.
                context.dirty = true;
                context.acknowledge_visible_chat();
            }
            update = context.quota.wait(), if context.quota.is_open() => {
                let woke = context.quota.accept(update);
                context.dirty |= woke;
            }
            update = context.worker.wait(), if context.worker.is_open() => {
                let woke = context.worker.accept(update);
                context.dirty |= woke;
            }
            update = context.runtime_lifecycles.wait(), if context.runtime_lifecycles.is_open() => {
                let woke = context.runtime_lifecycles.accept(update);
                context.dirty |= woke;
            }
            update = context.runtime_reviews.wait(), if context.runtime_reviews.is_open() => {
                let woke = context.runtime_reviews.accept(update);
                context.dirty |= woke;
            }
            update = context.runtime_config.wait(), if context.runtime_config.is_open() => {
                let woke = context.runtime_config.accept(update);
                context.dirty |= woke;
            }
            update = context.runtime_records.wait(), if context.runtime_records.is_open() => {
                let woke = context.runtime_records.accept(update);
                context.dirty |= woke;
            }
            result = context.credential_sync.wait(), if context.credential_sync.is_open() => {
                let woke = context.credential_sync.accept(result);
                context.dirty |= woke;
            }
            update = context.resource.wait(), if context.resource.is_open() => {
                let woke = context.resource.accept(update);
                context.dirty |= woke;
            }
            update = context.capacity.wait(), if context.capacity.is_open() => {
                let woke = context.capacity.accept(update);
                context.dirty |= woke;
            }
            options = context.aws_options.wait(), if context.aws_options.is_open() => {
                let woke = context.aws_options.accept(options);
                context.dirty |= woke;
            }
            profile = context.import_profiles.wait(), if context.import_profiles.is_open() => {
                let woke = context.import_profiles.accept(profile);
                context.dirty |= woke;
            }
            update = context.import_tasks.wait(), if context.import_tasks.is_open() => {
                let woke = context.import_tasks.accept(update);
                context.dirty |= woke;
            }
            update = context.lifecycle.wait(), if context.lifecycle.is_open() => {
                let woke = context.lifecycle.accept(update);
                context.dirty |= woke;
            }
            update = context.dashboard_io.wait(), if context.dashboard_io.is_open() => {
                let woke = context.dashboard_io.accept(update);
                context.dirty |= woke;
            }
            _ = context.critical_operations_changed.changed(),
                if context.shutdown_requested =>
            {
                context.dirty = true;
            }
            // Turn clocks, countdowns, and credential-sync backoffs move on
            // their own, so the dashboard redraws once a second regardless.
            // The chat redraws only when its own time-driven text has moved:
            // a running turn clock in the session header, or the checkpoint
            // title.
            _ = clock_tick.tick() => {
                // Resume search covers the moving "Last active" text, so the
                // same clock that redraws the dialog rebuilds its rows.
                context.dashboard.rebuild_resume_rows();
                // The support panes carry clocks whatever has the keyboard, so
                // the surface redraws every second regardless.
                context.dirty = true;
                context.maybe_open_startup_session();
            }
            // The import progress dialog reports how long a step has stalled
            // and the resume dialog spins while it scans; both need a faster
            // tick, and only while they are on screen.
            _ = import_tick.tick(), if context.dashboard.needs_fast_tick() => {
                context.dirty = true;
            }
            // A drag held past a scrollable surface's edge keeps scrolling it
            // and keeps extending the selection, the way a held pointer does
            // in a terminal's own selection.
            _ = autoscroll_tick.tick(), if context.autoscroll_request().is_some() => {
                context.apply_autoscroll()?;
            }
        }
        context.drain_feeds();
        if !context.shutdown_requested {
            context.apply_chat_outcome(chat_outcome).await;
            actions::apply_dashboard_action(&mut context, action).await?;
            // The Sessions pane is a list of conversations, not a list of
            // things to go and open, so the transcript follows its selection.
            context.follow_selected_session();
        }
        if context.shutdown_requested {
            if context.refresh_shutdown_notice() {
                break;
            }
            context.dirty = true;
        }
    }
    context.cancel_background_work();
    let quit_detached = context.quit_detached;
    let workspace_switch_requested = context.workspace_switch_requested;
    // Hand the terminal back before saying anything on it; the warm chat and
    // the background feeds are torn down after, as the rest of the context
    // drops.
    drop(context.terminal);
    if let Some(shutdown) = context.worker_shutdown.take() {
        shutdown
            .shutdown()
            .await
            .context("shut down dashboard session manager")?;
    }
    Ok(if workspace_switch_requested {
        DashboardExit::WorkspacePicker
    } else if quit_detached {
        DashboardExit::Detached
    } else if context.shutdown_requested {
        DashboardExit::Interrupted
    } else {
        DashboardExit::Normal
    })
}

impl DashboardContext {
    pub(crate) fn request_shutdown(&mut self) {
        self.begin_shutdown(true);
    }

    pub(crate) fn request_workspace_switch(&mut self) {
        self.workspace_switch_requested = true;
        self.begin_shutdown(false);
    }

    fn begin_shutdown(&mut self, detached: bool) {
        if self.shutdown_requested {
            return;
        }
        self.shutdown_requested = true;
        self.quit_detached = detached;
        self.cancel_background_work();
        self.refresh_shutdown_notice();
        self.dirty = true;
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

    pub(super) fn acknowledge_visible_chat(&mut self) {
        let Some((session_id, through)) = self
            .active_chat
            .as_ref()
            .map(|chat| (chat.session_id().to_owned(), chat.latest_event_ordinal()))
        else {
            return;
        };
        let Some(session) = self.controller.state.sessions.get_mut(&session_id) else {
            return;
        };
        if through <= session.viewed_through_event_ordinal {
            return;
        }
        session.viewed_through_event_ordinal = through;
        self.dashboard.set_state(self.controller.state.clone());
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
            self.workspace_id.clone(),
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
    fn open(workspace_id: &str, client_id: &str) -> Result<Option<Self>> {
        let mut controller = Controller::load()?;
        retain_workspace_sessions(&mut controller, workspace_id, client_id)?;
        let workspace_name = hel::hel_database::list_workspaces()?
            .into_iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("unknown workspace {workspace_id:?}"))?
            .name;
        let mut dashboard = DashboardState::new(
            controller.config.clone(),
            controller.state.clone(),
            BTreeMap::new(),
        );
        let notices = hel::hel_chat::Notices::default();
        dashboard.share_notices(notices.clone());
        for (session_id, queued) in projected_queued_prompts(&controller)? {
            dashboard.apply_queued_prompts(&session_id, queued);
        }
        dashboard.set_workspace_name(workspace_name);
        let mut terminal = TerminalGuard::enter()?;
        if configuration_needs_setup(&controller.config) {
            terminal.suspend()?;
            let setup_result = run_setup_dialog(&config_path());
            terminal.resume()?;
            match setup_result? {
                SetupOutcome::Written => {
                    controller.reload()?;
                    retain_workspace_sessions(&mut controller, workspace_id, client_id)?;
                    dashboard.set_config(controller.config.clone());
                    dashboard.set_state(controller.state.clone());
                    dashboard
                        .set_notice("Setup complete. Press Alt-N to start your first session.");
                }
                SetupOutcome::Cancelled => return Ok(None),
            }
        }

        let (quota_profiles_tx, quota_updates_rx) = spawn_quota_refresher();
        let remote_worker = spawn_remote_dashboard_worker_poller(workspace_id.to_owned())?;
        let worker_targets_tx = remote_worker.targets;
        let worker_updates_rx = remote_worker.updates;
        let worker_commands_tx = remote_worker.control;
        let worker_shutdown = remote_worker.shutdown;
        let runtime_lifecycles_rx = remote_worker.lifecycles;
        let runtime_reviews_rx = remote_worker.reviews;
        let runtime_review_views = runtime_reviews_rx
            .borrow()
            .iter()
            .cloned()
            .map(|review| (review.session_id.clone(), review))
            .collect();
        let runtime_config_rx = remote_worker.config;
        let runtime_records_rx = remote_worker.records;
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

        let mut context = Self {
            terminal,
            controller,
            workspace_id: workspace_id.to_owned(),
            client_id: client_id.to_owned(),
            dashboard,
            notices,
            events: Some(event::EventStream::new()),
            active_chat: None,
            opening_chat_session: None,
            pending_chat_session: None,
            startup: StartupSession::idle(),
            dirty: true,
            drawn_notice_generation: 0,
            controller_changed: true,
            quit_detached: false,
            workspace_switch_requested: false,
            shutdown_requested: false,
            critical_operations,
            critical_operations_changed,
            quota_profiles_tx,
            quota: Feed::new(quota_updates_rx),
            manual_quota_refresh_generation: None,
            target_test_cancel: None,
            worker_targets_tx,
            worker: Feed::new(worker_updates_rx),
            runtime_lifecycles: Feed::new(runtime_lifecycles_rx),
            runtime_reviews: Feed::new(runtime_reviews_rx),
            runtime_review_views,
            runtime_config: Feed::new(runtime_config_rx),
            runtime_records: Feed::new(runtime_records_rx),
            config_reload_in_flight: false,
            remote_lifecycle_sessions: BTreeSet::new(),
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
            import_updates_tx,
            import_profiles: Feed::new(import_updates_rx),
            import_task_tx,
            import_tasks: Feed::new(import_task_rx),
            pending_import: None,
            import_discovery_id: 0,
            next_import_task_id: 0,
            active_import: None,
            clipboard_read_in_flight: false,
            selection: SelectionState::new(),
            selection_text: None,
            dashboard_io_tx,
            dashboard_io: Feed::new(dashboard_io_rx),
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

    /// Keeps one projection per session in flight and remembers only the
    /// newest snapshot that arrived behind it. The shared permits bound work
    /// across different sessions as well.
    /// Fill a freshly resumed conversation from the tail of its stored
    /// transcript, off the event loop.
    ///
    /// The projection is durable the moment the resume completes, so nothing
    /// has to travel back through the daemon reply to show it. The view keeps
    /// only the last `TAIL_SEED_ITEMS` entries, so the seed reads exactly that
    /// many rather than the whole history. The poller delivers the complete
    /// projection a moment later; this exists so the conversation is not blank
    /// until it does.
    pub(super) fn request_transcript_tail_seed(&mut self, session_id: &str) {
        let session_id = session_id.to_owned();
        let updates = self.dashboard_io_tx.clone();
        tokio::spawn(async move {
            let seeded = tokio::task::spawn_blocking({
                let session_id = session_id.clone();
                move || {
                    hel::hel_database::load_materialized_projection_tail(
                        &session_id,
                        hel::hel_chat::TAIL_SEED_ITEMS,
                    )
                }
            })
            .await;
            // A failed seed costs a blank conversation until the next poll,
            // which is where the full projection comes from anyway. It is not
            // worth failing a resume that otherwise succeeded.
            let materialized = match seeded {
                Ok(Ok(Some((materialized, _)))) => materialized,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    tracing::debug!(%session_id, %error, "transcript tail seed failed");
                    return;
                }
                Err(error) => {
                    tracing::debug!(%session_id, %error, "transcript tail seed task failed");
                    return;
                }
            };
            let _ = updates.send(DashboardIoUpdate::TranscriptTailSeed {
                materialized: Box::new(materialized),
            });
        });
    }

    pub(super) fn request_materialized_projection(
        &mut self,
        materialized: MaterializedSession,
        viewed_through_event_ordinal: u64,
    ) {
        let Some((materialized, viewed_through_event_ordinal)) = enqueue_materialized_projection(
            &mut self.materialized_projections_in_flight,
            &mut self.pending_materialized_projections,
            materialized,
            viewed_through_event_ordinal,
        ) else {
            return;
        };

        let session_id = materialized.session_id.clone();
        let previous = self.dashboard.take_projection_cache(&session_id);
        spawn_materialized_session_projection(
            materialized,
            viewed_through_event_ordinal,
            previous,
            self.dashboard_io_tx.clone(),
            Arc::clone(&self.materialized_projection_permits),
        );
    }

    fn hydrate_stored_session_summaries(&mut self) {
        let sessions = self
            .controller
            .state
            .sessions
            .values()
            .filter(|session| session.state.is_active())
            .map(|session| (session.id.clone(), session.viewed_through_event_ordinal))
            .collect::<Vec<_>>();
        // The startup pick compares recorded activity, which is exactly what
        // these summaries carry, so it waits for them.
        self.startup = StartupSession::begin(
            sessions.iter().map(|(id, _)| id.clone()),
            std::time::Instant::now(),
        );
        if sessions.is_empty() {
            // With nothing to talk to, the keyboard belongs on the list, where
            // a session can be created or resumed.
            self.dashboard.focus_sessions();
        }
        for (session_id, viewed_through_event_ordinal) in sessions {
            spawn_stored_session_summary(
                session_id,
                viewed_through_event_ordinal,
                self.dashboard_io_tx.clone(),
            );
        }
    }

    /// Records that one live session's stored summary has arrived, however it
    /// turned out, and opens the startup conversation once they all have.
    pub(super) fn finish_startup_summary(&mut self, session_id: &str) {
        self.startup.summary_arrived(session_id);
        self.maybe_open_startup_session();
    }

    /// Opens whatever the selection moved on to while an attach was running.
    pub(super) fn open_pending_chat_session(&mut self) {
        if let Some(session_id) = self.pending_chat_session.take() {
            self.open_chat_session(&session_id);
        }
    }

    /// Brings the conversation on screen into line with the Sessions pane's
    /// selection.
    ///
    /// Moving the selection moves the transcript, so the pane reads as a list
    /// of conversations rather than a list of things to go and open. Attaching
    /// is asynchronous and coalesced, so walking the list costs one attach for
    /// the row the user stops on rather than one per row passed through.
    pub(crate) fn follow_selected_session(&mut self) {
        let Some(selected) = self.dashboard.selected_session_id().map(str::to_owned) else {
            return;
        };
        if self.opening_chat_session.as_deref() == Some(selected.as_str()) {
            return;
        }
        if self.opening_chat_session.is_some() {
            self.pending_chat_session = Some(selected);
            return;
        }
        if self
            .active_chat
            .as_ref()
            .is_some_and(|chat| chat.session_id() == selected)
        {
            return;
        }
        self.open_chat_session(&selected);
    }

    /// The user took the choice into their own hands, so the surface stops
    /// trying to pick a conversation for them.
    fn cancel_startup_session(&mut self) {
        self.startup.cancel();
    }

    /// Whether a failed attach belongs to the startup pick and will be tried
    /// again on the next tick.
    pub(super) fn retry_startup_attach(&mut self, session_id: &str) -> bool {
        self.startup
            .attach_failed(session_id, std::time::Instant::now())
    }

    /// Opens the conversation the surface should start on, once the summaries
    /// it compares have arrived or the wait for them has run out.
    pub(super) fn maybe_open_startup_session(&mut self) {
        if !self.startup.ready(std::time::Instant::now()) {
            return;
        }
        let Some(session_id) = startup_session_choice(
            self.controller
                .state
                .sessions
                .values()
                .filter(|session| session.state.is_active()),
            |session_id| self.dashboard.session_activity_at_ms(session_id),
        ) else {
            self.dashboard.focus_sessions();
            return;
        };
        self.startup.attempting(&session_id);
        self.dashboard.focus_prompt();
        self.open_chat_session(&session_id);
    }

    pub(super) fn resolve_project_sources(&mut self) {
        let session_ids = self
            .controller
            .state
            .sessions
            .values()
            .filter(|session| session.state.is_active() && session.project_directory.is_some())
            .filter(|session| {
                !self.dashboard.has_resolved_project_source(&session.id)
                    && !self.project_sources_in_flight.contains(&session.id)
            })
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            self.project_sources_in_flight.insert(session_id.clone());
            spawn_project_source_resolution(
                &self.controller,
                session_id,
                self.dashboard_io_tx.clone(),
                self.critical_operations.clone(),
            );
        }
    }

    pub(super) fn finish_materialized_projection(
        &mut self,
        session_id: String,
        result: std::result::Result<Box<PreparedMaterializedSessionDetail>, String>,
    ) {
        self.materialized_projections_in_flight.remove(&session_id);
        match result {
            Ok(detail) => {
                self.dashboard.apply_prepared_materialized_session(*detail);
            }
            Err(error) => self
                .dashboard
                .set_notice(format!("Could not update session transcript: {error}")),
        }
        if let Some((materialized, viewed_through_event_ordinal)) =
            self.pending_materialized_projections.remove(&session_id)
        {
            self.request_materialized_projection(materialized, viewed_through_event_ordinal);
        }
    }

    /// Redraws the view on screen, if anything asked for a redraw.
    ///
    /// A notice is the only report several background failures get, and it can
    /// be written from any task through the shared slot. Comparing the slot
    /// with what the last frame drew is what makes a notice reach the screen
    /// even when nothing else marked the view dirty; without it a notice could
    /// be replaced or dismissed having rendered zero frames.
    fn draw(&mut self) -> Result<()> {
        let notice_generation = self.notices.generation();
        self.dirty |= notice_generation != self.drawn_notice_generation;
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;
        self.drawn_notice_generation = notice_generation;
        let Self {
            terminal,
            dashboard,
            active_chat,
            selection,
            selection_text,
            ..
        } = self;
        let transcript_selected = selection.active_surface() == Some(SurfaceId::Transcript);
        // The highlight and the extraction both run inside the draw closure,
        // once the surface has drawn: the hitboxes are registered by that
        // render and the cells the selection covers only exist in this frame.
        terminal.terminal.draw(|frame| {
            render_combined(frame, dashboard, active_chat.as_mut(), transcript_selected);
            *selection_text = draw_selection(frame, selection, dashboard.frame_surfaces());
        })?;
        // The transcript reports a row space it can no longer measure the
        // selection in — a width change, a rebuilt cache, a jump across the
        // deep past. Dropping the selection is the honest answer; walking
        // history to rescue it is the full-transcript probe this design exists
        // to avoid.
        let invalidated = self
            .active_chat
            .as_mut()
            .is_some_and(hel::hel_chat::ActiveChat::transcript_selection_invalidated);
        if invalidated && self.selection.active_surface() == Some(SurfaceId::Transcript) {
            self.selection.clear();
            self.dirty = true;
        }
        Ok(())
    }

    /// The hitboxes the surface registered on its last frame. The combined
    /// renderer merges the conversation's into this one registry, so there is
    /// only ever one to consult.
    fn frame_surfaces(&self) -> &FrameSurfaces {
        self.dashboard.frame_surfaces()
    }

    /// The surface a held drag is asking to scroll, if any.
    fn autoscroll_request(&self) -> Option<(SurfaceId, i8)> {
        self.selection.autoscroll_request(self.frame_surfaces())
    }

    /// Scrolls the surface a drag is holding against its edge, then re-resolves
    /// the still pointer against the frame that scroll produced.
    ///
    /// The pointer emits no events while it is held still, so the rows that
    /// moved under it only join the selection once the registry is rebuilt,
    /// which needs the redraw in between.
    fn apply_autoscroll(&mut self) -> Result<()> {
        let Some((surface, direction)) = self.autoscroll_request() else {
            return Ok(());
        };
        let Some(chat) = self.active_chat.as_mut() else {
            return Ok(());
        };
        chat.autoscroll_selection(surface, direction);
        self.dirty = true;
        self.draw()?;
        let Self {
            selection,
            dashboard,
            ..
        } = self;
        selection.retrack(dashboard.frame_surfaces());
        Ok(())
    }

    /// Routes one terminal event through the selection engine, hit-testing
    /// against the surfaces the view on screen registered.
    fn route_selection(&mut self, event: Event) -> SelectionRouting {
        let Self {
            selection,
            dashboard,
            ..
        } = self;
        route_selection_event(selection, dashboard.frame_surfaces(), event)
    }

    /// Copies the finished selection to the system and terminal clipboards.
    ///
    /// The frame on screen predates the release that finished the drag, so
    /// this redraws before reading. Surfaces that scroll their own rows own
    /// the text a selection covers, because most of it is not on the frame the
    /// stash is read from; everything else comes out of that stash.
    fn copy_selection(&mut self, surface: SurfaceId, range: SelectionRange) -> Result<()> {
        self.dirty = true;
        self.draw()?;
        let extracted = match surface {
            SurfaceId::Transcript => self
                .active_chat
                .as_mut()
                .and_then(|chat| chat.transcript_selection_text(&range)),
            SurfaceId::ElicitationMessage => self
                .active_chat
                .as_ref()
                .and_then(|chat| chat.elicitation_selection_text(&range)),
            // The reviewer pane scrolls its own rows, so the text a selection
            // covers comes out of that pane rather than off this frame.
            SurfaceId::ReviewerTranscript => self
                .active_chat
                .as_ref()
                .and_then(|chat| chat.reviewer_selection_text(&range)),
            _ => self.selection_text.take(),
        };
        let Some(text) = extracted.filter(|text| !text.trim().is_empty()) else {
            tracing::debug!(?surface, ?range, "selection covered no text");
            return Ok(());
        };
        // The desktop clipboard opens a blocking platform connection, so it
        // runs on a blocking task; OSC 52 is one escape sequence and also
        // reaches a terminal Hel is talking to over SSH.
        spawn_clipboard_write(text.clone(), self.dashboard_io_tx.clone());
        if let Err(error) = self.terminal.copy_to_terminal_clipboard(&text) {
            self.dashboard
                .set_failure_notice(format!("Copy to the terminal clipboard failed: {error:#}"));
            return Ok(());
        }
        let lines = text.lines().count().max(1);
        self.dashboard.set_notice(format!(
            "Copied {lines} line{}",
            if lines == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    /// Recomputes what depends on controller state after it may have changed.
    fn refresh_controller_derived_state(&mut self) {
        if !self.controller_changed {
            return;
        }
        self.controller_changed = false;
        let archive_targets = checkpoint_archive_targets(&self.controller);
        if archive_targets != self.checkpoint_archive_targets_seen {
            self.checkpoint_archive_targets_seen = archive_targets.clone();
            self.checkpoint_archive_generation =
                self.checkpoint_archive_generation.wrapping_add(1).max(1);
            spawn_checkpoint_archive_size_refresh(
                self.checkpoint_archive_generation,
                archive_targets,
                self.dashboard_io_tx.clone(),
            );
        }
        let capacity_targets = self.controller.deployment_capacity_targets();
        if *self.capacity_targets_tx.borrow() != capacity_targets {
            self.capacity_targets_tx
                .send_replace(capacity_targets.clone());
            self.dashboard
                .set_deployment_capacity_targets(capacity_targets);
            self.dirty = true;
        }
    }

    /// Asks the poller for fresh quotas and reports the refresh in the UI.
    /// Returns the generation, so a manual refresh can recognize its own
    /// completion.
    pub(crate) fn request_quota_refresh(&mut self) -> u64 {
        let profiles = quota_refresh_profiles(&self.controller);
        self.dashboard
            .begin_quota_refresh(profiles.iter().map(|profile| profile.profile_id.clone()));
        let generation = self
            .quota_profiles_tx
            .borrow()
            .generation
            .wrapping_add(1)
            .max(1);
        self.quota_profiles_tx.send_replace(QuotaRefreshBatch {
            generation,
            profiles,
        });
        generation
    }

    /// Republishes what the pollers should watch, leaving out sessions a
    /// lifecycle operation currently owns.
    pub(crate) fn refresh_poll_targets(&self) {
        refresh_dashboard_poll_targets(
            &self.controller,
            &self.worker_targets_tx,
            &self.resource_targets_tx,
            &self.credential_sync_handle,
            &self.lifecycle_operations.keys().cloned().collect(),
        );
    }

    /// Drops the warm chat when it belongs to `session_id`.
    ///
    /// Pause and destroy retire that session's actor. Resume starts a new one,
    /// often on a different profile. Keeping the old view would redraw a
    /// Closing/Closed snapshot and refuse prompts.
    pub(crate) fn drop_warm_chat_for(&mut self, session_id: &str) {
        if self
            .active_chat
            .as_ref()
            .is_some_and(|chat| chat.session_id() == session_id)
        {
            self.active_chat = None;
        }
    }

    /// Opens the conversation for `session_id` without waiting on the session
    /// manager. Attaching can involve worker/relay I/O, so the result comes
    /// back through the dashboard I/O channel and the surface stays responsive
    /// while it is in flight.
    ///
    /// The conversation being replaced is saved first: it holds unsent input
    /// and a read position that a quit or a crash would otherwise lose.
    pub(crate) fn open_chat_session(&mut self, session_id: &str) {
        self.dashboard.select_active_session(session_id);
        if self
            .active_chat
            .as_ref()
            .is_some_and(|chat| chat.session_id() == session_id && chat.session_feed_open())
        {
            self.dashboard.set_current_session(Some(session_id));
            self.acknowledge_visible_chat();
            self.dirty = true;
            return;
        }
        if self.opening_chat_session.as_deref() == Some(session_id) {
            self.pending_chat_session = None;
            return;
        }
        if self.opening_chat_session.is_some() {
            // Hold the newest request rather than refusing it. The selection
            // drives this, so a refusal would leave the conversation showing a
            // row the user has already moved off.
            self.pending_chat_session = Some(session_id.to_owned());
            return;
        }
        self.pending_chat_session = None;
        let Some(session_record) = self.controller.state.sessions.get(session_id).cloned() else {
            self.dashboard.set_notice(format!(
                "Could not open session: unknown session {session_id}"
            ));
            return;
        };
        if let Some(ordinal) = self
            .active_chat
            .as_ref()
            .filter(|chat| chat.session_id() != session_id)
            .map(hel::hel_chat::ActiveChat::latest_event_ordinal)
        {
            self.record_detach(ordinal);
        }
        let header = hel::hel_chat::SessionHeaderIdentity {
            target: session_record
                .project_target(&self.controller.config, &session_record.target_template_id),
            profile: session_record.last_profile.clone(),
            harness_kind: Some(session_record.harness_kind),
        };
        let sessions = self.worker_commands_tx.clone();
        let notices = self.notices.clone();
        let updates = self.dashboard_io_tx.clone();
        let session_id = session_id.to_owned();
        let bundle_id = session_record.bundle_id.clone();
        let draft = session_record.draft_input.clone();
        let context = hel::hel_chat::ChatSessionContext {
            config: self.controller.config.clone(),
            session: session_record,
        };
        let (persistence_tx, mut persistence_rx) =
            tokio::sync::mpsc::unbounded_channel::<hel::hel_chat::ChatDaemonRequest>();
        let refusals = self.dashboard_io_tx.clone();
        tokio::spawn(async move {
            while let Some(request) = persistence_rx.recv().await {
                // A review action's refusal is a sentence for the person who
                // pressed the key, so it comes back to the chat rather than
                // only into the log.
                let refusal_session = match &request {
                    hel::hel_chat::ChatDaemonRequest::StartTurnReview { session_id }
                    | hel::hel_chat::ChatDaemonRequest::ResolveTurnReview { session_id, .. } => {
                        Some(session_id.clone())
                    }
                    _ => None,
                };
                let result = async {
                    let mut daemon = crate::daemon::connect_or_start().await?;
                    match request {
                        hel::hel_chat::ChatDaemonRequest::SaveReview { session_id, review } => {
                            daemon.save_active_review(session_id, review).await
                        }
                        hel::hel_chat::ChatDaemonRequest::ClearReview { session_id } => {
                            daemon.clear_active_review(session_id).await
                        }
                        hel::hel_chat::ChatDaemonRequest::RememberReviewerSelection {
                            workspace_id,
                            selection,
                        } => {
                            daemon
                                .remember_reviewer_selection(workspace_id, selection)
                                .await
                        }
                        hel::hel_chat::ChatDaemonRequest::StartTurnReview { session_id } => {
                            daemon.start_turn_review(session_id).await
                        }
                        hel::hel_chat::ChatDaemonRequest::ResolveTurnReview {
                            session_id,
                            resolution,
                        } => daemon.resolve_turn_review(session_id, resolution).await,
                    }
                }
                .await;
                if let (Err(error), Some(session_id)) = (&result, refusal_session)
                    && refusals
                        .send(DashboardIoUpdate::ReviewRefused {
                            session_id,
                            message: format!("{error:#}"),
                        })
                        .is_err()
                {
                    tracing::debug!("a review refusal was dropped because the dashboard closed");
                }
                if let Err(error) = result {
                    tracing::warn!(%error, "could not persist chat state through the daemon");
                }
            }
        });
        self.opening_chat_session = Some(session_id.clone());
        self.dashboard.set_current_session(Some(&session_id));
        self.dashboard.set_notice("Opening session…");
        tokio::spawn(async move {
            let result = sessions
                .session(session_id.clone())
                .await
                .map(|managed| {
                    hel::hel_chat::ActiveChat::open_with_persistence(
                        managed,
                        &bundle_id,
                        Some(context),
                        sessions,
                        header,
                        draft,
                        notices,
                        Some(persistence_tx),
                    )
                })
                .map_err(|error| format!("{error:#}"));
            if let Err(error) = updates.send(DashboardIoUpdate::ChatOpened {
                session_id,
                result: Box::new(result),
            }) {
                tracing::debug!(%error, "chat-open result dropped after dashboard shutdown");
            }
        });
        self.dirty = true;
    }

    /// Suspends the terminal for the setup dialog and takes back what it
    /// changed.
    ///
    /// The dialog reads the terminal itself, and once polled an `EventStream`
    /// leaves a reader thread inside crossterm's internal reader, where it
    /// holds the reader lock and consumes terminal input. So the live stream is
    /// dropped before the replacement is built, which takes that same lock. The
    /// replacement reads nothing until the loop polls it again.
    pub(crate) fn run_setup_dialog(&mut self) -> Result<SetupOutcome> {
        drop(self.events.take());
        self.terminal.suspend()?;
        let outcome = run_setup_dialog(&config_path());
        self.terminal.resume()?;
        self.events = Some(event::EventStream::new());
        outcome
    }

    /// Tells every operation still in flight to stop. Cancellation is
    /// cooperative, so this only requests it.
    fn cancel_background_work(&self) {
        self.critical_operations.cancel_all();
        for operation in self.lifecycle_operations.values() {
            operation.cancelled.store(true, Ordering::Release);
        }
        if let Some(active) = self.active_import.as_ref() {
            active.cancelled.store(true, Ordering::Release);
        }
    }

    /// Takes every message queued behind the one that woke the loop, feed by
    /// feed, in the order the UI depends on.
    fn drain_feeds(&mut self) {
        self.drain_quota_updates();
        self.drain_runtime_records();
        self.drain_worker_updates();
        self.drain_runtime_lifecycles();
        self.drain_runtime_reviews();
        self.drain_runtime_config();
        schedule_due_credential_syncs(
            &mut self.credential_sync_signals,
            &self.credential_sync_handle,
            Instant::now(),
        );
        self.drain_credential_results();
        self.drain_resource_updates();
        self.drain_capacity_updates();
        self.drain_aws_resource_options();
        self.drain_import_profiles();
        self.drain_import_tasks();
        self.drain_lifecycle_updates();
        self.drain_dashboard_io();
        self.refresh_open_review();
    }

    /// Keeps the stop confirmation's warning current.
    ///
    /// A review can only be started from inside a chat, so the open chat is
    /// the authority while there is one. This is an in-memory read: the loop
    /// never touches the database for it.
    /// Hands the open chat whatever review the daemon is running for it.
    ///
    /// The chat draws the pane and sends the resolutions; it hosts nothing.
    /// A session with no review gets `None`, which closes the pane.
    fn drain_runtime_reviews(&mut self) {
        let mut latest = None;
        while let Some(reviews) = self.runtime_reviews.next_ready() {
            latest = Some(reviews);
        }
        let Some(reviews) = latest else {
            return;
        };
        self.runtime_review_views = reviews
            .into_iter()
            .map(|review| (review.session_id.clone(), review))
            .collect();
        self.apply_runtime_review_to_active_chat();
    }

    /// Applies the retained daemon projection whenever a chat becomes active.
    /// Absence is meaningful: it closes a review the previous projection held.
    pub(crate) fn apply_runtime_review_to_active_chat(&mut self) {
        let Some(chat) = self.active_chat.as_mut() else {
            return;
        };
        let review = self.runtime_review_views.get(chat.session_id()).cloned();
        chat.apply_review_view(review);
    }

    fn refresh_open_review(&mut self) {
        let Some(chat) = self.active_chat.as_ref() else {
            return;
        };
        let session_id = chat.session_id().to_owned();
        let open = chat.has_open_review();
        self.dashboard.set_session_review_open(&session_id, open);
    }

    fn drain_quota_updates(&mut self) {
        while let Some(update) = self.quota.next_ready() {
            match update {
                QuotaUpdate::Refreshing { profile_ids } => {
                    self.dashboard.begin_quota_refresh(profile_ids)
                }
                QuotaUpdate::Report(outcome) => {
                    if outcome.credentials_changed {
                        self.credential_sync_handle
                            .sync_profile_now(&outcome.report.profile_id, None);
                    }
                    self.dashboard.apply_quota(outcome.report);
                }
                QuotaUpdate::Finished { generation } => {
                    if complete_manual_quota_refresh(
                        &mut self.manual_quota_refresh_generation,
                        generation,
                    ) {
                        self.dashboard
                            .replace_notice_if(QUOTA_REFRESH_NOTICE, QUOTA_REFRESHED_NOTICE);
                    }
                }
            }
        }
    }

    fn drain_worker_updates(&mut self) {
        while let Some(update) = self.worker.next_ready() {
            self.controller_changed = true;
            let session_id = update.session_id.clone();
            let connected = update.view.connected;
            // Only unreachable relays drive the worker diagnostics flow.
            let connection_error = match update.view.error.as_ref() {
                Some(ViewError::Unreachable(detail)) => Some(detail.clone()),
                Some(ViewError::TargetMissing(_) | ViewError::ProjectionIntegrity(_)) | None => {
                    None
                }
            };
            if let Some(snapshot) = update.view.snapshot.as_ref()
                && let Some(session) = self.controller.state.sessions.get(&session_id).cloned()
                && let Some(signal) = snapshot.latest_credential_sync_signal.clone()
            {
                self.credential_sync_signals
                    .observe(&session_id, &session.last_profile, signal);
            }
            let materialized = update
                .view
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.materialized.clone());
            let last_acp_activity_at_ms = update
                .view
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.operational.last_acp_activity_at_ms);
            self.dashboard
                .set_last_acp_activity(&session_id, last_acp_activity_at_ms);
            // A view is published as disconnected only once the relay has
            // failed past the unreachable threshold, so this reddens the band
            // exactly when the target is genuinely unreachable and clears it on
            // recovery.
            self.dashboard
                .set_session_connectivity(&session_id, connected);
            match apply_worker_poll_update(
                &mut self.controller,
                &mut self.dashboard,
                update,
                &self.dashboard_io_tx,
                &self.critical_operations,
            ) {
                Ok(true) => {
                    let _ = self.resource_triggers_tx.try_send(session_id.clone());
                    if let Some(materialized) = materialized {
                        let viewed_through_event_ordinal = self
                            .controller
                            .state
                            .sessions
                            .get(&session_id)
                            .map_or(0, |session| session.viewed_through_event_ordinal);
                        self.request_materialized_projection(
                            materialized,
                            viewed_through_event_ordinal,
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    self.dashboard
                        .set_notice(format!("Could not save harness title: {error:#}"));
                }
            }
            if let Some(episode_id) =
                self.worker_diagnoses
                    .observe(&session_id, connected, connection_error)
            {
                spawn_worker_diagnosis(
                    &self.controller,
                    session_id,
                    episode_id,
                    self.dashboard_io_tx.clone(),
                    self.critical_operations.clone(),
                );
            }
        }
    }

    fn drain_runtime_lifecycles(&mut self) {
        let mut latest = None;
        while let Some(lifecycles) = self.runtime_lifecycles.next_ready() {
            latest = Some(lifecycles);
        }
        let Some(lifecycles) = latest else {
            return;
        };
        let active = lifecycles
            .iter()
            .map(|lifecycle| lifecycle.session_id.clone())
            .collect::<BTreeSet<_>>();
        for session_id in self
            .remote_lifecycle_sessions
            .difference(&active)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.dashboard.finish_session_operation(&session_id);
            self.remote_lifecycle_sessions.remove(&session_id);
        }
        for lifecycle in lifecycles {
            let kind = match lifecycle.kind {
                crate::daemon::RuntimeLifecycleKind::Create => SessionOperationKind::Launching,
                crate::daemon::RuntimeLifecycleKind::Close
                | crate::daemon::RuntimeLifecycleKind::ForceStop => SessionOperationKind::Stopping,
                crate::daemon::RuntimeLifecycleKind::Resume => SessionOperationKind::Resuming,
                crate::daemon::RuntimeLifecycleKind::DestroyStopped => {
                    SessionOperationKind::Destroying
                }
            };
            if !self
                .lifecycle_operations
                .contains_key(&lifecycle.session_id)
                && self
                    .remote_lifecycle_sessions
                    .insert(lifecycle.session_id.clone())
            {
                self.dashboard.begin_session_operation_at(
                    lifecycle.session_id.clone(),
                    kind,
                    None,
                    lifecycle.started_at_epoch_seconds,
                );
            }
            self.dashboard
                .replace_session_operation_stages(&lifecycle.session_id, lifecycle.active_stages);
            if let Some((profile_id, target_id)) = lifecycle.resume_destination {
                self.dashboard
                    .set_resume_destination(&lifecycle.session_id, profile_id, target_id);
            }
            if let Some(notice) = lifecycle.notice {
                self.dashboard.set_notice(notice);
            }
        }
        self.controller_changed = true;
    }

    /// Hands the open conversation the surface's current view of the config
    /// and its own session record. The chat snapshots both when it opens, so
    /// without this a long-lived chat would keep offering reviewer profiles
    /// that a config reload has since renamed or removed.
    pub(crate) fn refresh_chat_context(&mut self) {
        let Some(chat) = self.active_chat.as_mut() else {
            return;
        };
        chat.refresh_context(
            &self.controller.config,
            self.controller.state.sessions.get(chat.session_id()),
        );
    }

    fn drain_runtime_config(&mut self) {
        let mut latest = None;
        while let Some(config) = self.runtime_config.next_ready() {
            latest = Some(config);
        }
        let Some(config) = latest else {
            return;
        };
        // The chat's copy of `[review]` follows the daemon's, so `/review
        // status` and the composer's armed indicator report what is actually
        // running rather than what this process last read from disk.
        if let Some(chat) = self.active_chat.as_mut() {
            chat.set_review_config(config.review.clone());
        }
        if config == self.controller.config || self.config_reload_in_flight {
            return;
        }
        self.controller.config = config.clone();
        self.dashboard.set_config(config);
        self.refresh_chat_context();
        self.refresh_poll_targets();
        self.request_quota_refresh();
        self.config_reload_in_flight = true;
        let workspace_id = self.workspace_id.clone();
        let client_id = self.client_id.clone();
        spawn_io(
            "reload daemon configuration",
            self.dashboard_io_tx.clone(),
            move || {
                let mut controller = Controller::load()?;
                retain_workspace_sessions(&mut controller, &workspace_id, &client_id)?;
                Ok(controller)
            },
            DashboardIoUpdate::ConfigReloaded,
        );
    }

    fn drain_runtime_records(&mut self) {
        let mut latest = None;
        while let Some(records) = self.runtime_records.next_ready() {
            latest = Some(records);
        }
        let Some(records) = latest else {
            return;
        };
        let sessions: BTreeMap<String, SessionRecord> = records
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect();
        let settled_remote_operations = self
            .remote_lifecycle_sessions
            .iter()
            .filter(|session_id| {
                self.dashboard
                    .session_operation_kind(session_id)
                    .is_some_and(|kind| {
                        remote_lifecycle_settled(
                            kind,
                            sessions.get(*session_id).map(|session| session.state),
                        )
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for session_id in settled_remote_operations {
            self.dashboard.finish_session_operation(&session_id);
            self.remote_lifecycle_sessions.remove(&session_id);
        }
        if let Some(chat) = self.active_chat.as_mut() {
            let feed_expected = sessions
                .get(chat.session_id())
                .is_some_and(session_target_is_pollable);
            chat.set_session_feed_expected(feed_expected);
        }
        if self.controller.state.sessions == sessions {
            return;
        }
        self.controller.state.sessions = sessions;
        self.dashboard.set_state(self.controller.state.clone());
        self.refresh_chat_context();
        self.controller_changed = true;
        self.refresh_poll_targets();
    }

    fn drain_credential_results(&mut self) {
        while let Some(result) = self.credential_sync.next_ready() {
            crate::pollers::log_credential_sync_actions(&result);
            if let Some(notice) = self.credential_sync_notices.notice(&result) {
                self.dashboard.set_notice(notice);
            }
        }
    }

    fn drain_resource_updates(&mut self) {
        while let Some(update) = self.resource.next_ready() {
            self.dashboard
                .apply_resource_usage(&update.session_id, update.usage);
        }
    }

    fn drain_capacity_updates(&mut self) {
        while let Some(update) = self.capacity.next_ready() {
            self.dashboard.apply_deployment_capacity(
                &update.target_id,
                update.result,
                update.sampled_at_epoch_seconds,
            );
        }
    }

    pub(crate) fn request_capacity_refresh(&mut self) {
        self.dashboard.begin_capacity_refresh();
        match self.capacity_triggers_tx.try_send(()) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {
                self.dashboard.set_notice("Refreshing target capacity…");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                self.dashboard
                    .set_notice("Could not refresh target capacity: poller stopped.");
            }
        }
    }

    fn drain_aws_resource_options(&mut self) {
        while let Some((target_id, result)) = self.aws_options.next_ready() {
            self.resolving_aws_resource_options.remove(&target_id);
            self.dashboard
                .apply_aws_resource_options(&target_id, result);
        }
    }

    fn drain_import_profiles(&mut self) {
        while let Some((discovery_id, profile)) = self.import_profiles.next_ready() {
            self.dashboard.apply_resume_profile(discovery_id, profile);
        }
    }

    fn drain_import_tasks(&mut self) {
        while let Some(update) = self.import_tasks.next_ready() {
            match update {
                DashboardImportUpdate::Progress {
                    task_id,
                    step,
                    total,
                    message,
                } => {
                    if self
                        .active_import
                        .as_ref()
                        .is_some_and(|active| active.task_id == task_id)
                    {
                        self.dashboard.update_import_progress(step, total, message);
                    }
                }
                DashboardImportUpdate::Finished {
                    task_id,
                    pending,
                    result,
                } => {
                    if self
                        .active_import
                        .as_ref()
                        .is_none_or(|active| active.task_id != task_id)
                    {
                        continue;
                    }
                    self.active_import = None;
                    match *result {
                        Ok(DashboardImportTaskResult::NeedsBundle(prompt)) => {
                            self.pending_import = Some(pending);
                            self.dashboard.show_import_bundle_confirmation(
                                prompt.dirty_git_roots,
                                prompt.omitted_non_git_dirs,
                                prompt.scratch_git_roots,
                                prompt.has_untracked_files,
                            );
                        }
                        Ok(DashboardImportTaskResult::Imported(imported)) => {
                            self.dashboard.finish_import();
                            self.dashboard.set_notice("Saving imported session…");
                            io::spawn_imported_session_apply(
                                *imported,
                                pending,
                                self.dashboard_io_tx.clone(),
                                self.critical_operations.clone(),
                            );
                        }
                        Ok(DashboardImportTaskResult::Cancelled) => {
                            self.dashboard.finish_import();
                            self.dashboard
                                .set_notice("Import cancelled; no Mjolnir files were changed.");
                        }
                        Err(error) => {
                            self.dashboard.finish_import();
                            self.dashboard
                                .set_notice(format!("Import failed: {error:#}"));
                        }
                    }
                }
            }
        }
    }

    fn drain_lifecycle_updates(&mut self) {
        while let Some(update) = self.lifecycle.next_ready() {
            self.controller_changed = true;
            let session_id = update.session_id.clone();
            let operation = self.lifecycle_operations.remove(&session_id);
            self.dashboard.finish_session_operation(&session_id);
            spawn_lifecycle_reload(
                LifecycleReload { update, operation },
                self.dashboard_io_tx.clone(),
            );
        }
    }

    fn drain_dashboard_io(&mut self) {
        while let Some(update) = self.dashboard_io.next_ready() {
            self.controller_changed = true;
            self.apply_dashboard_io_update(update);
        }
    }

    /// Opens the resume dialog and starts one background scan per profile.
    /// Every profile appears immediately as a placeholder, so the dialog is
    /// usable while the scans are still running, and the scans run
    /// concurrently rather than one after another.
    pub(crate) fn start_resume_discovery(&mut self) {
        self.import_discovery_id = self.import_discovery_id.wrapping_add(1);
        self.dashboard.show_resume_dialog(
            self.import_discovery_id,
            resume_profile_placeholders(
                self.controller
                    .config
                    .profiles
                    .iter()
                    .map(|(id, profile)| (id.clone(), profile.kind)),
            ),
        );
        io::spawn_hidden_native_sessions_load(self.dashboard_io_tx.clone());
        let discovery_id = self.import_discovery_id;
        for (profile_id, profile) in self.controller.config.profiles.clone() {
            let updates = self.import_updates_tx.clone();
            tokio::task::spawn_blocking(move || {
                let completed = crate::import::discover_import_profile(
                    profile_id,
                    profile.kind,
                    profile.home,
                    |profile| {
                        if let Ok(permit) = updates.try_reserve() {
                            permit.send((discovery_id, profile.clone()));
                        }
                    },
                );
                let _ = updates.blocking_send((discovery_id, completed));
            });
        }
    }

    /// Starts a background import and shows its progress dialog.
    pub(crate) fn start_import(
        &mut self,
        pending: PendingDashboardImport,
        safety: crate::import::DashboardImportSafety,
    ) {
        self.dashboard
            .show_import_progress(pending.display_title.clone());
        self.next_import_task_id = self.next_import_task_id.wrapping_add(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active_import = Some(ActiveDashboardImport {
            task_id: self.next_import_task_id,
            cancelled: cancelled.clone(),
        });
        spawn_dashboard_import(
            &self.controller,
            DashboardImportRequest {
                workspace_id: self.workspace_id.clone(),
                pending,
                safety,
                task_id: self.next_import_task_id,
                cancelled,
            },
            self.import_task_tx.clone(),
            self.critical_operations.clone(),
        );
    }

    /// Applies what the chat view asked for after handling its own input.
    async fn apply_chat_outcome(&mut self, outcome: hel::hel_chat::ChatEventOutcome) {
        match outcome {
            hel::hel_chat::ChatEventOutcome::None | hel::hel_chat::ChatEventOutcome::Handled => {}
            hel::hel_chat::ChatEventOutcome::CycleFocus { reverse } => {
                self.dashboard.cycle_focus(reverse);
                self.dirty = true;
            }
            hel::hel_chat::ChatEventOutcome::QuitDetach {
                last_seen_event_ordinal,
            } => {
                // The warm chat goes on holding this input in memory, so save
                // it before the process goes away. The critical-operation
                // guard keeps the TUI alive until the durable write finishes,
                // while unrelated reads remain free to be abandoned.
                let persist = self.record_detach(last_seen_event_ordinal);
                drop(persist);
                self.request_shutdown();
            }
        }
    }

    /// Persists how far the warm chat has been read and the draft it holds.
    fn record_detach(
        &mut self,
        last_seen_event_ordinal: u64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let detached = self
            .active_chat
            .as_ref()
            .map(|chat| (chat.session_id().to_owned(), chat.draft().to_owned()))?;
        let (session_id, draft) = detached;
        record_chat_detach_state(
            &mut self.controller,
            &mut self.dashboard,
            DetachedChatState {
                client_id: &self.client_id,
                workspace_id: &self.workspace_id,
                session_id: &session_id,
                event_ordinal: last_seen_event_ordinal,
                draft: &draft,
            },
            &self.dashboard_io_tx,
            self.critical_operations.clone(),
        )
    }
}

/// A durable terminal lifecycle state is authoritative over a remote UI
/// overlay. The daemon lifecycle feed normally removes the overlay first, but
/// records and lifecycles travel on separate watch channels; retaining a
/// completed overlay would otherwise force a fresh Running record back to a
/// displayed Provisioning state indefinitely.
fn remote_lifecycle_settled(kind: SessionOperationKind, state: Option<SessionState>) -> bool {
    match kind {
        SessionOperationKind::Launching | SessionOperationKind::Importing => {
            state.is_none_or(|state| {
                matches!(
                    state,
                    SessionState::Running
                        | SessionState::Disconnected
                        | SessionState::Lost
                        | SessionState::Error
                        | SessionState::DestroyedWithDataLoss
                )
            })
        }
        SessionOperationKind::Resuming => state.is_none_or(|state| {
            matches!(
                state,
                SessionState::Running
                    | SessionState::Disconnected
                    | SessionState::Stopped
                    | SessionState::Lost
                    | SessionState::Error
                    | SessionState::DestroyedWithDataLoss
            )
        }),
        SessionOperationKind::Connecting => state.is_some_and(|state| {
            matches!(
                state,
                SessionState::Running
                    | SessionState::Disconnected
                    | SessionState::Lost
                    | SessionState::Error
                    | SessionState::DestroyedWithDataLoss
            )
        }),
        SessionOperationKind::Stopping => state.is_some_and(|state| {
            matches!(
                state,
                SessionState::Stopped
                    | SessionState::Lost
                    | SessionState::Error
                    | SessionState::DestroyedWithDataLoss
            )
        }),
        SessionOperationKind::Destroying => {
            state.is_none_or(|state| matches!(state, SessionState::DestroyedWithDataLoss))
        }
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
    workspace_id: &'a str,
    session_id: &'a str,
    event_ordinal: u64,
    draft: &'a str,
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
    session.draft_input = detached.draft.to_owned();
    dashboard.set_state(controller.state.clone());
    dashboard.clear_notice();
    Some(io::spawn_detached_session_state_persist(
        detached.client_id.to_owned(),
        detached.workspace_id.to_owned(),
        detached.session_id.to_owned(),
        detached.event_ordinal,
        detached.draft.to_owned(),
        updates.clone(),
        tracker,
    ))
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
fn dispatch_event(
    context: &mut DashboardContext,
    event: Event,
    action: &mut DashboardAction,
    chat_outcome: &mut hel::hel_chat::ChatEventOutcome,
) -> bool {
    let to_chat = match &event {
        Event::Mouse(mouse) if !context.dashboard.modal_open() => {
            let over_chat = context
                .dashboard
                .chat_region_contains(mouse.column, mouse.row);
            if over_chat && mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                context.dashboard.focus_prompt();
            }
            over_chat
        }
        _ => !context.dashboard.modal_open() && context.dashboard.prompt_has_focus(),
    };
    match context.active_chat.as_mut().filter(|_| to_chat) {
        Some(chat) => {
            *chat_outcome = chat.handle_event(event);
            matches!(*chat_outcome, hel::hel_chat::ChatEventOutcome::None)
        }
        None => {
            *action = dashboard_event_action(&mut context.dashboard, event);
            context.controller_changed = true;
            matches!(*action, DashboardAction::None)
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
    hel::hel_selection::highlight(frame.buffer_mut(), &surface, &range);
    if matches!(
        id,
        SurfaceId::Transcript | SurfaceId::ElicitationMessage | SurfaceId::ReviewerTranscript
    ) {
        return None;
    }
    Some(hel::hel_selection::extract_rows(
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

/// Applies one terminal event to the dashboard and reports the work it asks
/// for. Every event redraws, so events that carry no action still return
/// `None` rather than being skipped.
fn dashboard_event_action(dashboard: &mut DashboardState, event: Event) -> DashboardAction {
    match event {
        Event::Key(key) => dashboard.handle_key(key),
        Event::Paste(pasted) => {
            dashboard.handle_paste(&pasted);
            DashboardAction::None
        }
        Event::Mouse(mouse) => dashboard.handle_mouse(mouse),
        // Resize and focus changes only need the redraw.
        _ => DashboardAction::None,
    }
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
    let id = hel_tui::global_chord(key)?;
    dashboard.global_chord_allowed(id).then_some(id)
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

fn configuration_needs_setup(config: &HelConfig) -> bool {
    config.profiles.is_empty() && config.bundles.is_empty() && config.targets.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_selection::SurfaceFrame;
    use hel::hel_state::HelState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::{Position, Rect};
    use ratatui::style::Modifier;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn escape() -> Event {
        Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ))
    }

    /// A dashboard with profiles and a target, so the adaptive layout draws
    /// its three panes with text in them.
    fn populated_dashboard() -> DashboardState {
        let mut config = HelConfig::default();
        for (id, kind) in [
            ("claude-1", hel::hel_config::HarnessKind::Claude),
            ("codex-1", hel::hel_config::HarnessKind::Codex),
        ] {
            config.profiles.insert(
                id.into(),
                hel::hel_config::HarnessProfile {
                    context_window_bytes: None,
                    kind,
                    home: std::path::PathBuf::from("/profiles").join(id),
                    executable: None,
                    environment: std::collections::BTreeMap::new(),
                },
            );
        }
        config.targets.insert(
            "podman".into(),
            hel::hel_config::TargetTemplate::LocalPodman {
                container: hel::hel_config::ContainerTemplate {
                    image: "ubuntu:24.04".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: std::collections::BTreeMap::new(),
                },
            },
        );
        let mut state = HelState::default();
        for (id, title) in [("session-1", "First"), ("session-2", "Second")] {
            state.sessions.insert(
                id.into(),
                hel::hel_state::SessionRecord {
                    workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                    archived: false,
                    container_cpus: None,
                    container_memory: None,
                    id: id.into(),
                    title: title.into(),
                    harness_kind: hel::hel_config::HarnessKind::Codex,
                    last_profile: "codex-1".into(),
                    bundle_id: "hel".into(),
                    project_directory: None,
                    managed_worktree: None,
                    target_template_id: "podman".into(),
                    resource_allocation: None,
                    additional_mounts: Vec::new(),
                    state: hel::hel_state::SessionState::Running,
                    target: None,
                    native_session_id: None,
                    acp_session_title: None,
                    session_title_override: None,
                    created_at: "2026-08-14T00:00:00Z".into(),
                    updated_at: "2026-08-14T00:00:00Z".into(),
                    viewed_through_event_ordinal: 0,
                    draft_input: String::new(),
                    last_error: None,
                    last_checkpoint_error: None,
                    checkpoint: None,
                },
            );
        }
        DashboardState::new(config, state, std::collections::BTreeMap::new())
    }

    #[test]
    fn durable_terminal_state_settles_remote_lifecycle_overlay() {
        assert!(remote_lifecycle_settled(
            SessionOperationKind::Launching,
            Some(SessionState::Running)
        ));
        assert!(!remote_lifecycle_settled(
            SessionOperationKind::Stopping,
            Some(SessionState::Running)
        ));
        assert!(remote_lifecycle_settled(
            SessionOperationKind::Stopping,
            Some(SessionState::Stopped)
        ));
        assert!(remote_lifecycle_settled(
            SessionOperationKind::Resuming,
            Some(SessionState::Stopped)
        ));
        assert!(remote_lifecycle_settled(
            SessionOperationKind::Launching,
            None
        ));
        assert!(remote_lifecycle_settled(
            SessionOperationKind::Destroying,
            None
        ));
    }

    /// Draws the combined surface exactly as the loop does, so the highlight
    /// and the extraction see the frame it just produced. No conversation is
    /// attached, which stands for a workspace whose sessions are all stopped.
    fn draw_with_selection(
        terminal: &mut Terminal<TestBackend>,
        dashboard: &mut DashboardState,
        selection: &SelectionState,
    ) -> Option<String> {
        let mut text = None;
        terminal
            .draw(|frame| {
                render_combined(frame, dashboard, None, false);
                text = draw_selection(frame, selection, dashboard.frame_surfaces());
            })
            .expect("draw the combined surface");
        text
    }

    fn reversed_cells(terminal: &Terminal<TestBackend>) -> Vec<(u16, u16)> {
        let buffer = terminal.backend().buffer();
        (buffer.area.y..buffer.area.bottom())
            .flat_map(|y| (buffer.area.x..buffer.area.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                buffer
                    .cell(Position::new(x, y))
                    .expect("cell")
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect()
    }

    /// Screen row of the first line holding `needle`.
    fn row_containing(terminal: &Terminal<TestBackend>, needle: &str) -> u16 {
        let buffer = terminal.backend().buffer();
        (buffer.area.y..buffer.area.bottom())
            .find(|y| {
                (buffer.area.x..buffer.area.right())
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("missing {needle} on screen"))
    }

    /// A drag inside a pane belongs to that pane: the range stops at its last
    /// row even though the pointer left it, and the copied text is the pane's
    /// own rows without borders or anything drawn around it.
    #[test]
    fn dragging_inside_a_pane_copies_only_that_panes_rows() {
        let mut dashboard = populated_dashboard();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        let mut selection = SelectionState::new();
        draw_with_selection(&mut terminal, &mut dashboard, &selection);
        let quotas = *dashboard
            .frame_surfaces()
            .surface(SurfaceId::DashboardPane(2))
            .expect("quotas pane registered");

        let press = (quotas.rect.x, quotas.rect.y + 1);
        // Drag out past the pane's bottom-right corner, into the footer.
        let release = (quotas.rect.right() + 10, quotas.rect.bottom() + 5);
        assert_eq!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Down(MouseButton::Left), press.0, press.1),
            ),
            SelectionRouting::Consumed
        );
        assert_eq!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    release.0,
                    release.1
                ),
            ),
            SelectionRouting::Consumed
        );
        assert_eq!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Up(MouseButton::Left), release.0, release.1),
            ),
            SelectionRouting::Copy {
                surface: SurfaceId::DashboardPane(2),
                range: SelectionRange {
                    start: hel::hel_selection::ContentPos::new(1, 0),
                    end: hel::hel_selection::ContentPos::new(2, quotas.rect.width - 1),
                },
            }
        );

        let text = draw_with_selection(&mut terminal, &mut dashboard, &selection)
            .expect("the selection covers text");
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            vec![
                "  claude-1  Claude Code  refreshing…",
                "  codex-1   Codex        refreshing…",
            ]
        );
        // Exactly the two selected pane rows are reversed, border columns and
        // the pane above included.
        let expected = (quotas.rect.y + 1..quotas.rect.bottom())
            .flat_map(|y| (quotas.rect.x..quotas.rect.right()).map(move |x| (x, y)))
            .collect::<Vec<_>>();
        assert_eq!(reversed_cells(&terminal), expected);
    }

    #[test]
    fn tiny_borderless_grid_can_be_selected_and_copied() {
        let mut dashboard = populated_dashboard();
        dashboard.cycle_pane_layout();
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("terminal");
        let mut selection = SelectionState::new();
        draw_with_selection(&mut terminal, &mut dashboard, &selection);
        let surface = *dashboard
            .frame_surfaces()
            .surface(SurfaceId::DashboardPane(0))
            .expect("tiny sessions grid registered");
        assert_eq!(surface.rect.height, 2);

        let start = (surface.rect.x, surface.rect.y);
        let end = (surface.rect.right() - 1, surface.rect.bottom() - 1);
        assert_eq!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Down(MouseButton::Left), start.0, start.1),
            ),
            SelectionRouting::Consumed
        );
        assert_eq!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Drag(MouseButton::Left), end.0, end.1),
            ),
            SelectionRouting::Consumed
        );
        assert!(matches!(
            route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Up(MouseButton::Left), end.0, end.1),
            ),
            SelectionRouting::Copy {
                surface: SurfaceId::DashboardPane(0),
                ..
            }
        ));

        let copied = draw_with_selection(&mut terminal, &mut dashboard, &selection)
            .expect("tiny grid selection extracts text");
        assert!(copied.contains("podman"), "copied grid text: {copied:?}");
    }

    /// A press is held back until the button comes up, then replayed to the
    /// view, so clicking still selects and a second gesture still opens.
    #[test]
    fn click_gestures_reach_the_view_as_presses_and_still_double_click() {
        let mut dashboard = populated_dashboard();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        let mut selection = SelectionState::new();
        draw_with_selection(&mut terminal, &mut dashboard, &selection);
        let row = row_containing(&terminal, "session-2");
        let column = dashboard
            .frame_surfaces()
            .surface(SurfaceId::DashboardPane(0))
            .expect("active pane registered")
            .rect
            .x;

        let mut click = || {
            assert_eq!(
                route_selection_event(
                    &mut selection,
                    dashboard.frame_surfaces(),
                    mouse(MouseEventKind::Down(MouseButton::Left), column, row),
                ),
                SelectionRouting::Consumed,
                "the press waits for the release"
            );
            let SelectionRouting::Forward(event) = route_selection_event(
                &mut selection,
                dashboard.frame_surfaces(),
                mouse(MouseEventKind::Up(MouseButton::Left), column, row),
            ) else {
                panic!("a release without movement forwards a press");
            };
            assert_eq!(
                event,
                mouse(MouseEventKind::Down(MouseButton::Left), column, row)
            );
            dashboard_event_action(&mut dashboard, event)
        };

        assert_eq!(click(), DashboardAction::None, "the first click selects");
        assert_eq!(
            click(),
            DashboardAction::Open {
                session_id: "session-2".into(),
            },
            "a second gesture on the same row opens it"
        );
    }

    #[test]
    fn presses_off_every_surface_and_wheel_events_reach_the_view() {
        let mut surfaces = FrameSurfaces::new();
        surfaces.push(SurfaceFrame::fixed(
            SurfaceId::ModalBody,
            Rect::new(10, 5, 20, 4),
        ));
        let mut selection = SelectionState::new();

        let outside = mouse(MouseEventKind::Down(MouseButton::Left), 2, 2);
        assert_eq!(
            route_selection_event(&mut selection, &surfaces, outside.clone()),
            SelectionRouting::Forward(outside)
        );
        assert_eq!(selection.active_surface(), None);

        // The wheel scrolls whatever it is over, even inside a surface.
        let wheel = mouse(MouseEventKind::ScrollDown, 12, 6);
        assert_eq!(
            route_selection_event(&mut selection, &surfaces, wheel.clone()),
            SelectionRouting::Forward(wheel)
        );
        // A drag that never started on a surface is the view's too.
        let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 12, 6);
        assert_eq!(
            route_selection_event(&mut selection, &surfaces, drag.clone()),
            SelectionRouting::Forward(drag)
        );
    }

    /// A surface that scrolls its own rows still gets highlighted, but its
    /// text is not read back from the frame: most of the selection is off it,
    /// and the surface's own row cache is what holds those rows.
    #[test]
    fn a_scrollable_surface_is_highlighted_without_stashing_frame_text() {
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).expect("terminal");
        let mut surfaces = FrameSurfaces::new();
        surfaces.push(SurfaceFrame::scrollable(
            SurfaceId::Transcript,
            Rect::new(0, 1, 20, 3),
            400,
            9_000,
        ));
        let mut selection = SelectionState::new();
        route_selection_event(
            &mut selection,
            &surfaces,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 1),
        );
        route_selection_event(
            &mut selection,
            &surfaces,
            mouse(MouseEventKind::Up(MouseButton::Left), 19, 3),
        );

        let mut text = Some("stale".to_owned());
        terminal
            .draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("visible transcript row"),
                    Rect::new(0, 1, 20, 3),
                );
                text = draw_selection(frame, &selection, &surfaces);
            })
            .expect("draw");

        assert_eq!(text, None, "the extraction is the transcript's own job");
        assert_eq!(reversed_cells(&terminal).len(), 60, "all three rows lit up");
    }

    #[test]
    fn escape_clears_a_finished_selection_before_the_view_sees_it() {
        let mut surfaces = FrameSurfaces::new();
        surfaces.push(SurfaceFrame::fixed(
            SurfaceId::ModalBody,
            Rect::new(10, 5, 20, 4),
        ));
        let mut selection = SelectionState::new();
        route_selection_event(
            &mut selection,
            &surfaces,
            mouse(MouseEventKind::Down(MouseButton::Left), 11, 6),
        );
        route_selection_event(
            &mut selection,
            &surfaces,
            mouse(MouseEventKind::Up(MouseButton::Left), 15, 7),
        );
        assert!(selection.range().is_some(), "the drag left a selection");

        assert_eq!(
            route_selection_event(&mut selection, &surfaces, escape()),
            SelectionRouting::Consumed
        );
        assert_eq!(selection.range(), None);
        // With nothing selected, Esc is the view's key again.
        assert_eq!(
            route_selection_event(&mut selection, &surfaces, escape()),
            SelectionRouting::Forward(escape())
        );
    }

    /// The dashboard loop batches buffered input and stops at the first event
    /// that asks for work, so events that only need a redraw must report no
    /// action and actionable keys must report theirs.
    #[test]
    fn only_events_that_ask_for_work_end_an_input_batch() {
        let mut dashboard = DashboardState::new(
            HelConfig::default(),
            HelState::default(),
            std::collections::BTreeMap::new(),
        );

        assert!(matches!(
            dashboard_event_action(&mut dashboard, Event::Resize(80, 24)),
            DashboardAction::None
        ));
        // Escape no longer quits the combined surface, so a key that does
        // ask for work stands in for it here.
        assert!(matches!(
            dashboard_event_action(
                &mut dashboard,
                Event::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::F(3),
                    crossterm::event::KeyModifiers::NONE,
                )),
            ),
            DashboardAction::OpenWorkspacePicker
        ));
    }

    /// The global chords the controller answers before anything else sees
    /// the key. These drive the same two calls the batching loop makes.
    fn chord(dashboard: &DashboardState, key: crossterm::event::KeyEvent) -> Option<CommandId> {
        global_chord_event(dashboard, &Event::Key(key))
    }

    fn alt(character: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(character),
            crossterm::event::KeyModifiers::ALT,
        )
    }

    fn function_key(number: u8) -> crossterm::event::KeyEvent {
        plain_key(crossterm::event::KeyCode::F(number))
    }

    fn plain_key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// Walks the focus ring to `wanted`, which is the only way in from
    /// outside the crate that owns the panes.
    fn focus_on(dashboard: &mut DashboardState, wanted: hel_tui::Focus) {
        dashboard.focus_sessions();
        for _ in 0..8 {
            if dashboard.focus() == wanted {
                return;
            }
            dashboard.cycle_focus(false);
        }
        panic!("{wanted:?} is not on the focus ring");
    }

    /// The point of the chord: the user does not have to leave the composer
    /// to start a session.
    #[test]
    fn alt_n_opens_the_new_wizard_while_the_composer_has_focus() {
        let mut dashboard = populated_dashboard();
        dashboard.focus_prompt();

        let command = chord(&dashboard, alt('n')).expect("Alt-N is a global chord");
        assert_eq!(command, CommandId::NewSession);
        assert!(matches!(
            dashboard.dispatch_command(command),
            DashboardAction::None
        ));
        assert!(dashboard.modal_open(), "the new-session wizard is open");
        assert_eq!(dashboard.focus(), hel_tui::Focus::Prompt);
    }

    /// Resume is a chord like new session: the pane letter it used to answer
    /// is gone, so this is the only way in from the composer.
    #[test]
    fn alt_s_opens_the_resume_dialog_from_the_composer() {
        let mut dashboard = populated_dashboard();
        dashboard.focus_prompt();

        let command = chord(&dashboard, alt('s')).expect("Alt-S is a global chord");
        assert_eq!(command, CommandId::ResumeDialog);
        assert!(matches!(
            dashboard.dispatch_command(command),
            DashboardAction::OpenResumeDialog
        ));
        assert_eq!(dashboard.focus(), hel_tui::Focus::Prompt);

        // Like Alt-N, it waits for an open dialog to close.
        dashboard.show_resume_dialog(1, Vec::new());
        assert!(dashboard.modal_open());
        assert_eq!(chord(&dashboard, alt('s')), None);
    }

    /// A chord that would act on a surface the user cannot see waits for the
    /// dialog to close.
    #[test]
    fn alt_n_is_ignored_while_a_modal_is_open() {
        let mut dashboard = populated_dashboard();
        dashboard.focus_prompt();
        let command = chord(&dashboard, alt('n')).expect("Alt-N is a global chord");
        dashboard.dispatch_command(command);
        assert!(dashboard.modal_open());

        assert_eq!(chord(&dashboard, alt('n')), None);
    }

    #[test]
    fn f4_opens_the_web_dialog_from_the_composer() {
        let mut dashboard = populated_dashboard();
        dashboard.focus_prompt();

        let command = chord(&dashboard, function_key(4)).expect("F4 is a global chord");
        assert_eq!(command, CommandId::WebViewer);
        assert!(matches!(
            dashboard.dispatch_command(command),
            DashboardAction::LoadWebAccess
        ));
    }

    #[test]
    fn f3_opens_the_workspace_picker_from_any_pane() {
        for focus in [
            hel_tui::Focus::Sessions,
            hel_tui::Focus::Prompt,
            hel_tui::Focus::Targets,
            hel_tui::Focus::Quota,
        ] {
            let mut dashboard = populated_dashboard();
            focus_on(&mut dashboard, focus);
            let command = chord(&dashboard, function_key(3)).expect("F3 is a global chord");
            assert_eq!(command, CommandId::Workspaces, "{focus:?}");
            assert!(matches!(
                dashboard.dispatch_command(command),
                DashboardAction::OpenWorkspacePicker
            ));
        }
    }

    #[test]
    fn alt_a_marks_all_read_from_the_targets_pane() {
        let mut dashboard = populated_dashboard();
        focus_on(&mut dashboard, hel_tui::Focus::Targets);

        let command = chord(&dashboard, alt('a')).expect("Alt-A is a global chord");
        assert_eq!(command, CommandId::MarkAllRead);
        dashboard.dispatch_command(command);
        // Nothing here is unread, and saying so is how the command reports it
        // ran from a pane that has no `a` of its own.
        assert_eq!(dashboard.notice().as_deref(), Some("No unread sessions."));
    }

    #[test]
    fn alt_x_cancels_the_selected_sessions_launch_from_the_composer() {
        let mut dashboard = populated_dashboard();
        dashboard.focus_sessions();
        let session_id = dashboard
            .selected_session_id()
            .expect("a session is selected")
            .to_owned();
        dashboard.begin_session_operation(
            session_id.clone(),
            SessionOperationKind::Launching,
            None,
        );
        dashboard.focus_prompt();

        let command = chord(&dashboard, alt('x')).expect("Alt-X is a global chord");
        assert_eq!(command, CommandId::CancelOperation);
        assert_eq!(
            dashboard.dispatch_command(command),
            DashboardAction::CancelOperation {
                session_id,
                kind: SessionOperationKind::Launching,
            }
        );
    }

    /// Inside the target-actions dialog Alt-X belongs to the test that dialog
    /// is running, so the pre-filter must leave the key alone.
    #[test]
    fn alt_x_inside_the_target_dialog_cancels_the_running_test() {
        let mut dashboard = populated_dashboard();
        focus_on(&mut dashboard, hel_tui::Focus::Targets);
        assert!(matches!(
            dashboard.handle_key(plain_key(crossterm::event::KeyCode::Enter)),
            DashboardAction::None
        ));
        assert!(dashboard.modal_open(), "the target actions dialog is open");
        // Rename, Test, Close: one Tab lands on Test.
        dashboard.handle_key(plain_key(crossterm::event::KeyCode::Tab));
        assert!(matches!(
            dashboard.handle_key(plain_key(crossterm::event::KeyCode::Enter)),
            DashboardAction::TestTarget { .. }
        ));

        assert_eq!(
            chord(&dashboard, alt('x')),
            None,
            "the dialog keeps the key"
        );
        assert!(matches!(
            dashboard.handle_key(alt('x')),
            DashboardAction::CancelTargetTest
        ));
    }

    #[test]
    fn plain_x_no_longer_cancels_anything() {
        let mut dashboard = populated_dashboard();
        dashboard.focus_sessions();
        let session_id = dashboard
            .selected_session_id()
            .expect("a session is selected")
            .to_owned();
        dashboard.begin_session_operation(session_id, SessionOperationKind::Launching, None);

        let plain_x = plain_key(crossterm::event::KeyCode::Char('x'));
        assert_eq!(chord(&dashboard, plain_x), None);
        assert!(matches!(
            dashboard.handle_key(plain_x),
            DashboardAction::None
        ));
    }

    fn live_session(id: &str, created_at: &str) -> hel::hel_state::SessionRecord {
        hel::hel_state::SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: id.into(),
            title: id.into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
            last_profile: "codex-1".into(),
            bundle_id: "hel".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Running,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: created_at.into(),
            updated_at: created_at.into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    /// The conversation worth opening on is the one whose agent spoke most
    /// recently, which is what the stored summaries record.
    #[test]
    fn startup_opens_the_session_with_the_newest_materialized_activity() {
        let sessions = [
            live_session("session-a", "2026-08-01T00:00:00Z"),
            live_session("session-b", "2026-08-02T00:00:00Z"),
            live_session("session-c", "2026-08-03T00:00:00Z"),
        ];
        let activity = |id: &str| match id {
            "session-a" => Some(10),
            "session-b" => Some(300),
            "session-c" => Some(200),
            _ => None,
        };

        assert_eq!(
            startup_session_choice(sessions.iter(), activity),
            Some("session-b".into())
        );
    }

    /// With no activity recorded — nothing stored yet, or every read failed —
    /// every session ranks equal on the first key, so the newest one wins.
    #[test]
    fn startup_falls_back_to_the_newest_creation_then_the_larger_id() {
        let sessions = [
            live_session("session-a", "2026-08-01T00:00:00Z"),
            live_session("session-b", "2026-08-03T00:00:00Z"),
            live_session("session-c", "2026-08-02T00:00:00Z"),
        ];
        assert_eq!(
            startup_session_choice(sessions.iter(), |_| None),
            Some("session-b".into())
        );

        // A tie on creation time too still resolves the same way on every
        // run, rather than following the iteration order.
        let tied = [
            live_session("session-a", "2026-08-01T00:00:00Z"),
            live_session("session-z", "2026-08-01T00:00:00Z"),
        ];
        assert_eq!(
            startup_session_choice(tied.iter(), |_| None),
            Some("session-z".into())
        );
        assert_eq!(
            startup_session_choice(tied.iter().rev(), |_| None),
            Some("session-z".into())
        );
    }

    #[test]
    fn a_workspace_with_no_live_session_has_nothing_to_open() {
        assert_eq!(
            startup_session_choice(std::iter::empty(), |_| Some(1)),
            None
        );
    }

    /// The pick waits for the summaries it compares, but not for ever, and it
    /// only ever fires once.
    #[test]
    fn the_startup_pick_waits_for_its_summaries_then_gives_up() {
        let start = std::time::Instant::now();
        let mut startup =
            StartupSession::begin(["session-a".to_owned(), "session-b".to_owned()], start);

        assert!(!startup.ready(start), "both summaries are still pending");
        startup.summary_arrived("session-a");
        assert!(!startup.ready(start), "one summary is still pending");
        startup.summary_arrived("session-b");
        assert!(startup.ready(start));
        assert!(!startup.ready(start), "the choice is made only once");

        // A summary that never comes back stops holding the surface up.
        let mut stalled = StartupSession::begin(["session-a".to_owned()], start);
        assert!(!stalled.ready(start));
        assert!(stalled.ready(start + STARTUP_SESSION_WAIT));
    }

    /// The session manager adopts sessions asynchronously after the surface
    /// starts, so the startup pick's first attach usually lands too early.
    /// It has to ride that out rather than give up on the first refusal.
    #[test]
    fn the_startup_pick_retries_an_attach_the_manager_was_not_ready_for() {
        let start = std::time::Instant::now();
        let mut startup = StartupSession::begin(["session-a".to_owned()], start);
        startup.summary_arrived("session-a");
        assert!(startup.ready(start));
        startup.attempting("session-a");

        // The refusal is not worth reporting, and the pick re-arms.
        assert!(startup.attach_failed("session-a", start));
        assert!(startup.ready(start));

        // A failure for some other session is not this pick's business.
        startup.attempting("session-a");
        assert!(!startup.attach_failed("session-b", start));

        // Past the window it stops retrying and the failure is reported.
        assert!(!startup.attach_failed("session-a", start + STARTUP_SESSION_ATTACH_WINDOW));
        assert!(!startup.ready(start + STARTUP_SESSION_ATTACH_WINDOW));
    }

    /// A user who acts during the retries takes the choice back, and the
    /// retries stop rather than yanking a conversation open underneath them.
    #[test]
    fn cancelling_stops_the_startup_attach_retries() {
        let start = std::time::Instant::now();
        let mut startup = StartupSession::begin(["session-a".to_owned()], start);
        startup.summary_arrived("session-a");
        assert!(startup.ready(start));
        startup.attempting("session-a");

        startup.cancel();
        assert!(!startup.attach_failed("session-a", start));
        assert!(!startup.ready(start));
    }

    /// The user acting is the strongest signal there is about which
    /// conversation they want, so it takes the choice away.
    #[test]
    fn a_user_who_acts_first_keeps_the_choice() {
        let start = std::time::Instant::now();
        let mut startup = StartupSession::begin(["session-a".to_owned()], start);

        startup.cancel();
        startup.summary_arrived("session-a");
        assert!(!startup.ready(start));
        assert!(!startup.ready(start + STARTUP_SESSION_WAIT * 10));
    }

    /// An empty workspace has nothing to wait for, so the surface never holds
    /// the keyboard back from the Sessions pane.
    #[test]
    fn a_workspace_with_no_live_session_never_arms_the_startup_pick() {
        let start = std::time::Instant::now();
        let mut startup = StartupSession::begin(std::iter::empty(), start);
        assert!(!startup.ready(start));
        assert!(!startup.ready(start + STARTUP_SESSION_WAIT));
    }

    #[test]
    fn resume_progress_explains_the_blocking_work() {
        assert_eq!(
            resume_progress_notice("0123456789", "codex-1", "podman"),
            "Preparing 01234567: verifying checkpoint, provisioning podman, and restoring codex-1…"
        );
    }

    #[test]
    fn only_a_fully_empty_config_triggers_automatic_setup() {
        let mut config = hel::hel_config::HelConfig::default();
        assert!(configuration_needs_setup(&config));
        config.targets.insert(
            "podman".into(),
            hel::hel_config::TargetTemplate::LocalPodman {
                container: hel::hel_config::ContainerTemplate {
                    image: "ubuntu:24.04".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: std::collections::BTreeMap::new(),
                },
            },
        );
        assert!(!configuration_needs_setup(&config));
    }

    #[test]
    fn materialized_projections_are_single_flight_and_coalesce_to_the_latest_snapshot() {
        let mut in_flight = BTreeSet::new();
        let mut pending = BTreeMap::new();
        let mut first = MaterializedSession::empty("session-1");
        first.applied_event_ordinal = 1;
        let mut superseded = MaterializedSession::empty("session-1");
        superseded.applied_event_ordinal = 2;
        let mut latest = MaterializedSession::empty("session-1");
        latest.applied_event_ordinal = 3;
        let mut stale = MaterializedSession::empty("session-1");
        stale.applied_event_ordinal = 2;

        assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, first, 0).is_some());
        assert!(
            enqueue_materialized_projection(&mut in_flight, &mut pending, superseded, 1).is_none()
        );
        assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, latest, 2).is_none());
        assert!(enqueue_materialized_projection(&mut in_flight, &mut pending, stale, 1).is_none());

        let (queued, receipt) = pending.remove("session-1").unwrap();
        assert_eq!(queued.applied_event_ordinal, 3);
        assert_eq!(receipt, 2);
        assert_eq!(in_flight, BTreeSet::from(["session-1".to_owned()]));
    }

    #[test]
    fn critical_operations_hold_shutdown_until_their_guards_drop() {
        let (tracker, changed) = CriticalOperationTracker::new();
        let first = tracker.begin("saving draft for 01234567");
        let second = tracker.begin("stopping session 89abcdef");

        assert_eq!(tracker.blockers().len(), 2);
        assert_eq!(
            shutdown_wait_notice(&tracker.blockers()).as_deref(),
            Some("Waiting for 2 operations to complete before exiting")
        );
        assert!(changed.has_changed().unwrap());

        drop(first);
        assert_eq!(
            shutdown_wait_notice(&tracker.blockers()).as_deref(),
            Some("Waiting for stopping session 89abcdef to complete before exiting")
        );
        drop(second);
        assert_eq!(shutdown_wait_notice(&tracker.blockers()), None);
    }

    #[test]
    fn shutdown_cancels_process_owning_critical_operations() {
        let (tracker, _changed) = CriticalOperationTracker::new();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _guard = tracker.begin_cancellable("checking repository", cancelled.clone());

        tracker.cancel_all();

        assert!(cancelled.load(Ordering::Acquire));
    }
}
