// Port of mj-chat/src/chat/tests.rs (its golden_cases module is
// chat_tests_golden_test.go).

package chat

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"errors"

	"image"
	"image/color"
	"image/png"
	"reflect"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/BrokkAi/acp-go/schema"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// newTestChat is `ChatState::new(&snapshot(), &[])`.
func newTestChat() *ChatState {
	worker := snapshot()
	return NewChatState(&worker, nil)
}

func requireChatAction(t *testing.T, got, want ChatAction) {
	t.Helper()
	if !rustEqual(got, want) {
		t.Fatalf("action = %#v, want %#v", got, want)
	}
}

// requireEqual is Rust's assert_eq! for values with a derived PartialEq: an
// empty slice equals a missing one, and values that serialize identically
// (references compared by value) are equal.
func requireEqual[T any](t *testing.T, got, want T) {
	t.Helper()
	if !rustEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

func rustEqual(got, want any) bool {
	if reflect.DeepEqual(got, want) {
		return true
	}
	if reflect.TypeOf(got) != reflect.TypeOf(want) {
		return false
	}
	gotValue, wantValue := reflect.ValueOf(got), reflect.ValueOf(want)
	if gotValue.Kind() == reflect.Slice && wantValue.Kind() == reflect.Slice && gotValue.Len() == 0 && wantValue.Len() == 0 {
		return true
	}
	gotJSON, gotErr := json.Marshal(got)
	wantJSON, wantErr := json.Marshal(want)
	return gotErr == nil && wantErr == nil && bytes.Equal(gotJSON, wantJSON)
}

func requireNotice(t *testing.T, chat *ChatState, want string) {
	t.Helper()
	if got, ok := chat.Notice(); !ok || got != want {
		t.Fatalf("notice = %q (%t), want %q", got, ok, want)
	}
}

func requireNoNotice(t *testing.T, chat *ChatState) {
	t.Helper()
	if got, ok := chat.Notice(); ok {
		t.Fatalf("notice = %q, want none", got)
	}
}

func requireNoticeContains(t *testing.T, chat *ChatState, part string) {
	t.Helper()
	if got, ok := chat.Notice(); !ok || !strings.Contains(got, part) {
		t.Fatalf("notice = %q (%t), want it to contain %q", got, ok, part)
	}
}

func requireCurrent(t *testing.T, notices Notices, want string) {
	t.Helper()
	if got, ok := notices.Current(); !ok || got != want {
		t.Fatalf("current notice = %q (%t), want %q", got, ok, want)
	}
}

func requireNoCurrent(t *testing.T, notices Notices) {
	t.Helper()
	if got, ok := notices.Current(); ok {
		t.Fatalf("current notice = %q, want none", got)
	}
}

func int64Ptr(value int64) *int64 { return &value }

func uint64Ptr(value uint64) *uint64 { return &value }

func stringPtr(value string) *string { return &value }

func TestActivityAnimationStopsWhenForegroundAndBackgroundWorkSettle(t *testing.T) {
	chat := newTestChat()
	if chat.NeedsAnimation() {
		t.Fatal("an idle chat animates")
	}
	chat.phase = core.WorkerPhaseRunning
	if chat.NeedsAnimation() {
		t.Fatal("a running phase alone animates")
	}
	chat.turnStartedAtEpochSeconds = uint64Ptr(1)
	if !chat.NeedsAnimation() {
		t.Fatal("a started turn does not animate")
	}
	chat.activityReachable = false
	if chat.NeedsAnimation() {
		t.Fatal("an unreachable activity animates")
	}
	chat.activityReachable = true
	chat.turnStartedAtEpochSeconds = nil
	chat.phase = core.WorkerPhaseIdle
	chat.sessionActivity.ForegroundToolStartedAtMs = int64Ptr(1)
	if !chat.NeedsAnimation() {
		t.Fatal("foreground tool work does not animate")
	}
	chat.sessionActivity = client.SessionActivity{}
	if chat.NeedsAnimation() {
		t.Fatal("settled activity animates")
	}
	chat.phase = core.WorkerPhaseClosing
	if !chat.NeedsAnimation() {
		t.Fatal("closing does not animate")
	}
	// A lifecycle snapshot can still carry the old primary activity when the
	// terminal has already delivered Closed. The settled phase wins.
	chat.phase = core.WorkerPhaseClosed
	running := core.RelayExecutionStateRunning
	chat.sessionActivity.Execution = &running
	chat.sessionActivity.ForegroundToolStartedAtMs = int64Ptr(1)
	if chat.NeedsAnimation() {
		t.Fatal("a closed chat animates")
	}
}

func TestIdleBackgroundWorkAndWorkingReviewKeepAnimationIndependent(t *testing.T) {
	chat := newTestChat()
	chat.sessionActivity.BackgroundCommands = append(chat.sessionActivity.BackgroundCommands, core.BackgroundCommand{
		ID:          "test:background",
		StartedAtMs: 1,
		Command:     "cargo test",
		CanStop:     false,
	})
	if !chat.NeedsAnimation() {
		t.Fatal("background work does not animate")
	}

	chat.phase = core.WorkerPhaseClosed
	if chat.NeedsAnimation() {
		t.Fatal("a closed chat animates")
	}

	chat.setTurnReviews([]client.RuntimeReviewView{{
		ReviewID:       "",
		Roots:          nil,
		ChildSessionID: nil,
		SessionID:      "session-1",
		Questions:      nil,
		Phase:          core.TurnReviewPhaseCapturingDelta{},
		Roles:          nil,
		Status:         "capturing the turn",
		Verdict:        nil,
	}})
	if !chat.NeedsAnimation() {
		t.Fatal("a working review does not animate")
	}
}

func TestReviewStatusOmitsTheDeprecatedTier(t *testing.T) {
	review := core.ReviewConfig{
		Legacy:      nil,
		Disabled:    false,
		BriefWriter: nil,
		Reviewers: []core.SubagentChoice{
			{Model: "gpt-6-terra", Effort: nil},
			{Model: "gpt-6-terra", Effort: nil},
		},
	}
	if got, want := ReviewStatusLine(&review, true), "Review trigger is before push; 2 reviewers. A review is open now."; got != want {
		t.Fatalf("status = %q, want %q", got, want)
	}
}

// freshlyOpenedChat mirrors what `ActiveChat::open` does for a session with
// no warm view: build the state from the snapshot, then seed the saved
// draft.
func freshlyOpenedChat(savedDraft string) *ChatState {
	session := core.MaterializedSessionEmpty("session-fresh")
	chat := ChatStateFromMaterialized(&session, nil, nil)
	chat.SetHistoryContext("bundle-1")
	chat.restoreDraft(savedDraft)
	return chat
}

func testImage() ClipboardImage {
	picture := image.NewRGBA(image.Rect(0, 0, 2, 2))
	pixels := []color.RGBA{
		{255, 0, 0, 255}, {0, 255, 0, 255},
		{0, 0, 255, 255}, {255, 255, 0, 255},
	}
	for index, pixel := range pixels {
		picture.SetRGBA(index%2, index/2, pixel)
	}
	var encoded bytes.Buffer
	if err := png.Encode(&encoded, picture); err != nil {
		panic(err)
	}
	result, err := ClipboardImageFromPNGBase64(base64.StdEncoding.EncodeToString(encoded.Bytes()))
	if err != nil {
		panic(err)
	}
	return result
}

func testReferenceImage() ClipboardImage {
	picture := testImage()
	data, err := base64.StdEncoding.DecodeString(picture.DataBase64)
	if err != nil {
		panic(err)
	}
	reference, err := core.NewAttachmentRef(data, picture.MimeType, 2, 2)
	if err != nil {
		panic(err)
	}
	result, err := ClipboardImageFromReference(reference)
	if err != nil {
		panic(err)
	}
	return result
}

func backgroundTask(id, command string, canStop bool) core.BackgroundCommand {
	return core.BackgroundCommand{
		ID:          id,
		StartedAtMs: core.EpochMillis() - 1_000,
		Command:     command,
		CanStop:     canStop,
	}
}

// TestADraftSavedWithUnsentRecordsStillRestoresTheComposer: older builds
// saved refused and unanswered prompts with the draft. Those fields are no
// longer read, and the composer still comes back.
func TestADraftSavedWithUnsentRecordsStillRestoresTheComposer(t *testing.T) {
	saved, err := json.Marshal(map[string]any{
		"text":  "next step",
		"image": nil,
		"unsent": []any{map[string]any{
			"kind": "Unanswered", "text": "inspect ", "image": testImage(),
			"error": "offline", "recorded_at_ms": 42,
		}},
		"unanswered": map[string]any{"turn": "prompt-1", "told": true},
	})
	if err != nil {
		t.Fatal(err)
	}
	chat := newTestChat()
	chat.restoreDraft(ChatDraftPrefix + string(saved))
	requireEqual(t, chat.draftPayload(), PromptPayloadText("next step"))
	requireNoNotice(t, chat)
}

func TestImageDraftRoundTripsAndPlainTextDraftsStayCompatible(t *testing.T) {
	payload := PromptPayloadWithImage("describe this", testImage())
	encoded := payload.EncodeDraft()
	if !strings.HasPrefix(encoded, ChatDraftPrefix) {
		t.Fatalf("encoded draft %q lacks the prefix", encoded)
	}
	decoded, err := PromptPayloadDecodeDraft(encoded)
	if err != nil {
		t.Fatal(err)
	}
	requireEqual(t, decoded, payload)
	plain, err := PromptPayloadDecodeDraft("plain draft")
	if err != nil {
		t.Fatal(err)
	}
	requireEqual(t, plain, PromptPayloadText("plain draft"))

	chat := newTestChat()
	chat.restoreDraft(encoded)
	requireEqual(t, chat.draftPayload(), payload)
}

func TestFailedImageSubmissionPreservesNewerImagesAndSurvivesReopening(t *testing.T) {
	original := PromptPayloadWithImage("inspect old image ", testImage())
	newer := testImage()
	newer.MimeType = "image/jpeg"
	chat := newTestChat()
	chat.setPromptImagesSupported(true)
	chat.handleClipboardContent(ClipboardContentImage{Image: newer})
	applyChatRemoteResult(chat, chatRemoteResultPrompt{
		CommandID: "test-submit",
		Text:      original.Text,
		Images:    original.Images,
		Err:       errors.New("offline"),
	})
	if want := "inspect old image [image 2]\n\n[image 1]"; chat.input != want {
		t.Fatalf("input = %q, want %q", chat.input, want)
	}
	requireEqual(t, chat.inputImages[0].Image, original.Images[0].Image)
	requireEqual(t, chat.inputImages[1].Image, newer)
	saved := chat.draftPayload()
	reopened := newTestChat()
	reopened.setPromptImagesSupported(true)
	reopened.restoreDraft(chat.encodedDraft())
	requireEqual(t, reopened.draftPayload(), saved)
}

func TestPendingAttachmentIsFailedWhenASavedDraftIsRestored(t *testing.T) {
	payload := PromptPayloadWithImage("inspect ", clipboardImagePending())
	chat := newTestChat()
	chat.restoreDraft(payload.EncodeDraft())

	if chat.input != "inspect [image 1]" {
		t.Fatalf("input = %q", chat.input)
	}
	if !chat.inputImages[0].Image.isPlaceholder() {
		t.Fatal("the restored attachment is not a placeholder")
	}
	if chat.inputImages[0].Image.isPending() {
		t.Fatal("the restored attachment is still pending")
	}
	requireNoticeContains(t, chat, "Re-paste the failed attachment")
}

func TestRemovedPendingAttachmentReleasesVisibleCapacity(t *testing.T) {
	chat := newTestChat()
	chat.setPromptImagesSupported(true)
	for sequence := range uint64(core.MaxImages) {
		if !chat.reserveAttachment(sequence) {
			t.Fatalf("attachment %d was not reserved", sequence)
		}
	}
	if chat.reserveAttachment(core.MaxImages) {
		t.Fatal("an attachment past the limit was reserved")
	}

	chat.HandleKey(key(tea.KeyBackspace))
	if !chat.reserveAttachment(core.MaxImages + 1) {
		t.Fatal("removing an attachment did not release its capacity")
	}
	if len(chat.inputImages) != core.MaxImages {
		t.Fatalf("images = %d, want %d", len(chat.inputImages), core.MaxImages)
	}
}

func TestALiteralDraftEnvelopePrefixRoundTripsAsText(t *testing.T) {
	payload := PromptPayloadText(ChatDraftPrefix + `{"text":"literal"}`)
	decoded, err := PromptPayloadDecodeDraft(payload.EncodeDraft())
	if err != nil {
		t.Fatal(err)
	}
	requireEqual(t, decoded, payload)
}

func TestActiveVoiceRemainsStoppableAfterAvailabilityIsLost(t *testing.T) {
	chat := newTestChat()
	chat.voiceActive = true
	area := uv.Rect(10, 8, 4, 1)
	chat.voiceButtonArea = &area

	requireChatAction(t, chat.DictationToggleAction(), ChatActionToggleVoice{})
	requireChatAction(t, chat.HandleMouse(formMouseDown(11, 8)), ChatActionToggleVoice{})
}

func TestClockSamplingTracksDisplayedUnitsAndKeepsTheDrawnBaseline(t *testing.T) {
	chat := newTestChat()
	chat.headerTarget = ""
	chat.headerProfile = ""
	if chat.clockText(100) != chat.clockText(101) {
		t.Fatal("an idle clock changes every second")
	}
	chat.setSessionActivity(client.SessionActivity{
		PursuingGoal: false,
		BackgroundCommands: []core.BackgroundCommand{{
			ID:          "test:clock",
			StartedAtMs: 0,
			Command:     "cargo test",
			CanStop:     false,
		}},
	})
	// The task count is static until its elapsed-time dialog is opened.
	if chat.clockText(100) != chat.clockText(101) {
		t.Fatal("the task count changes every second")
	}
	chat.openTaskDialog()
	if chat.clockText(100) == chat.clockText(101) {
		t.Fatal("the task dialog clock does not tick")
	}
	if chat.clockText(3_600) != chat.clockText(3_601) {
		t.Fatal("an hour-scale clock changes every second")
	}
	chat.lastClockText = stringPtr("previous frame")
	if !chat.ClockChanged() {
		t.Fatal("a changed clock is not reported")
	}
	// Sampling must not acknowledge an undrawn frame.
	if !chat.ClockChanged() {
		t.Fatal("sampling acknowledged an undrawn frame")
	}
	if chat.lastClockText == nil || *chat.lastClockText != "previous frame" {
		t.Fatalf("drawn baseline = %v", chat.lastClockText)
	}
}

func TestSubagentControlAppearsOnlyAfterTheFirstChild(t *testing.T) {
	chat := newTestChat()
	screen := strings.Join(drawnTranscript(chat, 100, 24), "\n")
	if strings.Contains(screen, "Subagents") {
		t.Fatalf("subagent control drawn without children:\n%s", screen)
	}
	if chat.subagentControlArea != nil {
		t.Fatal("subagent control registered without children")
	}

	chat.SetSubagentCount(1)
	screen = strings.Join(drawnTranscript(chat, 100, 24), "\n")
	if !strings.Contains(screen, "Subagents · 0 ›") {
		t.Fatalf("subagent control missing:\n%s", screen)
	}
	if chat.subagentControlArea == nil {
		t.Fatal("subagent control not registered")
	}
	// Launch finding R1-3: the ASCII symbol set draws no middle dot.
	var ascii string
	WithSymbols(core.SymbolSetAscii, func() {
		ascii = strings.Join(drawnTranscript(chat, 100, 24), "\n")
	})
	if !strings.Contains(ascii, "Subagents - 0 ") {
		t.Fatalf("ASCII subagent control missing:\n%s", ascii)
	}
	if strings.Contains(ascii, "·") {
		t.Fatalf("ASCII control draws a middle dot:\n%s", ascii)
	}
}

func screenContains(lines []string, part string) bool {
	for _, line := range lines {
		if strings.Contains(line, part) {
			return true
		}
	}
	return false
}

func TestStoppableBackgroundTaskKeyboardActivationIsDeduplicated(t *testing.T) {
	chat := newTestChat()
	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{backgroundTask("task-1", "cargo test", true)},
	})
	chat.openTaskDialog()
	drawnTranscript(chat, 80, 16)

	// Tab focuses the first Stop button; Enter submits it immediately.
	requireChatAction(t, chat.HandleKey(key(tea.KeyTab)), ChatActionNone{})
	requireChatAction(t, chat.HandleKey(chord(tea.KeyTab, tea.ModShift)), ChatActionNone{})
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionStopBackgroundTask{ID: "task-1"})
	if !chat.backgroundStopPending("task-1") {
		t.Fatal("the stop is not pending")
	}
	drawnTranscript(chat, 80, 16)
	if !screenContains(drawnTranscript(chat, 80, 16), "Interrupting…") {
		t.Fatal("the pending stop is not shown")
	}

	// The disabled pending control cannot submit a second request.
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
}

func TestDisappearedBackgroundTaskClearsPendingStopAndFailureReenablesIt(t *testing.T) {
	chat := newTestChat()
	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{backgroundTask("task-1", "cargo test", true)},
	})
	requireChatAction(t, chat.requestBackgroundStop("task-1"), ChatActionStopBackgroundTask{ID: "task-1"})
	applyChatRemoteResult(chat, chatRemoteResultStopBackgroundTask{ID: "task-1", Err: nil})
	if !chat.backgroundStopPending("task-1") {
		t.Fatal("an acknowledged stop is no longer pending")
	}
	chat.setSessionActivity(client.SessionActivity{})
	if chat.backgroundStopPending("task-1") {
		t.Fatal("the stop is pending after the task disappeared")
	}
	noticeAfter, noticeAfterOK := chat.Notice()
	applyChatRemoteResult(chat, chatRemoteResultStopBackgroundTask{ID: "task-1", Err: errors.New("stale request")})
	if got, ok := chat.Notice(); got != noticeAfter || ok != noticeAfterOK {
		t.Fatalf("a stale failure changed the notice to %q (%t)", got, ok)
	}

	chat.setSessionActivity(client.SessionActivity{
		BackgroundCommands: []core.BackgroundCommand{backgroundTask("task-1", "cargo test", true)},
	})
	requireChatAction(t, chat.requestBackgroundStop("task-1"), ChatActionStopBackgroundTask{ID: "task-1"})
	applyChatRemoteResult(chat, chatRemoteResultStopBackgroundTask{ID: "task-1", Err: errors.New("provider unavailable")})
	if chat.backgroundStopPending("task-1") {
		t.Fatal("a failed stop is still pending")
	}
	requireNotice(t, chat, "Background task could not be stopped: provider unavailable")
	chat.openTaskDialog()
	drawnTranscript(chat, 80, 16)
	if !screenContains(drawnTranscript(chat, 80, 16), "[Stop]") {
		t.Fatal("the stop control is not enabled again")
	}
}

// Hard-won: 30de9e86: Typed composer text was lost when leaving and reopening a conversation.
func TestASavedDraftReopensInTheComposerWithTheCursorAtItsEnd(t *testing.T) {
	chat := newTestChat()

	chat.restoreDraft("half typed thought")

	if chat.input != "half typed thought" {
		t.Fatalf("input = %q", chat.input)
	}
	if chat.inputCursor != len("half typed thought") {
		t.Fatalf("cursor = %d", chat.inputCursor)
	}
}

func TestEnterDoesNotSendAPromptWhileTheWorkerIsClosingOrClosed(t *testing.T) {
	for _, phase := range []core.WorkerPhase{core.WorkerPhaseClosing, core.WorkerPhaseClosed} {
		chat := newTestChat()
		chat.phase = phase
		chat.input = "hello"
		requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
		requireCurrent(t, chat.feedback, "The worker is closing; this prompt was not sent")
		if chat.input != "hello" {
			t.Fatalf("input = %q", chat.input)
		}
	}
}

func TestBootstrapUsesSnapshotQueueWithoutDuplicatingReplayedAdditions(t *testing.T) {
	var worker core.WorkerSnapshot
	if err := json.Unmarshal([]byte(`{
		"session_id": "1234567890",
		"phase": "running",
		"latest_seq": 1,
		"last_checkpoint_seq": null,
		"active_prompt": null,
		"config": {},
		"queued_prompts": [{
			"id": "queued-0001",
			"text": "next",
			"attachments": [],
			"created_at_ms": 1
		}],
		"handled_requests": {}
	}`), &worker); err != nil {
		t.Fatal(err)
	}
	events := []core.SequencedEvent{{
		Seq:          1,
		RecordedAtMs: int64Ptr(1),
		RequestID:    stringPtr("enqueue-1"),
		Event: core.WorkerEventQueuedPromptAdded{Prompt: core.QueuedPrompt{
			ID:          "queued-0001",
			Kind:        core.QueuedCommandKindPrompt{},
			Text:        "next",
			Attachments: []core.Attachment{},
			CreatedAtMs: 1,
		}},
	}}

	chat := NewChatState(&worker, events)

	if len(chat.queuedPrompts) != 1 {
		t.Fatalf("queued prompts = %d, want 1", len(chat.queuedPrompts))
	}
	if chat.queuedPrompts[0].id != "queued-0001" {
		t.Fatalf("queued prompt id = %q", chat.queuedPrompts[0].id)
	}
}

func TestSubmittingAPromptClearsAStaleQueueNotice(t *testing.T) {
	chat := newTestChat()
	chat.SetNotice("Queued 1: next")

	chat.markPromptSubmitted("hello")

	if chat.phase != core.WorkerPhaseRunning {
		t.Fatalf("phase = %v", chat.phase)
	}
	requireNoNotice(t, chat)
}

func TestNoticesSetReplaceIfAndClear(t *testing.T) {
	notices := NewNotices()
	requireNoCurrent(t, notices)

	notices.Set("first notice")
	requireCurrent(t, notices, "first notice")

	if notices.ReplaceIf("wrong expectation", "replaced") {
		t.Fatal("a mismatched expectation replaced the notice")
	}
	requireCurrent(t, notices, "first notice")

	if !notices.ReplaceIf("first notice", "second notice") {
		t.Fatal("a matching expectation did not replace the notice")
	}
	requireCurrent(t, notices, "second notice")

	notices.Clear()
	requireNoCurrent(t, notices)
}

func TestAFreshFailureNoticeSurvivesRoutineBackgroundNotices(t *testing.T) {
	notices := NewNotices()
	notices.SetFailure("Resume failed: archived transcript is invalid")

	notices.Set("Profile quotas refreshed")
	requireCurrent(t, notices, "Resume failed: archived transcript is invalid")

	// A newer failure replaces the protected one at once, and the bar says
	// that one was overwritten.
	notices.SetFailure("Resume failed: target disconnected")
	requireCurrent(t, notices, "2 failures · latest: Resume failed: target disconnected")

	afterSet := time.Now()
	if !notices.Dismiss(afterSet.Add(NoticeMinimumDisplay)) {
		t.Fatal("the failure was not dismissed after its minimum display")
	}
	notices.Set("Profile quotas refreshed")
	requireCurrent(t, notices, "Profile quotas refreshed")
}

func TestClonedNoticesShareOneSlot(t *testing.T) {
	notices := NewNotices()
	clone := notices

	notices.Set("set through the original")
	requireCurrent(t, clone, "set through the original")

	clone.Clear()
	requireNoCurrent(t, notices)
}

// Dismissal is what an incidental key press asks for, and a notice that
// nobody has had time to read must survive it.
// Hard-won: 7c56a0f4: Incidental keypresses erased notices before the user could see a frame.
func TestANoticeIsDismissedOnlyOnceItHasBeenShowingLongEnough(t *testing.T) {
	notices := NewNotices()
	if !notices.Dismiss(time.Now()) {
		t.Fatal("dismissing an empty bar failed")
	}

	notices.Set("Credential sync failed")
	afterSet := time.Now()
	if notices.Dismiss(afterSet) {
		t.Fatal("a fresh notice was dismissed")
	}
	requireCurrent(t, notices, "Credential sync failed")

	if !notices.Dismiss(afterSet.Add(NoticeMinimumDisplay)) {
		t.Fatal("a shown notice was not dismissed")
	}
	requireNoCurrent(t, notices)
}

// Draws are gated on a dirty flag that background work never sets, so a
// renderer tells the bar moved by recording this counter with each frame.
func TestNoticeGenerationChangesOnlyWhenDisplayedTextChanges(t *testing.T) {
	notices := NewNotices()
	drawn := notices.Generation()

	notices.Set("Import failed")
	if notices.Generation() == drawn {
		t.Fatal("a new notice kept the generation")
	}
	drawn = notices.Generation()

	// Repeating the same report updates its age without changing the
	// visible footer.
	notices.Set("Import failed")
	if notices.Generation() != drawn {
		t.Fatal("a repeated notice changed the generation")
	}

	if !notices.ReplaceIf("Import failed", "Import failed: no space left") {
		t.Fatal("the notice was not replaced")
	}
	if notices.Generation() == drawn {
		t.Fatal("a replacement kept the generation")
	}
	drawn = notices.Generation()

	notices.Clear()
	if notices.Generation() == drawn {
		t.Fatal("clearing kept the generation")
	}

	// Clearing an empty bar changes nothing on screen.
	drawn = notices.Generation()
	notices.Clear()
	if notices.Generation() != drawn {
		t.Fatal("clearing an empty bar changed the generation")
	}
}

func textElicitation() core.ElicitationRequest {
	return core.ElicitationRequest{
		ID:          "ask-1",
		Message:     "Which branch should I use?",
		Title:       nil,
		Description: nil,
		Fields: []core.ElicitationField{{
			ID:                 "branch",
			Title:              "Branch",
			Description:        nil,
			Required:           false,
			Secret:             false,
			CustomAnswerFor:    nil,
			CustomAnswerOption: nil,
			Kind:               core.ElicitationFieldKindText{},
		}},
		ToolPermission: nil,
	}
}

func TestRestoredQuestionDraftsKeepDistinctAnswersAndRejectChangedRequests(t *testing.T) {
	request := textElicitation()
	answers := []string{"first session", "second session"}
	var drafts []ChatElicitationDraft
	for _, answer := range answers {
		chat := newTestChat()
		chat.restoreElicitation(request)
		chat.elicitation.paste(answer)
		drafts = append(drafts, *chat.elicitationDraft())
	}
	for index, draft := range drafts {
		answer := answers[index]
		chat := newTestChat()
		if chat.restoreElicitationDraft(draft) {
			t.Fatal("a draft restored without its request")
		}
		changed := request
		changed.Message = "A different question with the same id"
		chat.restoreElicitation(changed)
		if chat.restoreElicitationDraft(draft) {
			t.Fatal("a draft restored onto a changed request")
		}
		chat.syncElicitation([]core.ElicitationRequest{request})
		if !chat.restoreElicitationDraft(draft) {
			t.Fatal("a draft did not restore onto its request")
		}
		requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionRespondElicitation{
			Request: request,
			Response: core.ElicitationResponse{
				Action:  core.ElicitationResponseAccept,
				Content: map[string]core.ElicitationValue{"branch": core.ElicitationValueString(answer)},
			},
		})
		chat.syncElicitation(nil)
		if chat.restoreElicitationDraft(draft) {
			t.Fatal("a draft restored after its request was answered")
		}
	}
}

func TestRestoredQuestionDraftsCannotChangeTheAnswerRecipient(t *testing.T) {
	request := textElicitation()
	reviewer := newTestChat()
	reviewer.showReviewRoleElicitation(stringPtr("reviewer-a"), request)
	draft := *reviewer.elicitationDraft()
	primary := newTestChat()
	primary.restoreElicitation(request)
	if primary.restoreElicitationDraft(draft) {
		t.Fatal("a reviewer's draft restored onto the primary's form")
	}
	otherReviewer := newTestChat()
	otherReviewer.showReviewRoleElicitation(stringPtr("reviewer-b"), request)
	if otherReviewer.restoreElicitationDraft(draft) {
		t.Fatal("a reviewer's draft restored onto another reviewer's form")
	}
}

func TestReviewerFormReconciliationDropsStaleFormsAndResurfacesPrimary(t *testing.T) {
	request := core.ElicitationRequest{
		ID:             "reviewer-form-1",
		Message:        "Allow reading /etc?",
		Title:          nil,
		Description:    nil,
		Fields:         []core.ElicitationField{},
		ToolPermission: nil,
	}
	chat := newTestChat()
	if !chat.showReviewRoleElicitation(stringPtr("reviewer-a"), request) {
		t.Fatal("the reviewer form did not show")
	}

	chat.reconcileReviewerElicitation([]ReviewerPendingElicitation{{Role: stringPtr("reviewer-a"), Request: request}})
	if !chat.reviewerElicitationOpen() {
		t.Fatal("a still-pending reviewer form closed")
	}

	changed := request
	changed.Message = "Allow reading /var?"
	chat.reconcileReviewerElicitation([]ReviewerPendingElicitation{{Role: stringPtr("reviewer-a"), Request: changed}})
	if chat.reviewerElicitationOpen() {
		t.Fatal("a stale reviewer form stayed open")
	}

	chat.syncElicitation([]core.ElicitationRequest{request})
	chat.elicitation = nil
	if !chat.showReviewRoleElicitation(stringPtr("reviewer-a"), changed) {
		t.Fatal("the changed reviewer form did not show")
	}
	chat.reconcileReviewerElicitation(nil)
	if chat.reviewerElicitationOpen() {
		t.Fatal("a withdrawn reviewer form stayed open")
	}
	if chat.elicitation == nil {
		t.Fatal("primary pending form did not resurface")
	}
	if chat.elicitationIsReviewers {
		t.Fatal("the resurfaced form is still the reviewer's")
	}
}

// A pending elicitation is durable projection state, rebuilt from the
// session the next time it is opened, so leaving the view is a different act
// from answering the agent. The moved-key notices are handled on the same
// terms: they pass the open form without consuming it.
// Hard-won: 3fbdd2ae: An open elicitation trapped users because detach keys were consumed.
func TestControlGAndControlQPassAChatWhoseElicitationIsStillOpen(t *testing.T) {
	chat := newTestChat()
	request := textElicitation()
	chat.restoreElicitation(request)

	requireChatAction(t, chat.HandleKey(ctrl('g')), ChatActionNone{})
	requireChatAction(t, chat.HandleKey(ctrl('q')), ChatActionNone{})
	requireEqual(t, chat.MaterializedSession().PendingElicitations, []core.ElicitationRequest{request})

	// Every other key still belongs to the form, and Escape still answers
	// the agent rather than leaving.
	requireChatAction(t, chat.HandleKey(key('q')), ChatActionNone{})
	requireChatAction(t, chat.HandleKey(key(tea.KeyEscape)), ChatActionRespondElicitation{
		Request:  request,
		Response: core.ElicitationResponse{Action: core.ElicitationResponseCancel},
	})
	if pending := chat.MaterializedSession().PendingElicitations; len(pending) != 0 {
		t.Fatalf("pending elicitations = %v", pending)
	}
}

func TestEscapeOnlyCancelsAnActiveTurn(t *testing.T) {
	chat := newTestChat()
	controlC := ctrl('c')
	requireChatAction(t, chat.HandleKey(controlC), ChatActionNone{})
	requireChatAction(t, chat.HandleKey(key(tea.KeyEscape)), ChatActionNone{})

	// A phase alone does not identify a running turn.
	chat.phase = core.WorkerPhaseRunning
	requireChatAction(t, chat.HandleKey(key(tea.KeyEscape)), ChatActionNone{})

	// Esc interrupts a Codex goal turn as well as a prompted turn.
	chat.sessionActivity.HarnessTurnStartedAtMs = int64Ptr(1_000)
	chat.sessionActivity.PursuingGoal = true
	requireChatAction(t, chat.HandleKey(key(tea.KeyEscape)), ChatActionCancel{})

	// A turn Claude Code started on its own after a background task can be
	// stopped.
	chat.sessionActivity.PursuingGoal = false
	requireChatAction(t, chat.HandleKey(key(tea.KeyEscape)), ChatActionCancel{})
	chat.sessionActivity.HarnessTurnStartedAtMs = nil

	chat.setPromptInFlight(true)
	requireChatAction(t, chat.HandleKey(key(tea.KeyEscape)), ChatActionCancel{})
	requireChatAction(t, chat.HandleKey(controlC), ChatActionNone{})
}

func TestCancellationWaitsForTurnCompletionBeforeQueueCanDrain(t *testing.T) {
	chat := newTestChat()
	chat.phase = core.WorkerPhaseRunning
	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-1", "next"))
	chat.applyEvent(&core.SequencedEvent{
		Seq:          1,
		RecordedAtMs: nil,
		RequestID:    stringPtr("cancel"),
		Event:        core.WorkerEventCancelled{},
	})
	if chat.phase != core.WorkerPhaseRunning {
		t.Fatalf("phase after cancellation = %v", chat.phase)
	}

	chat.applyEvent(&core.SequencedEvent{
		Seq:          2,
		RecordedAtMs: nil,
		RequestID:    nil,
		Event:        core.WorkerEventTurnCompleted{},
	})
	if chat.phase != core.WorkerPhaseIdle {
		t.Fatalf("phase after completion = %v", chat.phase)
	}
	if chat.queuedPrompts[0].text != "next" {
		t.Fatalf("front of queue = %q", chat.queuedPrompts[0].text)
	}
}

// A harness that advertises no selector cannot apply the change at all, so
// the refusal belongs in the footer now rather than in a transcript line
// that arrives seconds after "Configuration update accepted". The words are
// the runtime's own, and the article follows the key's name. The composer
// clears the same as it would for a command that was actually sent, so the
// next command typed does not append to the refused one.
// Hard-won: 27140151: An unsupported selector was reported accepted before its late refusal arrived.
func TestASelectorTheHarnessDoesNotExposeIsRefusedBeforeAnythingIsSent(t *testing.T) {
	chat := newTestChat()
	// A live session whose harness advertises effort but no model at all.
	chat.setConfigOptions([]schema.SessionConfigOption{selectConfigOption("effort", "high", []string{"high", "low"})})

	chat.input = "/model o3-mini"
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
	requireCurrent(t, chat.feedback, "This agent does not offer a model setting")
	// Nothing was sent, but the command was still handled: the composer
	// clears exactly as it would for an accepted command, so the next
	// command typed does not append to the refused one.
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}

	chat.setConfigOptions(nil)
	chat.input = "/effort xhigh"
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
	requireCurrent(t, chat.feedback, "This agent does not offer a reasoning effort setting")
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}
}

func TestEditingQueuedImagesPreservesPayloadAndRecoversFailedRemoval(t *testing.T) {
	source := newTestChat()
	source.setPromptImagesSupported(true)
	source.setInput("compare ")
	source.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	source.handlePaste(" with ")
	source.handleClipboardContent(ClipboardContentImage{Image: testImage()})
	payload := source.draftPayload()
	session := core.MaterializedSessionEmpty("image-queue")
	session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
		AcceptedOrdinal: nil,
		CommandID:       "queued-image",
		Kind:            core.QueuedCommandKindPrompt{},
		Content:         promptContentBlocks(payload.Text, payload.Images),
		QueuedAtMs:      10,
	})
	chat := ChatStateFromMaterialized(&session, nil, nil)
	if _, ok := chat.HandleKey(key(tea.KeyUp)).(ChatActionRemoveQueuedPrompt); !ok {
		t.Fatal("Up did not peel the queued prompt")
	}
	requireEqual(t, chat.draftPayload(), payload)
	applyChatRemoteResult(chat, chatRemoteResultRemoveQueuedPrompt{
		ID:   "queued-image",
		Text: payload.Text,
		Kind: core.QueuedCommandKindPrompt{},
		Err:  errors.New("offline"),
	})
	requireEqual(t, chat.queuedPrompts[len(chat.queuedPrompts)-1].images, payload.Images)
	chat.HandleKey(key(tea.KeyBackspace))
	if len(chat.inputImages) != 1 {
		t.Fatalf("images = %d, want 1", len(chat.inputImages))
	}
	if chat.input != "compare [image 1] with " {
		t.Fatalf("input = %q", chat.input)
	}
}

func TestStaleProjectionDoesNotRestoreAQueueEntryBeingEdited(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-queue-edit")
	session.AppliedEventOrdinal = 5
	session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
		AcceptedOrdinal: nil,
		CommandID:       "queued-prompt",
		Kind:            core.QueuedCommandKindPrompt{},
		Content:         []any{map[string]any{"type": "text", "text": "revise me"}},
		QueuedAtMs:      10,
	})
	chat := ChatStateFromMaterialized(&session, nil, nil)

	requireChatAction(t, chat.HandleKey(key(tea.KeyUp)), ChatActionRemoveQueuedPrompt{
		ID:   "queued-prompt",
		Text: "revise me",
		Kind: core.QueuedCommandKindPrompt{},
	})
	applyChatRemoteResult(chat, chatRemoteResultRemoveQueuedPrompt{
		ID:   "queued-prompt",
		Text: "revise me",
		Kind: core.QueuedCommandKindPrompt{},
		Err:  nil,
	})

	// The relay accepted the removal, but its previously published view can
	// still arrive before the projection containing that command.
	chat.ApplyMaterialized(&session, nil, nil)
	if chat.input != "revise me" {
		t.Fatalf("input = %q", chat.input)
	}
	if len(chat.queuedPrompts) != 0 {
		t.Fatalf("queued prompts = %d", len(chat.queuedPrompts))
	}
	if _, ok := chat.pendingQueueRemovals["queued-prompt"]; !ok {
		t.Fatal("the removal is no longer pending")
	}

	session.AppliedEventOrdinal = 6
	session.QueuedPrompts = nil
	chat.ApplyMaterialized(&session, nil, nil)
	if len(chat.pendingQueueRemovals) != 0 {
		t.Fatalf("pending removals = %v", chat.pendingQueueRemovals)
	}
}

func TestFailedQueueRemovalRestoresThePeeledEntry(t *testing.T) {
	chat := newTestChat()
	chat.queuedPrompts = append(chat.queuedPrompts, queued("queued-prompt", "revise me"))
	chat.HandleKey(ctrl('p'))

	applyChatRemoteResult(chat, chatRemoteResultRemoveQueuedPrompt{
		ID:   "queued-prompt",
		Text: "revise me",
		Kind: core.QueuedCommandKindPrompt{},
		Err:  errors.New("relay rejected removal"),
	})

	if len(chat.pendingQueueRemovals) != 0 {
		t.Fatalf("pending removals = %v", chat.pendingQueueRemovals)
	}
	if len(chat.queuedPrompts) != 1 {
		t.Fatalf("queued prompts = %d", len(chat.queuedPrompts))
	}
	if chat.queuedPrompts[0].id != "queued-prompt" {
		t.Fatalf("queued prompt id = %q", chat.queuedPrompts[0].id)
	}
}

func TestAnUnchangedModeCatalogueDoesNotUndoAnOptimisticToggle(t *testing.T) {
	options := []schema.SessionConfigOption{modeConfigOption("default", []string{"default", "plan"})}
	chat := newTestChat()
	chat.setConfigOptions(options)
	chat.setInput("/plan")
	if _, ok := chat.submitInput().(ChatActionPlanCommand); !ok {
		t.Fatal("/plan did not become a plan command")
	}

	chat.setConfigOptions(options)

	if !chat.planModeActive() {
		t.Fatal("an unchanged catalogue undid the toggle")
	}
}

func TestACurrentModeUpdateCorrectsTheLocallyTrackedPlanMode(t *testing.T) {
	chat := grokChat()
	chat.setInput("/plan")
	chat.submitInput()
	if !chat.planModeActive() {
		t.Fatal("/plan did not activate plan mode")
	}

	session := core.MaterializedSessionEmpty("1234567890")
	if session.Configuration.Values == nil {
		session.Configuration.Values = map[string]any{}
	}
	session.Configuration.Values["mode"] = "default"
	chat.ApplyMaterialized(&session, nil, nil)

	if chat.planModeActive() {
		t.Fatal("the current mode did not correct plan mode")
	}
}

// Hard-won: 6a66a7d0: Refused slash commands remained in the draft and contaminated the next command.
func TestReviewTierSlashArgsAreNotConfigGestures(t *testing.T) {
	for _, tier := range []string{"quick", "extended"} {
		chat := newTestChat()
		chat.setInput("/review " + tier)
		requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
		requireNotice(t, chat, "usage: /review [status]")
		if chat.input != "" {
			t.Fatalf("input = %q", chat.input)
		}
	}
}

func TestARefusedSlashCommandClearsTheDraftSoTheNextCommandStandsAlone(t *testing.T) {
	chat := newTestChat()
	chat.input = "/model"
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
	if chat.input != "" {
		t.Fatalf("draft kept: %q", chat.input)
	}
	requireNoticeContains(t, chat, "/model")
}

// Hard-won: 6a66a7d0: Unknown slash commands were sent to the agent as ordinary prompts.
func TestAnUnknownSlashCommandIsNotSentToTheAgent(t *testing.T) {
	chat := newTestChat()
	chat.input = "/bogus thing"
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}
	requireNoticeContains(t, chat, "/bogus")

	// A path is a prompt, not a command.
	chat.input = "/tmp/log is empty"
	if _, ok := chat.HandleKey(key(tea.KeyEnter)).(ChatActionPrompt); !ok {
		t.Fatal("a path was not sent as a prompt")
	}
}

// shifted is a letter typed with Shift, as crossterm reports it
// (`KeyEvent::new(KeyCode::Char('H'), KeyModifiers::SHIFT)`).
func shifted(character rune, modifiers tea.KeyMod) tea.KeyPressMsg {
	lower := []rune(strings.ToLower(string(character)))[0]
	press := tea.KeyPressMsg{Code: lower, ShiftedCode: character, Mod: modifiers | tea.ModShift}
	if modifiers == 0 {
		press.Text = string(character)
	}
	return press
}

func TestEditorPreservesUppercaseTextWhileShortcutsRemainCaseInsensitive(t *testing.T) {
	chat := newTestChat()

	chat.HandleKey(shifted('H', 0))
	// Some terminals report the uppercase character without a Shift
	// modifier.
	chat.HandleKey(key('I'))
	if chat.input != "HI" {
		t.Fatalf("input = %q", chat.input)
	}

	chat.HandleKey(ctrl('r'))
	chat.HandleKey(shifted('N', 0))
	if chat.historySearch == nil || chat.historySearch.query.Value() != "N" {
		t.Fatalf("history search = %#v", chat.historySearch)
	}
	chat.HandleKey(key(tea.KeyEscape))

	// A shifted Alt chord still reaches the readline shortcut it names, so
	// Alt-Shift-B moves back a word rather than typing one.
	if chat.inputCursor != 2 {
		t.Fatalf("cursor = %d", chat.inputCursor)
	}
	chat.HandleKey(shifted('B', tea.ModAlt))
	if chat.inputCursor != 0 {
		t.Fatalf("cursor after Alt-Shift-B = %d", chat.inputCursor)
	}
	if chat.input != "HI" {
		t.Fatalf("input = %q", chat.input)
	}
}

// Hard-won: ac740841: Synchronous clipboard access could stall the chat event loop.
func TestCtrlVReturnsPasteRequestAction(t *testing.T) {
	chat := newTestChat()

	requireChatAction(t, chat.HandleKey(ctrl('v')), ChatActionPasteFromClipboard{})
	requireChatAction(t, chat.HandleKey(chord('v', tea.ModSuper)), ChatActionPasteFromClipboard{})
	requireChatAction(t, chat.HandleKey(chord('v', tea.ModCtrl|tea.ModAlt)), ChatActionPasteFromClipboard{})
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}
}

// adapterSessionUpdate is a worker Adapter event carrying a runtime session
// update, as the relay journals it.
func adapterSessionUpdate(t *testing.T, seq uint64, update string) core.SequencedEvent {
	t.Helper()
	payload, err := json.Marshal(core.RuntimeEventSessionUpdate{Update: json.RawMessage(update)})
	if err != nil {
		t.Fatal(err)
	}
	return core.SequencedEvent{
		Seq:   seq,
		Event: core.WorkerEventAdapter{Kind: "session_update", Payload: payload},
	}
}

func TestHydratedTailContinuesTheLastStreamedMessage(t *testing.T) {
	event := adapterSessionUpdate(t, 1, `{
		"sessionUpdate": "agent_message_chunk",
		"messageId": "answer",
		"content": {"type": "text", "text": "hello"}
	}`)
	initial := snapshot()
	initial.LatestSeq = 1
	full := NewChatState(&initial, []core.SequencedEvent{event})
	entries := full.boundedEntries(10, 512*1024)
	tail := ChatStateFromTail(initial.SessionID, core.WorkerPhaseRunning, 1, entries)
	tail.ApplyEvents([]core.SequencedEvent{adapterSessionUpdate(t, 2, `{
		"sessionUpdate": "agent_message_chunk",
		"messageId": "answer",
		"content": {"type": "text", "text": " world"}
	}`)})

	if len(tail.entries) != 1 {
		t.Fatalf("entries = %d, want 1", len(tail.entries))
	}
	if tail.entries[0].Text != "hello world" {
		t.Fatalf("text = %q", tail.entries[0].Text)
	}
	materialized := tail.MaterializedSession()
	if materialized.Transcript[0].Position != 1 {
		t.Fatalf("position = %d", materialized.Transcript[0].Position)
	}
	requireEqual(t, materialized.Transcript[0].LatestContentEventOrdinal, uint64Ptr(2))
	if unread := materialized.UnreadAgentMessagesAfter(1); unread != 1 {
		t.Fatalf("unread = %d", unread)
	}
}

// Hard-won: f378854d: Every streamed token created a separate transcript entry.
func TestStreamedMessageChunksCoalesceIntoOneEntry(t *testing.T) {
	initial := snapshot()
	initial.LatestSeq = 0
	chat := NewChatState(&initial, nil)
	for _, chunk := range []struct {
		seq  uint64
		text string
	}{{1, "gpt"}, {2, "-5.6"}, {3, "-terra"}} {
		update, _ := json.Marshal(map[string]any{
			"sessionUpdate": "agent_message_chunk",
			"content":       map[string]any{"type": "text", "text": chunk.text},
		})
		chat.applySessionUpdate(chunk.seq, update)
	}
	chat.applySessionUpdate(4, json.RawMessage(`{
		"sessionUpdate": "agent_thought_chunk",
		"content": {"type": "text", "text": "hmm"}
	}`))
	if len(chat.entries) != 2 {
		t.Fatalf("entries = %d, want 2", len(chat.entries))
	}
	if chat.entries[0].Role != core.ChatRoleAgent {
		t.Fatalf("first role = %v", chat.entries[0].Role)
	}
	if chat.entries[0].Text != "gpt-5.6-terra" {
		t.Fatalf("first text = %q", chat.entries[0].Text)
	}
	if chat.entries[1].Role != core.ChatRoleThought {
		t.Fatalf("second role = %v", chat.entries[1].Role)
	}
}

func TestPartialToolUpdatesPreserveUnchangedStructuredFields(t *testing.T) {
	chat := newTestChat()
	chat.applySessionUpdate(1, json.RawMessage(`{
		"sessionUpdate": "tool_call",
		"toolCallId": "inspect",
		"title": "inspect",
		"content": [{
			"type": "content",
			"content": {"type": "text", "text": "first result"}
		}],
		"locations": [{"path": "src/lib.rs", "line": 7}]
	}`))
	chat.applySessionUpdate(2, json.RawMessage(`{
		"sessionUpdate": "tool_call_update",
		"toolCallId": "inspect",
		"content": [{
			"type": "content",
			"content": {"type": "text", "text": "replacement result"}
		}]
	}`))

	requireEqual(t, chat.entries[0].ToolContent, []string{"replacement result"})
	requireEqual(t, chat.entries[0].ToolLocations, []string{"src/lib.rs:7"})
}

func TestUnknownJSONDoesNotLeakNestedTextIntoTheTranscript(t *testing.T) {
	chat := newTestChat()
	chat.applySessionUpdate(1, json.RawMessage(`{"items": [{"text": "not an ACP message"}]}`))
	if len(chat.entries) != 0 {
		t.Fatalf("entries = %d, want 0", len(chat.entries))
	}
}

func TestMessageIDsKeepAdjacentAgentMessagesSeparate(t *testing.T) {
	chat := newTestChat()
	for _, message := range []struct {
		seq      uint64
		id, text string
	}{{1, "one", "first"}, {2, "two", "second"}} {
		update, _ := json.Marshal(map[string]any{
			"sessionUpdate": "agent_message_chunk",
			"messageId":     message.id,
			"content":       map[string]any{"type": "text", "text": message.text},
		})
		chat.applySessionUpdate(message.seq, update)
	}
	if len(chat.entries) != 2 {
		t.Fatalf("entries = %d, want 2", len(chat.entries))
	}
	if chat.entries[0].Text != "first" || chat.entries[1].Text != "second" {
		t.Fatalf("texts = %q, %q", chat.entries[0].Text, chat.entries[1].Text)
	}
}

func TestSameOrdinalMaterializedUpdateKeepsTranscriptCacheButRefreshesQueue(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-same-ordinal")
	session.AppliedEventOrdinal = 1
	session.Transcript = append(session.Transcript, &core.TranscriptItem{
		StableID:                  "user:1",
		Position:                  1,
		LatestContentEventOrdinal: nil,
		CreatedAtMs:               10,
		LastChangedAtMs:           10,
		Body:                      core.TranscriptBodyUser{Content: []json.RawMessage{json.RawMessage(`"first"`)}},
	})

	chat := ChatStateFromMaterialized(&session, nil, nil)
	if chat.entries[0].Text != "first" {
		t.Fatalf("text = %q", chat.entries[0].Text)
	}

	// Arc::make_mut: the chat may still share the original item.
	changed := *session.Transcript[0]
	changed.Body = core.TranscriptBodyUser{Content: []json.RawMessage{json.RawMessage(`"changed without new ordinal"`)}}
	session.Transcript[0] = &changed
	session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
		AcceptedOrdinal: nil,
		CommandID:       "queued",
		Kind:            core.QueuedCommandKindPrompt{},
		Content:         []any{"queued prompt"},
		QueuedAtMs:      20,
	})
	chat.ApplyMaterialized(&session, nil, nil)

	if chat.entries[0].Text != "first" {
		t.Fatalf("text after same-ordinal update = %q", chat.entries[0].Text)
	}
	if len(chat.queuedPrompts) != 1 || chat.queuedPrompts[0].text != "queued prompt" {
		t.Fatalf("queued prompts = %#v", chat.queuedPrompts)
	}
}

func TestMaterializedDiffCountsArriveAfterThePathAndIgnoreStaleRevisions(t *testing.T) {
	session := core.MaterializedSessionEmpty("session-diffstats")
	session.AppliedEventOrdinal = 1
	session.Transcript = append(session.Transcript, &core.TranscriptItem{
		StableID:                  "tool:edit",
		Position:                  1,
		LatestContentEventOrdinal: nil,
		CreatedAtMs:               10,
		LastChangedAtMs:           10,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(json.RawMessage(`{
				"toolCallId": "edit",
				"title": "Edit src/lib.rs",
				"status": "completed",
				"content": [{
					"type": "diff",
					"path": "/workspace/src/lib.rs",
					"oldText": "alpha\n",
					"newText": "alpha\nbeta\n"
				}]
			}`)),
			TerminalOutputs: nil,
			TerminalRefs:    nil,
			Presentation:    nil,
		},
	})

	chat := ChatStateFromMaterialized(&session, nil, nil)
	requireEqual(t, chat.entries[0].ToolDiffstats, []string{"/workspace/src/lib.rs"})
	requests := chat.takeDiffstatRequests(1)
	request := requests[len(requests)-1]
	exact, exactErr := request.compute()
	chat.applyDiffstats("tool:edit", 9, exact, exactErr)
	requireEqual(t, chat.entries[0].ToolDiffstats, []string{"/workspace/src/lib.rs"})
	chat.applyDiffstats("tool:edit", 10, exact, exactErr)
	requireEqual(t, chat.entries[0].ToolDiffstats, []string{"/workspace/src/lib.rs  +1 −0"})
}

func TestClearRequiresCapabilityAndIdleStateBeforeSubmission(t *testing.T) {
	chat := newTestChat()
	chat.phase = core.WorkerPhaseIdle
	chat.setInput("/clear")
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
	requireNoticeContains(t, chat, "does not support")
	chat.clearContextSupported = true
	chat.rebuildCommandChoices()
	if !chat.listsCommand("clear") {
		t.Fatal("/clear is not listed")
	}
	chat.phase = core.WorkerPhaseRunning
	chat.setInput("/clear")
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionNone{})
	requireNoticeContains(t, chat, "idle")
	chat.phase = core.WorkerPhaseIdle
	chat.setInput("/clear")
	requireChatAction(t, chat.HandleKey(key(tea.KeyEnter)), ChatActionPrompt{Text: "/clear"})
}

func TestContinuingFeedFailureSurvivesDismissalAndClearsOnlyOnRecovery(t *testing.T) {
	notices := NewNotices()
	notices.Set("Unrelated task finished")
	notices.SetPersistentFailure(stringPtr("Could not refresh sessions: connection closed"))
	generation := notices.Generation()
	notices.SetPersistentFailure(stringPtr("Could not refresh sessions: connection closed"))
	if notices.Generation() != generation {
		t.Fatal("repeating the failure changed the generation")
	}
	if history := notices.History(); len(history) != 2 {
		t.Fatalf("history = %d, want 2", len(history))
	}
	if notices.Dismiss(time.Now().Add(NoticeMinimumDisplay)) {
		t.Fatal("a continuing failure was dismissed")
	}
	if current, ok := notices.Current(); !ok || !strings.Contains(current, "Could not refresh sessions") {
		t.Fatalf("current = %q (%t)", current, ok)
	}
	notices.SetPersistentFailure(nil)
	requireCurrent(t, notices, "Unrelated task finished")
	if notices.Generation() <= generation {
		t.Fatal("recovery did not advance the generation")
	}

	notices.SetPersistentFailure(stringPtr("Could not refresh sessions"))
	notices.Clear()
	if current, ok := notices.Current(); !ok || !strings.Contains(current, "Could not refresh sessions") {
		t.Fatalf("current after clear = %q (%t)", current, ok)
	}
	notices.SetPersistentFailure(nil)
	requireNoCurrent(t, notices)
}

func TestSavedImageDraftsRequireCurrentCapabilityWithoutLosingContent(t *testing.T) {
	payload := PromptPayloadWithImage("inspect ", testImage())
	chat := newTestChat()
	chat.setInputPayload(payload)
	reopened := newTestChat()
	reopened.restoreDraft(chat.encodedDraft())
	requireChatAction(t, reopened.submitInput(), ChatActionNone{})
	requireEqual(t, reopened.draftPayload(), payload)
	requireNoticeContains(t, reopened, "advertised image support")
	reopened.setPromptImagesSupported(true)
	if _, ok := reopened.submitInput().(ChatActionPrompt); !ok {
		t.Fatal("a supported image draft was not submitted")
	}
	requireEqual(t, reopened.takeSubmittingImages(), payload.Images)
}

// Hard-won: 6a66a7d0: A refused slash command remained in the draft and contaminated later input.
func TestAttachRequiresCapabilityAndClearsTheCommandOnRefusal(t *testing.T) {
	chat := newTestChat()
	chat.setInput("/attach picture.png")
	requireChatAction(t, chat.submitInput(), ChatActionNone{})
	if chat.input != "" {
		t.Fatalf("input = %q", chat.input)
	}
	notice, ok := chat.Notice()
	if !ok || notice != attachUnsupportedNotice {
		t.Fatalf("notice = %q (%t)", notice, ok)
	}
	if strings.Contains(notice, "marker") {
		t.Fatalf("notice mentions markers: %q", notice)
	}
	if chat.reserveAttachment(1) {
		t.Fatal("an attachment was reserved without image support")
	}
	if len(chat.inputImages) != 0 {
		t.Fatalf("images = %d", len(chat.inputImages))
	}
}
