// Port of mj-cli/src/dashboard.rs.
//
// The interactive session dashboard.
//
// One loop owns the terminal. It waits on every background feed at once,
// then batches whatever queued behind the message that woke it, so a burst
// of updates costs one draw. Nothing in this loop blocks: filesystem,
// database, process, and network work all run as the tasks in
// dashboard_io.go and pollers.go, and answer over channels.
//
// dashboardContext holds everything the loop owns, which is what lets the
// wait, the drains, and the action handling in dashboard_actions.go be
// separate functions over the same state.
//
// Terminal input is Bubble Tea's: crossterm's Event becomes a tea.Msg
// (tea.KeyPressMsg, tea.MouseMsg, tea.PasteMsg, tea.WindowSizeMsg, ...),
// read by the terminal guard and delivered over terminalEvents. Rust's
// tokio watch receivers are *core.Watch, unbounded senders
// *core.Unbounded, bounded ones Go channels, CancellationToken a
// context.Context with its CancelFunc, and DropGuard a CancelFunc called on
// release.

package cli

import (
	"cmp"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/mjimport"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// dashboardClockTick is the redraw cadence for displays that move with the
// wall clock: turn timers, countdowns, and elapsed times.
const dashboardClockTick = time.Second

// startupSessionWait is how long the surface waits for stored session
// summaries before it opens a conversation anyway. The summaries decide
// which session has the newest activity; a stalled read must not leave the
// screen without a conversation.
const startupSessionWait = 2 * time.Second

// startupSession is whether the surface still gets to choose which
// conversation it opens on.
//
// It waits for the stored summaries, because those carry the activity times
// the choice compares — but only for startupSessionWait, and only until the
// user makes the choice themselves.
type startupSession struct {
	// pendingSummaries are live sessions whose stored summary has not come
	// back yet.
	pendingSummaries map[string]struct{}
	// openPending is false once the choice has been made, or taken away.
	openPending bool
	deadline    time.Time
}

// idleStartupSession has nothing to choose: the workspace has no live
// session, or one is already open.
func idleStartupSession() startupSession {
	return startupSession{pendingSummaries: map[string]struct{}{}, deadline: time.Now()}
}

func beginStartupSession(sessionIDs []string, now time.Time) startupSession {
	pending := make(map[string]struct{}, len(sessionIDs))
	for _, id := range sessionIDs {
		pending[id] = struct{}{}
	}
	return startupSession{
		pendingSummaries: pending,
		openPending:      len(pending) > 0,
		deadline:         now.Add(startupSessionWait),
	}
}

// summaryArrived: one session has answered, whether its summary loaded or
// failed.
func (s *startupSession) summaryArrived(sessionID string) {
	delete(s.pendingSummaries, sessionID)
}

// cancel: the user acted, so the choice is theirs now.
func (s *startupSession) cancel() { s.openPending = false }

// pickPending reports whether the pick is still to come. While it is,
// nothing else may open a conversation on the surface's behalf.
func (s *startupSession) pickPending() bool { return s.openPending }

// ready reports whether the pick should run now. Answering true once
// retires the choice, so a later tick cannot open a second conversation.
func (s *startupSession) ready(now time.Time) bool {
	if !s.openPending {
		return false
	}
	if len(s.pendingSummaries) > 0 && now.Before(s.deadline) {
		return false
	}
	s.openPending = false
	return true
}

// startupSessionChoice is the live session the surface should open on: the
// one with the newest recorded activity.
//
// Ties break by newest creation time and then by the larger session id, so
// the choice is the same on every run. When no summary carried an activity
// time — none is stored yet, or every read failed — every session ranks
// equal on the first key and creation time decides, which is the intended
// fallback rather than an accident.
func startupSessionChoice(workspaceID *string, sessions []*core.SessionRecord, activityAtMs func(string) (uint64, bool)) *string {
	activity := func(id string) uint64 {
		at, _ := activityAtMs(id)
		return at
	}
	var best *core.SessionRecord
	for _, session := range sessions {
		if workspaceID == nil || session.WorkspaceID != *workspaceID ||
			!session.State.IsActive() ||
			// A failed target has no worker: opening it is a decision for
			// the person, not a place to start.
			session.State == core.SessionStateError {
			continue
		}
		if best == nil {
			best = session
			continue
		}
		// Rust's max_by keeps the later of equal elements.
		order := cmp.Compare(activity(session.ID), activity(best.ID))
		if order == 0 {
			// CompareByCreation orders oldest first, so the newer of the
			// two is the greater.
			order = session.CompareByCreation(best)
		}
		if order == 0 {
			order = cmp.Compare(session.ID, best.ID)
		}
		if order >= 0 {
			best = session
		}
	}
	if best == nil {
		return nil
	}
	id := best.ID
	return &id
}

// animationTick is the fastest activity animation; individual styles
// advance from monotonic time.
const animationTick = time.Duration(chat.SpinnerRedrawIntervalMs) * time.Millisecond

// selectionAutoscrollTick is the scroll cadence while a drag is held past a
// scrollable surface's edge.
const selectionAutoscrollTick = 80 * time.Millisecond

const quotaRefreshNotice = "Refreshing targets and quotas…"
const quotaRefreshedNotice = "Targets and quotas refreshed."

// dashboardExit is how a dashboard run ended.
type dashboardExit interface{ isDashboardExit() }

type dashboardExitNormal struct{}

type dashboardExitDetached struct{ sessions detachedSessions }

type dashboardExitInterrupted struct{}

// dashboardExitRestart hands the terminal to an upgraded executable.
type dashboardExitRestart struct {
	target UpgradeTarget
	resume *upgradeResume
}

func (dashboardExitNormal) isDashboardExit()      {}
func (dashboardExitDetached) isDashboardExit()    {}
func (dashboardExitInterrupted) isDashboardExit() {}
func (dashboardExitRestart) isDashboardExit()     {}

// detachedSessions is what a detaching terminal knows about the sessions it
// leaves running.
type detachedSessions interface{ isDetachedSessions() }

type detachedSessionsKnown struct{ count int }

type detachedSessionsDaemonNotRunning struct{}

type detachedSessionsUnknown struct{}

func (detachedSessionsKnown) isDetachedSessions()            {}
func (detachedSessionsDaemonNotRunning) isDetachedSessions() {}
func (detachedSessionsUnknown) isDetachedSessions()          {}

// Rust re-exports mj_client::operations::CriticalOperationTracker here; Go
// callers use client.CriticalOperationTracker.

// shutdownWaitNotice names what a quit is still waiting for; nil when
// nothing.
func shutdownWaitNotice(blockers []string) *string {
	var notice string
	switch len(blockers) {
	case 0:
		return nil
	case 1:
		notice = fmt.Sprintf("Waiting for %s to complete before exiting", blockers[0])
	default:
		notice = fmt.Sprintf("Waiting for %d operations to complete before exiting", len(blockers))
	}
	return &notice
}

// quitTimings is when each step of the last dashboard exit began. The
// dashboard records its steps; `main` adds the runtime shutdown and logs
// them all in one line just before the process exits, so a slow quit names
// the step that waited.
type quitTimings struct {
	// requestedAt is nil when the loop ended without a quit request.
	requestedAt              *time.Time
	loopEndedAt              time.Time
	autoAfkStartedAt         time.Time
	autoAfkFinishedAt        time.Time
	savesStartedAt           time.Time
	savesFinishedAt          time.Time
	sessionManagerStartedAt  time.Time
	sessionManagerFinishedAt time.Time
	dashboardDropStartedAt   time.Time
	dashboardDropFinishedAt  time.Time
	returnedAt               time.Time
	attachmentShutdown       time.Duration
	detachConnect            time.Duration
	detachRequest            time.Duration
}

// lastQuitTimings is Rust's QUIT_TIMINGS static.
var lastQuitTimings struct {
	mu      sync.Mutex
	timings *quitTimings
}

// logQuitTimings logs the last dashboard exit's steps.
// runtimeShutdownStarted is when `main` began shutting the runtime down,
// which ends the teardown step.
func logQuitTimings(runtimeShutdownStarted time.Time) {
	lastQuitTimings.mu.Lock()
	timings := lastQuitTimings.timings
	lastQuitTimings.timings = nil
	lastQuitTimings.mu.Unlock()
	if timings == nil {
		return
	}
	// saturating_duration_since: a step that ended before it began is zero.
	ms := func(from, to time.Time) int64 { return max(to.Sub(from), 0).Milliseconds() }
	exitedAt := time.Now()
	startedAt := timings.loopEndedAt
	if timings.requestedAt != nil {
		startedAt = *timings.requestedAt
	}
	cleanupStarted := timings.savesStartedAt
	if timings.sessionManagerStartedAt.Before(cleanupStarted) {
		cleanupStarted = timings.sessionManagerStartedAt
	}
	cleanupFinished := timings.savesFinishedAt
	if timings.sessionManagerFinishedAt.After(cleanupFinished) {
		cleanupFinished = timings.sessionManagerFinishedAt
	}
	slog.Info("the dashboard exited",
		"total_ms", ms(startedAt, exitedAt),
		"pending_writes_ms", ms(startedAt, timings.loopEndedAt),
		"detach_count_ms", ms(timings.loopEndedAt, timings.autoAfkStartedAt),
		"auto_afk_ms", ms(timings.autoAfkStartedAt, timings.autoAfkFinishedAt),
		"terminal_restore_ms", ms(timings.autoAfkFinishedAt, timings.savesStartedAt),
		"final_saves_ms", ms(timings.savesStartedAt, timings.savesFinishedAt),
		"session_manager_ms", ms(timings.sessionManagerStartedAt, timings.sessionManagerFinishedAt),
		"cleanup_ms", ms(cleanupStarted, cleanupFinished),
		"dashboard_drop_ms", ms(timings.dashboardDropStartedAt, timings.dashboardDropFinishedAt),
		"attachment_shutdown_ms", timings.attachmentShutdown.Milliseconds(),
		"detach_connect_ms", timings.detachConnect.Milliseconds(),
		"detach_request_ms", timings.detachRequest.Milliseconds(),
		"teardown_ms", ms(timings.returnedAt, runtimeShutdownStarted),
		"runtime_shutdown_ms", ms(runtimeShutdownStarted, exitedAt),
		"quit_requested", timings.requestedAt != nil,
	)
}

// recordAttachmentTeardown records main's sequential attachment teardown in
// the same exit record as the dashboard-owned cleanup stages.
func recordAttachmentTeardown(dashboardDropFinishedAt time.Time, attachmentShutdown, detachConnect, detachRequest time.Duration) {
	lastQuitTimings.mu.Lock()
	defer lastQuitTimings.mu.Unlock()
	if timings := lastQuitTimings.timings; timings != nil {
		timings.dashboardDropFinishedAt = dashboardDropFinishedAt
		timings.attachmentShutdown = attachmentShutdown
		timings.detachConnect = detachConnect
		timings.detachRequest = detachRequest
	}
}

// finishShutdownSave bounds acknowledgements, not the daemon's accepted
// mutation. A timeout must report uncertain durability and must not replay
// the mutation.
func finishShutdownSave(label string, deadline time.Time, save func(ctx context.Context) error) error {
	ctx, cancel := context.WithDeadline(context.Background(), deadline)
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- save(ctx) }()
	select {
	case err := <-done:
		if err != nil {
			return fmt.Errorf("Could not save %s before exit: %w", label, err)
		}
		return nil
	case <-ctx.Done():
		return fmt.Errorf("The daemon did not acknowledge %s before exit; the save may still complete", label)
	}
}

// activeDashboardImport is the import task the dashboard is running.
type activeDashboardImport struct {
	taskID    uint64
	cancelled *atomic.Bool
}

// cachedQuestionDraft is a local form snapshot together with the relay
// frontier at which it was captured. A projection older than that frontier
// must not invalidate a freshly saved draft while an attachment is still
// settling. It is part of the terminal upgrade handoff, so its JSON is
// serde's.
type cachedQuestionDraft struct {
	Draft                chat.ChatElicitationDraft `json:"draft"`
	CapturedEventOrdinal uint64                    `json:"captured_event_ordinal"`
}

// importProfileUpdate is one profile the import discovery found, for the
// discovery run it belongs to (Rust's `(u64, ImportProfileOption)`).
type importProfileUpdate struct {
	discoveryID uint64
	option      tui.ImportProfileOption
}

// generationTask is a background task the dashboard cancels by releasing
// it, with the request generation it serves (Rust's
// `(u64, DropGuard)`).
type generationTask struct {
	generation uint64
	cancel     context.CancelFunc
}

// goContextRefresh is the session whose fast-start context is due for a
// refresh, and when (Rust's `(String, Instant)`).
type goContextRefresh struct {
	sessionID string
	at        time.Time
}

// pathInputJob is the path completion in flight and the draft it was asked
// for (Rust's `(String, Arc<AtomicBool>)`).
type pathInputJob struct {
	draft     string
	cancelled *atomic.Bool
}

// sessionPreflight is the cancellable worker resolving an isolated
// session's network clone plan, keyed by the TUI generation that requested
// it (Rust's `(u64, Arc<AtomicBool>)`).
type sessionPreflight struct {
	generation uint64
	cancelled  *atomic.Bool
}

// sessionThrough is how far one session has been read (Rust's
// `(String, u64)`).
type sessionThrough struct {
	sessionID string
	through   uint64
}

// dashboardContext is everything one dashboard run owns.
type dashboardContext struct {
	terminal               *terminalGuard
	controller             controller.Controller
	workspaceID            string
	clientID               string
	dashboard              *tui.DashboardState
	paneSizePersistence    *workspaceSettingPersistence[core.PaneSizes]
	sidebarSizePersistence *workspaceSettingPersistence[core.PaneSize]
	layoutPersistence      *workspaceSettingPersistence[core.ConversationLayout]
	knownWorkspaceLayouts  map[string]struct{}
	// workspaceLayouts is the conversation pane arrangement loaded for each
	// workspace. M4 hands these to the terminal UI; until then they are
	// only stored and saved.
	workspaceLayouts map[string]core.ConversationLayout
	// notices is one notifications bar for the whole process: the dashboard
	// and every chat view opened from it report through this shared handle.
	notices chat.Notices
	// daemonRunningAgainSince is when the "daemon is running again" notice
	// went up, so the clock tick can take it down again. Nothing else clears
	// it on an idle dashboard.
	daemonRunningAgainSince *time.Time
	// events is the terminal input, which remains owned by this event loop,
	// including during Setup. Nil while the setup dialog owns the terminal.
	events terminalEvents
	login  *activeLogin
	// splash is the startup splash, drawn in place of the dashboard until it
	// has played out. Everything behind it is already running.
	splash *splashPlayback
	// chats is every warm conversation, keyed by session id: one for each
	// pane that shows a session. All of them are pumped on every loop
	// iteration, so a conversation stays current whether or not it is the
	// one with the keyboard, and moving between panes is a redraw rather
	// than a rebuild.
	chats map[string]*chat.ActiveChat
	// runtimeSessions are the live session views published by the
	// controller runtime feed.
	runtimeSessions core.SnapshotMap[string, client.RuntimeSessionView]
	// departedRecords is the last record of each session that left the
	// runtime feed while a chat still showed it. The feed carries only live
	// sessions, but the chat still detaches, and this says where its draft
	// and read position belong. Kept only while the chat exists.
	departedRecords map[string]core.SessionRecord
	// questionDrafts are in-memory form drafts for sessions that are not
	// currently attached. Each value retains its complete request identity
	// (and may contain one primary and one deferred reviewer form), so an id
	// reused by a changed form cannot inherit old answers.
	questionDrafts      map[string][]cachedQuestionDraft
	transcriptPositions map[string]chat.TranscriptPosition
	questionTargetsSeen map[string]map[string]struct{}
	// composerDrafts: composer text is owned by this terminal once a session
	// is opened. The inherited shared value is retained only as a
	// compare-and-clear baseline for the detach persistence task.
	composerDrafts         composerDraftCache
	composerDaemonRequests map[string]chat.ChatDaemonSender
	// draftSaveFailures are retained so quitting cannot erase their notice
	// before it is read.
	draftSaveFailures map[string]string
	// openingChatSessions: session-manager attachment is asynchronous; an
	// actor may need to answer from a worker or relay before a chat can be
	// built. Each pane runs its own attach, so opening one conversation
	// never cancels another.
	openingChatSessions map[tui.PaneId]string
	attachments         map[string]*sessionAttachment
	// drawnChatSessions are the sessions the last frame drew a conversation
	// for. Read receipts follow what was on screen.
	drawnChatSessions []string
	// startup is which conversation the surface opens on, and whether it is
	// still the surface's choice to make.
	startup           startupSession
	goContextRefresh  *goContextRefresh
	goContextInFlight bool
	// gitProbesInFlight are sessions whose checkout is being read right now,
	// so a slow target is asked once rather than once per tick.
	gitProbesInFlight    map[string]struct{}
	goSelectionRequested *string
	goSelectionInFlight  bool
	// drawnNoticeGeneration is the notice generation the frame on screen was
	// drawn from. Background work writes the shared notice slot without
	// waking the loop, so the once-a-second clock compares against this to
	// notice one.
	drawnNoticeGeneration uint64
	// controllerChanged: the poll targets are recomputed only after the
	// controller may have changed.
	controllerChanged bool
	quitDetached      bool
	// shutdownRequestedAt is when the person asked to quit; the quit timing
	// log measures from here.
	shutdownRequestedAt       *time.Time
	criticalOperations        client.CriticalOperationTracker
	criticalOperationsChanged *core.Watch[uint64]
	// loopStalls is built here because it reads the scheduler statistics of
	// the thread that opens the dashboard, which is the thread that runs its
	// loop.
	loopStalls loopStallsMeter
	// daemonPresence is what the two-second keep-alive last saw. It never
	// starts a daemon, so a daemon that goes away stays away until the user
	// asks for it back.
	daemonPresence *core.Watch[DaemonPresence]
	autoAfk        *autoAfkReporter

	// quota is what the daemon says about quota. The daemon is the only
	// prober; this process never asks a provider.
	quota               *daemon.Feed[client.QuotaSnapshot]
	profileCapabilities *daemon.Feed[core.ProfileCapabilitiesSnapshot]
	// manualQuotaRefreshCycles is the daemon's finished-cycle count when the
	// person last pressed Refresh, until a later cycle ends.
	manualQuotaRefreshCycles *uint64
	// quotaCycles is the daemon's finished-cycle count in the latest
	// snapshot.
	quotaCycles      uint64
	targetTestCancel *atomic.Bool
	pathInputJob     *pathInputJob
	// completionJob is the completion request in flight, with the draft it
	// was asked for. A newer request or a changed draft cancels it.
	completionJob *pathInputJob
	// sessionPreflightCancel is the cancellable worker resolving an isolated
	// session's network clone plan, keyed by the TUI generation that
	// requested it.
	sessionPreflightCancel *sessionPreflight

	worker                 *daemon.Feed[sessionruntime.SessionManagerUpdate]
	runtimeState           *daemon.Feed[daemon.RuntimeStateUpdate]
	runtimeAlerts          *daemon.Feed[daemon.AlertBatch]
	runtimeSubagentParents *core.Watch[map[string]struct{}]
	runtimeHealth          *daemon.Feed[daemon.RuntimeFeedHealth]
	// runtimeReviews are the reviews the daemon is running. The chat renders
	// one of these rather than driving a review of its own.
	runtimeReviews *daemon.Feed[[]client.RuntimeReviewView]
	// runtimeNotices are background events the daemon reported. Only ones
	// newer than reportedNoticeID reach the notice bar, so a surface that
	// attaches late does not replay a backlog.
	runtimeNotices   *daemon.Feed[[]client.RuntimeNotice]
	reportedNoticeID *uint64
	// runtimeUIRequests are dashboard requests made on the person's behalf
	// (`mj ui`). Only ones newer than those present at attach are applied.
	runtimeUIRequests *daemon.Feed[[]client.UiRequest]
	uiRequestCursor   client.UiRequestCursor
	// uiRequestActions are what the applied requests asked of the host, run
	// after the frame's own action.
	uiRequestActions []tui.DashboardAction
	// runtimeReviewViews is the last complete review projection, retained
	// even while the session list is on screen so a subsequently opened chat
	// starts in the right state.
	runtimeReviewViews        map[string][]client.RuntimeReviewView
	runtimeConfig             *daemon.Feed[daemon.RuntimeLaunchConfig]
	pendingRuntimeUpdates     []pendingRuntimeUpdate
	remoteLifecycleOperations map[string]remoteLifecycleObservation
	// pendingWorkspaceSelection is a workspace the user just created, opened
	// once the feed carries it.
	pendingWorkspaceSelection *string
	workerCommandsTx          sessionruntime.SessionManagerControl
	workerShutdown            *sessionruntime.SessionManagerShutdown
	workerDiagnoses           daemon.WorkerDiagnosisTracker

	lifecycleUpdatesTx  *core.Unbounded[dashboardLifecycleUpdate]
	lifecycle           *daemon.Feed[dashboardLifecycleUpdate]
	lifecycleOperations map[string]*activeLifecycleOperation

	importUpdatesTx   chan<- importProfileUpdate
	importProfiles    *daemon.Feed[importProfileUpdate]
	importTaskTx      chan<- dashboardImportUpdate
	importTasks       *daemon.Feed[dashboardImportUpdate]
	pendingImport     *pendingDashboardImport
	importDiscoveryID uint64
	// nativeScanCache is one scan cache for the process, so reopening the
	// resume dialog reparses only the native session files that changed.
	nativeScanCache  *mjimport.NativeScanCache
	nextImportTaskID uint64
	activeImport     *activeDashboardImport
	// clipboardReadInFlight: at most one desktop clipboard IPC request is
	// allowed at a time. The request itself runs on a blocking worker; this
	// flag keeps repeated Ctrl-V key repeats from creating an unbounded queue
	// of reads.
	clipboardReadInFlight bool
	startupPrompts        startupPrompts
	// selection is pane-scoped text selection, driven by the mouse events
	// this loop intercepts before the views see them.
	selection chat.SelectionState
	// selectionText is the text under the selection, read out of the buffer
	// the last draw produced. Copy redraws first, so it never reads a stale
	// frame.
	selectionText *string

	wikiSearchTask *generationTask
	// sessionsTextTask owns the debounce and network reads for the current
	// filter query.
	sessionsTextTask context.CancelFunc
	wikiSyncTask     *generationTask
	sessionsSyncTask *generationTask
	usageReport      usageReportLoader

	dashboardIoTx        *core.Unbounded[dashboardIoUpdate]
	dashboardIo          *daemon.Feed[dashboardIoUpdate]
	webRequestGeneration uint64
	webRequestCancel     context.CancelFunc
	summaryReads         *summaryReads
	// materializedProjectionPermits is Rust's Semaphore of two permits for
	// background projections.
	materializedProjectionPermits   chan struct{}
	materializedProjectionsInFlight map[string]struct{}
	pendingMaterializedProjections  map[string]queuedMaterializedProjection
	projectSourcesInFlight          map[string]struct{}
	readReceipts                    readReceipts

	checkpointArchiveTargetsSeen map[string]string
	checkpointArchiveGeneration  uint64
	checkpointArchivePending     map[string]uint64
	checkpointArchiveRecords     core.SnapshotMap[string, string]
}

// materializedProjection is one worker publication, kept together while
// background projections coalesce.
type materializedProjection struct {
	materialized core.MaterializedSession
	assessment   *core.TurnAssessment
}

// materializedProjectionFromSession is Rust's `From<MaterializedSession>`.
func materializedProjectionFromSession(materialized core.MaterializedSession) materializedProjection {
	return materializedProjection{materialized: materialized}
}

// materializedProjectionFromSnapshot is Rust's
// `From<ManagedSessionSnapshot>`.
func materializedProjectionFromSnapshot(snapshot core.ManagedSessionSnapshot) materializedProjection {
	var assessment *core.TurnAssessment
	if operational := snapshot.Operational.Get(); operational != nil {
		assessment = operational.Assessment
	}
	return materializedProjection{materialized: snapshot.Materialized, assessment: assessment}
}

// queuedMaterializedProjection is a projection waiting behind the one in
// flight for its session, with the read frontier it was published at
// (Rust's `(MaterializedProjection, u64)`).
type queuedMaterializedProjection struct {
	projection                materializedProjection
	viewedThroughEventOrdinal uint64
}

// enqueueMaterializedProjection starts projection when its session has none
// in flight and hands it back; otherwise it replaces the queued one unless
// that one is newer, and answers nil.
func enqueueMaterializedProjection(inFlight map[string]struct{}, pending map[string]queuedMaterializedProjection, projection materializedProjection, viewedThroughEventOrdinal uint64) *queuedMaterializedProjection {
	sessionID := projection.materialized.SessionID
	if _, running := inFlight[sessionID]; running {
		queued, exists := pending[sessionID]
		if !exists || projection.materialized.AppliedEventOrdinal >= queued.projection.materialized.AppliedEventOrdinal {
			pending[sessionID] = queuedMaterializedProjection{projection: projection, viewedThroughEventOrdinal: viewedThroughEventOrdinal}
		}
		return nil
	}
	inFlight[sessionID] = struct{}{}
	return &queuedMaterializedProjection{projection: projection, viewedThroughEventOrdinal: viewedThroughEventOrdinal}
}

// launchRepositoryTopLevel is the top level of the Git worktree `mj` was
// started in, or nil outside one. The new-session wizard offers it as the
// local project. Blocking: run it off the loop.
func launchRepositoryTopLevel() *string {
	placement, err := core.DaemonConfigLoadPlacement(core.ConfigPath())
	if err != nil || placement.Runtime == core.DaemonRuntimeSsh {
		return nil
	}
	directory, err := os.Getwd()
	if err != nil {
		return nil
	}
	command := exec.Command("git", "rev-parse", "--show-toplevel")
	command.Dir = directory
	output, err := core.RunWithInput(command, nil)
	if err != nil || !output.Status.Success() {
		return nil
	}
	if !utf8.Valid(output.Stdout) {
		return nil
	}
	topLevel := strings.TrimSpace(string(output.Stdout))
	if topLevel == "" {
		return nil
	}
	return &topLevel
}

// dashboardGo is the fast-start mode a dashboard opens in, and whether it
// opens on its setup (Rust's `(GoMode, bool)`).
type dashboardGo struct {
	mode  tui.GoMode
	setup bool
}

// cloneConversationLayout is ConversationLayout's derived Clone: a copy
// that shares no map or node with the original.
func cloneConversationLayout(layout core.ConversationLayout) core.ConversationLayout {
	encoded, err := json.Marshal(layout)
	if err != nil {
		panic(fmt.Sprintf("encode a conversation layout: %v", err))
	}
	var cloned core.ConversationLayout
	if err := json.Unmarshal(encoded, &cloned); err != nil {
		panic(fmt.Sprintf("decode a conversation layout: %v", err))
	}
	return cloned
}

// conversationLayoutsEqual is ConversationLayout's derived PartialEq, which
// counts an empty map equal to a missing one as Rust's maps have no nil.
func conversationLayoutsEqual(a, b core.ConversationLayout) bool {
	left, errLeft := json.Marshal(a)
	right, errRight := json.Marshal(b)
	return errLeft == nil && errRight == nil && string(left) == string(right)
}

// loopArmResult is what one function-shaped wait of the loop produced:
// apply runs its arm on the loop, or is nil when the wait was cancelled
// before it received anything.
type loopArmResult struct {
	name  string
	apply func()
}

// loopArms runs the loop's waits that are functions rather than channels,
// one goroutine each, for one pass. The first to finish wakes the loop; the
// rest are cancelled and waited for before the loop touches its state
// again, and anything one had already received is kept by its apply. Rust's
// select! drops the losing futures, which are cancel safe; a goroutine
// cannot be dropped, so what it took is delivered instead of lost.
type loopArms struct {
	ctx     context.Context
	results chan loopArmResult
	running sync.WaitGroup
}

func newLoopArms(ctx context.Context) *loopArms {
	// One slot per arm the loop can start, so no finished wait blocks.
	return &loopArms{ctx: ctx, results: make(chan loopArmResult, 32)}
}

func (a *loopArms) start(name string, wait func(ctx context.Context) func()) {
	a.running.Add(1)
	go func() {
		defer a.running.Done()
		a.results <- loopArmResult{name: name, apply: wait(a.ctx)}
	}()
}

// finish waits for every arm to end and hands back what the ones that did
// not win received.
func (a *loopArms) finish() []loopArmResult {
	a.running.Wait()
	var rest []loopArmResult
	for {
		select {
		case result := <-a.results:
			if result.apply != nil {
				rest = append(rest, result)
			}
		default:
			return rest
		}
	}
}

// startFeedArm waits on feed, paced by gate, when it is open. stashed is
// whether an earlier pass already holds a message the drain has not taken:
// that feed is ready at once, as its channel would be in Rust, and its wait
// must not take a second message over the first.
func startFeedArm[T any](arms *loopArms, name string, gate *time.Time, feed *daemon.Feed[T], stashed map[string]bool) {
	if !feed.IsOpen() {
		return
	}
	if stashed[name] {
		arms.start(name, func(ctx context.Context) func() {
			if _, err := paced(ctx, gate, func(context.Context) struct{} { return struct{}{} }); err != nil {
				return nil
			}
			return func() {}
		})
		return
	}
	arms.start(name, func(ctx context.Context) func() {
		var message T
		var ok bool
		if _, err := paced(ctx, gate, func(ctx context.Context) struct{} {
			message, ok = feed.Wait(ctx)
			return struct{}{}
		}); err != nil {
			return nil
		}
		if !ok && ctx.Err() != nil {
			// Cancelled, not closed: nothing was taken.
			return nil
		}
		return func() {
			if feed.Accept(message, ok) {
				stashed[name] = true
			}
		}
	})
}

// runDashboardForWorkspace runs the dashboard over loaded until the person
// quits, detaches, or an upgraded daemon asks the terminal to restart.
func runDashboardForWorkspace(ctx context.Context, loaded loadedDashboard, screen *dashboardScreen, openWorkspaceManager bool, goStart *dashboardGo, daemonPresence *core.Watch[DaemonPresence], resume *upgradeResume, updateNotice controller.StartupUpdateNotice) (dashboardExit, error) {
	c, err := openDashboardContext(loaded, screen.terminal, screen.events, daemonPresence)
	if err != nil {
		return nil, err
	}
	termination := screen.termination
	c.splash = screen.splash
	automaticSetup := resume == nil && goStart == nil && !openWorkspaceManager
	if resume != nil {
		c.restoreUpgrade(ctx, *resume)
	}
	if automaticSetup {
		// First-run setup decides whether this is a new installation from
		// whether the configuration is empty, so nothing else may add to it
		// first.
		c.dashboard.HoldAvailabilityCheck()
		spawnFirstRunSetup(c.dashboardIoTx, c.criticalOperations)
	}
	var restartExecutable *UpgradeTarget
	if goStart == nil {
		launch, err := core.SpawnBlocking(launchRepositoryTopLevel).Join(ctx)
		if err != nil {
			return nil, fmt.Errorf("find the repository mj was started in: %w", err)
		}
		c.dashboard.SetLaunchProjectDirectory(launch)
	}
	if goStart != nil {
		type savedModes struct {
			modes []tui.GoMode
			err   error
		}
		saved, err := core.SpawnBlocking(func() savedModes {
			modes, err := savedWorkspaceModes()
			return savedModes{modes: modes, err: err}
		}).Join(ctx)
		if err != nil {
			return nil, fmt.Errorf("load project workspace settings task failed: %w", err)
		}
		if saved.err != nil {
			return nil, saved.err
		}
		c.dashboard.RegisterGoWorkspaces(saved.modes)
		c.cancelStartupSession()
		// The daemon holds every record, stopped ones included, so it
		// chooses the session to start in.
		var startup *core.SessionRecord
		if workspaceID, ok := c.dashboard.ActiveWorkspaceID(); ok {
			startup, err = goStartupRecord(ctx, c.dashboard, workspaceID, goStart.mode.LastSessionID)
			if err != nil {
				return nil, err
			}
		}
		action := c.dashboard.BeginGo(goStart.mode, goStart.setup, startup)
		if err := applyDashboardAction(ctx, c, action); err != nil {
			return nil, err
		}
	}
	if openWorkspaceManager {
		action := c.dashboard.BeginWorkspaceManager()
		if err := applyDashboardAction(ctx, c, action); err != nil {
			return nil, err
		}
	}
	// Two things a first launch may have to say, and inside tmux it has
	// both: that the multiplexer takes ctrl+b before the dashboard sees it,
	// and that the prefix leads to the key list and the command list. They
	// queue rather than write the notice bar here, because this launch may
	// still be opening a restored conversation and reporting on that; a hint
	// written over those reports would be recorded as shown without ever
	// being drawn.
	hints := newPendingHints(seenHintsLoad())
	prefix := c.dashboard.Keybinds().PrefixLabel()
	if multiplexer, ok := multiplexerName(); ok && prefix == core.DefaultPrefix {
		hints.queue(hintPrefixCollision, fmt.Sprintf(
			"%s is %s's prefix too: press it twice, or set [keys] prefix.", prefix, multiplexer))
	}
	hints.queue(hintPrefixKeys, fmt.Sprintf("Press %s ? for every key and %s : for every command.", prefix, prefix))
	// The first clock tick is a period away rather than immediate, and a
	// tick that was gated off does not fire a burst to catch up when it
	// comes back: a Ticker keeps at most one.
	clockTick := time.NewTicker(dashboardClockTick)
	defer clockTick.Stop()
	animationTicker := time.NewTicker(animationTick)
	defer animationTicker.Stop()
	// Only read while a held pointer sits at a scrollable surface's edge,
	// so an idle dashboard never acts on it.
	autoscrollTick := time.NewTicker(selectionAutoscrollTick)
	defer autoscrollTick.Stop()
	splashTick := time.NewTicker(splashFrame)
	defer splashTick.Stop()

	// A wakeup applies what it brought and marks the surface out of date;
	// the pacer decides when a frame shows it. The renderer writes only the
	// cells that differ, but building a frame costs the same whatever moved,
	// so background updates share one frame per interval while input is
	// drawn at once. The first frame is drawn before anything is awaited.
	pacer := newFramePacer(frameInterval)
	pacer.markUrgent()
	// The watches wake on a send after their channel was taken, so the
	// channels are taken once and renewed only after each wake.
	presenceChanged := c.daemonPresence.Changed()
	criticalChanged := c.criticalOperationsChanged.Changed()
	// stashed are the feeds whose message a losing wait took and the feed
	// holds for the next drain.
	stashed := map[string]bool{}
	// Startup awaited the daemon and restored state before this point; the
	// first pass starts here, so only the first frame's own work is counted.
	c.loopStalls.forgetPass()
loop:
	for {
		c.loopStalls.phase(loopStallsPhaseHousekeeping)
		c.updateSubagentScope()
		c.reconcileStoredSessionSummaries()
		if c.shutdownRequestedAt == nil {
			c.refreshControllerDerivedState()
			c.startSessionsTextSearch()
			c.reconcileWikiSyncs()
		}
		// The startup update notice waits for the same settled surface as a
		// hint, so its display time starts once it can be drawn rather than
		// behind the splash, and it goes first so the hints wait for it.
		if updateNotice != nil && c.readyForAHint() {
			switch notice := updateNotice.(type) {
			case controller.StartupUpdateNoticeAvailable:
				c.dashboard.SetNotice(notice.Text)
			case controller.StartupUpdateNoticeFailed:
				c.dashboard.SetFailureNotice(notice.Text)
			}
			updateNotice = nil
			pacer.markUrgent()
		}
		// A hint that reaches the bar needs the frame that draws it,
		// whatever this wakeup was otherwise going to do: the next pass
		// records it as shown.
		if hints.pump(&c.notices, c.readyForAHint(), time.Now()) {
			pacer.markUrgent()
		}
		c.loopStalls.phase(loopStallsPhaseDraw)
		started := time.Now()
		if pacer.shouldDraw(started) {
			if err := c.draw(); err != nil {
				return nil, err
			}
			pacer.drew(started, time.Now())
		}
		c.loopStalls.phase(loopStallsPhaseNotifications)
		// After the frame, so a bell never precedes the row it is about.
		if !pacer.pending() {
			if err := c.emitNotifications(); err != nil {
				return nil, err
			}
		}
		c.loopStalls.phase(loopStallsPhaseHousekeeping)
		now := time.Now()
		feedGate := pacer.feedGate(now)
		deferredFrame := pacer.deferredFrame(now)
		input := false
		var action tui.DashboardAction = tui.DashboardActionNone{}
		var chatOutcome chat.ChatEventOutcome = chat.ChatEventOutcomeNone{}
		// The winning arm takes the message that woke the loop; the drains
		// below batch whatever is queued behind it. The order is the
		// priority: input first, so a key never waits behind a feed, then
		// the timers, which fire at most once per period and so cannot
		// starve the feeds after them. Feeds are held until a paced frame
		// could show what they bring.
		c.loopStalls.selectStarted()
		waitCtx, cancelWaits := context.WithCancel(ctx)
		arms := newLoopArms(waitCtx)
		// The warm chat's own feeds: remote command results, its clipboard
		// and history I/O, dictation, and the session view. They run
		// whether or not the chat is on screen, which is what keeps an
		// off-screen chat current.
		login, chats := c.login, c.chats
		arms.start("conversation terminals", func(ctx context.Context) func() {
			var update loginTerminalUpdate
			var fromLogin bool
			var pumpErr error
			if _, err := paced(ctx, feedGate, func(ctx context.Context) struct{} {
				update, fromLogin, pumpErr = loginTerminalPump(ctx, login, chats)
				return struct{}{}
			}); err != nil || pumpErr != nil {
				return nil
			}
			return func() {
				pacer.mark()
				if fromLogin {
					c.acceptLoginUpdate(update)
				} else {
					// Only conversations visible in the frame advance
					// receipts.
					c.acknowledgeVisibleChats()
				}
			}
		})
		for _, persistence := range []struct {
			name    string
			running bool
			wait    func(context.Context)
		}{
			{"sidebar_size_persistence", c.sidebarSizePersistence.isRunning(), c.sidebarSizePersistence.wait},
			{"pane_size_persistence", c.paneSizePersistence.isRunning(), c.paneSizePersistence.wait},
			{"layout_persistence", c.layoutPersistence.isRunning(), c.layoutPersistence.wait},
		} {
			if !persistence.running {
				continue
			}
			arms.start(persistence.name, func(ctx context.Context) func() {
				persistence.wait(ctx)
				if ctx.Err() != nil {
					return nil
				}
				return pacer.mark
			})
		}
		startFeedArm(arms, "quota", feedGate, c.quota, stashed)
		startFeedArm(arms, "profile_capabilities", feedGate, c.profileCapabilities, stashed)
		startFeedArm(arms, "worker", feedGate, c.worker, stashed)
		startFeedArm(arms, "runtime_reviews", feedGate, c.runtimeReviews, stashed)
		startFeedArm(arms, "runtime_notices", feedGate, c.runtimeNotices, stashed)
		startFeedArm(arms, "runtime_alerts", feedGate, c.runtimeAlerts, stashed)
		startFeedArm(arms, "runtime_ui_requests", feedGate, c.runtimeUIRequests, stashed)
		startFeedArm(arms, "runtime_config", feedGate, c.runtimeConfig, stashed)
		startFeedArm(arms, "runtime_health", feedGate, c.runtimeHealth, stashed)
		startFeedArm(arms, "runtime_state", feedGate, c.runtimeState, stashed)
		startFeedArm(arms, "import_profiles", feedGate, c.importProfiles, stashed)
		startFeedArm(arms, "import_tasks", feedGate, c.importTasks, stashed)
		startFeedArm(arms, "lifecycle", feedGate, c.lifecycle, stashed)
		startFeedArm(arms, "dashboard_io", feedGate, c.dashboardIo, stashed)

		var terminationDone <-chan struct{}
		if c.shutdownRequestedAt == nil {
			terminationDone = termination.Done()
		}
		var autoscrollC <-chan time.Time
		if _, _, ok := c.autoscrollRequest(); ok {
			autoscrollC = autoscrollTick.C
		}
		var deferredC <-chan time.Time
		var deferredTimer *time.Timer
		if deferredFrame != nil {
			deferredTimer = time.NewTimer(time.Until(*deferredFrame))
			deferredC = deferredTimer.C
		}
		var splashC <-chan time.Time
		if c.splash != nil {
			splashC = splashTick.C
		}
		var animationC <-chan time.Time
		if c.needsAnimation() {
			animationC = animationTicker.C
		}
		var autoAfkErrors <-chan string
		if c.autoAfk != nil {
			autoAfkErrors = c.autoAfk.errors
		}
		var criticalC <-chan struct{}
		if c.shutdownRequestedAt != nil {
			criticalC = criticalChanged
		}
		const (
			wokeTermination = iota
			wokeInput
			wokeAutoscroll
			wokeDeferredFrame
			wokeSplash
			wokeClock
			wokeAnimation
			wokePresence
			wokeAutoAfk
			wokeCritical
			wokeArm
		)
		var woke int
		var event terminalEvent
		var eventOK bool
		var autoAfkError string
		var winner loopArmResult
		// biased: termination, then input, win over whatever else is ready.
		select {
		case <-terminationDone:
			woke = wokeTermination
		default:
			select {
			case event, eventOK = <-c.events:
				woke = wokeInput
			default:
				select {
				case <-terminationDone:
					woke = wokeTermination
				case event, eventOK = <-c.events:
					woke = wokeInput
				case <-autoscrollC:
					woke = wokeAutoscroll
				case <-deferredC:
					woke = wokeDeferredFrame
				case <-splashC:
					woke = wokeSplash
				case <-clockTick.C:
					woke = wokeClock
				case <-animationC:
					woke = wokeAnimation
				case <-presenceChanged:
					woke = wokePresence
				case autoAfkError = <-autoAfkErrors:
					woke = wokeAutoAfk
				case <-criticalC:
					woke = wokeCritical
				case winner = <-arms.results:
					woke = wokeArm
				}
			}
		}
		if deferredTimer != nil {
			deferredTimer.Stop()
		}
		cancelWaits()
		losers := arms.finish()
		if woke == wokeArm && winner.apply == nil {
			// The winning wait ended without a message; nothing woke it but
			// its cancellation, which cannot happen before this point.
			woke = -1
		}
		switch woke {
		case wokeTermination:
			c.loopStalls.woke("termination")
			c.beginShutdown(false)
			pacer.markUrgent()
		case wokeInput:
			c.loopStalls.woke("input")
			input = true
			pacer.markUrgent()
			if !eventOK {
				applyLoopArms(losers)
				break loop
			}
			if event.err != nil {
				return nil, event.err
			}
			if isOperatorInput(event.msg) {
				c.autoAfk.input()
			}
			// Nothing behind the splash has been seen, so nothing there
			// should act on input yet.
			if c.shutdownRequestedAt != nil || c.splash != nil {
				applyLoopArms(losers)
				continue loop
			}
			if c.login != nil {
				c.loginEvent(event.msg)
				applyLoopArms(losers)
				continue loop
			}
			if err := c.handleInputBatch(event.msg, &action, &chatOutcome); err != nil {
				return nil, err
			}
		case wokeAutoscroll:
			// A drag held past a scrollable surface's edge keeps scrolling
			// it and keeps extending the selection, the way a held pointer
			// does in a terminal's own selection.
			c.loopStalls.woke("autoscroll")
			input = true
			pacer.markUrgent()
			if err := c.applyAutoscroll(); err != nil {
				return nil, err
			}
		case wokeDeferredFrame:
			c.loopStalls.woke("deferred_frame")
		case wokeSplash:
			c.loopStalls.woke("splash")
			pacer.mark()
		case wokeClock:
			// Poll displayed time values without forcing a frame when none
			// of the visible clocks or countdowns changed.
			c.loopStalls.woke("clock")
			// Resume search covers the moving "Last active" text, so the
			// same clock that redraws the dialog rebuilds its rows.
			c.dashboard.RebuildResumeRows()
			redraw := c.clockTickRedraws()
			presence := c.daemonPresence.Borrow()
			redraw = reconcileDaemonFailure(c.dashboard, presence, &c.daemonRunningAgainSince, time.Now()) || redraw
			redraw = expireDaemonRunningAgain(c.notices, &c.daemonRunningAgainSince, time.Now()) || redraw
			// The startup pick fires at most once, and opening the
			// conversation it chooses has to reach the screen.
			redraw = c.maybeOpenStartupSession() || redraw
			if redraw {
				pacer.mark()
			}
		case wokeAnimation:
			// Input redraws never advance animations. Only visible activity
			// arms this timer; settled conversations keep the slow clock.
			c.loopStalls.woke("animation")
			changed := c.dashboard.AnimationChanged()
			if !changed {
				if visible := c.visibleChat(); visible != nil {
					changed = visible.AnimationChanged()
				}
			}
			if changed {
				pacer.mark()
			}
		case wokePresence:
			// The keep-alive no longer starts a daemon, so a daemon that is
			// gone has to be visible instead of silently replaced.
			c.loopStalls.woke("daemon_presence")
			presenceChanged = c.daemonPresence.Changed()
			pacer.mark()
			switch presence := c.daemonPresence.Borrow().(type) {
			case DaemonPresenceUpgraded:
				if c.shutdownRequestedAt == nil {
					target := presence.Target
					restartExecutable = &target
					c.requestShutdown()
				}
			case DaemonPresenceAttached:
				showDaemonReattached(c.dashboard)
				c.daemonRunningAgainSince = nil
				if notice, ok := c.dashboard.Notice(); ok && notice == daemonRunningAgainNotice {
					since := time.Now()
					c.daemonRunningAgainSince = &since
				}
			case DaemonPresenceMissing:
				// The reason is already in the log. The notice bar is one
				// line, so it carries the action instead.
				slog.Warn("the Mjolnir daemon is not running", "reason", presence.Reason)
				c.dashboard.SetFailureNotice(daemonUnavailableNotice)
			}
		case wokeAutoAfk:
			c.loopStalls.woke("auto_afk")
			c.dashboard.SetFailureNotice(autoAfkError)
			pacer.mark()
		case wokeCritical:
			c.loopStalls.woke("critical_operations")
			criticalChanged = c.criticalOperationsChanged.Changed()
			pacer.markUrgent()
		case wokeArm:
			c.loopStalls.woke(winner.name)
			winner.apply()
		}
		applyLoopArms(losers)
		c.loopStalls.phase(loopStallsPhaseDrainFeeds)
		if input {
			// The frame that shows this input is drawn before any queued
			// feed message is applied; the next wakeup drains them. Only the
			// bookkeeping that input itself can invalidate runs now.
			c.cancelStalePathInput()
		} else {
			drained := c.drainFeeds()
			clear(stashed)
			if drained {
				// A background message can be queued behind a timer that
				// won the select, and the drain applies it here. Its result
				// has to reach the screen even when the timer itself had
				// nothing to show.
				pacer.mark()
			}
		}
		c.loopStalls.phase(loopStallsPhasePersistence)
		if workspaceID, ok := c.dashboard.ActiveWorkspaceID(); ok {
			if _, known := c.knownWorkspaceLayouts[workspaceID]; known && c.dashboard.WorkspacePaneSizesModified(workspaceID) {
				c.paneSizePersistence.update(workspaceID, c.dashboard.PaneSizes())
			}
		}
		c.sidebarSizePersistence.update("global", c.dashboard.PaneSize(tui.SupportPaneSessions))
		c.saveActiveWorkspaceLayout()
		c.loopStalls.phase(loopStallsPhaseActions)
		if c.shutdownRequestedAt == nil {
			if err := c.runActions(ctx, chatOutcome, action); err != nil {
				return nil, err
			}
			c.loopStalls.phase(loopStallsPhaseHousekeeping)
			// The Sessions pane is a list of conversations, not a list of
			// things to go and open, so the transcript follows its selection.
			c.reconcileConversationAttachments()
			c.refreshGoContext()
			c.refreshGitStatus()
			c.usageReport.sync(c.dashboard, c.dashboardIoTx)
		}
		if c.wikiSearchTask != nil {
			if id, ok := c.dashboard.ResumeSearchRequestID(); !ok || id != c.wikiSearchTask.generation {
				c.wikiSearchTask.cancel()
				c.wikiSearchTask = nil
			}
		}
		c.captureComposerDrafts()
		c.rememberGoSelection()
		if c.shutdownRequestedAt != nil && c.refreshShutdownNotice() {
			break
		}
	}
	return c.finishDashboard(restartExecutable)
}

// applyLoopArms runs what the waits that lost the select had received.
func applyLoopArms(results []loopArmResult) {
	for _, result := range results {
		result.apply()
	}
}

// goStartupRecord asks the daemon which session a fast start opens on. A
// stopped one is read again with its verdict, which the dashboard keeps.
func goStartupRecord(ctx context.Context, dashboard *tui.DashboardState, workspaceID string, lastSessionID *string) (*core.SessionRecord, error) {
	daemonClient, err := ConnectOrStart(ctx)
	if err != nil {
		return nil, err
	}
	defer daemonClient.Close()
	startup, err := daemonClient.GoStartupSession(ctx, workspaceID, lastSessionID)
	if err != nil {
		return nil, err
	}
	if startup == nil || startup.State != core.SessionStateStopped {
		return startup, nil
	}
	current, err := daemonClient.SessionRecordWithVerdict(ctx, startup.ID)
	if err != nil {
		return nil, err
	}
	if current == nil {
		return nil, nil
	}
	record := current.Record
	dashboard.RememberStoppedRecord(*current)
	return &record, nil
}

// handleInputBatch dispatches one input event and the ones already queued
// behind it.
//
// Key repeats and pastes arrive as several ready events, and they coalesce
// into one frame; the first event that asks for work ends the batch so that
// dispatch still follows input order. Within the batch, only an event that
// is resolved against the frame needs that frame rebuilt first: a pointer is
// hit-tested against the controls the last frame registered, and a modal
// opened by the previous key registers its controls on the frame after it
// opens. An event that changed nothing leaves the frame current, so it
// needs no rebuild.
func (c *dashboardContext) handleInputBatch(event tea.Msg, action *tui.DashboardAction, chatOutcome *chat.ChatEventOutcome) error {
	// The user acting is the strongest signal there is about which
	// conversation they want, so it takes the choice away from the startup
	// pick before that pick can override their input.
	switch event.(type) {
	case tea.KeyPressMsg, tea.KeyReleaseMsg, tea.MouseMsg, tea.PasteMsg:
		c.cancelStartupSession()
	}
	previousConsumed := false
	for {
		_, isMouse := event.(tea.MouseMsg)
		if (previousConsumed && (isMouse || c.dashboard.ModalOpen())) || c.dashboard.NotificationInputNeedsFrame() {
			c.loopStalls.phase(loopStallsPhaseDraw)
			if err := c.draw(); err != nil {
				return err
			}
			c.loopStalls.phase(loopStallsPhaseArm)
		}
		if openingCancelEvent(event, c.focusedPaneIsOpening(), c.dashboard.ModalOpen(), c.dashboard.NotificationFormOwnsKeyboard()) {
			c.cancelChatOpen()
			c.dashboard.FocusSessions()
			c.dashboard.SetNotice("Session opening cancelled. Press Enter in Sessions to retry.")
			return nil
		}
		// The prefix router runs here and nowhere else: this loop forwards
		// the literal doubled prefix into the panes and the composer, and a
		// second pass down there would arm the prefix again on that very
		// key.
		route := c.dashboard.RouteBoundKeyEvent(event)
		if command, ok := route.(tui.KeyRouteCommand); ok && !c.dashboard.CommandAllowedNow(command.ID) {
			route = tui.KeyRouteConsumed{}
		}
		var consumed, batched bool
		switch routed := route.(type) {
		case tui.KeyRouteCommand:
			c.dashboard.CancelAllComponentPointers(c.chats)
			// Detaching from an open conversation has to save the draft and
			// the read cursor, which is the chat's own bookkeeping rather
			// than the dashboard's.
			visible := c.visibleChat()
			switch {
			case routed.ID == tui.CommandIdQuitDetach && visible != nil:
				*chatOutcome = visible.Detach()
			case routed.ID == tui.CommandIdSwitchWorkspace:
				*action = tui.DashboardActionNone{}
				if routed.Index != nil {
					*action = c.dashboard.SelectWorkspaceIndex(*routed.Index)
				}
			default:
				*action = c.dashboard.DispatchCommand(routed.ID)
			}
			consumed, batched = true, false
		case tui.KeyRouteConsumed:
			consumed, batched = true, false
		default:
			// Selection sees mouse and Esc first: a drag inside a pane
			// belongs to the engine, and everything else comes back out for
			// the view.
			switch selection := c.routeSelection(event).(type) {
			case selectionRoutingConsumed:
				consumed, batched = true, true
			case selectionRoutingCopy:
				if err := c.copySelection(selection.surface, selection.selectionRange); err != nil {
					return err
				}
				consumed, batched = true, true
			case selectionRoutingForward:
				consumed, batched = dispatchEvent(c, selection.event, action, chatOutcome)
			}
		}
		previousConsumed = consumed
		if !batched {
			return nil
		}
		// A zero timeout: only events already queued join the batch.
		select {
		case next, ok := <-c.events:
			if !ok {
				return nil
			}
			if next.err != nil {
				return next.err
			}
			event = next.msg
			if isOperatorInput(event) {
				c.autoAfk.input()
			}
		default:
			return nil
		}
	}
}

// runActions carries out what this pass's input and background results
// asked for, in the order Rust runs them.
func (c *dashboardContext) runActions(ctx context.Context, chatOutcome chat.ChatEventOutcome, action tui.DashboardAction) error {
	c.applyChatOutcome(chatOutcome)
	if err := applyDashboardAction(ctx, c, action); err != nil {
		return err
	}
	// A settled Git identity check lets the new session it held back
	// continue as though the user had just asked for it.
	if followup := c.dashboard.TakeGitIdentityFollowup(); followup != nil {
		if err := applyDashboardAction(ctx, c, followup); err != nil {
			return err
		}
	}
	// Requests an agent made for the person run as their keys would, after
	// the person's own input; one held behind a dialog runs once the dialog
	// has closed.
	requested := c.uiRequestActions
	c.uiRequestActions = nil
	for _, request := range requested {
		if err := applyDashboardAction(ctx, c, request); err != nil {
			return err
		}
	}
	for {
		followup := c.dashboard.TakeUIFollowup()
		if followup == nil {
			break
		}
		if err := applyDashboardAction(ctx, c, followup); err != nil {
			return err
		}
	}
	// Input or a background result can open an isolated creation review;
	// either way its prerequisite check starts here.
	if discovery := c.dashboard.TakeProjectDiscovery(); discovery != nil {
		if err := applyDashboardAction(ctx, c, discovery); err != nil {
			return err
		}
	}
	if check, ok := c.dashboard.TakePrerequisiteCheck(); ok {
		if err := applyDashboardAction(ctx, c, check); err != nil {
			return err
		}
	}
	// A wizard that just opened reads the stored project and mount history
	// again: this run's sessions may have added to it.
	if refresh, ok := c.dashboard.TakeMountHistoryRefresh(); ok {
		if err := applyDashboardAction(ctx, c, refresh); err != nil {
			return err
		}
	}
	// What is there is checked at startup and whenever a picker opens, one
	// check at a time.
	if check := c.dashboard.TakeAvailabilityCheck(); check != nil {
		if err := applyDashboardAction(ctx, c, check); err != nil {
			return err
		}
	}
	// Settings shows whether each saved profile has a daemon login. Request
	// status off the loop; this is the one place that starts it so every
	// surface that opens Settings is covered.
	if generation, ok := c.dashboard.TakeProfileLoginRefresh(); ok {
		spawnProfileLoginStatuses(generation, c.dashboardIoTx)
	}
	if generation, profileID, ok := c.dashboard.TakeSetupLogin(); ok {
		if err := applyDashboardAction(ctx, c, tui.DashboardActionSetupLogin{Generation: generation, ProfileID: profileID}); err != nil {
			return err
		}
	}
	return nil
}

// finishDashboard tears the run down once the loop has ended: it hands the
// terminal back, saves what is left within one budget, shuts the session
// manager down, and says how the run ended.
func (c *dashboardContext) finishDashboard(restartExecutable *UpgradeTarget) (dashboardExit, error) {
	loopEndedAt := time.Now()
	// Capture the feed-owned projection at the exit decision, before cleanup
	// can close feeds or change the dashboard's state.
	var detached detachedSessions = detachedSessionsUnknown{}
	switch c.daemonPresence.Borrow().(type) {
	case DaemonPresenceMissing:
		detached = detachedSessionsDaemonNotRunning{}
	case DaemonPresenceAttached:
		if c.runtimeState.IsOpen() && !c.runtimeState.SourceIsClosed() &&
			!c.runtimeHealth.SourceIsClosed() && c.runtimeHealth.Latest().RefreshError == nil {
			projection := c.runtimeState.Latest()
			roots := sessionRootsFromProjection(&projection.Records, &projection.Subagents)
			count := 0
			for _, session := range roots {
				if session.lifecycle.IsDashboardVisible() {
					count++
				}
			}
			detached = detachedSessionsKnown{count: count}
		}
	}
	autoAfkStartedAt := time.Now()
	c.autoAfk.shutdown()
	autoAfkFinishedAt := time.Now()
	c.cancelBackgroundWork()
	quitDetached := c.quitDetached
	// Hand the terminal back before saying anything on it; the warm chat
	// and the background feeds are torn down after.
	var resume *upgradeResume
	if restartExecutable != nil {
		captured := c.captureUpgrade()
		resume = &captured
	}
	c.terminal.close()
	for _, id := range sortedKeys(c.draftSaveFailures) {
		fmt.Fprintln(os.Stderr, c.draftSaveFailures[id])
	}
	for _, id := range sortedKeys(c.readReceipts.failures) {
		fmt.Fprintf(os.Stderr, "Could not save read status for %s: %s\n", sessionNoticeName(&c.controller.State, id), c.readReceipts.failures[id])
	}
	// All final persistence shares one budget; independent saves run
	// together.
	deadline := time.Now().Add(saveAckTimeout)
	workspaceID, hasWorkspace := c.dashboard.ActiveWorkspaceID()
	lastWorkspace := func(ctx context.Context) error {
		if !hasWorkspace {
			return nil
		}
		daemonClient, err := client.ConnectExisting(ctx)
		if err != nil {
			return err
		}
		defer daemonClient.Close()
		return daemonClient.TouchWorkspace(ctx, workspaceID)
	}
	saves := func() [4]error {
		var results [4]error
		var group sync.WaitGroup
		for index, save := range []func() error{
			func() error {
				return finishShutdownSave("sidebar size", deadline, func(context.Context) error { return c.sidebarSizePersistence.finish() })
			},
			func() error {
				return finishShutdownSave("workspace pane sizes", deadline, func(context.Context) error { return c.paneSizePersistence.finish() })
			},
			func() error {
				return finishShutdownSave("workspace layout", deadline, func(context.Context) error { return c.layoutPersistence.finish() })
			},
			func() error { return finishShutdownSave("last workspace", deadline, lastWorkspace) },
		} {
			group.Add(1)
			go func() {
				defer group.Done()
				results[index] = save()
			}()
		}
		group.Wait()
		return results
	}
	manager := func() error {
		shutdown := c.workerShutdown
		c.workerShutdown = nil
		if shutdown == nil {
			return nil
		}
		if err := shutdown.Shutdown(context.Background()); err != nil {
			return fmt.Errorf("shut down dashboard session manager: %w", err)
		}
		return nil
	}
	savesDone, managerDone := overlapFinalCleanup(saves, manager)
	for _, err := range savesDone.output {
		if err != nil {
			slog.Warn("dashboard final persistence was not confirmed", "error", err)
			fmt.Fprintln(os.Stderr, err)
		}
	}
	isRestart := restartExecutable != nil && resume != nil
	var exit dashboardExit
	switch {
	case isRestart:
		exit = dashboardExitRestart{target: *restartExecutable, resume: resume}
	case quitDetached:
		exit = dashboardExitDetached{sessions: detached}
	case c.shutdownRequestedAt != nil:
		exit = dashboardExitInterrupted{}
	default:
		exit = dashboardExitNormal{}
	}
	dashboardDropStartedAt := time.Now()
	lastQuitTimings.mu.Lock()
	lastQuitTimings.timings = &quitTimings{
		requestedAt:              c.shutdownRequestedAt,
		loopEndedAt:              loopEndedAt,
		autoAfkStartedAt:         autoAfkStartedAt,
		autoAfkFinishedAt:        autoAfkFinishedAt,
		savesStartedAt:           savesDone.startedAt,
		savesFinishedAt:          savesDone.finishedAt,
		sessionManagerStartedAt:  managerDone.startedAt,
		sessionManagerFinishedAt: managerDone.finishedAt,
		dashboardDropStartedAt:   dashboardDropStartedAt,
		dashboardDropFinishedAt:  dashboardDropStartedAt,
		returnedAt:               dashboardDropStartedAt,
	}
	lastQuitTimings.mu.Unlock()
	if err := managerDone.output; err != nil {
		if !isRestart {
			return nil, err
		}
		slog.Warn("old terminal session manager failed during upgrade handoff", "error", err)
	}
	return exit, nil
}

func (c *dashboardContext) cancelSessionPreflight() {
	if preflight := c.sessionPreflightCancel; preflight != nil {
		c.sessionPreflightCancel = nil
		preflight.cancelled.Store(true)
	}
}

// focusedChat is the conversation the focused pane holds, whether or not it
// is on screen (Rust's focused_chat and focused_chat_mut). Per-pane
// bookkeeping uses this; the keyboard and the drawn conversation use
// visibleChat, which also answers for the standby composers that stand in
// front of a session.
func (c *dashboardContext) focusedChat() *chat.ActiveChat {
	sessionID, ok := c.dashboard.CurrentSessionID()
	if !ok {
		return nil
	}
	return c.chats[sessionID]
}

// focusedPaneIsOpening reports whether the focused pane is still waiting
// for an attach, which is what Escape cancels.
func (c *dashboardContext) focusedPaneIsOpening() bool {
	_, opening := c.openingChatSessions[c.dashboard.FocusedPane()]
	return opening
}

// syncOpeningSession keeps the dashboard's single in-flight-attach report on
// the focused pane, which is the pane whose empty conversation is drawn.
func (c *dashboardContext) syncOpeningSession() {
	var opening *string
	if session, ok := c.openingChatSessions[c.dashboard.FocusedPane()]; ok {
		opening = &session
	}
	c.dashboard.SetOpeningSession(opening)
}

// cancelChatOpenFor stops the session's shared attachment, whichever
// surfaces requested it.
func (c *dashboardContext) cancelChatOpenFor(sessionID string) {
	if attachment := c.attachments[sessionID]; attachment != nil {
		attachment.deferAttach()
	}
	for pane, session := range c.openingChatSessions {
		if session == sessionID {
			delete(c.openingChatSessions, pane)
		}
	}
	c.syncOpeningSession()
}

func (c *dashboardContext) deferAllChatOpens() {
	for _, id := range sortedKeys(c.attachments) {
		c.attachments[id].deferAttach()
	}
	clear(c.openingChatSessions)
	c.dashboard.SetOpeningSession(nil)
}

// readyForAHint reports whether the surface has settled enough to give the
// notice bar to a one-time hint: the startup pick has run, no pane is still
// attaching, and nothing is covering the footer. Until then the bar belongs
// to what the launch itself is doing.
func (c *dashboardContext) readyForAHint() bool {
	return c.splash == nil &&
		c.login == nil &&
		!c.startup.pickPending() &&
		len(c.openingChatSessions) == 0 &&
		!c.dashboard.ModalOpen() &&
		c.shutdownRequestedAt == nil
}

// recordChatDetach persists how far a warm chat has been read and the draft
// it holds.
func (c *dashboardContext) recordChatDetach(sessionID string) {
	active := c.chats[sessionID]
	if active == nil {
		return
	}
	c.transcriptPositions[sessionID] = active.TranscriptPosition()
	c.recordDetach(sessionID, active.LatestEventOrdinal())
}

// parkChatComposer keeps pending input jobs alive when navigation drops a
// worker-facing view.
func (c *dashboardContext) parkChatComposer(sessionID string) {
	if c.dashboard.HasStandbyComposer(sessionID) {
		return
	}
	if active := c.chats[sessionID]; active != nil {
		if composer := active.TakeLiveComposer(); composer != nil {
			c.dashboard.AdoptStandbyComposer(sessionID, composer)
		}
	}
}

// retireChatsOutsideTheLayout saves and retires chats when neither the
// layout nor a form card holds them.
func (c *dashboardContext) retireChatsOutsideTheLayout() {
	shown := c.wantedChatSessions()
	for _, sessionID := range sortedKeys(c.chats) {
		if _, kept := shown[sessionID]; kept {
			continue
		}
		c.parkChatComposer(sessionID)
		c.saveQuestionDraft(sessionID)
		c.recordChatDetach(sessionID)
		delete(c.chats, sessionID)
	}
}

func (c *dashboardContext) openSessionInSplit(sessionID string, direction chat.Direction) {
	if _, pinned := c.dashboard.PinID(sessionID); pinned {
		c.openChatSession(sessionID)
		return
	}
	if !c.controller.State.Sessions.ContainsKey(sessionID) && !c.dashboard.IsNativeAgent(sessionID) {
		return
	}
	browse := c.dashboard.BrowsePane()
	newPane, ok := c.dashboard.SplitConversationPane(browse, direction)
	if !ok {
		return
	}
	c.pinSessionIn(sessionID, browse)
	c.dashboard.FocusPane(newPane)
	c.dashboard.FocusSessions()
	c.syncOpeningSession()
	c.saveActiveWorkspaceLayout()
}

func (c *dashboardContext) splitEmptyPane(direction chat.Direction) {
	c.splitConversationPane(c.dashboard.FocusedPane(), direction)
}

func (c *dashboardContext) splitConversationPane(target tui.PaneId, direction chat.Direction) {
	if _, ok := c.dashboard.SplitConversationPane(target, direction); !ok {
		return
	}
	c.selection.Clear()
	c.syncOpeningSession()
	c.saveActiveWorkspaceLayout()
}

func (c *dashboardContext) pinSessionIn(sessionID string, pane tui.PaneId) {
	if pane == c.dashboard.BrowsePane() || !c.dashboard.HasConversationPane(pane) {
		return
	}
	if !c.controller.State.Sessions.ContainsKey(sessionID) && !c.dashboard.IsNativeAgent(sessionID) {
		return
	}
	if source, ok := c.dashboard.PaneForSession(sessionID); ok {
		if source == pane {
			return
		}
		c.cancelChatOpenIn(source)
	}
	c.openChatSessionInto(pane, sessionID)
	c.syncOpeningSession()
	c.saveActiveWorkspaceLayout()
}

func (c *dashboardContext) unpinSession(sessionID string) {
	pane, ok := c.dashboard.PaneForSession(sessionID)
	if !ok || pane == c.dashboard.BrowsePane() {
		return
	}
	c.captureComposerDraft(sessionID)
	c.saveQuestionDraft(sessionID)
	c.cancelChatOpenIn(pane)
	c.dashboard.SetPaneSession(pane, nil)
	c.retireChatsOutsideTheLayout()
	c.saveActiveWorkspaceLayout()
}

// focusConversationPane moves the keyboard into one conversation pane and
// its composer. The arrangement is saved, because which pane has the focus
// is part of it.
func (c *dashboardContext) focusConversationPane(pane tui.PaneId) {
	if c.dashboard.FocusedPane() != pane {
		c.dashboard.FocusPane(pane)
		c.selection.Clear()
		c.syncOpeningSession()
		c.saveActiveWorkspaceLayout()
	}
	c.dashboard.FocusPrompt()
}

// closePane removes a pinned or empty pane while preserving its
// conversation state.
func (c *dashboardContext) closePane(pane tui.PaneId) {
	if pane == c.dashboard.BrowsePane() {
		c.dashboard.SetNotice("Browse must remain; move it with Swap pane.")
		return
	}
	if sessionID, ok := c.dashboard.PaneSession(pane); ok {
		c.captureComposerDraft(sessionID)
		c.saveQuestionDraft(sessionID)
	}
	delete(c.openingChatSessions, pane)
	c.cancelChatOpenIn(pane)
	if _, closed := c.dashboard.ClosePane(pane); closed {
		c.retireChatsOutsideTheLayout()
		c.selection.Clear()
	}
	c.syncOpeningSession()
	c.saveActiveWorkspaceLayout()
}

// saveActiveWorkspaceLayout queues the active workspace's arrangement for
// saving once this client has changed it.
func (c *dashboardContext) saveActiveWorkspaceLayout() {
	workspaceID, ok := c.dashboard.ActiveWorkspaceID()
	if !ok {
		return
	}
	if _, known := c.knownWorkspaceLayouts[workspaceID]; !known || !c.dashboard.WorkspaceLayoutModified(workspaceID) {
		return
	}
	c.setWorkspaceLayout(workspaceID, c.dashboard.ConversationLayoutFor(workspaceID))
}

// setWorkspaceLayout records a workspace's conversation pane arrangement
// and queues its save.
func (c *dashboardContext) setWorkspaceLayout(workspaceID string, layout core.ConversationLayout) {
	if current, ok := c.workspaceLayouts[workspaceID]; ok && conversationLayoutsEqual(current, layout) {
		return
	}
	c.workspaceLayouts[workspaceID] = cloneConversationLayout(layout)
	c.layoutPersistence.update(workspaceID, layout)
}

func (c *dashboardContext) requestShutdown() { c.beginShutdown(true) }

// finishSessionsStoppedBySuspend lets go of the conversations of
// sub-agents that their parent's suspend stopped (R15-1). Their records are
// gone, so there is no draft or read position to save, and trying would
// only report an unknown session. The dashboard has already put the view
// back on the parent, whose conversation opens in their place.
func (c *dashboardContext) finishSessionsStoppedBySuspend() {
	stopped := c.dashboard.TakeStoppedBySuspend()
	for _, sessionID := range stopped.Sessions {
		c.cancelChatOpenFor(sessionID)
		discardSessionComposer(c.dashboard, c.chats, &c.composerDrafts, sessionID)
		delete(c.transcriptPositions, sessionID)
		delete(c.questionDrafts, sessionID)
	}
	if stopped.Reopen != nil {
		c.openChatSession(*stopped.Reopen)
	}
}

// discardWorkspaceComposers: a removed workspace's composers must not be
// saved back on tab switch.
func (c *dashboardContext) discardWorkspaceComposers(workspaceID string) {
	var sessions []string
	for id, session := range c.controller.State.Sessions.All() {
		if session.WorkspaceID == workspaceID {
			sessions = append(sessions, id)
		}
	}
	for _, sessionID := range sessions {
		c.cancelChatOpenFor(sessionID)
		discardSessionComposer(c.dashboard, c.chats, &c.composerDrafts, sessionID)
		delete(c.questionDrafts, sessionID)
	}
}

func (c *dashboardContext) selectWorkspace(workspaceID *string) {
	active, hasActive := c.dashboard.ActiveWorkspaceID()
	if hasActive == (workspaceID != nil) && (!hasActive || active == *workspaceID) {
		return
	}
	if hasActive {
		if _, known := c.knownWorkspaceLayouts[active]; known {
			if c.dashboard.WorkspacePaneSizesModified(active) {
				c.paneSizePersistence.update(active, c.dashboard.PaneSizes())
			}
			if c.dashboard.WorkspaceLayoutModified(active) {
				c.setWorkspaceLayout(active, c.dashboard.ConversationLayoutFor(active))
			}
		}
	}
	c.acknowledgeVisibleChats()
	for _, sessionID := range sortedKeys(c.chats) {
		c.captureComposerDraft(sessionID)
		c.saveQuestionDraft(sessionID)
	}
	c.cancelStartupSession()
	c.deferAllChatOpens()
	c.workspaceID = ""
	if workspaceID != nil {
		c.workspaceID = *workspaceID
	}
	c.selection.Clear()
	c.dashboard.SetActiveWorkspace(workspaceID)
	c.goSelectionRequested = nil
	// The tab switch swapped in the other workspace's arrangement, so the
	// conversations the previous one held are no longer in any pane.
	c.retireChatsOutsideTheLayout()
	for _, entry := range c.dashboard.PaneSessions() {
		c.openChatSessionInto(entry.Pane, entry.SessionID)
	}
	if session, ok := c.dashboard.TakeSessionToOpen(); ok {
		c.openChatSession(session)
		c.dashboard.FocusPrompt()
	}
	c.reconcileConversationAttachments()
}

// chatSessionRecord is the record a chat's bookkeeping reads: the live one,
// or the last one of a session that has just left the feed. The pointer is
// into the context's own state.
func (c *dashboardContext) chatSessionRecord(sessionID string) *core.SessionRecord {
	if session := c.controller.State.Sessions.GetMut(sessionID); session != nil {
		return session
	}
	if departed, ok := c.departedRecords[sessionID]; ok {
		return &departed
	}
	return nil
}

func (c *dashboardContext) sessionInActiveWorkspace(sessionID string) bool {
	session := c.dashboard.SessionRecord(sessionID)
	if session == nil {
		return false
	}
	active, ok := c.dashboard.ActiveWorkspaceID()
	return ok && session.WorkspaceID == active
}

func (c *dashboardContext) beginShutdown(detached bool) {
	if c.shutdownRequestedAt != nil {
		return
	}
	// Closing this disposable stream never waits for the login process.
	if login := c.login; login != nil {
		c.login = nil
		if login.job != nil {
			login.job()
		}
	}
	// Timed from here so saving the drafts below counts toward the quit.
	requestedAt := time.Now()
	c.collectPreparedStartupPrompts()
	c.startupPrompts.cancelUnadmitted(c.dashboard, c.chats)
	c.startupPrompts.retainUnconfirmed(c.dashboard, c.chats)
	for _, sessionID := range c.dashboard.StandbySessionIDs() {
		if _, warm := c.chats[sessionID]; warm {
			continue
		}
		var viewed uint64
		if session, ok := c.controller.State.Sessions.Get(sessionID); ok {
			viewed = session.ViewedThroughEventOrdinal
		}
		c.recordDetach(sessionID, viewed)
	}
	// A warm chat may be hidden while another session opens. Every shutdown
	// path must save every one of them, including global quit and workspace
	// switching, before the process-local composer cache goes away.
	for _, sessionID := range sortedKeys(c.chats) {
		c.recordChatDetach(sessionID)
	}
	for _, sessionID := range c.composerDrafts.sessionIDs() {
		c.flushComposerDraft(sessionID, true)
	}
	c.shutdownRequestedAt = &requestedAt
	// Shutdown progress belongs on screen, not behind the splash.
	c.splash = nil
	for _, retry := range c.readReceipts.retryFailed() {
		c.spawnReadReceipt(retry.sessionID, retry.through)
	}
	if c.webRequestCancel != nil {
		c.webRequestCancel()
		c.webRequestCancel = nil
	}
	c.webRequestGeneration++
	c.quitDetached = detached
	c.cancelBackgroundWork()
	c.refreshShutdownNotice()
}

// refreshShutdownNotice returns true once every user-authored mutation has
// reached a durable boundary. Pure reads and projections are deliberately
// not blockers.
func (c *dashboardContext) refreshShutdownNotice() bool {
	if c.composerDrafts.isSaving() {
		c.dashboard.SetNotice("Saving drafts…")
		return false
	}
	if c.readReceipts.isSaving() {
		c.dashboard.SetNotice("Saving read status…")
		return false
	}
	if notice := shutdownWaitNotice(c.criticalOperations.Blockers()); notice != nil {
		c.dashboard.SetNotice(*notice)
		return false
	}
	return true
}

// acknowledgeVisibleChats advances the read receipt of every conversation
// that was on screen. The frame records which those were; before the first
// frame it is the one the focused pane would draw.
func (c *dashboardContext) acknowledgeVisibleChats() {
	// Freeze the final receipt set so ongoing worker output cannot keep
	// extending shutdown while its existing saves drain.
	if c.shutdownRequestedAt != nil || c.login != nil {
		return
	}
	sessions := append([]string(nil), c.drawnChatSessions...)
	if visible := c.visibleChat(); visible != nil {
		sessionID := visible.SessionID()
		found := false
		for _, drawn := range sessions {
			if drawn == sessionID {
				found = true
				break
			}
		}
		if !found {
			sessions = append(sessions, sessionID)
		}
	}
	for _, sessionID := range sessions {
		c.acknowledgeChat(sessionID)
	}
}

func (c *dashboardContext) acknowledgeChat(sessionID string) {
	active := c.chats[sessionID]
	if active == nil {
		return
	}
	through := active.LatestEventOrdinal()
	session := c.controller.State.Sessions.GetMut(sessionID)
	if session == nil || through <= session.ViewedThroughEventOrdinal {
		return
	}
	session.ViewedThroughEventOrdinal = through
	c.dashboard.SetState(c.controller.State)
	c.reconcileQuestionDrafts()
	if through, ok := c.readReceipts.acknowledge(sessionID, through); ok {
		c.spawnReadReceipt(sessionID, through)
	}
}

func (c *dashboardContext) acknowledgeDashboardSessions(receipts []sessionThrough) {
	for _, receipt := range receipts {
		session := c.controller.State.Sessions.GetMut(receipt.sessionID)
		if session == nil {
			continue
		}
		session.ViewedThroughEventOrdinal = max(session.ViewedThroughEventOrdinal, receipt.through)
		if through, ok := c.readReceipts.acknowledge(receipt.sessionID, receipt.through); ok {
			c.spawnReadReceipt(receipt.sessionID, through)
		}
	}
	c.dashboard.SetState(c.controller.State)
	c.reconcileQuestionDrafts()
}

func (c *dashboardContext) spawnReadReceipt(sessionID string, through uint64) {
	var retryDelay time.Duration
	if _, failed := c.readReceipts.failures[sessionID]; failed {
		retryDelay = 250 * time.Millisecond
	}
	sessionName := c.sessionNoticeName(sessionID)
	workspaceID := c.workspaceID
	if record := c.chatSessionRecord(sessionID); record != nil {
		workspaceID = record.WorkspaceID
	}
	spawnReadReceiptPersist(
		c.clientID,
		workspaceID,
		namedSession{id: sessionID, name: sessionName},
		through,
		retryDelay,
		c.dashboardIoTx,
		c.criticalOperations,
	)
}

// finishReadReceipt records a read receipt's save: through on success, the
// failure otherwise (Rust's Result<u64, String>).
func (c *dashboardContext) finishReadReceipt(sessionID string, through uint64, failure error) {
	next, retry := c.readReceipts.complete(sessionID, through, failure)
	if failed, ok := c.readReceipts.failures[sessionID]; ok {
		c.dashboard.SetNotice(fmt.Sprintf("Could not save read status for %s: %s", c.sessionNoticeName(sessionID), failed))
	}
	if retry {
		c.spawnReadReceipt(sessionID, next)
	}
}

// openDashboardContext takes the terminal and starts every background feed
// over the state loaded before it (DashboardContext::open).
func openDashboardContext(loaded loadedDashboard, terminal *terminalGuard, events terminalEvents, daemonPresence *core.Watch[DaemonPresence]) (*dashboardContext, error) {
	workspaceNames := make(map[string]string, len(loaded.workspaces))
	for _, workspace := range loaded.workspaces {
		workspaceNames[workspace.ID] = workspace.Name
	}
	ctrl := loaded.controller
	bootstrap := loaded.bootstrap
	dashboard := tui.NewDashboardState(ctrl.Config, ctrl.State, map[string]client.ProfileQuota{})
	dashboard.SetWorkspaceNames(workspaceNames)
	dashboard.SetLaunchRecency(loaded.launchRecency)
	dashboard.SetSessionAttention(bootstrap.Projection.Metadata.SessionAttention)
	dashboard.SetFormHolders(bootstrap.Projection.Metadata.FormHolders)
	dashboard.SetUnreadFrontiers(bootstrap.Projection.Metadata.UnreadFrontiers)
	dashboard.SetNotificationDismissals(bootstrap.Projection.Metadata.NotificationDismissals)
	// Sessions inherit this host's Git author identity; the first new
	// session of the run makes sure there is one.
	dashboard.RequireGitIdentityCheck()
	for _, id := range sortedKeys(loaded.layouts) {
		dashboard.CacheWorkspacePaneSizes(id, loaded.layouts[id])
	}
	for _, id := range sortedKeys(loaded.conversationLayouts) {
		dashboard.CacheWorkspaceLayout(id, cloneConversationLayout(loaded.conversationLayouts[id]))
	}
	workspaceID := loaded.workspaceID
	dashboard.SetActiveWorkspace(&workspaceID)
	notices := chat.NewNotices()
	dashboard.ShareNotices(notices)
	for _, sessionID := range sortedKeys(loaded.queuedPrompts) {
		dashboard.ApplyQueuedPrompts(sessionID, loaded.queuedPrompts[sessionID])
	}

	remoteWorker, err := daemon.SpawnRemoteDashboardWorkerPoller(workspaceID, bootstrap)
	if err != nil {
		return nil, err
	}
	runtimeStateSource := daemon.NewWatchSource(remoteWorker.State)
	// Apply bootstrap lifecycle and resource views even when the first
	// delta contains no changes and therefore causes no watch wakeup.
	runtimeStateSource.MarkChanged()
	initialState := remoteWorker.State.Borrow()
	dashboard.SetSessionVerdicts(initialState.Verdicts)
	dashboard.SetMoveSnapshot(initialState.Moves)
	dashboard.SetRecentLaunch(initialState.RecentLaunch)
	dashboard.SetContainerCapacities(initialState.ContainerCapacities)
	runtimeReviewViews := map[string][]client.RuntimeReviewView{}
	var reviews []client.RuntimeReviewView
	for _, review := range remoteWorker.Reviews.Borrow() {
		runtimeReviewViews[review.SessionID] = append(runtimeReviewViews[review.SessionID], review)
	}
	for _, sessionID := range sortedKeys(runtimeReviewViews) {
		reviews = append(reviews, runtimeReviewViews[sessionID]...)
	}
	dashboard.SetSessionReviews(reviews)
	// Notices already queued when this surface attaches belong to whatever
	// was on screen before it, so start after them rather than replaying
	// them into the notice bar.
	var reportedNoticeID *uint64
	for _, notice := range remoteWorker.Notices.Borrow() {
		if reportedNoticeID == nil || notice.ID > *reportedNoticeID {
			id := notice.ID
			reportedNoticeID = &id
		}
	}
	// Like notices, requests already published when this surface attaches
	// were meant for whatever was on screen before it.
	uiRequestCursor := client.UiRequestCursorAfter(remoteWorker.UIRequests.Borrow())
	dashboard.SetProfileCapabilities(remoteWorker.ProfileCapabilities.Borrow())
	dashboard.SetQuotaSnapshot(remoteWorker.Quotas.Borrow())
	runtimeLaunch := remoteWorker.Config.Borrow()
	dashboard.SetRuntimeConfig(runtimeLaunch.Config, runtimeLaunch.Availability)
	lifecycleUpdates := core.NewUnbounded[dashboardLifecycleUpdate]()
	criticalOperations, criticalOperationsChanged := client.NewCriticalOperationTracker()
	importUpdates := make(chan importProfileUpdate, 32)
	importTasks := make(chan dashboardImportUpdate, 8)
	dashboardIo := core.NewUnbounded[dashboardIoUpdate]()
	knownWorkspaceLayouts := make(map[string]struct{}, len(loaded.layouts))
	for id := range loaded.layouts {
		knownWorkspaceLayouts[id] = struct{}{}
	}
	workspaceLayouts := loaded.conversationLayouts
	if workspaceLayouts == nil {
		workspaceLayouts = map[string]core.ConversationLayout{}
	}
	sidebarSizePersistence := startSidebarSizePersistence(ctrl.Config.SidebarSize, notices)
	paneSizePersistence := startPaneSizesPersistence(loaded.layouts, notices)
	layoutPersistence := startLayoutsPersistence(loaded.conversationLayouts, notices)
	autoAfkEnabled := ctrl.Config.Advanced.AutoAfkMinutes > 0
	autoAfkAway := ctrl.State.OperatorPresence.Away

	c := &dashboardContext{
		terminal:                        terminal,
		controller:                      ctrl,
		workspaceID:                     workspaceID,
		clientID:                        loaded.clientID,
		dashboard:                       dashboard,
		paneSizePersistence:             paneSizePersistence,
		sidebarSizePersistence:          sidebarSizePersistence,
		layoutPersistence:               layoutPersistence,
		knownWorkspaceLayouts:           knownWorkspaceLayouts,
		workspaceLayouts:                workspaceLayouts,
		notices:                         notices,
		events:                          events,
		chats:                           map[string]*chat.ActiveChat{},
		departedRecords:                 map[string]core.SessionRecord{},
		questionDrafts:                  map[string][]cachedQuestionDraft{},
		transcriptPositions:             map[string]chat.TranscriptPosition{},
		questionTargetsSeen:             map[string]map[string]struct{}{},
		composerDaemonRequests:          map[string]chat.ChatDaemonSender{},
		draftSaveFailures:               map[string]string{},
		openingChatSessions:             map[tui.PaneId]string{},
		attachments:                     map[string]*sessionAttachment{},
		startup:                         idleStartupSession(),
		gitProbesInFlight:               map[string]struct{}{},
		controllerChanged:               true,
		criticalOperations:              criticalOperations,
		criticalOperationsChanged:       criticalOperationsChanged,
		loopStalls:                      newLoopStallsMeter(),
		daemonPresence:                  daemonPresence,
		autoAfk:                         newAutoAfkReporter(autoAfkEnabled, autoAfkAway),
		quota:                           daemon.NewFeed[client.QuotaSnapshot](daemon.NewWatchSource(remoteWorker.Quotas)),
		worker:                          daemon.NewFeed[sessionruntime.SessionManagerUpdate](daemon.NewSessionManagerSource(remoteWorker.Updates)),
		runtimeState:                    daemon.NewFeed[daemon.RuntimeStateUpdate](runtimeStateSource),
		runtimeAlerts:                   daemon.NewFeed[daemon.AlertBatch](daemon.NewUnboundedSource(remoteWorker.Alerts)),
		runtimeSubagentParents:          remoteWorker.SubagentParents,
		runtimeHealth:                   daemon.NewFeed[daemon.RuntimeFeedHealth](daemon.NewWatchSource(remoteWorker.Health)),
		runtimeReviews:                  daemon.NewFeed[[]client.RuntimeReviewView](daemon.NewWatchSource(remoteWorker.Reviews)),
		runtimeNotices:                  daemon.NewFeed[[]client.RuntimeNotice](daemon.NewWatchSource(remoteWorker.Notices)),
		reportedNoticeID:                reportedNoticeID,
		runtimeUIRequests:               daemon.NewFeed[[]client.UiRequest](daemon.NewWatchSource(remoteWorker.UIRequests)),
		uiRequestCursor:                 uiRequestCursor,
		runtimeReviewViews:              runtimeReviewViews,
		runtimeConfig:                   daemon.NewFeed[daemon.RuntimeLaunchConfig](daemon.NewWatchSource(remoteWorker.Config)),
		profileCapabilities:             daemon.NewFeed[core.ProfileCapabilitiesSnapshot](daemon.NewWatchSource(remoteWorker.ProfileCapabilities)),
		remoteLifecycleOperations:       map[string]remoteLifecycleObservation{},
		workerCommandsTx:                remoteWorker.Control,
		workerShutdown:                  remoteWorker.Shutdown,
		lifecycleUpdatesTx:              lifecycleUpdates,
		lifecycle:                       daemon.NewFeed[dashboardLifecycleUpdate](daemon.NewUnboundedSource(lifecycleUpdates)),
		lifecycleOperations:             map[string]*activeLifecycleOperation{},
		importUpdatesTx:                 importUpdates,
		importProfiles:                  daemon.NewFeed[importProfileUpdate](daemon.NewChannelSource[importProfileUpdate](importUpdates)),
		importTaskTx:                    importTasks,
		importTasks:                     daemon.NewFeed[dashboardImportUpdate](daemon.NewChannelSource[dashboardImportUpdate](importTasks)),
		nativeScanCache:                 mjimport.NewNativeScanCache(),
		selection:                       chat.NewSelectionState(),
		dashboardIoTx:                   dashboardIo,
		dashboardIo:                     daemon.NewFeed[dashboardIoUpdate](daemon.NewUnboundedSource(dashboardIo)),
		summaryReads:                    newSummaryReads(),
		materializedProjectionPermits:   make(chan struct{}, 2),
		materializedProjectionsInFlight: map[string]struct{}{},
		pendingMaterializedProjections:  map[string]queuedMaterializedProjection{},
		projectSourcesInFlight:          map[string]struct{}{},
		readReceipts:                    readReceipts{sessions: map[string]*receipt{}, failures: map[string]string{}},
		checkpointArchiveTargetsSeen:    map[string]string{},
		checkpointArchivePending:        map[string]uint64{},
	}
	c.resolveProjectSources()
	c.hydrateStoredSessionSummaries()
	return c, nil
}

// loadedDashboard is everything the dashboard reads from the store before
// it takes the terminal. Loading touches the database, so it runs off the
// render loop.
type loadedDashboard struct {
	workspaceID         string
	clientID            string
	controller          controller.Controller
	bootstrap           client.RuntimeReplica
	workspaces          []core.WorkspaceRecord
	layouts             map[string]core.PaneSizes
	conversationLayouts map[string]core.ConversationLayout
	queuedPrompts       map[string][]core.QueuedPrompt
	// launchRecency is the project ordering and legacy preselection, before
	// stopped sessions are set aside.
	launchRecency []client.LaunchRecency
}

// loadedDashboardFromDaemon builds the dashboard's starting state from what
// the daemon read from the store and its atomic runtime publication.
func loadedDashboardFromDaemon(workspaceID, clientID string, workspaces []core.WorkspaceRecord, startup client.DashboardStartup) loadedDashboard {
	bootstrap := startup.Runtime.IntoReplica()
	config := bootstrap.Projection.Metadata.InstalledConfig()
	state := core.NewStateState()
	state.Sessions = bootstrap.Projection.Records
	state.Subagents = bootstrap.Projection.Subagents
	state.EnvironmentOwners = bootstrap.Projection.EnvironmentOwners
	state.OperatorPresence = bootstrap.Projection.Metadata.OperatorPresence
	state.LastSubagentPolicies = bootstrap.Projection.Metadata.LastSubagentPolicies
	state.MountHistory = startup.Metadata.MountHistory
	state.ContainerSizes = startup.Metadata.ContainerSizes
	var launchRecency []client.LaunchRecency
	if recency := bootstrap.Projection.Metadata.LaunchRecency.Get(); recency != nil {
		launchRecency = append(launchRecency, (*recency)...)
	}
	return loadedDashboard{
		workspaceID:         workspaceID,
		clientID:            clientID,
		controller:          controller.Controller{Config: config, State: state},
		bootstrap:           bootstrap,
		workspaces:          workspaces,
		layouts:             startup.PaneSizes,
		conversationLayouts: startup.ConversationLayouts,
		queuedPrompts:       startup.QueuedPrompts,
		launchRecency:       launchRecency,
	}
}

// background is the color the dashboard's first frame paints behind
// everything.
func (l *loadedDashboard) background() chat.Color {
	theme := chat.EffectiveTheme(l.controller.Config.Theme)
	var background chat.Color
	chat.WithTheme(theme, func() { background = chat.ThemePalette().Background })
	return background
}

// terminalEvent is one terminal input as crossterm's EventStream yields it:
// a Bubble Tea message, or the error that ended reading.
type terminalEvent struct {
	msg tea.Msg
	err error
}

// terminalEvents is the terminal's input. A closed channel means input has
// ended.
type terminalEvents <-chan terminalEvent

// dashboardScreen is the interactive terminal and what reads it: raw mode
// and the alternate screen, its input, and the signals that ask it to
// close.
type dashboardScreen struct {
	terminal *terminalGuard
	events   terminalEvents
	// termination ends at the first termination signal (the termination
	// coordinator's token).
	termination context.Context
	// splash is a startup splash still playing, which the dashboard
	// finishes.
	splash *splashPlayback
}

// enterDashboardScreen takes the terminal (DashboardScreen::enter).
func enterDashboardScreen() (*dashboardScreen, error) {
	terminal, err := terminalGuardEnter()
	if err != nil {
		return nil, err
	}
	// mj-controller's termination coordinator: the first SIGINT, SIGTERM
	// or SIGHUP ends the token.
	termination, _ := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM, syscall.SIGHUP)
	// Rust opens crossterm's EventStream here. The Charm input reader that
	// replaces it belongs to the terminal guard (main.rs's TerminalGuard),
	// which is not ported yet and refuses the terminal above, so no stream
	// is reached without it.
	return &dashboardScreen{
		terminal:    terminal,
		termination: termination,
	}, nil
}

// detachReadFrontier: background events in a warm, hidden chat have not
// been read.
func detachReadFrontier(visible bool, latest, acknowledged uint64) uint64 {
	if visible {
		return max(latest, acknowledged)
	}
	return acknowledged
}

func questionDraftProjectionIsCurrent(capturedEventOrdinal, projectionOrdinal uint64) bool {
	return projectionOrdinal >= capturedEventOrdinal
}

// questionClickFocuses reports whether a left press belongs to the visible
// question rather than to a genuine dashboard modal. ElicitationMessage is
// the question's marker; its presence lets the generic ModalBody surface
// cover the answer area without making every dashboard dialog focus the
// composer.
func questionClickFocuses(dashboardModalOpen bool, surfaces *chat.FrameSurfaces, mouse tea.Mouse) bool {
	if dashboardModalOpen || surfaces.Surface(chat.SurfaceIdElicitationMessage{}) == nil {
		return false
	}
	surface := surfaces.SurfaceAt(mouse.X, mouse.Y)
	if surface == nil {
		return false
	}
	switch surface.ID.(type) {
	case chat.SurfaceIdElicitationMessage, chat.SurfaceIdModalBody:
		return true
	}
	return false
}

// pointerPressFocusesPrompt: prompt focus follows the press, while
// selection still owns the gesture.
func pointerPressFocusesPrompt(dashboardModalOpen bool, surfaces *chat.FrameSurfaces, mouse tea.Mouse) bool {
	if questionClickFocuses(dashboardModalOpen, surfaces, mouse) {
		return true
	}
	if dashboardModalOpen {
		return false
	}
	surface := surfaces.SurfaceAt(mouse.X, mouse.Y)
	if surface == nil {
		return false
	}
	_, prompt := surface.ID.(chat.SurfaceIdPromptInput)
	return prompt
}

func routePromptSelection(selection *chat.SelectionState, dashboard *tui.DashboardState, event tea.Msg) selectionRouting {
	if click, ok := event.(tea.MouseClickMsg); ok && click.Button == tea.MouseLeft &&
		pointerPressFocusesPrompt(dashboard.ModalOpen(), dashboard.FrameSurfaces(), click.Mouse()) {
		dashboard.FocusPrompt()
	}
	return routeSelectionEvent(selection, dashboard.FrameSurfaces(), event)
}

// markActiveChatRetiringForRemoteLifecycle: a lifecycle started by another
// attached surface reaches this UI through the daemon feed rather than its
// local action path. Give the open chat the same early retirement signal so
// its closing relay feed is not reported as a lost connection while the
// session row already says Stopping.
func markActiveChatRetiringForRemoteLifecycle(activeChat *chat.ActiveChat, sessionID string, kind tui.SessionOperationKind) {
	switch kind {
	case tui.SessionOperationKindSuspending, tui.SessionOperationKindDestroying,
		tui.SessionOperationKindStopping, tui.SessionOperationKindMoving:
		markActiveChatRetiring(activeChat, sessionID)
	}
}

// nextTerminalEvent is the next terminal event; false once input has ended
// or ctx ended. Cancel-safe, so losing the select race cannot drop one. A
// missing stream is never ready: it is absent only while the setup dialog
// owns the terminal, which is not while this loop is waiting.
func nextTerminalEvent(ctx context.Context, events terminalEvents) (terminalEvent, bool) {
	if events == nil {
		<-ctx.Done()
		return terminalEvent{}, false
	}
	select {
	case event, ok := <-events:
		return event, ok
	case <-ctx.Done():
		return terminalEvent{}, false
	}
}

// discardSessionComposer: lifecycle disposal must drop the editor itself
// before retiring its cached value, or the next capture would adopt the
// abandoned standby text again.
func discardSessionComposer(dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat, drafts *composerDraftCache, sessionID string) {
	delete(chats, sessionID)
	dashboard.TakeStandbyComposer(sessionID)
	drafts.discard(sessionID)
}

// terminalDraftState queues a draft for a known session even after it
// leaves the current feed. Receipt durability is owned separately by
// readReceipts.
type terminalDraftState struct {
	clientID  string
	sessionID string
	revision  uint64
	draft     core.DetachedSessionDraft
}

// recordTerminalDraftState starts the draft's save, or reports an unknown
// session on the notice bar and answers nil.
func recordTerminalDraftState(controller *controller.Controller, departed *core.SessionRecord, dashboard *tui.DashboardState, draft terminalDraftState, updates *core.Unbounded[dashboardIoUpdate], tracker client.CriticalOperationTracker) *core.JoinHandle[struct{}] {
	if !controller.State.Sessions.ContainsKey(draft.sessionID) && departed == nil {
		dashboard.SetNotice(fmt.Sprintf("Could not save draft for %s: unknown session", core.ShortID(draft.sessionID)))
		return nil
	}
	sessionName := sessionNoticeName(&controller.State, draft.sessionID)
	task := spawnTerminalDraftPersist(
		draft.clientID,
		namedSession{id: draft.sessionID, name: sessionName},
		draft.revision,
		draft.draft,
		updates,
		tracker,
	)
	return task.handle
}

// openingCancelEvent reports whether event is the Escape that cancels the
// focused pane's attach.
func openingCancelEvent(event tea.Msg, opening, modal, formOwnsKeyboard bool) bool {
	key, pressed := event.(tea.KeyPressMsg)
	return opening && !modal && !formOwnsKeyboard && pressed && key.Code == tea.KeyEscape
}

// pumpChats waits for the next background message of any warm
// conversation.
//
// chat.ActiveChatPump is cancel safe, so the waits that lose this race end
// without losing what they were waiting for. With no warm chat there is
// nothing to wait for, and the arm never fires before ctx ends.
func pumpChats(ctx context.Context, chats map[string]*chat.ActiveChat) {
	if len(chats) == 0 {
		<-ctx.Done()
		return
	}
	// The first pump to finish wins; the others are cancelled and waited
	// for, so no two goroutines touch a chat once this returns.
	raceCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	finished := make(chan struct{}, len(chats))
	var pumps sync.WaitGroup
	for _, id := range sortedKeys(chats) {
		active := chats[id]
		pumps.Add(1)
		go func() {
			defer pumps.Done()
			chat.ActiveChatPump(raceCtx, active)
			finished <- struct{}{}
		}()
	}
	select {
	case <-finished:
	case <-ctx.Done():
	}
	cancel()
	pumps.Wait()
}

// chatIsVisible reports whether the warm chat belongs on screen, given the
// session an attach is running for.
//
// Only an attach for a different session hides it. An attach for the chat
// already loaded is a reattach of the same conversation, and blanking the
// transcript for that would be a flicker rather than a correction.
func chatIsVisible(opening *string, chatSessionID string) bool {
	return opening == nil || *opening == chatSessionID
}

// chatOutcomeOf is a handler's outcome, or None when it produced none.
func chatOutcomeOf(outcome chat.ChatEventOutcome) chat.ChatEventOutcome {
	if outcome == nil {
		return chat.ChatEventOutcomeNone{}
	}
	return outcome
}

// isChatOutcomeNone reports whether outcome asks the loop for no work.
func isChatOutcomeNone(outcome chat.ChatEventOutcome) bool {
	_, none := outcome.(chat.ChatEventOutcomeNone)
	return none
}

// dispatchEvent hands one event to the part of the surface it belongs to,
// and reports whether it was consumed and whether the loop may keep
// batching, which it may while the event asked for no work.
//
// A mouse event goes where the pointer is, not where the keyboard is: the
// wheel over a transcript scrolls that transcript, including an unfocused
// pane's, and a click there hands the keyboard back to the composer. Keys go
// to a dashboard modal if one is open, then to the composer if it has
// focus, and otherwise to the panes. A conversation's own dialog never
// gates the host: it lives inside its pane and takes keys only while that
// pane's composer has focus.
func dispatchEvent(context *dashboardContext, event tea.Msg, action *tui.DashboardAction, chatOutcome *chat.ChatEventOutcome) (consumed bool, batch bool) {
	context.dashboard.PrepareComponentEvent(event, context.chats)
	mouse, isMouse := event.(tea.MouseMsg)
	if isMouse && !context.dashboard.ModalOpen() && !context.dashboard.ComponentPointerCaptured() {
		if session, ok := context.dashboard.ConversationPointerSession(context.chats); ok {
			if pane, ok := context.dashboard.PaneForSession(session); ok {
				if paneChat := context.paneChatMut(pane); paneChat != nil {
					result := paneChat.HandleEventResult(event)
					*chatOutcome = chatOutcomeOf(result.Action)
					return result.Consumed, isChatOutcomeNone(*chatOutcome)
				}
			}
		}
	}
	if result, ok := context.dashboard.HandleNotificationFormEvent(context.chats, event); ok {
		*chatOutcome = chatOutcomeOf(result.Action)
		return result.Consumed, isChatOutcomeNone(*chatOutcome)
	}
	dashboardPointer := isMouse && context.dashboard.ComponentHandlesMouse(mouse)
	// The wheel belongs to the pane under the pointer. Only an unfocused
	// pane needs saying so: over the focused pane the conversation below is
	// the one visibleChat already returns.
	var wheelPane *tui.PaneId
	if wheel, ok := event.(tea.MouseWheelMsg); ok && !dashboardPointer && !context.dashboard.ModalOpen() &&
		(wheel.Button == tea.MouseWheelUp || wheel.Button == tea.MouseWheelDown) {
		if pane, ok := context.dashboard.ChatRegionContains(wheel.X, wheel.Y); ok && pane != context.dashboard.FocusedPane() {
			wheelPane = &pane
		}
	}
	toChat := false
	if !dashboardPointer {
		if isMouse && !context.dashboard.ModalOpen() {
			pointer := mouse.Mouse()
			pane, overPane := context.dashboard.ChatRegionContains(pointer.X, pointer.Y)
			if click, ok := event.(tea.MouseClickMsg); ok && overPane && click.Button == tea.MouseLeft {
				// A click in a pane takes the keyboard there before the event
				// reaches a conversation, so it reaches the conversation the
				// user just pointed at.
				context.focusConversationPane(pane)
			}
			visible := context.visibleChat()
			toChat = overPane || (visible != nil && visible.ComponentHandlesMouse(mouse))
			if !toChat && visible != nil && visible.TranscriptScrollbarDragging() {
				switch held := event.(type) {
				case tea.MouseMotionMsg:
					toChat = held.Button == tea.MouseLeft
				case tea.MouseReleaseMsg:
					toChat = held.Button == tea.MouseLeft
				}
			}
		} else if !isMouse {
			toChat = !context.dashboard.ModalOpen() && context.dashboard.PromptHasFocus()
		}
	}
	var target *chat.ActiveChat
	if toChat {
		if wheelPane != nil {
			target = context.paneChatMut(*wheelPane)
		} else {
			target = context.visibleChat()
		}
	}
	if target != nil {
		result := target.HandleEventResult(event)
		*chatOutcome = chatOutcomeOf(result.Action)
		return result.Consumed, isChatOutcomeNone(*chatOutcome)
	}
	preflightGeneration := context.dashboard.SessionPreflightGeneration()
	result := context.dashboard.HandleEventResult(event)
	*action = result.Action
	if *action == nil {
		*action = tui.DashboardActionNone{}
	}
	if context.dashboard.SessionPreflightGeneration() != preflightGeneration {
		context.cancelSessionPreflight()
	}
	_, none := (*action).(tui.DashboardActionNone)
	context.controllerChanged = context.controllerChanged || !none
	return result.Consumed, none
}

// drawSelection highlights the live selection on the frame the view just
// drew and returns the text it covers.
//
// Both halves read the same frame, so the copied text is exactly what the
// highlight marks. Surfaces that scroll their own content extract from
// their row cache instead: only the visible band of such a selection is on
// this frame, and the highlight is all that band is good for.
func drawSelection(frame *chat.Frame, selection *chat.SelectionState, surfaces *chat.FrameSurfaces) *string {
	selection.ReconcileSurfaces(surfaces)
	id := selection.ActiveSurface()
	if id == nil {
		return nil
	}
	selected, ok := selection.Range()
	if !ok {
		return nil
	}
	surface := surfaces.Surface(id)
	if surface == nil {
		return nil
	}
	frameSurface := *surface
	chat.Highlight(frame.Buffer(), &frameSurface, &selected)
	switch id.(type) {
	case chat.SurfaceIdTranscript, chat.SurfaceIdElicitationMessage, chat.SurfaceIdNotificationText:
		return nil
	}
	text := chat.ExtractRows(frame.Buffer(), &frameSurface, &selected)
	return &text
}

// selectionRouting is what the selection engine decided about one terminal
// event.
type selectionRouting interface{ isSelectionRouting() }

// selectionRoutingConsumed: the engine took the event; the frame only needs
// redrawing.
type selectionRoutingConsumed struct{}

// selectionRoutingForward: the engine wants nothing to do with this event;
// hand it to the view. A release that never dragged arrives here as the
// press the view's click handling expects.
type selectionRoutingForward struct{ event tea.Msg }

// selectionRoutingCopy: a drag finished. The caller extracts the selected
// text and copies it.
type selectionRoutingCopy struct {
	surface        chat.SurfaceId
	selectionRange chat.SelectionRange
}

func (selectionRoutingConsumed) isSelectionRouting() {}
func (selectionRoutingForward) isSelectionRouting()  {}
func (selectionRoutingCopy) isSelectionRouting()     {}

// routeSelectionEvent routes one terminal event between the selection
// engine and the view.
//
// The engine only claims left-button gestures that start on a registered
// surface: a press elsewhere, the wheel, and every other mouse kind stay the
// view's. Esc drops a finished selection instead of reaching the view, so
// the key that clears the highlight cannot also quit or cancel.
func routeSelectionEvent(selection *chat.SelectionState, surfaces *chat.FrameSurfaces, event tea.Msg) selectionRouting {
	selection.ReconcileSurfaces(surfaces)
	switch mouse := event.(type) {
	case tea.MouseClickMsg:
		if mouse.Button != tea.MouseLeft {
			return selectionRoutingForward{event: event}
		}
		selection.OnMouseDown(mouse.X, mouse.Y, surfaces)
		if selection.ActiveSurface() != nil {
			return selectionRoutingConsumed{}
		}
		return selectionRoutingForward{event: event}
	case tea.MouseMotionMsg:
		if mouse.Button == tea.MouseLeft && selection.ActiveSurface() != nil {
			selection.OnMouseDrag(mouse.X, mouse.Y, surfaces)
			return selectionRoutingConsumed{}
		}
		return selectionRoutingForward{event: event}
	case tea.MouseReleaseMsg:
		if mouse.Button != tea.MouseLeft || selection.ActiveSurface() == nil {
			return selectionRoutingForward{event: event}
		}
		switch released := selection.OnMouseUp(mouse.X, mouse.Y, surfaces).(type) {
		case chat.SelectionActionClick:
			// The views key their click and double-click handling on
			// presses, so a click that the engine held back is replayed as
			// one when the button comes up.
			return selectionRoutingForward{event: tea.MouseClickMsg{X: released.Column, Y: released.Row, Button: tea.MouseLeft}}
		case chat.SelectionActionCopyRequested:
			return selectionRoutingCopy{surface: released.Surface, selectionRange: released.Range}
		default:
			return selectionRoutingConsumed{}
		}
	case tea.KeyPressMsg:
		if mouse.Code == tea.KeyEscape && mouse.Mod == 0 {
			if _, ok := selection.Range(); ok {
				selection.Clear()
				return selectionRoutingConsumed{}
			}
		}
	}
	return selectionRoutingForward{event: event}
}

const daemonUnavailableNotice = "Mjolnir daemon is unavailable; waiting to reconnect."
const daemonRunningAgainNotice = "Mjolnir daemon is running again."

// showDaemonReattached reports that the keep-alive reached a daemon again.
//
// The unavailable notice is a failure notice, which a plain notice cannot
// replace until it has been readable for a while. A daemon restart is often
// shorter than that, so a plain SetNotice was dropped and the stale failure
// stayed up while the dashboard was live. The reconnect is exactly the event
// that makes that failure false, so it replaces it directly, also when it is
// shown inside a count of stacked failures.
func showDaemonReattached(dashboard *tui.DashboardState) {
	if !replaceStaleDaemonFailure(dashboard) {
		dashboard.SetNotice(daemonRunningAgainNotice)
	}
}

// replaceStaleDaemonFailure replaces a "daemon unavailable" or "daemon not
// running" failure with the reconnect notice. Answers whether one was
// showing.
func replaceStaleDaemonFailure(dashboard *tui.DashboardState) bool {
	current, ok := dashboard.Notice()
	if !ok {
		return false
	}
	stale := false
	for _, failure := range []string{
		daemonUnavailableNotice,
		// A request that hit the stopped daemon reports this error text.
		client.DaemonNotRunningMessage,
	} {
		if strings.HasSuffix(current, failure) {
			stale = true
			break
		}
	}
	return stale && dashboard.ReplaceNoticeIf(current, daemonRunningAgainNotice)
}

// reconcileDaemonFailure keeps the bar true to the keep-alive's current
// state, not only to its transitions. A restart can leave the keep-alive
// attached with the failure still showing, when the transition was missed
// or the failure was posted after it, so the connection state also clears
// it here on the clock tick. Answers whether the bar changed.
func reconcileDaemonFailure(dashboard *tui.DashboardState, presence DaemonPresence, runningAgainSince **time.Time, now time.Time) bool {
	if _, attached := presence.(DaemonPresenceAttached); attached && replaceStaleDaemonFailure(dashboard) {
		*runningAgainSince = &now
		return true
	}
	return false
}

const daemonRunningAgainDisplay = 8 * time.Second

// expireDaemonRunningAgain takes the reconnect notice down once it has been
// readable for daemonRunningAgainDisplay. Only that notice expires here: if
// something else has taken the bar since, it is left alone. Answers whether
// the bar changed, so the caller can draw the frame that shows the key
// hints.
func expireDaemonRunningAgain(notices chat.Notices, since **time.Time, now time.Time) bool {
	shownAt := *since
	if shownAt == nil {
		return false
	}
	if current, ok := notices.Current(); !ok || current != daemonRunningAgainNotice {
		*since = nil
		return false
	}
	if now.Sub(*shownAt) < daemonRunningAgainDisplay {
		return false
	}
	*since = nil
	return notices.Dismiss(now)
}

func resumeProgressNotice(sessionName, profileID, targetID string) string {
	return fmt.Sprintf("Preparing %s: verifying checkpoint, provisioning %s, and restoring %s…", sessionName, targetID, profileID)
}
