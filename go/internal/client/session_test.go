// Port of the storage_tests in mj-client/src/session.rs.

package client

import (
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestHistorySearchYieldsUntilBackendRespondsAndPropagatesFailures(t *testing.T) {
	history := core.NewUnbounded[historyTestRequest]()
	view := core.NewWatch(ManagedSessionView{})
	session := NewSessionHandle(&replacementTestSession{
		history:         history,
		sessionID:       "session",
		stopped:         false,
		acceptedOrdinal: 0,
		view:            view,
		seen:            view.Changed(),
	})
	type searchResult struct {
		page core.PromptHistoryPage
		err  error
	}
	search := func(query string) <-chan searchResult {
		result := make(chan searchResult, 1)
		go func() {
			page, err := session.SearchPrompts(context.Background(), "bundle", core.HistoryScopeProject, query, nil, 8)
			result <- searchResult{page: page, err: err}
		}()
		return result
	}
	// receive takes the backend's next request, failing if the search
	// finished before storage replied.
	receive := func(searching <-chan searchResult, what string) historyTestRequest {
		t.Helper()
		requests := make(chan historyTestRequest, 1)
		go func() {
			request, _ := history.Recv(context.Background())
			requests <- request
		}()
		select {
		case result := <-searching:
			t.Fatalf("%s completed before storage replied: %+v", what, result)
		case request := <-requests:
			return request
		}
		panic("unreachable")
	}

	searching := search("needle")
	request := receive(searching, "search")
	if request.bundleID != "bundle" || request.scope != core.HistoryScopeProject || request.query != "needle" {
		t.Fatalf("request %+v", request)
	}
	request.response <- historyTestResponse{err: errors.New("storage unavailable")}
	if result := <-searching; result.err == nil || !strings.Contains(result.err.Error(), "storage unavailable") {
		t.Fatalf("result %+v", result)
	}

	searching = search("retry")
	request = receive(searching, "retry")
	request.response <- historyTestResponse{page: core.PromptHistoryPage{
		Entries: []core.PromptHistoryEntry{{ID: 1, SessionID: "session", Text: "retry works"}},
		Next:    nil,
	}}
	result := <-searching
	if result.err != nil {
		t.Fatal(result.err)
	}
	if got := result.page.Entries[0].Text; got != "retry works" {
		t.Fatalf("text %q", got)
	}
}
