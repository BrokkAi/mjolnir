// Port of mj-tui/src/notify.rs.

// Desktop and bell alerts consume the daemon notify-once event stream.

package tui

import (
	"fmt"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// Notification is one thing worth interrupting the person for.
type Notification struct {
	SessionID string
	// SessionTitle is the session's display name, for the notification's
	// title.
	SessionTitle string
	Level        AttentionLevel
	// Body is one line saying what happened: the question, the failure, or
	// that the agent finished.
	Body string
}

// PendingAlert is an alert event waiting out the configured notify delay.
type PendingAlert struct {
	event    client.AlertEvent
	received time.Time
}

func (d *DashboardState) attentionNoticeLevel(sessionID string) AttentionLevel {
	panic("unported: mj-tui/src/notify.rs DashboardState::attention_notice_level")
}

func (d *DashboardState) failureDrawn(sessionID string) {
	panic("unported: mj-tui/src/notify.rs DashboardState::failure_drawn")
}

// TakeDrawnFailureDismissals drains only failures actually drawn; the host
// submits these through the same supervised action as an explicit
// notification dismissal.
func (d *DashboardState) TakeDrawnFailureDismissals() []DashboardAction {
	panic("unported: mj-tui/src/notify.rs DashboardState::take_drawn_failure_dismissals")
}

// SetAlertEvents advances the durable stream, suppressing history on initial
// attachment.
func (d *DashboardState) SetAlertEvents(high uint64, events []client.AlertEvent) {
	panic("unported: mj-tui/src/notify.rs DashboardState::set_alert_events")
}

func (d *DashboardState) NotificationEvents() []Notification {
	return d.notificationEventsAt(time.Now())
}

func (d *DashboardState) notificationEventsAt(now time.Time) []Notification {
	panic("unported: mj-tui/src/notify.rs DashboardState::notification_events_at")
}

func (d *DashboardState) notificationContentSession(sessionID string, level AttentionLevel) string {
	panic("unported: mj-tui/src/notify.rs DashboardState::notification_content_session")
}

func (d *DashboardState) notificationCardText(sessionID string, level AttentionLevel) string {
	panic("unported: mj-tui/src/notify.rs DashboardState::notification_card_text")
}

// notificationMessage is what to say about a session at level: the
// question's own message, the recorded error, or the last line the agent
// wrote.
func (d *DashboardState) notificationMessage(sessionID string, level AttentionLevel) string {
	panic("unported: mj-tui/src/notify.rs DashboardState::notification_message")
}

// ReportSessionUnreachable reports that a session's worker cannot be
// reached, in one plain sentence. The host keeps the relay's own error and
// any worker diagnostics for its log; the footer only says what happened.
func (d *DashboardState) ReportSessionUnreachable(sessionID string) {
	if _, reported := d.unreachableNotices[sessionID]; reported {
		return
	}
	name := d.sessionNoticeName(sessionID)
	text := name + " cannot be reached. The log has the worker's details."
	d.SetNotice(text)
	d.unreachableNotices[sessionID] = text
}

// ClearSessionUnreachable forgets an unreachable episode when its worker is
// no longer expected.
func (d *DashboardState) ClearSessionUnreachable(sessionID string) {
	panic("unported: mj-tui/src/notify.rs DashboardState::clear_session_unreachable")
}

// ReportSessionReachable withdraws the unreachable notice for a session
// whose worker answered again, if that notice is still the one showing.
func (d *DashboardState) ReportSessionReachable(sessionID string) {
	text, reported := d.unreachableNotices[sessionID]
	if !reported {
		return
	}
	delete(d.unreachableNotices, sessionID)
	name := d.sessionNoticeName(sessionID)
	d.ReplaceNoticeIf(text, name+" is reachable again.")
}

// ReportOpenFailure reports that a session could not be opened, naming it,
// so a failure from a restored pane says which of its sessions it was
// about.
func (d *DashboardState) ReportOpenFailure(sessionID string, errorText string) {
	name := d.sessionNoticeName(sessionID)
	detach := ""
	if key, ok := d.FirstKeyLabel(CommandIdQuitDetach); ok {
		detach = " " + key + " quits."
	}
	d.SetNotice(fmt.Sprintf("Could not open %s: %s. Press Enter in Sessions to retry, or select another session.%s", name, errorText, detach))
}

// sessionHasNoWorker reports whether sessionID is not running and no
// transition is bringing it back, so it has no worker to attach to.
func (d *DashboardState) sessionHasNoWorker(sessionID string) bool {
	panic("unported: mj-tui/src/notify.rs DashboardState::session_has_no_worker")
}

// PaneSessionIsSuspended reports whether a pane holding sessionID must let
// it go rather than attach to it: the session is suspended (or otherwise
// not running) and no transition is bringing it back.
//
// A harness-native child and a stopped Mjolnir sub-agent have no worker
// either, but their panes draw them from the store, so they stay (R10-2: a
// finished native child's row is Stopped, and Browse let it go as soon as
// the selection put it there).
func (d *DashboardState) PaneSessionIsSuspended(sessionID string) bool {
	return d.sessionHasNoWorker(sessionID) &&
		!d.IsNativeAgent(sessionID) &&
		!d.state.Get().Subagents.ContainsKey(sessionID)
}

// PaneSessionCanAttach reports whether a pane showing sessionID may start
// attaching to it: no lifecycle owns it, it has not failed, and it still
// has a worker. A suspended session has none, so an attach would only wait
// out its timeout and report "Session opening did not respond" (R4-11).
func (d *DashboardState) PaneSessionCanAttach(sessionID string) bool {
	panic("unported: mj-tui/src/notify.rs DashboardState::pane_session_can_attach")
}

// ReleaseSuspendedPane lets a pane go of a suspended session instead of
// attaching to a session with no worker. The host cancels the attach and
// saves the layout.
//
// Browse shows the selection, so it keeps a stopped row that is still
// listed because its failure needs a person: emptying it would clear the
// selection the row's keys act on. Any other suspended session has left the
// list, and Browse just empties. A pinned pane empties and says so.
func (d *DashboardState) ReleaseSuspendedPane(pane PaneId, sessionID string) {
	panic("unported: mj-tui/src/notify.rs DashboardState::release_suspended_pane")
}

func (d *DashboardState) saySuspendedSessionWasUnpinned(name string) {
	resume := ""
	if key, ok := d.FirstKeyLabel(CommandIdResumeDialog); ok {
		resume = " " + key + " finds it to resume."
	}
	d.SetNotice(name + " is suspended, so it was unpinned from its pane." + resume)
}

// sessionNoticeName is "Session" and the shared notice name, cut to fit a
// notice. See core's StateState.SessionNoticeName.
func (d *DashboardState) sessionNoticeName(sessionID string) string {
	return "Session " + FitSessionName(d.state.Get().SessionNoticeName(sessionID), NoticeNameCells)
}

// TerminalTitle is the terminal title the host should show: `mj` alone when
// nothing needs a person, otherwise counts such as `mj · 1 unreachable · 2
// waiting · 1 unread`. False when the configuration keeps the title alone.
func (d *DashboardState) TerminalTitle() (string, bool) {
	panic("unported: mj-tui/src/notify.rs DashboardState::terminal_title")
}
