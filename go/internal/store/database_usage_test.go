// Port of the tests in mj-store/src/database/usage.rs.

package store

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// usageCompleted is a completed turn that reported input tokens.
func usageCompleted(t testing.TB, command string, ordinal, input uint64) core.MaterializedTurnOutcome {
	t.Helper()
	var turn core.MaterializedTurnOutcome
	text := fmt.Sprintf(`{"command_id":%q,"completed_ordinal":%d,"completed_at_ms":100,
            "outcome":{"kind":"completed","stop_reason":"EndTurn"},
            "usage":{"scope":"turn","total_tokens":%d,"input_tokens":%d,"output_tokens":10}}`,
		command, ordinal, input+10, input)
	if err := json.Unmarshal([]byte(text), &turn); err != nil {
		t.Fatal(err)
	}
	return turn
}

// usageConfiguration is a projected configuration choosing model and
// effort.
func usageConfiguration(t testing.TB, values map[string]string) *core.SessionConfiguration {
	t.Helper()
	encoded, err := json.Marshal(values)
	if err != nil {
		t.Fatal(err)
	}
	var configuration core.SessionConfiguration
	if err := json.Unmarshal(encoded, &configuration); err != nil {
		t.Fatal(err)
	}
	return &configuration
}

// usageActiveTurn is Some(Some(turn)), a turn that started.
func usageActiveTurn(command string, position uint64) **core.MaterializedTurn {
	turn := &core.MaterializedTurn{CommandID: command, TurnStartPosition: position, StartedAtMs: int64(position)}
	return &turn
}

// usageClearedTurn is Some(None), a turn that ended.
func usageClearedTurn() **core.MaterializedTurn {
	var turn *core.MaterializedTurn
	return &turn
}

// usageApplyEvents applies mutations to sessionID's projection as events
// 1, 2, ..., chained by their digests.
func usageApplyEvents(t testing.TB, path, sessionID string, events []core.MaterializedSessionMutation) {
	t.Helper()
	if _, err := applyProjectionPageTo(path, sessionID, func(page *ProjectionPage) (struct{}, error) {
		for i := range events {
			if _, err := page.Apply(uint64(i)+1, fmt.Sprintf("%064x", i), fmt.Sprintf("%064x", i+1), &events[i]); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, nil
	}); err != nil {
		t.Fatal(err)
	}
}

// usageLegacyTurns is turn_outcome.rs's legacy_turns, which the usage
// regressions share in Rust: one stored turn for every legacy outcome
// shape, with and without a provider diagnostic. It is copied here because
// the usage tests are ported apart from the turn outcome tests.
func usageLegacyTurns(t testing.TB) []core.MaterializedTurnOutcome {
	t.Helper()
	var outcomes []core.TurnOutcomeKind
	for _, stopReason := range []string{
		"EndTurn",
		"Cancelled",
		"error",
		"awaiting_input",
		"QuotaLimit",
		"prompt_unanswered",
		"ModelCapacity",
		"Refusal",
	} {
		outcomes = append(outcomes, core.TurnOutcomeKindCompleted{StopReason: stopReason})
	}
	reason := func(reason core.OutcomeReason) *core.OutcomeReason { return &reason }
	for _, why := range []*core.OutcomeReason{
		nil,
		reason(core.OutcomeReasonLegacyUnclassified),
		reason(core.OutcomeReasonRequestedCancellation),
		reason(core.OutcomeReasonWorkerRestarted),
	} {
		outcomes = append(outcomes, core.TurnOutcomeKindInterrupted{Message: "original interruption", Reason: why})
	}
	for _, why := range []*core.OutcomeReason{
		nil,
		reason(core.OutcomeReasonAdmissionRejected),
		reason(core.OutcomeReasonRuntimeFailure),
	} {
		outcomes = append(outcomes, core.TurnOutcomeKindRejected{Message: "original rejection", Reason: why})
	}
	var turns []core.MaterializedTurnOutcome
	for index, outcome := range outcomes {
		for _, diagnostic := range []bool{false, true} {
			var diagnosticValue any
			if diagnostic {
				diagnosticValue = map[string]any{"message": "different provider diagnostic", "code": "service"}
			}
			encoded, err := json.Marshal(map[string]any{
				"command_id":       fmt.Sprintf("turn-%d-%v", index, diagnostic),
				"accepted_ordinal": index + 1, "turn_start_position": index + 1,
				"completed_ordinal": index + 2, "completed_at_ms": 100,
				"outcome":    outcome,
				"diagnostic": diagnosticValue,
				"usage":      map[string]any{"scope": "turn", "input_tokens": 42, "output_tokens": 10, "total_tokens": 52},
			})
			if err != nil {
				t.Fatal(err)
			}
			var turn core.MaterializedTurnOutcome
			if err := json.Unmarshal(encoded, &turn); err != nil {
				t.Fatal(err)
			}
			turns = append(turns, turn)
		}
	}
	return turns
}

func usageMust(t testing.TB, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func TestSemanticTurnMigrationPreservesAllBilledTurnsAndExactLastTurns(t *testing.T) {
	path := filepath.Join(t.TempDir(), "semantic-turns.sqlite3")
	turns := usageLegacyTurns(t)
	for i := range turns {
		turn := turns[i]
		record := session(turn.CommandID, "project")
		usageMust(t, SaveSessionTo(path, &record))
		materialized := core.MaterializedSessionEmpty(turn.CommandID)
		materialized.LastTurnOutcome = &turn
		usageMust(t, SaveMaterializedSessionTo(path, &materialized))
	}
	connection, err := OpenConnection(path)
	usageMust(t, err)
	for i := range turns {
		body, err := serializeTurnOutcome(&turns[i])
		usageMust(t, err)
		_, err = connection.Exec("INSERT INTO session_turn_usage VALUES (?1,?1,?2,?3,?4)",
			turns[i].CommandID, turns[i].CompletedOrdinal, turns[i].TurnStartPosition, body)
		usageMust(t, err)
	}
	before := make([]core.UsageSummary, len(turns))
	for i := range turns {
		before[i], err = sessionSummary(connection, turns[i].CommandID)
		usageMust(t, err)
	}
	for i := range turns {
		body, err := json.Marshal(&turns[i])
		usageMust(t, err)
		_, err = connection.Exec("UPDATE session_turn_usage SET body=?2 WHERE session_id=?1", turns[i].CommandID, string(body))
		usageMust(t, err)
		_, err = connection.Exec("UPDATE materialized_sessions SET last_turn_outcome_json=?2 WHERE session_id=?1", turns[i].CommandID, string(body))
		usageMust(t, err)
	}
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	restoreReviewStateSchema(t, connection, 81)
	restorePre153TranscriptLayout(t, connection)
	_, err = connection.Exec("DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE session_turn_selections DROP COLUMN profile; DELETE FROM schema_migrations WHERE version>=82; UPDATE schema_compatibility SET minimum_compatible_version=81; PRAGMA user_version=81;")
	usageMust(t, err)
	usageMust(t, connection.Close())
	forgetVerifiedSchema(path)
	connection, err = open(path)
	usageMust(t, err)
	defer connection.Close()
	state, err := readSchemaState(connection)
	usageMust(t, err)
	if state.revision != schemaVersion {
		t.Fatalf("revision = %d", state.revision)
	}
	if state.minimumCompatible == nil || *state.minimumCompatible != minimumCompatibleVersion {
		t.Fatalf("minimum compatible = %v", state.minimumCompatible)
	}
	if state.ensureSupportedBy(81) == nil {
		t.Fatal("revision 81 must not read the migrated store")
	}
	for i := range turns {
		turn := turns[i]
		summary, err := sessionSummary(connection, turn.CommandID)
		usageMust(t, err)
		if !reflect.DeepEqual(summary, before[i]) {
			t.Fatalf("%s: summary changed by the migration", turn.CommandID)
		}
		page, err := loadSessionUsageFrom(path, turn.CommandID, 0, 100)
		usageMust(t, err)
		if page == nil || !reflect.DeepEqual(page.Turns, []core.MaterializedTurnOutcome{turn}) {
			t.Fatalf("%s: page turns = %+v", turn.CommandID, page)
		}
		if page.Coverage.RecordedTurns != 1 {
			t.Fatalf("%s: recorded turns = %d", turn.CommandID, page.Coverage.RecordedTurns)
		}
		if page.Totals["input_tokens"].Tokens != 42 {
			t.Fatalf("%s: input tokens = %d", turn.CommandID, page.Totals["input_tokens"].Tokens)
		}
		projected, err := readMaterializedTurnState(connection, turn.CommandID)
		usageMust(t, err)
		if projected == nil || projected.LastTurnOutcome == nil || !reflect.DeepEqual(*projected.LastTurnOutcome, turn) {
			t.Fatalf("%s: projected outcome = %+v", turn.CommandID, projected)
		}
		var body string
		usageMust(t, connection.QueryRow("SELECT last_turn_outcome_json FROM materialized_sessions WHERE session_id=?1", turn.CommandID).Scan(&body))
		var stored core.ApiTurnOutcome
		usageMust(t, json.Unmarshal([]byte(body), &stored))
		if !reflect.DeepEqual(stored, core.ApiTurnOutcomeFrom(&turn)) {
			t.Fatalf("%s: stored outcome = %+v", turn.CommandID, stored)
		}
	}
}

func TestTurnSelectionsFollowEventOrderAndSurviveCleanupAndReplay(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage.sqlite")
	for _, id := range []string{"root", "child"} {
		record := session(id, "project")
		usageMust(t, SaveSessionTo(path, &record))
	}
	connection, err := open(path)
	usageMust(t, err)
	_, err = connection.Exec("INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name) VALUES ('child','root','investigate')")
	usageMust(t, err)
	usageMust(t, connection.Close())
	one, two := usageCompleted(t, "one", 2, 20), usageCompleted(t, "two", 4, 40)
	events := []core.MaterializedSessionMutation{
		{Configuration: usageConfiguration(t, map[string]string{"model": "sol", "effort": "high"}), ActiveTurn: usageActiveTurn("one", 1)},
		{ActiveTurn: usageClearedTurn(), LastTurnOutcome: &one},
		{Configuration: usageConfiguration(t, map[string]string{"model": "luna", "effort": "low"}), ActiveTurn: usageActiveTurn("two", 3)},
		{ActiveTurn: usageClearedTurn(), LastTurnOutcome: &two},
		{ActiveTurn: usageActiveTurn("crashed", 5)},
	}
	for range 2 {
		usageApplyEvents(t, path, "child", events)
	}
	before, err := loadSessionUsageFrom(path, "child", 0, 1)
	usageMust(t, err)
	if model := before.TurnSelections["one"].Model; model == nil || *model != "sol" {
		t.Fatalf("turn one model = %v", model)
	}
	if len(before.ByModel) != 2 {
		t.Fatalf("models = %d", len(before.ByModel))
	}
	if before.Coverage.UnfinishedTurns != 1 {
		t.Fatalf("unfinished = %d", before.Coverage.UnfinishedTurns)
	}
	if effort := before.ByModel[0].Selection.Effort; effort == nil || *effort != "low" {
		t.Fatalf("first model effort = %v", effort)
	}
	later, err := loadSessionUsageFrom(path, "child", 2, 1)
	usageMust(t, err)
	if model := later.TurnSelections["two"].Model; model == nil || *model != "luna" {
		t.Fatalf("turn two model = %v", model)
	}
	if before.Totals["input_tokens"].Tokens != 60 {
		t.Fatalf("input tokens = %d", before.Totals["input_tokens"].Tokens)
	}
	usageMust(t, deleteSessionFrom(path, "child"))
	usageMust(t, deleteSessionFrom(path, "root"))
	retained, err := loadSessionUsageFrom(path, "child", 0, 1)
	usageMust(t, err)
	if !reflect.DeepEqual(before, retained) {
		t.Fatal("usage must survive session cleanup")
	}
	tree, err := loadUsageTreeFrom(path, "root")
	usageMust(t, err)
	if tree == nil || len(tree.Sessions) != 2 {
		t.Fatalf("tree = %+v", tree)
	}
	if !reflect.DeepEqual(tree.Summary.Totals, before.Totals) {
		t.Fatal("tree totals differ")
	}
	if !reflect.DeepEqual(tree.Summary.ByModel, before.ByModel) {
		t.Fatal("tree models differ")
	}
	if tree.Summary.Coverage.UnfinishedTurns != 1 {
		t.Fatalf("tree unfinished = %d", tree.Summary.Coverage.UnfinishedTurns)
	}
	for _, member := range tree.Sessions {
		if member.OperationalSessionPresent {
			t.Fatalf("%s is still operational", member.SessionID)
		}
	}
	if task := tree.Sessions[0].TaskName; task == nil || *task != "investigate" {
		t.Fatalf("task name = %v", task)
	}
}

// Hard-won: #1554: a reviewer's projected configuration can omit the
// effort its harness was launched with, leaving session_turn_selections
// without the effort the review-arms eval reads.
func TestReviewerTurnSelectionsFallBackToTheRegisteredLaunchSettings(t *testing.T) {
	path := filepath.Join(t.TempDir(), "reviewer-selection.sqlite")
	parent := session("root", "project")
	usageMust(t, SaveSessionTo(path, &parent))
	child := session("reviewer", "project")
	var relation core.SubagentRecord
	usageMust(t, json.Unmarshal([]byte(fmt.Sprintf(`{
            "kind": "reviewer",
            "child_session_id": %q,
            "parent_session_id": %q,
            "task_name": "review",
            "profile_id": "codex",
            "model": "gpt-6-terra",
            "effort": "xhigh",
            "working_directory": "",
            "initial_prompt": "review",
            "request_key": "review-key",
            "created_at": "2026-11-16T00:00:00Z",
            "handback_tool": true
        }`, child.ID, parent.ID)), &relation))
	usageMust(t, registerTestSubagent(path, &child, &relation))
	usageApplyEvents(t, path, child.ID, []core.MaterializedSessionMutation{
		{ActiveTurn: usageActiveTurn("review-turn-1", 1)},
		{Configuration: usageConfiguration(t, map[string]string{"model": "sol", "effort": "low"}), ActiveTurn: usageActiveTurn("review-turn-2", 3)},
	})
	connection, err := open(path)
	usageMust(t, err)
	selection := func(command string) [2]*string {
		t.Helper()
		var model, effort *string
		usageMust(t, connection.QueryRow(`SELECT model, effort FROM session_turn_selections
                 WHERE session_id=?1 AND command_id=?2`, child.ID, command).Scan(&model, &effort))
		return [2]*string{model, effort}
	}
	expect := func(got [2]*string, model, effort string) {
		t.Helper()
		if got[0] == nil || *got[0] != model || got[1] == nil || *got[1] != effort {
			t.Fatalf("selection = %v, %v; want %s, %s", got[0], got[1], model, effort)
		}
	}
	expect(selection("review-turn-1"), "gpt-6-terra", "xhigh")
	expect(selection("review-turn-2"), "sol", "low")
	usageMust(t, connection.Close())
	usageMust(t, deleteSessionFrom(path, child.ID))
	connection, err = open(path)
	usageMust(t, err)
	defer connection.Close()
	expect(selection("review-turn-1"), "gpt-6-terra", "xhigh")
}

func TestTreeKeepsRegisteredDescendantsWhenOperationalStateIsRemoved(t *testing.T) {
	path := filepath.Join(t.TempDir(), "tree.sqlite")
	state := core.NewStateState()
	for _, id := range []string{"root", "child", "grandchild"} {
		state.Sessions.Insert(id, session(id, "project"))
	}
	usageMust(t, SaveStateTo(path, &state))
	{
		child, parent := "child", "root"
		var relation core.SubagentRecord
		usageMust(t, json.Unmarshal([]byte(fmt.Sprintf(`{
                "child_session_id":%q,"parent_session_id":%q,"task_name":%q,
                "profile_id":"codex","working_directory":"","initial_prompt":"probe",
                "request_key":%q,"created_at":"2026-09-30T00:00:00Z"
            }`, child, parent, child, child)), &relation))
		usageMust(t, registerTestSubagent(path, state.Sessions.Index(child), &relation))
		state.Subagents.Insert(child, core.SubagentMetadataFrom(&relation))
	}
	usageMust(t, SaveStateTo(path, &state))
	// Operational children cannot delegate today; retained accounting
	// still traverses deeper historical/imported relationships.
	connection, err := open(path)
	usageMust(t, err)
	_, err = connection.Exec("INSERT INTO subagent_accounting(child_session_id,parent_session_id,task_name) VALUES ('grandchild','child','nested')")
	usageMust(t, err)
	usageMust(t, connection.Close())
	before, err := loadUsageTreeFrom(path, "root")
	usageMust(t, err)
	if before == nil || len(before.Sessions) != 3 {
		t.Fatalf("tree before = %+v", before)
	}
	for id := range state.Sessions.Keys() {
		usageMust(t, deleteSessionFrom(path, id))
	}
	after, err := loadUsageTreeFrom(path, "root")
	usageMust(t, err)
	if after == nil || len(after.Sessions) != 3 {
		t.Fatalf("tree after = %+v", after)
	}
	for _, member := range after.Sessions {
		if member.OperationalSessionPresent {
			t.Fatalf("%s is still operational", member.SessionID)
		}
	}
}

// usageGroupTotals is the totals groups holds for selection, nil when none.
func usageGroupTotals(groups usageGroups, selection core.UsageSelection) map[string]core.UsageCounterTotal {
	for _, group := range groups {
		if group.selection.Compare(selection) == 0 {
			return group.totals
		}
	}
	return nil
}

func TestProviderBreakdownsPartitionCountersWithoutDoubleCountingOrGuessing(t *testing.T) {
	usage := *usageCompleted(t, "one", 1, 100).Usage
	first := usage
	first.InputTokens = 30
	first.OutputTokens = 4
	first.TotalTokens = 34
	second := first
	second.InputTokens = 60
	second.OutputTokens = 6
	second.TotalTokens = 66
	usage.ProviderDetails = &core.ProviderTurnUsage{
		ModelUsage: map[string]core.TokenUsage{"a": first, "b": second},
	}
	modelA, modelB, high := "a", "b", "high"
	selected := core.UsageSelection{Model: &modelA, Effort: &high}
	var groups usageGroups
	usageMust(t, attribute(&usage, selected, &groups))
	if got := usageGroupTotals(groups, selected)["input_tokens"].Tokens; got != 30 {
		t.Fatalf("selected input = %d", got)
	}
	if got := usageGroupTotals(groups, core.UsageSelection{Model: &modelB})["input_tokens"].Tokens; got != 60 {
		t.Fatalf("other model input = %d", got)
	}
	if got := usageGroupTotals(groups, core.UsageSelection{})["input_tokens"].Tokens; got != 10 {
		t.Fatalf("unknown remainder = %d", got)
	}
	for counter, total := range counters(&usage) {
		var sum uint64
		for _, group := range groups {
			sum += group.totals[counter].Tokens
		}
		if sum != total {
			t.Fatalf("%s: attributed %d of %d", counter, sum, total)
		}
	}
	b := usage.ProviderDetails.ModelUsage["b"]
	b.Scope = core.UsageScopeLastRequest
	usage.ProviderDetails.ModelUsage["b"] = b
	groups = nil
	usageMust(t, attribute(&usage, selected, &groups))
	if len(groups) != 1 {
		t.Fatalf("groups = %d", len(groups))
	}
	if got := usageGroupTotals(groups, core.UsageSelection{})["input_tokens"].Tokens; got != 100 {
		t.Fatalf("unknown input = %d", got)
	}
}

func TestUsageSurvivesReopeningAndReplayWithHonestCoverage(t *testing.T) {
	path := filepath.Join(t.TempDir(), "usage.sqlite")
	connection, err := open(path)
	usageMust(t, err)
	// Use the same projection setup as the materialized database tests.
	usageMust(t, connection.Close())
	record := session("usage", "project")
	usageMust(t, SaveSessionTo(path, &record))

	var turns []core.MaterializedTurnOutcome
	turnScope, lastRequest, unspecified := core.UsageScopeTurn, core.UsageScopeLastRequest, core.UsageScopeUnspecified
	for i, scope := range []*core.UsageScope{&turnScope, &turnScope, &lastRequest, nil, &unspecified} {
		code := "provider.auth_error"
		status := uint16(403)
		start := uint64(i) + 1
		turn := core.MaterializedTurnOutcome{
			Diagnostic: &core.TurnDiagnostic{
				Message:    "Usage limit exceeded",
				Code:       &code,
				HTTPStatus: &status,
			},
			CommandID:         fmt.Sprintf("p%d", i),
			TurnStartPosition: &start,
			CompletedOrdinal:  uint64(i) + 1,
			CompletedAtMs:     100,
			Outcome:           core.TurnOutcomeKindCompleted{StopReason: "EndTurn"},
		}
		if scope != nil {
			calls, duration, elapsed := uint64(2), uint64(1700), uint64(2377)
			details := &core.ProviderTurnUsage{
				ModelCalls:    &calls,
				APIDurationMs: &duration,
				ElapsedMs:     &elapsed,
				ModelUsage:    map[string]core.TokenUsage{},
			}
			if i != 0 {
				cost := core.ProviderTurnCostFromUsdTicks(88_767_200, false)
				details.Cost = &cost
			}
			cached := uint64(0)
			if i == 0 {
				cached = 16
			}
			turn.Usage = &core.TokenUsage{
				ProviderDetails:  details,
				Scope:            *scope,
				TotalTokens:      30,
				InputTokens:      20,
				OutputTokens:     10,
				CachedReadTokens: &cached,
			}
		}
		turns = append(turns, turn)
	}
	for range 2 {
		if _, err := applyProjectionPageTo(path, "usage", func(page *ProjectionPage) (struct{}, error) {
			for i := range turns {
				turn := turns[i]
				prior := core.RelayEventGenesisDigest
				if turn.CompletedOrdinal != 1 {
					prior = fmt.Sprintf("%064x", turn.CompletedOrdinal-1)
				}
				if _, err := page.Apply(turn.CompletedOrdinal, prior, fmt.Sprintf("%064x", turn.CompletedOrdinal),
					&core.MaterializedSessionMutation{
						LastTurnOutcome: &turn,
						ProviderCost:    &core.ProviderCost{Amount: 1.25, Currency: "USD", ObservedAtMs: 100},
					}); err != nil {
					return struct{}{}, err
				}
			}
			return struct{}{}, nil
		}); err != nil {
			t.Fatal(err)
		}
	}
	connection, err = open(path)
	usageMust(t, err)
	_, err = connection.Exec(`INSERT INTO session_turn_selections(session_id, command_id, model, effort, profile)
             VALUES ('usage', 'p0', 'deepseek-flash', NULL, 'deepseek')`)
	usageMust(t, err)
	usageMust(t, connection.Close())
	first, err := loadSessionUsageFrom(path, "usage", 0, 2)
	usageMust(t, err)
	if len(first.Turns) != 2 {
		t.Fatalf("first page turns = %d", len(first.Turns))
	}
	details := first.Turns[0].Usage.ProviderDetails
	if details.Cost != nil {
		t.Fatal("the first turn reported no provider cost")
	}
	if details.ElapsedMs == nil || *details.ElapsedMs != 2377 {
		t.Fatalf("elapsed = %v", details.ElapsedMs)
	}
	if status := first.Turns[0].Diagnostic.HTTPStatus; status == nil || *status != 403 {
		t.Fatalf("status = %v", status)
	}
	if first.ProviderSessionCost == nil || first.ProviderSessionCost.Amount != 1.25 {
		t.Fatalf("session cost = %+v", first.ProviderSessionCost)
	}
	coverage := first.Coverage
	for name, pair := range map[string][2]uint64{
		"recorded_turns":       {coverage.RecordedTurns, 5},
		"full_turn_reports":    {coverage.FullTurnReports, 2},
		"last_request_reports": {coverage.LastRequestReports, 1},
		"unspecified_reports":  {coverage.UnspecifiedReports, 1},
		"missing_reports":      {coverage.MissingReports, 1},
		"provider_cost_turns":  {coverage.ProviderCostTurns, 1},
		"estimated_cost_turns": {coverage.EstimatedCostTurns, 1},
		"missing_cost_turns":   {coverage.MissingCostTurns, 0},
	} {
		if pair[0] != pair[1] {
			t.Fatalf("%s = %d, want %d", name, pair[0], pair[1])
		}
	}
	// Hard-won: #1449: a cached DeepSeek turn had tokens but no cost.
	if first.TurnCostUsdTicks != 88_767_200+132_960 {
		t.Fatalf("turn cost = %d", first.TurnCostUsdTicks)
	}
	// Only the two whole-turn reports are summed: the last-request and
	// unspecified turns stay out of the totals.
	if total := first.Totals["total_tokens"]; total.Tokens != 60 || total.ReportedTurns != 2 {
		t.Fatalf("total tokens = %+v", total)
	}
	if cached := first.Totals["cached_read_tokens"].Tokens; cached != 16 {
		t.Fatalf("cached read = %d", cached)
	}
	if _, ok := first.Totals["thought_tokens"]; ok {
		t.Fatal("no turn reported thought tokens")
	}
	next, err := loadSessionUsageFrom(path, "usage", first.NextAfterSeq, 2)
	usageMust(t, err)
	if len(next.Turns) != 2 || next.LatestSeq != 5 || !reflect.DeepEqual(next.Totals, first.Totals) {
		t.Fatalf("next page = %d turns, latest %d", len(next.Turns), next.LatestSeq)
	}
	last, err := loadSessionUsageFrom(path, "usage", next.NextAfterSeq, 2)
	usageMust(t, err)
	if len(last.Turns) != 1 || last.Turns[0].Usage.Scope != core.UsageScopeUnspecified {
		t.Fatalf("last page = %+v", last.Turns)
	}
	if last.NextAfterSeq != last.LatestSeq {
		t.Fatalf("next after %d, latest %d", last.NextAfterSeq, last.LatestSeq)
	}
	if !reflect.DeepEqual(last.Totals, first.Totals) {
		t.Fatal("totals differ between pages")
	}
}

func TestRecentPlacementsRebuildIdenticallyFromSqlite(t *testing.T) {
	path := filepath.Join(t.TempDir(), "placements.sqlite3")
	connection, err := open(path)
	usageMust(t, err)
	var workspace string
	usageMust(t, connection.QueryRow("SELECT workspace_id FROM workspaces LIMIT 1").Scan(&workspace))
	const base int64 = 1_795_000_000
	insert := func(id, profile string, timestamp int64) {
		t.Helper()
		created := usageRFC3339Seconds(timestamp)
		_, err := connection.Exec("INSERT INTO session_contexts(session_id, bundle_id, workspace_id, created_at, profile_id) VALUES (?1, 'b1', ?2, ?3, ?4)",
			id, workspace, created, profile)
		usageMust(t, err)
		_, err = connection.Exec(`INSERT INTO sessions(session_id, title, harness_kind, last_profile, target_template_id, state, updated_at)
                 VALUES (?1, 't', 'codex', ?2, 'local', 'running', ?3)`, id, profile, created)
		usageMust(t, err)
	}
	insert("old-primary", "codex1", base-100)
	insert("new-primary", "codex1", base+10)
	insert("new-child", "codex1", base+20)
	_, err = connection.Exec(`INSERT INTO subagent_accounting(child_session_id, parent_session_id, task_name, category)
             VALUES ('new-child', 'new-primary', 'worker', 'subtle')`)
	usageMust(t, err)
	usageMust(t, connection.Close())

	expected := []RecentPlacementRow{
		{SessionID: "new-primary", ProfileID: "codex1", Category: "primary", CreatedAtEpochSeconds: base + 10},
		{SessionID: "new-child", ProfileID: "codex1", Category: "subtle", CreatedAtEpochSeconds: base + 20},
	}
	rebuilt, err := LoadRecentPlacementsSinceFrom(path, base)
	usageMust(t, err)
	if !reflect.DeepEqual(rebuilt, expected) {
		t.Fatalf("placements = %+v", rebuilt)
	}
	connection, err = open(path)
	usageMust(t, err)
	_, err = connection.Exec("DELETE FROM sessions WHERE session_id='new-child'")
	usageMust(t, err)
	usageMust(t, connection.Close())
	rebuilt, err = LoadRecentPlacementsSinceFrom(path, base)
	usageMust(t, err)
	if !reflect.DeepEqual(rebuilt, expected) {
		t.Fatalf("placements after deleting the child = %+v", rebuilt)
	}
}
