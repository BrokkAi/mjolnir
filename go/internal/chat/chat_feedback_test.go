// Tests ported from mj-chat/src/chat/feedback.rs.

package chat

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestLocalValidationDoesNotReplaceDashboardNotifications(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.notices.Set("Dashboard notification")
	chat.SetNotice("usage: /model <value>")
	if current, ok := chat.notices.Current(); !ok || current != "Dashboard notification" {
		t.Fatalf("dashboard notice = %q (%t)", current, ok)
	}
	if notice, ok := chat.Notice(); !ok || notice != "usage: /model <value>" {
		t.Fatalf("chat notice = %q (%t)", notice, ok)
	}
	chat.setInput("new draft")
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("chat notice survives a new draft: %q", notice)
	}
}

func TestAConversationFailureSurvivesProjectionRebuildWithoutRepeating(t *testing.T) {
	worker := snapshot()
	chat := NewChatState(&worker, nil)
	chat.conversationNotice("Cancellation failed: disconnected")
	chat.conversationNotice("Cancellation failed: disconnected")
	empty := core.MaterializedSessionEmpty("test")
	chat.ApplyMaterialized(&empty, nil, nil)
	screen := strings.Join(transcriptText(chat, 100), "\n")
	if count := strings.Count(screen, "Cancellation failed"); count != 1 {
		t.Fatalf("notice appears %d times in:\n%s", count, screen)
	}
	if notice, ok := chat.Notice(); ok {
		t.Fatalf("the failure became a chat notice: %q", notice)
	}
}
