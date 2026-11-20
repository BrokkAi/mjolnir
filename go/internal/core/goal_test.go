// Port of the tests in mj-core/src/goal.rs.

package core

import (
	"encoding/json"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acp "github.com/BrokkAi/acp-go/schema"
)

func goalUpdate(t *testing.T, meta string) *acp.SessionUpdate {
	t.Helper()
	var update acp.SessionUpdate
	if err := json.Unmarshal([]byte(`{"sessionUpdate":"session_info_update","_meta":`+meta+`}`), &update); err != nil {
		t.Fatal(err)
	}
	return &update
}

func goalApply(t *testing.T, state *GoalState, meta string) {
	t.Helper()
	if _, err := state.Apply(goalUpdate(t, meta)); err != nil {
		t.Fatal(err)
	}
}

func TestGoalMetadataSurvivesPromptBoundariesAndDistinguishesMissingFromClear(t *testing.T) {
	var state GoalState
	goalApply(t, &state, `{"jetbrains":{"air":{"goal":{"objective":"finish","status":"active","createdAt":12,"tokensUsed":42,"tokenBudget":100}}}}`)
	if !state.Active() {
		t.Fatal("goal is not active")
	}
	goalApply(t, &state, `{"execution":{"version":1,"status":"running","turnId":"next"}}`)
	goalApply(t, &state, `{"execution":{"version":1,"status":"idle","turnId":"old"}}`)
	if !state.Running() {
		t.Fatal("goal is not running")
	}
	if !state.Synchronized() {
		t.Fatal("goal is not synchronized")
	}
	state.Restart()
	if !state.Active() {
		t.Fatal("restart lost the goal")
	}
	if state.Synchronized() {
		t.Fatal("restart kept synchronization")
	}
	if got := state.Snapshot.Details["tokensUsed"]; got != json.Number("42") {
		t.Fatalf("tokensUsed = %#v", got)
	}
	goalApply(t, &state, `{"jetbrains":{"air":{"goal":null}}}`)
	if state.Snapshot != nil {
		t.Fatal("null did not clear the goal")
	}
}

// Hard-won: dc436f0c: Claude bridge 0.84 moved goal metadata and Mjolnir stopped reading it
func TestReadsClaudeGoalMetadataUnderTheAirExtension(t *testing.T) {
	// claude-agent-acp 0.84.0 sends goal controls and snapshots only
	// under `_meta.jetbrains.air.goal`.
	var controls jsonvalue.Object
	if err := json.Unmarshal([]byte(`{"goal":null,"jetbrains":{"air":{
		"capabilities":["asyncTasks"],
		"goal":{"version":1,"controlMethod":"_session/goal","actions":["set","clear"]}
	}}}`), &controls); err != nil {
		t.Fatal(err)
	}
	value, ok := GoalMeta(controls)
	if !ok {
		t.Fatal("no goal meta")
	}
	var capability GoalCapability
	if err := goalDecode(value, &capability, false); err != nil {
		t.Fatal(err)
	}
	if !capability.Supports(GoalControlActionClear) {
		t.Fatal("capability does not support clear")
	}

	var state GoalState
	goalApply(t, &state, `{"jetbrains":{"air":{"goal":
		{"objective":"finish","status":"active","createdAt":12}}}}`)
	if !state.Active() {
		t.Fatal("goal is not active")
	}
	goalApply(t, &state, `{"jetbrains":{"air":{"goal":null}}}`)
	if state.Snapshot != nil {
		t.Fatal("null did not clear the goal")
	}
}

func TestALaterPauseSupersedesASavedResumeDecision(t *testing.T) {
	var goal GoalSnapshot
	if err := json.Unmarshal([]byte(`{"objective":"finish","status":"paused","createdAt":1,"updatedAt":10}`), &goal); err != nil {
		t.Fatal(err)
	}
	snapshot := goal
	state := GoalState{
		Snapshot: &snapshot,
		Decision: &GoalDecision{Goal: goal, Resume: true},
	}
	state.Restart()
	goalApply(t, &state, `{"jetbrains":{"air":{"goal":{"objective":"finish","status":"paused","createdAt":1,"updatedAt":10}}}}`)
	if state.Decision == nil {
		t.Fatal("the original pre-resume pause retains the accepted decision")
	}
	goalApply(t, &state, `{"jetbrains":{"air":{"goal":{"objective":"finish","status":"paused","createdAt":1,"updatedAt":11}}}}`)
	if state.Decision != nil {
		t.Fatal("a newer pause must not be undone during recovery")
	}
}

func TestExplicitResumeDecisionSurvivesARestart(t *testing.T) {
	request := "launch-1"
	context := GoalRecoveryContext{Request: &request}
	if !context.Asking() {
		t.Fatal("not asking")
	}
	goalApply(t, &context.State, `{"mjGoalResumePending":"launch-1"}`)
	context.Request = nil
	context.State.Restart()
	if !context.Asking() {
		t.Fatal("restart lost the pending resume")
	}
	goalApply(t, &context.State, `{"mjGoalResumeAnswered":"launch-1"}`)
	context.State.Restart()
	if context.Asking() {
		t.Fatal("answered resume still asks")
	}
}

func TestGoalControlsFollowAdvertisedActionsAndExpireOnRestart(t *testing.T) {
	var state GoalState
	for _, c := range []struct {
		actions string
		pause   bool
	}{
		{`["set", "clear"]`, false},
		{`["set", "pause", "resume", "clear"]`, true},
	} {
		goalApply(t, &state, `{"mjGoalCapability": {
			"version":1, "controlMethod":"_session/goal", "actions":`+c.actions+`
		}}`)
		if !state.Supports(GoalControlActionClear) {
			t.Fatal("clear unsupported")
		}
		if state.Supports(GoalControlActionPause) != c.pause || state.Supports(GoalControlActionResume) != c.pause {
			t.Fatalf("pause/resume support for %s", c.actions)
		}
		goalApply(t, &state, `{"jetbrains":{"air":{"goal":null}}}`)
		if !state.Supports(GoalControlActionClear) {
			t.Fatal("clearing a goal retains controls")
		}
		state.Restart()
		if state.Supports(GoalControlActionClear) {
			t.Fatal("restart kept controls")
		}
	}
	for _, c := range []struct {
		version string
		method  string
	}{{"2", "_session/goal"}, {"1", "unknown"}} {
		goalApply(t, &state, `{"mjGoalCapability": {
			"version":`+c.version+`, "controlMethod":"`+c.method+`", "actions":["clear"]
		}}`)
		if state.Supports(GoalControlActionClear) {
			t.Fatalf("version %s method %s supports clear", c.version, c.method)
		}
	}
}
