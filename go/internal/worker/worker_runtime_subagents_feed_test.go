// Port of the tests in mj-worker/src/worker_runtime/subagents/feed.rs.

//go:build unix

package worker

import (
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestQueueChangesAreByteBoundedAndCompletionDuringPagingIsNotLost(t *testing.T) {
	directory := t.TempDir()
	endpoint, err := SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	for i := range 12 {
		_, err := endpoint.enqueue(core.SubagentToolRequest{
			RequestID:   fmt.Sprintf("request-%02d", i),
			CreatedAtMs: int64(i),
			Action:      core.SubagentToolActionSendMessage{ChildSessionID: "child", Message: strings.Repeat("λ", 450_000)},
		})
		subagentsMust(t, err)
	}
	// Preparation can expose no endpoint before opening a persisted queue.
	// Its initial generation must not be mistaken for an observed empty one.
	endpoint, err = SubagentEndpointOpen(directory)
	subagentsMust(t, err)
	feed := defaultSubagentFeed()
	preparing, err := feed.Poll(nil, nil)
	subagentsMust(t, err)
	first, err := feed.Poll(endpoint, &preparing.Sequence)
	subagentsMust(t, err)
	if !first.HasMore {
		t.Fatal("the first page must leave more")
	}
	again, err := feed.Poll(endpoint, nil)
	subagentsMust(t, err)
	if !feedPagesEqual(first, again) {
		t.Fatal("an unacknowledged page is sent again unchanged")
	}
	firstID := first.Changes[0].RequestID()
	_, err = endpoint.Complete(core.SubagentToolResult{
		RequestID:     firstID,
		CompletedAtMs: 20,
		IsError:       false,
		Message:       "completed while its request page was in flight",
	})
	subagentsMust(t, err)
	seen := map[string]core.SubagentQueueChange{}
	page := first
	for {
		encoded, err := workerRuntimeMarshalJSON(page)
		subagentsMust(t, err)
		if len(encoded) >= core.RelayReplayByteBudget {
			t.Fatalf("page of %d bytes", len(encoded))
		}
		for _, change := range page.Changes {
			seen[change.RequestID()] = change
		}
		more := page.HasMore
		sequence := page.Sequence
		page, err = feed.Poll(endpoint, &sequence)
		subagentsMust(t, err)
		if !more {
			break
		}
	}
	if len(seen) != 12 {
		t.Fatalf("seen %d changes", len(seen))
	}
	if _, ok := seen[firstID].(core.SubagentQueueChangeCompleted); !ok {
		t.Fatalf("%s = %#v", firstID, seen[firstID])
	}
	if len(page.Changes) != 0 {
		t.Fatalf("changes = %v", page.Changes)
	}
}

// feedPagesEqual is SubagentQueuePage equality.
func feedPagesEqual(a, b core.SubagentQueuePage) bool {
	left, err := workerRuntimeMarshalJSON(a)
	if err != nil {
		return false
	}
	right, err := workerRuntimeMarshalJSON(b)
	return err == nil && string(left) == string(right)
}
