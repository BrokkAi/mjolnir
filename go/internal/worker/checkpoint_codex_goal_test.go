// Port of the tests of mj-worker/src/checkpoint/codex_goal.rs.

package worker

import (
	"database/sql"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// codexGoalTestOpen is rusqlite's Connection::open: read-write, creating the
// database when it is missing.
func codexGoalTestOpen(t *testing.T, path string) *sql.DB {
	t.Helper()
	database, err := sql.Open("sqlite", codexGoalURI(path, "mode=rwc"))
	if err != nil {
		t.Fatal(err)
	}
	database.SetMaxOpenConns(1)
	t.Cleanup(func() { database.Close() })
	return database
}

func codexGoalTestFixture(t *testing.T, home string, threadID string, objective string) codexGoal {
	t.Helper()
	database := codexGoalTestOpen(t, filepath.Join(home, codexGoalDatabase))
	transaction, err := database.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if err := codexGoalInitializeSchema(transaction); err != nil {
		t.Fatal(err)
	}
	budget := int64(9000)
	goal := codexGoal{
		GoalID:          "identity-" + threadID,
		Objective:       objective,
		Status:          "paused",
		TokenBudget:     &budget,
		TokensUsed:      1234,
		TimeUsedSeconds: 56,
		CreatedAtMs:     1234567,
		UpdatedAtMs:     2345678,
	}
	if _, err := transaction.Exec("INSERT INTO thread_goals VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
		threadID, goal.GoalID, goal.Objective, goal.Status, goal.TokenBudget, goal.TokensUsed,
		goal.TimeUsedSeconds, goal.CreatedAtMs, goal.UpdatedAtMs); err != nil {
		t.Fatal(err)
	}
	if _, err := transaction.Exec("INSERT INTO thread_goal_continuation_deferrals VALUES (?)", threadID); err != nil {
		t.Fatal(err)
	}
	if err := transaction.Commit(); err != nil {
		t.Fatal(err)
	}
	database.Close()
	return goal
}

func codexGoalTestCollect(t *testing.T, home, threadID string) (core.NativeArtifact, []byte) {
	t.Helper()
	artifact, ok, err := codexGoalCollect(home, threadID)
	if err != nil || !ok {
		t.Fatalf("collect = %v, %v", ok, err)
	}
	data, err := core.NativeArtifactDataBytesOf(artifact.Data)
	if err != nil {
		t.Fatal(err)
	}
	return artifact, data
}

func codexGoalTestRead(t *testing.T, database *sql.DB, threadID string) *codexGoal {
	t.Helper()
	goal, err := codexGoalRead(database, threadID)
	if err != nil {
		t.Fatal(err)
	}
	return goal
}

func codexGoalTestExpect(t *testing.T, database *sql.DB, threadID string, expected *codexGoal) {
	t.Helper()
	if got := codexGoalTestRead(t, database, threadID); !reflect.DeepEqual(got, expected) {
		t.Fatalf("goal %s = %+v, want %+v", threadID, got, expected)
	}
}

func TestGoalCheckpointRestoresAccountingAndOnlyTheSelectedSession(t *testing.T) {
	source := t.TempDir()
	target := t.TempDir()
	goal := codexGoalTestFixture(t, source, "selected", "finish campaign")
	codexGoalTestFixture(t, source, "private-other", "unrelated private objective")
	other := codexGoalTestFixture(t, target, "existing-other", "keep target session")
	artifact, artifactBytes := codexGoalTestCollect(t, source, "selected")
	if strings.Contains(fromUTF8Lossy(artifactBytes), "unrelated private") {
		t.Fatal("the artifact leaks another session's goal")
	}
	// Partial recovery must preserve any current native database, including
	// goals owned by other conversations in this home.
	if restored, err := codexGoalRestoreMissing(target, "selected", artifact.RelativePath, artifactBytes); err != nil || restored {
		t.Fatalf("restore missing = %v, %v", restored, err)
	}
	retained := codexGoalTestOpen(t, filepath.Join(target, codexGoalDatabase))
	codexGoalTestExpect(t, retained, "selected", nil)
	codexGoalTestExpect(t, retained, "existing-other", &other)
	missing := t.TempDir()
	if restored, err := codexGoalRestoreMissing(missing, "selected", artifact.RelativePath, artifactBytes); err != nil || !restored {
		t.Fatalf("restore missing = %v, %v", restored, err)
	}
	if restored, err := codexGoalRestoreMissing(missing, "selected", artifact.RelativePath, artifactBytes); err != nil || restored {
		t.Fatalf("restore missing = %v, %v", restored, err)
	}
	if err := codexGoalRestore(target, "selected", artifact.RelativePath, artifactBytes); err != nil {
		t.Fatal(err)
	}
	restored := codexGoalTestOpen(t, filepath.Join(target, codexGoalDatabase))
	codexGoalTestExpect(t, restored, "selected", &goal)
	codexGoalTestExpect(t, restored, "existing-other", &other)
	codexGoalTestExpect(t, restored, "private-other", nil)
	var deferrals int64
	if err := restored.QueryRow("SELECT count(*) FROM thread_goal_continuation_deferrals WHERE thread_id='selected'").Scan(&deferrals); err != nil || deferrals != 1 {
		t.Fatalf("deferrals = %d, %v", deferrals, err)
	}
	fresh := t.TempDir()
	if err := codexGoalRestore(fresh, "selected", artifact.RelativePath, artifactBytes); err != nil {
		t.Fatal(err)
	}
	freshDB := codexGoalTestOpen(t, filepath.Join(fresh, codexGoalDatabase))
	if version, err := codexGoalSchemaVersion(freshDB); err != nil || version != 2 {
		t.Fatalf("schema version = %d, %v", version, err)
	}
	codexGoalTestExpect(t, freshDB, "selected", &goal)
}

func TestClearedGoalCheckpointRemovesOnlyThatGoalAndRejectsForeignIdentity(t *testing.T) {
	source := t.TempDir()
	target := t.TempDir()
	codexGoalTestFixture(t, source, "other", "other")
	original := codexGoalTestFixture(t, target, "selected", "finish")
	artifact, artifactBytes := codexGoalTestCollect(t, source, "selected")
	if err := codexGoalRestore(target, "different", artifact.RelativePath, artifactBytes); err == nil {
		t.Fatal("a foreign identity must be rejected")
	}
	database := codexGoalTestOpen(t, filepath.Join(target, codexGoalDatabase))
	codexGoalTestExpect(t, database, "selected", &original)
	if err := codexGoalRestore(target, "selected", artifact.RelativePath, artifactBytes); err != nil {
		t.Fatal(err)
	}
	codexGoalTestExpect(t, database, "selected", nil)
	var deferrals int64
	if err := database.QueryRow("SELECT count(*) FROM thread_goal_continuation_deferrals").Scan(&deferrals); err != nil || deferrals != 0 {
		t.Fatalf("deferrals = %d, %v", deferrals, err)
	}
}

func TestUnknownNativeGoalSchemaIsRejectedWithoutChangingExistingGoals(t *testing.T) {
	source := t.TempDir()
	target := t.TempDir()
	codexGoalTestFixture(t, source, "selected", "new snapshot")
	original := codexGoalTestFixture(t, target, "selected", "existing snapshot")
	artifact, artifactBytes := codexGoalTestCollect(t, source, "selected")
	database := codexGoalTestOpen(t, filepath.Join(target, codexGoalDatabase))
	if _, err := database.Exec("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version=2"); err != nil {
		t.Fatal(err)
	}
	if err := codexGoalRestore(target, "selected", artifact.RelativePath, artifactBytes); err == nil {
		t.Fatal("an unknown schema must be rejected")
	}
	codexGoalTestExpect(t, database, "selected", &original)
	if _, _, err := codexGoalCollect(target, "selected"); err == nil {
		t.Fatal("an unknown schema must not be collected")
	}
}

func TestCheckpointGoalIntoFreshLiveHome(t *testing.T) {
	source, sourceOK := os.LookupEnv("CODEX_GOAL_SOURCE_HOME")
	target, targetOK := os.LookupEnv("CODEX_GOAL_TARGET_HOME")
	threadID, threadOK := os.LookupEnv("CODEX_GOAL_THREAD_ID")
	if !sourceOK || !targetOK || !threadOK {
		t.Skip("requires disposable CODEX_GOAL_SOURCE_HOME, CODEX_GOAL_TARGET_HOME and CODEX_GOAL_THREAD_ID for a packaged native recovery probe")
	}
	if source == target {
		t.Fatal("source and target homes must differ")
	}
	if _, err := os.Stat(filepath.Join(target, codexGoalDatabase)); err == nil {
		t.Fatal("the target home already has a goal database")
	}
	artifact, artifactBytes := codexGoalTestCollect(t, source, threadID)
	if err := codexGoalRestore(target, threadID, artifact.RelativePath, artifactBytes); err != nil {
		t.Fatal(err)
	}
}
