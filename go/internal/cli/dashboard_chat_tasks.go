// Port of mj-cli/src/dashboard/chat_tasks.rs.

package cli

import (
	"fmt"
	"log/slog"
	"maps"
	"slices"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// resumeDiscoveryFilter is the (query, state) filter the resume dialog opens
// with (Rust's Option<(String, Option<SessionStateFilter>)>; nil is None).
type resumeDiscoveryFilter struct {
	query string
	state *tui.SessionStateFilter
}

// startSessionsTextSearch starts a background search of conversation text
// when the Sessions filter's text changed since the last one. Cheap when it
// did not.
func (c *dashboardContext) startSessionsTextSearch() {
	requestID, query, ok := c.dashboard.NextSessionsTextSearch()
	if !ok {
		return
	}
	// Replacing the guard cancels the previous search.
	search := spawnSessionsTextSearch(requestID, query, c.dashboardIoTx)
	if c.sessionsTextTask != nil {
		c.sessionsTextTask()
	}
	c.sessionsTextTask = search
}

// reconcileWikiSyncs keeps one cancellable follower per surface episode,
// independent of queries.
func (c *dashboardContext) reconcileWikiSyncs() {
	if _, ok := c.dashboard.WikiSyncEpisode(false); !ok && c.sessionsTextTask != nil {
		c.sessionsTextTask()
		c.sessionsTextTask = nil
	}
	for _, dialog := range []bool{true, false} {
		episode, active := c.dashboard.WikiSyncEpisode(dialog)
		task := &c.sessionsSyncTask
		if dialog {
			task = &c.wikiSyncTask
		}
		current := *task
		if (current == nil && !active) || (current != nil && active && current.generation == episode) {
			continue
		}
		if current != nil {
			current.cancel()
		}
		*task = nil
		if active {
			*task = &generationTask{
				generation: episode,
				cancel:     spawnWikiSync(dialog, episode, c.dashboardIoTx),
			}
		}
	}
}

// startResumeDiscovery opens the resume dialog and starts one background
// scan per profile. Every profile appears immediately as a placeholder, so
// the dialog is usable while the scans are still running, and the scans run
// concurrently rather than one after another.
func (c *dashboardContext) startResumeDiscovery() {
	c.startResumeDiscoveryWithFilter(nil)
}

func (c *dashboardContext) startResumeDiscoveryWithFilter(filter *resumeDiscoveryFilter) {
	c.importDiscoveryID++
	config := c.controller.Config
	c.dashboard.ShowResumeDialog(c.importDiscoveryID, tui.ResumeProfilePlaceholders(
		func(yield func(string, core.HarnessKind) bool) {
			for id, profile := range config.EnabledProfiles() {
				if !yield(id, profile.Kind) {
					return
				}
			}
		},
	))
	if filter != nil {
		c.dashboard.SetResumeFilter(filter.query, filter.state)
	}
	spawnResumeCandidates(c.importDiscoveryID, c.dashboardIoTx)
	// Opening the dialog issues its initial query. It lists the most recent
	// indexed sessions and, just as importantly, brings back the index's
	// state, which decides whether the search box can be typed into.
	if requestID, query, ok := c.dashboard.NextWikiSearch(); ok {
		// The dialog has just opened and has nothing to show yet, so this
		// first ask does not wait out a typing pause.
		guard := spawnWikiSearch(requestID, query, 0, c.dashboardIoTx)
		if c.wikiSearchTask != nil {
			c.wikiSearchTask.cancel()
		}
		c.wikiSearchTask = &generationTask{generation: requestID, cancel: guard}
	}
	discoveryID := c.importDiscoveryID
	for profileID, profile := range config.EnabledProfiles() {
		updates := c.importUpdatesTx
		cache := c.nativeScanCache
		core.SpawnBlocking(func() struct{} {
			completed := discoverClientImportProfile(profileID, profile, cache, func(partial tui.ImportProfileOption) {
				// A full channel drops the partial list: the completed one
				// follows.
				partial.Sessions = slices.Clone(partial.Sessions)
				select {
				case updates <- importProfileUpdate{discoveryID: discoveryID, option: partial}:
				default:
				}
			})
			updates <- importProfileUpdate{discoveryID: discoveryID, option: completed}
			return struct{}{}
		})
	}
}

// startImport starts a background import and shows its progress dialog.
func (c *dashboardContext) startImport(pending pendingDashboardImport, safety dashboardImportSafety) {
	c.dashboard.ShowImportProgress(pending.displayTitle)
	c.nextImportTaskID++
	cancelled := &atomic.Bool{}
	c.activeImport = &activeDashboardImport{taskID: c.nextImportTaskID, cancelled: cancelled}
	spawnDashboardImport(
		&c.controller,
		dashboardImportRequest{
			workspaceID: c.workspaceID,
			pending:     pending,
			safety:      safety,
			taskID:      c.nextImportTaskID,
			cancelled:   cancelled,
		},
		c.importTaskTx,
		c.criticalOperations,
	)
}

// openSubagents shows parentID's sub-agents, keeping the composer draft,
// and opens the first one's conversation. The prompt border's click and the
// Sub-agents command both come here.
func (c *dashboardContext) openSubagents(parentID string) {
	c.openChildWorkspace(parentID, core.SubagentKindWorker)
}

func (c *dashboardContext) openChildWorkspace(parentID string, kind core.SubagentKind) {
	c.captureComposerDraft(parentID)
	reviewers := kind == core.SubagentKindReviewer
	if reviewers {
		c.dashboard.OpenReviewerWorkspace(parentID)
	} else {
		c.dashboard.OpenSubagentWorkspace(parentID)
	}
	if c.updateSubagentScope() {
		if reviewers {
			c.dashboard.SetNotice("Loading reviewers…")
		} else {
			c.dashboard.SetNotice("Loading sub-agents…")
		}
	}
	if childID, ok := c.dashboard.SelectedSessionID(); ok {
		c.openChatSession(childID)
	}
}

// applyChatOutcome applies what the chat view asked for after handling its
// own input.
func (c *dashboardContext) applyChatOutcome(outcome chat.ChatEventOutcome) {
	switch outcome := outcome.(type) {
	case chat.ChatEventOutcomeNone, chat.ChatEventOutcomeHandled:
	case chat.ChatEventOutcomeCycleFocus:
		c.dashboard.CycleFocus(outcome.Reverse)
	case chat.ChatEventOutcomeOpenSubagents:
		focused := c.focusedChat()
		if focused == nil {
			return
		}
		c.openSubagents(focused.SessionID())
	case chat.ChatEventOutcomeOpenReviewers:
		focused := c.focusedChat()
		if focused == nil {
			return
		}
		c.openChildWorkspace(focused.SessionID(), core.SubagentKindReviewer)
	case chat.ChatEventOutcomeRecallQueuedPrompt:
		if _, _, known := c.startupPrompts.knownPrompt(outcome.SessionID, outcome.ID); known {
			if active, ok := c.chats[outcome.SessionID]; ok {
				prompt := active.RecalledStartupPrompt(outcome.ID, outcome.Text)
				c.startupPrompts.withdraw(
					namedSession{
						name: sessionNoticeName(&c.controller.State, outcome.SessionID),
						id:   outcome.SessionID,
					},
					prompt,
					c.dashboardIoTx,
					c.criticalOperations,
				)
			}
		} else if active, ok := c.chats[outcome.SessionID]; ok {
			active.RemoveWorkerQueuedPrompt(outcome.ID, outcome.Text, outcome.Kind)
		}
	case chat.ChatEventOutcomeCopyText:
		if err := c.copyText(outcome.Text, outcome.Notice); err != nil {
			slog.Warn("copying chat text failed", "error", err.Error())
		}
	case chat.ChatEventOutcomeQuitDetach:
		c.requestShutdown()
	}
}

// recordDetach captures departing input and advances its receipt through
// their owners.
func (c *dashboardContext) recordDetach(sessionID string, lastSeenEventOrdinal uint64) {
	c.captureComposerDraft(sessionID)
	var previous uint64
	if record := c.chatSessionRecord(sessionID); record != nil {
		previous = record.ViewedThroughEventOrdinal
	}
	through := detachReadFrontier(c.chatWasOnScreen(sessionID), lastSeenEventOrdinal, previous)
	if session := c.controller.State.Sessions.GetMut(sessionID); session != nil {
		session.ViewedThroughEventOrdinal = max(session.ViewedThroughEventOrdinal, through)
		c.dashboard.SetState(c.controller.State)
	}
	// Existing optimistic advances already belong to readReceipts. A detach
	// that moves no frontier must not create a fresh save at quit.
	if through > previous {
		if through, ok := c.readReceipts.acknowledge(sessionID, through); ok {
			c.spawnReadReceipt(sessionID, through)
		}
	}
	c.flushComposerDraft(sessionID, true)
}

func (c *dashboardContext) flushComposerDraft(sessionID string, retry bool) {
	revision, draft, ok := c.composerDrafts.start(sessionID, retry)
	if !ok {
		return
	}
	var departed *core.SessionRecord
	if record, ok := c.departedRecords[sessionID]; ok {
		departed = &record
	}
	queued := recordTerminalDraftState(
		&c.controller,
		departed,
		c.dashboard,
		terminalDraftState{
			clientID:  c.clientID,
			sessionID: sessionID,
			revision:  revision,
			draft:     draft,
		},
		c.dashboardIoTx,
		c.criticalOperations,
	)
	if queued == nil {
		c.composerDrafts.complete(sessionID, revision, false)
		if c.draftSaveFailures == nil {
			c.draftSaveFailures = map[string]string{}
		}
		c.draftSaveFailures[sessionID] = fmt.Sprintf("Could not save draft for %s: unknown session", sessionID)
	}
}

// captureComposerDrafts: every loop source can mutate an editor, including
// asynchronous paste and submission restoration. Capture after dispatch,
// not only on keys.
func (c *dashboardContext) captureComposerDrafts() {
	sessions := map[string]struct{}{}
	for sessionID := range c.chats {
		sessions[sessionID] = struct{}{}
	}
	for _, sessionID := range c.dashboard.StandbySessionIDs() {
		sessions[sessionID] = struct{}{}
	}
	for _, sessionID := range slices.Sorted(maps.Keys(sessions)) {
		c.captureComposerDraft(sessionID)
	}
	for _, sessionID := range c.composerDrafts.sessionIDs() {
		c.flushComposerDraft(sessionID, false)
	}
}

// chatWasOnScreen is whether a conversation was on screen, which decides
// whether its read frontier may move forward when it detaches.
func (c *dashboardContext) chatWasOnScreen(sessionID string) bool {
	if slices.Contains(c.drawnChatSessions, sessionID) {
		return true
	}
	visible := c.visibleChat()
	return visible != nil && visible.SessionID() == sessionID
}

// captureComposerDraft captures current editor text while retaining the
// original shared seed.
func (c *dashboardContext) captureComposerDraft(sessionID string) {
	var inherited string
	if record := c.chatSessionRecord(sessionID); record != nil {
		inherited = record.DraftInput
	}
	captureEditorDraft(&c.composerDrafts, c.dashboard, c.chats, sessionID, inherited)
}
