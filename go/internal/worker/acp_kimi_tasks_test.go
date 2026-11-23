// Port of mj-worker/src/acp/kimi_tasks.rs: its test-only items and tests.

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// FullScan parses the complete main-agent wire stream.
func FullScan(wirePath string) (KimiTaskSnapshot, error) {
	bytes, err := os.ReadFile(wirePath)
	if err != nil {
		return KimiTaskSnapshot{}, fmt.Errorf("read Kimi wire stream %s: %w", wirePath, err)
	}
	tracker, _, err := parseComplete(bytes, wirePath)
	if err != nil {
		return KimiTaskSnapshot{}, err
	}
	return tracker.snapshot(), nil
}

// parseComplete parses every complete line of bytes; it returns the tracker
// and the number of lines read.
func parseComplete(bytes []byte, path string) (TaskTracker, uint64, error) {
	var tracker TaskTracker
	var lineCount uint64
	lines, err := completeLines(bytes, path)
	if err != nil {
		return TaskTracker{}, 0, err
	}
	for _, line := range lines {
		if err := parseLine(&tracker, line.line, line.number, path); err != nil {
			return TaskTracker{}, 0, err
		}
		lineCount = line.number + 1
	}
	return tracker, lineCount, nil
}

func kimiSessionFixture(t *testing.T) (string, string, string) {
	temp := t.TempDir()
	sessionID := "session-test"
	sessionDir := filepath.Join(temp, "sessions/workspace", sessionID)
	if err := os.MkdirAll(filepath.Join(sessionDir, "agents/main"), 0o755); err != nil {
		t.Fatal(err)
	}
	kimiWriteJSON(t, filepath.Join(sessionDir, stateFile), map[string]any{"version": 2, "id": sessionID})
	return temp, sessionID, sessionDir
}

func kimiWriteJSON(t *testing.T, path string, value any) {
	t.Helper()
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, encoded, 0o644); err != nil {
		t.Fatal(err)
	}
}

func kimiWriteIndex(t *testing.T, home string, rows []any) {
	t.Helper()
	contents := make([]string, len(rows))
	for i, row := range rows {
		encoded, err := jsonvalue.Marshal(row)
		if err != nil {
			t.Fatal(err)
		}
		contents[i] = string(encoded)
	}
	if err := os.WriteFile(filepath.Join(home, sessionIndexFile), []byte(strings.Join(contents, "\n")), 0o644); err != nil {
		t.Fatal(err)
	}
}

func kimiLifecycle(t *testing.T, eventType, taskID string, parentToolCallID *string, detached bool, kind string) jsonvalue.Object {
	status := "completed"
	if strings.HasSuffix(eventType, "started") {
		status = "running"
	}
	info := jsonvalue.Object{
		{Key: "taskId", Value: taskID},
		{Key: "description", Value: "background work"},
		{Key: "status", Value: status},
		{Key: "detached", Value: detached},
		{Key: "startedAt", Value: int64(1234)},
		{Key: "endedAt", Value: nil},
		{Key: "kind", Value: kind},
		{Key: "agentId", Value: "main"},
	}
	if parentToolCallID != nil {
		info.Set("parentToolCallId", *parentToolCallID)
	}
	return jsonvalue.Object{{Key: "type", Value: eventType}, {Key: "agentId", Value: "main"}, {Key: "info", Value: info}}
}

func kimiRecordText(t *testing.T, record any) string {
	t.Helper()
	encoded, err := jsonvalue.Marshal(record)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func kimiAppendJSONL(t *testing.T, path string, records []any) {
	t.Helper()
	lines := make([]string, len(records))
	for i, record := range records {
		lines[i] = kimiRecordText(t, record)
	}
	if err := os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
}

// kimiAppendLines appends each record as its own line.
func kimiAppendLines(t *testing.T, path string, records []any) {
	t.Helper()
	file, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	for _, record := range records {
		if _, err := file.WriteString(kimiRecordText(t, record) + "\n"); err != nil {
			t.Fatal(err)
		}
	}
}

func kimiOpen(t *testing.T, path string) *KimiWireFollower {
	t.Helper()
	follower, err := KimiWireFollowerOpen(path)
	if err != nil {
		t.Fatal(err)
	}
	return follower
}

func kimiRefresh(t *testing.T, follower *KimiWireFollower) KimiWireRefresh {
	t.Helper()
	refresh, err := follower.Refresh()
	if err != nil {
		t.Fatal(err)
	}
	return refresh
}

func kimiSettled(t *testing.T, follower *KimiWireFollower, since int64, want bool) {
	t.Helper()
	settled, known := follower.TurnSettledSince(since)
	if !known || settled != want {
		t.Fatalf("turn settled since %d: %v (known %v), want %v", since, settled, known, want)
	}
}

// Records captured from a real Kimi session wire stream.
//
// Hard-won: 59191cd: Kimi usage stayed empty because the bridge emits turn usage in journal records.
func TestTurnUsageSumsTheTurnScopedRecordsKimiJournalled(t *testing.T) {
	path := filepath.Join(t.TempDir(), "wire.jsonl")
	kimiAppendJSONL(t, path, []any{
		acpFeaturesJSON(t, `{"type":"turn.started","agentId":"main","turnId":0,"time":1787697600000}`),
		acpFeaturesJSON(t, `{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":12504,"output":416,"inputCacheRead":11264,"inputCacheCreation":0},"usageScope":"turn","time":1787697646392}`),
		// A second model request in the same turn.
		acpFeaturesJSON(t, `{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":717,"output":25,"inputCacheRead":23552,"inputCacheCreation":128},"usageScope":"turn","time":1787697675995}`),
		// Out-of-turn work Kimi does for itself is not this turn's spend.
		acpFeaturesJSON(t, `{"type":"usage.record","model":"kimi-code/k2","usage":{"inputOther":900,"output":50,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"session","time":1787697675996}`),
		acpFeaturesJSON(t, `{"type":"turn.ended","agentId":"main","turnId":0,"reason":"completed"}`),
	})
	follower := kimiOpen(t, path)
	kimiSettled(t, follower, 1787697600000, true)
	usage := follower.TurnUsageSince(1787697600000)
	if usage == nil {
		t.Fatal("no usage")
	}
	if usage.Scope != core.UsageScopeTurn {
		t.Fatalf("scope %v", usage.Scope)
	}
	if usage.InputTokens != 12504+717+11264+23552+128 {
		t.Fatalf("input %d", usage.InputTokens)
	}
	if usage.OutputTokens != 416+25 {
		t.Fatalf("output %d", usage.OutputTokens)
	}
	if *usage.CachedReadTokens != 11264+23552 || *usage.CachedWriteTokens != 128 {
		t.Fatalf("cache %d %d", *usage.CachedReadTokens, *usage.CachedWriteTokens)
	}
	if usage.ThoughtTokens != nil {
		t.Fatal("thought tokens")
	}
	if usage.TotalTokens != usage.InputTokens+usage.OutputTokens {
		t.Fatalf("total %d", usage.TotalTokens)
	}
	details := usage.ProviderDetails
	if models := slices.Sorted(func(yield func(string) bool) {
		for model := range details.ModelUsage {
			if !yield(model) {
				return
			}
		}
	}); !reflect.DeepEqual(models, []string{"kimi-code/k3"}) {
		t.Fatalf("models %v", models)
	}
	if details.ModelUsage["kimi-code/k3"].OutputTokens != usage.OutputTokens {
		t.Fatal("model output differs")
	}
	// A prompt that started after this turn did not spend these tokens.
	if follower.TurnUsageSince(1787697700000) != nil {
		t.Fatal("a later prompt has this turn's usage")
	}
}

func TestANewTurnStartsItsOwnUsageAndAnUnreadableRecordIsNotCountedAsWhole(t *testing.T) {
	path := filepath.Join(t.TempDir(), "wire.jsonl")
	kimiAppendJSONL(t, path, []any{
		acpFeaturesJSON(t, `{"type":"turn.started","agentId":"main","turnId":0,"time":100}`),
		acpFeaturesJSON(t, `{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":10,"output":5,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":110}`),
		acpFeaturesJSON(t, `{"type":"turn.ended","agentId":"main","turnId":0,"reason":"completed"}`),
		acpFeaturesJSON(t, `{"type":"turn.started","agentId":"main","turnId":1,"time":200}`),
		acpFeaturesJSON(t, `{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":7,"output":3,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":210}`),
	})
	follower := kimiOpen(t, path)
	kimiSettled(t, follower, 200, false)
	usage := follower.TurnUsageSince(200)
	if usage.InputTokens != 7 || usage.OutputTokens != 3 {
		t.Fatalf("counts %d %d", usage.InputTokens, usage.OutputTokens)
	}
	if usage.Scope != core.UsageScopeTurn {
		t.Fatalf("scope %v", usage.Scope)
	}

	// A counter this build cannot read leaves a floor, never a total a
	// session sum would trust.
	kimiAppendLines(t, path, []any{acpFeaturesJSON(t,
		`{"type":"usage.record","model":"kimi-code/k3","usage":{"output":"lots"},"usageScope":"turn","time":220}`)})
	kimiRefresh(t, follower)
	usage = follower.TurnUsageSince(200)
	if usage.InputTokens != 7 || usage.OutputTokens != 3 {
		t.Fatalf("counts %d %d", usage.InputTokens, usage.OutputTokens)
	}
	if usage.Scope != core.UsageScopeUnspecified {
		t.Fatalf("scope %v", usage.Scope)
	}
}

func TestDiagnosticsFollowOnlyTheCurrentMatchingMainTurn(t *testing.T) {
	path := filepath.Join(t.TempDir(), "wire.jsonl")
	failure := acpFeaturesJSON(t, `{"type":"turn.ended", "agentId":"main", "turnId":1,"reason":"failed", "error":{
            "message":"Five-hour usage limit exceeded. Resets at 23:00 UTC.", "code":"provider.auth_error", "details":{"statusCode":403}
        }}`)
	kimiAppendLines(t, path, []any{
		acpFeaturesJSON(t, `{"type":"turn.started","agentId":"main","turnId":1,"time":100}`),
		failure,
	})
	follower := kimiOpen(t, path)
	diagnostic := follower.TurnDiagnosticSince(90)
	if diagnostic == nil || !diagnostic.IsUsageLimit() {
		t.Fatalf("diagnostic %+v", diagnostic)
	}
	if diagnostic.HTTPStatus == nil || *diagnostic.HTTPStatus != 403 {
		t.Fatalf("status %v", diagnostic.HTTPStatus)
	}
	if follower.TurnDiagnosticSince(101) != nil {
		t.Fatal("an older failure describes a newer prompt")
	}
	kimiAppendLines(t, path, []any{
		acpFeaturesJSON(t, `{"type":"turn.started","agentId":"main","turnId":2,"time":200}`),
		failure,
		acpFeaturesJSON(t, `{"type":"turn.ended","agentId":"child","turnId":2,"reason":"failed","error":{"message":"quota exhausted"}}`),
	})
	kimiRefresh(t, follower)
	if follower.TurnDiagnosticSince(190) != nil {
		t.Fatal("another turn's failure was taken")
	}
	next := acpFeaturesSet(t, failure, int64(2), "turnId")
	// An incomplete JSONL append must not produce a diagnostic.
	file, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if _, err := file.WriteString(kimiRecordText(t, next)); err != nil {
		t.Fatal(err)
	}
	kimiRefresh(t, follower)
	if follower.TurnDiagnosticSince(190) != nil {
		t.Fatal("a partial line produced a diagnostic")
	}
	if _, err := file.WriteString("\n"); err != nil {
		t.Fatal(err)
	}
	kimiRefresh(t, follower)
	if follower.TurnDiagnosticSince(190) == nil {
		t.Fatal("the completed line produced no diagnostic")
	}
	kimiAppendLines(t, path, []any{
		acpFeaturesJSON(t, `{"type":"turn.started","agentId":"main","turnId":3,"time":300}`),
		acpFeaturesJSON(t, `{"type":"turn.ended","agentId":"main","turnId":3,"reason":"completed"}`),
	})
	kimiRefresh(t, follower)
	if follower.TurnDiagnosticSince(290) != nil {
		t.Fatal("a completed turn has a diagnostic")
	}
}

// #1132: Kimi Code 2.0.2 journals no `turn.started`; that event is
// observable only and never reaches `wire.jsonl`. The durable record that
// opens a main-agent turn is `turn.prompt`. These are the records of a turn
// whose stored OAuth token was refused, in the order Kimi writes them (keys
// and shapes from a real session wire stream and the issue).
//
// Hard-won: 8d4f80a: Kimi login failures were misreported as unanswered prompts, prompting futile resends.
func TestATurnOpenedByItsPromptRecordCarriesTheNativeFailure(t *testing.T) {
	temp := t.TempDir()
	path := filepath.Join(temp, "wire.jsonl")
	kimiAppendJSONL(t, path, []any{
		acpFeaturesJSON(t, `{"type":"agent.turn.started","turnId":0,"queueItemId":"q","time":1000,"kind":"event"}`),
		acpFeaturesJSON(t, `{"type":"turn.prompt","agentId":"main","input":[],"origin":{"kind":"user"},"promptId":"msg_1","turnId":0,"time":1001}`),
		acpFeaturesJSON(t, `{"type":"agent.turn.ended","turnId":0,"outcome":"failed","errorMessage":"OAuthUnauthorizedError: Stored token for \"kimi-code\" was rejected; re-login required.","time":1003,"kind":"event"}`),
		acpFeaturesJSON(t, `{"type":"turn.ended","agentId":"main","turnId":0,"reason":"failed","error":{"code":"internal","message":"Stored token for \"kimi-code\" was rejected; re-login required.","name":"OAuthUnauthorizedError","retryable":false},"durationMs":3,"time":1004}`),
		acpFeaturesJSON(t, `{"type":"prompt.completed","agentId":"main","promptId":"msg_1","reason":"failed","time":1005}`),
	})
	follower := kimiOpen(t, path)
	kimiSettled(t, follower, 1000, true)
	diagnostic := follower.TurnDiagnosticSince(1000)
	if diagnostic == nil {
		t.Fatal("the refused login is the turn's failure")
	}
	if diagnostic.Message != `Stored token for "kimi-code" was rejected; re-login required.` {
		t.Fatalf("message %q", diagnostic.Message)
	}
	if !core.TurnDiagnosticReportsAuthFailure(diagnostic) {
		t.Fatal("not an auth failure")
	}
	if follower.TurnDiagnosticSince(1002) != nil {
		t.Fatal("a later prompt has the failure")
	}

	// An earlier Kimi wrote neither `agentId` nor, on the prompt, a `turnId`
	// (a real wire stream from 2026-09-05).
	older := filepath.Join(temp, "older.jsonl")
	kimiAppendJSONL(t, older, []any{
		acpFeaturesJSON(t, `{"type":"turn.prompt","input":[],"origin":{"kind":"user"},"time":2000}`),
		acpFeaturesJSON(t, `{"type":"usage.record","model":"kimi-code/k3","usage":{"inputOther":10,"output":2,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":2001}`),
		acpFeaturesJSON(t, `{"type":"turn.ended","turnId":1,"reason":"completed","durationMs":1,"time":2002}`),
	})
	follower = kimiOpen(t, older)
	kimiSettled(t, follower, 2000, true)
	if usage := follower.TurnUsageSince(2000); usage == nil || usage.OutputTokens != 2 {
		t.Fatalf("usage %+v", usage)
	}

	// A child agent's records in the main stream are not the main turn's.
	child := filepath.Join(temp, "child.jsonl")
	kimiAppendJSONL(t, child, []any{
		acpFeaturesJSON(t, `{"type":"turn.prompt","agentId":"main","input":[],"origin":{"kind":"user"},"turnId":4,"time":3000}`),
		acpFeaturesJSON(t, `{"type":"turn.prompt","agentId":"agent-2","input":[],"origin":{"kind":"user"},"turnId":0,"time":3001}`),
		acpFeaturesJSON(t, `{"type":"turn.ended","agentId":"agent-2","turnId":0,"reason":"failed","error":{"message":"quota exhausted"}}`),
	})
	follower = kimiOpen(t, child)
	kimiSettled(t, follower, 3000, false)
	if follower.TurnDiagnosticSince(3000) != nil {
		t.Fatal("a child's failure is the main turn's")
	}
}

func TestResolveUsesLastMatchingIndexRowAndValidatesState(t *testing.T) {
	temp, sessionID, sessionDir := kimiSessionFixture(t)
	second := filepath.Join(temp, "sessions/other", sessionID)
	if err := os.MkdirAll(second, 0o755); err != nil {
		t.Fatal(err)
	}
	kimiWriteJSON(t, filepath.Join(second, stateFile), map[string]any{"id": sessionID})
	kimiWriteIndex(t, temp, []any{
		map[string]any{"sessionId": sessionID, "sessionDir": sessionDir},
		map[string]any{"sessionId": sessionID, "sessionDir": second},
	})
	resolved, err := ResolveSessionDir(temp, sessionID)
	if err != nil {
		t.Fatal(err)
	}
	want, err := filepath.EvalSymlinks(second)
	if err != nil {
		t.Fatal(err)
	}
	if resolved != want {
		t.Fatalf("resolved %s, want %s", resolved, want)
	}
}

func TestResolveRejectsSessionPathOutsideHome(t *testing.T) {
	temp, sessionID, _ := kimiSessionFixture(t)
	outside := t.TempDir()
	outsideSession := filepath.Join(outside, "session")
	if err := os.MkdirAll(outsideSession, 0o755); err != nil {
		t.Fatal(err)
	}
	kimiWriteJSON(t, filepath.Join(outsideSession, stateFile), map[string]any{"id": sessionID})
	kimiWriteIndex(t, temp, []any{map[string]any{"sessionId": sessionID, "sessionDir": outsideSession}})
	_, err := ResolveSessionDir(temp, sessionID)
	if err == nil || !strings.Contains(err.Error(), "escapes Kimi home") {
		t.Fatalf("error %v", err)
	}
}

func TestResolveRejectsStateIDMismatch(t *testing.T) {
	temp, sessionID, sessionDir := kimiSessionFixture(t)
	kimiWriteJSON(t, filepath.Join(sessionDir, stateFile), map[string]any{"id": "other"})
	kimiWriteIndex(t, temp, []any{map[string]any{"sessionId": sessionID, "sessionDir": sessionDir}})
	_, err := ResolveSessionDir(temp, sessionID)
	if err == nil || !strings.Contains(err.Error(), "does not match") {
		t.Fatalf("error %v", err)
	}
}

func TestDuplicateStartsAreIdempotentAndTerminalStatusIsIgnored(t *testing.T) {
	wire := filepath.Join(t.TempDir(), "wire.jsonl")
	start := kimiLifecycle(t, taskStarted, "same", nil, true, "agent")
	terminate := kimiLifecycle(t, taskTerminated, "same", nil, true, "agent")
	info, _ := terminate.Get("info")
	infoObject := info.(jsonvalue.Object)
	infoObject.Set("status", "aborted")
	terminate.Set("info", infoObject)
	kimiAppendJSONL(t, wire, []any{start, start, terminate})
	snapshot, err := FullScan(wire)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Tasks) != 0 {
		t.Fatalf("tasks %v", snapshot.Tasks)
	}
}

// Hard-won: 492a4bd: Kimi background shells were invisible and could not be stopped while the turn ran.
func TestDetachedProcessTaskIsTrackedUntilItTerminates(t *testing.T) {
	wire := filepath.Join(t.TempDir(), "wire.jsonl")
	// Kimi's process records carry no info.agentId and may omit timeoutMs.
	parent := "tool_bash"
	withoutInfoAgent := func(record jsonvalue.Object) jsonvalue.Object {
		info, _ := record.Get("info")
		infoObject := info.(jsonvalue.Object).Clone()
		infoObject.Delete("agentId")
		record = record.Clone()
		record.Set("info", infoObject)
		return record
	}
	start := withoutInfoAgent(kimiLifecycle(t, taskStarted, "bash-r5ae", &parent, true, "process"))
	terminate := withoutInfoAgent(kimiLifecycle(t, taskTerminated, "bash-r5ae", &parent, true, "process"))
	kimiAppendJSONL(t, wire, []any{start})

	snapshot, err := FullScan(wire)
	if err != nil {
		t.Fatal(err)
	}
	var ids []string
	for _, task := range snapshot.Tasks {
		ids = append(ids, task.TaskID)
	}
	if !reflect.DeepEqual(ids, []string{"bash-r5ae"}) {
		t.Fatalf("tasks %v", ids)
	}
	if task := snapshot.Task("bash-r5ae"); task.ParentToolCallID == nil || *task.ParentToolCallID != "tool_bash" {
		t.Fatalf("parent %v", task.ParentToolCallID)
	}

	kimiAppendJSONL(t, wire, []any{start, terminate})
	snapshot, err = FullScan(wire)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Tasks) != 0 {
		t.Fatalf("tasks %v", snapshot.Tasks)
	}
}

func TestLegacyTerminationAliasRemovesTask(t *testing.T) {
	wire := filepath.Join(t.TempDir(), "wire.jsonl")
	kimiAppendJSONL(t, wire, []any{
		kimiLifecycle(t, legacyTaskStarted, "legacy", nil, true, "agent"),
		kimiLifecycle(t, legacyTaskTerminated, "legacy", nil, true, "agent"),
	})
	snapshot, err := FullScan(wire)
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Tasks) != 0 {
		t.Fatalf("tasks %v", snapshot.Tasks)
	}
}

func TestFollowerRetainsPartialLineUntilCompletion(t *testing.T) {
	temp := t.TempDir()
	wire := filepath.Join(temp, "wire.jsonl")
	line := kimiRecordText(t, kimiLifecycle(t, taskStarted, "partial", nil, true, "agent"))
	if err := os.WriteFile(wire, []byte(line[:len(line)/2]), 0o644); err != nil {
		t.Fatal(err)
	}
	follower := kimiOpen(t, wire)
	if len(follower.Snapshot().Tasks) != 0 {
		t.Fatal("a partial line produced a task")
	}
	appendBytes := func(path string, parts ...[]byte) {
		file, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
		if err != nil {
			t.Fatal(err)
		}
		defer file.Close()
		for _, part := range parts {
			if _, err := file.Write(part); err != nil {
				t.Fatal(err)
			}
		}
	}
	appendBytes(wire, []byte(line[len(line)/2:]), []byte("\n"))
	if _, ok := kimiRefresh(t, follower).(KimiWireRefreshUpdated); !ok {
		t.Fatal("refresh did not update")
	}
	if tasks := follower.Snapshot().Tasks; len(tasks) == 0 || tasks[0].TaskID != "partial" {
		t.Fatalf("tasks %v", tasks)
	}

	// A partial task line can begin exactly after bytes consumed from an
	// unrelated complete record; keep this offset case with the same
	// partial-line completion invariant.
	equalWire := filepath.Join(temp, "equal-tail.jsonl")
	complete := []byte("{\"type\":\"unknown\"}\n")
	partial := kimiRecordText(t, kimiLifecycle(t, taskStarted, "equal-tail", nil, true, "agent"))
	if len(partial) < len(complete) {
		t.Fatal("fixture too short")
	}
	prefix := append(slices.Clone(complete), partial[:len(complete)]...)
	if err := os.WriteFile(equalWire, prefix, 0o644); err != nil {
		t.Fatal(err)
	}
	follower = kimiOpen(t, equalWire)
	if len(follower.Snapshot().Tasks) != 0 {
		t.Fatal("a partial line produced a task")
	}
	appendBytes(equalWire, []byte(partial[len(complete):]), []byte("\n"))
	kimiRefresh(t, follower)
	if tasks := follower.Snapshot().Tasks; len(tasks) == 0 || tasks[0].TaskID != "equal-tail" {
		t.Fatalf("tasks %v", tasks)
	}
}

func TestMalformedRelevantLineFailsClosed(t *testing.T) {
	wire := filepath.Join(t.TempDir(), "wire.jsonl")
	if err := os.WriteFile(wire, []byte("{\"type\":\"task.started\",\"agentId\":\"main\"}\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	_, err := FullScan(wire)
	if err == nil || !strings.Contains(err.Error(), "task lifecycle") {
		t.Fatalf("error %v", err)
	}
}

func TestFollowerDetectsTruncationAndReplacement(t *testing.T) {
	wire := filepath.Join(t.TempDir(), "wire.jsonl")
	kimiAppendJSONL(t, wire, []any{acpFeaturesJSON(t, `{"type":"unknown"}`)})
	follower := kimiOpen(t, wire)
	if err := os.WriteFile(wire, []byte("{}\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	// Ensure filesystems with coarse timestamp granularity still expose the
	// replacement through the Unix inode identity or prefix sample.
	time.Sleep(2 * time.Millisecond)
	if _, ok := kimiRefresh(t, follower).(KimiWireRefreshRescanRequired); !ok {
		t.Fatal("the replacement was not detected")
	}
}
