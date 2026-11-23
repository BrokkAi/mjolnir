// Port of mj-chat/src/chat/input.rs (tests).

package chat

import (
	"reflect"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestMultilinePasteIsOneDraftAndOneQueuedPrompt(t *testing.T) {
	running := snapshot()
	running.Phase = core.WorkerPhaseRunning
	running.ActivePrompt = &core.ActivePrompt{RequestID: "p", Text: "busy", Attachments: []core.Attachment{}}
	chat := NewChatState(&running, nil)

	chat.handlePaste("first\r\nsecond\rthird")

	if chat.input != "first\nsecond\nthird" {
		t.Fatalf("input = %q", chat.input)
	}
	if len(chat.queuedPrompts) != 0 {
		t.Fatalf("queued = %#v", chat.queuedPrompts)
	}
	if got, want := chat.HandleKey(key(tea.KeyEnter)), ChatAction(ChatActionPrompt{Text: "first\nsecond\nthird"}); !reflect.DeepEqual(got, want) {
		t.Fatalf("Enter = %#v, want %#v", got, want)
	}
	if len(chat.queuedPrompts) != 0 {
		t.Fatalf("queued = %#v", chat.queuedPrompts)
	}
}

func TestControlKAndControlYRoundTripALineHoldingAnImageMarker(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.setPromptImagesSupported(true)
	chat.setInput("before\nafter")
	chat.HandleKey(ctrl('a'))
	if !chat.reserveAttachment(0) {
		t.Fatal("the attachment was not reserved")
	}
	if chat.input != "before\n[image 1]after" {
		t.Fatalf("input = %q", chat.input)
	}
	if len(chat.inputImages) != 1 {
		t.Fatalf("images = %d", len(chat.inputImages))
	}

	chat.HandleKey(ctrl('a'))
	chat.HandleKey(ctrl('k'))
	if chat.input != "before\n" {
		t.Fatalf("input after kill = %q", chat.input)
	}
	if len(chat.inputImages) != 0 {
		t.Fatalf("images after kill = %d", len(chat.inputImages))
	}

	chat.HandleKey(ctrl('y'))
	if chat.input != "before\n[image 1]after" {
		t.Fatalf("input after yank = %q", chat.input)
	}
	if len(chat.inputImages) != 1 {
		t.Fatalf("images after yank = %d", len(chat.inputImages))
	}
	if want := (ByteRange{Start: 7, End: len("before\n[image 1]")}); chat.inputImages[0].Range != want {
		t.Fatalf("marker range = %#v, want %#v", chat.inputImages[0].Range, want)
	}
}
