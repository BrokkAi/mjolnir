// Port of the tests in mj-core/src/acp/step_clock.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/schema"
)

// stepClockRecorded is one line of a recorded bridge session: either an
// update the agent sent, or a request it made of the client.
type stepClockRecorded struct {
	AtMs          int64                 `json:"at_ms"`
	Update        *schema.SessionUpdate `json:"update"`
	ClientRequest *string               `json:"client_request"`
}

// stepStarts replays a recording and reports the offset, in milliseconds
// from the prompt, at which each step began. A repeated offset never
// appears: the list is exactly the moments the clock restarted.
func stepStarts(t *testing.T, name string) []int64 {
	t.Helper()
	recording, err := os.ReadFile(filepath.Join("testdata", "acp_step_clock", name))
	if err != nil {
		t.Fatal(err)
	}
	var clock StepClock
	clock.beginTurnAt(0)
	starts := []int64{0}
	for _, line := range strings.Split(string(recording), "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		var recorded stepClockRecorded
		if err := json.Unmarshal([]byte(line), &recorded); err != nil {
			t.Fatalf("recorded ACP line should parse: %v\n%s", err, line)
		}
		switch {
		case recorded.Update != nil:
			clock.observeAt(recorded.Update, recorded.AtMs)
		case recorded.ClientRequest != nil:
			clock.beginClientWorkAt(recorded.AtMs)
		default:
			t.Fatal("recorded line has neither an update nor a request")
		}
		started := clock.StartedAtMs()
		if started == nil {
			t.Fatal("a turn is open")
		}
		if starts[len(starts)-1] != *started {
			starts = append(starts, *started)
		}
	}
	return starts
}

func stepClockExpect(t *testing.T, name string, want []int64) {
	t.Helper()
	if got := stepStarts(t, name); !reflect.DeepEqual(got, want) {
		t.Fatalf("%s step starts = %v, want %v", name, got, want)
	}
}

func TestCodexStreamsAMessageAndAToolCallAsFourSteps(t *testing.T) {
	// Preamble message, the `List files` call running, the same call
	// completing, the thought, then the answer. The `usage_update` and
	// `session_info_update` either side of the answer are not steps, so the
	// last step stays open at 10186 rather than restarting at 13800.
	stepClockExpect(t, "codex.jsonl", []int64{0, 2918, 5190, 5193, 10161, 10186})
}

func TestClaudeToolOutputAndUsageUpdatesDoNotRestartTheStep(t *testing.T) {
	// Claude Code revises one tool call several times with no `status` of
	// its own — a title, then appended output — and sprays `usage_update`
	// between them. Only the creation and the completion are steps, so the
	// run of updates from 2856 to 3054 leaves the clock at 1911.
	stepClockExpect(t, "claude.jsonl", []int64{0, 1500, 1911, 3081, 7427, 7455, 8117, 16543})
}

func TestKimiStreamingToolInputDoesNotRestartTheStep(t *testing.T) {
	// Kimi streams a tool call's arguments as repeated `in_progress` updates
	// (4309, 4437, 4437) and later revises its title while still
	// `in_progress` (4858): one step, opened at 4309. The permission request
	// and the terminal it then creates are real work starting, so those do
	// restart it, and the content-only update at 4897 does not. The three
	// `Read` calls from 13079 on run in parallel, so their interleaved
	// updates are steps of their own.
	stepClockExpect(t, "kimi.jsonl", []int64{
		0, 3239, 4221, 4309, 4838, 4859, 5271, 9546, 13079, 13081, 13082, 13728, 14078,
		14463, 16883, 16936, 16937, 20625, 20627,
	})
}

func TestGrokToolCallsWithoutAStatusStartOnePendingStep(t *testing.T) {
	// Grok announces a tool call with no `status` field and revises it with a
	// title, a kind and content but still no status. Both are `pending`, so
	// they are one step; the terminal it opens at 10370 and the failure at
	// 10401 are the next two.
	stepClockExpect(t, "grok.jsonl", []int64{0, 9569, 9849, 10343, 10370, 10401})
}

func TestParallelToolCallsEachBecomeSeparateSteps(t *testing.T) {
	// Three calls are announced, started and completed as interleaved runs.
	// Each call's own status changes are steps, and the thought that follows
	// them is another.
	stepClockExpect(t, "parallel_tool_calls.jsonl", []int64{
		0, 1633, 1678, 1709, 2437, 4601, 4602, 4603, 4613, 4619, 4621, 4623, 5636, 5788,
	})
}

func TestATurnThatEndsLeavesNoStepInFlight(t *testing.T) {
	var clock StepClock
	clock.beginTurnAt(1_000)
	if started := clock.StartedAtMs(); started == nil || *started != 1_000 {
		t.Fatalf("started = %v", started)
	}
	clock.EndTurn()
	if started := clock.StartedAtMs(); started != nil {
		t.Fatalf("started = %d", *started)
	}
}

func TestANewTurnReopensAStepOfTheKindTheLastTurnEndedOn(t *testing.T) {
	var chunk schema.SessionUpdate
	if err := json.Unmarshal([]byte(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}`), &chunk); err != nil {
		t.Fatalf("chunk parses: %v", err)
	}

	var clock StepClock
	clock.beginTurnAt(1_000)
	clock.observeAt(&chunk, 1_100)
	clock.EndTurn()
	clock.beginTurnAt(2_000)
	clock.observeAt(&chunk, 2_100)

	if started := clock.StartedAtMs(); started == nil || *started != 2_100 {
		t.Fatalf("the first message of a new turn opens a step even though the last turn ended on one: %v", started)
	}
}
