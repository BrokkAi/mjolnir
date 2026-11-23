// Port of mj-worker/src/checkpoint/codex_goal.rs.

// Session-scoped native goal checkpoints. Codex stores accounting outside
// rollouts.

package worker

import (
	"bytes"
	"crypto/sha512"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"

	// The pure-Go SQLite driver, registered as "sqlite".
	_ "modernc.org/sqlite"
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
	var exists bool
	if err := connection.QueryRow(
		"SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations')",
	).Scan(&exists); err != nil {
		return 0, err
	}
	if !exists {
		var tables int64
		if err := connection.QueryRow("SELECT count(*) FROM sqlite_master WHERE type='table'").Scan(&tables); err != nil {
			return 0, err
		}
		if tables != 0 {
			return 0, errors.New("unrecognized Codex goal database schema")
		}
		return 0, nil
	}
	rows, err := connection.Query("SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version")
	if err != nil {
		return 0, err
	}
	defer rows.Close()
	count := 0
	for rows.Next() {
		var version, success int64
		var checksum []byte
		if err := rows.Scan(&version, &success, &checksum); err != nil {
			return 0, err
		}
		if version != int64(count)+1 || count >= len(codexGoalMigrations) || success == 0 {
			return 0, fmt.Errorf("unsupported Codex goal migration %d", version)
		}
		digest := sha512.Sum384([]byte(codexGoalMigrations[count][1]))
		if !bytes.Equal(checksum, digest[:]) {
			return 0, fmt.Errorf("Codex goal migration %d does not match the supported schema", version)
		}
		count++
	}
	if err := rows.Err(); err != nil {
		return 0, err
	}
	return count, nil
}

func codexGoalInitializeSchema(connection codexGoalQueryer) error {
	version, err := codexGoalSchemaVersion(connection)
	if err != nil {
		return err
	}
	if _, err := connection.Exec(`CREATE TABLE IF NOT EXISTS _sqlx_migrations (
        version BIGINT PRIMARY KEY, description TEXT NOT NULL,
        installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        success BOOLEAN NOT NULL, checksum BLOB NOT NULL, execution_time BIGINT NOT NULL
    )`); err != nil {
		return err
	}
	for index := version; index < len(codexGoalMigrations); index++ {
		description, statement := codexGoalMigrations[index][0], codexGoalMigrations[index][1]
		if _, err := connection.Exec(statement); err != nil {
			return err
		}
		digest := sha512.Sum384([]byte(statement))
		if _, err := connection.Exec(
			"INSERT INTO _sqlx_migrations(version, description, success, checksum, execution_time) VALUES (?, ?, 1, ?, 0)",
			int64(index)+1, description, digest[:],
		); err != nil {
			return err
		}
	}
	return nil
}

// codexGoalRead is the goal of threadID; nil when it has none.
func codexGoalRead(connection codexGoalQueryer, threadID string) (*codexGoal, error) {
	var goal codexGoal
	err := connection.QueryRow(
		"SELECT goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms FROM thread_goals WHERE thread_id = ?",
		threadID,
	).Scan(&goal.GoalID, &goal.Objective, &goal.Status, &goal.TokenBudget, &goal.TokensUsed,
		&goal.TimeUsedSeconds, &goal.CreatedAtMs, &goal.UpdatedAtMs)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return &goal, nil
}

// codexGoalOpen opens the goal database at path, which must not be a
// symbolic link (SQLITE_OPEN_NOFOLLOW), read-only or read-write without
// creating it, on one connection with a five-second busy timeout. The
// transactions it begins are immediate when immediate is set.
func codexGoalOpen(path string, readOnly bool, immediate bool) (*sql.DB, error) {
	// database_path resolved every directory, so the database file itself is
	// the only place a link can remain.
	if info, err := os.Lstat(path); err == nil && info.Mode()&fs.ModeSymlink != 0 {
		return nil, errors.New("unable to open database file: symbolic link in path")
	}
	mode := "rw"
	if readOnly {
		mode = "ro"
	}
	query := "mode=" + mode + "&_pragma=busy_timeout(5000)"
	if immediate {
		query += "&_txlock=immediate"
	}
	database, err := sql.Open("sqlite", codexGoalURI(path, query))
	if err != nil {
		return nil, err
	}
	database.SetMaxOpenConns(1)
	if err := database.Ping(); err != nil {
		database.Close()
		return nil, err
	}
	return database, nil
}

// codexGoalURI is a `file:` URI naming path, with query appended, that
// SQLite reads back as path whatever bytes it holds.
func codexGoalURI(path string, query string) string {
	slashed := filepath.ToSlash(path)
	if filepath.VolumeName(path) != "" && !strings.HasPrefix(slashed, "/") {
		slashed = "/" + slashed
	}
	var uri strings.Builder
	uri.WriteString("file:")
	for i := 0; i < len(slashed); i++ {
		b := slashed[i]
		switch {
		case 'A' <= b && b <= 'Z', 'a' <= b && b <= 'z', '0' <= b && b <= '9',
			b == '-', b == '_', b == '.', b == '~', b == '/', b == ':':
			uri.WriteByte(b)
		default:
			fmt.Fprintf(&uri, "%%%02X", b)
		}
	}
	uri.WriteByte('?')
	uri.WriteString(query)
	return uri.String()
}

// codexGoalCollect is the goal artifact for threadID; false when the home
// has no goal database.
func codexGoalCollect(home string, threadID string) (core.NativeArtifact, bool, error) {
	if err := checkpoint.ValidateComponent(threadID, "native goal thread ID"); err != nil {
		return core.NativeArtifact{}, false, err
	}
	exists, err := pathTryExists(filepath.Join(home, codexGoalDatabase))
	if err != nil {
		return core.NativeArtifact{}, false, err
	}
	if !exists {
		return core.NativeArtifact{}, false, nil
	}
	path, err := codexGoalDatabasePath(home)
	if err != nil {
		return core.NativeArtifact{}, false, err
	}
	snapshot := codexGoalSnapshot{Version: 1, ThreadID: threadID}
	database, err := codexGoalOpen(path, true, false)
	if err != nil {
		return core.NativeArtifact{}, false, fmt.Errorf("open native Codex goal state for checkpoint: %w", err)
	}
	defer database.Close()
	transaction, err := database.Begin()
	if err != nil {
		return core.NativeArtifact{}, false, err
	}
	defer transaction.Rollback()
	version, err := codexGoalSchemaVersion(transaction)
	if err != nil {
		return core.NativeArtifact{}, false, err
	}
	if version == 0 {
		return core.NativeArtifact{}, false, errors.New("Codex goal database is not initialized")
	}
	if snapshot.Goal, err = codexGoalRead(transaction, threadID); err != nil {
		return core.NativeArtifact{}, false, err
	}
	if version >= 2 {
		if err := transaction.QueryRow(
			"SELECT EXISTS(SELECT 1 FROM thread_goal_continuation_deferrals WHERE thread_id = ?)", threadID,
		).Scan(&snapshot.ContinuationDeferred); err != nil {
			return core.NativeArtifact{}, false, err
		}
	}
	// serde_json's bytes: no HTML escaping in the objective.
	data, err := jsonvalue.Marshal(snapshot)
	if err != nil {
		return core.NativeArtifact{}, false, err
	}
	return core.NativeArtifact{
		RelativePath: codexGoalArtifactPath(threadID),
		Data:         core.NativeArtifactDataBytes(data),
		Mode:         0o600,
	}, true, nil
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
	exists, err := pathTryExists(filepath.Join(home, codexGoalDatabase))
	if err != nil {
		return false, err
	}
	if exists {
		return false, nil
	}
	if err := codexGoalRestore(home, threadID, path, data); err != nil {
		return false, err
	}
	return true, nil
}

// codexGoalRestore replaces threadID's goal and continuation deferral with
// the artifact's, leaving other threads' goals alone.
func codexGoalRestore(home string, threadID string, path string, data []byte) error {
	if err := checkpoint.ValidateComponent(threadID, "native goal thread ID"); err != nil {
		return err
	}
	if filepath.Clean(path) != codexGoalArtifactPath(threadID) {
		return errors.New("goal artifact does not belong to this native session")
	}
	var snapshot codexGoalSnapshot
	if err := json.Unmarshal(data, &snapshot); err != nil {
		return fmt.Errorf("decode native goal checkpoint: %w", err)
	}
	if snapshot.Version != 1 || snapshot.ThreadID != threadID {
		return errors.New("native goal checkpoint identity/version mismatch")
	}
	if goal := snapshot.Goal; goal != nil {
		valid := goal.GoalID != "" &&
			strings.TrimSpace(goal.Objective) != "" &&
			goal.TokensUsed >= 0 &&
			goal.TimeUsedSeconds >= 0 &&
			goal.CreatedAtMs >= 0 &&
			goal.UpdatedAtMs >= 0 &&
			(goal.TokenBudget == nil || *goal.TokenBudget > 0)
		switch goal.Status {
		case "active", "paused", "blocked", "usage_limited", "budget_limited", "complete":
		default:
			valid = false
		}
		if !valid {
			return errors.New("invalid native goal checkpoint")
		}
	}
	if snapshot.ContinuationDeferred && snapshot.Goal == nil {
		return errors.New("goal continuation deferral has no goal")
	}
	exists, err := pathTryExists(filepath.Join(home, codexGoalDatabase))
	if err != nil {
		return err
	}
	if snapshot.Goal == nil && !exists {
		return nil
	}
	if err := os.MkdirAll(home, 0o777); err != nil {
		return err
	}
	databasePath, err := codexGoalDatabasePath(home)
	if err != nil {
		return err
	}
	created, err := os.OpenFile(databasePath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	switch {
	case err == nil:
		created.Close()
	case errors.Is(err, fs.ErrExist):
	default:
		return fmt.Errorf("create native Codex goal database: %w", err)
	}
	database, err := codexGoalOpen(databasePath, false, true)
	if err != nil {
		return err
	}
	defer database.Close()
	transaction, err := database.Begin()
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	if err := codexGoalInitializeSchema(transaction); err != nil {
		return err
	}
	if _, err := transaction.Exec("DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ?", threadID); err != nil {
		return err
	}
	if _, err := transaction.Exec("DELETE FROM thread_goals WHERE thread_id = ?", threadID); err != nil {
		return err
	}
	if goal := snapshot.Goal; goal != nil {
		if _, err := transaction.Exec(
			"INSERT INTO thread_goals(thread_id, goal_id, objective, status, token_budget, tokens_used, time_used_seconds, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
			threadID, goal.GoalID, goal.Objective, goal.Status, goal.TokenBudget, goal.TokensUsed,
			goal.TimeUsedSeconds, goal.CreatedAtMs, goal.UpdatedAtMs,
		); err != nil {
			return err
		}
		if snapshot.ContinuationDeferred {
			if _, err := transaction.Exec("INSERT INTO thread_goal_continuation_deferrals(thread_id) VALUES (?)", threadID); err != nil {
				return err
			}
		}
	}
	if err := transaction.Commit(); err != nil {
		return fmt.Errorf("restore native Codex goal state: %w", err)
	}
	return nil
}
