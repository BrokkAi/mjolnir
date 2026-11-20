// Port of mj-worker/src/checkpoint/codex_goal.rs.

// Session-scoped native goal checkpoints. Codex stores accounting outside
// rollouts.

package worker

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const codexGoalDatabase = "goals_1.sqlite"

const codexGoalArtifactRoot = core.NativeArtifactRoot

// codexGoalMigrations match the pinned Codex goals database migrations,
// including their checksums: each is a description and its SQL.
var codexGoalMigrations = [2][2]string{
	{
		"thread goals",
		`CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active',
        'paused',
        'blocked',
        'usage_limited',
        'budget_limited',
        'complete'
    )),
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
`,
	},
	{
		"thread goal continuation deferrals",
		`CREATE TABLE thread_goal_continuation_deferrals (
    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE
);
`,
	},
}

// codexGoal is one thread's goal row. It refuses unknown fields.
type codexGoal struct {
	GoalID          string `json:"goal_id"`
	Objective       string `json:"objective"`
	Status          string `json:"status"`
	TokenBudget     *int64 `json:"token_budget"`
	TokensUsed      int64  `json:"tokens_used"`
	TimeUsedSeconds int64  `json:"time_used_seconds"`
	CreatedAtMs     int64  `json:"created_at_ms"`
	UpdatedAtMs     int64  `json:"updated_at_ms"`
}

// UnmarshalJSON refuses unknown fields and requires every non-Option field,
// as serde's deny_unknown_fields derive does.
func (g *codexGoal) UnmarshalJSON(data []byte) error {
	type plain codexGoal
	return codexGoalDecodeStrict(data, (*plain)(g),
		"goal_id", "objective", "status", "tokens_used", "time_used_seconds", "created_at_ms", "updated_at_ms")
}

// codexGoalSnapshot is the checkpoint artifact for one thread. It refuses
// unknown fields.
type codexGoalSnapshot struct {
	Version              uint32     `json:"version"`
	ThreadID             string     `json:"thread_id"`
	Goal                 *codexGoal `json:"goal"`
	ContinuationDeferred bool       `json:"continuation_deferred"`
}

// UnmarshalJSON refuses unknown fields and requires every non-Option field,
// as serde's deny_unknown_fields derive does.
func (s *codexGoalSnapshot) UnmarshalJSON(data []byte) error {
	type plain codexGoalSnapshot
	return codexGoalDecodeStrict(data, (*plain)(s), "version", "thread_id", "continuation_deferred")
}

// codexGoalDecodeStrict decodes a JSON object into v, refusing unknown
// fields and fields missing from required. A null required field is refused
// too, as serde refuses null for a non-Option field.
func codexGoalDecodeStrict(data []byte, v any, required ...string) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range required {
		value, ok := fields[name]
		if !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
		if string(bytes.TrimSpace(value)) == "null" {
			return fmt.Errorf("invalid type: null for field `%s`", name)
		}
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode(v)
}

func codexGoalArtifactPath(threadID string) string {
	return filepath.Join(codexGoalArtifactRoot, threadID+".json")
}

func codexGoalDatabasePath(home string) (string, error) {
	resolved, err := filepath.Abs(home)
	if err == nil {
		resolved, err = filepath.EvalSymlinks(resolved)
	}
	if err != nil {
		return "", fmt.Errorf("resolve native Codex home: %w", err)
	}
	return filepath.Join(resolved, codexGoalDatabase), nil
}

// codexGoalQueryer is what the schema and goal readers need from a
// connection: *sql.DB and *sql.Tx both provide it (Rust's Transaction
// derefs to Connection).
type codexGoalQueryer interface {
	Exec(query string, args ...any) (sql.Result, error)
	Query(query string, args ...any) (*sql.Rows, error)
	QueryRow(query string, args ...any) *sql.Row
}

// codexGoalSchemaVersion returns the last supported migration. Unknown
// schemas fail before mutation.
func codexGoalSchemaVersion(connection codexGoalQueryer) (int, error) {
	panic("unported: mj-worker/src/checkpoint/codex_goal.rs schema_version")
}

func codexGoalInitializeSchema(connection codexGoalQueryer) error {
	panic("unported: mj-worker/src/checkpoint/codex_goal.rs initialize_schema")
}

// codexGoalRead is the goal of threadID; nil when it has none.
func codexGoalRead(connection codexGoalQueryer, threadID string) (*codexGoal, error) {
	panic("unported: mj-worker/src/checkpoint/codex_goal.rs read_goal")
}

// codexGoalCollect is the goal artifact for threadID; false when the home
// has no goal database.
func codexGoalCollect(home string, threadID string) (core.NativeArtifact, bool, error) {
	panic("unported: mj-worker/src/checkpoint/codex_goal.rs collect")
}

// codexGoalIsArtifact is Path::starts_with(ARTIFACT_ROOT): whether path's
// first component is the goal artifact root.
func codexGoalIsArtifact(path string) bool {
	first, _, _ := strings.Cut(filepath.ToSlash(path), "/")
	return first == codexGoalArtifactRoot
}

// codexGoalRestoreMissing restores the artifact only into a home without a
// goal database; true when it did.
func codexGoalRestoreMissing(home string, threadID string, path string, data []byte) (bool, error) {
	panic("unported: mj-worker/src/checkpoint/codex_goal.rs restore_missing")
}

// codexGoalRestore replaces threadID's goal and continuation deferral with
// the artifact's, leaving other threads' goals alone.
func codexGoalRestore(home string, threadID string, path string, data []byte) error {
	panic("unported: mj-worker/src/checkpoint/codex_goal.rs restore")
}
