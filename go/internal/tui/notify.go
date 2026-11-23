// Port of mj-tui/src/notify.rs.

// Desktop and bell alerts consume the daemon notify-once event stream.

package tui

import (
	"fmt"
	"iter"
	"maps"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	level := d.AttentionLevel(sessionID)
	// Published cards choose which session interrupts for a shared question.
	if (level == AttentionLevelWaiting || level == AttentionLevelImportant) && d.notificationCards != nil {
		card := d.notificationCards.GetShared(sessionID)
		switch {
		case card == nil:
			return AttentionLevelIdle
		case card.Badge == core.NotificationKindNeedsInput:
			return AttentionLevelWaiting
		default:
			return AttentionLevelImportant
		}
	}
	return level
}

func (d *DashboardState) failureDrawn(sessionID string) {
	if d.ModalOpen() {
		return
	}
	if target, ok := d.notificationDismissalTarget(sessionID); ok {
		if _, failure := target.(core.NotificationDismissalFailure); failure {
			if d.renderedFailureDismissals == nil {
				d.renderedFailureDismissals = map[string]struct{}{}
			}
			d.renderedFailureDismissals[sessionID] = struct{}{}
		}
	}
}

// TakeDrawnFailureDismissals drains only failures actually drawn; the host
// submits these through the same supervised action as an explicit
// notification dismissal.
func (d *DashboardState) TakeDrawnFailureDismissals() []DashboardAction {
	drawn := d.renderedFailureDismissals
	d.renderedFailureDismissals = map[string]struct{}{}
	var actions []DashboardAction
	for _, id := range slices.Sorted(maps.Keys(drawn)) {
		if action, ok := d.beginDismissNotificationFor(id); ok {
			actions = append(actions, action)
		}
	}
	return actions
}

// SetAlertEvents advances the durable stream, suppressing history on initial
// attachment.
func (d *DashboardState) SetAlertEvents(high uint64, events []client.AlertEvent) {
	if d.alertSequence == nil {
		d.alertSequence = &high
		return
	}
	after := *d.alertSequence
	visible, hasVisible := d.CurrentSessionID()
	for _, event := range events {
		if event.Sequence <= after ||
			(hasVisible && visible == event.Destination) ||
			d.config.Get().Notify.Mode == core.NotifyModeOff {
			continue
		}
		d.pendingAlerts = append(d.pendingAlerts, PendingAlert{event: event, received: time.Now()})
	}
	next := max(after, high)
	d.alertSequence = &next
}

func (d *DashboardState) NotificationEvents() []Notification {
	return d.notificationEventsAt(time.Now())
}

func (d *DashboardState) notificationEventsAt(now time.Time) []Notification {
	pending := d.pendingAlerts
	d.pendingAlerts = nil
	notify := d.config.Get().Notify
	if notify.Mode == core.NotifyModeOff {
		return nil
	}
	delay := time.Duration(notify.DelaySeconds) * time.Second
	var notices []Notification
	for _, alert := range pending {
		// Rust's Instant::saturating_duration_since.
		if max(now.Sub(alert.received), 0) < delay {
			d.pendingAlerts = append(d.pendingAlerts, alert)
			continue
		}
		event := alert.event
		// The daemon made the event from a published item, so with no delay
		// it rings as is. A delayed event rings only if its item survived the
		// delay; the card map may lag the event batch, which a delay absorbs.
		published := delay == 0 || d.alertItemStillPublished(&event)
		if current, ok := d.CurrentSessionID(); !published || (ok && current == event.Destination) {
			continue
		}
		level := AttentionLevelImportant
		if event.Kind == core.NotificationKindNeedsInput {
			level = AttentionLevelWaiting
		}
		notices = append(notices, Notification{
			SessionID:    event.Destination,
			SessionTitle: event.SessionTitle,
			Body:         event.Text,
			Level:        level,
		})
	}
	return notices
}

// alertItemStillPublished reports whether a published card still holds the
// item an alert event was made from, on a session that is not muted.
func (d *DashboardState) alertItemStillPublished(event *client.AlertEvent) bool {
	if d.notificationCards == nil {
		return false
	}
	state := d.state.Get()
	for session, card := range d.notificationCards.All() {
		if record := state.Sessions.GetShared(session); record != nil && record.NotificationsIgnored {
			continue
		}
		items := len(card.Entries)
		if card.Form != nil {
			items++
		}
		for item := range items {
			identity, ok := card.ItemIdentity(session, item)
			if card.ItemDestination(session, item) == event.Destination && ok && identity == event.Identity {
				return true
			}
		}
	}
	return false
}

func (d *DashboardState) notificationContentSession(sessionID string, level AttentionLevel) string {
	if level == AttentionLevelWaiting {
		if child, ok := d.waitingChildOf(sessionID); ok {
			return child
		}
	}
	return sessionID
}

func (d *DashboardState) notificationCardText(sessionID string, level AttentionLevel) string {
	holder := d.notificationContentSession(sessionID, level)
	detail, found := (*d.sessionDetails.Get())[holder]
	loading := found && detail.transcriptHydration == TranscriptHydrationLoading
	missingContent := !found || (detail.lastAgentMessage == nil &&
		len(detail.pendingElicitations) == 0 &&
		detail.questionMessage == nil)
	if loading && missingContent {
		return "loading…"
	}
	return d.notificationMessage(sessionID, level)
}

// notificationMessage is what to say about a session at level: the
// question's own message, the recorded error, or the last line the agent
// wrote.
func (d *DashboardState) notificationMessage(sessionID string, level AttentionLevel) string {
	if (level == AttentionLevelWaiting || level == AttentionLevelImportant) && d.notificationCards != nil {
		if card := d.notificationCards.GetShared(sessionID); card != nil {
			items := len(card.Entries)
			if card.Form != nil {
				items++
			}
			return d.notificationItemText(sessionID, card, max(items-1, 0))
		}
	}
	holder := d.notificationContentSession(sessionID, level)
	sessionID = holder
	detail := (*d.sessionDetails.Get())[sessionID]
	var locatedRead *NotificationMessageRead
	if read := d.notifications.messageRead; read != nil && read.request.SessionID == holder &&
		detail != nil && detail.questionMessage != nil {
		if item, ok := read.request.Source.(NotificationReadSourceItem); ok && item.Value == *detail.questionMessage {
			locatedRead = read
		}
	}
	switch level {
	case AttentionLevelWaiting, AttentionLevelImportant:
		kind := core.NotificationKindImportant
		if level == AttentionLevelWaiting {
			kind = core.NotificationKindNeedsInput
		}
		facts := core.NotificationMessageFacts{SupportsOpenRequest: detail == nil || detail.supportsOpenRequest}
		if detail != nil && len(detail.pendingElicitations) > 0 {
			message := detail.pendingElicitations[len(detail.pendingElicitations)-1].Message
			facts.PendingRequest = &message
		}
		if activity, ok := client.CombinedActivityLabel(notifyReviewViews(d.sessionReviews(sessionID))); ok {
			facts.ReviewActivity = &activity
		}
		if detail != nil {
			facts.QuestionMessage = detail.questionMessage
		}
		for _, source := range core.NotificationMessageSources(kind, &facts) {
			switch source := source.(type) {
			case core.NotificationMessageSourcePendingRequest:
				return source.Value
			case core.NotificationMessageSourceReviewActivity:
				return source.Value
			case core.NotificationMessageSourceQuestionMessage:
				if detail == nil {
					continue
				}
				if text, ok := notifyAgentMessageText(detail.projection.transcript, source.Value); ok {
					return text
				}
				if locatedRead != nil {
					if locatedRead.status == TranscriptHydrationLoading {
						return "loading…"
					}
					if locatedRead.text != nil {
						return *locatedRead.text
					}
					return "message not available"
				}
			case core.NotificationMessageSourceLastAgentMessage:
				if detail != nil && detail.lastAgentMessage != nil {
					return *detail.lastAgentMessage
				}
			}
		}
		return core.NotificationMessagePlaceholder(kind)
	case AttentionLevelFailed:
		if session := d.state.Get().Sessions.GetShared(sessionID); session != nil && session.LastError != nil {
			return *session.LastError
		}
		return "Session failed"
	case AttentionLevelUnread:
		if detail != nil && detail.unreadInterruptions > 0 {
			return "Work was interrupted"
		}
	}
	if detail != nil && detail.lastAgentMessage != nil {
		return *detail.lastAgentMessage
	}
	return "Finished"
}

// notifyReviewViews yields each review view by reference, for
// client.CombinedActivityLabel.
func notifyReviewViews(reviews []client.RuntimeReviewView) iter.Seq[*client.RuntimeReviewView] {
	return func(yield func(*client.RuntimeReviewView) bool) {
		for index := range reviews {
			if !yield(&reviews[index]) {
				return
			}
		}
	}
}

// notifyAgentMessageText is core.MaterializedAgentMessageText over the
// shared transcript items a projection holds.
func notifyAgentMessageText(items []*core.TranscriptItem, stableID string) (string, bool) {
	for _, item := range items {
		if item.StableID != stableID {
			continue
		}
		if agent, ok := item.Body.(core.TranscriptBodyAgent); ok {
			return core.MaterializedChunksText(agent.Chunks), true
		}
		return "", false
	}
	return "", false
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
	if d.unreachableNotices == nil {
		d.unreachableNotices = map[string]string{}
	}
	d.unreachableNotices[sessionID] = text
}

// ClearSessionUnreachable forgets an unreachable episode when its worker is
// no longer expected.
func (d *DashboardState) ClearSessionUnreachable(sessionID string) {
	text, reported := d.unreachableNotices[sessionID]
	if !reported {
		return
	}
	delete(d.unreachableNotices, sessionID)
	if notice, ok := d.Notice(); ok && notice == text {
		d.ClearNotice()
	}
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
	if d.TransitionKind(sessionID) != nil {
		return false
	}
	session := d.state.Get().Sessions.GetShared(sessionID)
	return session != nil && !session.State.HasLiveWorker()
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
	return d.TransitionKind(sessionID) == nil &&
		d.TransitionFailureKind(sessionID) == nil &&
		!d.SessionFailed(sessionID) &&
		!d.PaneSessionIsSuspended(sessionID)
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
	if pane == d.BrowsePane() {
		if !d.stoppedNotificationRow(sessionID) {
			d.SetPaneSession(pane, nil)
		}
		return
	}
	d.SetPaneSession(pane, nil)
	name := d.sessionNoticeName(sessionID)
	d.saySuspendedSessionWasUnpinned(name)
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
	if !d.config.Get().Notify.Title {
		return "", false
	}
	// Questions, unreachable workers and failures each get their own word,
	// most urgent first: "waiting" means a question for a person.
	var failed, unreachable, waiting, important, unread int
	for _, entry := range d.AttentionQueue() {
		switch entry.Level {
		case AttentionLevelFailed:
			failed++
		case AttentionLevelUnreachable:
			unreachable++
		case AttentionLevelWaiting:
			waiting++
		case AttentionLevelImportant:
			important++
		case AttentionLevelUnread:
			unread++
		}
	}
	title := "mj"
	for _, count := range []struct {
		count int
		word  string
	}{
		{failed, "failed"},
		{unreachable, "unreachable"},
		{waiting, "waiting"},
		{important, "important"},
		{unread, "unread"},
	} {
		if count.count > 0 {
			title += fmt.Sprintf(" · %d %s", count.count, count.word)
		}
	}
	return title, true
}
