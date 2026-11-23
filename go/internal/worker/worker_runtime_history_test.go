// Port of the tests in mj-worker/src/worker_runtime/history.rs.

//go:build unix

package worker

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestBlameReadsTheTargetCheckoutIncludingUncommittedLinesAndReportsErrors(t *testing.T) {
	repo := t.TempDir()
	testSupportGit(repo, "init", "-q")
	testSupportGit(repo, "config", "user.name", "Test")
	testSupportGit(repo, "config", "user.email", "test@example.test")
	if err := os.WriteFile(filepath.Join(repo, "a.rs"), []byte("committed\nold\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	testSupportGit(repo, "add", "a.rs")
	testSupportGit(repo, "commit", "-qm", "initial")
	if err := os.WriteFile(filepath.Join(repo, "a.rs"), []byte("committed\nuncommitted\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	evidence, err := captureBlame(ctx, repo, "a.rs", 1, 2)
	if err != nil {
		t.Fatal(err)
	}
	if evidence.RelativePath != "a.rs" {
		t.Fatalf("relative path = %s", evidence.RelativePath)
	}
	if !strings.Contains(evidence.Porcelain, "\tcommitted") {
		t.Fatalf("porcelain = %s", evidence.Porcelain)
	}
	if !strings.Contains(evidence.Porcelain, "0000000000000000000000000000000000000000") {
		t.Fatalf("porcelain = %s", evidence.Porcelain)
	}
	if _, err := captureBlame(ctx, repo, "missing.rs", 1, 1); err == nil {
		t.Fatal("a missing file must fail")
	}
	if _, err := captureBlame(ctx, repo, "a.rs", 9, 10); err == nil {
		t.Fatal("lines past the end must fail")
	}
}

// TestDisconnectedControllerQueriesExpireAndReleaseCapacity: Rust runs this
// on tokio's paused clock and advances it past QUERY_TIMEOUT; here the
// endpoint's timeout is shortened instead.
func TestDisconnectedControllerQueriesExpireAndReleaseCapacity(t *testing.T) {
	endpoint := defaultHistoryEndpoint()
	endpoint.queryTimeout = 2 * time.Second
	query := core.HistoryQueryTraceFile{Path: "a.rs", Limit: 20}
	results := make(chan error, core.MaxPending)
	for range core.MaxPending {
		go func() {
			_, err := endpoint.Query(context.Background(), query, "/")
			results <- err
		}()
	}
	for len(endpoint.Requests()) != core.MaxPending {
		time.Sleep(time.Millisecond)
	}
	_, err := endpoint.Query(context.Background(), query, "/")
	if err == nil || !strings.Contains(err.Error(), "too many") {
		t.Fatalf("err = %v", err)
	}
	for range core.MaxPending {
		err := <-results
		if err == nil || !strings.Contains(err.Error(), "timed out") {
			t.Fatalf("err = %v", err)
		}
	}
	if len(endpoint.Requests()) != 0 {
		t.Fatal("expired queries must leave")
	}
	if len(endpoint.permits) != 0 {
		t.Fatalf("%d permits still held", len(endpoint.permits))
	}
}

func TestHistoryRepliesAreDeliveredAndCancelledRequestsDoNotAccumulate(t *testing.T) {
	endpoint := defaultHistoryEndpoint()
	request := core.HistoryQuerySearchSessions{Query: "needle", Limit: 20}
	type outcome struct {
		result core.HistoryResult
		err    error
	}
	pending := make(chan outcome, 1)
	go func() {
		result, err := endpoint.Query(context.Background(), request, "/")
		pending <- outcome{result, err}
	}()
	var requests []core.HistoryRequest
	for {
		requests = endpoint.Requests()
		if len(requests) != 0 {
			break
		}
		time.Sleep(time.Millisecond)
	}
	value, err := json.Marshal(map[string]any{"text": strings.Repeat("x", 100_000)})
	if err != nil {
		t.Fatal(err)
	}
	endpoint.Complete(core.HistoryResult{RequestID: requests[0].RequestID, Value: value, IsError: false})
	answered := <-pending
	if answered.err != nil {
		t.Fatal(answered.err)
	}
	var decoded map[string]string
	if err := json.Unmarshal(answered.result.Value, &decoded); err != nil {
		t.Fatal(err)
	}
	if len(decoded["text"]) != 100_000 {
		t.Fatalf("text length = %d", len(decoded["text"]))
	}
	if len(endpoint.Requests()) != 0 {
		t.Fatal("an answered query must leave")
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancelled := make(chan error, 1)
	go func() {
		_, err := endpoint.Query(ctx, core.HistoryQueryTraceFile{Path: "a.rs", Limit: 20}, "/")
		cancelled <- err
	}()
	for len(endpoint.Requests()) == 0 {
		time.Sleep(time.Millisecond)
	}
	cancel()
	if err := <-cancelled; err == nil {
		t.Fatal("the cancelled query must end")
	}
	if len(endpoint.Requests()) != 0 {
		t.Fatal("a cancelled query must leave")
	}
}
