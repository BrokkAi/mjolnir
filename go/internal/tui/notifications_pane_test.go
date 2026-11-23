// Port of mj-tui/src/notifications_pane.rs (tests).

package tui

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// historyAnswer is one form response the backend observed: the request id
// and the answer.
type historyAnswer struct {
	id       string
	response core.ElicitationResponse
}

// historyRequest is one prompt search the backend forwards for a test to
// answer on reply.
type historyRequest struct {
	project string
	query   string
	reply   chan core.PromptHistoryPage
}

// historyBackend is the notifications tests' hand-written session fake. It
// is both the session control (every session it opens is another copy of
// itself) and the session handle.
type historyBackend struct {
	inner         client.SessionHandle
	formView      *client.ManagedSessionView
	responses     *core.Unbounded[historyAnswer]
	responseError *string
	listings      *core.Unbounded[string]
	requests      *core.Unbounded[historyRequest]
}

func (b historyBackend) Session(_ context.Context, sessionID string) (client.SessionHandle, error) {
	backend := b
	backend.inner = client.NewReplacementSessionTestFixture(sessionID, 1).Stopped
	return client.NewSessionHandle(backend), nil
}

func (b historyBackend) ToolCallDetail(context.Context, string) (core.ToolCallDetailResult, error) {
	return nil, nil
}

func (b historyBackend) SearchPrompts(
	ctx context.Context,
	project string,
	_ core.HistoryScope,
	query string,
	_ *core.HistoryCursor,
	_ int,
) (core.PromptHistoryPage, error) {
	reply := make(chan core.PromptHistoryPage, 1)
	if err := b.requests.Send(historyRequest{project: project, query: query, reply: reply}); err != nil {
		panic(err)
	}
	select {
	case page := <-reply:
		return page, nil
	case <-ctx.Done():
		return core.PromptHistoryPage{}, ctx.Err()
	}
}

func (b historyBackend) SessionPrompts(context.Context) ([]core.PromptHistoryEntry, error) {
	if b.formView != nil {
		return []core.PromptHistoryEntry{}, nil
	}
	if err := b.listings.Send(b.SessionID()); err != nil {
		panic(err)
	}
	return []core.PromptHistoryEntry{{
		ID:        1,
		SessionID: b.SessionID(),
		Text:      "stored\nprompt 世界",
	}}, nil
}

func (b historyBackend) ConfigResult(context.Context, string) (*string, bool, error) {
	return nil, false, nil
}

func (b historyBackend) CloneBox() client.SessionHandleBackend { return b }

func (b historyBackend) SessionID() string { return b.inner.SessionID() }

func (b historyBackend) View() client.ManagedSessionView {
	if b.formView != nil {
		return *b.formView
	}
	return b.inner.View()
}

func (b historyBackend) IsStopped() bool { return b.formView == nil && b.inner.IsStopped() }

func (b historyBackend) HasChanged() (bool, error) { return b.inner.HasChanged() }

func (b historyBackend) Changed(ctx context.Context) (client.ManagedSessionView, error) {
	<-ctx.Done()
	return client.ManagedSessionView{}, ctx.Err()
}

func (b historyBackend) EnqueueSubmit(context.Context, string, core.RelayCommand) (client.PendingRelaySubmit, error) {
	return client.PendingRelaySubmit{}, errors.New("unexpected submit")
}

func (b historyBackend) EnqueueSync(context.Context) (client.PendingRelaySync, error) {
	return client.PendingRelaySync{}, errors.New("unexpected sync")
}

func (b historyBackend) RespondElicitation(_ context.Context, id string, response core.ElicitationResponse) error {
	if b.responses == nil {
		panic("form response observer")
	}
	if err := b.responses.Send(historyAnswer{id: id, response: response}); err != nil {
		return err
	}
	if b.responseError != nil {
		return errors.New(*b.responseError)
	}
	return nil
}

func (b historyBackend) StopBackgroundTask(context.Context, string) error {
	return errors.New("unexpected stop")
}

func (b historyBackend) Reviewer(context.Context, *string, client.ReviewerAction) (client.ReviewerOutcome, error) {
	return nil, errors.New("unexpected review")
}

// formChat is an attached chat whose live view holds request as its one
// pending form, with the channel its answers arrive on.
func formChat(session string, request core.ElicitationRequest) (*chat.ActiveChat, *core.Unbounded[historyAnswer]) {
	return formChatWithError(session, request, nil)
}

func formChatWithError(
	session string,
	request core.ElicitationRequest,
	responseError *string,
) (*chat.ActiveChat, *core.Unbounded[historyAnswer]) {
	fixture := client.NewReplacementSessionTestFixture(session, 1)
	answers := core.NewUnbounded[historyAnswer]()
	materialized := core.MaterializedSessionEmpty(session)
	materialized.PendingElicitations = []core.ElicitationRequest{request}
	relay := core.NewRelaySnapshot(session)
	view := client.ManagedSessionView{
		Connected: true,
		Snapshot: &core.ManagedSessionSnapshot{
			Materialized: materialized,
			Operational:  core.NewSharedSnapshot(relay.OperationalState()),
		},
	}
	backend := historyBackend{
		inner:         fixture.Stopped,
		formView:      &view,
		responses:     answers,
		responseError: responseError,
		listings:      core.NewUnbounded[string](),
		requests:      core.NewUnbounded[historyRequest](),
	}
	active := chat.ActiveChatOpen(
		client.NewSessionHandle(backend),
		"test-bundle",
		nil,
		client.NewSessionControl(backend),
		chat.SessionHeaderIdentity{},
		"",
		chat.NewNotices(),
	)
	return active, answers
}

// ratatuiTestTerminal is ratatui's Terminal over a TestBackend: every draw
// renders a fresh frame of the terminal's size, and buffer is what the last
// draw left on screen.
type ratatuiTestTerminal struct {
	width, height int
	buffer        *uv.Buffer
}

func newRatatuiTestTerminal(width, height int) *ratatuiTestTerminal {
	return &ratatuiTestTerminal{width: width, height: height, buffer: uv.NewBuffer(width, height)}
}

func (t *ratatuiTestTerminal) draw(render func(frame *chat.Frame)) {
	t.buffer = uv.NewBuffer(t.width, t.height)
	render(chat.NewFrame(t.buffer))
}

// drawCombined draws the combined surface with chats, as the Rust tests'
// render_combined_for_test closures do, and returns the drawn rows.
func (t *ratatuiTestTerminal) drawCombined(dashboard *DashboardState, chats map[string]*chat.ActiveChat) []string {
	t.draw(func(frame *chat.Frame) {
		renderCombinedForTest(frame, dashboard, chats, map[PaneId]string{}, false)
	})
	return bufferLines(t.buffer)
}

// renderDashboardForTest is Rust's #[cfg(test)] render::render: the
// combined surface with no conversation on it.
func renderDashboardForTest(frame *chat.Frame, dashboard *DashboardState) {
	withNoColorOverrideForTest(true, func() struct{} {
		renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
		return struct{}{}
	})
}

// cloneFrameSurfaces is a copy of surfaces that later frames cannot reuse.
func cloneFrameSurfaces(surfaces *chat.FrameSurfaces) *chat.FrameSurfaces {
	clone := chat.NewFrameSurfaces()
	clone.Append(surfaces)
	return &clone
}

// cloneNotificationCard is a deep copy of a published card, so a test can
// edit it without touching the snapshot the dashboard holds.
func cloneNotificationCard(card *client.NotificationCard) {
	card.Entries = slices.Clone(card.Entries)
	if card.Form != nil {
		form := *card.Form
		card.Form = &form
	}
}

// mutableNotificationCards is a copy of the published cards whose records
// can be edited in place.
func mutableNotificationCards(dashboard *DashboardState) core.SnapshotMap[string, client.NotificationCard] {
	cards := *dashboard.notificationCards
	return cards
}

func editCard(cards *core.SnapshotMap[string, client.NotificationCard], id string) *client.NotificationCard {
	card := cards.GetMut(id)
	cloneNotificationCard(card)
	return card
}

// requireFormEvent is Rust's `.unwrap()` on HandleNotificationFormEvent's
// result: requireFormEvent(t)(dashboard.HandleNotificationFormEvent(...)).
func requireFormEvent(t *testing.T) func(chat.EventResult[chat.ChatEventOutcome], bool) chat.EventResult[chat.ChatEventOutcome] {
	return func(result chat.EventResult[chat.ChatEventOutcome], ok bool) chat.EventResult[chat.ChatEventOutcome] {
		t.Helper()
		if !ok {
			t.Fatal("the notification form did not take the event")
		}
		return result
	}
}

func requireDashboardAction(t *testing.T, got, want DashboardAction, message ...any) {
	t.Helper()
	if reflectSnapshot(got) != reflectSnapshot(want) {
		t.Fatalf("%s: got %#v, want %#v", fmt.Sprint(message...), got, want)
	}
}

func recvAnswer(t *testing.T, answers *core.Unbounded[historyAnswer]) historyAnswer {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	answer, ok := answers.Recv(ctx)
	if !ok {
		t.Fatal("no form answer arrived")
	}
	return answer
}

func noAnswer(t *testing.T, answers *core.Unbounded[historyAnswer]) {
	t.Helper()
	if _, ok, _ := answers.TryRecv(); ok {
		t.Fatal("an answer was sent")
	}
}

func acceptResponse(id string, value core.ElicitationValue) core.ElicitationResponse {
	return core.ElicitationResponse{
		Action:  core.ElicitationResponseAccept,
		Content: map[string]core.ElicitationValue{id: value},
	}
}

func booleanQuestion(sessionID string, value bool) core.ElicitationRequest {
	request := question(sessionID)
	request.Fields[0].Kind = core.ElicitationFieldKindBoolean{Default: &value}
	return request
}

func mutableSessionDetail(dashboard *DashboardState, sessionID string) *SessionDetail {
	return (*dashboard.sessionDetails.Mut())[sessionID]
}

func mouseAtCell(kind tea.MouseMsg, x, y int) tea.MouseMsg {
	return mouseAt(kind, uv.Pos(x, y))
}

var (
	leftButtonDown = tea.MouseClickMsg{Button: tea.MouseLeft}
	leftButtonUp   = tea.MouseReleaseMsg{Button: tea.MouseLeft}
)

// Hard-won: 4c23909e0: selection consumed inline checkbox presses, and a size warning retained invisible form input geometry.
func TestInlineFormControlsPrecedeSelectionAndHiddenFramesReleaseInput(t *testing.T) {
	for _, conversationVisible := range []bool{false, true} {
		dashboard := dashboardWithSession(runningSession())
		serveAttention(dashboard, []string{"session-1"}, core.SessionAttentionNeedsInput)
		request := booleanQuestion("session-1", false)
		mutableSessionDetail(dashboard, "session-1").pendingElicitations = []core.ElicitationRequest{request}
		serveFormCard(dashboard, "session-1", nil, &request)
		if conversationVisible {
			current := "session-1"
			dashboard.SetCurrentSession(&current)
		} else {
			dashboard.SetCurrentSession(nil)
		}
		dashboard.focus = FocusNotifications
		dashboard.notifications.size = core.PaneSizeMaximized
		active, answers := formChat("session-1", request)
		chats := map[string]*chat.ActiveChat{"session-1": active}
		terminal := newRatatuiTestTerminal(120, 44)
		lines := terminal.drawCombined(dashboard, chats)
		card := *dashboard.notificationsArea
		offset := point(lines[card.Min.Y:card.Max.Y], chat.CheckboxMarker(false))
		column := offset.X
		selection := chat.NewSelectionState()
		for _, kind := range []tea.MouseMsg{leftButtonDown, leftButtonUp} {
			mouse := mouseAtCell(kind, column, card.Min.Y+offset.Y)
			owns, ok := dashboard.NotificationFormMouseOwnership(chats, mouse)
			if !ok {
				t.Fatal("the notification form decides the pointer")
			}
			if _, down := kind.(tea.MouseClickMsg); !owns && down {
				selection.OnMouseDown(column, card.Min.Y+offset.Y, dashboard.FrameSurfaces())
			}
			if !owns {
				t.Fatal("the form owns its checkbox")
			}
			if selection.ActiveSurface() != nil {
				t.Fatal("no selection started")
			}
			if !requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouse)).Consumed {
				t.Fatal("the form consumed the pointer")
			}
		}
		// Hard-won: 9739e9996: lifecycle-owned chats still accepted inline answers before redraw.
		dashboard.BeginSessionOperation("session-1", SessionOperationKindMoving, nil)
		for _, code := range []rune{tea.KeyEnter, tea.KeyEscape} {
			if _, ok := dashboard.HandleNotificationFormEvent(chats, key(code)); ok {
				t.Fatal("a lifecycle-owned chat takes no answers")
			}
		}
		terminal.drawCombined(dashboard, chats)
		if dashboard.NotificationFormOwnsKeyboard() {
			t.Fatal("a lifecycle-owned form does not own the keyboard")
		}
		dashboard.FinishSessionOperation("session-1")
		// Hard-won: 9739e9996: border-only forms claimed Esc while rejecting it themselves.
		dashboard.notifications.size = core.PaneSizeStandard
		borderOnly := newRatatuiTestTerminal(120, 19)
		borderOnly.drawCombined(dashboard, chats)
		if dashboard.notificationsArea == nil {
			t.Fatal("the notifications pane is drawn")
		}
		if dashboard.NotificationFormOwnsKeyboard() {
			t.Fatal("a border-only form does not own the keyboard")
		}
		if chats["session-1"].ElicitationInputAvailableIn(chat.ElicitationSurfaceNotification) {
			t.Fatal("a border-only form takes no input")
		}
		dashboard.notifications.size = core.PaneSizeMaximized
		tiny := newRatatuiTestTerminal(120, 5)
		tiny.drawCombined(dashboard, chats)
		if dashboard.NotificationFormOwnsKeyboard() {
			t.Fatal("a hidden form does not own the keyboard")
		}
		for _, code := range []rune{tea.KeyEnter, tea.KeyEscape} {
			if _, ok := dashboard.HandleNotificationFormEvent(chats, key(code)); ok {
				t.Fatal("a hidden form takes no keys")
			}
		}
		if !chats["session-1"].HasElicitation() {
			t.Fatal("the form is still pending")
		}
		noAnswer(t, answers)
		lines = terminal.drawCombined(dashboard, chats)
		// Hard-won: 1f62bc101: a hidden presentation could regain an old pressed button when it returned.
		card = *dashboard.notificationsArea
		at := point(lines[card.Min.Y:card.Max.Y], "Answer & Next")
		press := mouseAtCell(leftButtonDown, at.X, card.Min.Y+at.Y)
		requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, press))
		tiny.drawCombined(dashboard, chats)
		if _, captured := chats["session-1"].ElicitationPointerSurface(); captured {
			t.Fatal("a hidden presentation releases its press")
		}
		terminal.drawCombined(dashboard, chats)
		requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouseLike(leftButtonUp, press.Mouse())))
		if !chats["session-1"].HasElicitation() {
			t.Fatal("the release did not answer")
		}
		noAnswer(t, answers)
		requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, key(tea.KeyEnter)))
		answer := recvAnswer(t, answers)
		if !answer.response.Equal(acceptResponse("path", core.ElicitationValueBoolean(true))) {
			t.Fatalf("response %#v", answer.response)
		}
		// Replacing the chat leaves an old host rectangle until redraw;
		// only the newly owned dialog knows that it has not been shown.
		next := booleanQuestion("session-1", true)
		next.ID = "not-yet-rendered"
		active, nextAnswers := formChat("session-1", next)
		chats["session-1"] = active
		result := requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, key(tea.KeyEnter)))
		if result.Consumed {
			t.Fatal("an unshown form consumes nothing")
		}
		if !chats["session-1"].HasElicitation() {
			t.Fatal("the unshown form is still pending")
		}
		noAnswer(t, nextAnswers)
		// Hard-won: 45223194f: a visible one-row answer control rejected every event without a message row.
		active, compactAnswers := formChat("session-1", question("session-1"))
		chats["session-1"] = active
		dashboard.notifications.size = core.PaneSizeStandard
		compact := newRatatuiTestTerminal(120, 20)
		compact.drawCombined(dashboard, chats)
		area := *dashboard.notifications.formArea
		if area.Dy() != 3 {
			t.Fatalf("one row inside the form border: %v", area)
		}
		// Hard-won: 5dc2084: compact controls accepted answers while the request had no visible row.
		message := dashboard.frameSurfaces.Surface(chat.SurfaceIdNotificationText(dashboard.notifications.textGeneration))
		if message == nil || message.Rect.Dx() <= 0 || message.Rect.Dy() <= 0 {
			t.Fatalf("the question has a visible row: %#v", message)
		}
		if !slices.ContainsFunc(bufferLines(compact.buffer), func(line string) bool { return strings.Contains(line, "path") }) {
			t.Fatal("the compact form shows its field")
		}

		// Hard-won: c9867384e: inline focus left a prefix armed, so q detached instead of entering the answer.
		dashboard.RouteBoundKey(tea.KeyPressMsg{Code: 'b', Mod: tea.ModCtrl})
		if !dashboard.PrefixPending() {
			t.Fatal("the prefix is armed")
		}
		dashboard.resizeMode = true
		for _, kind := range []tea.MouseMsg{leftButtonDown, leftButtonUp} {
			mouse := mouseAtCell(kind, area.Min.X+area.Dx()/2+4, area.Min.Y+1)
			if !requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouse)).Consumed {
				t.Fatal("the compact form consumed the pointer")
			}
		}
		if dashboard.PrefixPending() {
			t.Fatal("a press in the form disarms the prefix")
		}
		if dashboard.resizeMode {
			t.Fatal("a press in the form leaves resize mode")
		}
		if route := dashboard.RouteBoundKey(key('q')); !reflect.DeepEqual(route, KeyRouteForward{}) {
			t.Fatalf("q is forwarded: %#v", route)
		}
		for _, event := range []tea.Msg{key('q'), tea.PasteMsg{Content: "compact"}, key(tea.KeyEnter)} {
			if !requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, event)).Consumed {
				t.Fatalf("the compact form consumed %#v", event)
			}
		}
		answer = recvAnswer(t, compactAnswers)
		if !answer.response.Equal(acceptResponse("path", core.ElicitationValueString("qcompact"))) {
			t.Fatalf("response %#v", answer.response)
		}
	}

	// Hard-won: 45223194f: release over another session's notification bypassed the conversation's capture.
	dashboard := dashboardWithSession(runningSession())
	second := runningSession()
	second.ID = "session-2"
	dashboard.durableRecords.Insert(second.ID, second)
	dashboard.state.Mut().Sessions.Insert(second.ID, second)
	serveAttention(dashboard, []string{"session-1"}, core.SessionAttentionNeedsInput)
	request := booleanQuestion("session-1", false)
	mutableSessionDetail(dashboard, "session-1").pendingElicitations = []core.ElicitationRequest{request}
	serveFormCard(dashboard, "session-1", nil, &request)
	current := "session-2"
	dashboard.SetCurrentSession(&current)
	dashboard.notifications.size = core.PaneSizeMaximized
	first, _ := formChat("session-1", request)
	secondChat, _ := formChat("session-2", request)
	chats := map[string]*chat.ActiveChat{"session-1": first, "session-2": secondChat}
	terminal := newRatatuiTestTerminal(120, 44)
	lines := terminal.drawCombined(dashboard, chats)
	at := point(lines, chat.CheckboxMarker(false))
	if at.Y >= dashboard.notificationsArea.Min.Y {
		t.Fatal("the pressed control belongs to the conversation")
	}
	chats["session-2"].HandleEventResult(mouseAtCell(leftButtonDown, at.X, at.Y))
	if owner, ok := dashboard.ConversationPointerSession(chats); !ok || owner != "session-2" {
		t.Fatalf("conversation capture: %q %v", owner, ok)
	}
	area := *dashboard.notifications.formArea
	release := mouseAtCell(leftButtonUp, area.Min.X+2, area.Min.Y+1)
	if _, ok := dashboard.NotificationFormMouseOwnership(chats, release); ok {
		t.Fatal("the conversation's capture decides the release")
	}
	if _, ok := dashboard.HandleNotificationFormEvent(chats, release); ok {
		t.Fatal("the form does not take the conversation's release")
	}
	owner, _ := dashboard.ConversationPointerSession(chats)
	chats[owner].HandleEventResult(release)
	if _, ok := dashboard.ConversationPointerSession(chats); ok {
		t.Fatal("the release ended the capture")
	}
	// Hard-won: 9739e9996: global commands left a different session's notification button armed.
	card := *dashboard.notificationsArea
	at = point(lines[card.Min.Y:card.Max.Y], "Answer & Next")
	press := mouseAtCell(leftButtonDown, at.X, card.Min.Y+at.Y)
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, press))
	if surface, ok := chats["session-1"].ElicitationPointerSurface(); !ok || surface != chat.ElicitationSurfaceNotification {
		t.Fatal("the notification presentation holds the press")
	}
	dashboard.CancelAllComponentPointers(chats)
	dashboard.focus = FocusSessions
	if _, ok := chats["session-1"].ElicitationPointerSurface(); ok {
		t.Fatal("cancelling disarms the notification button")
	}
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouseLike(leftButtonUp, press.Mouse())))
	if !chats["session-1"].HasElicitation() {
		t.Fatal("a cancelled press does not answer")
	}
	// Hard-won: dc29e81c6: terminal focus loss left a notification-only button armed.
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, press))
	dashboard.PrepareComponentEvent(tea.BlurMsg{}, chats)
	if _, ok := chats["session-1"].ElicitationPointerSurface(); ok {
		t.Fatal("focus loss disarms the notification button")
	}
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouseLike(leftButtonUp, press.Mouse())))
	if !chats["session-1"].HasElicitation() {
		t.Fatal("a disarmed press does not answer")
	}
	// Hard-won: c9867384e: a dashboard button's release was stolen by the inline form.
	at = point(lines[card.Min.Y:card.Max.Y], "Go to Session")
	dashboardPress := mouseAtCell(leftButtonDown, at.X, card.Min.Y+at.Y)
	dashboard.HandleMouse(dashboardPress)
	if !dashboard.ComponentPointerCaptured() {
		t.Fatal("the dashboard button holds the press")
	}
	releaseInForm := mouseAtCell(leftButtonUp, area.Min.X+3, area.Min.Y+1)
	if _, ok := dashboard.HandleNotificationFormEvent(chats, releaseInForm); ok {
		t.Fatal("the form does not steal the dashboard button's release")
	}
	dashboard.HandleMouse(releaseInForm)
	if dashboard.ComponentPointerCaptured() {
		t.Fatal("the release ends the dashboard capture")
	}
	dashboard.HandleMouse(dashboardPress)
	requireDashboardAction(t, dashboard.HandleMouse(mouseLike(leftButtonUp, dashboardPress.Mouse())), DashboardActionOpen{SessionID: "session-1"})
	// Go to Session above moved Browse to the notification's session.
	// Put the competing conversation back before exercising its capture.
	dashboard.SetCurrentSession(&current)
	// Hard-won: e1b4abc81: lifecycle-owned conversation capture blocked an unrelated notification form.
	at = point(lines, chat.CheckboxMarker(false))
	chats["session-2"].HandleEventResult(mouseAtCell(leftButtonDown, at.X, at.Y))
	if surface, ok := chats["session-2"].ElicitationPointerSurface(); !ok || surface != chat.ElicitationSurfaceConversation {
		t.Fatal("the conversation holds its press")
	}
	dashboard.BeginSessionOperation("session-2", SessionOperationKindMoving, nil)
	if _, ok := dashboard.ConversationPointerSession(chats); ok {
		t.Fatal("a lifecycle-owned conversation holds no capture")
	}
	lines = terminal.drawCombined(dashboard, chats)
	if _, ok := chats["session-2"].ElicitationPointerSurface(); ok {
		t.Fatal("the redraw released the lifecycle-owned capture")
	}
	card = *dashboard.notificationsArea
	at = point(lines[card.Min.Y:card.Max.Y], chat.CheckboxMarker(false))
	press = mouseAtCell(leftButtonDown, at.X, card.Min.Y+at.Y)
	if owned, ok := dashboard.NotificationFormMouseOwnership(chats, press); !ok || !owned {
		t.Fatal("the notification form owns its checkbox")
	}
	if !requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, press)).Consumed {
		t.Fatal("the form consumed the press")
	}
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouseLike(leftButtonUp, press.Mouse())))
	// Hard-won: dc29e81c6: a transcript thumb released over another session's form kept dragging.
	dashboard.FinishSessionOperation("session-2")
	if session, ok := dashboard.CurrentSessionID(); !ok || session != "session-2" {
		t.Fatalf("current session %q", session)
	}
	projection := core.MaterializedSessionEmpty("session-2")
	projection.AppliedEventOrdinal = 40
	for index := range uint64(40) {
		latest := index + 1
		chunk := json.RawMessage(fmt.Sprintf(`{"content":{"type":"text","text":"Transcript row %d"}}`, index))
		projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
			StableID:                  fmt.Sprintf("scrollbar-history-%d", index),
			Position:                  index,
			LatestContentEventOrdinal: &latest,
			Body:                      core.TranscriptBodyAgent{Chunks: []json.RawMessage{chunk}},
		})
	}
	chats["session-2"].ApplyMaterialized(&projection, nil, nil)
	terminal.drawCombined(dashboard, chats)
	transcript := chats["session-2"].FrameSurfaces().Surface(chat.SurfaceIdTranscript{}).Rect
	scrollbarPress := mouseAtCell(leftButtonDown, transcript.Max.X, transcript.Min.Y+transcript.Dy()/2)
	chats["session-2"].HandleEventResult(scrollbarPress)
	if !chats["session-2"].TranscriptScrollbarDragging() {
		t.Fatal("the press grabbed the transcript thumb")
	}
	if owner, ok := dashboard.ConversationPointerSession(chats); !ok || owner != "session-2" {
		t.Fatal("the thumb captures the pointer for its conversation")
	}
	form := *dashboard.notifications.formArea
	release = mouseAtCell(leftButtonUp, form.Min.X+3, form.Min.Y+1)
	if _, ok := dashboard.HandleNotificationFormEvent(chats, release); ok {
		t.Fatal("the form does not take the thumb's release")
	}
	owner, _ = dashboard.ConversationPointerSession(chats)
	chats[owner].HandleEventResult(release)
	if chats["session-2"].TranscriptScrollbarDragging() {
		t.Fatal("the release ended the drag")
	}
	chats["session-2"].HandleEventResult(scrollbarPress)
	if !chats["session-2"].TranscriptScrollbarDragging() {
		t.Fatal("the thumb is grabbed again")
	}
	dashboard.PrepareComponentEvent(tea.BlurMsg{}, chats)
	if chats["session-2"].TranscriptScrollbarDragging() {
		t.Fatal("focus loss ends the drag")
	}
	chats["session-2"].HandleEventResult(scrollbarPress)
	if !chats["session-2"].TranscriptScrollbarDragging() {
		t.Fatal("focus loss disarms capture without deleting the displayed hitbox")
	}
	// Hard-won: 56747ef: the earlier-message reader left a hidden transcript thumb captured.
	chats["session-2"].HandleEventResult(tea.KeyPressMsg{Code: tea.KeyPgUp, Mod: tea.ModCtrl})
	if chats["session-2"].TranscriptScrollbarDragging() {
		t.Fatal("the earlier-message reader released the thumb")
	}
	if _, ok := dashboard.ConversationPointerSession(chats); ok {
		t.Fatal("no conversation holds the pointer")
	}
	if owned, ok := dashboard.NotificationFormMouseOwnership(chats, release); !ok || !owned {
		t.Fatal("the form owns its own release again")
	}
}

// notificationsPaneDashboard is Rust's notifications_pane::tests::dashboard:
// two questions and one important update waiting in the Notifications pane,
// which has focus.
func notificationsPaneDashboard() *DashboardState {
	dashboard := dashboardWithSession(runningSession())
	dashboard.state.Mut().Sessions.Clear()
	for _, fixture := range []struct {
		id        string
		title     string
		attention core.SessionAttention
		recency   uint64
		target    core.NotificationDismissal
	}{
		{"q1", "Newest question", core.SessionAttentionNeedsInput, 30, core.NotificationDismissalQuestion{Value: core.InferredQuestion{TurnID: "turn-1", CompletedOrdinal: 4}}},
		{"q2", "Older question", core.SessionAttentionNeedsInput, 20, core.NotificationDismissalQuestion{Value: core.InferredQuestion{TurnID: "turn-2", CompletedOrdinal: 3}}},
		{"i1", "Important update", core.SessionAttentionImportant, 40, core.NotificationDismissalReadThrough{Value: 5}},
	} {
		session := runningSession()
		session.ID = fixture.id
		session.Title = fixture.title
		session.ACPSessionTitle = nil
		session.UpdatedAt = ""
		session.ViewedThroughEventOrdinal = 0
		dashboard.state.Mut().Sessions.Insert(fixture.id, session)
		serveAttention(dashboard, []string{fixture.id}, fixture.attention)
		details := dashboard.sessionDetails.Mut()
		if *details == nil {
			*details = map[string]*SessionDetail{}
		}
		detail, ok := (*details)[fixture.id]
		if !ok {
			detail = &SessionDetail{}
			(*details)[fixture.id] = detail
		}
		recency := fixture.recency
		detail.lastActivityAtMs = &recency
		message := "Which approach should we take?\nThe question stays here until you answer or clear it."
		detail.lastAgentMessage = &message
		dashboard.notificationDismissals.Mut().Insert(fixture.id, fixture.target)
	}
	dashboard.focus = FocusNotifications
	return dashboard
}

// publishedDashboard is notificationsPaneDashboard with the daemon's
// published cards: q1's question stack with a held form for q2, and i1's
// important update.
func publishedDashboard() *DashboardState {
	dashboard := notificationsPaneDashboard()
	holder := "q2"
	cards := core.NewSnapshotMap[string, client.NotificationCard]()
	cards.Insert("q1", client.NotificationCard{
		SinceMs: 20,
		Badge:   core.NotificationKindNeedsInput,
		Entries: []core.NotificationStackEntry{{
			CompletedAtMs:    20,
			CompletedOrdinal: 4,
			Kind:             core.NotificationKindImportant,
			Message:          core.EntryMessageFound{Value: "outside-tail"},
		}},
		Dismissal: core.NotificationDismissalStack{Revision: 50},
		Holder:    &holder,
		Form: &client.CardForm{
			Source:     client.CardFormSourceElicitation,
			RaisedAtMs: 60,
			RequestID:  "request",
			Message:    "Which approach should we take?\nThe question stays here until you answer or clear it.",
		},
	})
	cards.Insert("i1", client.NotificationCard{
		SinceMs: 10,
		Badge:   core.NotificationKindImportant,
		Entries: []core.NotificationStackEntry{{
			CompletedAtMs:    10,
			CompletedOrdinal: 7,
			Kind:             core.NotificationKindImportant,
			Message:          core.EntryMessageNone{},
		}},
		Dismissal: core.NotificationDismissalReadThrough{Value: 7},
	})
	dashboard.SetNotificationCards(&cards)
	return dashboard
}

func anyLineContains(lines []string, needle string) bool {
	return slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, needle) })
}

func requireReadRequest(t *testing.T, request *NotificationReadRequest) NotificationReadRequest {
	t.Helper()
	if request == nil {
		t.Fatal("a message read is wanted")
	}
	return *request
}

func sameReadRequest(a *NotificationReadRequest, b *NotificationReadRequest) bool {
	if a == nil || b == nil {
		return a == b
	}
	return a.Compare(*b) == 0
}

// Hard-won: dc77f11c1: an id-less question skipped its bounded text read.
// Hard-won: 1f62bc101: changing card items retained a hidden form's armed click.
func TestNotificationNavigationKeepsReadsBoundToTheSelectedEntry(t *testing.T) {
	dashboard := publishedDashboard()
	entries := dashboard.notificationEntries()
	if len(entries) != 2 {
		t.Fatalf("q2 has attention but no published card: %d entries", len(entries))
	}
	if entries[0].sessionID != "i1" {
		t.Fatal("the older important card stays ahead of the newer question")
	}
	dashboard.HandleKey(altKey('s'))
	if displayed, _ := dashboard.DisplayedNotificationSessionID(); displayed != "q2" {
		t.Fatalf("displayed %q", displayed)
	}
	if destination := dashboard.notificationDestination(&dashboard.notificationEntries()[0]); destination != "q2" {
		t.Fatal("the selected live form targets its holder")
	}

	// Hard-won: a882023b6: a form beside dismissible messages fell back to a free-text reply.
	if session, ok := dashboard.NotificationFormSession(); !ok || session != "q2" {
		t.Fatal("a selected form remains answerable beside dismissible messages")
	}
	// Hard-won: 1f62bc101: changing the selected item left the retained chat's notification button armed.
	request := booleanQuestion("q2", true)
	active, answers := formChat("q2", request)
	chats := map[string]*chat.ActiveChat{"q2": active}
	current := "q2"
	dashboard.SetCurrentSession(&current)
	terminal := newRatatuiTestTerminal(120, 44)
	lines := terminal.drawCombined(dashboard, chats)
	card := *dashboard.notificationsArea
	at := point(lines[card.Min.Y:card.Max.Y], "Answer & Next")
	press := mouseAtCell(leftButtonDown, at.X, card.Min.Y+at.Y)
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, press))
	if surface, ok := chats["q2"].ElicitationPointerSurface(); !ok || surface != chat.ElicitationSurfaceNotification {
		t.Fatal("the notification presentation holds the press")
	}
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	terminal.drawCombined(dashboard, chats)
	if _, ok := chats["q2"].ElicitationPointerSurface(); ok {
		t.Fatal("changing the item released the press")
	}
	dashboard.HandleKey(altKey(tea.KeyPgDown))
	terminal.drawCombined(dashboard, chats)
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, mouseLike(leftButtonUp, press.Mouse())))
	if !chats["q2"].HasElicitation() {
		t.Fatal("the stale release did not answer")
	}
	noAnswer(t, answers)
	newest := drawn(dashboard, 120, 44)
	if !anyLineContains(newest, "2 of 2") || !anyLineContains(newest, "Which approach") {
		t.Fatalf("newest item: %#v", newest)
	}
	dashboard.HandleKey(altKey(tea.KeyPgDown))
	if dashboard.WantedNotificationMessage() != nil {
		t.Fatal("a form needs no message read")
	}

	previous := point(newest, "Previous")
	dashboard.HandleMouse(mouseAt(leftButtonDown, previous))
	dashboard.HandleMouse(mouseAt(leftButtonUp, previous))
	// Hard-won: c9867384e: a same-batch paste used the old message frame after revealing a form.
	revision := dashboard.notifications.selectionRevision
	dashboard.HandleKey(altKey(tea.KeyPgDown))
	if revision == dashboard.notifications.selectionRevision {
		t.Fatal("moving to the form is a new selection")
	}
	if dashboard.notifications.formArea != nil {
		t.Fatal("the form has not been drawn yet")
	}
	if !dashboard.NotificationInputNeedsFrame() {
		t.Fatal("input waits for the form's frame")
	}
	drawn(dashboard, 120, 44)
	if dashboard.NotificationInputNeedsFrame() {
		t.Fatal("the frame drew the form")
	}
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	item := requireReadRequest(t, dashboard.WantedNotificationMessage())
	if item.SessionID != "q1" {
		t.Fatal("an entry belongs to its own session")
	}
	if !reflect.DeepEqual(item.Source, NotificationReadSource(NotificationReadSourceItem{Value: "outside-tail"})) {
		t.Fatalf("source %#v", item.Source)
	}
	dashboard.SyncNotificationMessageRead(&item, true)
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	if destination := dashboard.notificationDestination(&dashboard.notificationEntries()[0]); destination != "q1" {
		t.Fatal("the selected stack entry targets its own session")
	}
	if !dashboard.HasNotificationForSession("q2") {
		t.Fatal("the same card still contains a live form for the child")
	}
	if !sameReadRequest(dashboard.WantedNotificationMessage(), &item) {
		t.Fatal("the read stays wanted")
	}
	if !anyLineContains(drawn(dashboard, 120, 44), "1 of 2") {
		t.Fatal("first of two")
	}

	latest := uint64(3)
	stored := &core.TranscriptItem{
		StableID:                  "outside-tail",
		Position:                  2,
		LatestContentEventOrdinal: &latest,
		Body: core.TranscriptBodyAgent{Chunks: []json.RawMessage{
			json.RawMessage(`{"content":{"type":"text", "text":"The earlier completed update"}}`),
		}},
	}
	text := "The earlier completed update"
	if !dashboard.NotificationMessageReadFinished(&item, &text) {
		t.Fatal("the read finished for the wanted entry")
	}
	if !anyLineContains(drawn(dashboard, 120, 44), "The earlier completed update") {
		t.Fatal("the read text is shown")
	}
	cards := mutableNotificationCards(dashboard)
	questionCard := editCard(&cards, "q1")
	arrived := questionCard.Entries[0]
	arrived.CompletedOrdinal = 6
	arrived.CompletedAtMs = 40
	arrived.Message = core.EntryMessageNone{}
	questionCard.Entries = append(questionCard.Entries, arrived)
	questionCard.Dismissal = core.NotificationDismissalStack{Revision: 52}

	skipped := editCard(&cards, "i1")
	arrived = skipped.Entries[0]
	arrived.CompletedOrdinal = 8
	arrived.CompletedAtMs = 45
	skipped.Entries = append(skipped.Entries, arrived)
	skipped.Dismissal = core.NotificationDismissalReadThrough{Value: 8}
	dashboard.SetNotificationCards(&cards)

	reading := drawn(dashboard, 120, 44)
	if !anyLineContains(reading, "1 of 3") || !anyLineContains(reading, "The earlier completed update") {
		t.Fatalf("reading: %#v", reading)
	}
	if !sameReadRequest(dashboard.WantedNotificationMessage(), &item) {
		t.Fatal("the selected entry keeps its read")
	}

	dashboard.HandleKey(altKey(tea.KeyPgDown))
	if dashboard.NotificationMessageReadFinished(&item, nil) {
		t.Fatal("a read for an unselected entry is stale")
	}
	dashboard.HandleKey(altKey(tea.KeyPgDown))
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	returning := requireReadRequest(t, dashboard.WantedNotificationMessage())
	if returning.Compare(item) == 0 {
		t.Fatal("returning to an entry is a new read")
	}
	dashboard.SyncNotificationMessageRead(&returning, true)
	if dashboard.NotificationMessageReadFinished(&item, nil) {
		t.Fatal("the earlier read is stale")
	}

	cards = mutableNotificationCards(dashboard)
	replacement := editCard(&cards, "q1")
	replacement.Entries[0].CompletedOrdinal = 5
	replacement.Entries[0].CompletedAtMs = 25
	replacement.SinceMs = 25
	replacement.Form.RequestID = "replacement"
	dashboard.SetNotificationCards(&cards)
	if dashboard.NotificationMessageReadFinished(&returning, nil) {
		t.Fatal("a replaced card's read is stale")
	}
	if !anyLineContains(drawn(dashboard, 120, 44), "3 of 3") {
		t.Fatal("the replacement opens at its newest item")
	}

	projection := materializedSessionFor("q1", []*core.TranscriptItem{stored})
	projection.AppliedEventOrdinal = 5
	dashboard.ApplyMaterializedSession(&projection)
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	if dashboard.WantedNotificationMessage() != nil {
		t.Fatal("loaded text needs no read")
	}
	if !anyLineContains(drawn(dashboard, 120, 44), "The earlier completed update") {
		t.Fatal("loaded text is shown")
	}

	dashboard.SetQuestionMessage("q1", nil, true)
	cards = mutableNotificationCards(dashboard)
	questionEntry := &editCard(&cards, "q1").Entries[0]
	questionEntry.Kind = core.NotificationKindNeedsInput
	questionEntry.Message = core.EntryMessageLocating{}
	dashboard.SetNotificationCards(&cards)
	dashboard.HandleKey(altKey(tea.KeyPgUp))
	locating := requireReadRequest(t, dashboard.WantedNotificationMessage())
	if locating.SessionID != "q1" {
		t.Fatalf("locating session %q", locating.SessionID)
	}
	if !reflect.DeepEqual(locating.Source, NotificationReadSource(NotificationReadSourceThrough{Value: 5})) {
		t.Fatalf("locating source %#v", locating.Source)
	}

	cards = mutableNotificationCards(dashboard)
	editCard(&cards, "q1").Entries = nil
	dashboard.SetNotificationCards(&cards)
	if !anyLineContains(drawn(dashboard, 120, 44), "1 of 1") {
		t.Fatal("the form alone remains")
	}
	if dashboard.WantedNotificationMessage() != nil {
		t.Fatal("a form needs no read")
	}

	notificationsSkipTo(t, dashboard, "i1")
	requireDashboardAction(t, dashboard.HandleKey(altKey('n')), DashboardActionDismissNotification{
		SessionID: "i1",
		Target:    core.NotificationDismissalReadThrough{Value: 8},
	})
	if !dashboard.notificationDismissalPending("i1") {
		t.Fatal("the dismissal is pending")
	}
	dashboard.NotificationDismissalFinished("i1", core.NotificationDismissalReadThrough{Value: 8}, 8, nil)
	if dashboard.notificationDismissalPending("i1") {
		t.Fatal("the dismissal finished")
	}
	if viewed := dashboard.state.Get().Sessions.GetShared("i1").ViewedThroughEventOrdinal; viewed != 8 {
		t.Fatalf("viewed through %d", viewed)
	}
	notificationsSkipTo(t, dashboard, "q1")
	requireDashboardAction(t, dashboard.HandleKey(altKey('n')), DashboardActionDismissNotification{
		SessionID: "q1",
		Target:    core.NotificationDismissalStack{Revision: 52},
	})
	dashboard.HandleKey(altKey('s'))
	bounded := requireReadRequest(t, dashboard.WantedNotificationMessage())
	if bounded.SessionID != "i1" {
		t.Fatalf("bounded session %q", bounded.SessionID)
	}
	if !reflect.DeepEqual(bounded.Source, NotificationReadSource(NotificationReadSourceThrough{Value: 8})) {
		t.Fatalf("bounded source %#v", bounded.Source)
	}

	empty := core.NewSnapshotMap[string, client.NotificationCard]()
	dashboard.SetNotificationCards(&empty)
	if len(dashboard.notificationEntries()) != 0 {
		t.Fatal("no cards, no entries")
	}
	if len(dashboard.notifications.skipped) != 0 {
		t.Fatal("deferrals of vanished cards are pruned")
	}

	// Hard-won: b817d9202: a native child's projected form suppressed its owner's quick reply forever.
	owner := "q1"
	native := core.NewSnapshotMap[string, client.NotificationCard]()
	native.Insert("q1", client.NotificationCard{
		SinceMs:   1,
		Badge:     core.NotificationKindNeedsInput,
		Entries:   []core.NotificationStackEntry{},
		Dismissal: nil,
		Holder:    &owner,
		Form: func() *client.CardForm {
			form := client.CardFormNative("child", 1, "native-request", 1, "Approve native action?")
			return &form
		}(),
	})
	dashboard.SetNotificationCards(&native)
	if _, ok := dashboard.NotificationFormSession(); ok {
		t.Fatal("a native form is not an inline form")
	}
	if !anyLineContains(drawn(dashboard, 120, 44), "Quick reply:") {
		t.Fatal("the quick reply remains")
	}
	dashboard.HandleKey(key('y'))
	reply, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("native forms keep the existing quick reply dispatcher")
	}
	if reply.SessionID != "q1" || reply.Prompt.Payload.Text != "y" {
		t.Fatalf("reply %#v", reply)
	}
	// Hard-won: e1b4abc81: completed review findings entered the inline loader without any elicitation to serve.
	dashboard.NotificationReplyFinished("q1", reply.Prompt.CommandID, nil)
	findings := core.NewSnapshotMap[string, client.NotificationCard]()
	findings.Insert("q1", client.NotificationCard{
		SinceMs:   2,
		Badge:     core.NotificationKindNeedsInput,
		Entries:   []core.NotificationStackEntry{},
		Dismissal: nil,
		Holder:    &owner,
		Form: func() *client.CardForm {
			form := client.CardFormReviewFindings("reviewer-a", 2, "Review found issues")
			return &form
		}(),
	})
	dashboard.SetNotificationCards(&findings)
	if _, ok := dashboard.NotificationFormSession(); ok {
		t.Fatal("review findings are not an inline form")
	}
	lines = drawn(dashboard, 120, 44)
	if !anyLineContains(lines, "Quick reply:") {
		t.Fatal("review findings keep the quick reply")
	}
	if anyLineContains(lines, "Waiting for the session's form update") {
		t.Fatal("review findings do not wait for a form")
	}
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionOpen{SessionID: "q1"})
	// Hard-won: dc29e81c6: a background form attachment cleared or replaced an unrelated failure notice.
	dashboard.SetFailureNotice("Unrelated command failed")
	notice, _ := dashboard.Notice()
	unavailable := "Worker unavailable"
	dashboard.ReportChatAttachmentResult("q2", &unavailable, false)
	if shown, _ := dashboard.Notice(); shown != notice {
		t.Fatalf("notice %q", shown)
	}
	if formError := dashboard.notifications.formErrors["q2"]; formError != "Worker unavailable" {
		t.Fatalf("form error %q", formError)
	}
	dashboard.ReportChatAttachmentResult("q2", nil, false)
	if shown, _ := dashboard.Notice(); shown != notice {
		t.Fatalf("notice %q", shown)
	}
	if _, ok := dashboard.notifications.formErrors["q2"]; ok {
		t.Fatal("a successful attach clears the form error")
	}
}

// Hard-won: 6597a0761: a new card from the same session kept the old scroll offset.
func TestANewCardFromTheSameSessionOpensAtItsBeginning(t *testing.T) {
	dashboard := notificationsPaneDashboard()
	var long []string
	for i := range 80 {
		long = append(long, fmt.Sprintf("line %d", i))
	}
	text := strings.Join(long, "\n")
	for _, detail := range *dashboard.sessionDetails.Mut() {
		message := text
		detail.lastAgentMessage = &message
	}
	drawn(dashboard, 140, 50)
	dashboard.scrollNotification(5)
	drawn(dashboard, 140, 50)
	if dashboard.notifications.scroll != 5 {
		t.Fatalf("scroll %d", dashboard.notifications.scroll)
	}
	// The same session publishes a new question while staying the top card.
	dashboard.notificationDismissals.Mut().Insert("q1", core.NotificationDismissalQuestion{Value: core.InferredQuestion{
		TurnID:           "turn-9",
		CompletedOrdinal: 9,
	}})
	drawn(dashboard, 140, 50)
	if dashboard.notificationEntries()[0].sessionID != "q1" {
		t.Fatal("q1 stays the top card")
	}
	if dashboard.notifications.scroll != 0 {
		t.Fatalf("scroll %d", dashboard.notifications.scroll)
	}
}

// notificationsSkipTo is Rust's skip_to: skip until sessionID leads the
// queue, at most one pass through it, so an unreachable card fails the test
// instead of hanging it.
func notificationsSkipTo(t *testing.T, dashboard *DashboardState, sessionID string) {
	t.Helper()
	for range len(dashboard.notificationEntries()) + 1 {
		if dashboard.notificationEntries()[0].sessionID == sessionID {
			return
		}
		dashboard.HandleKey(altKey('s'))
	}
	t.Fatalf("%s never reached the front of the notification queue", sessionID)
}

func TestNotificationsKeysRotateSubmitAndRestoreFailedDismissals(t *testing.T) {
	dashboard := notificationsPaneDashboard()
	front := func(dashboard *DashboardState) string { return dashboard.notificationEntries()[0].sessionID }
	if front(dashboard) != "q1" {
		t.Fatal("q1 leads")
	}
	dashboard.HandleKey(altKey('s'))
	if front(dashboard) != "q2" {
		t.Fatal("q2 leads")
	}
	dashboard.HandleKey(altKey('s'))
	if front(dashboard) != "i1" {
		t.Fatal("i1 leads")
	}
	dashboard.HandleKey(altKey('s'))
	if front(dashboard) != "q1" {
		t.Fatal("q1 leads again")
	}
	dismissal, ok := dashboard.HandleKey(altKey('n')).(DashboardActionDismissNotification)
	if !ok {
		t.Fatal("dismissal action")
	}
	if dismissal.SessionID != "q1" {
		t.Fatalf("dismissed %q", dismissal.SessionID)
	}
	if level := dashboard.AttentionLevel("q1"); level != AttentionLevelIdle {
		t.Fatalf("q1 attention %v", level)
	}
	if front(dashboard) != "q2" {
		t.Fatal("q2 leads after the dismissal")
	}
	dashboard.NotificationDismissalFinished(dismissal.SessionID, dismissal.Target, 0, errors.New("Worker disconnected"))
	if !slices.ContainsFunc(dashboard.notificationEntries(), func(entry NotificationEntry) bool { return entry.sessionID == "q1" }) {
		t.Fatal("a failed dismissal restores the card")
	}
	if dashboard.notifications.errors["q1"] != "Worker disconnected" {
		t.Fatalf("error %q", dashboard.notifications.errors["q1"])
	}
	if level := dashboard.AttentionLevel("q1"); level != AttentionLevelWaiting {
		t.Fatalf("q1 attention %v", level)
	}
	notificationsSkipTo(t, dashboard, "i1")
	requireDashboardAction(t, dashboard.HandleKey(altKey('n')), DashboardActionDismissNotification{
		SessionID: "i1",
		Target:    core.NotificationDismissalReadThrough{Value: 5},
	})
	allWaiting := func() bool {
		return !slices.ContainsFunc(dashboard.notificationEntries(), func(entry NotificationEntry) bool {
			return entry.level != AttentionLevelWaiting
		})
	}
	if len(dashboard.notificationEntries()) != 2 || !allWaiting() {
		t.Fatal("only the two questions remain")
	}
	if level := dashboard.AttentionLevel("i1"); level != AttentionLevelIdle {
		t.Fatalf("i1 attention %v", level)
	}
	dashboard.NotificationDismissalFinished("i1", core.NotificationDismissalReadThrough{Value: 5}, 5, nil)
	if level := dashboard.AttentionLevel("i1"); level != AttentionLevelIdle {
		t.Fatalf("i1 attention %v", level)
	}
	// A new served dismissal target invalidates the same cached attention
	// projection without needing another session-record change.
	dashboard.notificationDismissals.Mut().Insert("i1", core.NotificationDismissalReadThrough{Value: 6})
	if level := dashboard.AttentionLevel("i1"); level != AttentionLevelImportant {
		t.Fatalf("i1 attention %v", level)
	}
	dashboard.notificationDismissals.Mut().Insert("i1", core.NotificationDismissalReadThrough{Value: 5})
	if level := dashboard.AttentionLevel("i1"); level != AttentionLevelIdle {
		t.Fatalf("i1 attention %v", level)
	}
	if !allWaiting() {
		t.Fatal("only questions remain")
	}

	// An update after the press may replace the card or just its target.
	// Release always carries the pressed identity, never a recomputed top.
	for _, label := range []string{"Go to Session", "Clear & Next", "Skip", "Send"} {
		for _, replaceTarget := range []bool{false, true} {
			race := notificationsPaneDashboard()
			race.quickReplyMut("q1").editor.SetDraft("pressed reply")
			race.quickReplyMut("q2").editor.SetDraft("other reply")
			oldQuestion := "old-question"
			race.SetQuestionMessage("q1", &oldQuestion, true)
			target, _ := race.notificationDismissalTarget("q1")
			lines := drawn(race, 140, 50)
			button := point(lines, label)
			race.HandleMouse(mouseAt(leftButtonDown, button))
			if replaceTarget {
				race.notificationDismissals.Mut().Insert("q1", core.NotificationDismissalReadThrough{Value: 99})
				newQuestion := "new-question"
				race.SetQuestionMessage("q1", &newQuestion, true)
			} else {
				identity := race.notificationEntries()[0].identity()
				race.notifications.skipped = append(race.notifications.skipped, identity.skipIdentity())
			}
			drawn(race, 140, 50)
			switch action := race.HandleMouse(mouseAt(leftButtonUp, button)).(type) {
			case DashboardActionOpen:
				if action.SessionID != "q1" {
					t.Fatalf("%s opened %q", label, action.SessionID)
				}
			case DashboardActionNotificationReply:
				if action.Card.SessionID != "q1" || !reflect.DeepEqual(action.Card.Target, target) || action.SessionID != "q1" {
					t.Fatalf("%s replied %#v", label, action)
				}
				if action.Prompt.Payload.Text != "pressed reply" {
					t.Fatalf("%s sent %q", label, action.Prompt.Payload.Text)
				}
			case DashboardActionDismissNotification:
				if action.SessionID != "q1" || !reflect.DeepEqual(action.Target, target) {
					t.Fatalf("%s dismissed %#v", label, action)
				}
				race.NotificationDismissalFinished(action.SessionID, action.Target, 0, errors.New("notification is no longer current"))
				if !strings.Contains(race.notifications.errors["q1"], "no longer current") {
					t.Fatalf("error %q", race.notifications.errors["q1"])
				}
			case DashboardActionNotificationSkip:
				if action.Card.SessionID != "q1" || !reflect.DeepEqual(action.Card.Target, target) {
					t.Fatalf("%s skipped %#v", label, action)
				}
				var last string
				switch skipped := race.notifications.skipped[len(race.notifications.skipped)-1].(type) {
				case SkippedCardPublished:
					last = skipped.Value
				case SkippedCardLegacy:
					last = skipped.Value.SessionID
				}
				if last != "q1" {
					t.Fatalf("last skipped %q", last)
				}
				if replaceTarget && race.notificationEntries()[0].sessionID != "q1" {
					t.Fatal("Skip must not defer the replacement question")
				}
			default:
				t.Fatalf("captured %s: %#v", label, action)
			}
		}
	}

	// A located question can predate the entire runtime tail. Only an
	// actual background read is loading; missing/unsupported are terminal.
	old := notificationsPaneDashboard()
	outside := "outside-tail"
	old.SetQuestionMessage("q1", &outside, true)
	wanted := old.WantedNotificationMessage()
	request := requireReadRequest(t, wanted)
	if request.SessionID != "q1" {
		t.Fatalf("read session %q", request.SessionID)
	}
	if !reflect.DeepEqual(request.Source, NotificationReadSource(NotificationReadSourceItem{Value: "outside-tail"})) {
		t.Fatalf("read source %#v", request.Source)
	}
	old.SyncNotificationMessageRead(wanted, true)
	if text := old.notificationCardText("q1", AttentionLevelWaiting); text != "loading…" {
		t.Fatalf("card text %q", text)
	}
	old.NotificationMessageReadFinished(&request, nil)
	if text := old.notificationCardText("q1", AttentionLevelWaiting); text != "message not available" {
		t.Fatalf("card text %q", text)
	}
	old.SyncNotificationMessageRead(wanted, false)
	if text := old.notificationCardText("q1", AttentionLevelWaiting); text != "message not available" {
		t.Fatalf("card text %q", text)
	}
	old.SyncNotificationMessageRead(wanted, true)
	buried := "The buried question"
	old.NotificationMessageReadFinished(&request, &buried)
	if text := old.notificationMessage("q1", AttentionLevelWaiting); text != "The buried question" {
		t.Fatalf("message %q", text)
	}
	identity := old.notificationEntries()[0].identity()
	old.notifications.skipped = append(old.notifications.skipped, identity.skipIdentity())
	if sameReadRequest(old.WantedNotificationMessage(), wanted) {
		t.Fatal("a deferred card's read is no longer wanted")
	}
	old.SyncNotificationMessageRead(nil, false)
	if old.notifications.messageRead != nil {
		t.Fatal("no read wanted, no read kept")
	}

	// Millisecond ordering must survive age formatting's coarser clock;
	// an unknown activity must never outrank a real stored timestamp.
	recency := notificationsPaneDashboard()
	ten, thirty, zero := uint64(10), uint64(30), uint64(0)
	mutableSessionDetail(recency, "q1").lastActivityAtMs = &ten
	mutableSessionDetail(recency, "q2").lastActivityAtMs = &thirty
	if front(recency) != "q2" {
		t.Fatal("the newer question leads")
	}
	mutableSessionDetail(recency, "q1").lastActivityAtMs = nil
	if front(recency) != "q2" {
		t.Fatal("an unknown activity does not outrank a stored one")
	}
	mutableSessionDetail(recency, "q1").lastActivityAtMs = &zero
	if front(recency) != "q2" {
		t.Fatal("a zero activity does not outrank a stored one")
	}
}

func TestDraggingAnAnswerRowSelectsItsDrawnTextAndAClickStillAnswers(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	serveAttention(dashboard, []string{"session-1"}, core.SessionAttentionNeedsInput)
	request := booleanQuestion("session-1", false)
	mutableSessionDetail(dashboard, "session-1").pendingElicitations = []core.ElicitationRequest{request}
	serveFormCard(dashboard, "session-1", nil, &request)
	dashboard.notifications.size = core.PaneSizeMaximized
	dashboard.focus = FocusNotifications
	active, _ := formChat("session-1", request)
	chats := map[string]*chat.ActiveChat{"session-1": active}
	terminal := newRatatuiTestTerminal(120, 44)
	terminal.drawCombined(dashboard, chats)
	lines := terminal.drawCombined(dashboard, chats)
	if dashboard.notifications.formArea == nil {
		t.Fatalf("%#v", lines)
	}
	// The conversation pane shows the same question; use the card's copy.
	form := *dashboard.notifications.formArea
	at := point(lines[form.Min.Y:], chat.CheckboxMarker(false))
	x := at.X
	y := form.Min.Y + at.Y
	event := func(kind tea.MouseMsg, column, row int) tea.Msg {
		return mouseAtCell(kind, column, row)
	}
	selection := chat.NewSelectionState()

	// A drag from the answer row selects what is drawn there; the form gets
	// the press but never the click.
	press := event(leftButtonDown, x, y)
	forwarded := dashboard.RouteQuestionPress(&selection, true, press)
	if forwarded == nil {
		t.Fatal("the press reaches the form")
	}
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, forwarded))
	surfaces := cloneFrameSurfaces(dashboard.FrameSurfaces())
	selection.OnMouseDrag(x+6, y, surfaces)
	dashboard.SettleQuestionPress(&selection, chats)
	copied, ok := selection.OnMouseUp(x+6, y, surfaces).(chat.SelectionActionCopyRequested)
	if !ok {
		t.Fatal("a drag copies")
	}
	if _, ok := copied.Surface.(chat.SurfaceIdNotificationFormBody); !ok {
		t.Fatalf("copied surface %#v", copied.Surface)
	}
	frame := *surfaces.Surface(copied.Surface)
	drawnText := chat.ExtractRows(terminal.buffer, &frame, &copied.Range)
	row := []rune(lines[y])
	expected := string(row[min(x, len(row)):min(x+7, len(row))])
	if drawnText != strings.TrimRight(expected, " ") {
		t.Fatalf("drawn %q, expected %q", drawnText, expected)
	}
	lines = terminal.drawCombined(dashboard, chats)
	if !anyLineContains(lines, chat.CheckboxMarker(false)) {
		t.Fatalf("the drag did not choose the option: %#v", lines)
	}
	if _, ok := chats["session-1"].ElicitationPointerSurface(); ok {
		t.Fatal("the settled drag released the form")
	}

	// A release in another cell with no drag event in between is still a
	// selection, not a click.
	selection = chat.NewSelectionState()
	forwarded = dashboard.RouteQuestionPress(&selection, true, press)
	if forwarded == nil {
		t.Fatal("the press reaches the form")
	}
	requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, forwarded))
	movedRelease := event(leftButtonUp, x+6, y)
	if dashboard.RouteQuestionPress(&selection, true, movedRelease) != nil {
		t.Fatal("a moved release is not forwarded as a click")
	}
	if _, ok := selection.OnMouseUp(x+6, y, surfaces).(chat.SelectionActionCopyRequested); !ok {
		t.Fatal("a moved release copies")
	}
	dashboard.SettleQuestionPress(&selection, chats)
	if _, ok := chats["session-1"].ElicitationPointerSurface(); ok {
		t.Fatal("the settled selection released the form")
	}

	// The question's own text can start a selection too.
	textAt := point(lines[form.Min.Y:], "Choose a path")
	textY := form.Min.Y + textAt.Y
	switch hit := surfaces.SurfaceAt(textAt.X, textY); {
	case hit == nil:
		t.Fatalf("%#v", lines)
	default:
		_, notification := hit.ID.(chat.SurfaceIdNotificationText)
		_, message := hit.ID.(chat.SurfaceIdElicitationMessage)
		if !notification && !message {
			t.Fatalf("%#v", lines)
		}
	}
	selection = chat.NewSelectionState()
	if dashboard.RouteQuestionPress(&selection, true, event(leftButtonDown, textAt.X, textY)) == nil {
		t.Fatal("a press on the question's text is routed")
	}
	idle := chat.NewSelectionState()
	dashboard.SettleQuestionPress(&idle, chats)

	// A press and release in place is still a click.
	selection = chat.NewSelectionState()
	release := event(leftButtonUp, x, y)
	for _, pointer := range []tea.Msg{press, release} {
		forwarded := dashboard.RouteQuestionPress(&selection, true, pointer)
		if forwarded == nil {
			t.Fatal("a click reaches the form")
		}
		requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, forwarded))
	}
	lines = terminal.drawCombined(dashboard, chats)
	if !anyLineContains(lines, chat.CheckboxMarker(true)) {
		t.Fatalf("the click chose the option: %#v", lines)
	}
}

func TestDraggingNotificationTextUsesTranscriptSelectionAndCardIdentity(t *testing.T) {
	dashboard := notificationsPaneDashboard()
	dashboard.SetPaneSize(SupportPaneNotifications, core.PaneSizeMaximized)
	source := strings.Repeat("a word boundary ", 12) + "\nSecond hard line 世界"
	message := source
	mutableSessionDetail(dashboard, "q1").lastAgentMessage = &message
	terminal := newRatatuiTestTerminal(100, 50)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	surfaces := cloneFrameSurfaces(dashboard.FrameSurfaces())
	var body *chat.SurfaceFrame
	for y := 0; y < 50 && body == nil; y++ {
		for x := range 100 {
			if surface := surfaces.SurfaceAt(x, y); surface != nil {
				if _, ok := surface.ID.(chat.SurfaceIdNotificationText); ok {
					copied := *surface
					body = &copied
					break
				}
			}
		}
	}
	if body == nil {
		t.Fatal("the notification body is a selection surface")
	}
	selection := chat.NewSelectionState()
	selection.OnMouseDown(body.Rect.Min.X, body.Rect.Min.Y, surfaces)
	selection.OnMouseDrag(body.Rect.Max.X+5, body.Rect.Max.Y+2, surfaces)
	copied, ok := selection.OnMouseUp(body.Rect.Max.X+5, body.Rect.Max.Y+2, surfaces).(chat.SelectionActionCopyRequested)
	generation, isText := copied.Surface.(chat.SurfaceIdNotificationText)
	if !ok || !isText {
		t.Fatal("drag copies the notification body")
	}
	var expected []string
	for _, line := range strings.Split(source, "\n") {
		expected = append(expected, strings.TrimRightFunc(line, unicode.IsSpace))
	}
	if text, ok := dashboard.NotificationSelectionText(uint64(generation), &copied.Range); !ok || text != strings.Join(expected, "\n") {
		t.Fatalf("selection text %q", text)
	}
	terminal.draw(func(frame *chat.Frame) {
		renderDashboardForTest(frame, dashboard)
		chat.Highlight(frame.Buffer(), body, &copied.Range)
	})
	if cell := terminal.buffer.CellAt(body.Rect.Min.X, body.Rect.Min.Y); cell == nil || !chat.CellStyle(cell).AddModifier.Contains(chat.ModifierReversed) {
		t.Fatal("the selection is highlighted")
	}
	if surface := surfaces.SurfaceAt(body.Rect.Min.X, body.Rect.Max.Y+1); surface != nil {
		if _, ok := surface.ID.(chat.SurfaceIdNotificationText); ok {
			t.Fatal("reply/buttons are not a selection surface")
		}
	}
	// A new dismissal target for the same session invalidates the row space.
	dashboard.notificationDismissals.Mut().Insert("q1", core.NotificationDismissalReadThrough{Value: 999})
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	selection.ReconcileSurfaces(dashboard.FrameSurfaces())
	if selection.ActiveSurface() != nil {
		t.Fatal("the old row space is gone")
	}

	// Hard-won: 45223194f: loading text and successive form pages shared a selection generation.
	dashboard = dashboardWithSession(runningSession())
	serveAttention(dashboard, []string{"session-1"}, core.SessionAttentionNeedsInput)
	request := question("session-1")
	next := request.Fields[0]
	next.ID = "second"
	next.Title = "Second question"
	request.Fields = append(request.Fields, next)
	mutableSessionDetail(dashboard, "session-1").pendingElicitations = []core.ElicitationRequest{request}
	serveFormCard(dashboard, "session-1", nil, &request)
	dashboard.notifications.size = core.PaneSizeMaximized
	dashboard.focus = FocusNotifications
	chats := map[string]*chat.ActiveChat{}
	var previousGeneration *uint64
	for stage := range 5 {
		terminal.drawCombined(dashboard, chats)
		// Hard-won: 9739e9996: copy after redraw applied a stale range to the new form.
		if previousGeneration != nil && dashboard.NotificationSelectionIsCurrent(*previousGeneration) {
			t.Fatalf("stage %d: generation %d is still current", stage, *previousGeneration)
		}
		generation := dashboard.notifications.textGeneration
		previousGeneration = &generation
		selection.ReconcileSurfaces(dashboard.FrameSurfaces())
		if selection.ActiveSurface() != nil {
			t.Fatalf("stage %d: new content invalidates the old row space", stage)
		}
		if stage == 4 {
			break
		}
		surface := dashboard.FrameSurfaces().Surface(chat.SurfaceIdNotificationText(dashboard.notifications.textGeneration))
		if surface == nil {
			t.Fatalf("stage %d: the notification text is a surface", stage)
		}
		rect := surface.Rect
		selection.OnMouseDown(rect.Min.X, rect.Min.Y, dashboard.FrameSurfaces())
		selection.OnMouseDrag(rect.Min.X+5, rect.Min.Y, dashboard.FrameSurfaces())
		switch {
		case stage == 0:
			active, _ := formChat("session-1", request)
			chats["session-1"] = active
		case stage >= 2:
			// Hard-won: 57e492d: height-only compact rewrap kept the old selection generation.
			height := 44
			size := core.PaneSizeMaximized
			if stage == 2 {
				height = 20
				size = core.PaneSizeStandard
			}
			terminal = newRatatuiTestTerminal(120, height)
			dashboard.notifications.size = size
		default:
			requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, tea.PasteMsg{Content: "first"}))
			requireFormEvent(t)(dashboard.HandleNotificationFormEvent(chats, key(tea.KeyEnter)))
		}
	}
}

func TestNotificationsKeysReplyOpenSkipAndKeepSizesExclusive(t *testing.T) {
	replies := notificationsPaneDashboard()
	typeText := func(dashboard *DashboardState, text string) {
		for _, character := range text {
			dashboard.HandleKey(key(character))
		}
	}
	ctrl := func(character rune) tea.KeyPressMsg {
		return tea.KeyPressMsg{Code: character, Mod: tea.ModCtrl}
	}
	draft := func(dashboard *DashboardState, sessionID string) string {
		return dashboard.quickReplyMut(sessionID).editor.Draft()
	}
	promptText := func(dashboard *DashboardState, sessionID string) string {
		return dashboard.quickReplyMut(sessionID).editor.SingleLinePromptText()
	}
	// Ctrl-C before the first background pump must already know its
	// destination project, and later project changes cannot retarget it.
	abandonedProject := replies.state.Get().Sessions.GetShared("q1").BundleID
	typeText(replies, "abandoned before first pump")
	replies.HandleKey(ctrl('c'))
	var questionLines []string
	for i := range 80 {
		questionLines = append(questionLines, fmt.Sprintf("question line %d", i))
	}
	longQuestion := strings.Join(questionLines, "\n")
	mutableSessionDetail(replies, "q1").lastAgentMessage = &longQuestion
	typeText(replies, "draft /? d")
	if got := draft(replies, "q1"); got != "draft /? d" {
		t.Fatalf("draft %q", got)
	}
	replies.HandleKey(ctrl('j'))
	if got := draft(replies, "q1"); got != "draft /? d" {
		t.Fatalf("draft %q", got)
	}

	// Use the real Prompt's kill/yank editing, rather than pane shortcuts.
	for _, character := range []rune{'a', 'k', 'y'} {
		replies.HandleKey(ctrl(character))
	}
	if got := draft(replies, "q1"); got != "draft /? d" {
		t.Fatalf("draft %q", got)
	}
	replies.HandleKey(altKey('s'))
	typeText(replies, "other draft")
	replies.HandleKey(altKey('s'))
	replies.HandleKey(altKey('s'))
	if replies.notificationEntries()[0].sessionID != "q1" {
		t.Fatal("q1 leads")
	}
	sent, ok := replies.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("Enter sends a nonempty quick reply")
	}
	if sent.Card.SessionID != "q1" || sent.SessionID != "q1" || sent.Prompt.Payload.Text != "draft /? d" {
		t.Fatalf("reply %#v", sent)
	}
	prompt := sent.Prompt
	if len(replies.quickReplyMut("q1").pending) == 0 {
		t.Fatal("the reply is in flight")
	}
	// Admission cannot capture focus or prevent editing another draft.
	replies.HandleKey(key(tea.KeyTab))
	if replies.focus == FocusNotifications {
		t.Fatal("Tab leaves the pane")
	}
	replies.HandleKey(tea.KeyPressMsg{Code: tea.KeyTab, Mod: tea.ModShift})
	if replies.focus != FocusNotifications {
		t.Fatal("BackTab returns")
	}
	requireDashboardAction(t, replies.HandleKey(key(tea.KeyEnter)), DashboardActionNone{}, "same draft is already in flight")
	replies.HandleKey(key('x'))
	newer, ok := replies.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("a different draft can send")
	}
	if newer.Prompt.Payload.Text != "draft /? dx" {
		t.Fatalf("newer %q", newer.Prompt.Payload.Text)
	}
	if !replies.NotificationReplyFinished("q1", newer.Prompt.CommandID, nil) {
		t.Fatal("the newer reply finished")
	}
	replies.pasteNotificationReply("draft /? d")
	drawn(replies, 140, 50)
	replies.scrollNotification(5)
	drawn(replies, 140, 50)
	if replies.notifications.scroll != 5 {
		t.Fatalf("scroll %d", replies.notifications.scroll)
	}
	if !replies.NotificationReplyUnconfirmed("q1", prompt.CommandID, "lost acknowledgement") {
		t.Fatal("the pending reply is unconfirmed")
	}
	visible := strings.Join(drawn(replies, 140, 50), "\n")
	if !strings.Contains(visible, "Could not confirm the reply") {
		t.Fatal(visible)
	}
	if len(replies.quickReplyMut("q1").pending) == 0 {
		t.Fatal("uncertain admission stays in flight")
	}
	if replies.NotificationReplyFinished("q2", prompt.CommandID, nil) {
		t.Fatal("a receipt belongs to its own session")
	}
	replies.HandleKey(altKey('s'))
	if replies.notificationEntries()[0].sessionID != "q2" {
		t.Fatal("q2 leads")
	}
	if !replies.NotificationReplyFinished("q1", prompt.CommandID, errors.New("refused")) {
		t.Fatal("the refusal reaches q1")
	}
	if got := draft(replies, "q1"); got != "draft /? d" {
		t.Fatalf("draft %q", got)
	}
	if !slices.ContainsFunc(replies.quickReplyMut("q1").recoverable, func(reply RecoverableReply) bool {
		return reply.error != nil && *reply.error == "refused" && reply.text == "draft /? d"
	}) {
		t.Fatal("the refused text is recoverable")
	}
	if got := draft(replies, "q2"); got != "other draft" {
		t.Fatalf("q2 draft %q", got)
	}
	// A later successful receipt clears only its original session's line.
	replies.HandleKey(altKey('s'))
	replies.HandleKey(altKey('s'))
	retry, ok := replies.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("retry sends")
	}
	if !replies.NotificationReplyFinished(retry.SessionID, retry.Prompt.CommandID, nil) {
		t.Fatal("the retry finished")
	}
	if got := draft(replies, "q1"); got != "" {
		t.Fatalf("q1 draft %q", got)
	}
	if got := draft(replies, "q2"); got != "other draft" {
		t.Fatalf("q2 draft %q", got)
	}

	// The detached editor uses the real destination project and shared
	// background search owner. Enter accepts a recalled prompt, then sends
	// only on a separate key press.
	fixture := client.NewReplacementSessionTestFixture("q1", 1)
	received := core.NewUnbounded[historyRequest]()
	listed := core.NewUnbounded[string]()
	persistence := core.NewUnbounded[chat.ChatDaemonRequest]()
	history := client.NewSessionControl(historyBackend{
		inner:    fixture.Stopped,
		requests: received,
		listings: listed,
	})
	bridge := func(string) chat.ChatDaemonSender { return persistence }
	replies.state.Mut().Sessions.GetMut("q1").BundleID = "other-project"
	replies.HandleKey(ctrl('r'))
	typeText(replies, "recalled")
	replies.ServiceNotificationComposers(history, bridge)
	recorded := func() chat.ChatDaemonRequestRecordAbandonedPrompt {
		t.Helper()
		request, ok, _ := persistence.TryRecv()
		abandoned, isAbandoned := request.(chat.ChatDaemonRequestRecordAbandonedPrompt)
		if !ok || !isAbandoned {
			t.Fatalf("Ctrl-C reaches the shared dispatcher: %#v", request)
		}
		return abandoned
	}
	abandoned := recorded()
	if abandoned.SessionID != "q1" || abandoned.BundleID != abandonedProject || abandoned.Text != "abandoned before first pump" {
		t.Fatalf("initial Ctrl-C reaches the shared dispatcher: %#v", abandoned)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	search, ok := received.Recv(ctx)
	if !ok {
		t.Fatal("the search reached the backend")
	}
	if search.project != "other-project" || search.query != "recalled" {
		t.Fatalf("search %#v", search)
	}
	search.reply <- core.PromptHistoryPage{Entries: []core.PromptHistoryEntry{{
		ID:        1,
		SessionID: "q1",
		Text:      "recalled\nanswer",
	}}}
	waitFor := func(what string, done func() bool) {
		t.Helper()
		deadline := time.Now().Add(5 * time.Second)
		for !done() {
			if time.Now().After(deadline) {
				t.Fatalf("timed out waiting for %s", what)
			}
			runtime.Gosched()
			time.Sleep(time.Millisecond)
		}
	}
	waitFor("the recalled prompt", func() bool {
		if promptText(replies, "q1") == "recalled\nanswer" {
			return true
		}
		replies.ServiceNotificationComposers(history, bridge)
		return false
	})
	quickReplyLine := func(lines []string) string {
		t.Helper()
		for _, line := range lines {
			if strings.Contains(line, "Quick reply:") {
				return line
			}
		}
		t.Fatalf("no quick reply in %#v", lines)
		return ""
	}
	searchLine := quickReplyLine(drawn(replies, 180, 50))
	if !strings.Contains(searchLine, "recalled⏎answer") {
		t.Fatal(searchLine)
	}
	if !strings.Contains(searchLine, "recalled") || !strings.Contains(searchLine, "1/1") {
		t.Fatal(searchLine)
	}
	requireDashboardAction(t, replies.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if got := draft(replies, "q1"); got != "recalled\nanswer" {
		t.Fatalf("draft %q", got)
	}
	if len(replies.quickReplyMut("q1").pending) != 0 {
		t.Fatal("accepting a recalled prompt sends nothing")
	}
	recalledLine := quickReplyLine(drawn(replies, 180, 50))
	if !strings.Contains(recalledLine, "recalled⏎answer") {
		t.Fatal(recalledLine)
	}
	recalled, ok := replies.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("send recalled text")
	}
	if recalled.Prompt.Payload.Text != "recalled\nanswer" {
		t.Fatalf("recalled %q", recalled.Prompt.Payload.Text)
	}
	if !replies.NotificationReplyFinished("q1", recalled.Prompt.CommandID, nil) {
		t.Fatal("the recalled reply finished")
	}
	replies.HandleKey(key(tea.KeyUp))
	if got := promptText(replies, "q1"); got != "recalled\nanswer" {
		t.Fatalf("successful reply is immediately in local history: %q", got)
	}
	replies.quickReplyMut("q1").editor.ClearDraft()
	replies.ServiceNotificationComposers(history, bridge)
	// A stored listing is independently dispatched for the destination.
	var observed []string
	for {
		id, ok, _ := listed.TryRecv()
		if !ok {
			break
		}
		observed = append(observed, id)
	}
	if !slices.Contains(observed, "q1") {
		t.Fatalf("listings %#v", observed)
	}
	waitFor("q1's composer updates", func() bool { return replies.quickReplyMut("q1").editor.HasComposerUpdates() })
	replies.quickReplyMut("q2").editor.ClearDraft()
	replies.HandleKey(altKey('s'))
	if displayed, _ := replies.DisplayedNotificationSessionID(); displayed != "q2" {
		t.Fatalf("displayed %q", displayed)
	}
	waitFor("q2's stored prompt", func() bool {
		replies.ServiceNotificationComposers(history, bridge)
		replies.HandleKey(key(tea.KeyUp))
		return promptText(replies, "q2") == "stored\nprompt 世界"
	})
	replies.quickReplyMut("q2").editor.ClearDraft()
	replies.HandleKey(ctrl('p'))
	if got := promptText(replies, "q2"); got != "stored\nprompt 世界" {
		t.Fatalf("q2 prompt %q", got)
	}

	replies.quickReplyMut("q2").editor.ClearDraft()
	typeText(replies, "abandoned Quick reply")
	replies.HandleKey(ctrl('c'))
	replies.ServiceNotificationComposers(history, bridge)
	abandoned = recorded()
	if abandoned.SessionID != "q2" ||
		abandoned.BundleID != replies.state.Get().Sessions.GetShared("q2").BundleID ||
		abandoned.Text != "abandoned Quick reply" {
		t.Fatalf("Ctrl-C reaches the common daemon dispatcher: %#v", abandoned)
	}
	replies.HandleKey(key(tea.KeyUp))
	if got := promptText(replies, "q2"); got != abandoned.Text {
		t.Fatalf("q2 prompt %q", got)
	}

	// Refusal A must survive edits to B and a later unrelated success.
	recovery := notificationsPaneDashboard()
	typeText(recovery, "refused A")
	first, ok := recovery.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("send A")
	}
	recovery.quickReplyMut("q1").editor.ClearDraft()
	typeText(recovery, "draft B")
	if !recovery.NotificationReplyFinished("q1", first.Prompt.CommandID, errors.New("denied A")) {
		t.Fatal("A's refusal arrives")
	}
	if got := promptText(recovery, "q1"); got != "draft B" {
		t.Fatalf("prompt %q", got)
	}
	refusal := strings.Join(drawn(recovery, 180, 50), "\n")
	if !strings.Contains(refusal, "Not sent: refused A") || !strings.Contains(refusal, "denied A") {
		t.Fatal(refusal)
	}
	// Send and Restore have distinct hit areas. A refused A cannot retarget
	// a Send gesture over the currently displayed B.
	lines := drawn(recovery, 180, 50)
	send := point(lines, " Send ")
	restore := point(lines, "Restore unsent")
	if send == restore {
		t.Fatal("Send and Restore are distinct")
	}
	recovery.HandleMouse(mouseAt(leftButtonDown, send))
	current, ok := recovery.HandleMouse(mouseAt(leftButtonUp, send)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("Send submits B, not restoration of A")
	}
	if current.Prompt.Payload.Text != "draft B" {
		t.Fatalf("current %q", current.Prompt.Payload.Text)
	}
	recovery.NotificationReplyFinished("q1", current.Prompt.CommandID, errors.New("B refused too"))
	requireDashboardAction(t, recovery.HandleKey(altKey(tea.KeyEnter)), DashboardActionNone{}, "restore never sends")
	if got := promptText(recovery, "q1"); got != "refused A" {
		t.Fatalf("prompt %q", got)
	}
	if !slices.ContainsFunc(recovery.quickReplyMut("q1").recoverable, func(reply RecoverableReply) bool { return reply.text == "draft B" }) {
		t.Fatal("B is recoverable")
	}
	recovery.HandleKey(key('!'))
	second, ok := recovery.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("send edited A")
	}
	recovery.NotificationReplyFinished("q1", second.Prompt.CommandID, errors.New("denied edited A"))
	failed := 0
	for _, reply := range recovery.quickReplyMut("q1").recoverable {
		if reply.error != nil {
			failed++
		}
	}
	if failed != 3 {
		t.Fatalf("%d failed replies", failed)
	}
	recovery.HandleKey(altKey(tea.KeyEnter))
	if got := promptText(recovery, "q1"); got != "draft B" {
		t.Fatalf("prompt %q", got)
	}
	recovery.HandleKey(altKey(tea.KeyEnter))
	retryA, ok := recovery.HandleKey(key(tea.KeyEnter)).(DashboardActionNotificationReply)
	if !ok {
		t.Fatal("retry restored A")
	}
	if retryA.Prompt.Payload.Text != "refused A" {
		t.Fatalf("retry %q", retryA.Prompt.Payload.Text)
	}
	recovery.NotificationReplyFinished("q1", retryA.Prompt.CommandID, nil)
	if !slices.ContainsFunc(recovery.quickReplyMut("q1").recoverable, func(reply RecoverableReply) bool {
		return reply.error != nil && *reply.error == "denied edited A"
	}) {
		t.Fatal("edited A stays recoverable")
	}
	recovery.HandleKey(altKey(tea.KeyEnter))
	if got := promptText(recovery, "q1"); got != "refused A!" {
		t.Fatalf("prompt %q", got)
	}
	recovery.HandleKey(altKey(tea.KeyEnter))
	if got := promptText(recovery, "q1"); got != "draft B" {
		t.Fatalf("restoration also retains displaced unsent drafts: %q", got)
	}

	dashboard := notificationsPaneDashboard()
	dashboard.notificationDismissals.Mut().Remove("q1")
	// Hard-won: 56747ef: an older daemon's plain-text question entered the form loader.
	if _, ok := dashboard.NotificationFormSession(); ok {
		t.Fatal("a plain-text question has no form")
	}
	lines = drawn(dashboard, 120, 44)
	if !anyLineContains(lines, "Quick reply:") {
		t.Fatal("the quick reply is shown")
	}
	if anyLineContains(lines, "Waiting for the session's form update") {
		t.Fatal("no form is awaited")
	}
	requireDashboardAction(t, dashboard.HandleKey(altKey('n')), DashboardActionNone{})
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionOpen{SessionID: "q1"})
	if dashboard.focus != FocusPrompt {
		t.Fatalf("focus %v", dashboard.focus)
	}
	if len(dashboard.notificationEntries()) != 3 {
		t.Fatal("opening keeps every card")
	}
	dashboard.focus = FocusQuota
	dashboard.HandleKey(key(tea.KeyTab))
	if dashboard.focus != FocusNotifications {
		t.Fatalf("focus %v", dashboard.focus)
	}
	dashboard.SetPaneSize(SupportPaneTargets, core.PaneSizeMaximized)
	dashboard.SetPaneSize(SupportPaneNotifications, core.PaneSizeMaximized)
	if err := dashboard.PaneSizes().Validate(); err != nil {
		t.Fatal(err)
	}
	if size := dashboard.PaneSize(SupportPaneTargets); size != core.PaneSizeStandard {
		t.Fatalf("targets %v", size)
	}
	dashboard.SetPaneSize(SupportPaneQuota, core.PaneSizeMaximized)
	if size := dashboard.PaneSize(SupportPaneNotifications); size != core.PaneSizeStandard {
		t.Fatalf("notifications %v", size)
	}
	dashboard.SetPaneSize(SupportPaneNotifications, core.PaneSizeMaximized)
	restored := core.NewPaneSizes()
	restored.Targets = core.PaneSizeMaximized
	if err := dashboard.RestorePaneSizes(restored); err != nil {
		t.Fatal(err)
	}
	if size := dashboard.PaneSize(SupportPaneNotifications); size != core.PaneSizeStandard {
		t.Fatalf("notifications %v", size)
	}

	// The visible foreign card participates in the existing summary
	// selection; Later transfers that demand to the next card.
	foreign := notificationsPaneDashboard()
	sessions := &foreign.state.Mut().Sessions
	for _, id := range slices.Collect(sessions.Keys()) {
		sessions.GetMut(id).WorkspaceID = "foreign"
	}
	delete(*foreign.sessionDetails.Mut(), "q1")
	delete(*foreign.sessionDetails.Mut(), "q2")
	if _, ok := foreign.WantedSummarySessions()["q1"]; !ok {
		t.Fatal("the displayed card's session is wanted")
	}
	if _, ok := foreign.WantedSummarySessions()["q2"]; ok {
		t.Fatal("a hidden card's session is not wanted")
	}
	if text := foreign.notificationCardText("q1", AttentionLevelWaiting); text != "Waiting for your input" {
		t.Fatalf("card text %q", text)
	}
	foreign.SyncTranscriptHydration(map[string]struct{}{"q1": {}})
	if text := foreign.notificationCardText("q1", AttentionLevelWaiting); text != "loading…" {
		t.Fatalf("card text %q", text)
	}
	fifty := int64(50)
	otherWorkspace := "Question from the other workspace"
	foreign.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(
		core.MaterializedSessionSummary{
			SessionID:                         "q1",
			AppliedEventOrdinal:               4,
			LastActivityAtMs:                  &fifty,
			Execution:                         core.MaterializedExecutionStateIdle{},
			LastAgentMessage:                  &otherWorkspace,
			LastAgentMessageFollowsLastUser:   true,
			AgentMessageLatestContentOrdinals: []uint64{4},
			InterruptionEventOrdinals:         []uint64{},
		},
		0,
	))
	if text := foreign.notificationCardText("q1", AttentionLevelWaiting); text != "Question from the other workspace" {
		t.Fatalf("card text %q", text)
	}
	foreign.HandleKey(altKey('s'))
	if _, ok := foreign.WantedSummarySessions()["q2"]; !ok {
		t.Fatal("the next card's session is wanted")
	}
	if _, ok := foreign.WantedSummarySessions()["q1"]; ok {
		t.Fatal("the deferred card's session is no longer wanted")
	}

	// A promoted parent card retains its daemon target but displays and
	// opens the child's approval through the existing family navigation.
	family, parent := dashboardWithOneSubagent()
	serveAttention(family, []string{parent, "child-session"}, core.SessionAttentionNeedsInput)
	holders := core.NewSnapshotMap[string, string]()
	holders.Insert(parent, "child-session")
	family.SetFormHolders(holders)
	unrelated := "Unrelated parent output"
	mutableSessionDetail(family, parent).lastAgentMessage = &unrelated
	if displayed, _ := family.DisplayedNotificationSessionID(); displayed != "child-session" {
		t.Fatalf("displayed %q", displayed)
	}
	if displayedParent, _ := family.DisplayedNotificationParentID(); displayedParent != parent {
		t.Fatalf("parent %q", displayedParent)
	}
	if _, ok := family.WantedSummarySessions()["child-session"]; !ok {
		t.Fatal("the child's summary is wanted")
	}
	if text := family.notificationCardText(parent, AttentionLevelWaiting); text != "Waiting for your input" {
		t.Fatalf("card text %q", text)
	}
	family.SyncTranscriptHydration(map[string]struct{}{"child-session": {}})
	if text := family.notificationCardText(parent, AttentionLevelWaiting); text != "loading…" {
		t.Fatalf("card text %q", text)
	}
	// Cancellation/failure ends loading even if no message ever arrived.
	family.SyncTranscriptHydration(map[string]struct{}{})
	if text := family.notificationCardText(parent, AttentionLevelWaiting); text != "Waiting for your input" {
		t.Fatalf("card text %q", text)
	}
	zeroMs := int64(0)
	family.ApplyPreparedMaterializedSessionSummary(PreparedMaterializedSessionSummaryFromMaterialized(
		core.MaterializedSessionSummary{
			SessionID:                         "child-session",
			AppliedEventOrdinal:               1,
			LastActivityAtMs:                  &zeroMs,
			Execution:                         core.MaterializedExecutionStateIdle{},
			AgentMessageLatestContentOrdinals: []uint64{},
			InterruptionEventOrdinals:         []uint64{},
		},
		0,
	))
	if activity := (*family.sessionDetails.Get())["child-session"].lastActivityAtMs; activity != nil {
		t.Fatalf("a zero activity is unknown: %d", *activity)
	}
	if text := family.notificationCardText(parent, AttentionLevelWaiting); text != "Waiting for your input" {
		t.Fatalf("card text %q", text)
	}
	zero := uint64(0)
	mutableSessionDetail(family, parent).lastActivityAtMs = &zero
	family.state.Mut().Sessions.GetMut(parent).UpdatedAt = ""
	if !anyLineContains(drawn(family, 120, 40), "age unknown") {
		t.Fatal("an unknown age is said")
	}

	approval := question("child-session")
	approval.ToolPermission = &core.ToolPermissionForm{Skip: true}
	mutableSessionDetail(family, "child-session").pendingElicitations = []core.ElicitationRequest{approval}
	child := "child-session"
	serveFormCard(family, parent, &child, &approval)
	if text := family.notificationMessage(parent, AttentionLevelWaiting); text != approval.Message {
		t.Fatalf("message %q", text)
	}
	entries := family.notificationEntries()
	if len(entries) != 1 || entries[0].sessionID != parent {
		t.Fatalf("entries %d", len(entries))
	}
	if target, _ := family.notificationDismissalTarget(parent); !reflect.DeepEqual(entries[0].target, target) {
		t.Fatalf("target %#v", entries[0].target)
	}
	family.focus = FocusNotifications
	requireDashboardAction(t, family.HandleKey(altKey('g')), DashboardActionOpen{SessionID: "child-session"})
	if family.focus != FocusPrompt {
		t.Fatalf("focus %v", family.focus)
	}
	family.focus = FocusNotifications
	active, answers := formChat("child-session", approval)
	chats := map[string]*chat.ActiveChat{"child-session": active}
	// Reading older conversation messages must not steal the card's form
	// input.
	chats["child-session"].HandleEventResult(tea.KeyPressMsg{Code: tea.KeyPgUp, Mod: tea.ModCtrl})
	terminal := newRatatuiTestTerminal(120, 50)
	terminal.drawCombined(family, chats)
	if family.notifications.formArea == nil {
		t.Fatal("the form is drawn inline")
	}
	messageSurface := family.frameSurfaces.Surface(chat.SurfaceIdNotificationText(family.notifications.textGeneration))
	if messageSurface == nil {
		t.Fatal("the form's message is a selection surface")
	}
	if !uv.Pos(messageSurface.Rect.Min.X, messageSurface.Rect.Min.Y).In(*family.notifications.formArea) {
		t.Fatal("the message sits inside the form")
	}
	selected := chat.SelectionRange{
		Start: chat.NewContentPos(0, 0),
		End:   chat.NewContentPos(max(messageSurface.TotalRows-1, 0), max(messageSurface.Rect.Dx()-1, 0)),
	}
	if text, ok := chats["child-session"].ElicitationSelectionTextIn(chat.ElicitationSurfaceNotification, &selected); !ok || !strings.Contains(text, "path") {
		t.Fatalf("selection text %q", text)
	}

	area := *family.notificationsArea
	var open DashboardAction = DashboardActionNone{}
	for _, kind := range []tea.MouseMsg{leftButtonDown, leftButtonUp} {
		mouse := mouseAtCell(kind, area.Min.X+2, area.Max.Y-2)
		if _, ok := family.HandleNotificationFormEvent(chats, mouse); ok {
			t.Fatal("the card's buttons belong to the dashboard")
		}
		open = family.HandleMouse(mouse)
	}
	requireDashboardAction(t, open, DashboardActionOpen{SessionID: "child-session"})
	family.focus = FocusNotifications

	for _, press := range []tea.KeyPressMsg{key(tea.KeyTab), {Code: tea.KeyTab, Mod: tea.ModShift}} {
		if !requireFormEvent(t)(family.HandleNotificationFormEvent(chats, press)).Consumed {
			t.Fatal("the form consumes Tab")
		}
		if family.focus != FocusNotifications {
			t.Fatal("Tab stays in the form")
		}
	}

	if route := family.RouteBoundKeyEvent(key(tea.KeyEnter)); !reflect.DeepEqual(route, KeyRouteForward{}) {
		t.Fatalf("Enter is forwarded: %#v", route)
	}
	if _, ok := family.HandleNotificationFormEvent(chats, altKey('g')); ok {
		t.Fatal("Go to Session belongs to the card")
	}

	if !requireFormEvent(t)(family.HandleNotificationFormEvent(chats, tea.PasteMsg{Content: "Approved"})).Consumed {
		t.Fatal("the form takes the paste")
	}
	if !requireFormEvent(t)(family.HandleNotificationFormEvent(chats, key(tea.KeyEnter))).Consumed {
		t.Fatal("the form takes Enter")
	}
	answer := recvAnswer(t, answers)
	if answer.id != approval.ID {
		t.Fatalf("answered %q", answer.id)
	}
	if !answer.response.Equal(acceptResponse("path", core.ElicitationValueString("Approved"))) {
		t.Fatalf("response %#v", answer.response)
	}
	if chats["child-session"].HasElicitation() {
		t.Fatal("the answered form is gone")
	}
	notPermitted := "not permitted"
	active, rejected := formChatWithError("child-session", approval, &notPermitted)
	chats["child-session"] = active
	terminal.drawCombined(family, chats)
	family.HandleNotificationFormEvent(chats, tea.PasteMsg{Content: "Retry me"})
	family.HandleNotificationFormEvent(chats, key(tea.KeyEnter))
	// Hard-won: f69edee: minimizing a notification-only primary answer retired its result owner.
	family.notifications.size = core.PaneSizeMinimized
	if _, ok := family.NotificationFormSession(); ok {
		t.Fatal("a minimized pane shows no form")
	}
	if chats["child-session"].HasElicitation() {
		t.Fatal("the answer is in flight")
	}
	if !chats["child-session"].HasPendingElicitationInput() {
		t.Fatal("the answer keeps its result owner")
	}
	recvAnswer(t, rejected)
	pumpCtx, pumpCancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer pumpCancel()
	for !chats["child-session"].HasElicitation() {
		if pumpCtx.Err() != nil {
			t.Fatal("the refused form never came back")
		}
		chat.ActiveChatPump(pumpCtx, chats["child-session"])
	}
	if chats["child-session"].HasPendingElicitationInput() {
		t.Fatal("the refusal settled the answer")
	}
	// Retirement after the callback saves drafts; their existing handoff
	// format must carry the rejection when the form is reopened.
	drafts, err := json.Marshal(chats["child-session"].ElicitationDrafts())
	if err != nil {
		t.Fatal(err)
	}
	restoredChat, _ := formChat("child-session", approval)
	var decoded []chat.ChatElicitationDraft
	if err := json.Unmarshal(drafts, &decoded); err != nil {
		t.Fatal(err)
	}
	restoredChat.RestoreElicitationDrafts(decoded)
	chats["child-session"] = restoredChat
	family.notifications.size = core.PaneSizeMaximized
	lines = terminal.drawCombined(family, chats)
	area = *family.notificationsArea
	if !anyLineContains(lines[area.Min.Y:area.Max.Y], "Answer was not sent: not permitted") {
		t.Fatalf("%#v", lines)
	}
}
