// Port of mj-cli/src/dashboard/io/spawn.rs.
//
// The spawn helpers that run the dashboard's background jobs. tokio's
// UnboundedSender is a core.Unbounded; a JoinHandle<()> is a dashboardTask;
// tokio_util's DropGuard is the context.CancelFunc the caller must call
// where Rust drops the guard; Arc<AtomicBool> is *atomic.Bool; a
// tokio::sync::Semaphore shared by Arc is a buffered channel of slots. An
// async Rust job is a func(ctx) here: ctx ends when the job is aborted or
// times out, as dropping the Rust future does. Rust's `T: Send + 'static`
// bounds have no Go counterpart.
//
// Rust's `#[cfg(test)] pub(crate) use mj_client::settings::save::save_at as
// save_setup_at` re-export is not declared: Go tests call client's SaveAt
// directly.

package cli

import (
	"context"
	"fmt"
	"log/slog"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// dashboardTask is a spawned dashboard job, Rust's JoinHandle<()>. Go has no
// task abort or drop: abort ends the context the job runs under, and a job
// checks it before reporting, so an aborted job reports nothing, as a Rust
// task dropped at its next await point does. A holder that Rust wraps in
// AbortOnDropHandle calls abort wherever Rust drops it.
type dashboardTask struct {
	handle *core.JoinHandle[struct{}]
	cancel context.CancelFunc
}

// spawnDashboardTask runs job on its own goroutine under a context that
// abort ends. A panic in job is reported by join, never lost.
func spawnDashboardTask(job func(ctx context.Context)) *dashboardTask {
	ctx, cancel := context.WithCancel(context.Background())
	handle := core.Spawn(func() struct{} {
		defer cancel()
		job(ctx)
		return struct{}{}
	})
	return &dashboardTask{handle: handle, cancel: cancel}
}

// abort is JoinHandle::abort: the job's context ends.
func (t *dashboardTask) abort() { t.cancel() }

// done is closed once the job has returned.
func (t *dashboardTask) done() <-chan struct{} { return t.handle.Done() }

// isFinished is JoinHandle::is_finished.
func (t *dashboardTask) isFinished() bool {
	select {
	case <-t.handle.Done():
		return true
	default:
		return false
	}
}

// join is awaiting the JoinHandle: nil once the job returned, the panic it
// died of, or ctx's error when ctx ends first.
func (t *dashboardTask) join(ctx context.Context) error {
	_, err := t.handle.Join(ctx)
	return err
}

// report hands one finished answer to the dashboard. A closed channel means
// the dashboard has already shut down, which is not a failure, so every
// helper reports through here instead of deciding that for itself.
func report[T any](operation string, updates *core.Unbounded[T], update T) {
	if err := updates.Send(update); err != nil {
		slog.Debug("dashboard background result dropped after shutdown", "operation", operation, "error", err.Error())
	}
}

// blockingResult turns a finished blocking job into the answer the
// dashboard shows. err is the job's own failure; joinErr is a panic in the
// job, which arrives here as a join error and becomes a reported failure, so
// the screen never waits forever on work that died.
func blockingResult[T any](operation string, value T, err, joinErr error) (T, error) {
	var zero T
	if joinErr != nil {
		failure := fmt.Errorf("%s task failed: %w", operation, joinErr)
		slog.Warn("dashboard background operation panicked", "operation", operation, "error", failure.Error())
		return zero, failure
	}
	if err != nil {
		slog.Warn("dashboard background operation failed", "operation", operation, "error", err.Error())
		return zero, err
	}
	return value, nil
}

// spawnIO runs one blocking job off the loop and reports its outcome on the
// dashboard's I/O channel. Errors and panics are formatted once, here, so no
// caller can quietly drop one.
func spawnIO[T any](
	operation string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func() (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_io")
}

// spawnCriticalIO runs a user-authored mutation off the event loop and
// keeps dashboard exit pending until the mutation has reached its durable
// boundary.
func spawnCriticalIO[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func() (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_critical_io")
}

// spawnAsyncJob: network waits must not hold a blocking-pool thread:
// connecting to the daemon itself needs a blocking metadata read. Bound
// acknowledgement waits so a silent daemon cannot indefinitely prevent
// dashboard exit. tracker is nil for a job that does not hold exit.
func spawnAsyncJob[T any](
	tracker *client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_async_job")
}

func spawnCriticalAsync[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	return spawnAsyncJob(&tracker, label, updates, timeout, work, toUpdate)
}

func spawnBackgroundAsync[T any](
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	return spawnAsyncJob(nil, label, updates, timeout, work, toUpdate)
}

// loadWorkspaceManagementEntries loads the manager's complete view from the
// daemon: one listing plus one drafts query. The per-workspace snapshot this
// replaced loaded the whole controller once per workspace for session
// previews the manager never shows.
func loadWorkspaceManagementEntries(ctx context.Context, daemonClient *client.DaemonClient) ([]tui.WorkspaceManagementEntry, error) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs load_workspace_management_entries")
}

// workspaceManagementListings is the workspace list alone: names and
// session counts, no drafts.
func workspaceManagementListings(ctx context.Context, daemonClient *client.DaemonClient) ([]tui.WorkspaceManagementEntry, error) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs workspace_management_listings")
}

func fillWorkspaceManagementDrafts(ctx context.Context, daemonClient *client.DaemonClient, entries []tui.WorkspaceManagementEntry) error {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs fill_workspace_management_drafts")
}

// spawnWorkspaceManagementOperation: tracker is nil for an operation that
// does not hold dashboard exit.
func spawnWorkspaceManagementOperation(
	generation uint64,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker *client.CriticalOperationTracker,
	work func(ctx context.Context) (workspaceManagementResult, error),
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_management_operation")
}

func spawnWorkspaceManagementLoad(generation uint64, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_management_load")
}

// spawnWorkspacePaneSizesLoad reads layouts for workspace ids discovered
// after the dashboard opened. Loading is deliberately separate from the
// runtime snapshot so a late result cannot overwrite a pane size the local
// client already edited.
func spawnWorkspacePaneSizesLoad(workspaceIDs []string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_pane_sizes_load")
}

// spawnWorkspaceLayoutsLoad reads conversation layouts for workspace ids
// discovered after the dashboard opened, for the same reason pane sizes are
// read separately.
func spawnWorkspaceLayoutsLoad(workspaceIDs []string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_layouts_load")
}

func spawnWorkspaceCreate(
	generation uint64,
	name string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_create")
}

func spawnWorkspaceRename(
	generation uint64,
	workspaceID string,
	name string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_rename")
}

func spawnWorkspaceCloseCancel(workspaceID string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_close_cancel")
}

// spawnWorkspaceClose: the daemon owns stopping. Detaching the UI must not
// wait for checkpoints, and ordinary save acknowledgement deadlines are too
// short for them.
func spawnWorkspaceClose(generation uint64, workspaceID string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_close")
}

func spawnWorkspaceDraftRecovery(
	generation uint64,
	draftID string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_workspace_draft_recovery")
}

const saveAckTimeout = 15 * time.Second

// spawnCancellableIO is like spawnCriticalIO, with a cooperative
// cancellation flag for work that can own a subprocess while the dashboard
// is shutting down.
func spawnCancellableIO[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func(cancelled *atomic.Bool) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) *dashboardTask {
	_, task := spawnCancellableIOWithToken(tracker, label, updates, work, toUpdate)
	return task
}

func spawnCancellableIOWithToken[T any](
	tracker client.CriticalOperationTracker,
	label string,
	updates *core.Unbounded[dashboardIoUpdate],
	work func(cancelled *atomic.Bool) (T, error),
	toUpdate func(T, error) dashboardIoUpdate,
) (*atomic.Bool, *dashboardTask) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_cancellable_io_with_token")
}

// spawnFirstRunSetup discovers and saves first-run configuration without
// blocking terminal input.
func spawnFirstRunSetup(updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_first_run_setup")
}

// spawnAvailabilityCheck asks the daemon to check launch availability.
// Results arrive on the feed.
func spawnAvailabilityCheck(updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_availability_check")
}

// spawnProfileLoginStatuses reads saved-profile sign-in status off the
// dashboard's loop.
func spawnProfileLoginStatuses(generation uint64, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_profile_login_statuses")
}

func spawnSetupDiscovery(
	generation uint64,
	scope tui.DetectScope,
	machines []client.DaemonPair[string, core.Machine],
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_setup_discovery")
}

func spawnSetupSave(
	generation uint64,
	original string,
	updated string,
	loginProfile *string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_setup_save")
}

func spawnQuotaPreferencesSave(
	prefer []string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_quota_preferences_save")
}

func spawnSpinnerStyleSave(
	style core.SpinnerStyle,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_spinner_style_save")
}

// spawnProjectSourceResolution resolves one raw checkout's Git origin off
// the event loop. Each session is independent, so callers can launch these
// concurrently and redraw as the answers arrive.
func spawnProjectSourceResolution(
	sessionID string,
	sessionName string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_project_source_resolution")
}

// lifecycleOperationRequest is what every session lifecycle operation needs
// to run off the loop.
type lifecycleOperationRequest struct {
	sessionID string
	// sessionName is the session's name as notices give it, for the
	// operation's label.
	sessionName string
	kind        tui.SessionOperationKind
	cancelled   *atomic.Bool
	updates     *core.Unbounded[dashboardLifecycleUpdate]
}

// lifecycleSubmission keeps client ownership until the daemon confirms
// lifecycle admission. Rust shares an `Arc<Mutex<Option<guard>>>` that the
// first release takes; the copies here share one guard, whose Release is
// idempotent, so the first release is the one that counts.
type lifecycleSubmission struct {
	guard *client.CriticalOperationGuard
}

func (s lifecycleSubmission) submit(
	ctx context.Context,
	daemonClient *client.DaemonClient,
	request client.SessionLifecycleRequest,
) (client.LifecycleReceipt, error) {
	receipt, err := daemonClient.StartSessionLifecycle(ctx, request)
	if err != nil {
		return client.LifecycleReceipt{}, err
	}
	s.release()
	return receipt, nil
}

func (s lifecycleSubmission) release() {
	s.guard.Release()
}

// spawnLifecycleOperation protects submission, then follows daemon-owned
// work without holding quit.
func spawnLifecycleOperation(
	request lifecycleOperationRequest,
	tracker client.CriticalOperationTracker,
	work func(cancelled *atomic.Bool, submission lifecycleSubmission) (daemon.LifecycleSuccess, error),
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_lifecycle_operation")
}

// spawnMaterializedSessionProjection: permits is the projection slots the
// dashboard shares (Rust: Arc<Semaphore>).
func spawnMaterializedSessionProjection(
	projection materializedProjection,
	viewedThroughEventOrdinal uint64,
	previous tui.MaterializedProjectionCache,
	updates *core.Unbounded[dashboardIoUpdate],
	permits chan struct{},
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_materialized_session_projection")
}

// spawnStoppedSubagentTranscript loads the stored tail of a stopped
// sub-agent's conversation: as much as a live chat keeps, off the event
// loop.
func spawnStoppedSubagentTranscript(sessionID string, updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_stopped_subagent_transcript")
}

func spawnNotificationMessageRead(
	request tui.NotificationReadRequest,
	generation uint64,
	permits chan struct{},
	updates *core.Unbounded[dashboardIoUpdate],
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_notification_message_read")
}

func spawnStoredSessionSummary(
	sessionID string,
	viewedThroughEventOrdinal uint64,
	generation uint64,
	permits chan struct{},
	updates *core.Unbounded[dashboardIoUpdate],
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_stored_session_summary")
}

func spawnLifecycleReload(reload lifecycleReload, updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_lifecycle_reload")
}

func spawnDashboardRename(
	sessionID string,
	sessionName string,
	title string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_dashboard_rename")
}

func spawnDashboardNotificationsIgnored(
	sessionID string,
	sessionName string,
	ignored bool,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_dashboard_notifications_ignored")
}

func spawnDashboardChangeWorkspace(
	sessionID string,
	sessionName string,
	workspaceID string,
	workspaceName string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_dashboard_change_workspace")
}

type configRenameRequest struct {
	what    string
	oldID   string
	newID   string
	profile bool
}

func spawnConfigRename(
	request configRenameRequest,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_config_rename")
}

// spawnQuotaRefreshRequest asks the daemon, the only process that probes
// quota, to probe now. The request returns once the daemon has accepted it;
// the reports arrive through the runtime feed.
func spawnQuotaRefreshRequest(updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_quota_refresh_request")
}

func spawnCapacityRefreshRequest(updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_capacity_refresh_request")
}

// spawnQuotaProbeRequest asks the daemon to probe one profile's quota now,
// even a disabled profile's. The answer arrives through the runtime feed
// like any other.
func spawnQuotaProbeRequest(profileID string, updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_quota_probe_request")
}

// containerSettingsRequest is what the container editor asks the
// controller to persist.
type containerSettingsRequest struct {
	sessionID          string
	resourceAllocation *core.SessionResourceAllocation
	additionalMounts   []core.AdditionalMount
	mountHistory       []string
}

func spawnDashboardContainerSettings(
	request containerSettingsRequest,
	sessionName string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_dashboard_container_settings")
}

// namedSession is a session by its id and by the name notices give it, for
// background jobs whose label names the session they work on.
type namedSession struct {
	id   string
	name string
}

// spawnTerminalDraftPersist is one revisioned owner write; the cache
// sequences and coalesces each session.
func spawnTerminalDraftPersist(
	clientID string,
	session namedSession,
	revision uint64,
	draft core.DetachedSessionDraft,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_terminal_draft_persist")
}

func spawnReadReceiptPersist(
	clientID string,
	workspaceID string,
	session namedSession,
	through uint64,
	retryDelay time.Duration,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_read_receipt_persist")
}

func spawnClipboardRead(target tui.DashboardPasteTarget, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_clipboard_read")
}

// spawnClipboardWrite writes copied text to the desktop clipboard off the
// render loop.
func spawnClipboardWrite(text string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_clipboard_write")
}

func spawnCreateBundle(
	sources []string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_create_bundle")
}

func spawnRemoveBundle(
	bundleID string,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_remove_bundle")
}

func spawnImportedSessionApply(
	imported dashboardImportSuccess,
	pending pendingDashboardImport,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_imported_session_apply")
}

// spawnResumeCandidates loads the stopped sessions the resume dialog lists.
// The runtime feed carries only live sessions.
func spawnResumeCandidates(discoveryID uint64, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_resume_candidates")
}

// spawnResumeRecord fetches the whole record of the session picked in the
// resume dialog.
func spawnResumeRecord(sessionID string, updates *core.Unbounded[dashboardIoUpdate]) *dashboardTask {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_resume_record")
}

func spawnCheckpointArchiveSizeRefresh(
	generation uint64,
	targets map[string]string,
	updates *core.Unbounded[dashboardIoUpdate],
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_checkpoint_archive_size_refresh")
}

// goRecipeSave is the fast-start setup a new session saves before it
// registers: Rust's `Option<(PathBuf, GoRecipe, bool)>` argument of
// spawn_dashboard_create_session (directory, recipe, global).
type goRecipeSave struct {
	directory string
	recipe    core.GoRecipe
	global    bool
}

// spawnDashboardCreateSession: registering a session and provisioning it
// are one job with two answers: the dashboard shows the session as soon as
// it exists, then follows the launch, so this stays separate from
// spawnLifecycleOperation. Only the registration holds dashboard exit: once
// the daemon has the record, the launch is the daemon's. goSave is nil when
// no fast-start setup is saved.
func spawnDashboardCreateSession(
	action tui.DashboardAction,
	goSave *goRecipeSave,
	updates *core.Unbounded[dashboardIoUpdate],
	lifecycleUpdates *core.Unbounded[dashboardLifecycleUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_dashboard_create_session")
}

// followDaemonCreation reports how a session the daemon has accepted
// finishes launching.
//
// The daemon owns the launch from registration on, and it continues after
// this terminal exits. Callers therefore release their exit guard before
// calling this, so quitting never waits for a launch or its rollback.
func followDaemonCreation(
	operation string,
	sessionID string,
	lifecycleUpdates *core.Unbounded[dashboardLifecycleUpdate],
	cancelled *atomic.Bool,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs follow_daemon_creation")
}

// awaitDaemonCreation waits for the daemon to finish creating a session,
// across daemon restarts.
//
// A daemon that stops mid-create does not fail it: the next daemon finishes
// it from its durable record, and any daemon answers `WaitCreateSession`
// from that record. A lost connection therefore means "ask again", never
// "failed". The wait is read-only, so repeating it is safe;
// `StartCreateSession` is never repeated. Each retry follows a lost
// connection and waits a quarter second, and the loop ends with the
// daemon's answer or the caller's cancellation.
func awaitDaemonCreation(ctx context.Context, sessionID string, cancelled *atomic.Bool) (daemon.LifecycleSuccess, error) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs await_daemon_creation")
}

// followCreateWaitRetryDelay is follow_create_wait's RETRY_DELAY: the pause
// after a lost connection before the wait is asked again.
const followCreateWaitRetryDelay = 250 * time.Millisecond

// followCreateWait is the loop of awaitDaemonCreation, with the way it
// reaches a daemon given, so a test can answer it from a fake daemon.
func followCreateWait(
	ctx context.Context,
	sessionID string,
	cancelled *atomic.Bool,
	connect func(ctx context.Context) (*client.DaemonClient, error),
) (daemon.LifecycleSuccess, error) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs follow_create_wait")
}

// supervisedWikiTask keeps each read or follower behind its own join
// boundary. A panic becomes a visible failure without cancelling the
// sibling search; ending ctx still cancels its worker.
func supervisedWikiTask[T any](
	ctx context.Context,
	operation string,
	work func(ctx context.Context) (T, error),
) (T, error) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs supervised_wiki_task")
}

// spawnWikiSearch: calling the returned cancel (Rust: dropping the guard)
// cancels debounce and network waits.
func spawnWikiSearch(
	requestID uint64,
	query string,
	delay time.Duration,
	updates *core.Unbounded[dashboardIoUpdate],
) context.CancelFunc {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_wiki_search")
}

func spawnWikiSync(dialog bool, episode uint64, updates *core.Unbounded[dashboardIoUpdate]) context.CancelFunc {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_wiki_sync")
}

// followWikiSync: one episode belongs to the daemon that accepted its
// request.
func followWikiSync(
	ctx context.Context,
	daemonClient *client.DaemonClient,
	answer func(client.WikiSyncStatus),
) error {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs follow_wiki_sync")
}

// spawnSessionsTextSearch searches the conversations of the live sessions
// for the Sessions filter's text, after the typing pause.
//
// The wait and the daemon call are in a task, so the render loop never
// waits on the index. A task whose request id is no longer the newest when
// it wakes stops without asking the daemon, and the dashboard drops an
// answer that names an older request.
func spawnSessionsTextSearch(requestID uint64, query string, updates *core.Unbounded[dashboardIoUpdate]) context.CancelFunc {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_sessions_text_search")
}

// wikiSearchDebounce is how long the dialog waits after a keystroke before
// asking the index.
const wikiSearchDebounce = 250 * time.Millisecond

// wikiSearchLimit is how many archived sessions one search asks for.
const wikiSearchLimit = 50

// wikiBriefChars is how much of an archived transcript the preview pane
// asks for. It is a few lines tall, so a whole briefing would be wasted
// work.
const wikiBriefChars = 4_000

func spawnWikiBrief(wikiID string, updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_wiki_brief")
}

// wikiHitsContext is how many messages of context the hit preview asks for
// on each side of a matching message: enough to see what the match
// answered.
const wikiHitsContext = 1

// wikiHitsChars is how much of one message the hit preview asks for. The
// pane is scrollable but hit-centred, so a long message is shown around its
// match.
const wikiHitsChars = 2_000

func spawnWikiHits(wikiID string, query string, updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_wiki_hits")
}

// spawnDashboardRestoreSession starts a session from an archived transcript
// and follows it through creation the same way a new session is followed.
func spawnDashboardRestoreSession(
	request client.WikiRestoreRequest,
	retryLaunch tui.DashboardAction,
	updates *core.Unbounded[dashboardIoUpdate],
	lifecycleUpdates *core.Unbounded[dashboardLifecycleUpdate],
	tracker client.CriticalOperationTracker,
) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs spawn_dashboard_restore_session")
}

// machineRequest: each disposable read owns a connection. Superseding the
// input closes that connection, allowing the daemon to stop its executor
// and reap its children.
func machineRequest(action client.DaemonAction, cancelled *atomic.Bool) (client.DaemonReply, error) {
	panic("unported: mj-cli/src/dashboard/io/spawn.rs machine_request")
}
