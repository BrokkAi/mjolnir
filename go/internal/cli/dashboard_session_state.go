// Port of mj-cli/src/dashboard/session_state.rs.
//
// The dashboard context's session bookkeeping: sub-agent scope, stored
// summaries and projections, conversation attachments, the visible chat,
// and the background reads of each session's working context, Git status
// and project source.

package cli

import (
	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// updateSubagentScope: a family stays subscribed while its rows or a
// retained conversation need it. Reports whether the scope changed.
func (c *dashboardContext) updateSubagentScope() bool {
	panic("unported: mj-cli/src/dashboard/session_state.rs update_subagent_scope")
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
	panic("unported: mj-cli/src/dashboard/session_state.rs request_transcript_tail_seed")
}

// requestMaterializedProjection keeps one projection per session in flight
// and remembers only the newest snapshot that arrived behind it. The shared
// permits bound work across different sessions as well.
func (c *dashboardContext) requestMaterializedProjection(projection materializedProjection, viewedThroughEventOrdinal uint64) {
	panic("unported: mj-cli/src/dashboard/session_state.rs request_materialized_projection")
}

func (c *dashboardContext) wantedSummarySessions() map[string]uint64 {
	panic("unported: mj-cli/src/dashboard/session_state.rs wanted_summary_sessions")
}

func (c *dashboardContext) hydrateStoredSessionSummaries() {
	panic("unported: mj-cli/src/dashboard/session_state.rs hydrate_stored_session_summaries")
}

func (c *dashboardContext) reconcileStoredSessionSummaries() {
	panic("unported: mj-cli/src/dashboard/session_state.rs reconcile_stored_session_summaries")
}

// finishStartupSummary records that one live session's stored summary has
// arrived, however it turned out, and opens the startup conversation once
// they all have.
func (c *dashboardContext) finishStartupSummary(sessionID string) {
	panic("unported: mj-cli/src/dashboard/session_state.rs finish_startup_summary")
}

// reconcileConversationAttachments attaches to the conversations assigned
// by navigation. A failed open needs an explicit retry, not another attempt
// on every background completion.
func (c *dashboardContext) reconcileConversationAttachments() {
	panic("unported: mj-cli/src/dashboard/session_state.rs reconcile_conversation_attachments")
}

func (c *dashboardContext) wantedChatSessions() map[string]struct{} {
	panic("unported: mj-cli/src/dashboard/session_state.rs wanted_chat_sessions")
}

// cancelChatOpen stops the focused pane's attach. A failed or cancelled
// open stays observed, so it is not retried on every background wakeup.
func (c *dashboardContext) cancelChatOpen() {
	panic("unported: mj-cli/src/dashboard/session_state.rs cancel_chat_open")
}

func (c *dashboardContext) cancelChatOpenIn(pane tui.PaneId) {
	panic("unported: mj-cli/src/dashboard/session_state.rs cancel_chat_open_in")
}

// releaseSuspendedPane lets a pane go of a suspended session instead of
// attaching to it. The dashboard decides what the pane keeps; see
// tui.DashboardState.ReleaseSuspendedPane.
func (c *dashboardContext) releaseSuspendedPane(pane tui.PaneId, sessionID string) {
	panic("unported: mj-cli/src/dashboard/session_state.rs release_suspended_pane")
}

// deferChatOpenIn gives up this pane's attach in a way that allows one
// fresh attempt when whatever owns the session has finished with it.
func (c *dashboardContext) deferChatOpenIn(pane tui.PaneId) {
	panic("unported: mj-cli/src/dashboard/session_state.rs defer_chat_open_in")
}

// visibleChat is the warm chat when it belongs on screen, nil otherwise.
//
// While an attach for a different session is in flight, the chat still
// loaded is the one the selection has moved off. Drawing it under the new
// row's highlight, or handing it the keyboard, would report the wrong
// conversation, so it stays hidden until selected again. Its feeds keep
// running while an attach is pending, failed, or cancelled.
func (c *dashboardContext) visibleChat() *chat.ActiveChat {
	panic("unported: mj-cli/src/dashboard/session_state.rs visible_chat")
}

// paneChatMut is the warm chat one pane shows, whether or not that pane
// has the keyboard; nil when none.
//
// A pane answers only for the session it holds, so this asks the
// arrangement rather than the Sessions selection. While the pane is
// attaching to a different session its old conversation is hidden, the same
// rule the renderer draws by, and nothing is returned.
func (c *dashboardContext) paneChatMut(pane tui.PaneId) *chat.ActiveChat {
	panic("unported: mj-cli/src/dashboard/session_state.rs pane_chat_mut")
}

// dashboardAndVisibleChat is the dashboard and the conversation on screen
// (nil when none), borrowed together, so a caller can act on one and report
// through the other.
func (c *dashboardContext) dashboardAndVisibleChat() (*tui.DashboardState, *chat.ActiveChat) {
	panic("unported: mj-cli/src/dashboard/session_state.rs dashboard_and_visible_chat")
}

func (c *dashboardContext) needsAnimation() bool {
	panic("unported: mj-cli/src/dashboard/session_state.rs needs_animation")
}

// cancelStartupSession: the user took the choice into their own hands, so
// the surface stops trying to pick a conversation for them.
func (c *dashboardContext) cancelStartupSession() {
	panic("unported: mj-cli/src/dashboard/session_state.rs cancel_startup_session")
}

func (c *dashboardContext) refreshGoContext() {
	panic("unported: mj-cli/src/dashboard/session_state.rs refresh_go_context")
}

// refreshGitStatus reads the checkout of the visible sessions that are due,
// a couple per pass so a workspace full of sessions never floods a slow
// target.
func (c *dashboardContext) refreshGitStatus() {
	panic("unported: mj-cli/src/dashboard/session_state.rs refresh_git_status")
}

func (c *dashboardContext) spawnGitProbe(sessionID string) {
	panic("unported: mj-cli/src/dashboard/session_state.rs spawn_git_probe")
}

// liveControllerSnapshot is the sessions as the runtime feed last delivered
// them, for work that runs off the event loop. The maps are persistent, so
// this is cheap, and it never opens the store, which only the daemon may
// read.
func (c *dashboardContext) liveControllerSnapshot() controller.Controller {
	panic("unported: mj-cli/src/dashboard/session_state.rs live_controller_snapshot")
}

func (c *dashboardContext) rememberGoSelection() {
	panic("unported: mj-cli/src/dashboard/session_state.rs remember_go_selection")
}

// maybeOpenStartupSession opens the conversation the surface should start
// on, once the summaries it compares have arrived or the wait for them has
// run out. Reports whether the pick ran, which happens at most once.
func (c *dashboardContext) maybeOpenStartupSession() bool {
	panic("unported: mj-cli/src/dashboard/session_state.rs maybe_open_startup_session")
}

func (c *dashboardContext) resolveProjectSources() {
	panic("unported: mj-cli/src/dashboard/session_state.rs resolve_project_sources")
}

// finishMaterializedProjection: detail is the prepared projection, nil when
// err reports why it could not be prepared.
func (c *dashboardContext) finishMaterializedProjection(
	sessionID string,
	detail *tui.PreparedMaterializedSessionDetail,
	err error,
) {
	panic("unported: mj-cli/src/dashboard/session_state.rs finish_materialized_projection")
}
