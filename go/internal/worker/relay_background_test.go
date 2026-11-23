// Port of the cfg(test) items of mj-worker/src/relay/background.rs.

package worker

import (
	"maps"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AirTaskFixture is a test task. Fixtures are applied as AIR spawn/terminal
// edges, never as a provider level.
type AirTaskFixture struct {
	TaskID      string
	Description string
}

func (r *DurableRelay) setAirTasks(tasks []AirTaskFixture) error {
	var ended []string
	for _, key := range slices.Sorted(maps.Keys(r.airTasks)) {
		entry := r.airTasks[key]
		if entry.sessionID == "root" && !slices.ContainsFunc(tasks, func(task AirTaskFixture) bool {
			return task.TaskID == entry.taskID
		}) {
			ended = append(ended, entry.taskID)
		}
	}
	for _, taskID := range ended {
		if err := r.AirTaskChanged("root", core.AirTaskUpdateFinished{TaskID: taskID}); err != nil {
			return err
		}
	}
	for _, task := range tasks {
		canStop := false
		if entry, ok := r.airTasks[airTaskKey("root", task.TaskID)]; ok {
			if command := entry.command(); command != nil {
				canStop = command.CanStop
			}
		}
		if err := r.AirTaskChanged("root", core.AirTaskUpdateSpawned{
			TaskID:      task.TaskID,
			Description: task.Description,
			CanStop:     canStop,
		}); err != nil {
			return err
		}
	}
	return nil
}

func (r *DurableRelay) applyClaudeTaskFixture(taskID string, canStop, autonomousTurn, settled, replayed bool) error {
	var update core.AirTaskUpdate
	switch {
	case autonomousTurn:
		update = core.AirTaskUpdateTurnSpawned{TaskID: taskID, Replayed: replayed}
	case canStop:
		stoppable := true
		update = core.AirTaskUpdateChanged{TaskID: taskID, CanStop: &stoppable}
	default:
		update = core.AirTaskUpdateFinished{TaskID: taskID, Settled: settled}
	}
	session := "root"
	if r.snapshot.NativeSessionID != nil {
		session = *r.snapshot.NativeSessionID
	}
	return r.AirTaskChanged(session, update)
}

func (r *DurableRelay) turnTaskOrdinal(taskID string) (uint64, bool) {
	session := "root"
	if r.snapshot.NativeSessionID != nil {
		session = *r.snapshot.NativeSessionID
	}
	entry, ok := r.airTasks[airTaskKey(session, taskID)]
	if !ok {
		return 0, false
	}
	return entry.turnOrdinal()
}
