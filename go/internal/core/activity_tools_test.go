// Port of the tests in mj-core/src/activity/tools.rs.

package core

import (
	"encoding/json"
	"fmt"
	"testing"

	"github.com/BrokkAi/acp-go/schema"
)

func toolsInFlightCall(t *testing.T, id string, status schema.ToolCallStatus) *schema.SessionUpdate {
	t.Helper()
	var update schema.SessionUpdate
	text := fmt.Sprintf(`{"sessionUpdate":"tool_call","toolCallId":%q,"title":%q,"status":%q}`, id, id, status)
	if err := json.Unmarshal([]byte(text), &update); err != nil {
		t.Fatalf("tool call update: %v", err)
	}
	return &update
}

func TestAStatusChangeRestartsTheClockButARepeatDoesNot(t *testing.T) {
	var tools ToolsInFlight
	tools.observeAt(toolsInFlightCall(t, "one", schema.ToolCallStatusPending), 1_000)
	tools.observeAt(toolsInFlightCall(t, "one", schema.ToolCallStatusPending), 5_000)
	if got := tools.Snapshot()[0].StartedAtMs; got != 1_000 {
		t.Fatalf("started = %d", got)
	}
	tools.observeAt(toolsInFlightCall(t, "one", schema.ToolCallStatusInProgress), 9_000)
	if got := tools.Snapshot()[0].StartedAtMs; got != 9_000 {
		t.Fatalf("started = %d", got)
	}
}
