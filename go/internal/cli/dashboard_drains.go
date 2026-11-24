// Port of mj-cli/src/dashboard/drains.rs.
//
// The dashboard loop's drains: each applies what one background feed has
// queued since the last pass.

package cli

import (
	"log/slog"
	"maps"
	"reflect"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// remoteLifecycleObservation: operation identity and the notice already
// reported belong to one observation.
type remoteLifecycleObservation struct {
	operationID string
	// notice is nil when the daemon reported none.
	notice *string
}

// observeRemoteLifecycle records lifecycle as the observed operation for
// its session and reports whether it supersedes an observed one.
func observeRemoteLifecycle(
	operations map[string]remoteLifecycleObservation,
	dashboard *tui.DashboardState,
	lifecycle *client.RuntimeLifecycleView,
) bool {
	previous, hadPrevious := operations[lifecycle.SessionID]
	var notice *string
	if lifecycle.Notice != nil {
		text := *lifecycle.Notice
		notice = &text
	}
	operations[lifecycle.SessionID] = remoteLifecycleObservation{
		operationID: lifecycle.OperationID,
		notice:      notice,
	}
	supersedes := hadPrevious && previous.operationID != lifecycle.OperationID
	if notice != nil && (supersedes || !hadPrevious || previous.notice == nil || *previous.notice != *notice) {
		dashboard.SetNotice(*notice)
	}
	return supersedes
}

// lifecycleCompletion is Rust's LifecycleCompletion: Current, carrying the
// local operation the completion belongs to (nil when none was waiting), or
// Superseded.
type lifecycleCompletion struct {
	operation  *activeLifecycleOperation
	superseded bool
}

// takeLifecycleCompletion removes and returns the local operation update
// completes, unless a newer local operation for the session has replaced it.
func takeLifecycleCompletion(
	operations map[string]*activeLifecycleOperation,
	update *dashboardLifecycleUpdate,
) lifecycleCompletion {
	current, ok := operations[update.sessionID]
	if !ok {
		return lifecycleCompletion{}
	}
	if current.cancelled != update.operation {
		return lifecycleCompletion{superseded: true}
	}
	delete(operations, update.sessionID)
	return lifecycleCompletion{operation: current}
}

func (c *dashboardContext) cancelBackgroundWork() {
	if task := c.wikiSearchTask; task != nil {
		task.cancel()
		c.wikiSearchTask = nil
	}
	if task := c.wikiSyncTask; task != nil {
		task.cancel()
		c.wikiSyncTask = nil
	}
	if task := c.sessionsSyncTask; task != nil {
		task.cancel()
		c.sessionsSyncTask = nil
	}
	if cancel := c.sessionsTextTask; cancel != nil {
		cancel()
		c.sessionsTextTask = nil
	}
	c.usageReport.cancel()
	c.cancelChatOpen()
	c.criticalOperations.CancelAll()
	c.cancelSessionPreflight()
	if active := c.activeImport; active != nil {
		active.cancelled.Store(true)
	}
}

// drainFeeds takes every message queued behind the one that woke the loop,
// feed by feed, in the order the UI depends on. It applies whatever the
// background feeds have queued, and reports whether any message arrived.
// The report is what lets a timer wakeup that had nothing of its own to show
// still draw the message that rode with it.
func (c *dashboardContext) drainFeeds() bool {
	c.cancelStalePathInput()
	c.drainQuotaUpdates()
	c.drainRuntimeState()
	for {
		batch, ok := c.runtimeAlerts.NextReady()
		if !ok {
			break
		}
		c.dashboard.SetAlertEvents(batch.Sequence, batch.Events)
	}
	for {
		health, ok := c.runtimeHealth.NextReady()
		if !ok {
			break
		}
		c.notices.SetPersistentFailure(health.Error())
	}
	c.drainWorkerUpdates()
	c.drainRuntimeReviews()
	c.drainRuntimeNotices()
	c.drainUIRequests()
	c.drainRuntimeConfig()
	for {
		capabilities, ok := c.profileCapabilities.NextReady()
		if !ok {
			break
		}
		c.dashboard.SetProfileCapabilities(capabilities)
	}
	c.drainImportProfiles()
	c.drainImportTasks()
	c.drainLifecycleUpdates()
	for _, origin := range c.dashboard.MovePromptOrigins() {
		c.startupPrompts.observeMove(origin.SessionID, origin.OperationID, origin.Terminal)
	}
	c.reconcileComposerOwnership()
	c.startupPrompts.flushHeld(&c.controller.State, c.dashboardIoTx, c.criticalOperations)
	composerCompleted := c.collectPreparedStartupPrompts()
	c.startupPrompts.poll(c.dashboardIoTx)
	pendingCompleted := c.drainDashboardIo()
	// Collected rather than short-circuited: every feed's flag has to be
	// cleared for the next drain.
	delivered := []bool{
		pendingCompleted,
		composerCompleted,
		c.quota.TakeDelivered(),
		c.profileCapabilities.TakeDelivered(),
		c.worker.TakeDelivered(),
		c.runtimeState.TakeDelivered(),
		c.runtimeAlerts.TakeDelivered(),
		c.runtimeHealth.TakeDelivered(),
		c.runtimeReviews.TakeDelivered(),
		c.runtimeNotices.TakeDelivered(),
		c.runtimeUIRequests.TakeDelivered(),
		c.runtimeConfig.TakeDelivered(),
		c.lifecycle.TakeDelivered(),
		c.importProfiles.TakeDelivered(),
		c.importTasks.TakeDelivered(),
		c.dashboardIo.TakeDelivered(),
	}
	return slices.Contains(delivered, true)
}

// reconcileComposerOwnership: worker feeds may remain warm while one live
// composer belongs to standby.
func (c *dashboardContext) reconcileComposerOwnership() {
	for _, sessionID := range slices.Sorted(maps.Keys(c.chats)) {
		conversation := c.chats[sessionID]
		kind := c.dashboard.TransitionKind(sessionID)
		parked := kind != nil && tui.DashboardStateTransitionAcceptsPrompts(*kind)
		hasStandby := c.dashboard.HasStandbyComposer(sessionID)
		if parked && !hasStandby {
			if composer := conversation.TakeLiveComposer(); composer != nil {
				c.dashboard.AdoptStandbyComposer(sessionID, composer)
			}
		} else if !parked && hasStandby && conversation.SessionFeedOpen() && !c.sessionIsOpening(sessionID) {
			if composer := c.dashboard.TakeStandbyComposer(sessionID); composer != nil {
				conversation.AdoptLiveComposer(*composer)
				conversation.KeepDeferredSubmissionAsDraft()
			}
		}
	}
	for _, sessionID := range slices.Sorted(maps.Keys(c.chats)) {
		conversation := c.chats[sessionID]
		if !conversation.HasLiveComposer() {
			continue
		}
		if generation, id, succeeded, ok := c.startupPrompts.terminalMove(sessionID); ok {
			conversation.SettleDeferredMoveOrigin(generation, id, succeeded)
		}
	}
	// Rust extends the pane sessions' BTreeSet, so each session is visited
	// once, in order.
	sessions := c.dashboard.PaneSessionIDs()
	for _, sessionID := range c.dashboard.StandbySessionIDs() {
		sessions[sessionID] = struct{}{}
	}
	for _, sessionID := range slices.Sorted(maps.Keys(sessions)) {
		kind := c.dashboard.TransitionKind(sessionID)
		if c.dashboard.HasStandbyComposer(sessionID) || (kind != nil && *kind == core.SessionTransitionKindMoving) {
			c.dashboard.SetStandbyMoveOrigin(sessionID, c.standbyMoveOrigin(sessionID))
			if generation, id, succeeded, ok := c.startupPrompts.terminalMove(sessionID); ok {
				c.dashboard.SettleStandbyMoveOrigin(sessionID, generation, id, succeeded)
			}
		}
	}
}

// sessionIsOpening reports whether any pane is attaching to sessionID.
func (c *dashboardContext) sessionIsOpening(sessionID string) bool {
	for _, opening := range c.openingChatSessions {
		if opening == sessionID {
			return true
		}
	}
	return false
}

// standbyMoveOrigin is the startup prompts' move origin for a session as
// the standby composer takes it; nil when none is known.
func (c *dashboardContext) standbyMoveOrigin(sessionID string) *chat.ComposerMoveOrigin {
	generation, id, ok := c.startupPrompts.moveOrigin(sessionID)
	if !ok {
		return nil
	}
	return &chat.ComposerMoveOrigin{Generation: generation, ID: id}
}

func (c *dashboardContext) collectPreparedStartupPrompts() bool {
	changed, actions := c.dashboard.DrainStandbyComposers()
	bridges := c.composerDaemonRequests
	updates := c.dashboardIoTx
	notices := c.notices
	if c.dashboard.ServiceNotificationComposers(c.workerCommandsTx.Client(), func(sessionID string) chat.ChatDaemonSender {
		return chatDaemonRequests(bridges, sessionID, updates, notices)
	}) {
		changed = true
	}
	for _, action := range actions {
		if queue, ok := action.(tui.DashboardActionQueueStartupPrompt); ok {
			c.startupPrompts.enqueue(
				namedSession{name: sessionNoticeName(&c.controller.State, queue.SessionID), id: queue.SessionID},
				queue.Prompt,
				nil,
				c.dashboardIoTx,
				c.criticalOperations,
			)
		}
	}
	for _, sessionID := range slices.Sorted(maps.Keys(c.chats)) {
		conversation := c.chats[sessionID]
		prepared := conversation.TakePreparedStartupPrompts()
		if prompt, ok := conversation.DrainComposer(); ok {
			prepared = append(prepared, prompt)
		}
		if len(prepared) != 0 {
			changed = true
		}
		for _, prompt := range prepared {
			c.startupPrompts.enqueue(
				namedSession{name: sessionNoticeName(&c.controller.State, sessionID), id: sessionID},
				prompt,
				nil,
				c.dashboardIoTx,
				c.criticalOperations,
			)
		}
	}
	return changed
}

// clockTickRedraws is whether the once-a-second clock has anything new to
// put on screen.
//
// A notice is the only report several background failures get, and any task
// can write it through the shared slot without waking this loop, so the slot
// is compared with what the frame on screen was drawn from.
func (c *dashboardContext) clockTickRedraws() bool {
	if c.dashboard.TickReviewAutoconfigure() || c.dashboard.ClockChanged() {
		return true
	}
	if c.notices.Generation() != c.drawnNoticeGeneration {
		return true
	}
	visible := c.visibleChat()
	return visible != nil && visible.ClockChanged()
}

// drainRuntimeReviews hands the open chat whatever turn review the daemon
// is running for it.
//
// The chat draws the pane and sends the resolutions; it hosts nothing. A
// session with no review gets none, which closes the pane.
func (c *dashboardContext) drainRuntimeReviews() {
	var latest []client.RuntimeReviewView
	arrived := false
	for {
		reviews, ok := c.runtimeReviews.NextReady()
		if !ok {
			break
		}
		latest, arrived = reviews, true
	}
	if !arrived {
		return
	}
	c.runtimeReviewViews = map[string][]client.RuntimeReviewView{}
	for _, review := range latest {
		c.runtimeReviewViews[review.SessionID] = append(c.runtimeReviewViews[review.SessionID], review)
	}
	var all []client.RuntimeReviewView
	for _, sessionID := range slices.Sorted(maps.Keys(c.runtimeReviewViews)) {
		all = append(all, c.runtimeReviewViews[sessionID]...)
	}
	c.dashboard.SetSessionReviews(all)
	c.applyRuntimeReviewsToChats()
}

// drainRuntimeNotices reports each background notice the daemon published
// since the last one this surface showed.
func (c *dashboardContext) drainRuntimeNotices() {
	var latest []client.RuntimeNotice
	arrived := false
	for {
		notices, ok := c.runtimeNotices.NextReady()
		if !ok {
			break
		}
		latest, arrived = notices, true
	}
	if !arrived {
		return
	}
	for _, notice := range latest {
		if c.reportedNoticeID != nil && notice.ID <= *c.reportedNoticeID {
			continue
		}
		id := notice.ID
		c.reportedNoticeID = &id
		text := c.dashboard.NoticeNamingWorkspace(notice.SessionID, notice.Text)
		c.dashboard.SetNotice(text)
	}
}

// drainUIRequests applies each dashboard request published since the last
// one applied.
func (c *dashboardContext) drainUIRequests() {
	var latest []client.UiRequest
	arrived := false
	for {
		requests, ok := c.runtimeUIRequests.NextReady()
		if !ok {
			break
		}
		latest, arrived = requests, true
	}
	if !arrived {
		return
	}
	for _, request := range c.uiRequestCursor.TakeNew(latest) {
		action := c.dashboard.ApplyUIRequest(*request)
		if _, none := action.(tui.DashboardActionNone); !none {
			c.uiRequestActions = append(c.uiRequestActions, action)
		}
	}
}

// applyRuntimeReviewsToChats applies the retained daemon projection to
// every warm chat. Absence is meaningful: it closes a review the previous
// projection held.
func (c *dashboardContext) applyRuntimeReviewsToChats() {
	for _, sessionID := range slices.Sorted(maps.Keys(c.chats)) {
		conversation := c.chats[sessionID]
		reviews := slices.Clone(c.runtimeReviewViews[conversation.SessionID()])
		conversation.ApplyReviewViews(reviews)
	}
}

// applyRuntimeReviewToChat is the same for one conversation, for a chat
// that has just opened.
func (c *dashboardContext) applyRuntimeReviewToChat(sessionID string) {
	conversation, ok := c.chats[sessionID]
	if !ok {
		return
	}
	conversation.ApplyReviewViews(slices.Clone(c.runtimeReviewViews[sessionID]))
}

func (c *dashboardContext) drainQuotaUpdates() {
	var latest client.QuotaSnapshot
	arrived := false
	for {
		snapshot, ok := c.quota.NextReady()
		if !ok {
			break
		}
		latest, arrived = snapshot, true
	}
	if !arrived {
		return
	}
	cycles := latest.Cycles
	c.quotaCycles = cycles
	c.dashboard.SetQuotaSnapshot(latest)
	if daemon.CompleteManualQuotaRefresh(&c.manualQuotaRefreshCycles, cycles) {
		c.dashboard.ReplaceNoticeIf(quotaRefreshNotice, quotaRefreshedNotice)
	}
}

func (c *dashboardContext) drainWorkerUpdates() {
	for {
		update, ok := c.worker.NextReady()
		if !ok {
			break
		}
		sessionID := update.SessionID
		record := c.controller.State.Sessions.GetShared(sessionID)
		if record == nil || !sessionruntime.SessionTargetIsPollable(record) {
			continue
		}
		connected := update.View.Connected
		applyWorkerActivity(c.dashboard, c.chats, sessionID, &update.View)
		// Only unreachable relays drive the worker diagnostics flow.
		var connectionError *string
		if failure := update.View.Error; failure != nil && failure.Kind == client.ViewErrorKindUnreachable {
			detail := failure.Message
			connectionError = &detail
		}
		if materialized := applyWorkerPollUpdate(c.dashboard, update); materialized != nil {
			var viewedThroughEventOrdinal uint64
			if session := c.controller.State.Sessions.GetShared(sessionID); session != nil {
				viewedThroughEventOrdinal = session.ViewedThroughEventOrdinal
			}
			c.requestMaterializedProjection(*materialized, viewedThroughEventOrdinal)
		}
		if episodeID, ok := c.workerDiagnoses.Observe(sessionID, connected, connectionError); ok {
			spawnWorkerDiagnosis(&c.controller, sessionID, episodeID, c.dashboardIoTx, c.criticalOperations)
		}
	}
}

func (c *dashboardContext) drainRuntimeState() {
	var update daemon.RuntimeStateUpdate
	arrived := false
	for {
		latest, ok := c.runtimeState.NextReady()
		if !ok {
			break
		}
		update, arrived = latest, true
	}
	if !arrived {
		return
	}
	if !maps.Equal(update.SubagentParents, c.runtimeSubagentParents.Borrow()) {
		return
	}
	if notice, ok := c.dashboard.Notice(); ok && (notice == "Loading sub-agents…" || notice == "Loading reviewers…") {
		c.dashboard.ClearNotice()
	}
	c.autoAfk.configure(c.controller.Config.Advanced.AutoAfkMinutes > 0, update.OperatorPresence.Away)
	// The serialized runtime feed owns ordering through incarnation-scoped
	// cursors. A replacement daemon's revision can start below the old one.
	var removedLayouts []string
	for _, id := range slices.Sorted(maps.Keys(c.knownWorkspaceLayouts)) {
		if _, ok := update.WorkspaceNames[id]; !ok {
			removedLayouts = append(removedLayouts, id)
		}
	}
	for _, id := range removedLayouts {
		// The legacy default is also hidden when merely empty, so its
		// explicit deletion response owns composer disposal.
		if id != core.DefaultWorkspaceID {
			c.discardWorkspaceComposers(id)
		}
		c.paneSizePersistence.forget(id)
		c.layoutPersistence.forget(id)
		delete(c.workspaceLayouts, id)
		delete(c.knownWorkspaceLayouts, id)
	}
	var newLayouts []string
	for _, id := range slices.Sorted(maps.Keys(update.WorkspaceNames)) {
		if _, known := c.knownWorkspaceLayouts[id]; !known {
			newLayouts = append(newLayouts, id)
		}
	}
	if len(newLayouts) != 0 {
		for _, id := range newLayouts {
			c.knownWorkspaceLayouts[id] = struct{}{}
		}
		spawnWorkspacePaneSizesLoad(slices.Clone(newLayouts), c.dashboardIoTx)
		spawnWorkspaceLayoutsLoad(newLayouts, c.dashboardIoTx)
	}
	var active *string
	if id, ok := c.dashboard.ActiveWorkspaceID(); ok {
		active = &id
	}
	next := nextWorkspace(active, &c.pendingWorkspaceSelection, update.WorkspaceNames)
	c.dashboard.SetWorkspaceNames(update.WorkspaceNames)
	c.dashboard.SetOperatorPresence(update.OperatorPresence)
	c.dashboard.SetLaunchRecency(update.LaunchRecency)
	c.dashboard.SetSessionAttention(update.SessionAttention)
	c.dashboard.SetFormHolders(update.FormHolders)
	c.dashboard.SetNotificationCards(update.NotificationCards)
	c.dashboard.SetUnreadFrontiers(update.UnreadFrontiers)
	c.dashboard.SetNotificationDismissals(update.NotificationDismissals)
	c.dashboard.SetRecentLaunch(update.RecentLaunch)
	c.dashboard.SetContainerUpgradeSessions(update.ContainerUpgradeSessions)
	c.dashboard.SetFailedLaunchRetries(update.FailedLaunchRetries)
	c.dashboard.SetHostCapacities(update.HostCapacities)
	c.dashboard.SetTargetStorage(update.Storage)
	c.dashboard.SetContainerCapacities(update.ContainerCapacities)
	c.selectWorkspace(next)
	c.dashboard.SetSessionVerdicts(update.Verdicts)
	c.applyRuntimeRecords(
		update.Records,
		update.Sessions,
		update.Subagents,
		update.EnvironmentOwners,
		update.LastSubagentPolicies,
	)
	c.dashboard.SetNativeAgentSnapshot(update.NativeAgents)
	c.dashboard.SetSessionCPU(update.SessionCPU)
	c.dashboard.SetMoveSnapshot(update.Moves)
	c.applyRuntimeLifecycles(update.Lifecycles)
	refreshSubagentCounts(c.chats, c.dashboard)
}

func (c *dashboardContext) applyRuntimeLifecycles(lifecycles []client.RuntimeLifecycleView) {
	active := map[string]struct{}{}
	for _, lifecycle := range lifecycles {
		active[lifecycle.SessionID] = struct{}{}
	}
	for _, sessionID := range slices.Sorted(maps.Keys(c.remoteLifecycleOperations)) {
		if _, stillActive := active[sessionID]; stillActive {
			continue
		}
		moveActive := c.dashboard.MoveOperationActive(sessionID)
		if _, local := c.lifecycleOperations[sessionID]; !local && !moveActive {
			c.dashboard.FinishSessionOperation(sessionID)
		}
		delete(c.remoteLifecycleOperations, sessionID)
	}
	for index := range lifecycles {
		lifecycle := &lifecycles[index]
		if id := lifecycle.MoveOperationID; id != nil {
			c.startupPrompts.observeMove(lifecycle.SessionID, *id, nil)
		}
		kind := lifecycleKind(lifecycle.Kind)
		markActiveChatRetiringForRemoteLifecycle(c.chats[lifecycle.SessionID], lifecycle.SessionID, kind)
		// Keep the daemon identity even while a local action is waiting for
		// its callback. The callback is keyed only by session id, so this
		// lets it distinguish an authoritative newer operation.
		supersedes := observeRemoteLifecycle(c.remoteLifecycleOperations, c.dashboard, lifecycle)
		if _, local := c.lifecycleOperations[lifecycle.SessionID]; !local {
			if supersedes {
				// A new daemon operation for the same session supersedes the
				// old overlay even when both snapshots use the same lifecycle
				// kind.
				c.dashboard.FinishSessionOperation(lifecycle.SessionID)
			}
			applyLifecycleDisplay(c.dashboard, lifecycle)
		} else {
			operationID := lifecycle.OperationID
			c.dashboard.SetSessionOperationIdentity(lifecycle.SessionID, &operationID, lifecycle.Cancellable)
			stages := make([]tui.ProvisionStageStart, 0, len(lifecycle.ActiveStages))
			for _, stage := range lifecycle.ActiveStages {
				stages = append(stages, tui.ProvisionStageStart{Stage: stage.First, StartedAt: stage.Second})
			}
			c.dashboard.ReplaceSessionOperationStages(lifecycle.SessionID, stages)
			if destination := lifecycle.ResumeDestination; destination != nil {
				c.dashboard.SetResumeDestination(lifecycle.SessionID, destination[0], destination[1])
			}
		}
	}
}

// refreshChatContext hands the open conversation the surface's current
// view of the config and its own session record. The chat snapshots both
// when it opens, so without this a long-lived chat would keep offering
// reviewer profiles that a config reload has since renamed or removed.
func (c *dashboardContext) refreshChatContext() {
	refreshOpenChats(c.chats, &c.controller, c.dashboard, c.runtimeSessions)
}

func (c *dashboardContext) drainRuntimeConfig() {
	var latest daemon.RuntimeLaunchConfig
	arrived := false
	for {
		config, ok := c.runtimeConfig.NextReady()
		if !ok {
			break
		}
		latest, arrived = config, true
	}
	if !arrived {
		return
	}
	config := latest.Config
	c.dashboard.SetRuntimeConfig(config, latest.Availability)
	c.autoAfk.setEnabled(config.Advanced.AutoAfkMinutes > 0)
	// The chat's copy of `[review]` follows the daemon's, so `/review
	// status` and the composer's armed indicator report what is actually
	// running rather than what this process last read from disk.
	for _, sessionID := range slices.Sorted(maps.Keys(c.chats)) {
		c.chats[sessionID].SetReviewConfig(config.Review)
	}
	if reflect.DeepEqual(config, c.controller.Config) {
		return
	}
	// A save from any surface reaches every terminal here, so this is where
	// a changed `[daemon]` setting is carried out.
	daemonChanged := !reflect.DeepEqual(config.Daemon, c.controller.Config.Daemon)
	c.controller.Config = config
	c.refreshChatContext()
	if daemonChanged {
		spawnDaemonPlacement(c)
	}
	// A profile added or changed here is probed once by the daemon, which
	// reloads the same configuration; this process asks no provider. Config
	// and session records have one owner: the runtime feed. Reloading a
	// Controller from disk here could roll back a newer feed.
}

func (c *dashboardContext) applyRuntimeRecords(
	records core.SnapshotMap[string, core.SessionRecord],
	runtimeSessions core.SnapshotMap[string, client.RuntimeSessionView],
	subagents core.SnapshotMap[string, core.SubagentMetadata],
	environmentOwners core.SnapshotMap[string, string],
	lastSubagentPolicies map[string]core.SubagentPolicy,
) {
	sessions := records
	preserveReadPositions(&sessions, c.controller.State.Sessions)
	// Live views change on nearly every feed update; chats read only their
	// own session's policy keys, and ignore an unchanged list.
	c.runtimeSessions = runtimeSessions
	chatIDs := slices.Sorted(maps.Keys(c.chats))
	for _, sessionID := range chatIDs {
		applyPolicyConfigKey(c.chats[sessionID], c.runtimeSessions)
	}
	for _, sessionID := range chatIDs {
		conversation := c.chats[sessionID]
		record := sessions.GetShared(conversation.SessionID())
		feedExpected := record != nil && sessionruntime.SessionTargetIsPollable(record)
		// A session that is no longer pollable, or that any surface has
		// asked to stop or destroy, is being retired on purpose: its feed
		// will close and the chat must not chase a replacement actor.
		retiring := !feedExpected || c.dashboard.TransitionFailureKind(conversation.SessionID()) != nil
		if kind, ok := c.dashboard.SessionOperationKind(conversation.SessionID()); ok {
			switch kind {
			case tui.SessionOperationKindSuspending, tui.SessionOperationKindDestroying,
				tui.SessionOperationKindStopping, tui.SessionOperationKindMoving:
				retiring = true
			}
		}
		conversation.SetSessionRetiring(retiring)
		// Order matters: an expected feed clears the retiring flag.
		conversation.SetSessionFeedExpected(feedExpected)
	}
	if c.controller.State.Sessions.Equal(sessions, recordEqual[core.SessionRecord]) &&
		c.controller.State.Subagents.Equal(subagents, recordEqual[core.SubagentMetadata]) &&
		c.controller.State.EnvironmentOwners.Equal(environmentOwners, recordEqual[string]) &&
		maps.EqualFunc(c.controller.State.LastSubagentPolicies, lastSubagentPolicies, func(a, b core.SubagentPolicy) bool {
			return reflect.DeepEqual(a, b)
		}) {
		return
	}
	var retiredIDs []string
	for id, record := range c.controller.State.Sessions.Changes(sessions, recordEqual[core.SessionRecord]) {
		if record == nil || !sessionruntime.SessionTargetIsPollable(record) {
			retiredIDs = append(retiredIDs, id)
		}
		last := c.controller.State.Sessions.GetShared(id)
		if _, chatOpen := c.chats[id]; record == nil && last != nil && chatOpen {
			c.departedRecords[id] = *last
		} else {
			delete(c.departedRecords, id)
		}
	}
	for id := range c.departedRecords {
		if _, chatOpen := c.chats[id]; !chatOpen {
			delete(c.departedRecords, id)
		}
	}
	c.controller.State.Sessions = sessions
	c.controller.State.Subagents = subagents
	c.controller.State.EnvironmentOwners = environmentOwners
	c.controller.State.LastSubagentPolicies = lastSubagentPolicies
	c.dashboard.SetState(c.controller.State)
	for _, id := range retiredIDs {
		c.workerDiagnoses.Observe(id, true, nil)
	}
	c.finishSessionsStoppedBySuspend()
	c.reconcileQuestionDrafts()
	c.refreshChatContext()
	c.controllerChanged = true
}

// recordEqual is a record's derived PartialEq, as tui compares records.
func recordEqual[V any](a, b *V) bool { return reflect.DeepEqual(a, b) }

func (c *dashboardContext) drainImportProfiles() {
	for {
		update, ok := c.importProfiles.NextReady()
		if !ok {
			break
		}
		c.dashboard.ApplyResumeProfile(update.discoveryID, update.option)
	}
}

func (c *dashboardContext) drainImportTasks() {
	for {
		update, ok := c.importTasks.NextReady()
		if !ok {
			break
		}
		switch update := update.(type) {
		case dashboardImportUpdateUploading:
			if c.activeImport != nil && c.activeImport.taskID == update.taskID {
				c.dashboard.ShowImportUpload()
			}
		case dashboardImportUpdateRegistering:
			if c.activeImport != nil && c.activeImport.taskID == update.taskID {
				c.dashboard.ShowImportRegistration()
			}
		case dashboardImportUpdateProgress:
			if c.activeImport != nil && c.activeImport.taskID == update.taskID {
				c.dashboard.UpdateImportProgress(update.step, update.total, update.message)
			}
		case dashboardImportUpdateFinished:
			if c.activeImport == nil || c.activeImport.taskID != update.taskID {
				continue
			}
			c.activeImport = nil
			if update.err != nil {
				c.dashboard.FinishImport()
				slog.Error("Session import failed", "error", update.err.Error())
				c.dashboard.ShowImportFailure(update.err.Error())
				continue
			}
			switch result := update.result.(type) {
			case dashboardImportTaskResultNeedsBundle:
				pending := update.pending
				c.pendingImport = &pending
				c.dashboard.ShowImportBundleConfirmation(
					result.prompt.dirtyGitRoots,
					result.prompt.omittedNonGitDirs,
					result.prompt.scratchGitRoots,
					result.prompt.managedWorktree,
					result.prompt.recordedRoot,
				)
			case dashboardImportTaskResultImported:
				c.dashboard.FinishImport()
				c.dashboard.SetNotice("Saving imported session…")
				spawnImportedSessionApply(*result.success, update.pending, c.dashboardIoTx, c.criticalOperations)
			case dashboardImportTaskResultCancelled:
				c.dashboard.FinishImport()
				c.dashboard.SetNotice("Import cancelled; no Mjolnir files were changed.")
			}
		}
	}
}

func (c *dashboardContext) drainLifecycleUpdates() {
	for {
		update, ok := c.lifecycle.NextReady()
		if !ok {
			break
		}
		c.controllerChanged = true
		sessionID := update.sessionID
		completion := takeLifecycleCompletion(c.lifecycleOperations, &update)
		if completion.superseded {
			if update.failure != nil {
				slog.Warn("superseded lifecycle operation failed", "session_id", sessionID, "error", update.failure.Error())
			}
			continue
		}
		// Local operation identity cannot retire a different daemon
		// operation. If a newer daemon operation is already present in the
		// coherent runtime snapshot, retain its overlay; the next snapshot
		// that removes that operation will settle it. This prevents an old
		// local callback from hiding an authoritative newer operation.
		if _, remote := c.remoteLifecycleOperations[sessionID]; !remote && !c.dashboard.MoveOperationActive(sessionID) {
			c.dashboard.FinishSessionOperation(sessionID)
		}
		spawnLifecycleReload(lifecycleReload{update: update, operation: completion.operation}, c.dashboardIoTx)
	}
}

func (c *dashboardContext) drainDashboardIo() bool {
	completed := false
	pending := c.pendingRuntimeUpdates
	c.pendingRuntimeUpdates = nil
	for _, waiting := range pending {
		if !awaitingRuntime(waiting.update, &c.controller) {
			completed = true
			c.applyDashboardIoUpdate(waiting.update)
		} else if !time.Now().Before(waiting.deadline) {
			completed = true
			c.applyDashboardIoUpdate(runtimeWaitExpired(waiting.update))
		} else {
			c.pendingRuntimeUpdates = append(c.pendingRuntimeUpdates, waiting)
		}
	}
	for {
		update, ok := c.dashboardIo.NextReady()
		if !ok {
			break
		}
		c.controllerChanged = true
		c.applyDashboardIoUpdate(update)
	}
	return completed
}

// refreshOpenChats hands every open conversation the controller's current
// config and its own session record, so its header names the session the
// way the Sessions row does (`listed_title`) and its other context matches
// the dashboard's.
func refreshOpenChats(
	chats map[string]*chat.ActiveChat,
	ctrl *controller.Controller,
	dashboard *tui.DashboardState,
	runtimeSessions core.SnapshotMap[string, client.RuntimeSessionView],
) {
	for _, sessionID := range slices.Sorted(maps.Keys(chats)) {
		conversation := chats[sessionID]
		applyPolicyConfigKey(conversation, runtimeSessions)
		record := ctrl.State.Sessions.GetShared(conversation.SessionID())
		var project *core.SessionRecord
		if record != nil {
			project = ctrl.State.ProjectIdentitySession(record)
		}
		conversation.RefreshContext(&ctrl.Config, record, project)
		if dashboard.GoMode() != nil {
			conversation.SetDisplayTitle(dashboard.GoConversationTitle(conversation.SessionID()))
		}
	}
	refreshSubagentCounts(chats, dashboard)
}

func applyPolicyConfigKey(
	conversation *chat.ActiveChat,
	sessions core.SnapshotMap[string, client.RuntimeSessionView],
) {
	var keys []string
	if view := sessions.GetShared(conversation.SessionID()); view != nil {
		keys = view.PolicyConfigKeys
	}
	conversation.SetPolicyConfigKeys(keys)
}

// refreshSubagentCounts hands every open conversation the dashboard's count
// of its sub-agents and of how many are working, which the composer footer
// displays.
//
// The working count changes whenever a child's worker reports different
// activity, which is not a change to any record, so it is refreshed from the
// worker updates as well as with the records (I2-7).
func refreshSubagentCounts(chats map[string]*chat.ActiveChat, dashboard *tui.DashboardState) {
	for _, sessionID := range slices.Sorted(maps.Keys(chats)) {
		conversation := chats[sessionID]
		session := conversation.SessionID()
		conversation.SetSubagentCount(dashboard.SubagentCountFor(session))
		conversation.SetReviewerCounts(dashboard.ReviewerCountsFor(session))
		conversation.SetSubagentWorkingCount(dashboard.WorkingSubagentCountFor(session))
	}
}

// nextWorkspace is the tab to show after a feed frame: a workspace the user
// just created once the frame carries it, else the active tab while it
// exists, else the first. pending is the dashboard's pending selection,
// cleared once the frame carries it (Rust: `&mut Option<String>`).
func nextWorkspace(active *string, pending **string, names map[string]string) *string {
	if created := *pending; created != nil {
		if _, ok := names[*created]; ok {
			*pending = nil
			return created
		}
	}
	if active != nil {
		if _, ok := names[*active]; ok {
			id := *active
			return &id
		}
	}
	if len(names) == 0 {
		return nil
	}
	first := slices.Min(slices.Collect(maps.Keys(names)))
	return &first
}
