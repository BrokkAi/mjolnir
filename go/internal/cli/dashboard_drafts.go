// Port of mj-cli/src/dashboard/drafts.rs.

package cli

import (
	"context"
	"fmt"
	"log/slog"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// prepareAttachDraft transfers the latest local draft into both sides of an
// asynchronous attach: the chat being prepared and the composer the person
// can edit meanwhile.
func prepareAttachDraft(cache *composerDraftCache, dashboard *tui.DashboardState, session *core.SessionRecord) string {
	if text, ok := dashboard.StandbyPromptDraft(session.ID); ok {
		cache.capture(session.ID, text, session.DraftInput)
	}
	draft := cache.open(session.ID, session.DraftInput).Text
	if !dashboard.HasStandbyComposer(session.ID) {
		dashboard.SeedStandbyPrompt(session.ID, draft)
	}
	return draft
}

// captureEditorDraft: standby owns the composer while attaching; otherwise
// the warm chat owns it. In particular an empty standby must win over an
// older warm draft.
func captureEditorDraft(cache *composerDraftCache, dashboard *tui.DashboardState, chats map[string]*chat.ActiveChat, sessionID string, inherited string) {
	text, ok := dashboard.StandbyPromptDraft(sessionID)
	if !ok {
		active, found := chats[sessionID]
		if !found {
			return
		}
		text = active.Draft()
	}
	cache.capture(sessionID, text, inherited)
}

// chatDaemonRequests keeps ordering within a session while independent
// sessions dispatch concurrently. Attached and detached producers use this
// same bridge owner.
func chatDaemonRequests(bridges map[string]chat.ChatDaemonSender, sessionID string, updates *core.Unbounded[dashboardIoUpdate], notices chat.Notices) chat.ChatDaemonSender {
	if sender, ok := bridges[sessionID]; ok {
		return sender
	}
	persistence := core.NewUnbounded[chat.ChatDaemonRequest]()
	refusals := updates
	core.Spawn(func() struct{} {
		for {
			request, ok := persistence.Recv(context.Background())
			if !ok {
				return struct{}{}
			}
			// A review action's refusal is a sentence for the person who
			// pressed the key, so it comes back to the chat rather than
			// only into the log.
			var refusalSession *string
			if review, ok := request.(chat.ChatDaemonRequestStartTurnReview); ok {
				refusalSession = &review.SessionID
			}
			err := sendChatDaemonRequest(context.Background(), request)
			if err != nil && refusalSession != nil {
				report("persisting chat state", refusals, dashboardIoUpdate(dashboardIoUpdateReviewRefused{
					sessionID: *refusalSession,
					message:   err.Error(),
				}))
			}
			if err != nil {
				slog.Warn("could not persist chat state through the daemon", "error", err.Error())
				notices.SetFailure(fmt.Sprintf("Could not persist Prompt state: %v", err))
			}
		}
	})
	bridges[sessionID] = persistence
	return persistence
}

// sendChatDaemonRequest delivers one chat request over its own daemon
// connection.
func sendChatDaemonRequest(ctx context.Context, request chat.ChatDaemonRequest) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	defer daemon.Close()
	switch request := request.(type) {
	case chat.ChatDaemonRequestRecordAbandonedPrompt:
		return daemon.RecordAbandonedPrompt(ctx, request.SessionID, request.BundleID, request.Text)
	case chat.ChatDaemonRequestStartTurnReview:
		return daemon.StartTurnReview(ctx, request.SessionID)
	}
	return fmt.Errorf("unknown chat daemon request %T", request)
}

func (c *dashboardContext) chatDaemonRequests(sessionID string) chat.ChatDaemonSender {
	if c.composerDaemonRequests == nil {
		c.composerDaemonRequests = map[string]chat.ChatDaemonSender{}
	}
	return chatDaemonRequests(c.composerDaemonRequests, sessionID, c.dashboardIoTx, c.notices)
}

// sessionAttachmentFor is the session's attachment, made when it has none (Rust's
// `attachments.entry(id).or_default()`).
func (c *dashboardContext) sessionAttachmentFor(sessionID string) *sessionAttachment {
	if c.attachments == nil {
		c.attachments = map[string]*sessionAttachment{}
	}
	attachment, ok := c.attachments[sessionID]
	if !ok {
		attachment = newSessionAttachment()
		c.attachments[sessionID] = attachment
	}
	return attachment
}

// saveQuestionDraft caches pending question forms from the warm chat before
// it is replaced. The composer itself is tracked by composerDraftCache.
func (c *dashboardContext) saveQuestionDraft(sessionID string) {
	active, ok := c.chats[sessionID]
	if !ok {
		return
	}
	drafts := active.ElicitationDrafts()
	if len(drafts) == 0 {
		// A request was answered or removed while this session was away;
		// retaining its old entry would make a later attach look like a
		// new pending request.
		delete(c.questionDrafts, sessionID)
		return
	}
	capturedEventOrdinal := active.LatestEventOrdinal()
	cached := make([]cachedQuestionDraft, 0, len(drafts))
	for _, draft := range drafts {
		cached = append(cached, cachedQuestionDraft{Draft: draft, CapturedEventOrdinal: capturedEventOrdinal})
	}
	if c.questionDrafts == nil {
		c.questionDrafts = map[string][]cachedQuestionDraft{}
	}
	c.questionDrafts[sessionID] = cached
}

func (c *dashboardContext) reconcileQuestionDrafts() {
	for sessionID, drafts := range c.questionDrafts {
		if !c.controller.State.Sessions.ContainsKey(sessionID) {
			delete(c.questionDrafts, sessionID)
			continue
		}
		projectionOrdinal, pending, ok := c.dashboard.PendingElicitations(sessionID)
		if !ok {
			// A startup summary has no complete pending-request list yet;
			// keep the cache until its accepted full projection arrives.
			continue
		}
		kept := make([]cachedQuestionDraft, 0, len(drafts))
		for _, draft := range drafts {
			if !questionDraftProjectionIsCurrent(draft.CapturedEventOrdinal, projectionOrdinal) {
				// The active feed may have captured a newer local form than
				// the dashboard's last accepted full projection.
				kept = append(kept, draft)
				continue
			}
			// Reviewer requests live in the sidecar review projection, not
			// in the primary materialized session. The attached chat
			// validates those by source/role when it is restored.
			if draft.Draft.Reviewer() || questionDraftMatchesAny(&draft.Draft, pending) {
				kept = append(kept, draft)
			}
		}
		if len(kept) == 0 {
			delete(c.questionDrafts, sessionID)
		} else {
			c.questionDrafts[sessionID] = kept
		}
	}
}

func questionDraftMatchesAny(draft *chat.ChatElicitationDraft, pending []core.ElicitationRequest) bool {
	for i := range pending {
		if draft.Matches(&pending[i]) {
			return true
		}
	}
	return false
}

func (c *dashboardContext) restoreQuestionDraft(sessionID string, active *chat.ActiveChat) {
	drafts, ok := c.questionDrafts[sessionID]
	if !ok {
		return
	}
	delete(c.questionDrafts, sessionID)
	// ActiveChat defers reviewer restoration when the reviewer stream has
	// not surfaced its form yet. A false result here means the projection
	// has no matching pending request, so retaining the entry would allow a
	// stale answer to return on a later attach.
	restored := make([]chat.ChatElicitationDraft, 0, len(drafts))
	for _, cached := range drafts {
		restored = append(restored, cached.Draft)
	}
	active.RestoreElicitationDrafts(restored)
}

// openChatSession enters an existing pin or opens the session in Browse.
// The replaced view is saved and the new chat attaches in the background.
func (c *dashboardContext) openChatSession(sessionID string) {
	pane, ok := c.dashboard.PaneForSession(sessionID)
	if !ok {
		pane = c.dashboard.BrowsePane()
	}
	c.dashboard.RevealPane(pane)
	c.dashboard.FocusPane(pane)
	c.openChatSessionInto(pane, sessionID)
}

func (c *dashboardContext) openChatSessionInto(pane tui.PaneId, sessionID string) {
	if !c.dashboard.HasConversationPane(pane) {
		return
	}
	native := c.dashboard.IsNativeAgent(sessionID)
	if !native && !c.sessionInActiveWorkspace(sessionID) {
		return
	}
	if outgoing, ok := c.dashboard.PaneSession(pane); ok && outgoing != sessionID {
		c.selection.Clear()
		// A visible form keeps its chat and clipboard result channel.
		// Parking and immediately re-adopting would replace the channel
		// underneath a read accepted by the retained form.
		formRetained := chatRetainedWithoutConversation(c.dashboard, c.chats, outgoing)
		if !formRetained {
			c.parkChatComposer(outgoing)
		}
		// Save the outgoing conversation, then drop it unless the
		// notification form still owns its live chat.
		c.captureComposerDraft(outgoing)
		c.saveQuestionDraft(outgoing)
		c.recordChatDetach(outgoing)
		if outgoingPane, ok := c.dashboard.PaneForSession(outgoing); ok && outgoingPane == pane && !formRetained {
			delete(c.chats, outgoing)
		}
	}
	c.dashboard.SetPaneSession(pane, &sessionID)
	if native {
		c.cancelChatOpenIn(pane)
		return
	}
	// A stopped sub-agent has no worker to attach to, and an attach would
	// wait out its timeout and leave the pane empty. Its conversation is
	// stored, so read that instead.
	if c.dashboard.IsStoppedSubagent(sessionID) {
		c.cancelChatOpenIn(pane)
		if c.dashboard.BeginStoppedSubagent(sessionID) {
			spawnStoppedSubagentTranscript(sessionID, c.dashboardIoTx)
		}
		return
	}
	c.captureComposerDraft(sessionID)
	attachment := c.sessionAttachmentFor(sessionID)
	attachment.observePane(c.dashboard, pane, sessionID)
	if assignment, ok := c.dashboard.PaneAssignment(pane); ok {
		attachment.requestPane(pane, assignment)
	}
	attachment.selectSession(sessionID)
	c.saveQuestionDraft(sessionID)
	// A lifecycle owns the row's conversation until its authoritative
	// completion. Do not start an attach that can arrive after Stop/Move and
	// put a retiring chat back on screen.
	if c.dashboard.TransitionKind(sessionID) != nil ||
		c.dashboard.TransitionFailureKind(sessionID) != nil ||
		c.dashboard.SessionFailed(sessionID) {
		c.deferChatOpenIn(pane)
		return
	}
	// A suspended session has no worker to attach to; an attach would wait
	// out its timeout and report that opening did not respond (R4-11),
	// whichever pane asked.
	if c.dashboard.PaneSessionIsSuspended(sessionID) {
		c.releaseSuspendedPane(pane, sessionID)
		return
	}
	c.startChatAttach(sessionID)
	if attachment, ok := c.attachments[sessionID]; ok && attachment.pending() {
		detach := ""
		if key, ok := c.dashboard.FirstKeyLabel(tui.CommandIdQuitDetach); ok {
			detach = fmt.Sprintf("; %s quits", key)
		}
		c.dashboard.SetNotice(fmt.Sprintf(
			"Opening session%s Esc cancels; select another session to switch%s.",
			chat.ThemeGlyphs().Ellipsis,
			detach,
		))
	}
}

// startChatAttach is one session-keyed background preparation, shared by
// all display surfaces.
func (c *dashboardContext) startChatAttach(sessionID string) {
	if active, ok := c.chats[sessionID]; ok && active.SessionFeedOpen() {
		if composer := c.dashboard.TakeStandbyComposer(sessionID); composer != nil {
			active.AdoptLiveComposer(*composer)
			active.KeepDeferredSubmissionAsDraft()
		}
		c.acknowledgeVisibleChats()
		return
	}
	for _, entry := range c.dashboard.PaneSessions() {
		if entry.SessionID == sessionID {
			c.sessionAttachmentFor(entry.SessionID).observePane(c.dashboard, entry.Pane, sessionID)
		}
	}
	if attachment, ok := c.attachments[sessionID]; ok && attachment.pending() {
		return
	}
	sessionRecord, ok := c.controller.State.Sessions.Get(sessionID)
	if !ok {
		message := fmt.Sprintf("Unknown session %s", sessionID)
		c.dashboard.SetNotificationFormError(sessionID, &message)
		c.dashboard.ReportOpenFailure(sessionID, message)
		return
	}
	title := sessionRecord.ListedTitle()
	if c.dashboard.GoMode() != nil {
		title = c.dashboard.GoConversationTitle(sessionID)
	}
	harnessKind := sessionRecord.HarnessKind
	header := chat.SessionHeaderIdentity{
		Target: c.controller.State.ProjectIdentitySession(&sessionRecord).
			ProjectTarget(&c.controller.Config, sessionRecord.TargetTemplateID),
		Profile:       sessionRecord.LastProfile,
		Title:         title,
		HarnessKind:   &harnessKind,
		SubagentCount: c.dashboard.SubagentCountFor(sessionRecord.ID),
		Reviewers:     c.dashboard.ReviewerCountsFor(sessionRecord.ID),
	}
	sessions := c.workerCommandsTx
	notices := c.notices
	updates := c.dashboardIoTx
	bundleID := sessionRecord.BundleID
	if !c.dashboard.HasStandbyComposer(sessionID) {
		if active, ok := c.chats[sessionID]; ok {
			if composer := active.TakeLiveComposer(); composer != nil {
				c.dashboard.AdoptStandbyComposer(sessionID, composer)
			}
		}
	}
	// A draft typed while the session's transition ran belongs to this
	// composer now; it wins over the warm chat's older captured text.
	draft := prepareAttachDraft(&c.composerDrafts, c.dashboard, &sessionRecord)
	sessionContext := &chat.ChatSessionContext{
		Config:  c.controller.Config,
		Session: sessionRecord,
	}
	persistence := c.chatDaemonRequests(sessionID)
	if c.openingChatSessions == nil {
		c.openingChatSessions = map[tui.PaneId]string{}
	}
	for _, entry := range c.dashboard.PaneSessions() {
		if entry.SessionID == sessionID {
			c.openingChatSessions[entry.Pane] = entry.SessionID
		}
	}
	c.dashboard.SetNotificationFormError(sessionID, nil)
	c.syncOpeningSession()
	sessionAttachmentSpawn(
		c.sessionAttachmentFor(sessionID),
		sessionID,
		attachTimeout,
		func(ctx context.Context) (*chat.PreparedChat, error) {
			control := sessions.Client()
			managed, err := control.WaitForSession(ctx, sessionID, attachTimeout)
			if err != nil {
				return nil, err
			}
			preparation := core.SpawnBlocking(func() chat.PreparedChat {
				return chat.ActiveChatPrepareWithPersistence(
					managed,
					bundleID,
					sessionContext,
					control,
					header,
					draft,
					notices,
					persistence,
				)
			})
			prepared, err := preparation.Join(ctx)
			if err != nil {
				if ctx.Err() != nil {
					return nil, err
				}
				return nil, fmt.Errorf("chat preparation task failed: %w", err)
			}
			return &prepared, nil
		},
		func(generation uint64, prepared *chat.PreparedChat, err error) {
			report("opening a session", updates, dashboardIoUpdate(dashboardIoUpdateChatOpened{
				generation: generation,
				sessionID:  sessionID,
				prepared:   prepared,
				err:        err,
			}))
		},
	)
}

// trackPathInput remembers the path-input operation in flight; a new one
// asks the previous one to stop. Cancellation is cooperative, so this only
// requests it.
func (c *dashboardContext) trackPathInput(cancelled *atomic.Bool) {
	previous := c.pathInputJob
	c.pathInputJob = &pathInputJob{draft: c.dashboard.PathInputContext(), cancelled: cancelled}
	if previous != nil {
		previous.cancelled.Store(true)
	}
}

// trackCompletion remembers the completion request in flight. Only one is
// useful at a time, so a new one cancels the last.
func (c *dashboardContext) trackCompletion(cancelled *atomic.Bool) {
	previous := c.completionJob
	c.completionJob = &pathInputJob{draft: c.dashboard.PathInputContext(), cancelled: cancelled}
	if previous != nil {
		previous.cancelled.Store(true)
	}
}

func (c *dashboardContext) cancelStalePathInput() {
	inputContext := c.dashboard.PathInputContext()
	if job := c.pathInputJob; job != nil && job.draft != inputContext {
		c.pathInputJob = nil
		job.cancelled.Store(true)
	}
	if job := c.completionJob; job != nil && job.draft != inputContext {
		c.completionJob = nil
		job.cancelled.Store(true)
	}
}
