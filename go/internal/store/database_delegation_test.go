// Port of the tests in mj-store/src/database/delegation.rs.

package store

import (
	"encoding/json"
	"fmt"
	"os"
	"reflect"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// delegationIsolated re-runs test alone with its own store, as Rust's
// IsolatedTest does for the tests that install the process's writer. It
// reports whether this process is the isolated child.
func delegationIsolated(t *testing.T, marker, instance, test string) bool {
	t.Helper()
	if os.Getenv(marker) != "" {
		return true
	}
	targets.NewIsolatedTest(targets.TestName("database::delegation::tests", test)).
		Env(marker, "1").
		Env("MJ_INSTANCE", instance).
		IsolatedStore(t.TempDir()).
		Run()
	return false
}

func delegationMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func delegationDecode[T any](t *testing.T, text string) T {
	t.Helper()
	var value T
	delegationMust(t, json.Unmarshal([]byte(text), &value))
	return value
}

func TestRestartPreservesSelectedTurnAndResultUntilAcknowledged(t *testing.T) {
	if !delegationIsolated(t, "MJ_TEST_DURABLE_DELEGATION", "concurrency-sweep-delegation",
		"TestRestartPreservesSelectedTurnAndResultUntilAcknowledged") {
		return
	}
	writer := InstallIsolatedTestWriter()
	original := "original-turn"
	prepared := PreparedDelegation{
		Request: core.SubagentToolRequest{
			RequestID:   "request",
			CreatedAtMs: 1,
			Action:      core.SubagentToolActionSendInput{ChildSessionID: "child", Message: "continue"},
		},
		TurnTarget: &original,
	}
	_, err := PrepareDelegation("parent", prepared)
	delegationMust(t, err)
	delegationMust(t, DelegationDelivering("parent", "request"))
	if pending, err := HasPendingMutatingDelegations("parent"); err != nil || !pending {
		t.Fatalf("pending = %v, %v", pending, err)
	}
	delegationMust(t, writer.Shutdown())
	writer = InstallIsolatedTestWriter()
	defer writer.Shutdown()
	replacement := prepared
	later := "later-turn"
	replacement.TurnTarget = &later
	restored, err := PrepareDelegation("parent", replacement)
	delegationMust(t, err)
	if restored.TurnTarget == nil || *restored.TurnTarget != "original-turn" {
		t.Fatalf("turn target = %v", restored.TurnTarget)
	}
	result := core.SubagentToolResult{
		RequestID:     "request",
		CompletedAtMs: 2,
		IsError:       false,
		Message:       "original result",
	}
	delegationMust(t, RecordDelegationResult("parent", result))
	if pending, err := HasPendingMutatingDelegations("parent"); err != nil || pending {
		t.Fatalf("pending after the result = %v, %v", pending, err)
	}
	loaded, stored, err := LoadDelegation("parent", "request")
	delegationMust(t, err)
	if loaded == nil || loaded.TurnTarget == nil || *loaded.TurnTarget != "original-turn" {
		t.Fatalf("loaded = %+v", loaded)
	}
	if stored == nil || stored.Result.Message != "original result" {
		t.Fatalf("stored = %+v", stored)
	}
	// Acknowledgement is the end of the record's life: a later replay of
	// the same request id is a new request, and the table does not grow.
	delegationMust(t, AcknowledgeDelegation("parent", "request"))
	if loaded, _, err := LoadDelegation("parent", "request"); err != nil || loaded != nil {
		t.Fatalf("after acknowledgement = %+v, %v", loaded, err)
	}

	// A row an older build left acknowledged is pruned at start.
	_, err = PrepareDelegation("parent", *loaded)
	delegationMust(t, err)
	_, err = submitDatabaseWrite("mark legacy acknowledged", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("UPDATE delegation_effects SET phase='acknowledged'")
		return struct{}{}, err
	})
	delegationMust(t, err)
	delegationMust(t, PruneAcknowledgedDelegations())
	if loaded, _, err := LoadDelegation("parent", "request"); err != nil || loaded != nil {
		t.Fatalf("after pruning = %+v, %v", loaded, err)
	}
}

func TestLegacyRawDelegationResultsLoadWithoutReportMarkers(t *testing.T) {
	result := core.SubagentToolResult{
		RequestID:     "legacy",
		CompletedAtMs: 2,
		IsError:       false,
		Message:       "old result shape",
	}
	encoded, err := json.Marshal(&result)
	delegationMust(t, err)
	restored, err := decodeStoredResult(string(encoded))
	delegationMust(t, err)
	if restored.Result != result {
		t.Fatalf("result = %+v", restored.Result)
	}
	if len(restored.ReportedFinishes) != 0 {
		t.Fatalf("markers = %+v", restored.ReportedFinishes)
	}
}

func TestFinishedReportOutboxAndMarkerCommitOrRollBackTogether(t *testing.T) {
	if !delegationIsolated(t, "MJ_TEST_REPORT_OUTBOX_ATOMIC", "report-outbox-atomic",
		"TestFinishedReportOutboxAndMarkerCommitOrRollBackTogether") {
		return
	}
	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	parent := CheckpointTestSession("parent")
	delegationMust(t, SaveSession(&parent))
	child := CheckpointTestSession("child")
	relation := delegationDecode[core.SubagentRecord](t, `{
            "child_session_id":"child", "parent_session_id":"parent", "task_name":"audit", "profile_id":"profile",
            "working_directory":"", "initial_prompt":"audit", "request_key":"task", "created_at":"now"
        }`)
	delegationMust(t, SaveSubagentSession(&child, &relation))
	report := delegationDecode[core.SubagentFinishedReport](t, `{
            "child_session_id":"child", "task_name":"audit", "state":"completed", "finished":true,
            "output":"the report", "report_dir":"/reports/child"
        }`)
	identity := delegationDecode[core.SubagentFinishIdentity](t, `{
            "kind":"terminal", "state":"completed", "detail":"the report", "last_completed_ordinal":9
        }`)
	execute := func(label, batch string) {
		t.Helper()
		_, err := submitDatabaseWrite(label, func(connection *Connection) (struct{}, error) {
			_, err := connection.Exec(batch)
			return struct{}{}, err
		})
		delegationMust(t, err)
	}
	reportedFinish := func() *core.SubagentFinishIdentity {
		t.Helper()
		loaded, err := LoadSubagent("child")
		delegationMust(t, err)
		if loaded == nil {
			t.Fatal("the child relation is missing")
		}
		return loaded.ReportedFinish
	}
	pending := func() []MailboxOutboxEntry {
		t.Helper()
		entries, err := PendingMailboxEvents(10)
		delegationMust(t, err)
		return entries
	}
	execute("inject report marker failure", "CREATE TEMP TRIGGER fail_report_marker BEFORE UPDATE ON subagent_sessions BEGIN SELECT RAISE(ABORT, 'report marker failed'); END;")
	if EnqueueSubagentReports("parent", []SubagentFinishReport{{Identity: identity, Report: report}}, nil) == nil {
		t.Fatal("the refused marker must fail the enqueue")
	}
	if len(pending()) != 0 {
		t.Fatal("a rolled back marker must not leave its report")
	}
	if reportedFinish() != nil {
		t.Fatal("a rolled back report must not move the marker")
	}
	execute("clear report marker failure", "DROP TRIGGER fail_report_marker;")
	delegationMust(t, EnqueueSubagentReports("parent", []SubagentFinishReport{{Identity: identity, Report: report}}, nil))
	delegationMust(t, EnqueueSubagentReports("parent", []SubagentFinishReport{{Identity: identity, Report: report}}, nil))
	if len(pending()) != 1 {
		t.Fatalf("reports = %d", len(pending()))
	}
	if marker := reportedFinish(); marker == nil || !reflect.DeepEqual(*marker, identity) {
		t.Fatalf("marker = %+v", marker)
	}
	newer := delegationDecode[core.SubagentFinishIdentity](t, `{
            "kind":"terminal", "state":"completed", "detail":"new report", "last_completed_ordinal":10
        }`)
	newerReport := report
	newOutput := "new report"
	newerReport.Output = &newOutput
	delegationMust(t, EnqueueSubagentReports("parent", []SubagentFinishReport{{Identity: newer, Report: newerReport}}, nil))
	stale := report
	parked := true
	stale.Parked = &parked
	delegationMust(t, EnqueueSubagentReports("parent", []SubagentFinishReport{{Identity: identity, Report: stale}}, nil))
	if len(pending()) != 2 {
		t.Fatalf("reports = %d", len(pending()))
	}
	if marker := reportedFinish(); marker == nil || !reflect.DeepEqual(*marker, newer) {
		t.Fatalf("marker = %+v", marker)
	}
}

// Hard-won: #1302: an undelivered old wait owned reports that reconciliation skipped.
func TestRetiringLegacyWaitTransfersReportsAtomically(t *testing.T) {
	if !delegationIsolated(t, "MJ_TEST_LEGACY_WAIT_TRANSFER", "legacy-wait-transfer",
		"TestRetiringLegacyWaitTransfersReportsAtomically") {
		return
	}
	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	parent := CheckpointTestSession("parent")
	delegationMust(t, SaveSession(&parent))
	identityJSON := `{"kind":"terminal", "state":"completed", "detail":"retained report", "last_completed_ordinal":9}`
	identity := delegationDecode[core.SubagentFinishIdentity](t, identityJSON)
	relation := delegationDecode[core.SubagentRecord](t, fmt.Sprintf(`{
            "child_session_id":"child", "parent_session_id":"parent", "task_name":"audit", "profile_id":"profile",
            "working_directory":"", "initial_prompt":"audit", "request_key":"task", "created_at":"now",
            "reported_finish": %s
        }`, identityJSON))
	child := CheckpointTestSession("child")
	delegationMust(t, SaveSubagentSession(&child, &relation))
	prepared := delegationDecode[PreparedDelegation](t, `{
            "request":{"request_id":"old-wait", "created_at_ms":1, "action":{"action":"wait_agents"}}, "turn_target":null
        }`)
	_, err := PrepareDelegation("parent", prepared)
	delegationMust(t, err)
	report := `{"child_session_id":"child","state":"completed","finished":true,"output":"retained report","report_dir":"/reports/child"}`
	message, err := json.Marshal(fmt.Sprintf(`{"agents":[%s]}`, report))
	delegationMust(t, err)
	encodedIdentity, err := json.Marshal(&identity)
	delegationMust(t, err)
	stored := fmt.Sprintf(`{"result":{"request_id":"old-wait","completed_at_ms":2,"is_error":false,"message":%s},"reported_finishes":[["child",%s]]}`,
		message, encodedIdentity)
	_, err = submitDatabaseWrite("seed old result", func(connection *Connection) (struct{}, error) {
		if _, err := connection.Exec("UPDATE delegation_effects SET result_json=?1, phase='result'", stored); err != nil {
			return struct{}{}, err
		}
		_, err := connection.Exec("CREATE TEMP TRIGGER reject_retirement BEFORE UPDATE OF result_json ON delegation_effects BEGIN SELECT RAISE(ABORT, 'retirement failed'); END;")
		return struct{}{}, err
	})
	delegationMust(t, err)
	if _, err := RetireLegacyWait("parent", "old-wait"); err == nil {
		t.Fatal("the refused replacement must fail the retirement")
	}
	if entries, err := PendingMailboxEvents(10); err != nil || len(entries) != 0 {
		t.Fatalf("reports after a failed retirement = %d, %v", len(entries), err)
	}
	if loaded, _, err := LoadDelegation("parent", "old-wait"); err != nil || loaded == nil {
		t.Fatalf("the wait record must survive a failed retirement: %v", err)
	}
	_, err = submitDatabaseWrite("allow retirement", func(connection *Connection) (struct{}, error) {
		_, err := connection.Exec("DROP TRIGGER reject_retirement")
		return struct{}{}, err
	})
	delegationMust(t, err)
	replaced, err := RetireLegacyWait("parent", "old-wait")
	delegationMust(t, err)
	if replaced == nil {
		t.Fatal("the legacy wait must be retired")
	}
	again, err := RetireLegacyWait("parent", "old-wait")
	delegationMust(t, err)
	if again == nil || !reflect.DeepEqual(*again, *replaced) {
		t.Fatalf("a second retirement = %+v", again)
	}
	_, loadedResult, err := LoadDelegation("parent", "old-wait")
	delegationMust(t, err)
	if loadedResult == nil || !reflect.DeepEqual(*loadedResult, *replaced) {
		t.Fatalf("stored result = %+v", loadedResult)
	}
	var answer map[string]any
	delegationMust(t, json.Unmarshal([]byte(replaced.Result.Message), &answer))
	if answer["status"] != "reports_arrive_as_messages" {
		t.Fatalf("answer = %v", answer)
	}
	delegationMust(t, AcknowledgeDelegation("parent", "old-wait"))
	if retired, err := RetireLegacyWait("parent", "old-wait"); err != nil || retired != nil {
		t.Fatalf("retiring an acknowledged wait = %+v, %v", retired, err)
	}
	if loaded, _, err := LoadDelegation("parent", "old-wait"); err != nil || loaded != nil {
		t.Fatalf("after acknowledgement = %+v, %v", loaded, err)
	}
	rows, err := PendingMailboxEvents(10)
	delegationMust(t, err)
	if len(rows) != 1 {
		t.Fatalf("reports = %d", len(rows))
	}
	event := delegationDecode[core.MailboxEvent](t, rows[0].EventJSON)
	key, err := subagentReportKey("parent", "child", &identity)
	delegationMust(t, err)
	if event.Key != key {
		t.Fatalf("event key = %s, want %s", event.Key, key)
	}
	body, ok := event.Body.(core.MailboxEventBodySubagentFinished)
	if !ok || body.Report == nil {
		t.Fatalf("expected child report: %+v", event.Body)
	}
	if body.Report.TaskName != "audit" {
		t.Fatalf("task name = %q", body.Report.TaskName)
	}
	if body.Report.Output == nil || *body.Report.Output != "retained report" {
		t.Fatalf("output = %v", body.Report.Output)
	}
	loaded, err := LoadSubagent("child")
	delegationMust(t, err)
	if loaded == nil || loaded.ReportedFinish == nil || !reflect.DeepEqual(*loaded.ReportedFinish, identity) {
		t.Fatalf("marker = %+v", loaded)
	}
}

// Hard-won: #1302: blocked targets filled every global outbox batch.
func TestPendingMailboxBatchesDoNotStarveOtherTargets(t *testing.T) {
	if !delegationIsolated(t, "MJ_TEST_OUTBOX_TARGET_BATCH", "outbox-target-batch",
		"TestPendingMailboxBatchesDoNotStarveOtherTargets") {
		return
	}
	writer := InstallIsolatedTestWriter()
	defer writer.Shutdown()
	for _, target := range []string{"blocked", "ready"} {
		record := CheckpointTestSession(target)
		delegationMust(t, SaveSession(&record))
	}
	_, err := submitDatabaseWrite("seed blocked backlog", func(connection *Connection) (struct{}, error) {
		enqueue := func(key, target, text string, createdAtMs uint64) error {
			event := core.MailboxEvent{
				Key:         key,
				Source:      "test",
				Wake:        true,
				CreatedAtMs: createdAtMs,
				Body:        core.MailboxEventBodyPlainText{Text: text},
			}
			encoded, err := json.Marshal(&event)
			if err != nil {
				return err
			}
			_, err = EnqueueMailboxEventWith(connection, key, target, string(encoded), true, false)
			return err
		}
		for n := range uint64(600) {
			if err := enqueue(fmt.Sprintf("blocked-%04d", n), "blocked", fmt.Sprint(n), n); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, enqueue("ready", "ready", "ready", 601)
	})
	delegationMust(t, err)
	entries, err := PendingMailboxEvents(256)
	delegationMust(t, err)
	if entries[len(entries)-1].EventKey != "ready" {
		t.Fatalf("last entry = %s", entries[len(entries)-1].EventKey)
	}
	var blocked []MailboxOutboxEntry
	for _, entry := range entries {
		if entry.TargetSessionID == "blocked" {
			blocked = append(blocked, entry)
		}
	}
	if len(blocked) != 256 {
		t.Fatalf("blocked batch = %d", len(blocked))
	}
	if !slices.IsSortedFunc(blocked, func(a, b MailboxOutboxEntry) int {
		switch {
		case a.EventKey < b.EventKey:
			return -1
		case a.EventKey > b.EventKey:
			return 1
		}
		return 0
	}) {
		t.Fatal("the blocked batch is in key order")
	}
	for i := 1; i < len(blocked); i++ {
		if blocked[i-1].EventKey >= blocked[i].EventKey {
			t.Fatal("blocked keys repeat")
		}
	}
	// Selection never drops blocked rows, and a later sweep sees the same
	// ordered batch.
	again, err := PendingMailboxEvents(256)
	delegationMust(t, err)
	if !reflect.DeepEqual(entries, again) {
		t.Fatal("a later sweep must see the same batch")
	}
	all, err := PendingMailboxEvents(1000)
	delegationMust(t, err)
	if len(all) != 601 {
		t.Fatalf("all = %d", len(all))
	}
}
