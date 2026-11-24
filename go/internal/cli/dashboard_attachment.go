// Port of mj-cli/src/dashboard/attachment.rs.
//
// One cancellable attachment per session, shared by its display surfaces.
//
// Rust cancels a SessionAttachment when it is dropped; Go has no drop, so
// every owner that discards one calls drop. `select` and `defer` are Go
// keywords, so those methods are selectSession and deferAttach here.

package cli

import (
	"context"
	"fmt"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

const attachTimeout = 15 * time.Second

// nextAttachmentGeneration: records are retired with their last surface.
// Completion identities must remain unique when the same session later
// receives a fresh record. Rust starts the counter at 1 and fetch_adds; Add
// on a zero counter yields the same 1, 2, 3, ...
var nextAttachmentGeneration atomic.Uint64

type sessionAttachment struct {
	selection             *string
	assignments           map[tui.PaneId]paneInterest
	notificationRequested bool
	generation            uint64
	task                  *dashboardTask
}

// newSessionAttachment is Rust's `Default for SessionAttachment`.
func newSessionAttachment() *sessionAttachment {
	return &sessionAttachment{
		assignments: map[tui.PaneId]paneInterest{},
		generation:  nextAttachmentGeneration.Add(1),
	}
}

// paneInterest is a pane's assignment to this attachment: Requested, or
// Cancelled by Esc (Rust's two-variant enum, both carrying the assignment).
type paneInterest struct {
	assigned  tui.PaneAssignment
	cancelled bool
}

func (p paneInterest) assignment() tui.PaneAssignment {
	return p.assigned
}

// chatRetirement is the resources that existed when a lifecycle operation
// was admitted. The completion can retire these identities, never a
// subsequent attachment.
type chatRetirement struct {
	session string
	// chatInstance is nil when no chat was open for the session.
	chatInstance *chat.ChatInstance
	attachments  map[string]uint64
}

// chatRetirementCapture is Rust's ChatRetirement::capture.
func chatRetirementCapture(
	sessionID string,
	chats map[string]*chat.ActiveChat,
	attachments map[string]*sessionAttachment,
) chatRetirement {
	var instance *chat.ChatInstance
	if conversation, ok := chats[sessionID]; ok {
		captured := conversation.Instance()
		instance = &captured
	}
	captured := map[string]uint64{}
	for session, attachment := range attachments {
		if generation, ok := attachment.identity(sessionID); ok {
			captured[session] = generation
		}
	}
	return chatRetirement{session: sessionID, chatInstance: instance, attachments: captured}
}

func (r *chatRetirement) isCurrent(
	chats map[string]*chat.ActiveChat,
	attachments map[string]*sessionAttachment,
) bool {
	if conversation, ok := chats[r.session]; ok {
		if r.chatInstance == nil || !conversation.IsInstance(*r.chatInstance) {
			return false
		}
	}
	for session, attachment := range attachments {
		generation, ok := attachment.identity(r.session)
		if !ok {
			continue
		}
		if captured, had := r.attachments[session]; !had || captured != generation {
			return false
		}
	}
	return true
}

func (r *chatRetirement) sessionID() string {
	return r.session
}

func (r *chatRetirement) retireAttachments(attachments map[string]*sessionAttachment) {
	for session, generation := range r.attachments {
		if attachment, ok := attachments[session]; ok && attachment.accepts(generation, &r.session) {
			attachment.retire(r.session)
		}
	}
}

// chatRetainedWithoutConversation: an inline form or its accepted input
// retains its one logical chat even after navigation removes the
// conversation surface.
func chatRetainedWithoutConversation(
	dashboard *tui.DashboardState,
	chats map[string]*chat.ActiveChat,
	session string,
) bool {
	if form, ok := dashboard.NotificationFormSession(); ok && form == session {
		return true
	}
	conversation, ok := chats[session]
	return ok && conversation.HasPendingElicitationInput()
}

// wantedChatSessions is the sessions retained by live conversation or
// notification interests.
func wantedChatSessions(
	dashboard *tui.DashboardState,
	attachments map[string]*sessionAttachment,
	chats map[string]*chat.ActiveChat,
) map[string]struct{} {
	sessions := map[string]struct{}{}
	for _, entry := range dashboard.PaneSessions() {
		if attachment, ok := attachments[entry.SessionID]; ok && !attachment.paneCanShow(dashboard, entry.Pane) {
			continue
		}
		sessions[entry.SessionID] = struct{}{}
	}
	if session, ok := dashboard.NotificationFormSession(); ok {
		sessions[session] = struct{}{}
	}
	for session, conversation := range chats {
		if conversation.HasPendingElicitationInput() {
			sessions[session] = struct{}{}
		}
	}
	return sessions
}

// retireUnusedAttachments retires records as soon as neither the layout nor
// Notifications references them. A cancelled current assignment is retained
// to preserve Esc semantics.
func retireUnusedAttachments(dashboard *tui.DashboardState, attachments map[string]*sessionAttachment) {
	assigned := dashboard.PaneSessionIDs()
	notification, hasNotification := dashboard.NotificationFormSession()
	for session, attachment := range attachments {
		attachment.releaseUnownedTask(dashboard, session)
		if _, ok := assigned[session]; ok || (hasNotification && notification == session) {
			continue
		}
		// Rust's retain drops the record, which cancels it.
		attachment.drop()
		delete(attachments, session)
	}
}

// pruneAssignments forgets departed assignments, including closed panes,
// while retaining a current cancelled assignment until an explicit retry or
// navigation.
func (a *sessionAttachment) pruneAssignments(dashboard *tui.DashboardState, session string) {
	for pane, interest := range a.assignments {
		held, shown := dashboard.PaneSession(pane)
		assignment, assigned := dashboard.PaneAssignment(pane)
		if !shown || held != session || !assigned || assignment != interest.assignment() {
			delete(a.assignments, pane)
		}
	}
}

// observePane observes navigation before permitting an old task to join
// its new assignment. Work survives reassignment only through another live
// lease.
func (a *sessionAttachment) observePane(dashboard *tui.DashboardState, pane tui.PaneId, session string) {
	assignment, ok := dashboard.PaneAssignment(pane)
	if !ok {
		return
	}
	interest, had := a.assignments[pane]
	changed := !had || interest.assignment() != assignment
	if changed && a.pending() && !a.hasSurfaceInterest(dashboard, session) {
		a.cancel()
		a.selection = nil
	}
	a.bind(pane, assignment)
}

// bind: a navigation assignment joins the session's one attachment
// operation.
func (a *sessionAttachment) bind(pane tui.PaneId, assignment tui.PaneAssignment) {
	if interest, ok := a.assignments[pane]; ok && interest.assignment() == assignment {
		return
	}
	a.requestPane(pane, assignment)
}

// requestPane: an explicit retry can rejoin an assignment that Esc
// cancelled.
func (a *sessionAttachment) requestPane(pane tui.PaneId, assignment tui.PaneAssignment) {
	current, ok := a.assignments[pane]
	unchanged := ok && !current.cancelled && current.assigned == assignment
	a.assignments[pane] = paneInterest{assigned: assignment}
	if !unchanged && !a.pending() {
		a.selection = nil
	}
}

// bindNotification observes the notification lease so a newly displayed
// card can start work even when its conversation's earlier attempt was
// cancelled.
func (a *sessionAttachment) bindNotification(requested bool) {
	if requested && !a.notificationRequested && !a.pending() {
		a.selection = nil
	}
	a.notificationRequested = requested
}

func (a *sessionAttachment) panePending(pane tui.PaneId) bool {
	interest, ok := a.assignments[pane]
	return a.pending() && ok && !interest.cancelled
}

func (a *sessionAttachment) releasePane(dashboard *tui.DashboardState, pane tui.PaneId, session string) {
	if assignment, ok := dashboard.PaneAssignment(pane); ok {
		a.assignments[pane] = paneInterest{assigned: assignment, cancelled: true}
	}
	if !a.hasSurfaceInterest(dashboard, session) {
		a.cancel()
	}
}

// paneCanShow: access follows this assignment's interest, even if another
// surface completed the session's shared attach after this pane cancelled.
func (a *sessionAttachment) paneCanShow(dashboard *tui.DashboardState, pane tui.PaneId) bool {
	interest, ok := a.assignments[pane]
	if !ok || interest.cancelled {
		return false
	}
	assignment, assigned := dashboard.PaneAssignment(pane)
	return assigned && assignment == interest.assigned
}

func (a *sessionAttachment) hasConversationInterest(dashboard *tui.DashboardState, session string) bool {
	for _, entry := range dashboard.PaneSessions() {
		if entry.SessionID == session && a.paneCanShow(dashboard, entry.Pane) {
			return true
		}
	}
	return false
}

func (a *sessionAttachment) hasSurfaceInterest(dashboard *tui.DashboardState, session string) bool {
	if form, ok := dashboard.NotificationFormSession(); ok && form == session {
		return true
	}
	return a.hasConversationInterest(dashboard, session)
}

// acceptsPaneResult is test-only in Rust (`#[cfg(test)]`).
func (a *sessionAttachment) acceptsPaneResult(
	generation uint64,
	assignment tui.PaneAssignment,
	dashboard *tui.DashboardState,
	pane tui.PaneId,
	session string,
) bool {
	if !a.accepts(generation, &session) {
		return false
	}
	current, ok := a.assignments[pane]
	if !ok || current.cancelled || current.assigned != assignment {
		return false
	}
	shownAssignment, assigned := dashboard.PaneAssignment(pane)
	if !assigned || shownAssignment != assignment {
		return false
	}
	held, shown := dashboard.PaneSession(pane)
	return shown && held == session
}

func (a *sessionAttachment) acceptsSurfaceResult(generation uint64, dashboard *tui.DashboardState, session string) bool {
	return a.accepts(generation, &session) && a.hasSurfaceInterest(dashboard, session)
}

func (a *sessionAttachment) pending() bool {
	return a.task != nil
}

func (a *sessionAttachment) finish() {
	a.task = nil
}

func (a *sessionAttachment) identity(sessionID string) (uint64, bool) {
	if a.selection != nil && *a.selection == sessionID {
		return a.generation, true
	}
	return 0, false
}

// selectSession observes selection even when its chat is already warm.
// Failed/cancelled opens stay observed so render/feed ticks cannot retry
// them indefinitely. (Rust's `select`.)
func (a *sessionAttachment) selectSession(session string) bool {
	if a.selection != nil && *a.selection == session {
		return false
	}
	selected := session
	a.selection = &selected
	return true
}

func (a *sessionAttachment) releaseUnownedTask(dashboard *tui.DashboardState, session string) {
	if a.pending() && !a.hasSurfaceInterest(dashboard, session) {
		a.cancel()
	}
}

func (a *sessionAttachment) selectForPane(pane tui.PaneId, session string) bool {
	if interest, ok := a.assignments[pane]; !ok || interest.cancelled {
		return false
	}
	return a.selectSession(session)
}

func (a *sessionAttachment) cancel() {
	a.generation = nextAttachmentGeneration.Add(1)
	if a.task != nil {
		a.task.abort()
		a.task = nil
	}
}

// deferAttach: a lifecycle owns the selected conversation temporarily.
// Unlike a failed/cancelled open, its completion should allow one fresh
// attempt. (Rust's `defer`.)
func (a *sessionAttachment) deferAttach() {
	hadSelection := a.selection != nil
	a.selection = nil
	if hadSelection || a.task != nil {
		a.cancel()
	}
}

// retire: retiring a warm chat permits one fresh attachment without
// disturbing an attachment already underway for another selected session.
func (a *sessionAttachment) retire(session string) {
	if a.selection != nil && *a.selection == session {
		a.deferAttach()
	}
}

// accepts: input may have changed the row before the event loop starts its
// new attachment. A queued completion must already respect that choice.
// selection nil means no session is selected.
func (a *sessionAttachment) accepts(generation uint64, selection *string) bool {
	if a.generation != generation {
		return false
	}
	if a.selection == nil || selection == nil {
		return a.selection == nil && selection == nil
	}
	return *a.selection == *selection
}

// sessionAttachmentSpawn is SessionAttachment::spawn, a package function
// because Go methods take no type parameters. work's error is the failure
// text Rust carries as `Err(String)`.
//
// work runs under a context that ends when the attachment is cancelled or
// the timeout passes, which is how Rust's AbortOnDropHandle drops the
// future. A cancelled attachment reports nothing; a timed-out or panicking
// one reports its failure, so the spinner never runs forever.
func sessionAttachmentSpawn[T any](
	a *sessionAttachment,
	session string,
	timeout time.Duration,
	work func(ctx context.Context) (T, error),
	report func(generation uint64, value T, err error),
) {
	a.cancel()
	a.selectSession(session)
	generation := a.generation
	a.task = spawnDashboardTask(func(ctx context.Context) {
		workCtx, cancelWork := context.WithTimeout(ctx, timeout)
		defer cancelWork()
		type outcome struct {
			value T
			err   error
		}
		worker := core.Spawn(func() outcome {
			value, err := work(workCtx)
			return outcome{value: value, err: err}
		})
		var (
			value T
			err   error
		)
		select {
		case <-worker.Done():
			finished, joinErr := worker.Join(context.Background())
			if joinErr != nil {
				err = fmt.Errorf("session opening task failed: %w", joinErr)
			} else {
				value, err = finished.value, finished.err
			}
		case <-workCtx.Done():
			if ctx.Err() != nil {
				// Aborted: the attachment moved on and wants no answer.
				return
			}
			err = fmt.Errorf("Session opening did not respond within %d seconds", int64(timeout/time.Second))
		}
		if ctx.Err() != nil {
			return
		}
		report(generation, value, err)
	})
}

// drop is Rust's `Drop for SessionAttachment`: it cancels the attachment.
func (a *sessionAttachment) drop() {
	a.cancel()
}
