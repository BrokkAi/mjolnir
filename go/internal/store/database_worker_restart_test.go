// Port of the tests in mj-store/src/database/worker_restart.rs.

package store

import (
	"context"
	"os"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func TestWorkerRestartAndSessionRestartGateOnlySameEnvironmentPeers(t *testing.T) {
	const child = "MJ_ISOLATED_ENVIRONMENT_PEER_GATES"
	if _, ok := os.LookupEnv(child); !ok {
		root := t.TempDir()
		targets.NewIsolatedTest(targets.TestName(
			"mj_store::database::worker_restart::tests",
			"TestWorkerRestartAndSessionRestartGateOnlySameEnvironmentPeers",
		)).
			Env(child, "1").
			IsolatedStore(root).
			Run()
		return
	}

	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	gate := core.NewRecoveryGate()
	parent := session("parent-session", "project")
	parent.State = core.SessionStateRunning
	if err := SaveSession(&parent); err != nil {
		t.Fatal(err)
	}
	owned := session("owned-session", "project")
	owned.State = core.SessionStateParked
	ownedRelation := testSubagent(parent.ID, owned.ID)
	ownedRelation.Orchestrator = true
	if err := RegisterSubagentWithStartup(&owned, &ownedRelation, []StartupDelivery{}, nil, nil, true); err != nil {
		t.Fatal(err)
	}
	borrowed := session("borrowed", "project")
	borrowed.State = core.SessionStateParked
	borrowedRelation := testSubagent(parent.ID, borrowed.ID)
	if err := SaveSubagentSession(&borrowed, &borrowedRelation); err != nil {
		t.Fatal(err)
	}
	nested := session("nested", "project")
	nested.State = core.SessionStateParked
	// The owned session has a separate set of borrowers.
	nestedRelation := testSubagent(owned.ID, nested.ID)
	if err := SaveSubagentSession(&nested, &nestedRelation); err != nil {
		t.Fatal(err)
	}

	beginUpgrade := func(record *core.SessionRecord) {
		t.Helper()
		plan := targets.ContainerUpgradeTestPlan(record.ID)
		if _, err := BeginContainerUpgrade(record.ID, "upgrade-test", new(atomic.Bool), plan, nil); err != nil {
			t.Fatal(err)
		}
	}
	commitUpgrade := func(record *core.SessionRecord) bool {
		t.Helper()
		commit, err := TryCommitContainerUpgrade(record.ID, "upgrade-test")
		if err != nil {
			t.Fatal(err)
		}
		return commit.IsCommitted()
	}
	restart := func(record *core.SessionRecord, unpark bool) error {
		t.Helper()
		attempt, refusal := gate.TryStart(record.ID, core.BackgroundWorkWorkerReplacement)
		if refusal != nil {
			t.Fatalf("worker replacement refused for %s: %+v", record.ID, refusal)
		}
		defer attempt.Release()
		permit, err := WorkerPermitAcquire(context.Background(), record.ID, attempt)
		if err != nil {
			t.Fatal(err)
		}
		defer permit.Release()
		intent := WorkerRestartIntent{
			OperationID:  "test-restart",
			Target:       record.Target,
			DesiredBuild: "fixture",
		}
		ctx := permit.ScopeBlocking(context.Background())
		if unpark {
			if err := BeginSubagentUnpark(ctx, record.ID, &intent, nil); err != nil {
				return err
			}
		} else if err := BeginWorkerRestart(ctx, record.ID, &intent); err != nil {
			return err
		}
		return FinishWorkerRestart(ctx, record.ID, intent.OperationID)
	}
	mustRestart := func(record *core.SessionRecord, unpark bool) {
		t.Helper()
		if err := restart(record, unpark); err != nil {
			t.Fatalf("restart %s (unpark %v): %v", record.ID, unpark, err)
		}
	}
	refusedRestart := func(record *core.SessionRecord, unpark bool) {
		t.Helper()
		if err := restart(record, unpark); err == nil || !strings.Contains(err.Error(), "being upgraded") {
			t.Fatalf("restart %s (unpark %v) must wait for the upgrade: %v", record.ID, unpark, err)
		}
	}
	ownsTarget := func(record *core.SessionRecord) bool {
		t.Helper()
		owned, err := ContainerUpgradeOwnsTarget(record.ID)
		if err != nil {
			t.Fatal(err)
		}
		return owned
	}

	beginUpgrade(&parent)
	if !ownsTarget(&borrowed) {
		t.Fatal("the parent's upgrade must own the borrowed child's target")
	}
	if ownsTarget(&owned) {
		t.Fatal("the parent's upgrade must not own the owned environment")
	}
	// A parked worker may wake before commit. The resulting running peer
	// prevents the upgrade from crossing into recreation.
	mustRestart(&borrowed, true)
	if commitUpgrade(&parent) {
		t.Fatal("a running peer must hold the upgrade back")
	}
	if err := SaveSession(&borrowed); err != nil {
		t.Fatal(err)
	}
	if !commitUpgrade(&parent) {
		t.Fatal("the upgrade must commit once its peers are parked")
	}
	mustRestart(&parent, false)
	refusedRestart(&borrowed, true)
	refusedRestart(&borrowed, false)
	mustRestart(&owned, false)
	mustRestart(&owned, true)
	newChild := session("new-child", "project")
	newChild.Target = nil
	relation := testSubagent(parent.ID, newChild.ID)
	if err := SaveSubagentSession(&newChild, &relation); err == nil || !strings.Contains(err.Error(), "being upgraded") {
		t.Fatalf("a new borrower must wait for the upgrade: %v", err)
	}
	if err := RegisterSubagentWithStartup(&newChild, &relation, []StartupDelivery{}, nil, nil, true); err != nil {
		t.Fatal(err)
	}
	if err := FinishSessionRestart(parent.ID, "upgrade-test"); err != nil {
		t.Fatal(err)
	}

	beginUpgrade(&owned)
	if !commitUpgrade(&owned) {
		t.Fatal("the owned environment's upgrade must commit")
	}
	refusedRestart(&nested, true)
	mustRestart(&parent, false)
	mustRestart(&borrowed, true)
	newBorrower := session("new-borrower", "project")
	newBorrower.Target = nil
	newBorrowerRelation := testSubagent(parent.ID, newBorrower.ID)
	if err := SaveSubagentSession(&newBorrower, &newBorrowerRelation); err != nil {
		t.Fatal(err)
	}
}
