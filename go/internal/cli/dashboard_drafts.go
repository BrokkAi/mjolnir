// Port of mj-cli/src/dashboard/drafts.rs.

package cli

import (
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
	panic("unported: mj-cli/src/dashboard/drafts.rs chat_daemon_requests")
}

func (c *dashboardContext) chatDaemonRequests(sessionID string) chat.ChatDaemonSender {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::chat_daemon_requests")
}

// saveQuestionDraft caches pending question forms from the warm chat before
// it is replaced. The composer itself is tracked by composerDraftCache.
func (c *dashboardContext) saveQuestionDraft(sessionID string) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::save_question_draft")
}

func (c *dashboardContext) reconcileQuestionDrafts() {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::reconcile_question_drafts")
}

func (c *dashboardContext) restoreQuestionDraft(sessionID string, active *chat.ActiveChat) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::restore_question_draft")
}

// openChatSession enters an existing pin or opens the session in Browse.
// The replaced view is saved and the new chat attaches in the background.
func (c *dashboardContext) openChatSession(sessionID string) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::open_chat_session")
}

func (c *dashboardContext) openChatSessionInto(pane tui.PaneId, sessionID string) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::open_chat_session_into")
}

// startChatAttach is one session-keyed background preparation, shared by
// all display surfaces.
func (c *dashboardContext) startChatAttach(sessionID string) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::start_chat_attach")
}

// trackPathInput remembers the path-input operation in flight; a new one
// asks the previous one to stop. Cancellation is cooperative, so this only
// requests it.
func (c *dashboardContext) trackPathInput(cancelled *atomic.Bool) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::track_path_input")
}

// trackCompletion remembers the completion request in flight. Only one is
// useful at a time, so a new one cancels the last.
func (c *dashboardContext) trackCompletion(cancelled *atomic.Bool) {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::track_completion")
}

func (c *dashboardContext) cancelStalePathInput() {
	panic("unported: mj-cli/src/dashboard/drafts.rs DashboardContext::cancel_stale_path_input")
}
