// Port of the tests in mj-worker/src/acp/air_tasks.rs.

package worker

import (
	"reflect"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestClaude097AutonomousTurnSpawnIsIdentifiedByTaskType(t *testing.T) {
	update := jsonvalue.Object{
		{Key: "sessionUpdate", Value: "async_task_spawned"},
		{Key: "asyncTaskId", Value: "autonomous-turn-123"},
		{Key: "name", Value: "Claude is working"},
		{Key: "taskType", Value: "turn"},
		{Key: "showInTranscript", Value: false},
		{Key: "canStop", Value: true},
		{Key: "description", Value: "Working autonomously"},
	}
	change, handled, err := airTaskUpdate(update)
	if err != nil || !handled {
		t.Fatalf("handled = %v, err = %v", handled, err)
	}
	want := core.AirTaskUpdateTurnSpawned{TaskID: "autonomous-turn-123", Replayed: false}
	if !reflect.DeepEqual(change, core.AirTaskUpdate(want)) {
		t.Fatalf("change = %#v, want %#v", change, want)
	}
}
