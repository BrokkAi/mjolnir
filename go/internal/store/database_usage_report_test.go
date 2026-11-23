// Port of the tests in mj-store/src/database/usage_report.rs.

package store

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unsafe"

	"modernc.org/libc"
	sqlite3 "modernc.org/sqlite/lib"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// usageReportVMSteps runs query once on its own connection to path and
// returns the text of column in each row with the statement's virtual
// machine step count: rusqlite's get_status(StatementStatus::VmStep), which
// database/sql does not expose. A nil argument binds NULL.
func usageReportVMSteps(t *testing.T, path, query string, column int32, args ...*int64) ([]string, int32) {
	t.Helper()
	tls := libc.NewTLS()
	defer tls.Close()
	name, err := libc.CString(path)
	if err != nil {
		t.Fatal(err)
	}
	defer libc.Xfree(tls, name)
	text, err := libc.CString(query)
	if err != nil {
		t.Fatal(err)
	}
	defer libc.Xfree(tls, text)
	slotSize := int(unsafe.Sizeof(uintptr(0)))
	slot := tls.Alloc(slotSize)
	defer tls.Free(slotSize)
	if rc := sqlite3.Xsqlite3_open_v2(tls, name, slot, sqlite3.SQLITE_OPEN_READWRITE, 0); rc != sqlite3.SQLITE_OK {
		t.Fatalf("open %s: result %d", path, rc)
	}
	db := libc.AtomicLoadPUintptr(slot)
	defer sqlite3.Xsqlite3_close(tls, db)
	failure := func(step string) {
		t.Helper()
		t.Fatalf("%s: %s", step, libc.GoString(sqlite3.Xsqlite3_errmsg(tls, db)))
	}
	if sqlite3.Xsqlite3_prepare_v2(tls, db, text, -1, slot, 0) != sqlite3.SQLITE_OK {
		failure("prepare")
	}
	statement := libc.AtomicLoadPUintptr(slot)
	defer sqlite3.Xsqlite3_finalize(tls, statement)
	for i, arg := range args {
		rc := sqlite3.Xsqlite3_bind_null(tls, statement, int32(i+1))
		if arg != nil {
			rc = sqlite3.Xsqlite3_bind_int64(tls, statement, int32(i+1), *arg)
		}
		if rc != sqlite3.SQLITE_OK {
			failure("bind")
		}
	}
	var values []string
	for {
		switch sqlite3.Xsqlite3_step(tls, statement) {
		case sqlite3.SQLITE_ROW:
			values = append(values, libc.GoString(sqlite3.Xsqlite3_column_text(tls, statement, column)))
		case sqlite3.SQLITE_DONE:
			return values, sqlite3.Xsqlite3_stmt_status(tls, statement, sqlite3.SQLITE_STMTSTATUS_VM_STEP, 0)
		default:
			failure("step")
		}
	}
}

func usageReportInt(value int64) *int64 { return &value }

func usageReportExec(t *testing.T, connection Queryer, query string, args ...any) {
	t.Helper()
	if _, err := connection.Exec(query, args...); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: 1f8ff4afc: an hourly read scanned every historical turn body.
func TestUsageReportTimeIndexMigrationKeepsOldWritesAndSkipsHistory(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage-index.sqlite")
	root := session("root", "project")
	usageMust(t, SaveSessionTo(path, &root))
	connection, err := open(path)
	usageMust(t, err)
	defer connection.Close()
	// Reconstruct the previous revision, with a turn written before the
	// index.
	usageMust(t, ensureSessionContainerOverrideColumns(connection))
	restorePre153TranscriptLayout(t, connection)
	usageReportExec(t, connection, `DROP INDEX session_turn_usage_completed_time;
             DROP INDEX materialized_sessions_active_usage;
             DELETE FROM schema_migrations WHERE version>=125;
             UPDATE schema_compatibility SET minimum_compatible_version=124;
             PRAGMA user_version=124;`)
	turn := usageLegacyTurns(t)[0]
	turn.CommandID = "recent"
	turn.CompletedAtMs = 1_000
	body, err := serializeTurnOutcome(&turn)
	usageMust(t, err)
	usageReportExec(t, connection, `INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body)
             VALUES ('root','recent',1,?1)`, body)
	usageMust(t, migrateSchema(connection))
	state, err := readSchemaState(connection)
	usageMust(t, err)
	usageMust(t, state.ensureSupportedBy(schemaVersion))

	read := func() ([]string, int32) {
		return usageReportVMSteps(t, path, reportTurnsSQL, 1, usageReportInt(1_000), usageReportInt(2_000), nil)
	}
	before, stepsBefore := read()
	if !slices.Equal(before, []string{body}) {
		t.Fatalf("window rows = %v", before)
	}
	readActive := func() ([]string, int32) {
		return usageReportVMSteps(t, path, reportActiveSQL, 0, usageReportInt(2_000), nil)
	}
	activeBefore, activeStepsBefore := readActive()
	// Previous writers still use the same columns. SQLite maintains the
	// expression index for their inserts and updates without a new writer.
	usageReportExec(t, connection, `WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM history WHERE n<4096)
             INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body)
             SELECT 'root','old-' || n,n+1,json_set(?1,'$.completed_at_ms',0) FROM history`, body)
	after, stepsAfter := read()
	if !slices.Equal(after, before) {
		t.Fatalf("window rows after history = %v", after)
	}
	if stepsAfter != stepsBefore {
		t.Fatalf("hourly query work grew with old turns: %d -> %d", stepsBefore, stepsAfter)
	}
	usageReportExec(t, connection, `CREATE TEMP TABLE idle_members AS
                 WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM history WHERE n<4096)
                 SELECT 'idle-' || n AS id FROM history;
             INSERT INTO session_contexts(session_id,bundle_id,created_at)
                 SELECT id,'project','now' FROM idle_members;
             INSERT INTO sessions(session_id,title,harness_kind,last_profile,target_template_id,state,updated_at)
                 SELECT id,id,'codex','codex','local','stopped','now' FROM idle_members;
             INSERT INTO materialized_sessions(session_id) SELECT id FROM idle_members;`)
	activeAfter, activeStepsAfter := readActive()
	if !slices.Equal(activeAfter, activeBefore) || activeStepsAfter != activeStepsBefore {
		t.Fatalf("active-turn query work grew with inactive sessions: %d -> %d", activeStepsBefore, activeStepsAfter)
	}
	usageReportExec(t, connection, `UPDATE session_turn_usage SET body=json_set(body,'$.completed_at_ms',1500)
             WHERE session_id='root' AND command_id='old-1'`)
	if rows, _ := read(); len(rows) != 2 {
		t.Fatalf("window rows after an old update = %d", len(rows))
	}
	usageReportExec(t, connection, `UPDATE materialized_sessions SET active_turn_json=
                 json_object('command_id','recent','started_at_ms',1500)
             WHERE session_id='root'`)
	if members, _ := readActive(); !slices.Equal(members, []string{"root"}) {
		t.Fatalf("active members = %v", members)
	}
}

// Hard-won: #1559: the accounting schema lost reviewer kinds on destruction.
func TestUsageKindMigrationBackfillsLiveChildrenAndPreservesLegacyWrites(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage-kind.sqlite")
	for _, id := range []string{"root", "reviewer", "worker", "oracle", "destroyed", "legacy"} {
		record := session(id, "project")
		usageMust(t, SaveSessionTo(path, &record))
	}
	connection, err := open(path)
	usageMust(t, err)
	restorePre153TranscriptLayout(t, connection)
	usageReportExec(t, connection, `DROP TRIGGER subagent_accounting_kind_insert;
             DROP TRIGGER subagent_accounting_kind_update;
             ALTER TABLE subagent_accounting DROP COLUMN kind;
             DELETE FROM schema_migrations WHERE version>134;
             UPDATE schema_compatibility SET minimum_compatible_version=132 WHERE singleton=1;
             PRAGMA user_version=134;
             CREATE TRIGGER stop_before_brief_fence BEFORE INSERT ON schema_migrations
                 WHEN NEW.version=136 BEGIN SELECT RAISE(ABORT,'fixture boundary before brief fence'); END;
             INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name)
                 VALUES ('reviewer','root','review'),('worker','root','work'),
                     ('oracle','root','consult'),('destroyed','root','old');
             INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json)
                 VALUES ('reviewer','root','review','{"kind":"reviewer"}'),
                     ('worker','root','work','{}'),
                     ('oracle','root','consult','{"kind":"oracle"}');`)
	initial, err := readSchemaState(connection)
	usageMust(t, err)
	floor := initial.minimumCompatible
	// Test revision 135's compatibility before the separate brief fence.
	err = migrateSchema(connection)
	if err == nil || !strings.Contains(err.Error(), "fixture boundary before brief fence") {
		t.Fatalf("migration = %v", err)
	}
	usageReportExec(t, connection, "ROLLBACK;")
	at135, err := readSchemaState(connection)
	usageMust(t, err)
	if at135.revision != 135 {
		t.Fatalf("revision = %d", at135.revision)
	}
	if !committedOptionalEqual(at135.minimumCompatible, floor) {
		t.Fatalf("floor moved from %v to %v", floor, at135.minimumCompatible)
	}
	usageMust(t, at135.ensureSupportedBy(134))
	// An old daemon's conflict path must not erase the new accounting fact.
	usageReportExec(t, connection, `INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name)
                 VALUES ('reviewer','root','review') ON CONFLICT(child_session_id) DO NOTHING;
             INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name)
                 VALUES ('legacy','root','legacy');
             INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json)
                 VALUES ('legacy','root','legacy','{"kind":"reviewer"}');
             UPDATE subagent_sessions
                 SET record_json=json_set(record_json, '$.noticed_turn', 3)
                 WHERE child_session_id='reviewer';`)
	usageReportExec(t, connection, "DROP TRIGGER stop_before_brief_fence;")
	usageMust(t, connection.Close())
	usageMust(t, deleteSessionFrom(path, "reviewer"))
	usageMust(t, deleteSessionFrom(path, "legacy"))
	connection, err = open(path)
	usageMust(t, err)
	defer connection.Close()
	latest, err := readSchemaState(connection)
	usageMust(t, err)
	if latest.revision != schemaVersion {
		t.Fatalf("revision = %d", latest.revision)
	}
	if latest.minimumCompatible == nil || *latest.minimumCompatible != minimumCompatibleVersion {
		t.Fatalf("minimum compatible = %v", latest.minimumCompatible)
	}
	if latest.ensureSupportedBy(135) == nil {
		t.Fatal("revision 135 must not read the migrated store")
	}
	rows, err := connection.Query("SELECT child_session_id,kind FROM subagent_accounting ORDER BY child_session_id")
	usageMust(t, err)
	var retained []string
	for rows.Next() {
		var child string
		var kind *string
		usageMust(t, rows.Scan(&child, &kind))
		entry := child + "="
		if kind != nil {
			entry += *kind
		} else {
			entry += "<none>"
		}
		retained = append(retained, entry)
	}
	usageMust(t, rows.Err())
	rows.Close()
	expected := []string{"destroyed=<none>", "legacy=reviewer", "oracle=oracle", "reviewer=reviewer", "worker=worker"}
	if !slices.Equal(retained, expected) {
		t.Fatalf("retained kinds = %v", retained)
	}
}

// usageReportInsertTurn records one completed turn of sessionID with usage
// (raw JSON) and the selection that started it.
func usageReportInsertTurn(t *testing.T, connection Queryer, sessionID string, seq int64, timestamp int64, model, effort string, usage string) {
	t.Helper()
	command := fmt.Sprintf("command-%d", seq)
	var turn core.MaterializedTurnOutcome
	usageMust(t, json.Unmarshal([]byte(fmt.Sprintf(`{"command_id":%q,"completed_ordinal":%d,"completed_at_ms":%d,
                "outcome":{"kind":"completed","stop_reason":"EndTurn"},"usage":%s}`, command, seq, timestamp, usage)), &turn))
	body, err := serializeTurnOutcome(&turn)
	usageMust(t, err)
	usageReportExec(t, connection, "INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body) VALUES (?1,?2,?3,?4)",
		sessionID, command, seq, body)
	values := map[string]string{"model": model}
	if effort != "" {
		values["effort"] = effort
	}
	usageMust(t, recordTurnSelection(connection, sessionID, command, usageConfiguration(t, values)))
}

func usageReportRoleCost(t *testing.T, report *core.SessionUsageReport, role core.UsageAgentRole) core.CostUsdTicks {
	t.Helper()
	for _, group := range report.AgentUsage {
		if group.Role == role {
			return group.CostUsdTicks
		}
	}
	t.Fatalf("no %s group", role)
	return core.CostUsdTicks{}
}

// usageReportPriced is the known part of a cost that the unpriced models
// make unknown.
func usageReportPriced(session *core.SessionUsageEstimate) uint64 {
	var sum uint64
	for _, model := range session.Models {
		if !model.CostUsdTicks.Unknown {
			sum += model.CostUsdTicks.Ticks
		}
	}
	return sum
}

func usageReportLoad(t *testing.T, path string, query core.SessionUsageQuery, now int64) core.SessionUsageReport {
	t.Helper()
	report, err := loadFrom(path, &query, now)
	usageMust(t, err)
	return report
}

var usageReportUnknown = core.CostUsdTicks{Unknown: true}

// Hard-won: #1559: destroying reviewers erased their cost classification.
// Store/schema integration: retained descendants, inclusive time boundaries,
// model switches and incomplete reports must agree with the report reader.
func TestUsageReportPreservesRetainedTreesAndFiltersCompletedUsage(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage-report.sqlite")
	const now int64 = 31 * 86_400_000
	state := core.NewStateState()
	for _, id := range []string{"root", "child", "grandchild", "suspended"} {
		record := session(id, "project")
		switch id {
		case "suspended":
			record.LastProfile = "claude"
		case "child":
			record.LastProfile = "codex2"
		default:
			record.LastProfile = "codex"
		}
		record.State = core.SessionStateRunning
		if id == "suspended" {
			record.State = core.SessionStateStopped
		}
		state.Sessions.Insert(id, record)
	}
	usageMust(t, SaveStateTo(path, &state))
	connection, err := open(path)
	usageMust(t, err)
	usageReportExec(t, connection, "INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name) VALUES ('child','root','child'),('grandchild','child','grandchild')")
	for _, relation := range []struct {
		child, parent string
		kind          core.SubagentKind
	}{
		{"child", "root", core.SubagentKindWorker},
		{"grandchild", "child", core.SubagentKindReviewer},
	} {
		childRecord := state.Sessions.Index(relation.child)
		record := core.SubagentRecord{
			Kind:            relation.kind,
			ChildSessionID:  relation.child,
			ParentSessionID: relation.parent,
			TaskName:        relation.child,
			ProfileID:       childRecord.LastProfile,
			InitialPrompt:   "Inspect usage",
			RequestKey:      relation.child,
			CreatedAt:       childRecord.CreatedAt,
		}
		encoded, err := json.Marshal(&record)
		usageMust(t, err)
		usageReportExec(t, connection, `INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json)
                 VALUES (?1,?2,?3,?4)`, relation.child, relation.parent, relation.kind.RequestKey(relation.child), string(encoded))
	}
	const inclusive = `{"scope":"turn","total_tokens":1100000,"input_tokens":1000000,"output_tokens":100000,"cached_read_tokens":500000,"cached_write_tokens":0}`
	insert := func(sessionID string, seq int64, timestamp int64, model string, usage string) {
		t.Helper()
		usageReportInsertTurn(t, connection, sessionID, seq, timestamp, model, "high", usage)
	}
	insert("root", 1, now-23*3_600_000, "global.openai.gpt-6-luna", inclusive)
	insert("root", 2, now-23*3_600_000-1, "gpt-6-sol", inclusive)
	insert("root", 3, now+1, "gpt-6-astra", inclusive)
	insert("child", 1, now, "claude-opus-5-5-20260922",
		`{"scope":"turn","total_tokens":1800000,"input_tokens":1000000,"output_tokens":100000,"cached_read_tokens":500000,"cached_write_tokens":200000}`)
	insert("grandchild", 1, now, "custom-model", inclusive)
	// A restart switches the grandchild's profile between its turns.
	usageReportExec(t, connection, "UPDATE sessions SET last_profile='codex2' WHERE session_id='grandchild'")
	missingCacheBreakdown := `{"scope":"turn","total_tokens":1100000,"input_tokens":1000000,"output_tokens":100000,"cached_read_tokens":500000,"cached_write_tokens":0,
        "provider_details":{"model_usage":{"gpt-6.1-sol":{"scope":"turn","total_tokens":1100000,"input_tokens":1000000,"output_tokens":100000}}}}`
	insert("grandchild", 2, now, "custom-model", missingCacheBreakdown)
	insert("root", 4, now, "gpt-6-astra", `{"scope":"last_request","total_tokens":1100000,"input_tokens":1000000,"output_tokens":100000}`)
	insert("root", 5, now, "gpt-6-astra", "null")
	insert("suspended", 1, now-2*86_400_000, "gpt-6-astra", inclusive)
	insert("suspended", 2, now-8*86_400_000, "gpt-6-sol", inclusive)
	insert("suspended", 3, now-31*86_400_000, "gpt-6-astra", inclusive)
	usageMust(t, recordTurnSelection(connection, "root", "running", usageConfiguration(t, map[string]string{"model": "gpt-6-astra"})))
	active := fmt.Sprintf(`{"command_id":"running","turn_start_position":9,"started_at_ms":%d}`, now-10)
	result, err := connection.Exec("UPDATE materialized_sessions SET active_turn_json=?1 WHERE session_id='root'", active)
	usageMust(t, err)
	if updated, _ := result.RowsAffected(); updated != 1 {
		t.Fatalf("active turn rows = %d", updated)
	}
	usageMust(t, connection.Close())
	beforeDestroy := usageReportLoad(t, path, core.SessionUsageQuery{}, now)
	if cost := usageReportRoleCost(t, &beforeDestroy, core.UsageAgentRolePrimary); cost != core.KnownCost(1_050_000_000) {
		t.Fatalf("primary cost = %+v", cost)
	}
	if cost := usageReportRoleCost(t, &beforeDestroy, core.UsageAgentRoleSubagent); cost != core.KnownCost(71_000_000_000) {
		t.Fatalf("subagent cost = %+v", cost)
	}
	if cost := usageReportRoleCost(t, &beforeDestroy, core.UsageAgentRoleReviewer); cost != usageReportUnknown {
		t.Fatalf("reviewer cost = %+v", cost)
	}
	// Park/Close keep child records and their kind, even though the child
	// itself is no longer active. Only the root controls this report filter.
	connection, err = open(path)
	usageMust(t, err)
	usageReportExec(t, connection, "UPDATE sessions SET state='stopped' WHERE session_id IN ('child','grandchild')")
	usageMust(t, connection.Close())
	if parked := usageReportLoad(t, path, core.SessionUsageQuery{}, now); !reflect.DeepEqual(parked.AgentUsage, beforeDestroy.AgentUsage) {
		t.Fatal("stopping children changed the role groups")
	}
	codex2Profile := "codex2"
	filtered := usageReportLoad(t, path, core.SessionUsageQuery{Profile: &codex2Profile}, now)
	if cost := usageReportRoleCost(t, &filtered, core.UsageAgentRolePrimary); cost != core.KnownCost(0) {
		t.Fatalf("filtered primary = %+v", cost)
	}
	if cost := usageReportRoleCost(t, &filtered, core.UsageAgentRoleSubagent); cost != core.KnownCost(71_000_000_000) {
		t.Fatalf("filtered subagent = %+v", cost)
	}
	if cost := usageReportRoleCost(t, &filtered, core.UsageAgentRoleReviewer); cost != usageReportUnknown {
		t.Fatalf("filtered reviewer = %+v", cost)
	}
	usageMust(t, deleteSessionFrom(path, "grandchild"))
	usageMust(t, deleteSessionFrom(path, "child"))

	hourly := usageReportLoad(t, path, core.SessionUsageQuery{}, now)
	if len(hourly.Sessions) != 1 {
		t.Fatalf("sessions = %d", len(hourly.Sessions))
	}
	root := hourly.Sessions[0]
	if root.SubagentCount != 2 {
		t.Fatalf("subagents = %d", root.SubagentCount)
	}
	// Custom-model has no rate, and gpt-6.1-sol's cache counters are
	// unknown.
	if root.CostUsdTicks != usageReportUnknown {
		t.Fatalf("root cost = %+v", root.CostUsdTicks)
	}
	if priced := usageReportPriced(&root); priced != 72_050_000_000 { // $7.10 + $0.105, with caches counted once.
		t.Fatalf("priced = %d", priced)
	}
	if len(root.Models) != 5 {
		t.Fatalf("models = %d", len(root.Models))
	}
	for _, model := range root.Models {
		if model.Model != nil && *model.Model == "gpt-6.1-sol" && !model.CostUsdTicks.Unknown {
			t.Fatal("gpt-6.1-sol's cost must be unknown")
		}
	}
	if !root.Models[len(root.Models)-1].CostUsdTicks.Unknown {
		t.Fatal("an unknown cost sorts last")
	}
	if root.Coverage.FullTurnReports != 4 || root.Coverage.LastRequestReports != 1 || root.Coverage.MissingReports != 1 {
		t.Fatalf("coverage = %+v", root.Coverage)
	}
	if !root.IsPartial() {
		t.Fatal("the root's cost is partial")
	}
	if root.Coverage.UnfinishedTurns != 1 {
		t.Fatalf("unfinished = %d", root.Coverage.UnfinishedTurns)
	}
	if !slices.Equal(hourly.Profiles, []string{"claude", "codex", "codex2"}) {
		t.Fatalf("profiles = %v", hourly.Profiles)
	}
	if hourly.CostUsdTicks != usageReportUnknown {
		t.Fatalf("report cost = %+v", hourly.CostUsdTicks)
	}
	if cost := usageReportRoleCost(t, &hourly, core.UsageAgentRolePrimary); cost != core.KnownCost(1_050_000_000) {
		t.Fatalf("primary cost = %+v", cost)
	}
	if !reflect.DeepEqual(hourly.AgentUsage, beforeDestroy.AgentUsage) {
		t.Fatal("destroying children changed the role groups")
	}

	// A profile counts the turns recorded on it anywhere in the tree, by
	// the profile at turn start, after the children are destroyed.
	on := func(profile string, hourStartMs *int64) core.SessionUsageReport {
		t.Helper()
		return usageReportLoad(t, path, core.SessionUsageQuery{Profile: &profile, HourStartMs: hourStartMs, IncludeSuspended: true}, now)
	}
	codex2 := on("codex2", nil)
	if len(codex2.Sessions) != 1 {
		t.Fatalf("codex2 sessions = %d", len(codex2.Sessions))
	}
	tree := codex2.Sessions[0]
	if tree.SessionID != "root" || tree.Profile != "codex" {
		t.Fatalf("codex2 row = %s on %s", tree.SessionID, tree.Profile)
	}
	if tree.SubagentCount != 2 || tree.CostUsdTicks != usageReportUnknown {
		t.Fatalf("codex2 tree = %+v", tree)
	}
	if priced := usageReportPriced(&tree); priced != 71_000_000_000 {
		t.Fatalf("codex2 priced = %d", priced)
	}
	if tree.Coverage.RecordedTurns != 2 || tree.Coverage.UnfinishedTurns != 0 {
		t.Fatalf("codex2 coverage = %+v", tree.Coverage)
	}
	codex := on("codex", nil)
	if usageReportPriced(&codex.Sessions[0])+usageReportPriced(&tree) != usageReportPriced(&root) {
		t.Fatal("profile shares must add up to the whole")
	}
	if codex.Sessions[0].Coverage.RecordedTurns != 4 || codex.Sessions[0].Coverage.UnfinishedTurns != 1 {
		t.Fatalf("codex coverage = %+v", codex.Sessions[0].Coverage)
	}

	// The default span is the graph's 24 clock hours; its first hour starts
	// inclusively, and an older or later turn stays out.
	if hourly.SinceMs != now-23*core.HourMs || hourly.UntilMs != now {
		t.Fatalf("span = %d..%d", hourly.SinceMs, hourly.UntilMs)
	}
	withSuspended := usageReportLoad(t, path, core.SessionUsageQuery{IncludeSuspended: true}, now)
	if len(withSuspended.Sessions) != 2 {
		t.Fatalf("sessions with suspended = %d", len(withSuspended.Sessions))
	}
	// An unknown cost sorts after every known one, even $0.
	suspended := withSuspended.Sessions[0]
	if suspended.SessionID != "suspended" || suspended.Coverage.RecordedTurns != 0 || suspended.CostUsdTicks != core.KnownCost(0) {
		t.Fatalf("first row = %+v", suspended)
	}
	if !reflect.DeepEqual(withSuspended.Sessions[1], root) {
		t.Fatal("the root row changed with the suspended filter")
	}
	if withSuspended.CostUsdTicks != usageReportUnknown {
		t.Fatalf("cost with suspended = %+v", withSuspended.CostUsdTicks)
	}

	// One bar per hour; each holds the known cost of the turns that
	// completed in it, and together they match the default rows.
	if len(hourly.Hours) != 24 {
		t.Fatalf("bars = %d", len(hourly.Hours))
	}
	if hourly.Hours[0].StartMs != now-23*core.HourMs || hourly.Hours[23].StartMs != now {
		t.Fatalf("bar starts = %d, %d", hourly.Hours[0].StartMs, hourly.Hours[23].StartMs)
	}
	var known uint64
	for _, hour := range hourly.Hours {
		known += hour.KnownCostUsdTicks
	}
	if known != usageReportPriced(&root) {
		t.Fatalf("bars hold %d", known)
	}
	if !hourly.Hours[23].HasUnknownCost || hourly.Hours[0].HasUnknownCost || hourly.Hours[0].KnownCostUsdTicks == 0 {
		t.Fatalf("first and last bars = %+v, %+v", hourly.Hours[0], hourly.Hours[23])
	}
	for _, hour := range hourly.Hours[1:23] {
		if hour != (core.HourlyUsage{StartMs: hour.StartMs}) {
			t.Fatalf("empty hour = %+v", hour)
		}
	}

	// Naming an hour narrows the rows to the turns completed in it, while
	// the bars keep showing the whole graph.
	inHour := func(hour int64) core.SessionUsageReport {
		t.Helper()
		return usageReportLoad(t, path, core.SessionUsageQuery{HourStartMs: &hour}, now)
	}
	first := inHour(now - 23*core.HourMs)
	if !reflect.DeepEqual(first.Hours, hourly.Hours) {
		t.Fatal("bars changed with the selected hour")
	}
	if first.SinceMs != now-23*core.HourMs || first.UntilMs != now-22*core.HourMs-1 {
		t.Fatalf("first hour span = %d..%d", first.SinceMs, first.UntilMs)
	}
	if len(first.Sessions) != 1 || first.Sessions[0].Coverage.RecordedTurns != 1 {
		t.Fatalf("first hour rows = %+v", first.Sessions)
	}
	if first.Sessions[0].CostUsdTicks != core.KnownCost(hourly.Hours[0].KnownCostUsdTicks) {
		t.Fatalf("first hour cost = %+v", first.Sessions[0].CostUsdTicks)
	}
	// The running turn started in the current hour, after this one ended.
	if first.Sessions[0].Coverage.UnfinishedTurns != 0 {
		t.Fatal("the running turn started after the first hour")
	}
	last := inHour(now)
	if last.UntilMs != now || last.Sessions[0].Coverage.UnfinishedTurns != 1 || last.Sessions[0].Coverage.RecordedTurns != 5 {
		t.Fatalf("last hour = %+v", last.Sessions[0].Coverage)
	}
	if usageReportPriced(&last.Sessions[0])+usageReportPriced(&first.Sessions[0]) != usageReportPriced(&root) {
		t.Fatal("hour shares must add up to the whole")
	}
	for _, row := range inHour(now + core.HourMs).Sessions {
		if row.Coverage.RecordedTurns != 0 {
			t.Fatal("a future hour holds no turns")
		}
	}
	// Hours older than the graph are still readable, with no bar.
	beforeGraph := inHour(now - 24*core.HourMs)
	if beforeGraph.Sessions[0].Coverage.RecordedTurns != 1 || usageReportPriced(&beforeGraph.Sessions[0]) != 21_000_000_000 {
		t.Fatalf("hour before the graph = %+v", beforeGraph.Sessions[0])
	}
	claude := "claude"
	twoDaysAgo := now - 2*86_400_000
	twoDays := usageReportLoad(t, path, core.SessionUsageQuery{Profile: &claude, HourStartMs: &twoDaysAgo, IncludeSuspended: true}, now)
	// Root ran no turns on claude, so only the claude parent remains.
	// Cache-discounted Astra: $5 + $0.50 + $5.
	if len(twoDays.Sessions) != 1 {
		t.Fatalf("two days ago sessions = %d", len(twoDays.Sessions))
	}
	if row := twoDays.Sessions[0]; row.CostUsdTicks != core.KnownCost(105_000_000_000) || row.Coverage.RecordedTurns != 1 || !row.Suspended {
		t.Fatalf("two days ago row = %+v", row)
	}
	for _, hour := range twoDays.Hours {
		if hour.KnownCostUsdTicks != 0 {
			t.Fatal("bars do not show turns older than the graph")
		}
	}

	// Renaming a profile carries destroyed children's turns with it.
	_, err = renameProfileReferencesAt(path, "codex2", "codex-two")
	usageMust(t, err)
	if renamed := on("codex-two", nil); !reflect.DeepEqual(renamed.Sessions, codex2.Sessions) {
		t.Fatal("renamed profile lost its turns")
	}
	// A child destroyed before kind retention cannot be classified from its
	// task name. Keep its usage without guessing that it was a worker.
	connection, err = open(path)
	usageMust(t, err)
	usageReportExec(t, connection, "UPDATE subagent_accounting SET kind=NULL WHERE child_session_id='grandchild'")
	usageMust(t, connection.Close())
	historical := usageReportLoad(t, path, core.SessionUsageQuery{}, now)
	var unclassified core.AgentUsageEstimate
	for _, group := range beforeDestroy.AgentUsage {
		if group.Role == core.UsageAgentRoleReviewer {
			unclassified = group
		}
	}
	unclassified.Role = core.UsageAgentRoleUnclassifiedSubagent
	if !slices.ContainsFunc(historical.AgentUsage, func(group core.AgentUsageEstimate) bool {
		return reflect.DeepEqual(group, unclassified)
	}) {
		t.Fatal("the unclassified child's usage is missing")
	}
	if cost := usageReportRoleCost(t, &historical, core.UsageAgentRoleReviewer); cost != core.KnownCost(0) {
		t.Fatalf("reviewer cost = %+v", cost)
	}
	if !reflect.DeepEqual(historical.Sessions, hourly.Sessions) {
		t.Fatal("rows changed with the classification")
	}
}

func TestUsageReportClassifiesRecordedTurnsAndLegacyChildren(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage-roles.sqlite")
	const now int64 = 86_400_000
	root := session("root", "project")
	root.State = core.SessionStateRunning
	usageMust(t, SaveSessionTo(path, &root))
	for _, id := range []string{"orchestrator", "worker", "oracle", "reviewer", "destroyed"} {
		record := session(id, "project")
		usageMust(t, SaveSessionTo(path, &record))
	}

	connection, err := open(path)
	usageMust(t, err)
	usageReportExec(t, connection, `INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name,kind,category)
             VALUES ('orchestrator','root','orchestrator','worker','orchestrator'),
                    ('worker','orchestrator','worker','worker',NULL),
                    ('oracle','orchestrator','oracle','oracle',NULL),
                    ('reviewer','orchestrator','reviewer','reviewer',NULL),
                    ('destroyed','orchestrator','destroyed',NULL,NULL);`)
	// Each turn reports a distinct token count so the totals prove both
	// category precedence and per-turn attribution within one session.
	category := func(value string) *string { return &value }
	turns := []struct {
		session  string
		category *string
		tokens   uint64
	}{
		{"root", category("subtle"), 1},
		{"orchestrator", category("supervisor"), 2},
		{"orchestrator", nil, 4},
		{"orchestrator", category("subtle"), 8},
		{"worker", category("straightforward"), 16},
		{"worker", category("oracle"), 32},
		{"oracle", nil, 64},
		{"worker", category("reviewer"), 128},
		{"worker", category("brief_writer"), 256},
		{"reviewer", nil, 512},
		{"worker", nil, 1024},
		{"worker", category("future_category"), 2048},
		{"destroyed", nil, 4096},
	}
	for index, turn := range turns {
		ordinal := index + 1
		command := fmt.Sprintf("turn-%d", ordinal)
		var outcome core.MaterializedTurnOutcome
		usageMust(t, json.Unmarshal([]byte(fmt.Sprintf(`{
                "command_id": %q, "completed_ordinal": %d, "completed_at_ms": %d,
                "outcome": {"kind":"completed","stop_reason":"EndTurn"},
                "usage": {"scope":"turn", "input_tokens":%d, "output_tokens":0, "total_tokens":%d}
            }`, command, ordinal, now, turn.tokens, turn.tokens)), &outcome))
		body, err := serializeTurnOutcome(&outcome)
		usageMust(t, err)
		usageReportExec(t, connection, `INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,body)
                 VALUES (?1,?2,?3,?4)`, turn.session, command, ordinal, body)
		usageReportExec(t, connection, `INSERT INTO session_turn_selections(session_id,command_id,category)
                 VALUES (?1,?2,?3)`, turn.session, command, turn.category)
	}
	usageMust(t, connection.Close())
	usageMust(t, deleteSessionFrom(path, "destroyed"))

	report := usageReportLoad(t, path, core.SessionUsageQuery{}, now)
	if len(report.Sessions) != 1 || report.Sessions[0].SubagentCount != 5 {
		t.Fatalf("sessions = %+v", report.Sessions)
	}
	type roleInput struct {
		role  core.UsageAgentRole
		input uint64
	}
	var usage []roleInput
	for _, group := range report.AgentUsage {
		var input uint64
		for _, model := range group.Models {
			input += model.Totals["input_tokens"].Tokens
		}
		usage = append(usage, roleInput{group.Role, input})
	}
	expected := []roleInput{
		{core.UsageAgentRolePrimary, 1},
		{core.UsageAgentRoleOrchestrator, 6},
		{core.UsageAgentRoleSubtle, 8},
		{core.UsageAgentRoleStraightforward, 16},
		{core.UsageAgentRoleOracle, 96},
		{core.UsageAgentRoleReviewer, 896},
		{core.UsageAgentRoleSubagent, 3072},
		{core.UsageAgentRoleUnclassifiedSubagent, 4096},
	}
	if !slices.Equal(usage, expected) {
		t.Fatalf("role usage = %+v", usage)
	}
}

type usageReportModelCost struct {
	model string
	cost  core.CostUsdTicks
}

func usageReportModelCosts(session *core.SessionUsageEstimate) []usageReportModelCost {
	var costs []usageReportModelCost
	for _, model := range session.Models {
		costs = append(costs, usageReportModelCost{*model.Model, model.CostUsdTicks})
	}
	return costs
}

// Store integration: Claude records the alias a turn selected; the
// profile's discovered model list says which model that alias runs.
func TestUsageReportPricesClaudeAliasesThroughTheProfileCatalog(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage-report.sqlite")
	const now int64 = 86_400_000
	state := core.NewStateState()
	for _, id := range []string{"aliases", "unresolved"} {
		record := session(id, "project")
		record.LastProfile = "claude4"
		record.State = core.SessionStateRunning
		state.Sessions.Insert(id, record)
	}
	usageMust(t, SaveStateTo(path, &state))
	connection, err := open(path)
	usageMust(t, err)
	var choices []core.SessionConfigChoice
	usageMust(t, json.Unmarshal([]byte(`[
                {"value":"default","name":"Default (recommended)","description":"Sonnet"},
                {"value":"opus","name":"Opus 5.5","description":null},
                {"value":"claude-fable-5-1[1m]","name":"Fable 5.1","description":null},
                {"value":"claude-opus-5","name":"Opus 5","description":null}]`), &choices))
	catalogs := core.ModelCatalogs{"claude4": choices}
	// Claude reports uncached input; caches are separate tallies.
	const usage = `{"scope":"turn","total_tokens":2100000,"input_tokens":1000000,
            "output_tokens":100000,"cached_read_tokens":1000000,"cached_write_tokens":0}`
	for seq, turn := range []struct{ session, model string }{
		{"aliases", "opus"},
		{"aliases", "fable"},
		{"unresolved", "default"},
	} {
		usageReportInsertTurn(t, connection, turn.session, int64(seq), now, turn.model, "", usage)
	}
	usageMust(t, connection.Close())

	report, err := loadWith(path, &core.SessionUsageQuery{}, now, catalogs)
	usageMust(t, err)
	if len(report.Sessions) != 2 {
		t.Fatalf("expected two sessions: %+v", report.Sessions)
	}
	aliases, unresolved := report.Sessions[0], report.Sessions[1]
	if aliases.SessionID != "aliases" {
		t.Fatalf("first session = %s", aliases.SessionID)
	}
	// `opus` is the listed Opus 5.5, not the older Opus 5 also listed;
	// `fable` matches the listed Fable 5.1 the way the bridge does.
	expected := []usageReportModelCost{
		{"claude-fable-5-1", core.KnownCost(152_500_000_000)}, // $10 + $5 + $0.25
		{"claude-opus-5-5", core.KnownCost(62_000_000_000)},   // $4 + $2 + $0.20
	}
	if got := usageReportModelCosts(&aliases); !slices.Equal(got, expected) {
		t.Fatalf("alias models = %+v", got)
	}
	if aliases.CostUsdTicks != core.KnownCost(214_500_000_000) {
		t.Fatalf("aliases cost = %+v", aliases.CostUsdTicks)
	}
	// `default` names no versioned model, so its cost is unknown.
	if model := unresolved.Models[0].Model; model == nil || *model != "default" {
		t.Fatalf("unresolved model = %v", model)
	}
	if unresolved.CostUsdTicks != usageReportUnknown || report.CostUsdTicks != usageReportUnknown {
		t.Fatalf("unresolved cost = %+v, report = %+v", unresolved.CostUsdTicks, report.CostUsdTicks)
	}
}

// Store integration: Antigravity uncached-input turns and Grok Build
// `*-build` model breakdowns both price in the session usage report.
func TestUsageReportPricesAntigravityAndGrokBuildTurns(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage-report.sqlite")
	const now int64 = 86_400_000
	state := core.NewStateState()
	for _, entry := range []struct{ id, profile string }{{"neon-root", "gemini"}, {"grok-child", "grok"}} {
		record := session(entry.id, "project")
		record.LastProfile = entry.profile
		record.State = core.SessionStateRunning
		state.Sessions.Insert(entry.id, record)
	}
	usageMust(t, SaveStateTo(path, &state))
	connection, err := open(path)
	usageMust(t, err)
	usageReportExec(t, connection, `INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name,kind)
             VALUES ('grok-child','neon-root','grok-worker','worker')`)
	// Antigravity reports uncached input; caches are separate tallies.
	usageReportInsertTurn(t, connection, "neon-root", 1, now, "gemini-4-neon", "",
		`{"scope":"turn","total_tokens":2100000,"input_tokens":1000000,
                "output_tokens":100000,"thought_tokens":50000,"cached_read_tokens":1000000,
                "cached_write_tokens":0}`)
	// Grok Build reports `grok-4.7-build` in model_usage when `grok-4.7` is
	// selected, and includes cached reads in input_tokens.
	for _, turn := range []struct {
		seq                int64
		selected, reported string
	}{
		{2, "grok-4.7", "grok-4.7-build"},
		{3, "grok-4.7-build-fast", "grok-4.7-build-fast"},
	} {
		modelRow := `{"scope":"turn","total_tokens":2100000,
                "input_tokens":2000000,"output_tokens":100000,"thought_tokens":50000,
                "cached_read_tokens":1000000,"cached_write_tokens":0}`
		usageReportInsertTurn(t, connection, "grok-child", turn.seq, now, turn.selected, "",
			fmt.Sprintf(`{"scope":"turn","total_tokens":2100000,"input_tokens":2000000,
                    "output_tokens":100000,"thought_tokens":50000,"cached_read_tokens":1000000,
                    "cached_write_tokens":0,"provider_details":{"model_usage":{%q:%s}}}`, turn.reported, modelRow))
	}
	usageMust(t, connection.Close())

	report := usageReportLoad(t, path, core.SessionUsageQuery{}, now)
	if len(report.Sessions) != 1 {
		t.Fatalf("expected one root session: %+v", report.Sessions)
	}
	root := report.Sessions[0]
	if root.SessionID != "neon-root" {
		t.Fatalf("root = %s", root.SessionID)
	}
	expected := []usageReportModelCost{
		{"grok-4.7-build-fast", core.KnownCost(62_000_000_000)}, // $4 + $1.20 + $1.00
		{"gemini-4-neon", core.KnownCost(46_500_000_000)},       // $3 + $1.50 + $0.15
		{"grok-4.7-build", core.KnownCost(31_000_000_000)},      // $2 + $0.60 + $0.50
	}
	if got := usageReportModelCosts(&root); !slices.Equal(got, expected) {
		t.Fatalf("models = %+v", got)
	}
	if root.CostUsdTicks != core.KnownCost(139_500_000_000) || report.CostUsdTicks != core.KnownCost(139_500_000_000) {
		t.Fatalf("costs = %+v, %+v", root.CostUsdTicks, report.CostUsdTicks)
	}
	var primary, subagent *core.AgentUsageEstimate
	for i := range report.AgentUsage {
		switch report.AgentUsage[i].Role {
		case core.UsageAgentRolePrimary:
			primary = &report.AgentUsage[i]
		case core.UsageAgentRoleSubagent:
			subagent = &report.AgentUsage[i]
		}
	}
	if primary == nil || primary.CostUsdTicks != core.KnownCost(46_500_000_000) {
		t.Fatalf("primary group = %+v", primary)
	}
	if model := primary.Models[0].Model; model == nil || *model != "gemini-4-neon" {
		t.Fatalf("primary model = %v", model)
	}
	if subagent == nil || subagent.CostUsdTicks != core.KnownCost(93_000_000_000) {
		t.Fatalf("subagent group = %+v", subagent)
	}
}
