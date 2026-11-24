// Port of mj-cli/src/dashboard/chat_tasks.rs.

package cli

import (
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
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::start_sessions_text_search")
}

// reconcileWikiSyncs keeps one cancellable follower per surface episode,
// independent of queries.
func (c *dashboardContext) reconcileWikiSyncs() {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::reconcile_wiki_syncs")
}

// startResumeDiscovery opens the resume dialog and starts one background
// scan per profile. Every profile appears immediately as a placeholder, so
// the dialog is usable while the scans are still running, and the scans run
// concurrently rather than one after another.
func (c *dashboardContext) startResumeDiscovery() {
	c.startResumeDiscoveryWithFilter(nil)
}

func (c *dashboardContext) startResumeDiscoveryWithFilter(filter *resumeDiscoveryFilter) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::start_resume_discovery_with_filter")
}

// startImport starts a background import and shows its progress dialog.
func (c *dashboardContext) startImport(pending pendingDashboardImport, safety dashboardImportSafety) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::start_import")
}

// openSubagents shows parentID's sub-agents, keeping the composer draft,
// and opens the first one's conversation. The prompt border's click and the
// Sub-agents command both come here.
func (c *dashboardContext) openSubagents(parentID string) {
	c.openChildWorkspace(parentID, core.SubagentKindWorker)
}

func (c *dashboardContext) openChildWorkspace(parentID string, kind core.SubagentKind) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::open_child_workspace")
}

// applyChatOutcome applies what the chat view asked for after handling its
// own input.
func (c *dashboardContext) applyChatOutcome(outcome chat.ChatEventOutcome) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::apply_chat_outcome")
}

// recordDetach captures departing input and advances its receipt through
// their owners.
func (c *dashboardContext) recordDetach(sessionID string, lastSeenEventOrdinal uint64) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::record_detach")
}

func (c *dashboardContext) flushComposerDraft(sessionID string, retry bool) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::flush_composer_draft")
}

// captureComposerDrafts: every loop source can mutate an editor, including
// asynchronous paste and submission restoration. Capture after dispatch,
// not only on keys.
func (c *dashboardContext) captureComposerDrafts() {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::capture_composer_drafts")
}

// chatWasOnScreen is whether a conversation was on screen, which decides
// whether its read frontier may move forward when it detaches.
func (c *dashboardContext) chatWasOnScreen(sessionID string) bool {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::chat_was_on_screen")
}

// captureComposerDraft captures current editor text while retaining the
// original shared seed.
func (c *dashboardContext) captureComposerDraft(sessionID string) {
	panic("unported: mj-cli/src/dashboard/chat_tasks.rs DashboardContext::capture_composer_draft")
}
