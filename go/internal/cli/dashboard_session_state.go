// Port of mj-cli/src/dashboard/session_state.rs.
//
// The dashboard context's session bookkeeping: sub-agent scope, stored
// summaries and projections, conversation attachments, the visible chat,
// and the background reads of each session's working context, Git status
// and project source.

package cli

import (
	"context"
	"fmt"
	"log/slog"
	"maps"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// updateSubagentScope: a family stays subscribed while its rows or a
// retained conversation need it. Reports whether the scope changed.
func (c *dashboardContext) updateSubagentScope() bool {
	parents := map[string]struct{}{}
	if parent, ok := c.dashboard.SubagentParentID(); ok {
		parents[parent] = struct{}{}
	}
	for _, entry := range c.dashboard.PaneSessions() {
		if child, ok := c.controller.State.Subagents.Get(entry.SessionID); ok {
			parents[child.ParentSessionID] = struct{}{}
		}
	}
	if parent, ok := c.dashboard.DisplayedNotificationParentID(); ok {
		parents[parent] = struct{}{}
	}
	// watch::Sender::send_if_modified; the dashboard is the only sender.
	if maps.Equal(c.runtimeSubagentParents.Borrow(), parents) {
		return false
	}
	c.runtimeSubagentParents.SendReplace(parents)
	return true
}

// requestTranscriptTailSeed fills a freshly resumed conversation from the
// tail of its stored transcript, off the event loop.
//
// The projection is durable the moment the resume completes, so nothing has
// to travel back through the daemon reply to show it. The view keeps only
// the last chat.TailSeedItems entries, so the seed reads exactly that many
// rather than the whole history. The poller delivers the complete projection
// a moment later; this exists so the conversation is not blank until it
// does.
func (c *dashboardContext) requestTranscriptTailSeed(sessionID string) {
	updates := c.dashboardIoTx
	core.Spawn(func() struct{} {
		ctx := context.Background()
		seeded, err := func() (*core.MaterializedSession, error) {
			daemonClient, err := ConnectOrStart(ctx)
			if err != nil {
				return nil, err
			}
			defer daemonClient.Close()
			return daemonClient.StoredProjectionTail(ctx, sessionID, chat.TailSeedItems)
		}()
		// A failed seed costs a blank conversation until the next poll,
		// which is where the full projection comes from anyway. It is not
		// worth failing a resume that otherwise succeeded.
		if err != nil {
			slog.Debug("transcript tail seed failed", "session_id", sessionID, "error", err.Error())
			return struct{}{}
		}
		if seeded == nil {
			return struct{}{}
		}
		report[dashboardIoUpdate]("seeding the transcript tail", updates, dashboardIoUpdateTranscriptTailSeed{materialized: seeded})
		return struct{}{}
	})
}

// requestMaterializedProjection keeps one projection per session in flight
// and remembers only the newest snapshot that arrived behind it. The shared
// permits bound work across different sessions as well.
func (c *dashboardContext) requestMaterializedProjection(projection materializedProjection, viewedThroughEventOrdinal uint64) {
	queued := enqueueMaterializedProjection(c.materializedProjectionsInFlight, c.pendingMaterializedProjections, projection, viewedThroughEventOrdinal)
	if queued == nil {
		return
	}
	sessionID := queued.projection.materialized.SessionID
	previous := c.dashboard.TakeProjectionCache(sessionID)
	spawnMaterializedSessionProjection(queued.projection, queued.viewedThroughEventOrdinal, previous, c.dashboardIoTx, c.materializedProjectionPermits)
}

func (c *dashboardContext) wantedSummarySessions() map[string]uint64 {
	return c.dashboard.WantedSummarySessions()
}

func (c *dashboardContext) hydrateStoredSessionSummaries() {
	wanted := c.wantedSummarySessions()
	c.startup = beginStartupSession(slices.Sorted(maps.Keys(wanted)), time.Now())
	c.reconcileStoredSessionSummaries()
}

func (c *dashboardContext) reconcileStoredSessionSummaries() {
	wanted := c.wantedSummarySessions()
	item := c.dashboard.WantedNotificationMessage()
	for _, removed := range c.summaryReads.reconcile(wanted, item, c.dashboardIoTx) {
		c.finishStartupSummary(removed)
	}
	pending := map[string]struct{}{}
	for _, session := range c.summaryReads.loadingSessions() {
		pending[session] = struct{}{}
	}
	for session := range c.materializedProjectionsInFlight {
		pending[session] = struct{}{}
	}
	c.dashboard.SyncTranscriptHydration(pending)
	loading := item != nil && c.summaryReads.itemLoading(*item)
	c.dashboard.SyncNotificationMessageRead(item, loading)
}

// finishStartupSummary records that one live session's stored summary has
// arrived, however it turned out, and opens the startup conversation once
// they all have.
func (c *dashboardContext) finishStartupSummary(sessionID string) {
	c.startup.summaryArrived(sessionID)
	c.maybeOpenStartupSession()
}

// reconcileConversationAttachments attaches to the conversations assigned
// by navigation. A failed open needs an explicit retry, not another attempt
// on every background completion.
func (c *dashboardContext) reconcileConversationAttachments() {
	// Startup chooses only once; after that the layout already owns every
	// selection. Attachment work follows it without choosing anything.
	if c.startup.pickPending() {
		return
	}
	notification, hasNotification := c.dashboard.NotificationFormSession()
	for _, session := range slices.Sorted(maps.Keys(c.attachments)) {
		attachment := c.attachments[session]
		attachment.pruneAssignments(c.dashboard, session)
		attachment.bindNotification(hasNotification && notification == session)
	}
	// A transition finishing may make an assigned conversation attachable.
	// Selection changes from refresh never assign a different session. A
	// lifecycle release already emptied its pane. A historical session
	// selected deliberately has no worker and must not start an attach.
	for _, entry := range c.dashboard.PaneSessions() {
		pane, session := entry.Pane, entry.SessionID
		if c.dashboard.SessionRecord(session) == nil && !c.dashboard.IsNativeAgent(session) {
			continue
		}
		c.attachmentFor(session).observePane(c.dashboard, pane, session)
		if c.dashboard.PaneSessionIsSuspended(session) {
			c.cancelChatOpenIn(pane)
			continue
		}
		if c.dashboard.PaneSessionCanAttach(session) && c.attachmentFor(session).selectForPane(pane, session) {
			c.openChatSessionInto(pane, session)
		}
	}
	if hasNotification && c.dashboard.PaneSessionCanAttach(notification) {
		attachment := c.attachmentFor(notification)
		attachment.bindNotification(true)
		if attachment.selectSession(notification) {
			c.startChatAttach(notification)
		}
	}
	retireUnusedAttachments(c.dashboard, c.attachments)
	opening := map[tui.PaneId]string{}
	for _, entry := range c.dashboard.PaneSessions() {
		if attachment, ok := c.attachments[entry.SessionID]; ok && attachment.panePending(entry.Pane) {
			opening[entry.Pane] = entry.SessionID
		}
	}
	c.openingChatSessions = opening
	c.retireChatsOutsideTheLayout()
	c.syncOpeningSession()
}

func (c *dashboardContext) wantedChatSessions() map[string]struct{} {
	return wantedChatSessions(c.dashboard, c.attachments, c.chats)
}

// cancelChatOpen stops the focused pane's attach. A failed or cancelled
// open stays observed, so it is not retried on every background wakeup.
func (c *dashboardContext) cancelChatOpen() {
	c.cancelChatOpenIn(c.dashboard.FocusedPane())
}

func (c *dashboardContext) cancelChatOpenIn(pane tui.PaneId) {
	if session, ok := c.dashboard.PaneSession(pane); ok {
		if attachment, ok := c.attachments[session]; ok {
			attachment.releasePane(c.dashboard, pane, session)
		}
	}
	delete(c.openingChatSessions, pane)
	c.syncOpeningSession()
}

// releaseSuspendedPane lets a pane go of a suspended session instead of
// attaching to it. The dashboard decides what the pane keeps; see
// tui.DashboardState.ReleaseSuspendedPane.
func (c *dashboardContext) releaseSuspendedPane(pane tui.PaneId, sessionID string) {
	c.cancelChatOpenIn(pane)
	c.dashboard.ReleaseSuspendedPane(pane, sessionID)
	c.retireChatsOutsideTheLayout()
	c.saveActiveWorkspaceLayout()
}

// deferChatOpenIn gives up this pane's attach in a way that allows one
// fresh attempt when whatever owns the session has finished with it.
func (c *dashboardContext) deferChatOpenIn(pane tui.PaneId) {
	if session, ok := c.dashboard.PaneSession(pane); ok {
		if attachment, ok := c.attachments[session]; ok {
			attachment.deferAttach()
		}
	}
	delete(c.openingChatSessions, pane)
	c.syncOpeningSession()
}

// visibleChat is the warm chat when it belongs on screen, nil otherwise.
//
// While an attach for a different session is in flight, the chat still
// loaded is the one the selection has moved off. Drawing it under the new
// row's highlight, or handing it the keyboard, would report the wrong
// conversation, so it stays hidden until selected again. Its feeds keep
// running while an attach is pending, failed, or cancelled.
func (c *dashboardContext) visibleChat() *chat.ActiveChat {
	current, ok := c.dashboard.CurrentSessionID()
	if !ok {
		return nil
	}
	if attachment, ok := c.attachments[current]; ok && !attachment.paneCanShow(c.dashboard, c.dashboard.FocusedPane()) {
		return nil
	}
	var opening *string
	if session, ok := c.openingChatSessions[c.dashboard.FocusedPane()]; ok {
		opening = &session
	}
	conversation, ok := c.chats[current]
	if !ok {
		return nil
	}
	if conversation.HasLiveComposer() &&
		chatIsVisible(opening, conversation.SessionID()) &&
		c.dashboard.ChatInputAvailable(conversation.SessionID()) {
		return conversation
	}
	return nil
}

// paneChatMut is the warm chat one pane shows, whether or not that pane
// has the keyboard; nil when none.
//
// A pane answers only for the session it holds, so this asks the
// arrangement rather than the Sessions selection. While the pane is
// attaching to a different session its old conversation is hidden, the same
// rule the renderer draws by, and nothing is returned.
func (c *dashboardContext) paneChatMut(pane tui.PaneId) *chat.ActiveChat {
	var opening *string
	if session, ok := c.openingChatSessions[pane]; ok {
		opening = &session
	}
	sessionID, ok := c.dashboard.PaneSession(pane)
	if !ok {
		return nil
	}
	if attachment, ok := c.attachments[sessionID]; ok && !attachment.paneCanShow(c.dashboard, pane) {
		return nil
	}
	if !chatIsVisible(opening, sessionID) || !c.dashboard.ChatInputAvailable(sessionID) {
		return nil
	}
	conversation, ok := c.chats[sessionID]
	if !ok || !conversation.HasLiveComposer() {
		return nil
	}
	return conversation
}

// dashboardAndVisibleChat is the dashboard and the conversation on screen
// (nil when none), borrowed together, so a caller can act on one and report
// through the other.
func (c *dashboardContext) dashboardAndVisibleChat() (*tui.DashboardState, *chat.ActiveChat) {
	visible := c.visibleChat() != nil
	focused, ok := c.dashboard.CurrentSessionID()
	if !visible || !ok {
		return c.dashboard, nil
	}
	return c.dashboard, c.chats[focused]
}

func (c *dashboardContext) needsAnimation() bool {
	if c.dashboard.NeedsFastTick() {
		return true
	}
	visible := c.visibleChat()
	return visible != nil && visible.NeedsAnimation()
}

// cancelStartupSession: the user took the choice into their own hands, so
// the surface stops trying to pick a conversation for them.
func (c *dashboardContext) cancelStartupSession() {
	c.startup.cancel()
}

func (c *dashboardContext) refreshGoContext() {
	if c.dashboard.GoMode() == nil || c.goContextInFlight {
		return
	}
	sessionID, ok := c.dashboard.SelectedSessionID()
	if !ok {
		return
	}
	if refresh := c.goContextRefresh; refresh != nil && refresh.sessionID == sessionID && time.Since(refresh.at) < 5*time.Second {
		return
	}
	c.goContextInFlight = true
	c.goContextRefresh = &goContextRefresh{sessionID: sessionID, at: time.Now()}
	reportID := sessionID
	snapshot := c.liveControllerSnapshot()
	spawnIO(
		"reading session working context",
		c.dashboardIoTx,
		func() (client.DaemonPair[string, string], error) {
			executor := core.CancellableProcessExecutorWithTimeout(3 * time.Second)
			directory, branch, err := snapshot.SessionWorkingContext(sessionID, executor)
			return client.DaemonPair[string, string]{First: directory, Second: branch}, err
		},
		func(working client.DaemonPair[string, string], err error) dashboardIoUpdate {
			return dashboardIoUpdateGoContext{sessionID: reportID, result: daemonResultOf(working, err)}
		},
	)
}

// refreshGitStatus reads the checkout of the visible sessions that are due,
// a couple per pass so a workspace full of sessions never floods a slow
// target.
func (c *dashboardContext) refreshGitStatus() {
	if c.shutdownRequestedAt != nil {
		return
	}
	for _, sessionID := range c.dashboard.GitProbeCandidates(time.Now(), 2) {
		c.spawnGitProbe(sessionID)
	}
}

func (c *dashboardContext) spawnGitProbe(sessionID string) {
	if _, inFlight := c.gitProbesInFlight[sessionID]; inFlight {
		return
	}
	c.gitProbesInFlight[sessionID] = struct{}{}
	reportID := sessionID
	snapshot := c.liveControllerSnapshot()
	spawnIO(
		"reading session git status",
		c.dashboardIoTx,
		func() (core.SessionGitStatus, error) {
			executor := core.CancellableProcessExecutorWithTimeout(8 * time.Second)
			return snapshot.SessionGitStatus(sessionID, executor)
		},
		func(status core.SessionGitStatus, err error) dashboardIoUpdate {
			return dashboardIoUpdateGitStatus{sessionID: reportID, result: daemonResultOf(status, err)}
		},
	)
}

// liveControllerSnapshot is the sessions as the runtime feed last delivered
// them, for work that runs off the event loop. The maps are persistent, so
// this is cheap, and it never opens the store, which only the daemon may
// read.
func (c *dashboardContext) liveControllerSnapshot() controller.Controller {
	return controllerSnapshot(&c.controller)
}

// controllerSnapshot is Rust's `Controller { config: clone, state: clone }`
// for work off the event loop. The state's record maps are persistent
// snapshots; its two plain maps are copied so the loop can replace their
// entries while a background read holds the snapshot. The loop replaces the
// config as a whole and never edits it in place.
func controllerSnapshot(ctrl *controller.Controller) controller.Controller {
	state := ctrl.State
	state.LastSubagentPolicies = maps.Clone(state.LastSubagentPolicies)
	state.LastOracleChoices = maps.Clone(state.LastOracleChoices)
	return controller.Controller{Config: ctrl.Config, State: state}
}

func (c *dashboardContext) rememberGoSelection() {
	if c.goSelectionInFlight {
		return
	}
	goMode := c.dashboard.GoMode()
	if goMode == nil {
		return
	}
	sessionID, ok := c.dashboard.SelectedSessionID()
	if !ok {
		return
	}
	if c.goSelectionRequested != nil && *c.goSelectionRequested == sessionID {
		return
	}
	if goMode.WorkspaceID == nil {
		return
	}
	workspaceID := *goMode.WorkspaceID
	if active, ok := c.dashboard.ActiveWorkspaceID(); !ok || active != workspaceID {
		return
	}
	directory := goMode.Directory
	requested := sessionID
	c.goSelectionRequested = &requested
	c.goSelectionInFlight = true
	spawnCriticalIO(
		c.criticalOperations,
		"remembering conversation",
		c.dashboardIoTx,
		func() (struct{}, error) {
			return struct{}{}, core.GoPreferencesRememberSession(core.GoPreferencesPath(), directory, workspaceID, sessionID)
		},
		func(_ struct{}, err error) dashboardIoUpdate {
			return dashboardIoUpdateGoSelectionSaved{err: err}
		},
	)
}

// maybeOpenStartupSession opens the conversation the surface should start
// on, once the summaries it compares have arrived or the wait for them has
// run out. Reports whether the pick ran, which happens at most once.
func (c *dashboardContext) maybeOpenStartupSession() bool {
	if !c.startup.ready(time.Now()) {
		return false
	}
	// A restored arrangement already says which conversation belongs in
	// which pane. Open each one where it belongs instead of picking a
	// conversation to start on.
	restored := c.dashboard.PaneSessions()
	if len(restored) != 0 {
		focused := c.dashboard.FocusedPane()
		c.dashboard.FocusPrompt()
		for _, entry := range restored {
			if c.dashboard.SessionFailed(entry.SessionID) {
				// The band says the session failed; attaching would wait on
				// a worker that is gone.
				continue
			}
			c.openChatSessionInto(entry.Pane, entry.SessionID)
		}
		c.dashboard.FocusPane(focused)
		c.syncOpeningSession()
		return true
	}
	var candidates []*core.SessionRecord
	for session := range c.controller.State.Sessions.Values() {
		if c.dashboard.TransitionKind(session.ID) == nil && c.dashboard.TransitionFailureKind(session.ID) == nil {
			candidates = append(candidates, session)
		}
	}
	var workspaceID *string
	if active, ok := c.dashboard.ActiveWorkspaceID(); ok {
		workspaceID = &active
	}
	sessionID := startupSessionChoice(workspaceID, candidates, c.dashboard.SessionActivityAtMs)
	if sessionID == nil {
		c.dashboard.FocusSessions()
		return true
	}
	c.dashboard.FocusPrompt()
	c.openChatSession(*sessionID)
	return true
}

func (c *dashboardContext) resolveProjectSources() {
	var sessionIDs []string
	for session := range c.controller.State.Sessions.Values() {
		if !session.State.IsActive() {
			continue
		}
		if owns, known := c.controller.State.OwnsEnvironment(session.ID); !known || !owns {
			continue
		}
		checkout, err := c.controller.State.Checkout(session.ID)
		if err != nil {
			continue
		}
		if _, ok := checkout.ProjectDirectory(); !ok {
			continue
		}
		if c.dashboard.HasResolvedProjectSource(session.ID) {
			continue
		}
		if _, inFlight := c.projectSourcesInFlight[session.ID]; inFlight {
			continue
		}
		sessionIDs = append(sessionIDs, session.ID)
	}
	for _, sessionID := range sessionIDs {
		c.projectSourcesInFlight[sessionID] = struct{}{}
		sessionName := c.sessionNoticeName(sessionID)
		spawnProjectSourceResolution(sessionID, sessionName, c.dashboardIoTx, c.criticalOperations)
	}
}

// finishMaterializedProjection: detail is the prepared projection, nil when
// err reports why it could not be prepared.
func (c *dashboardContext) finishMaterializedProjection(
	sessionID string,
	detail *tui.PreparedMaterializedSessionDetail,
	err error,
) {
	delete(c.materializedProjectionsInFlight, sessionID)
	if err == nil {
		if c.dashboard.ApplyPreparedMaterializedSession(*detail) {
			// Only a projection that passed the dashboard's ordinal guard may
			// answer whether a cached request is still pending. Startup
			// summaries deliberately do not do so.
			c.reconcileQuestionDrafts()
		}
	} else {
		c.dashboard.SetNotice(fmt.Sprintf("Could not update session transcript: %s", err.Error()))
	}
	if queued, ok := c.pendingMaterializedProjections[sessionID]; ok {
		delete(c.pendingMaterializedProjections, sessionID)
		c.requestMaterializedProjection(queued.projection, queued.viewedThroughEventOrdinal)
	}
}

// attachmentFor is Rust's `attachments.entry(session).or_default()`.
func (c *dashboardContext) attachmentFor(sessionID string) *sessionAttachment {
	attachment, ok := c.attachments[sessionID]
	if !ok {
		attachment = newSessionAttachment()
		c.attachments[sessionID] = attachment
	}
	return attachment
}
