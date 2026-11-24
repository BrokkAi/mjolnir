// Port of the tests in mj-cli/src/dashboard/drains.rs.

package cli

import (
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// drainsTestNotice is the dashboard's notice, nil when none (Rust's
// `Option<String>`).
func drainsTestNotice(dashboard *tui.DashboardState) *string {
	notice, ok := dashboard.Notice()
	if !ok {
		return nil
	}
	return &notice
}

func drainsTestEqualNotice(t *testing.T, got, want *string) {
	t.Helper()
	if (got == nil) != (want == nil) || (got != nil && *got != *want) {
		t.Fatalf("notice %v, want %v", describeOptional(got), describeOptional(want))
	}
}

func describeOptional(value *string) string {
	if value == nil {
		return "None"
	}
	return "Some(" + *value + ")"
}

// Hard-won: #1388: unrelated runtime updates repeatedly restored retained lifecycle warnings.
func TestLifecycleNoticesAreReportedOncePerObservedTransition(t *testing.T) {
	dashboard := tui.NewDashboardState(core.DefaultConfig(), core.NewStateState(), map[string]client.ProfileQuota{})
	notices := chat.NewNotices()
	dashboard.ShareNotices(notices)
	operations := map[string]remoteLifecycleObservation{}
	lifecycle := client.RuntimeLifecycleView{
		OperationID:           "first",
		MoveOperationID:       nil,
		Cancellable:           true,
		SessionID:             "session",
		Kind:                  client.RuntimeLifecycleKindCreate,
		StartedAtEpochSeconds: 1,
		ActiveStages:          nil,
		ResumeDestination:     nil,
		Notice:                pointer("Clone cache preparation failed"),
	}
	observeRemoteLifecycle(operations, dashboard, &lifecycle)
	drainsTestEqualNotice(t, drainsTestNotice(dashboard), lifecycle.Notice)
	dashboard.SetNotice("A later background notice")
	for range 20 {
		observeRemoteLifecycle(operations, dashboard, &lifecycle)
	}
	drainsTestEqualNotice(t, drainsTestNotice(dashboard), pointer("A later background notice"))
	dashboard.ClearNotice()
	observeRemoteLifecycle(operations, dashboard, &lifecycle)
	if drainsTestNotice(dashboard) != nil {
		t.Fatal("dismissed warnings must stay dismissed")
	}
	if history := notices.History(); len(history) != 2 {
		t.Fatalf("notice history %v, want 2 entries", history)
	}

	lifecycle.Notice = pointer("Clone-cache cleanup was skipped")
	observeRemoteLifecycle(operations, dashboard, &lifecycle)
	drainsTestEqualNotice(t, drainsTestNotice(dashboard), lifecycle.Notice)
	dashboard.ClearNotice()
	lifecycle.OperationID = "second"
	if !observeRemoteLifecycle(operations, dashboard, &lifecycle) {
		t.Fatal("a new operation must supersede the observed one")
	}
	drainsTestEqualNotice(t, drainsTestNotice(dashboard), lifecycle.Notice)
	dashboard.ClearNotice()
	lifecycle.Notice = nil
	observeRemoteLifecycle(operations, dashboard, &lifecycle)
	lifecycle.Notice = pointer("Clone cache preparation failed")
	observeRemoteLifecycle(operations, dashboard, &lifecycle)
	drainsTestEqualNotice(t, drainsTestNotice(dashboard), lifecycle.Notice)
}

func TestACreatedWorkspaceOpensWithTheFeedFrameThatAddsIt(t *testing.T) {
	names := func(ids ...string) map[string]string {
		result := map[string]string{}
		for _, id := range ids {
			result[id] = id
		}
		return result
	}
	pending := pointer("created")
	if got := nextWorkspace(pointer("work"), &pending, names("home", "work")); got == nil || *got != "work" {
		t.Fatalf("a frame from before the create keeps the current tab: %v", describeOptional(got))
	}
	if pending == nil || *pending != "created" {
		t.Fatalf("pending %v, want Some(created)", describeOptional(pending))
	}
	if got := nextWorkspace(pointer("work"), &pending, names("created", "work")); got == nil || *got != "created" {
		t.Fatalf("next %v, want Some(created)", describeOptional(got))
	}
	if pending != nil {
		t.Fatalf("pending %v, want None", describeOptional(pending))
	}
	if got := nextWorkspace(pointer("gone"), &pending, names("home", "work")); got == nil || *got != "home" {
		t.Fatalf("a deleted active tab falls back to the first workspace: %v", describeOptional(got))
	}
}

func TestDelayedLifecycleCallbackCannotRemoveANewerOperation(t *testing.T) {
	old := new(atomic.Bool)
	current := new(atomic.Bool)
	operations := map[string]*activeLifecycleOperation{
		"session": {
			retirement:  nil,
			cancelled:   current,
			kind:        tui.SessionOperationKindResuming,
			retryLaunch: nil,
			noticeName:  "session",
		},
	}
	update := dashboardLifecycleUpdate{
		sessionID: "session",
		operation: old,
		success:   daemon.LifecycleSuccessClosed{},
	}
	if completion := takeLifecycleCompletion(operations, &update); !completion.superseded {
		t.Fatal("an old callback must be superseded")
	}
	if operations["session"].cancelled != current {
		t.Fatal("the newer operation must be kept")
	}
	update.operation = current
	if completion := takeLifecycleCompletion(operations, &update); completion.superseded || completion.operation == nil {
		t.Fatal("the current callback must take its operation")
	}
	if len(operations) != 0 {
		t.Fatalf("operations %v, want none", operations)
	}
}
