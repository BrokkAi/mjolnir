// Tests of mj-chat/src/chat/submissions.rs.

package chat

import (
	"bytes"
	"encoding/base64"
	"errors"
	"image"
	"image/color"
	"image/png"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func submit(chat *ChatState, id string, text string) {
	sender, _ := newChatRemoteOperationQueue(4)
	queueChatRemoteOperation(
		sender,
		chatRemoteOperationPrompt{
			CommandID: id,
			Text:      text,
			Images:    []PromptImage{},
		},
		chat,
	)
}

// submissionResult is Rust's result(chat, id, Result<u64, String>): err nil
// for Ok(ordinal).
func submissionResult(chat *ChatState, id string, ordinal uint64, err error) {
	applyChatRemoteResult(
		chat,
		chatRemoteResultPrompt{
			CommandID: id,
			Text:      "hello",
			Images:    []PromptImage{},
			Ordinal:   ordinal,
			Err:       err,
		},
	)
}

// submissionQueued is a session whose queue holds command id.
func submissionQueued(id string) *core.MaterializedSession {
	session := core.MaterializedSessionEmpty("test")
	acceptedOrdinal := uint64(1)
	session.QueuedPrompts = append(session.QueuedPrompts, core.MaterializedQueuedPrompt{
		CommandID:       id,
		Kind:            core.QueuedCommandKindPrompt{},
		Content:         []any{map[string]any{"type": "text", "text": "hello"}},
		QueuedAtMs:      1,
		AcceptedOrdinal: &acceptedOrdinal,
	})
	return &session
}

// submissionsTestImage is the 2x2 PNG of chat/tests.rs's test_image.
func submissionsTestImage(t *testing.T) ClipboardImage {
	t.Helper()
	picture := image.NewNRGBA(image.Rect(0, 0, 2, 2))
	picture.Set(0, 0, color.NRGBA{R: 255, A: 255})
	picture.Set(1, 0, color.NRGBA{G: 255, A: 255})
	picture.Set(0, 1, color.NRGBA{B: 255, A: 255})
	picture.Set(1, 1, color.NRGBA{R: 255, G: 255, A: 255})
	var encoded bytes.Buffer
	if err := png.Encode(&encoded, picture); err != nil {
		t.Fatal(err)
	}
	result, err := ClipboardImageFromPNGBase64(base64.StdEncoding.EncodeToString(encoded.Bytes()))
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func newSubmissionChat() *ChatState {
	worker := snapshot()
	return NewChatState(&worker, nil)
}

func submissionScreen(chat *ChatState) string {
	return strings.Join(transcriptText(chat, 100), "\n")
}

func TestARefusedLocalResendPreservesTheUnconfirmedReceipt(t *testing.T) {
	chat := newSubmissionChat()
	sender, _ := newChatRemoteOperationQueue(1)
	queueChatRemoteOperation(
		sender,
		chatRemoteOperationPrompt{
			CommandID: "original",
			Text:      "hello",
			Images:    []PromptImage{},
		},
		chat,
	)
	chat.unconfirmSubmission("original", "connection reset")
	chat.setInput("next draft")
	retry := chat.resendSubmission("original")
	if retry == nil {
		t.Fatal("the unconfirmed receipt is resendable")
	}
	queueChatRemoteOperation(sender, retry, chat)
	if id, ok := chat.recoverableSubmissionID(); !ok || id != "original" {
		t.Fatalf("recoverable submission = %q, %v", id, ok)
	}
	if len(chat.pendingSubmissions) != 1 {
		t.Fatalf("pending submissions = %d", len(chat.pendingSubmissions))
	}
	if chat.input != "next draft" {
		t.Fatalf("input = %q", chat.input)
	}
}

// Hard-won: #1514: failed legacy receipt installation must remain retryable.
func TestFailedReceiptPreparationPreservesBytesIdentityAndVisibleDraft(t *testing.T) {
	chat := newSubmissionChat()
	payload := PromptPayloadWithImage("old receipt", submissionsTestImage(t))
	chat.beginSubmission(
		"original",
		SubmissionKindPrompt,
		payload,
		"Sending…",
	)
	chat.unconfirmSubmission("original", "connection reset")
	chat.setInput("new draft")
	if !chat.prepareReceiptRetry("original", "receipt-failure") {
		t.Fatal("the receipt retry is prepared")
	}
	if !strings.Contains(submissionScreen(chat), "preparing images") {
		t.Fatal("the receipt says its images are being prepared")
	}
	if strings.Contains(chat.encodedDraft(), "preparing images") {
		t.Fatal("the saved draft does not carry the preparing status")
	}
	chat.applyComposerUpdate(composerIoUpdateReceiptPrepared{
		commandID: "original",
		err:       errors.New("install refused"),
	})
	if chat.input != "new draft" {
		t.Fatalf("input = %q", chat.input)
	}
	if !reflect.DeepEqual(chat.pendingSubmissions[0].payload, payload) {
		t.Fatalf("payload = %#v", chat.pendingSubmissions[0].payload)
	}
	if !strings.Contains(chat.pendingSubmissions[0].status, "install refused") {
		t.Fatalf("status = %q", chat.pendingSubmissions[0].status)
	}
	if len(chat.composerRuntime.preparedReceiptRetries) != 0 {
		t.Fatalf("prepared retries = %v", chat.composerRuntime.preparedReceiptRetries)
	}
	saved := chat.encodedDraft()
	reopened := newSubmissionChat()
	reopened.restoreDraft(saved)
	if id, ok := reopened.recoverableSubmissionID(); !ok || id != "original" {
		t.Fatalf("recoverable submission = %q, %v", id, ok)
	}
	if !reflect.DeepEqual(reopened.pendingSubmissions[0].payload, payload) {
		t.Fatalf("payload = %#v", reopened.pendingSubmissions[0].payload)
	}
	if !reopened.prepareReceiptRetry("original", "receipt-failure") {
		t.Fatal("the reopened receipt retry is prepared")
	}
}

func TestSubmissionIsVisibleBeforeAnyRemoteReplyAndKeepsTheNewDraft(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "one", "hello")
	chat.setInput("next draft")
	screen := submissionScreen(chat)
	if !strings.Contains(screen, "hello") {
		t.Fatal(screen)
	}
	if !strings.Contains(screen, "Sending") {
		t.Fatal(screen)
	}
	submissionResult(chat, "one", 42, nil)
	screen = submissionScreen(chat)
	if !strings.Contains(screen, "hello") || !strings.Contains(screen, "Queued") {
		t.Fatal(screen)
	}
	if chat.input != "next draft" {
		t.Fatalf("input = %q", chat.input)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("notice = %q", notice)
	}
	chat.ApplyMaterialized(submissionQueued("one"), nil, nil)
	if count := len(chat.submissionEntries()); count != 0 {
		t.Fatalf("submission entries = %d", count)
	}
	if len(chat.queuedPrompts) != 1 {
		t.Fatalf("queued prompts = %d", len(chat.queuedPrompts))
	}
}

func TestProjectionBeforeReplyPreventsALateErrorRestoringASentPrompt(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "one", "hello")
	chat.ApplyMaterialized(submissionQueued("one"), nil, nil)
	chat.setInput("new draft")
	submissionResult(chat, "one", 0, errors.New("lost reply"))
	if chat.input != "new draft" {
		t.Fatalf("input = %q", chat.input)
	}
	if len(chat.conversationNotices) != 0 {
		t.Fatalf("notices = %#v", chat.conversationNotices)
	}
	if len(chat.pendingSubmissions) != 0 {
		t.Fatalf("pending = %d", len(chat.pendingSubmissions))
	}
}

func TestIdenticalPromptsReconcileByCommandIdentity(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "one", "hello")
	submit(chat, "two", "hello")
	submissionResult(chat, "one", 1, nil)
	chat.ApplyMaterialized(submissionQueued("one"), nil, nil)
	if count := len(chat.submissionEntries()); count != 1 {
		t.Fatalf("submission entries = %d", count)
	}
	if chat.pendingSubmissions[0].id != "two" {
		t.Fatalf("pending = %q", chat.pendingSubmissions[0].id)
	}
}

func TestAnUnconfirmedSubmissionSurvivesRebuildAndSavedDraftRestore(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "one", "hello")
	chat.unconfirmSubmission("one", "connection lost")
	chat.setInput("new draft")
	empty := core.MaterializedSessionEmpty("test")
	chat.ApplyMaterialized(&empty, nil, nil)
	saved := chat.encodedDraft()
	reopened := newSubmissionChat()
	reopened.restoreDraft(saved)
	if reopened.input != "new draft" {
		t.Fatalf("input = %q", reopened.input)
	}
	screen := submissionScreen(reopened)
	if !strings.Contains(screen, "hello") || !strings.Contains(screen, "Delivery unconfirmed") {
		t.Fatal(screen)
	}
	reopened.ApplyMaterialized(submissionQueued("one"), nil, nil)
	if count := len(reopened.submissionEntries()); count != 0 {
		t.Fatalf("submission entries = %d", count)
	}
}

// I1-12: the words "Delivery unconfirmed" appear once.
// Hard-won: 8bf21ad0: The shipped refusal path repeated the Delivery unconfirmed prefix.
func TestAnUnconfirmedStatusDoesNotRepeatItsPrefix(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "one", "hello")
	chat.unconfirmSubmission("one", "delivery unconfirmed: channel closed")
	if status := chat.pendingSubmissions[0].status; status != "Delivery unconfirmed: channel closed" {
		t.Fatalf("status = %q", status)
	}
}

// clearedThenAnswered is the divider the projection draws when the relay
// clears the context for command id, followed by one later turn.
func clearedThenAnswered(id string) *core.MaterializedSession {
	session := core.MaterializedSessionEmpty("test")
	session.Transcript = append(session.Transcript, &core.TranscriptItem{
		StableID:        core.ContextBoundaryPrefix + id,
		Position:        2,
		CreatedAtMs:     2,
		LastChangedAtMs: 2,
		Body: core.TranscriptBodySystem{
			Text: "Context cleared — a new conversation starts here.",
		},
	})
	session.Transcript = append(session.Transcript, agentMessageItem("agent:3", 3, "NONE"))
	return &session
}

// R4-4: an accepted `/clear` left "You · /clear · Queued" pinned below every
// later turn, and after resume the saved row read "Delivery unconfirmed". A
// clear has no user message to reconcile with; the relay's own "Clearing
// context…" line and divider are its record.
// Hard-won: 1c0dc652: Accepted /clear rows stayed pinned and returned as unconfirmed after resume.
func TestAnAcceptedClearLeavesNoPendingRowOrSavedSubmission(t *testing.T) {
	// The relay's acceptance arrives before the divider.
	chat := newSubmissionChat()
	submit(chat, "clear", "/clear")
	submissionResult(chat, "clear", 7, nil)
	chat.ApplyMaterialized(clearedThenAnswered("clear"), nil, nil)
	if screen := submissionScreen(chat); strings.Contains(screen, "Queued") {
		t.Fatal(screen)
	}
	if len(chat.pendingSubmissions) != 0 {
		t.Fatalf("pending = %d", len(chat.pendingSubmissions))
	}
	if draft := chat.encodedDraft(); strings.Contains(draft, "/clear") {
		t.Fatal(draft)
	}

	// The divider arrives before the acceptance.
	chat = newSubmissionChat()
	submit(chat, "clear", "/clear")
	chat.ApplyMaterialized(clearedThenAnswered("clear"), nil, nil)
	submissionResult(chat, "clear", 7, nil)
	if len(chat.pendingSubmissions) != 0 {
		t.Fatalf("pending = %d", len(chat.pendingSubmissions))
	}

	// A draft an older build saved with the row still pending is not
	// replayed once the divider shows the clear ran.
	chat = newSubmissionChat()
	submit(chat, "clear", "/clear")
	chat.unconfirmSubmission("clear", "connection lost")
	saved := chat.encodedDraft()
	reopened := newSubmissionChat()
	reopened.restoreDraft(saved)
	reopened.ApplyMaterialized(clearedThenAnswered("clear"), nil, nil)
	if screen := submissionScreen(reopened); strings.Contains(screen, "Delivery unconfirmed") {
		t.Fatal(screen)
	}
	if len(reopened.pendingSubmissions) != 0 {
		t.Fatalf("pending = %d", len(reopened.pendingSubmissions))
	}
}

// I1-12: a refused `/clear` becomes a dated notice, leaves nothing pinned
// below later turns or saved with the draft, and keeps the command in the
// composer for a retry.
// Hard-won: 8bf21ad0: A refused /clear was mislabeled unconfirmed and persisted as a pending row.
func TestARefusedSlashCommandIsANoticeNotAPinnedRow(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "clear", "/clear")
	applyChatRemoteResult(
		chat,
		chatRemoteResultPrompt{
			CommandID: "clear",
			Text:      "/clear",
			Images:    []PromptImage{},
			Err:       errors.New("/clear requires an idle session"),
		},
	)
	if len(chat.pendingSubmissions) != 0 {
		t.Fatalf("pending = %d", len(chat.pendingSubmissions))
	}
	if chat.input != "/clear" {
		t.Fatalf("input = %q", chat.input)
	}
	if count := len(chat.conversationNotices); count == 0 ||
		chat.conversationNotices[count-1].Text != "/clear was not run: /clear requires an idle session" {
		t.Fatalf("notices = %#v", chat.conversationNotices)
	}
	if strings.Contains(chat.encodedDraft(), "requires an idle session") {
		t.Fatal(chat.encodedDraft())
	}
}

func TestRefusedShellKeepsContentInTheConversationAndRestoresThePrefix(t *testing.T) {
	chat := newSubmissionChat()
	chat.beginSubmission(
		"shell",
		SubmissionKindShell,
		PromptPayloadText("pwd"),
		"Sending…",
	)
	applyChatRemoteResult(
		chat,
		chatRemoteResultRunShell{
			CommandID: "shell",
			Command:   "pwd",
			Err:       errors.New("refused"),
		},
	)
	if chat.input != "!pwd" {
		t.Fatalf("input = %q", chat.input)
	}
	screen := submissionScreen(chat)
	if !strings.Contains(screen, "Shell command was not sent") || !strings.Contains(screen, "pwd") {
		t.Fatal(screen)
	}
	if len(chat.submissionEntries()) != 0 {
		t.Fatalf("submission entries = %d", len(chat.submissionEntries()))
	}
}

func TestWorkerFailureRetiresSendingWithoutLosingThePayload(t *testing.T) {
	chat := newSubmissionChat()
	submit(chat, "one", "hello")
	applyChatRemoteResult(
		chat,
		chatRemoteResultWorkerFailed{Error: "worker stopped"},
	)
	screen := submissionScreen(chat)
	if !strings.Contains(screen, "hello") || !strings.Contains(screen, "Delivery unconfirmed") {
		t.Fatal(screen)
	}
	if strings.Contains(screen, "Sending") {
		t.Fatal(screen)
	}
	if strings.Contains(screen, "was not sent") {
		t.Fatal(screen)
	}
}

func TestAnImageOnlyPendingSubmissionKeepsItsMarkerAndImageOnRefusal(t *testing.T) {
	chat := newSubmissionChat()
	image := ClipboardImage{
		DataBase64: "image-data",
		MimeType:   "image/png",
	}
	payload := PromptPayloadWithImage("", image)
	chat.beginSubmission(
		"image",
		SubmissionKindPrompt,
		payload,
		"Sending…",
	)
	if screen := submissionScreen(chat); !strings.Contains(screen, "[image 1]") {
		t.Fatal(screen)
	}
	applyChatRemoteResult(
		chat,
		chatRemoteResultPrompt{
			CommandID: "image",
			Text:      payload.Text,
			Images:    payload.Images,
			Err:       errors.New("refused"),
		},
	)
	if draft := chat.draftPayload(); !reflect.DeepEqual(draft, payload) {
		t.Fatalf("draft = %#v, want %#v", draft, payload)
	}
}
