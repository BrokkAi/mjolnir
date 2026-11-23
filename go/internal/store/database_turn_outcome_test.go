// Port of the tests in mj-store/src/database/turn_outcome.rs.

package store

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// legacyTurns is one stored turn for every legacy outcome shape, with and
// without a provider diagnostic.
func legacyTurns(t testing.TB) []core.MaterializedTurnOutcome {
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

func TestStoredSemanticTurnsPreserveRawReasonsAndDiagnosticsExactly(t *testing.T) {
	for _, turn := range legacyTurns(t) {
		body, err := serializeTurnOutcome(&turn)
		if err != nil {
			t.Fatal(err)
		}
		decoded, err := deserializeTurnOutcome(body)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(decoded, turn) {
			t.Fatalf("round trip changed the turn:\n got %#v\nwant %#v", decoded, turn)
		}
		var raw core.MaterializedTurnOutcome
		if json.Unmarshal([]byte(body), &raw) == nil {
			t.Fatalf("a stored body must not decode as a raw turn: %s", body)
		}
		var semantic core.ApiTurnOutcome
		if err := json.Unmarshal([]byte(body), &semantic); err != nil {
			t.Fatal(err)
		}
		if want := core.ApiTurnOutcomeFrom(&turn); !reflect.DeepEqual(semantic, want) {
			t.Fatalf("semantic outcome:\n got %#v\nwant %#v", semantic, want)
		}
		if completed, ok := turn.Outcome.(core.TurnOutcomeKindCompleted); ok && completed.StopReason == "Cancelled" {
			if semantic.Outcome.Kind != core.TurnResultKindCancelled {
				t.Fatalf("a cancelled stop reason reads as %v", semantic.Outcome.Kind)
			}
		}
	}
}

func TestTurnMigrationRollsBackBothColumnsAndRevisionOnInvalidBody(t *testing.T) {
	path := filepath.Join(t.TempDir(), "rollback.sqlite3")
	record := session("session", "project")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
	connection, err := OpenConnection(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	legacyBytes, err := databaseMarshalJSON(legacyTurns(t)[0])
	if err != nil {
		t.Fatal(err)
	}
	legacy := string(legacyBytes)
	if _, err := connection.Exec("INSERT INTO session_turn_usage VALUES ('session','turn',2,1,?1)", legacy); err != nil {
		t.Fatal(err)
	}
	if _, err := connection.Exec("UPDATE materialized_sessions SET last_turn_outcome_json='{}' WHERE session_id='session'"); err != nil {
		t.Fatal(err)
	}
	restoreGithubV84Schema(t, connection)
	restorePre101SubagentPreference(t, connection)
	if _, err := connection.Exec("ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; DELETE FROM schema_migrations WHERE version>=82; UPDATE schema_compatibility SET minimum_compatible_version=81; PRAGMA user_version=81;"); err != nil {
		t.Fatal(err)
	}
	err = migrateTurnOutcomeSchema(connection)
	if err == nil || !strings.Contains(err.Error(), "materialized last turn for session") {
		t.Fatalf("migration error = %v", err)
	}
	var body string
	if err := connection.QueryRow("SELECT body FROM session_turn_usage").Scan(&body); err != nil {
		t.Fatal(err)
	}
	if body != legacy {
		t.Fatalf("usage body = %s", body)
	}
	var last string
	if err := connection.QueryRow("SELECT last_turn_outcome_json FROM materialized_sessions").Scan(&last); err != nil {
		t.Fatal(err)
	}
	if last != "{}" {
		t.Fatalf("last turn = %s", last)
	}
	state, err := readSchemaState(connection)
	if err != nil {
		t.Fatal(err)
	}
	if state.revision != 81 {
		t.Fatalf("revision = %d", state.revision)
	}
	if state.minimumCompatible == nil || *state.minimumCompatible != 81 {
		t.Fatalf("minimum compatible = %v", state.minimumCompatible)
	}
}

func TestMigrateCopiedLiveTurnOutcomes(t *testing.T) {
	path := os.Getenv("MJ_TURN_OUTCOME_COPY")
	if path == "" {
		t.Skip("requires MJ_TURN_OUTCOME_COPY pointing to an isolated read-only-source table copy")
	}
	parent := filepath.Dir(path)
	if parent == "" {
		t.Fatal("copied database needs a parent directory")
	}
	if err := core.EnsureMayControlStore(parent, "migrate the copied turn-outcome database"); err != nil {
		t.Fatal(err)
	}
	connection, err := OpenConnection(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	before := turnOutcomeCopiedRows(t, connection)
	if err := migrateTurnOutcomeSchema(connection); err != nil {
		t.Fatal(err)
	}
	after := turnOutcomeCopiedRows(t, connection)
	if len(before) != len(after) {
		t.Fatalf("%d rows before, %d after", len(before), len(after))
	}
	absentReasons, diagnostics := 0, 0
	for index := range before {
		if before[index][0] != after[index][0] {
			t.Fatalf("%s != %s", before[index][0], after[index][0])
		}
		var original core.MaterializedTurnOutcome
		if err := json.Unmarshal([]byte(before[index][1]), &original); err != nil {
			t.Fatal(err)
		}
		migrated, err := deserializeTurnOutcome(after[index][1])
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(migrated, original) {
			t.Fatalf("%s changed", before[index][0])
		}
		var semantic core.ApiTurnOutcome
		if err := json.Unmarshal([]byte(after[index][1]), &semantic); err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(semantic, core.ApiTurnOutcomeFrom(&original)) {
			t.Fatalf("%s semantic outcome differs", before[index][0])
		}
		if interrupted, ok := original.Outcome.(core.TurnOutcomeKindInterrupted); ok && interrupted.Reason == nil {
			absentReasons++
		}
		if original.Diagnostic != nil {
			diagnostics++
		}
	}
	fmt.Printf("Verified all copied turns exactly; absent interruption reasons=%d, diagnostics=%d\n", absentReasons, diagnostics)
	state, err := readSchemaState(connection)
	if err != nil {
		t.Fatal(err)
	}
	if state.revision != 82 {
		t.Fatalf("revision = %d", state.revision)
	}
}

func turnOutcomeCopiedRows(t *testing.T, connection *Connection) [][2]string {
	t.Helper()
	rows, err := connection.Query("SELECT 'usage:' || session_id || '/' || command_id, body FROM session_turn_usage UNION ALL SELECT 'last:' || session_id, last_turn_outcome_json FROM materialized_sessions WHERE last_turn_outcome_json IS NOT NULL ORDER BY 1")
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	var collected [][2]string
	for rows.Next() {
		var row [2]string
		if err := rows.Scan(&row[0], &row[1]); err != nil {
			t.Fatal(err)
		}
		collected = append(collected, row)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	return collected
}
