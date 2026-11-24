//go:build unix

// Port of the `#[cfg(all(test, unix))] mod tests` in
// mj-worker-launch/src/worker_launch.rs.

package workerlaunch

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/workerstaging"
)

func workerLaunchMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func TestOwnedChildLaunchUsesItsCheckoutAndReportWithoutParentPlacement(t *testing.T) {
	const test = "TestOwnedChildLaunchUsesItsCheckoutAndReportWithoutParentPlacement"
	const marker = "MJ_TEST_OWNED_CHILD_LAUNCH"
	if _, ok := os.LookupEnv(marker); !ok {
		dir := t.TempDir()
		targets.NewIsolatedTest(targets.TestName("worker_launch::tests", test)).
			Env(marker, "1").
			IsolatedStore(dir).
			Run()
		return
	}
	writer := store.InstallIsolatedTestWriter()
	defer writer.Shutdown()
	config := core.DefaultConfig()
	config.Targets["podman"] = core.TargetTemplateLocalBare{}
	config.Profiles["codex"] = core.NewHarnessProfile(
		core.HarnessKindCodex,
		filepath.Join(core.DataDir(), "profile"),
	)
	choice := core.SubagentChoice{Model: "test-model", Effort: nil}
	config.Review.Reviewers = []core.SubagentChoice{choice}
	parentID := "11111111111111111111111111111111"
	childID := "22222222222222222222222222222222"
	parent := store.CheckpointTestSession(parentID)
	parent.LastProfile = "missing-profile"
	workerLaunchMust(t, store.SaveSession(&parent))
	child := store.CheckpointTestSession(childID)
	child.Oracle = true
	oracleChoice := choice
	child.OracleChoice = &oracleChoice
	var review core.SessionReview = core.SessionReviewPush{Reviewers: nil}
	child.Review = &review
	child.Subagents = &core.SubagentPolicy{
		Mode:            core.SubagentPolicyMjolnir,
		Orchestrators:   false,
		Straightforward: nil,
		Subtle:          nil,
	}
	workspace := filepath.Join(core.DataDir(), "child-workspace")
	child.ProjectDirectory = &workspace
	child.Target = core.TargetLocatorLocalBare{WorkerRoot: filepath.Join(core.DataDir(), childID)}
	relation := core.SubagentRecord{
		ChildSessionID:   childID,
		ParentSessionID:  parentID,
		TaskName:         "isolated assignment",
		ProfileID:        "codex",
		WorkingDirectory: "relative-to-parent",
		RequestKey:       "owned-child-launch",
		CreatedAt:        child.CreatedAt,
		Kind:             core.SubagentKindWorker,
		ReviewerLaunch:   nil,
		Model:            nil,
		Effort:           nil,
		InitialPrompt:    "inspect",
		NoticedTurn:      nil,
		ReportedFinish:   nil,
		HandbackTool:     true,
		Orchestrator:     true,
		ResumeWithParent: false,
	}
	workerLaunchMust(t, store.RegisterSubagentWithStartup(&child, &relation, nil, nil, nil, true))
	parent.State = core.SessionStateStopped
	workerLaunchMust(t, store.SaveLifecycleSession(&parent))
	report := filepath.Join(workspace, ".mj/agents", childID)
	workerLaunchMust(t, store.RecordSubagentReportDir(childID, report))
	state, err := store.LoadState()
	workerLaunchMust(t, err)
	backend, _, err := WorkerPlacement(context.Background(), &config, &state, childID)
	workerLaunchMust(t, err)
	launch, _, _, err := SessionLaunchConfig(&config, &state, childID, &backend, workerstaging.LaunchHistoryStored{})
	workerLaunchMust(t, err)
	if launch.Cwd != workspace {
		t.Fatalf("cwd = %q, want %q", launch.Cwd, workspace)
	}
	if got := launch.Environment[core.ChildReportDirEnv]; got != report {
		t.Fatalf("%s = %q, want %q", core.ChildReportDirEnv, got, report)
	}
	if !launch.Oracle {
		t.Fatal("the owning child has no oracle")
	}
	if !(launch.ReviewTools() && launch.ReviewGate() && launch.PreparesReview()) {
		t.Fatalf("review tools %v, gate %v, prepares %v",
			launch.ReviewTools(), launch.ReviewGate(), launch.PreparesReview())
	}
	if role, ok := launch.SubagentMCPRole(); !ok || role != core.SubagentMcpRoleOrchestrator {
		t.Fatalf("sub-agent MCP role = %q, %v; want orchestrator", role, ok)
	}
	saved, err := json.Marshal(&launch)
	workerLaunchMust(t, err)
	var fields map[string]json.RawMessage
	workerLaunchMust(t, json.Unmarshal(saved, &fields))
	if _, ok := fields["owns_environment"]; ok {
		t.Fatalf("launch saved owns_environment: %s", saved)
	}
	var restored core.WorkerLaunchConfig
	workerLaunchMust(t, json.Unmarshal(saved, &restored))
	if !(restored.Oracle && restored.ReviewGate()) {
		t.Fatalf("restored oracle %v, review gate %v", restored.Oracle, restored.ReviewGate())
	}
	if restored.SubagentKind == nil || *restored.SubagentKind != core.SubagentKindWorker {
		t.Fatalf("restored sub-agent kind = %v, want worker", restored.SubagentKind)
	}
}
