// Port of mj-tui/src/notifications_pane.rs.

// The daemon publishes notification cards. This pane owns item selection,
// deferrals, per-session reply editors and their submission feedback.

package tui

import (
	"cmp"
	"fmt"
	"math"
	"reflect"
	"slices"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NotificationsPane is the Notifications support pane's own state. Make one
// with newNotificationsPane (Rust's Default).
type NotificationsPane struct {
	size            core.PaneSize
	maximizeEnabled bool
	skipped         []SkippedCard
	errors          map[string]string
	gesture         *NotificationGesture
	controls        []NotificationGesture
	messageRead     *NotificationMessageRead
	// replies is keyed by session; iterate it in sorted key order, as
	// Rust's BTreeMap does.
	replies    map[string]*QuickReply
	formErrors map[string]string
	formArea   *uv.Rectangle
	scroll     uint16
	// scrollCard is the last displayed card, used to match the selected
	// item across publications.
	scrollCard           *CardIdentity
	selectedItem         int
	selectionRevision    uint64
	renderedPresentation *notificationPresentation
	textGeneration       uint64
	textRows             *chat.SelectableText
	formText             *notificationFormText
}

// notificationPresentation is NotificationsPane.renderedPresentation's
// `(selection_revision, size)` pair.
type notificationPresentation struct {
	selectionRevision uint64
	size              core.PaneSize
}

// notificationFormText is NotificationsPane.formText's `(text, width)`
// pair: the inline form's message and the width it was laid out at.
type notificationFormText struct {
	text  string
	width int
}

// newNotificationsPane is NotificationsPane::default.
func newNotificationsPane() NotificationsPane {
	return NotificationsPane{
		size:            core.PaneSizeStandard,
		maximizeEnabled: true,
		errors:          map[string]string{},
		replies:         map[string]*QuickReply{},
		formErrors:      map[string]string{},
	}
}

// QuickReply is one session's detached reply editor.
type QuickReply struct {
	editor  *chat.ChatState
	pending []ReplySubmission
	error   *string
	// recoverable holds failed submissions and drafts displaced by recovery.
	// They remain recoverable independently of the current editor, until
	// that text is sent successfully.
	recoverable []RecoverableReply
}

type RecoverableReply struct {
	text  string
	error *string
}

func (r *QuickReply) restoreCandidate() (string, bool) {
	current := r.editor.SingleLinePromptText()
	for _, reply := range r.recoverable {
		if reply.text != current {
			return reply.text, true
		}
	}
	return "", false
}

func (r *QuickReply) keepUnsent(text string, err *string) {
	for index := range r.recoverable {
		if r.recoverable[index].text == text {
			if err != nil {
				r.recoverable[index].error = err
			}
			return
		}
	}
	if strings.TrimSpace(text) != "" {
		r.recoverable = append(r.recoverable, RecoverableReply{text: text, error: err})
	}
}

// GoString is QuickReply's Debug, which shows only the pending submissions.
func (r *QuickReply) GoString() string {
	return fmt.Sprintf("QuickReply { pending: %#v, .. }", r.pending)
}

type ReplySubmission struct {
	card      CardIdentity
	commandID string
	text      string
}

// NotificationReadSource is where a notification item's message is read
// from: a located message (NotificationReadSourceItem) or the transcript
// through a completed ordinal (NotificationReadSourceThrough).
type NotificationReadSource interface {
	isNotificationReadSource()
}

type NotificationReadSourceItem struct{ Value string }

type NotificationReadSourceThrough struct{ Value uint64 }

func (NotificationReadSourceItem) isNotificationReadSource()    {}
func (NotificationReadSourceThrough) isNotificationReadSource() {}

// CompareNotificationReadSource is NotificationReadSource's derived Ord:
// Item before Through, then by value.
func CompareNotificationReadSource(a, b NotificationReadSource) int {
	rank := func(source NotificationReadSource) int {
		if _, through := source.(NotificationReadSourceThrough); through {
			return 1
		}
		return 0
	}
	if order := cmp.Compare(rank(a), rank(b)); order != 0 {
		return order
	}
	switch a := a.(type) {
	case NotificationReadSourceItem:
		return strings.Compare(a.Value, b.(NotificationReadSourceItem).Value)
	case NotificationReadSourceThrough:
		return cmp.Compare(a.Value, b.(NotificationReadSourceThrough).Value)
	}
	return 0
}

type NotificationReadRequest struct {
	SessionID string
	Source    NotificationReadSource
	// SelectionRevision changes on each card or item transition, including a
	// return to the same item.
	SelectionRevision uint64
}

// Compare is NotificationReadRequest's derived Ord, field by field.
func (r NotificationReadRequest) Compare(other NotificationReadRequest) int {
	if order := strings.Compare(r.SessionID, other.SessionID); order != 0 {
		return order
	}
	if order := CompareNotificationReadSource(r.Source, other.Source); order != 0 {
		return order
	}
	return cmp.Compare(r.SelectionRevision, other.SelectionRevision)
}

type CardIdentity struct {
	action    NotificationCard
	published *client.NotificationCard
}

// equal is CardIdentity's derived PartialEq; the published card compares by
// value, as Rust's Arc does.
func (c *CardIdentity) equal(other *CardIdentity) bool {
	return reflect.DeepEqual(c.action, other.action) && reflect.DeepEqual(c.published, other.published)
}

// SkippedCard is a deferred card: a published card by its session, or a
// legacy card by its full identity.
type SkippedCard interface {
	isSkippedCard()
}

type SkippedCardPublished struct{ Value string }

type SkippedCardLegacy struct{ Value NotificationCard }

func (SkippedCardPublished) isSkippedCard() {}
func (SkippedCardLegacy) isSkippedCard()    {}

// skippedCardEqual is SkippedCard's derived PartialEq.
func skippedCardEqual(a, b SkippedCard) bool {
	return reflect.DeepEqual(a, b)
}

func (c *CardIdentity) skipIdentity() SkippedCard {
	if c.published != nil {
		return SkippedCardPublished{Value: c.action.SessionID}
	}
	return SkippedCardLegacy{Value: c.action}
}

func (p *NotificationsPane) prunePublishedSkips(contains func(string) bool) {
	p.skipped = slices.DeleteFunc(p.skipped, func(card SkippedCard) bool {
		published, ok := card.(SkippedCardPublished)
		return ok && !contains(published.Value)
	})
}

type NotificationMessageRead struct {
	request NotificationReadRequest
	status  TranscriptHydration
	text    *string
}

type NotificationGesture struct {
	command           CommandId
	identity          CardIdentity
	selectionRevision uint64
	sessionID         string
	text              string
	restoreText       *string
}

type NotificationEntry struct {
	sessionID string
	level     AttentionLevel
	title     string
	project   string
	recency   uint64
	target    core.NotificationDismissal
	published *client.NotificationCard
}

func (e *NotificationEntry) actionCard() NotificationCard {
	return NotificationCard{SessionID: e.sessionID, Target: e.target}
}

func (e *NotificationEntry) identity() CardIdentity {
	return CardIdentity{action: e.actionCard(), published: e.published}
}

func (e *NotificationEntry) matches(card *CardIdentity) bool {
	return e.sessionID == card.action.SessionID &&
		reflect.DeepEqual(e.target, card.action.Target) &&
		reflect.DeepEqual(e.published, card.published)
}

func (e *NotificationEntry) matchesSkip(skipped SkippedCard) bool {
	switch skipped := skipped.(type) {
	case SkippedCardPublished:
		return e.published != nil && e.sessionID == skipped.Value
	case SkippedCardLegacy:
		return e.published == nil &&
			e.sessionID == skipped.Value.SessionID &&
			reflect.DeepEqual(e.target, skipped.Value.Target)
	}
	return false
}

func (e *NotificationEntry) retainedItemIndex(previous *CardIdentity, index int) (int, bool) {
	panic("unported: mj-tui/src/notifications_pane.rs retained_item_index")
}

func (e *NotificationEntry) itemCount() int {
	if e.published == nil {
		return 1
	}
	count := len(e.published.Entries)
	if e.published.Form != nil {
		count++
	}
	return count
}

func (d *DashboardState) quickReplyMut(sessionID string) *QuickReply {
	panic("unported: mj-tui/src/notifications_pane.rs quick_reply_mut")
}

// handleNotificationKey returns nil (Rust's None) when the key is not the
// Notifications pane's to handle.
func (d *DashboardState) handleNotificationKey(key tea.KeyPressMsg) DashboardAction {
	panic("unported: mj-tui/src/notifications_pane.rs handle_notification_key")
}

func (d *DashboardState) pasteNotificationReply(text string) {
	if _, form := d.NotificationFormSession(); form {
		return
	}
	if sessionID, ok := d.DisplayedNotificationSessionID(); ok {
		d.quickReplyMut(sessionID).editor.PasteSingleLinePrompt(text)
	}
}

// ServiceNotificationComposers pumps detached editors through the same
// history owner as ActiveChat. persistence returns nil when a session's
// editor cannot reach the daemon.
func (d *DashboardState) ServiceNotificationComposers(
	control client.SessionControl,
	persistence func(sessionID string) chat.ChatDaemonSender,
) bool {
	panic("unported: mj-tui/src/notifications_pane.rs service_notification_composers")
}

func (d *DashboardState) ReportNotificationReplyFailure(sessionID, text, errorText, recovery string) {
	d.SetFailureNotice(fmt.Sprintf(
		"Quick reply to %s: %s. %s\nUnsent text:\n%s",
		d.sessionNoticeName(sessionID), errorText, recovery, text,
	))
}

func (d *DashboardState) revealNotificationReplyError(sessionID string) {
	if displayed, ok := d.DisplayedNotificationSessionID(); ok && displayed == sessionID {
		d.notifications.scroll = 0
	}
}

func (d *DashboardState) NotificationReplyUnconfirmed(sessionID, commandID, errorText string) bool {
	panic("unported: mj-tui/src/notifications_pane.rs notification_reply_unconfirmed")
}

// NotificationReplyFinished: a durable prompt outcome belongs to its
// original session and command, even when another notification has
// replaced the displayed card. A nil result is Rust's Ok(()); a non-nil one
// carries the failure text.
func (d *DashboardState) NotificationReplyFinished(sessionID, commandID string, result error) bool {
	panic("unported: mj-tui/src/notifications_pane.rs notification_reply_finished")
}

func (d *DashboardState) notificationAction(command CommandId) DashboardAction {
	gesture := d.notificationGesture(command)
	if gesture == nil {
		return DashboardActionNone{}
	}
	return d.activateNotification(*gesture)
}

func (d *DashboardState) notificationEntries() []NotificationEntry {
	panic("unported: mj-tui/src/notifications_pane.rs notification_entries")
}

func (d *DashboardState) notificationEntry(
	id string,
	level AttentionLevel,
	target core.NotificationDismissal,
	published *client.NotificationCard,
) *NotificationEntry {
	panic("unported: mj-tui/src/notifications_pane.rs notification_entry")
}

func (d *DashboardState) notificationDestination(entry *NotificationEntry) string {
	if entry.published != nil {
		return entry.published.ItemDestination(entry.sessionID, d.selectedNotificationItem(entry))
	}
	return d.notificationContentSession(entry.sessionID, entry.level)
}

func (d *DashboardState) resetNotificationItem() {
	pane := &d.notifications
	pane.scroll = 0
	pane.textGeneration++
	pane.textRows = nil
	pane.formText = nil
	pane.formArea = nil
	pane.messageRead = nil
	pane.selectionRevision++
}

func (d *DashboardState) syncNotificationSelection(entry *NotificationEntry) {
	panic("unported: mj-tui/src/notifications_pane.rs sync_notification_selection")
}

func (d *DashboardState) canNavigateNotification(command CommandId) bool {
	entries := d.notificationEntries()
	if len(entries) == 0 || entries[0].published == nil {
		return false
	}
	_, ok := d.notificationItemInDirection(&entries[0], command)
	return ok
}

func (d *DashboardState) retainedNotificationItem(entry *NotificationEntry) (int, bool) {
	previous := d.notifications.scrollCard
	if previous == nil {
		return 0, false
	}
	return entry.retainedItemIndex(previous, d.notifications.selectedItem)
}

func (d *DashboardState) selectedNotificationItem(entry *NotificationEntry) int {
	if index, ok := d.retainedNotificationItem(entry); ok {
		return index
	}
	return max(entry.itemCount()-1, 0)
}

func (d *DashboardState) notificationItemInDirection(entry *NotificationEntry, command CommandId) (int, bool) {
	index := d.selectedNotificationItem(entry)
	switch command {
	case CommandIdNotificationPreviousMessage:
		if index > 0 {
			return index - 1, true
		}
	case CommandIdNotificationNextMessage:
		if index+1 < entry.itemCount() {
			return index + 1, true
		}
	}
	return 0, false
}

// DisplayedNotificationSessionID is the session whose content the displayed
// card needs, including a child form promoted to its parent's row.
func (d *DashboardState) DisplayedNotificationSessionID() (string, bool) {
	entries := d.notificationEntries()
	if len(entries) == 0 {
		return "", false
	}
	return d.notificationDestination(&entries[0]), true
}

// notificationFormContentSession: a card requests a warm chat; only that
// chat decides which form it shows.
func (d *DashboardState) notificationFormContentSession() (string, bool) {
	entries := d.notificationEntries()
	if len(entries) == 0 {
		return "", false
	}
	entry := &entries[0]
	card := entry.published
	formSelected := card != nil && card.Form != nil && card.Form.RequiresElicitation() &&
		d.selectedNotificationItem(entry) == len(card.Entries)
	if entry.level != AttentionLevelWaiting || !formSelected {
		return "", false
	}
	return d.notificationDestination(entry), true
}

func (d *DashboardState) NotificationFormSession() (string, bool) {
	if d.notifications.size == core.PaneSizeMinimized {
		return "", false
	}
	return d.notificationFormContentSession()
}

func (d *DashboardState) notificationPresentationChanged() bool {
	rendered := d.notifications.renderedPresentation
	return rendered == nil || *rendered != notificationPresentation{
		selectionRevision: d.notifications.selectionRevision,
		size:              d.notifications.size,
	}
}

// ReportChatAttachmentResult: a notification-only attach reports in its
// card; a requested conversation's open keeps its shared opening/retry
// feedback. A nil errorText is a successful attach.
func (d *DashboardState) ReportChatAttachmentResult(session string, errorText *string, conversationOpening bool) {
	d.SetNotificationFormError(session, errorText)
	if conversationOpening {
		if errorText != nil {
			d.ReportOpenFailure(session, *errorText)
		} else {
			d.ClearNotice()
		}
	}
}

func (d *DashboardState) SetNotificationFormError(session string, errorText *string) {
	if errorText != nil {
		d.notifications.formErrors[session] = *errorText
	} else {
		delete(d.notifications.formErrors, session)
	}
}

// NotificationInputNeedsFrame: input batching refreshes newly selected form
// content before any next event.
func (d *DashboardState) NotificationInputNeedsFrame() bool {
	_, form := d.NotificationFormSession()
	return form && d.notificationPresentationChanged()
}

func (d *DashboardState) NotificationFormOwnsKeyboard() bool {
	if d.focus != FocusNotifications || d.notifications.formArea == nil {
		return false
	}
	session, ok := d.NotificationFormSession()
	return ok && d.ChatInputAvailable(session)
}

// ConversationPointerSession: pointer capture remains with its owning
// conversation across sessions.
func (d *DashboardState) ConversationPointerSession(chats map[string]*chat.ActiveChat) (string, bool) {
	panic("unported: mj-tui/src/notifications_pane.rs conversation_pointer_session")
}

// NotificationFormMouseOwnership reports (owned, true) when this
// presentation decides the pointer, and ok false (Rust's None) when it does
// not. owned false is message/chrome in this presentation: another
// conversation's geometry must not claim text that is drawn here.
func (d *DashboardState) NotificationFormMouseOwnership(chats map[string]*chat.ActiveChat, mouse tea.MouseMsg) (owned bool, ok bool) {
	panic("unported: mj-tui/src/notifications_pane.rs notification_form_mouse_ownership")
}

// HandleNotificationFormEvent applies notification input to the owning
// chat's ordinary event dispatcher; ok false is Rust's None.
func (d *DashboardState) HandleNotificationFormEvent(
	chats map[string]*chat.ActiveChat,
	event tea.Msg,
) (chat.EventResult[chat.ChatEventOutcome], bool) {
	panic("unported: mj-tui/src/notifications_pane.rs handle_notification_form_event")
}

// notificationFormHandlesEvent routes only the form body here; the host
// still owns card buttons and menus.
func (d *DashboardState) notificationFormHandlesEvent(event tea.Msg) bool {
	panic("unported: mj-tui/src/notifications_pane.rs notification_form_handles_event")
}

// DisplayedNotificationParentID keeps the existing child-family runtime
// subscription while its approval is displayed, even when the family is
// outside the active workspace.
func (d *DashboardState) DisplayedNotificationParentID() (string, bool) {
	entries := d.notificationEntries()
	if len(entries) == 0 {
		return "", false
	}
	entry := &entries[0]
	if d.notificationDestination(entry) == entry.sessionID {
		return "", false
	}
	return entry.sessionID, true
}

// WantedSummarySessions is the one owner for the existing background
// summary-hydration selection: session id to its viewed-through ordinal.
func (d *DashboardState) WantedSummarySessions() map[string]uint64 {
	panic("unported: mj-tui/src/notifications_pane.rs wanted_summary_sessions")
}

func (d *DashboardState) loadedNotificationText(sessionID, stableID string) (string, bool) {
	panic("unported: mj-tui/src/notifications_pane.rs loaded_notification_text")
}

func (d *DashboardState) notificationReadRequest(entry *NotificationEntry) *NotificationReadRequest {
	panic("unported: mj-tui/src/notifications_pane.rs notification_read_request")
}

func (d *DashboardState) WantedNotificationMessage() *NotificationReadRequest {
	entries := d.notificationEntries()
	if len(entries) == 0 {
		d.syncNotificationSelection(nil)
		return nil
	}
	d.syncNotificationSelection(&entries[0])
	return d.notificationReadRequest(&entries[0])
}

func (d *DashboardState) SyncNotificationMessageRead(wanted *NotificationReadRequest, loading bool) {
	panic("unported: mj-tui/src/notifications_pane.rs sync_notification_message_read")
}

func (d *DashboardState) NotificationMessageReadFinished(request *NotificationReadRequest, text *string) bool {
	panic("unported: mj-tui/src/notifications_pane.rs notification_message_read_finished")
}

func (d *DashboardState) selectedNotificationText(entry *NotificationEntry) string {
	if entry.published == nil {
		return d.notificationCardText(entry.sessionID, entry.level)
	}
	return d.notificationItemText(entry.sessionID, entry.published, d.selectedNotificationItem(entry))
}

func (d *DashboardState) notificationItemText(session string, card *client.NotificationCard, index int) string {
	panic("unported: mj-tui/src/notifications_pane.rs notification_item_text")
}

func (d *DashboardState) notificationGesture(command CommandId) *NotificationGesture {
	entries := d.notificationEntries()
	if len(entries) == 0 {
		return nil
	}
	gesture := d.gestureForNotification(&entries[0], command)
	return &gesture
}

func (d *DashboardState) gestureForNotification(entry *NotificationEntry, command CommandId) NotificationGesture {
	panic("unported: mj-tui/src/notifications_pane.rs gesture_for_notification")
}

func (d *DashboardState) activateNotification(gesture NotificationGesture) DashboardAction {
	panic("unported: mj-tui/src/notifications_pane.rs activate_notification")
}

func (d *DashboardState) notificationSkip() DashboardAction {
	return d.notificationAction(CommandIdNotificationSkip)
}

func (d *DashboardState) notificationClear() DashboardAction {
	return d.notificationAction(CommandIdNotificationClear)
}

func (d *DashboardState) NotificationHasRecoverableReply() bool {
	id, ok := d.DisplayedNotificationSessionID()
	if !ok {
		return false
	}
	reply, ok := d.notifications.replies[id]
	if !ok {
		return false
	}
	_, ok = reply.restoreCandidate()
	return ok
}

func (d *DashboardState) HasNotificationForSession(sessionID string) bool {
	for _, entry := range d.notificationEntries() {
		if card := entry.published; card != nil {
			if (len(card.Entries) > 0 && entry.sessionID == sessionID) ||
				(card.Form != nil && card.ItemDestination(entry.sessionID, len(card.Entries)) == sessionID) {
				return true
			}
		} else if d.notificationDestination(&entry) == sessionID {
			return true
		}
	}
	return false
}

// NotificationSelectionIsCurrent: text copying uses the exact row space
// registered by the last render.
func (d *DashboardState) NotificationSelectionIsCurrent(generation uint64) bool {
	return d.notifications.textGeneration == generation &&
		d.frameSurfaces.Surface(chat.SurfaceIdNotificationText(generation)) != nil
}

func (d *DashboardState) NotificationSelectionText(generation uint64, selection *chat.SelectionRange) (string, bool) {
	if !d.NotificationSelectionIsCurrent(generation) || d.notifications.textRows == nil {
		return "", false
	}
	return d.notifications.textRows.SelectionText(selection), true
}

// AutoscrollNotificationSelection: selection edge scrolling uses the pane's
// existing scroll offset.
func (d *DashboardState) AutoscrollNotificationSelection(direction int8) {
	d.scrollNotification(int16(direction))
}

func (d *DashboardState) scrollNotification(delta int16) {
	next := int(d.notifications.scroll) + int(delta)
	d.notifications.scroll = uint16(min(max(next, 0), math.MaxUint16))
}

// notificationsPaneRender is Rust's notifications_pane::render; the Go name
// carries the module prefix because package tui has several `render`s.
// activeChat is the warm chat of the displayed card's session, or nil.
func notificationsPaneRender(
	frame *chat.Frame,
	area uv.Rectangle,
	dashboard *DashboardState,
	entries []NotificationEntry,
	text string,
	activeChat *chat.ActiveChat,
) {
	panic("unported: mj-tui/src/notifications_pane.rs render")
}

func renderMessageNavigation(
	frame *chat.Frame,
	inner uv.Rectangle,
	dashboard *DashboardState,
	entry *NotificationEntry,
	focused bool,
	headerWidth int,
) int {
	panic("unported: mj-tui/src/notifications_pane.rs render_message_navigation")
}
