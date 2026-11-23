// Port of the tests in mj-chat/src/chat/earlier.rs.

package chat

import (
	"reflect"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestHistoryNavigationRetainsTheDraftAndDismissesWithoutSending(t *testing.T) {
	worker := core.WorkerSnapshotSummary("s", core.WorkerPhaseIdle, 0)
	chat := NewChatState(&worker, nil)
	chat.input = "unsent draft"
	chat.openEarlierMessages()
	if !chat.earlier.Requested {
		t.Fatal("opening the reader requests a page")
	}
	chat.earlier.Loading = true
	chat.earlier.Requested = false
	if action := chat.earlierKey(tea.KeyEnter); !reflect.DeepEqual(action, ChatActionNone{}) {
		t.Fatalf("Enter = %#v", action)
	}
	if chat.earlier.Requested {
		t.Fatal("Enter does not request a page while one loads")
	}
	chat.earlierKey(tea.KeyEscape)
	if chat.earlier != nil {
		t.Fatal("Esc closes the reader")
	}
	if chat.input != "unsent draft" {
		t.Fatalf("input = %q", chat.input)
	}
}
