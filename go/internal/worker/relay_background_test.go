// Port of the cfg(test) items of mj-worker/src/relay/background.rs.

package worker

// AirTaskFixture is a test task. Fixtures are applied as AIR spawn/terminal
// edges, never as a provider level.
type AirTaskFixture struct {
	TaskID      string
	Description string
}

func (r *DurableRelay) setAirTasks(tasks []AirTaskFixture) error {
	panic("unported: mj-worker/src/relay/background.rs set_air_tasks")
}

func (r *DurableRelay) applyClaudeTaskFixture(taskID string, canStop, autonomousTurn, settled, replayed bool) error {
	panic("unported: mj-worker/src/relay/background.rs apply_claude_task_fixture")
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
