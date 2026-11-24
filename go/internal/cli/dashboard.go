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
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

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
	panic("unported: mj-cli/src/dashboard.rs startup_session_choice")
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
	panic("unported: mj-cli/src/dashboard.rs log_quit_timings")
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
	panic("unported: mj-cli/src/dashboard.rs finish_shutdown_save")
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
	paneSizePersistence    workspaceSettingPersistence[core.PaneSizes]
	sidebarSizePersistence workspaceSettingPersistence[core.PaneSize]
	layoutPersistence      workspaceSettingPersistence[core.ConversationLayout]
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
	loopStalls meter
	// daemonPresence is what the two-second keep-alive last saw. It never
	// starts a daemon, so a daemon that goes away stays away until the user
	// asks for it back.
	daemonPresence *core.Watch[DaemonPresence]
	autoAfk        autoAfkReporter

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
	lifecycleOperations map[string]activeLifecycleOperation

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
	summaryReads         summaryReads
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
	panic("unported: mj-cli/src/dashboard.rs From<ManagedSessionSnapshot> for MaterializedProjection")
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
	panic("unported: mj-cli/src/dashboard.rs enqueue_materialized_projection")
}

// launchRepositoryTopLevel is the top level of the Git worktree `mj` was
// started in, or nil outside one. The new-session wizard offers it as the
// local project. Blocking: run it off the loop.
func launchRepositoryTopLevel() *string {
	panic("unported: mj-cli/src/dashboard.rs launch_repository_top_level")
}

// dashboardGo is the fast-start mode a dashboard opens in, and whether it
// opens on its setup (Rust's `(GoMode, bool)`).
type dashboardGo struct {
	mode  tui.GoMode
	setup bool
}

// runDashboardForWorkspace runs the dashboard over loaded until the person
// quits, detaches, or an upgraded daemon asks the terminal to restart.
func runDashboardForWorkspace(ctx context.Context, loaded loadedDashboard, screen *dashboardScreen, openWorkspaceManager bool, goStart *dashboardGo, daemonPresence *core.Watch[DaemonPresence], resume *upgradeResume, updateNotice controller.StartupUpdateNotice) (dashboardExit, error) {
	panic("unported: mj-cli/src/dashboard.rs run_dashboard_for_workspace")
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
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::focused_chat")
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
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::sync_opening_session")
}

// cancelChatOpenFor stops the session's shared attachment, whichever
// surfaces requested it.
func (c *dashboardContext) cancelChatOpenFor(sessionID string) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::cancel_chat_open_for")
}

func (c *dashboardContext) deferAllChatOpens() {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::defer_all_chat_opens")
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
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::record_chat_detach")
}

// parkChatComposer keeps pending input jobs alive when navigation drops a
// worker-facing view.
func (c *dashboardContext) parkChatComposer(sessionID string) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::park_chat_composer")
}

// retireChatsOutsideTheLayout saves and retires chats when neither the
// layout nor a form card holds them.
func (c *dashboardContext) retireChatsOutsideTheLayout() {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::retire_chats_outside_the_layout")
}

func (c *dashboardContext) openSessionInSplit(sessionID string, direction chat.Direction) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::open_session_in_split")
}

func (c *dashboardContext) splitEmptyPane(direction chat.Direction) {
	c.splitConversationPane(c.dashboard.FocusedPane(), direction)
}

func (c *dashboardContext) splitConversationPane(target tui.PaneId, direction chat.Direction) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::split_conversation_pane")
}

func (c *dashboardContext) pinSessionIn(sessionID string, pane tui.PaneId) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::pin_session_in")
}

func (c *dashboardContext) unpinSession(sessionID string) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::unpin_session")
}

// focusConversationPane moves the keyboard into one conversation pane and
// its composer. The arrangement is saved, because which pane has the focus
// is part of it.
func (c *dashboardContext) focusConversationPane(pane tui.PaneId) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::focus_conversation_pane")
}

// closePane removes a pinned or empty pane while preserving its
// conversation state.
func (c *dashboardContext) closePane(pane tui.PaneId) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::close_pane")
}

// saveActiveWorkspaceLayout queues the active workspace's arrangement for
// saving once this client has changed it.
func (c *dashboardContext) saveActiveWorkspaceLayout() {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::save_active_workspace_layout")
}

// setWorkspaceLayout records a workspace's conversation pane arrangement
// and queues its save.
func (c *dashboardContext) setWorkspaceLayout(workspaceID string, layout core.ConversationLayout) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::set_workspace_layout")
}

func (c *dashboardContext) requestShutdown() { c.beginShutdown(true) }

// finishSessionsStoppedBySuspend lets go of the conversations of
// sub-agents that their parent's suspend stopped (R15-1). Their records are
// gone, so there is no draft or read position to save, and trying would
// only report an unknown session. The dashboard has already put the view
// back on the parent, whose conversation opens in their place.
func (c *dashboardContext) finishSessionsStoppedBySuspend() {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::finish_sessions_stopped_by_suspend")
}

// discardWorkspaceComposers: a removed workspace's composers must not be
// saved back on tab switch.
func (c *dashboardContext) discardWorkspaceComposers(workspaceID string) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::discard_workspace_composers")
}

func (c *dashboardContext) selectWorkspace(workspaceID *string) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::select_workspace")
}

// chatSessionRecord is the record a chat's bookkeeping reads: the live one,
// or the last one of a session that has just left the feed. The pointer is
// into the context's own state.
func (c *dashboardContext) chatSessionRecord(sessionID string) *core.SessionRecord {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::chat_session_record")
}

func (c *dashboardContext) sessionInActiveWorkspace(sessionID string) bool {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::session_in_active_workspace")
}

func (c *dashboardContext) beginShutdown(detached bool) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::begin_shutdown")
}

// refreshShutdownNotice returns true once every user-authored mutation has
// reached a durable boundary. Pure reads and projections are deliberately
// not blockers.
func (c *dashboardContext) refreshShutdownNotice() bool {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::refresh_shutdown_notice")
}

// acknowledgeVisibleChats advances the read receipt of every conversation
// that was on screen. The frame records which those were; before the first
// frame it is the one the focused pane would draw.
func (c *dashboardContext) acknowledgeVisibleChats() {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::acknowledge_visible_chats")
}

func (c *dashboardContext) acknowledgeChat(sessionID string) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::acknowledge_chat")
}

func (c *dashboardContext) acknowledgeDashboardSessions(receipts []sessionThrough) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::acknowledge_dashboard_sessions")
}

func (c *dashboardContext) spawnReadReceipt(sessionID string, through uint64) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::spawn_read_receipt")
}

// finishReadReceipt records a read receipt's save: through on success, the
// failure otherwise (Rust's Result<u64, String>).
func (c *dashboardContext) finishReadReceipt(sessionID string, through uint64, failure error) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::finish_read_receipt")
}

// openDashboardContext takes the terminal and starts every background feed
// over the state loaded before it (DashboardContext::open).
func openDashboardContext(loaded loadedDashboard, terminal *terminalGuard, events terminalEvents, daemonPresence *core.Watch[DaemonPresence]) (*dashboardContext, error) {
	panic("unported: mj-cli/src/dashboard.rs DashboardContext::open")
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
	panic("unported: mj-cli/src/dashboard.rs LoadedDashboard::from_daemon")
}

// background is the color the dashboard's first frame paints behind
// everything.
func (l *loadedDashboard) background() chat.Color {
	panic("unported: mj-cli/src/dashboard.rs LoadedDashboard::background")
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
	panic("unported: mj-cli/src/dashboard.rs DashboardScreen::enter")
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
	panic("unported: mj-cli/src/dashboard.rs question_click_focuses")
}

// pointerPressFocusesPrompt: prompt focus follows the press, while
// selection still owns the gesture.
func pointerPressFocusesPrompt(dashboardModalOpen bool, surfaces *chat.FrameSurfaces, mouse tea.Mouse) bool {
	panic("unported: mj-cli/src/dashboard.rs pointer_press_focuses_prompt")
}

func routePromptSelection(selection *chat.SelectionState, dashboard *tui.DashboardState, event tea.Msg) selectionRouting {
	panic("unported: mj-cli/src/dashboard.rs route_prompt_selection")
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
	panic("unported: mj-cli/src/dashboard.rs discard_session_composer")
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
	panic("unported: mj-cli/src/dashboard.rs record_terminal_draft_state")
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
	panic("unported: mj-cli/src/dashboard.rs pump_chats")
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
	panic("unported: mj-cli/src/dashboard.rs dispatch_event")
}

// drawSelection highlights the live selection on the frame the view just
// drew and returns the text it covers.
//
// Both halves read the same frame, so the copied text is exactly what the
// highlight marks. Surfaces that scroll their own content extract from
// their row cache instead: only the visible band of such a selection is on
// this frame, and the highlight is all that band is good for.
func drawSelection(frame *chat.Frame, selection *chat.SelectionState, surfaces *chat.FrameSurfaces) *string {
	panic("unported: mj-cli/src/dashboard.rs draw_selection")
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
	panic("unported: mj-cli/src/dashboard.rs route_selection_event")
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
	panic("unported: mj-cli/src/dashboard.rs replace_stale_daemon_failure")
}

// reconcileDaemonFailure keeps the bar true to the keep-alive's current
// state, not only to its transitions. A restart can leave the keep-alive
// attached with the failure still showing, when the transition was missed
// or the failure was posted after it, so the connection state also clears
// it here on the clock tick. Answers whether the bar changed.
func reconcileDaemonFailure(dashboard *tui.DashboardState, presence DaemonPresence, runningAgainSince **time.Time, now time.Time) bool {
	panic("unported: mj-cli/src/dashboard.rs reconcile_daemon_failure")
}

const daemonRunningAgainDisplay = 8 * time.Second

// expireDaemonRunningAgain takes the reconnect notice down once it has been
// readable for daemonRunningAgainDisplay. Only that notice expires here: if
// something else has taken the bar since, it is left alone. Answers whether
// the bar changed, so the caller can draw the frame that shows the key
// hints.
func expireDaemonRunningAgain(notices chat.Notices, since **time.Time, now time.Time) bool {
	panic("unported: mj-cli/src/dashboard.rs expire_daemon_running_again")
}

func resumeProgressNotice(sessionName, profileID, targetID string) string {
	return fmt.Sprintf("Preparing %s: verifying checkpoint, provisioning %s, and restoring %s…", sessionName, targetID, profileID)
}
