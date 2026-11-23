// Port of mj-tui/src/notifications_pane.rs.

// The daemon publishes notification cards. This pane owns item selection,
// deferrals, per-session reply editors and their submission feedback.

package tui

import (
	"cmp"
	"context"
	"fmt"
	"maps"
	"math"
	"reflect"
	"slices"
	"strings"
	"time"

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
	if e.matches(previous) {
		return index, true
	}
	if e.sessionID != previous.action.SessionID {
		return 0, false
	}
	card := e.published
	if card == nil || previous.published == nil {
		return 0, false
	}
	earlier := previous.published
	if index < len(earlier.Entries) {
		item := &earlier.Entries[index]
		for position := range card.Entries {
			if card.Entries[position].CompletedOrdinal == item.CompletedOrdinal {
				return position, true
			}
		}
		return 0, false
	}
	if earlier.Form == nil || card.Form == nil {
		return 0, false
	}
	if card.ItemDestination(e.sessionID, len(card.Entries)) == earlier.ItemDestination(e.sessionID, len(earlier.Entries)) &&
		card.Form.RequestID == earlier.Form.RequestID {
		return len(card.Entries), true
	}
	return 0, false
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
	if reply, ok := d.notifications.replies[sessionID]; ok {
		return reply
	}
	editor := chat.ChatStateStandby(sessionID, d.config.Get(), chat.SessionHeaderIdentity{}, d.notices)
	editor.SetPromptHistoryProject(d.sessionBundleID(sessionID))
	editor.RequestSessionHistory()
	reply := &QuickReply{editor: editor}
	if d.notifications.replies == nil {
		d.notifications.replies = map[string]*QuickReply{}
	}
	d.notifications.replies[sessionID] = reply
	return reply
}

// sessionBundleID is the session's bundle, which its prompt history is kept
// under; nil for a session this dashboard does not hold.
func (d *DashboardState) sessionBundleID(sessionID string) *string {
	session := d.state.Get().Sessions.GetShared(sessionID)
	if session == nil {
		return nil
	}
	bundleID := session.BundleID
	return &bundleID
}

// handleNotificationKey returns nil (Rust's None) when the key is not the
// Notifications pane's to handle.
func (d *DashboardState) handleNotificationKey(key tea.KeyPressMsg) DashboardAction {
	if d.focus != FocusNotifications {
		return nil
	}
	// Bubble Tea reports crossterm's BackTab as Tab with Shift.
	if key.Code == tea.KeyTab {
		d.CycleFocus(key.Mod.Contains(tea.ModShift))
		return DashboardActionNone{}
	}
	sessionID, ok := d.DisplayedNotificationSessionID()
	if !ok {
		return nil
	}
	if _, form := d.NotificationFormSession(); form {
		d.recordEventHandled()
		if command, ok := paneCommandForKey(key, d.focus); ok {
			return d.runAvailableCommand(command)
		}
		return DashboardActionNone{}
	}
	result := d.quickReplyMut(sessionID).editor.HandleSingleLinePromptKey(standbyPromptKey(key).(tea.KeyPressMsg))
	switch action := result.Action.(type) {
	case chat.ChatActionCycleFocus:
		d.CycleFocus(action.Reverse)
	case chat.ChatActionPasteFromClipboard:
		return DashboardActionPasteFromClipboard{}
	}
	if !result.Consumed {
		if command, ok := paneCommandForKey(key, d.focus); ok {
			return d.runAvailableCommand(command)
		}
	}
	d.recordEventHandled()
	return DashboardActionNone{}
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
	changed := false
	for _, sessionID := range slices.Sorted(maps.Keys(d.notifications.replies)) {
		reply := d.notifications.replies[sessionID]
		changed = reply.editor.HasComposerUpdates() || changed
		reply.editor.DrainComposerUpdates()
		reply.editor.SetPromptHistoryProject(d.sessionBundleID(sessionID))
		bridge := persistence(sessionID)
		reply.editor.DispatchComposerRequests(
			func(ctx context.Context) (client.SessionHandle, error) {
				return control.Session(ctx, sessionID)
			},
			bridge,
		)
	}
	return changed
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
	reply, ok := d.notifications.replies[sessionID]
	if !ok {
		return false
	}
	index := slices.IndexFunc(reply.pending, func(pending ReplySubmission) bool {
		return pending.commandID == commandID
	})
	if index < 0 {
		return false
	}
	message := fmt.Sprintf("Could not confirm the reply: %s; checking its status.", errorText)
	reply.error = &message
	text := reply.pending[index].text
	d.revealNotificationReplyError(sessionID)
	d.ReportNotificationReplyFailure(
		sessionID,
		text,
		errorText,
		"Admission is unconfirmed; checking its status. The text is kept in this notice.",
	)
	return true
}

// NotificationReplyFinished: a durable prompt outcome belongs to its
// original session and command, even when another notification has
// replaced the displayed card. A nil result is Rust's Ok(()); a non-nil one
// carries the failure text.
func (d *DashboardState) NotificationReplyFinished(sessionID, commandID string, result error) bool {
	reply, ok := d.notifications.replies[sessionID]
	if !ok {
		return false
	}
	index := slices.IndexFunc(reply.pending, func(pending ReplySubmission) bool {
		return pending.commandID == commandID
	})
	if index < 0 {
		return false
	}
	pending := reply.pending[index]
	reply.pending = slices.Delete(reply.pending, index, index+1)
	if result == nil {
		if reply.editor.SingleLinePromptText() == pending.text {
			reply.editor.ClearDraft()
		}
		reply.editor.RecordPromptHistory(pending.text)
		reply.editor.RequestSessionHistory()
		reply.recoverable = slices.DeleteFunc(reply.recoverable, func(unsent RecoverableReply) bool {
			return unsent.text == pending.text
		})
		reply.error = nil
	} else {
		message := result.Error()
		reply.keepUnsent(pending.text, &message)
		reply.error = nil
		d.revealNotificationReplyError(sessionID)
	}
	return true
}

func (d *DashboardState) notificationAction(command CommandId) DashboardAction {
	gesture := d.notificationGesture(command)
	if gesture == nil {
		return DashboardActionNone{}
	}
	return d.activateNotification(*gesture)
}

func (d *DashboardState) notificationEntries() []NotificationEntry {
	var entries []NotificationEntry
	if cards := d.notificationCards; cards != nil {
		for id := range cards.Keys() {
			card := cards.GetShared(id)
			if card == nil {
				continue
			}
			level := AttentionLevelImportant
			if card.Badge == core.NotificationKindNeedsInput {
				level = AttentionLevelWaiting
			}
			if entry := d.notificationEntry(id, level, card.Dismissal, card); entry != nil {
				entries = append(entries, *entry)
			}
		}
	} else {
		for id := range d.servedAttention.Get().Keys() {
			level := d.AttentionLevel(id)
			if level != AttentionLevelWaiting && level != AttentionLevelImportant {
				continue
			}
			target, _ := d.notificationDismissalTarget(id)
			if entry := d.notificationEntry(id, level, target, nil); entry != nil {
				entries = append(entries, *entry)
			}
		}
	}
	// rank is the entry's position among the deferred cards. An entry that
	// was not deferred ranks -1, so it sorts first, as Rust's None does.
	rank := func(entry *NotificationEntry) int {
		return slices.IndexFunc(d.notifications.skipped, entry.matchesSkip)
	}
	important := func(entry *NotificationEntry) int {
		if entry.level == AttentionLevelImportant {
			return 1
		}
		return 0
	}
	slices.SortStableFunc(entries, func(a, b NotificationEntry) int {
		if order := cmp.Compare(rank(&a), rank(&b)); order != 0 {
			return order
		}
		var order int
		if a.published != nil && b.published != nil {
			order = cmp.Compare(a.published.SinceMs, b.published.SinceMs)
		} else {
			order = cmp.Or(
				cmp.Compare(important(&a), important(&b)),
				cmp.Compare(b.recency, a.recency),
				strings.Compare(a.title, b.title),
			)
		}
		if order != 0 {
			return order
		}
		return strings.Compare(a.sessionID, b.sessionID)
	})
	return entries
}

func (d *DashboardState) notificationEntry(
	id string,
	level AttentionLevel,
	target core.NotificationDismissal,
	published *client.NotificationCard,
) *NotificationEntry {
	session := d.state.Get().Sessions.GetShared(id)
	if session == nil || session.NotificationsIgnored {
		return nil
	}
	var recency uint64
	if published != nil {
		recency = uint64(max(published.SinceMs, 0))
	} else {
		detail := (*d.sessionDetails.Get())[id]
		if detail != nil && detail.lastActivityAtMs != nil && *detail.lastActivityAtMs > 0 {
			recency = *detail.lastActivityAtMs
		} else if seconds, ok := sessionUpdatedAtEpochSeconds(session); ok {
			if seconds > math.MaxUint64/1000 {
				recency = math.MaxUint64
			} else {
				recency = seconds * 1000
			}
		}
	}
	return &NotificationEntry{
		sessionID: id,
		level:     level,
		title:     session.ListedTitle(),
		project:   session.ProjectName(d.config.Get()),
		recency:   recency,
		target:    target,
		published: published,
	}
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
	card := d.notifications.scrollCard
	var changed bool
	switch {
	case entry != nil && card != nil:
		changed = !entry.matches(card)
	case entry == nil && card == nil:
		changed = false
	default:
		changed = true
	}
	if !changed {
		return
	}
	retained, kept := 0, false
	if entry != nil {
		retained, kept = d.retainedNotificationItem(entry)
		identity := entry.identity()
		d.notifications.scrollCard = &identity
	} else {
		d.notifications.scrollCard = nil
	}
	switch {
	case kept:
		d.notifications.selectedItem = retained
	case entry != nil:
		d.notifications.selectedItem = max(entry.itemCount()-1, 0)
	default:
		d.notifications.selectedItem = 0
	}
	if !kept {
		d.resetNotificationItem()
	}
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
		if d.notifications.formErrors == nil {
			d.notifications.formErrors = map[string]string{}
		}
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
	for _, entry := range d.PaneSessions() {
		session := entry.SessionID
		if !d.ChatInputAvailable(session) {
			continue
		}
		if active, ok := chats[session]; ok && active.ConversationPointerCaptured() {
			return session, true
		}
	}
	return "", false
}

// NotificationFormMouseOwnership reports (owned, true) when this
// presentation decides the pointer, and ok false (Rust's None) when it does
// not. owned false is message/chrome in this presentation: another
// conversation's geometry must not claim text that is drawn here.
func (d *DashboardState) NotificationFormMouseOwnership(chats map[string]*chat.ActiveChat, mouse tea.MouseMsg) (owned bool, ok bool) {
	if d.ModalOpen() || d.ComponentPointerCaptured() {
		return false, false
	}
	if _, captured := d.ConversationPointerSession(chats); captured {
		return false, false
	}
	area := d.notifications.formArea
	if area == nil {
		return false, false
	}
	session, form := d.NotificationFormSession()
	if !form {
		return false, false
	}
	active, found := chats[session]
	if !found || !active.HasElicitation() {
		return false, false
	}
	if surface, captured := active.ElicitationPointerSurface(); captured {
		return surface == chat.ElicitationSurfaceNotification, surface == chat.ElicitationSurfaceNotification
	}
	position := mouse.Mouse()
	if rectContains(*area, position.X, position.Y) {
		return active.ElicitationComponentHandlesMouseIn(chat.ElicitationSurfaceNotification, position), true
	}
	return false, false
}

// HandleNotificationFormEvent applies notification input to the owning
// chat's ordinary event dispatcher; ok false is Rust's None.
func (d *DashboardState) HandleNotificationFormEvent(
	chats map[string]*chat.ActiveChat,
	event tea.Msg,
) (chat.EventResult[chat.ChatEventOutcome], bool) {
	var none chat.EventResult[chat.ChatEventOutcome]
	_, mouse := event.(tea.MouseMsg)
	if d.ModalOpen() || (mouse && d.ComponentPointerCaptured()) {
		return none, false
	}
	if mouse {
		if _, captured := d.ConversationPointerSession(chats); captured {
			return none, false
		}
	}
	session, form := d.NotificationFormSession()
	if !form {
		return none, false
	}
	if !d.ChatInputAvailable(session) {
		if active, ok := chats[session]; ok {
			active.CancelComponentPointer()
		}
		return none, false
	}
	active, ok := chats[session]
	if !ok {
		return none, false
	}
	// The live composer owns input, exactly as for the conversation pane:
	// a parked composer's replacement attach would lose any result started
	// here.
	if !active.HasLiveComposer() {
		active.CancelComponentPointer()
		return none, false
	}
	if !active.HasElicitation() {
		return none, false
	}
	var routed bool
	if surface, captured := active.ElicitationPointerSurface(); mouse && captured {
		routed = surface == chat.ElicitationSurfaceNotification
	} else {
		routed = d.notificationFormHandlesEvent(event)
	}
	if !routed {
		return none, false
	}
	return active.HandleElicitationEvent(event), true
}

// notificationFormHandlesEvent routes only the form body here; the host
// still owns card buttons and menus.
func (d *DashboardState) notificationFormHandlesEvent(event tea.Msg) bool {
	if d.ModalOpen() || d.notifications.formArea == nil {
		return false
	}
	switch event := event.(type) {
	case tea.MouseMsg:
		position := event.Mouse()
		inside := rectContains(*d.notifications.formArea, position.X, position.Y)
		if click, ok := event.(tea.MouseClickMsg); inside && ok && click.Button == tea.MouseLeft {
			d.beginPointerPress()
			d.focus = FocusNotifications
		}
		return inside
	case tea.KeyMsg:
		if command, ok := paneCommandForKey(tea.KeyPressMsg(event.Key()), FocusNotifications); ok {
			switch command {
			case CommandIdNotificationOpen,
				CommandIdNotificationClear,
				CommandIdNotificationSkip,
				CommandIdNotificationIgnoreSession,
				CommandIdNotificationPreviousMessage,
				CommandIdNotificationNextMessage:
				return false
			}
		}
		return d.focus == FocusNotifications
	case tea.PasteMsg:
		return d.focus == FocusNotifications
	}
	return false
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
	panes := map[string]struct{}{}
	for _, entry := range d.PaneSessions() {
		panes[entry.SessionID] = struct{}{}
	}
	state := d.state.Get()
	activeWorkspace, hasWorkspace := d.ActiveWorkspaceID()
	subagentParent, hasSubagentParent := d.SubagentParentID()
	wanted := map[string]uint64{}
	for session := range state.Sessions.Values() {
		if d.IsNativeAgent(session.ID) {
			continue
		}
		_, shown := panes[session.ID]
		inWorkspace := hasWorkspace && session.WorkspaceID == activeWorkspace && session.State.IsActive()
		child := state.Subagents.GetShared(session.ID)
		inFamily := child != nil && hasSubagentParent && child.ParentSessionID == subagentParent
		if shown || inWorkspace || inFamily {
			wanted[session.ID] = session.ViewedThroughEventOrdinal
		}
	}
	if session, ok := d.DisplayedNotificationSessionID(); ok {
		var viewed uint64
		if record := state.Sessions.GetShared(session); record != nil {
			viewed = record.ViewedThroughEventOrdinal
		}
		wanted[session] = viewed
	}
	return wanted
}

func (d *DashboardState) loadedNotificationText(sessionID, stableID string) (string, bool) {
	detail := (*d.sessionDetails.Get())[sessionID]
	if detail == nil {
		return "", false
	}
	return detail.projection.agentMessageText(stableID)
}

func (d *DashboardState) notificationReadRequest(entry *NotificationEntry) *NotificationReadRequest {
	var sessionID string
	var source NotificationReadSource
	if card := entry.published; card != nil {
		index := d.selectedNotificationItem(entry)
		if index >= len(card.Entries) {
			return nil
		}
		source = notificationItemSource(&card.Entries[index])
		sessionID = entry.sessionID
	} else {
		if entry.level != AttentionLevelWaiting {
			return nil
		}
		sessionID = d.notificationDestination(entry)
		detail := (*d.sessionDetails.Get())[sessionID]
		if detail == nil || len(detail.pendingElicitations) > 0 || detail.questionMessage == nil {
			return nil
		}
		source = NotificationReadSourceItem{Value: *detail.questionMessage}
	}
	if item, ok := source.(NotificationReadSourceItem); ok {
		if _, loaded := d.loadedNotificationText(sessionID, item.Value); loaded {
			return nil
		}
	}
	return &NotificationReadRequest{
		SessionID:         sessionID,
		Source:            source,
		SelectionRevision: d.notifications.selectionRevision,
	}
}

// notificationItemSource is where a published item's message is read from:
// the located message, or else the transcript through its completion.
func notificationItemSource(item *core.NotificationStackEntry) NotificationReadSource {
	if found, ok := item.Message.(core.EntryMessageFound); ok {
		return NotificationReadSourceItem{Value: found.Value}
	}
	return NotificationReadSourceThrough{Value: item.CompletedOrdinal}
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
	if wanted == nil {
		d.notifications.messageRead = nil
		return
	}
	if read := d.notifications.messageRead; read == nil || read.request.Compare(*wanted) != 0 {
		d.notifications.messageRead = &NotificationMessageRead{
			request: *wanted,
			status:  TranscriptHydrationUnavailable,
		}
	}
	read := d.notifications.messageRead
	if loading {
		read.status = TranscriptHydrationLoading
	} else if read.status == TranscriptHydrationLoading {
		read.status = TranscriptHydrationUnavailable
	}
}

func (d *DashboardState) NotificationMessageReadFinished(request *NotificationReadRequest, text *string) bool {
	if wanted := d.WantedNotificationMessage(); wanted == nil || wanted.Compare(*request) != 0 {
		return false
	}
	if read := d.notifications.messageRead; read != nil && read.request.Compare(*request) == 0 {
		read.status = TranscriptHydrationReady
		read.text = text
		return true
	}
	return false
}

func (d *DashboardState) selectedNotificationText(entry *NotificationEntry) string {
	if entry.published == nil {
		return d.notificationCardText(entry.sessionID, entry.level)
	}
	return d.notificationItemText(entry.sessionID, entry.published, d.selectedNotificationItem(entry))
}

func (d *DashboardState) notificationItemText(session string, card *client.NotificationCard, index int) string {
	destination := card.ItemDestination(session, index)
	if index >= len(card.Entries) {
		if card.Form != nil {
			return card.Form.Message
		}
		if label, ok := client.CombinedActivityLabel(sessionsReviewViews(d.sessionReviews(destination))); ok {
			return label
		}
		return core.NotificationMessagePlaceholder(card.Badge)
	}
	item := &card.Entries[index]
	if found, ok := item.Message.(core.EntryMessageFound); ok {
		if text, loaded := d.loadedNotificationText(destination, found.Value); loaded {
			return text
		}
	}
	source := notificationItemSource(item)
	read := d.notifications.messageRead
	if read != nil && (read.request.SessionID != destination || CompareNotificationReadSource(read.request.Source, source) != 0) {
		read = nil
	}
	if read != nil && read.status == TranscriptHydrationLoading {
		return "loading…"
	}
	if read != nil && read.text != nil {
		return *read.text
	}
	if _, ok := source.(NotificationReadSourceItem); ok {
		return "message not available"
	}
	return core.NotificationMessagePlaceholder(item.Kind)
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
	sessionID := d.notificationDestination(entry)
	var text string
	var restoreText *string
	if reply, ok := d.notifications.replies[sessionID]; ok {
		text = reply.editor.SingleLinePromptText()
		if command == CommandIdNotificationRestore {
			if candidate, ok := reply.restoreCandidate(); ok {
				restoreText = &candidate
			}
		}
	}
	return NotificationGesture{
		command:           command,
		identity:          entry.identity(),
		selectionRevision: d.notifications.selectionRevision,
		sessionID:         sessionID,
		text:              text,
		restoreText:       restoreText,
	}
}

func (d *DashboardState) activateNotification(gesture NotificationGesture) DashboardAction {
	command := gesture.command
	identity := gesture.identity
	sessionID := gesture.sessionID
	text := gesture.text
	card := identity.action
	if gesture.restoreText != nil {
		restore := *gesture.restoreText
		reply := d.quickReplyMut(sessionID)
		// The gesture carries the captured recovery. Preserve the current
		// draft too, including any edits made after this button was drawn.
		previous := reply.editor.SetSingleLinePromptText(restore)
		reply.keepUnsent(previous, nil)
		// Rotate the restored entry to the back so repeated restoration
		// reaches every retained reply, rather than alternating two drafts.
		if index := slices.IndexFunc(reply.recoverable, func(unsent RecoverableReply) bool {
			return unsent.text == restore
		}); index >= 0 {
			restored := reply.recoverable[index]
			reply.recoverable = append(slices.Delete(reply.recoverable, index, index+1), restored)
		}
		return DashboardActionNone{}
	}
	blank := strings.TrimSpace(text) == ""
	switch {
	case command == CommandIdNotificationOpen || (command == CommandIdNotificationDefault && blank):
		action := d.focusSessionFromCPUReport(sessionID)
		d.focus = FocusPrompt
		return action
	case command == CommandIdNotificationRestore:
		return DashboardActionNone{}
	case command == CommandIdNotificationSend || command == CommandIdNotificationDefault:
		if blank || slices.ContainsFunc(d.quickReplyMut(sessionID).pending, func(pending ReplySubmission) bool {
			return pending.text == text
		}) {
			return DashboardActionNone{}
		}
		commandID, err := client.NewCommandID("notification-reply")
		if err != nil {
			message := fmt.Sprintf("Could not send reply: %v", err)
			d.quickReplyMut(sessionID).keepUnsent(text, &message)
			d.revealNotificationReplyError(sessionID)
			return DashboardActionNone{}
		}
		reply := d.quickReplyMut(sessionID)
		reply.pending = append(reply.pending, ReplySubmission{
			card:      identity,
			commandID: commandID,
			text:      text,
		})
		reply.error = nil
		return DashboardActionNotificationReply{
			Card:      card,
			SessionID: sessionID,
			Prompt: chat.StartupPrompt{
				CommandID: commandID,
				Payload:   chat.PromptPayloadText(text),
			},
		}
	case command == CommandIdNotificationClear:
		target := card.Target
		if target == nil || d.notificationDismissalPending(card.SessionID) {
			return DashboardActionNone{}
		}
		d.submitNotificationTarget(card.SessionID, target)
		return DashboardActionDismissNotification{
			SessionID: card.SessionID,
			Target:    target,
		}
	case command == CommandIdNotificationSkip || command == CommandIdNotificationIgnoreSession:
		entries := d.notificationEntries()
		skipped := identity.skipIdentity()
		d.notifications.skipped = slices.DeleteFunc(d.notifications.skipped, func(previous SkippedCard) bool {
			return skippedCardEqual(previous, skipped) || !slices.ContainsFunc(entries, func(entry NotificationEntry) bool {
				return entry.matchesSkip(previous)
			})
		})
		_, published := skipped.(SkippedCardPublished)
		if !published || slices.ContainsFunc(entries, func(entry NotificationEntry) bool {
			return entry.matchesSkip(skipped)
		}) {
			d.notifications.skipped = append(d.notifications.skipped, skipped)
		}
		if next := d.notificationEntries(); len(next) > 0 {
			d.syncNotificationSelection(&next[0])
		} else {
			d.syncNotificationSelection(nil)
		}
		d.notifications.scroll = 0
		if command == CommandIdNotificationIgnoreSession {
			return DashboardActionSetSessionNotificationsIgnored{
				SessionID: card.SessionID,
				Ignored:   true,
			}
		}
		return DashboardActionNotificationSkip{Card: card}
	case command == CommandIdNotificationPreviousMessage || command == CommandIdNotificationNextMessage:
		entries := d.notificationEntries()
		if len(entries) > 0 && entries[0].matches(&identity) && gesture.selectionRevision == d.notifications.selectionRevision {
			entry := &entries[0]
			d.syncNotificationSelection(entry)
			if index, ok := d.notificationItemInDirection(entry, command); ok {
				d.notifications.selectedItem = index
				d.resetNotificationItem()
			}
		}
		return DashboardActionNone{}
	}
	return DashboardActionNone{}
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
	pane := &dashboard.notifications
	pane.formArea = nil
	pane.renderedPresentation = &notificationPresentation{
		selectionRevision: pane.selectionRevision,
		size:              pane.size,
	}
	focused := dashboard.focus == FocusNotifications
	minimized := pane.size == core.PaneSizeMinimized
	clearing := 0
	for _, detail := range *dashboard.sessionDetails.Get() {
		if _, submitting := detail.notificationDismissal.(NotificationDismissalStateSubmitting); submitting {
			clearing++
		}
	}
	clearingLabel := ""
	if clearing > 0 {
		clearingLabel = " clearing…"
	}
	borders := chat.BordersAll
	if minimized {
		borders = chat.BordersTop
	}
	block := chat.Panel(focused).
		WithTitle(chat.RawLine(fmt.Sprintf(" Notifications (%d)%s ", len(entries), clearingLabel))).
		WithTitle(paneSizeControls(pane.size, pane.maximizeEnabled)).
		WithBorders(borders)
	if !minimized && activeChat != nil && activeChat.HasElicitation() && len(entries) > 0 {
		if errorText, ok := pane.errors[entries[0].sessionID]; ok {
			block = block.WithTitleBottom(chat.StyledLine(
				fmt.Sprintf(" Could not clear: %s ", errorText),
				chat.Style{}.Foreground(chat.ThemePalette().Error),
			))
		}
	}
	inner := block.Inner(area)
	frame.RenderWidget(block, area)
	for _, control := range paneSizeControlAreas(area, SupportPaneNotifications, pane.maximizeEnabled) {
		dashboard.paneSizeControlAreas = append(dashboard.paneSizeControlAreas, paneSizeControlArea{
			pane: control.Pane,
			size: control.Size,
			area: control.Area,
		})
	}
	pane.controls = nil
	if minimized || inner.Dy() == 0 {
		return
	}
	if len(entries) == 0 {
		frame.RenderWidget(chat.NewParagraph(chat.RawText("Nothing needs you")).WithStyle(chat.Muted()), inner)
		return
	}
	entry := &entries[0]
	now := uint64(max(time.Now().Unix(), 0))
	recencySeconds := entry.recency / 1_000
	age := "age unknown"
	if recencySeconds != 0 {
		var elapsed uint64
		if now > recencySeconds {
			elapsed = now - recencySeconds
		}
		age = fmt.Sprintf("%ds ago", elapsed)
	}
	glyph := '!'
	if entry.level == AttentionLevelWaiting {
		glyph = '?'
	}
	for _, command := range [...]CommandId{
		CommandIdNotificationOpen,
		CommandIdNotificationSend,
		CommandIdNotificationRestore,
		CommandIdNotificationClear,
		CommandIdNotificationSkip,
		CommandIdNotificationIgnoreSession,
		CommandIdNotificationPreviousMessage,
		CommandIdNotificationNextMessage,
	} {
		pane.controls = append(pane.controls, dashboard.gestureForNotification(entry, command))
	}
	header := chat.RawLine(fmt.Sprintf("%c %s · %s · %s", glyph, entry.title, entry.project, age))
	headerWidth := renderMessageNavigation(frame, inner, dashboard, entry, focused, header.Width())
	frame.RenderWidget(
		chat.NewParagraph(chat.TextFrom(header)).WithStyle(
			chat.Style{}.Foreground(chat.ThemePalette().Secondary).Add(chat.ModifierBold),
		),
		uv.Rect(inner.Min.X, inner.Min.Y, headerWidth, 1),
	)
	_, formRequested := dashboard.NotificationFormSession()
	formOpen := false
	if activeChat != nil {
		_, formOpen = activeChat.ElicitationHeight(inner.Dx())
	}
	// The form's panel needs its two borders and one interior row inside the
	// card body (the card's header and bottom margin take two rows).
	formFits := max(inner.Dy()-2, 0) >= 3
	inlineForm := formRequested && formOpen && formFits
	sessionID := dashboard.notificationDestination(entry)
	replyError := ""
	if reply, ok := pane.replies[sessionID]; ok {
		var messages []string
		if reply.error != nil {
			messages = append(messages, *reply.error)
		}
		for _, unsent := range reply.recoverable {
			shown := strings.ReplaceAll(unsent.text, "\n", "⏎")
			restore := "Alt+Enter restores; Enter sends restored text"
			if unsent.text == reply.editor.SingleLinePromptText() {
				restore = "Enter sends this text"
			}
			if unsent.error != nil {
				messages = append(messages, fmt.Sprintf("Not sent: %s — %s; %s.", shown, *unsent.error, restore))
			} else {
				messages = append(messages, fmt.Sprintf("Saved draft: %s; %s.", shown, restore))
			}
		}
		replyError = strings.Join(messages, "\n")
	}
	if errorText, ok := pane.errors[entry.sessionID]; ok {
		text = fmt.Sprintf("Could not clear: %s\n%s", errorText, text)
	}
	if replyError != "" {
		text = fmt.Sprintf("%s\n%s", replyError, text)
	}
	if formRequested && !inlineForm {
		if formError, ok := pane.formErrors[sessionID]; ok {
			text = fmt.Sprintf("Could not load form: %s. Use Go to Session to answer it there.\n%s", formError, text)
		} else if activeChat == nil && dashboard.SessionFailed(sessionID) {
			// No attach will run for a failed session, so no attach error
			// will arrive either; say why instead of loading forever.
			reason := dashboard.sessionFailureText(sessionID)
			text = fmt.Sprintf("Could not load form: %s\n%s", reason, text)
		} else if activeChat == nil && dashboard.PaneSessionIsSuspended(sessionID) {
			text = fmt.Sprintf("Could not load form: the session is suspended.\n%s", text)
		} else if formOpen {
			// The question comes first: it is what a cramped card must keep.
			text = fmt.Sprintf("%s\nEnlarge this pane to answer here, or use Go to Session.", text)
		} else if activeChat == nil {
			text = fmt.Sprintf("Loading form…\n%s", text)
		} else {
			text = fmt.Sprintf("Waiting for the session's form update…\n%s", text)
		}
	}
	// Header, top text margin, bottom margin, blank separator, reply and
	// buttons.
	top, reserved := 2, 6
	if formRequested {
		top, reserved = 1, 2
	}
	body := uv.Rect(inner.Min.X+1, inner.Min.Y+top, max(inner.Dx()-2, 0), max(inner.Dy()-reserved, 0))
	if inlineForm {
		pane.textRows = nil
		if !dashboard.ChatInputAvailable(sessionID) {
			activeChat.CancelComponentPointer()
		}
		surfaces := activeChat.DrawElicitationIn(frame, body, focused && !dashboard.ModalOpen())
		var formText *notificationFormText
		if message, ok := activeChat.ElicitationMessageText(); ok {
			width := 0
			if surface := surfaces.Surface(chat.SurfaceIdElicitationMessage{}); surface != nil {
				width = surface.Rect.Dx()
			}
			formText = &notificationFormText{text: message, width: width}
		}
		sameText := (pane.formText == nil) == (formText == nil) && (formText == nil || *pane.formText == *formText)
		if !sameText {
			pane.textGeneration++
			pane.formText = formText
		}
		// Claim input exactly when HandleNotificationFormEvent accepts it.
		if dashboard.ChatInputAvailable(sessionID) &&
			activeChat.HasLiveComposer() &&
			activeChat.ElicitationInputAvailableIn(chat.ElicitationSurfaceNotification) {
			formArea := body
			pane.formArea = &formArea
		} else {
			pane.formArea = nil
		}
		for _, mapping := range [...]struct{ source, target chat.SurfaceId }{
			{chat.SurfaceIdElicitationMessage{}, chat.SurfaceIdNotificationText(pane.textGeneration)},
			{chat.SurfaceIdModalBody{}, chat.SurfaceIdNotificationFormBody(pane.textGeneration)},
		} {
			if surface := surfaces.Surface(mapping.source); surface != nil {
				copied := *surface
				copied.ID = mapping.target
				dashboard.frameSurfaces.Push(copied)
			}
		}
	} else {
		pane.formText = nil
		if pane.textRows == nil || !pane.textRows.Matches(text, body.Dx()) {
			pane.textGeneration++
			rows := chat.NewSelectableText(text, body.Dx())
			pane.textRows = &rows
		}
		rows := pane.textRows.Rows()
		maxScroll := min(max(len(rows)-body.Dy(), 0), math.MaxUint16)
		pane.scroll = min(pane.scroll, uint16(maxScroll))
		if body.Dy() > 0 {
			frame.RenderWidget(chat.NewParagraph(chat.TextFrom(rows...)).WithScroll(int(pane.scroll), 0), body)
			dashboard.frameSurfaces.Push(chat.SurfaceFrameScrollable(
				chat.SurfaceIdNotificationText(pane.textGeneration),
				body,
				int(pane.scroll),
				len(rows),
			))
		}
	}
	if inner.Dy() < 3 {
		return
	}
	if !formRequested {
		currentCard := entry.identity()
		reply := dashboard.quickReplyMut(sessionID)
		_, restoring := reply.restoreCandidate()
		var pending *ReplySubmission
		for index := range reply.pending {
			if reply.pending[index].text == reply.editor.SingleLinePromptText() {
				pending = &reply.pending[index]
				break
			}
		}
		var sendLabel string
		switch {
		case pending != nil && !pending.card.equal(&currentCard):
			sendLabel = " Sending earlier reply… "
		case pending != nil:
			sendLabel = " Sending… "
		case len(reply.pending) > 0:
			sendLabel = " Send (reply in flight) "
		default:
			sendLabel = " Send "
		}
		canSend := pending == nil && strings.TrimSpace(reply.editor.SingleLinePromptText()) != ""
		sendWidth := min(chat.RawLine(sendLabel).Width(), inner.Dx())
		replyY := inner.Max.Y - 2
		sendArea := uv.Rect(max(inner.Max.X-sendWidth, 0), replyY, sendWidth, 1)
		const restoreLabel = " Restore unsent "
		restoreWidth := 0
		if restoring {
			restoreWidth = min(chat.RawLine(restoreLabel).Width(), max(sendArea.Min.X-inner.Min.X, 0))
		}
		restoreArea := uv.Rect(max(sendArea.Min.X-restoreWidth, 0), replyY, restoreWidth, 1)
		if restoring && restoreWidth > 0 {
			dashboard.surfaceForm.Register(
				SurfaceControlNotification(CommandIdNotificationRestore),
				chat.ControlKindButton{},
				restoreArea,
				true,
			)
			frame.RenderWidget(
				chat.NewParagraph(chat.RawText(restoreLabel)).WithStyle(chat.Actionable().Patch(chat.Raised())),
				restoreArea,
			)
		}
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText("Quick reply: ")).WithStyle(chat.Muted()),
			uv.Rect(inner.Min.X, replyY, min(13, inner.Dx()), 1),
		)
		input := uv.Rect(
			min(inner.Min.X+13, restoreArea.Min.X),
			replyY,
			max(restoreArea.Min.X-(inner.Min.X+14), 0),
			1,
		)
		inputFocused := focused && !dashboard.ModalOpen()
		dashboard.quickReplyMut(sessionID).editor.DrawSingleLinePrompt(frame, input, inputFocused)
		dashboard.surfaceForm.Register(
			SurfaceControlNotification(CommandIdNotificationSend),
			chat.ControlKindButton{},
			sendArea,
			canSend,
		)
		sendStyle := chat.Muted()
		if canSend {
			sendStyle = chat.Actionable().Patch(chat.Raised())
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(sendLabel)).WithStyle(sendStyle), sendArea)
	}
	x := inner.Min.X
	for _, button := range [...]struct {
		id      CommandId
		label   string
		enabled bool
	}{
		{CommandIdNotificationOpen, "Go to Session", true},
		{
			CommandIdNotificationClear,
			"Clear & Next",
			entry.target != nil && !dashboard.notificationDismissalPending(entry.sessionID),
		},
		{CommandIdNotificationSkip, "Skip", true},
		{CommandIdNotificationIgnoreSession, "Ignore Session", true},
	} {
		width := chat.RawLine(button.label).Width() + 2
		if x+width > inner.Max.X {
			break
		}
		rect := uv.Rect(x, inner.Max.Y-1, width, 1)
		dashboard.surfaceForm.Register(SurfaceControlNotification(button.id), chat.ControlKindButton{}, rect, button.enabled)
		style := chat.Muted()
		if button.enabled {
			style = chat.Actionable().Patch(chat.Raised())
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(" "+button.label+" ")).WithStyle(style), rect)
		x += width + 1
	}
}

func renderMessageNavigation(
	frame *chat.Frame,
	inner uv.Rectangle,
	dashboard *DashboardState,
	entry *NotificationEntry,
	focused bool,
	headerWidth int,
) int {
	if entry.published == nil || entry.itemCount() == 0 {
		return inner.Dx()
	}
	index := dashboard.selectedNotificationItem(entry)
	position := fmt.Sprintf("%d of %d", index+1, entry.itemCount())
	right := inner.Max.X
	fullLabels := [2]string{" Next Alt+PgDn ", " Previous Alt+PgUp "}
	fullWidth := len(fullLabels[0]) + len(fullLabels[1]) + len(position) + 1
	labels := [2]string{" Next ", " Previous "}
	if focused && inner.Dx() >= headerWidth+fullWidth {
		labels = fullLabels
	}
	// One message has nowhere to go, so the buttons would only be noise.
	var buttons []CommandId
	if entry.itemCount() > 1 {
		buttons = []CommandId{CommandIdNotificationNextMessage, CommandIdNotificationPreviousMessage}
	}
	for index, command := range buttons {
		label := labels[index]
		width := len(label)
		if max(right-inner.Min.X, 0) < width+len(position)+1 {
			continue
		}
		right -= width
		area := uv.Rect(right, inner.Min.Y, width, 1)
		_, enabled := dashboard.notificationItemInDirection(entry, command)
		dashboard.surfaceForm.Register(SurfaceControlNotification(command), chat.ControlKindButton{}, area, enabled)
		style := chat.Muted()
		if enabled {
			style = chat.Actionable().Patch(chat.Raised())
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(label)).WithStyle(style), area)
	}
	width := min(len(position), right-inner.Min.X)
	right -= width
	frame.RenderWidget(chat.NewParagraph(chat.RawText(position)).WithStyle(chat.Muted()), uv.Rect(right, inner.Min.Y, width, 1))
	return max(right-inner.Min.X-1, 0)
}
