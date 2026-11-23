// Port of the tests in mj-store/src/database/startup.rs.

package store

import (
	"database/sql"
	"fmt"
	"os"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func startupTestDelivery(commandID string, groupID *string) StartupDelivery {
	return StartupDelivery{
		SessionID: "session-1",
		CommandID: commandID,
		StepJSON:  fmt.Sprintf(`{"Prompt":{"text":"%s"}}`, commandID),
		Phase:     "pending",
		GroupID:   groupID,
	}
}

func startupTestRemaining(t *testing.T) []string {
	t.Helper()
	connection, err := openReader(DatabasePath())
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	remaining, err := valuesQueryMap(connection, "SELECT command_id FROM startup_steps ORDER BY sequence", nil,
		func(rows *sql.Rows) (string, error) {
			var id string
			err := rows.Scan(&id)
			return id, err
		})
	if err != nil {
		t.Fatal(err)
	}
	return remaining
}

func TestSettledStepsLeaveTheTableOnceNothingWillReadThem(t *testing.T) {
	const child = "MJ_TEST_STARTUP_PRUNING"
	if _, ok := os.LookupEnv(child); !ok {
		root := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(
			"mj_store::database::startup::tests",
			"TestSettledStepsLeaveTheTableOnceNothingWillReadThem",
		)).
			Env(child, "1").
			IsolatedStore(root).
			Run()
		return
	}
	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	group := "group-1"
	if _, err := EnqueueStartupDeliveries([]StartupDelivery{startupTestDelivery("startup-ungrouped", nil)}); err != nil {
		t.Fatal(err)
	}
	if _, err := EnqueueStartupDeliveries([]StartupDelivery{startupTestDelivery("startup-grouped", &group)}); err != nil {
		t.Fatal(err)
	}
	ordinal := uint64(3)
	for _, id := range []string{"startup-ungrouped", "startup-grouped"} {
		if err := SetStartupDeliveryPhase(id, "delivering", nil); err != nil {
			t.Fatal(err)
		}
		if err := SetStartupDeliveryAccepted(id, &ordinal); err != nil {
			t.Fatal(err)
		}
		if err := SetStartupDeliveryPhase(id, "done", nil); err != nil {
			t.Fatal(err)
		}
	}
	if remaining := startupTestRemaining(t); !slices.Equal(remaining, []string{"startup-grouped"}) {
		t.Fatalf("an ungrouped step has no status reader once done; a group keeps its status for the API: %v", remaining)
	}
	if err := DismissStartupGroup("session-1", "group-1"); err != nil {
		t.Fatal(err)
	}
	if remaining := startupTestRemaining(t); len(remaining) != 0 {
		t.Fatalf("a dismissed group is gone: %v", remaining)
	}

	// A store upgraded from a build that kept settled rows is pruned at
	// start.
	connection, err := open(DatabasePath())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := connection.Exec(`INSERT INTO startup_steps(session_id,group_id,command_id,step_json,phase)
                 VALUES ('session-1',NULL,'startup-legacy-done','{}','done'),
                        ('session-1','old','startup-legacy-dismissed','{}','dismissed'),
                        ('session-1','older','startup-superseded-done','{}','done'),
                        ('session-1','old','startup-legacy-failed','{}','failed'),
                        ('session-2','only','startup-latest-done','{}','done')`); err != nil {
		t.Fatal(err)
	}
	_ = connection.Close()
	if removed, err := PruneSettledStartupDeliveries(); err != nil || removed != 3 {
		t.Fatalf("first prune removed %d: %v", removed, err)
	}
	if removed, err := PruneSettledStartupDeliveries(); err != nil || removed != 0 {
		t.Fatalf("second prune removed %d: %v", removed, err)
	}
	if remaining := startupTestRemaining(t); !slices.Equal(remaining, []string{"startup-legacy-failed", "startup-latest-done"}) {
		t.Fatalf("a session's latest API group keeps its status until its client dismisses it; older settled groups are never read again: %v", remaining)
	}
}
