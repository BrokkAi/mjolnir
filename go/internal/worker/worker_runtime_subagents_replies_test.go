// Port of the tests in mj-worker/src/worker_runtime/subagents/replies.rs.

//go:build unix

package worker

import (
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestReplyRetirementRequiresBothReceiptsAndSurvivesReopen(t *testing.T) {
	for _, callerFirst := range []bool{false, true} {
		directory := t.TempDir()
		endpoint, err := SubagentEndpointOpen(directory)
		subagentsMust(t, err)
		request := core.SubagentToolRequest{RequestID: "durable-operation", CreatedAtMs: 1, Action: core.SubagentToolActionListAgents{}}
		_, err = endpoint.enqueue(request)
		subagentsMust(t, err)
		result := core.SubagentToolResult{
			RequestID:     request.RequestID,
			CompletedAtMs: 2,
			IsError:       false,
			Message:       strings.Repeat("reply", 20_000),
		}
		ids := []string{request.RequestID}
		// A pending-tool status is not receipt of its eventual reply.
		subagentsMust(t, endpoint.acknowledgeReplies(ids, true))
		_, err = endpoint.Complete(result)
		subagentsMust(t, err)
		subagentsMust(t, endpoint.acknowledgeReplies(ids, callerFirst))
		endpoint, err = SubagentEndpointOpen(directory)
		subagentsMust(t, err)
		cached, err := endpoint.enqueue(request)
		subagentsMust(t, err)
		if cached == nil || *cached != result {
			t.Fatalf("cached = %v", cached)
		}
		duplicate := result
		duplicate.Message = "duplicate must not replace the reply"
		_, err = endpoint.Complete(duplicate)
		subagentsMust(t, err)
		cached, err = endpoint.enqueue(request)
		subagentsMust(t, err)
		if cached == nil || *cached != result {
			t.Fatalf("cached = %v", cached)
		}
		subagentsMust(t, endpoint.acknowledgeReplies(ids, !callerFirst))
		if _, err := os.Stat(endpoint.resultPath(request.RequestID)); err == nil {
			t.Fatal("an acknowledged reply body is retired")
		}
		endpoint, err = SubagentEndpointOpen(directory)
		subagentsMust(t, err)
		retired, err := endpoint.enqueue(request)
		subagentsMust(t, err)
		if retired == nil || !strings.Contains(retired.Message, "already completed") {
			t.Fatalf("retired = %v", retired)
		}
		if queued, _ := endpoint.snapshot(); len(queued) != 0 {
			t.Fatal("a retired reply must never requeue the operation")
		}
	}
}
