// Port of mj-store/src/database/test_fixtures.rs.
//
// Records that tests of the store, and of the code above it, start from.
// Rust keeps this module public (doc-hidden) so the crates above the store
// can build their fixtures from it; Go test files cannot be imported by
// other packages, so it is an ordinary file here too.
//
// Rust finds the operation's worker permit in a task-local or thread-local
// (worker_lifecycle::current). Go has no task-locals, so the functions that
// consult or enter that scope take the context that carries it, and the work
// they run receives the context of the scope it runs in.

package store

import (
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
	"sync"
	"sync/atomic"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// EventDigest is a valid relay event digest derived from value.
func EventDigest(value uint64) string {
	return fmt.Sprintf("%064x", value)
}

// MaterializedSession is a running session's projection with one item of
// each transcript kind, a queued prompt and a pending elicitation.
func MaterializedSession(sessionID string) core.MaterializedSession {
	lastActivityAtMs := int64(1_500)
	sessionTitle := "Relay refactor"
	agentContentOrdinal := uint64(2)
	elicitationTitle := "Question"
	// The JSON values are written as serde_json writes json!(..) values:
	// compact, with object keys in sorted order.
	return core.MaterializedSession{
		SessionID:           sessionID,
		AppliedEventOrdinal: 7,
		AppliedEventDigest:  EventDigest(7),
		LastActivityAtMs:    &lastActivityAtMs,
		Execution:           core.MaterializedExecutionStateRunning{StartedAtMs: 1_000},
		SessionTitle:        &sessionTitle,
		Configuration: core.SessionConfiguration{
			Values: map[string]any{
				"model":  "gpt-5.6-sol",
				"effort": "high",
			},
		},
		Transcript: []*core.TranscriptItem{
			{
				StableID:        "user:1",
				Position:        1,
				CreatedAtMs:     1_000,
				LastChangedAtMs: 1_000,
				Body: core.TranscriptBodyUser{
					Content: []json.RawMessage{json.RawMessage(`{"text":"build it","type":"text"}`)},
				},
			},
			{
				StableID:                  "agent:2",
				Position:                  2,
				LatestContentEventOrdinal: &agentContentOrdinal,
				CreatedAtMs:               1_100,
				LastChangedAtMs:           1_300,
				Body: core.TranscriptBodyAgent{
					Chunks: []json.RawMessage{json.RawMessage(
						`{"_meta":{"provider":"test"},"content":{"text":"Working on it","type":"text"},"messageId":"answer-1"}`,
					)},
					Streaming: false,
				},
			},
			{
				StableID:        "tool:call-1",
				Position:        3,
				CreatedAtMs:     1_200,
				LastChangedAtMs: 1_400,
				Body: core.TranscriptBodyTool{
					Call: core.OpaqueToolCallFromValue(json.RawMessage(
						`{"_meta":{"provider":"test"},"content":[{"content":{"text":"done","type":"text"},"type":"content"}],` +
							`"kind":"edit","locations":[{"line":4,"path":"src/main.rs"}],"rawInput":{"path":"src/main.rs"},` +
							`"rawOutput":{"changed":true},"status":"completed","title":"Edit files","toolCallId":"call-1"}`,
					)),
					TerminalOutputs: []core.TerminalOutputRecord{},
					TerminalRefs:    []string{},
				},
			},
			{
				StableID:        "plan:1",
				Position:        4,
				CreatedAtMs:     1_250,
				LastChangedAtMs: 1_350,
				Body: core.TranscriptBodyPlan{
					Plan: json.RawMessage(
						`{"_meta":{"planProvider":"test"},"entries":[{"_meta":{"provider":"test"},` +
							`"content":"Implement relay","priority":"high","status":"in_progress"}]}`,
					),
				},
			},
		},
		QueuedPrompts: []core.MaterializedQueuedPrompt{{
			CommandID:  "prompt-2",
			Kind:       core.QueuedCommandKindPrompt{},
			Content:    []any{map[string]any{"type": "text", "text": "then test"}},
			QueuedAtMs: 1_500,
		}},
		PendingElicitations: []core.ElicitationRequest{{
			ID:      "elicitation-1",
			Message: "Choose one",
			Title:   &elicitationTitle,
			Fields:  []core.ElicitationField{},
		}},
	}
}

// CheckpointTestSession is a running Podman session with no target or
// checkpoint yet.
func CheckpointTestSession(sessionID string) core.SessionRecord {
	nativeSessionID := "native-session"
	return core.SessionRecord{
		Oracle:           true,
		WorkspaceID:      core.DefaultWorkspaceID,
		ID:               sessionID,
		Title:            "checkpoint transition",
		HarnessKind:      core.HarnessKindCodex,
		LastProfile:      "codex",
		BundleID:         "project",
		TargetTemplateID: "podman",
		AdditionalMounts: []core.AdditionalMount{},
		State:            core.SessionStateRunning,
		NativeSessionID:  &nativeSessionID,
		CreatedAt:        "2026-08-12T00:00:00Z",
		UpdatedAt:        "2026-08-12T00:00:00Z",
	}
}

// LocalBundle is a bundle of one local repository.
func LocalBundle(repository string) core.ProjectBundle {
	local := repository
	return core.ProjectBundle{
		PrimaryRepo: "project",
		Repositories: []core.ProjectRepository{{
			ID:          "project",
			Local:       &local,
			Destination: "project",
		}},
	}
}

// workerTestGates holds the recovery gate of each data directory
// (Rust's GATES static in worker_test_gate).
var workerTestGates struct {
	sync.Mutex
	gates map[string]*core.RecoveryGate
}

// WorkerTestGate is the recovery gate tests share for one data directory.
func WorkerTestGate() *core.RecoveryGate {
	workerTestGates.Lock()
	defer workerTestGates.Unlock()
	if workerTestGates.gates == nil {
		workerTestGates.gates = map[string]*core.RecoveryGate{}
	}
	directory := core.DataDir()
	gate, ok := workerTestGates.gates[directory]
	if !ok {
		gate = core.NewRecoveryGate()
		workerTestGates.gates[directory] = gate
	}
	return gate
}

// WorkerTestPermit is a worker permit for a test that stages or starts a
// worker without a daemon: the operation's own permit when ctx carries one,
// otherwise a worker-replacement admission from WorkerTestGate. nil when the
// gate refuses that admission. reason is unused, as in Rust.
func WorkerTestPermit(ctx context.Context, id string, reason string) (*WorkerPermit, error) {
	if owner := Current(ctx, id); owner != nil {
		return owner, nil
	}
	admission, refusal := WorkerTestGate().TryStart(id, core.BackgroundWorkWorkerReplacement)
	if refusal != nil {
		return nil, nil
	}
	// The permit holds its own handle on the admission; Rust drops this one
	// when the function returns.
	defer admission.Release()
	return WorkerPermitAcquire(ctx, id, admission)
}

// TestGit runs git in directory and returns its trimmed standard output. It
// panics when git fails, naming the command and git's standard error.
func TestGit(directory string, args []string) string {
	output, err := core.Capture(context.Background(), exec.Command("git", append([]string{"-C", directory}, args...)...))
	if err != nil {
		panic(err)
	}
	if !output.Status.Success() {
		panic(fmt.Sprintf("git %s failed: %s", strings.Join(args, " "), strings.ToValidUTF8(string(output.Stderr), "�")))
	}
	if !utf8.Valid(output.Stdout) {
		panic(fmt.Sprintf("git %s printed invalid UTF-8", strings.Join(args, " ")))
	}
	return strings.TrimSpace(string(output.Stdout))
}

// SessionAdmission is the explicit owner capability for controller fixtures
// that run without a daemon: a foreground admission from a fresh gate.
func SessionAdmission(sessionID string) core.SessionAdmission {
	gate := core.NewRecoveryGate()
	admission, err := gate.EnterForeground(context.Background(), sessionID, "test lifecycle", core.LifecycleOriginPerson, new(atomic.Bool), core.ProcessExecutor{})
	if err != nil {
		panic(err)
	}
	// Rust moves the admission into the returned value; the new handle
	// replaces this one.
	defer admission.Release()
	return admission.Admission()
}

// RelayAdmission is SessionAdmission holding the session's relay lease.
func RelayAdmission(sessionID string) *core.RelayLeaseAdmission {
	admission := SessionAdmission(sessionID)
	defer admission.Release()
	lease, err := admission.Lease(context.Background(), sessionID)
	if err != nil {
		panic(err)
	}
	return lease
}

// WorkerTestScope enters a fixture's operation exactly as the daemon registry
// does: inside the operation that ctx already carries for id, or else under a
// new foreground admission from WorkerTestGate. work receives the context of
// that scope.
func WorkerTestScope[T any](ctx context.Context, id string, work func(ctx context.Context) T) T {
	if owner := Current(ctx, id); owner != nil {
		return work(owner.ScopeBlocking(ctx))
	}
	permit, release := workerTestEnter(ctx, id)
	defer release()
	return work(permit.ScopeBlocking(ctx))
}

// workerTestEnter takes a foreground admission for id from WorkerTestGate
// and derives a worker permit from it. release ends both, as Rust drops the
// admission and the permit once the scoped work returns.
func workerTestEnter(ctx context.Context, id string) (*WorkerPermit, func()) {
	admission, err := WorkerTestGate().EnterForeground(
		ctx,
		id,
		"test lifecycle",
		core.LifecycleOriginPerson,
		new(atomic.Bool),
		core.ProcessExecutor{},
	)
	if err != nil {
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
	permit, err := WorkerPermitAcquire(ctx, id, admission)
	if err != nil {
		admission.Release()
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
	return permit, func() {
		permit.Admission().Release()
		admission.Release()
	}
}

// WorkerTestScopeAsync is WorkerTestScope for asynchronous work (Rust's
// worker_test_scope_async; in Go both run work on the calling goroutine).
func WorkerTestScopeAsync[T any](ctx context.Context, id string, work func(ctx context.Context) T) T {
	if owner := Current(ctx, id); owner != nil {
		return work(owner.Scope(ctx))
	}
	permit, release := workerTestEnter(ctx, id)
	defer release()
	return work(permit.Scope(ctx))
}

// WorkerTestRunBlocking runs work as worker_lifecycle's RunBlocking does,
// inside WorkerTestScope for id.
func WorkerTestRunBlocking[T any](ctx context.Context, id string, reason string, executor core.CommandExecutor, work func(ctx context.Context) (T, error)) (T, error) {
	type outcome struct {
		value T
		err   error
	}
	result := WorkerTestScope(ctx, id, func(ctx context.Context) outcome {
		value, err := RunBlocking(ctx, id, reason, executor, work)
		return outcome{value, err}
	})
	return result.value, result.err
}
