// Port of mj-checkpoint/src/checkpoint/tests.rs: shared fixtures and the
// native collection and harness path tests. The export, staging and restore
// tests of the same Rust module are in checkpoint_export_test.go and
// checkpoint_restore_test.go.

package checkpoint

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	checkpointTestSession = "018f9dd2-a3b4-7c8d-9000-123456789abc"
	checkpointTestNative  = "0190aabb-ccdd-7eef-9000-abcdef012345"
)

// recordingGit runs real Git, but counts repair fetches and can make them
// fail without reaching a network remote.
type recordingGit struct {
	fetchFailure bool
	mu           sync.Mutex
	fetchCount   int
}

func recordingGitForwarding() *recordingGit { return &recordingGit{} }

func recordingGitWithFetchFailure() *recordingGit { return &recordingGit{fetchFailure: true} }

func (g *recordingGit) fetches() int {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.fetchCount
}

func (g *recordingGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	if len(command.Arguments) > 0 && command.Arguments[0] == "fetch" {
		g.mu.Lock()
		g.fetchCount++
		g.mu.Unlock()
		if g.fetchFailure {
			return GitOutput{
				Status: 128,
				Stdout: []byte{},
				Stderr: []byte("fatal: could not read from remote repository"),
			}, nil
		}
	}
	return SystemGit{}.Run(repository, command)
}

// checkpointMust is Rust's unwrap for a test: checkpointMust(f())(t) fails
// the test on f's error and returns its value. (The test comes last so that a
// two-result call can be passed whole.)
func checkpointMust[T any](value T, err error) func(testing.TB) T {
	return func(t testing.TB) T {
		t.Helper()
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		return value
	}
}

// checkpointNoError fails the test on err.
func checkpointNoError(t testing.TB, err error) {
	t.Helper()
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
}

// checkpointWriteFile writes data at path, creating no directories.
func checkpointWriteFile(t testing.TB, path string, data []byte) {
	t.Helper()
	checkpointNoError(t, os.WriteFile(path, data, 0o644))
}

// checkpointMkdirAll creates path and its parents.
func checkpointMkdirAll(t testing.TB, path string) {
	t.Helper()
	checkpointNoError(t, os.MkdirAll(path, 0o755))
}

// checkpointReadFile reads path.
func checkpointReadFile(t testing.TB, path string) []byte {
	t.Helper()
	return checkpointMust(os.ReadFile(path))(t)
}

// artifactBytes reads a native artifact's whole body.
func artifactBytes(t testing.TB, artifact *core.NativeArtifact) []byte {
	t.Helper()
	reader := checkpointMust(artifact.Reader())(t)
	defer reader.Close()
	return checkpointMust(io.ReadAll(reader))(t)
}

// artifactPaths are the artifacts' relative paths in order.
func artifactPaths(artifacts []core.NativeArtifact) []string {
	return nativeArtifactPaths(artifacts)
}

// checkpointSamePaths compares path lists as Rust compares Vec<PathBuf>.
func checkpointSamePaths(left, right []string) bool {
	return slices.EqualFunc(left, right, checkpointPathEqual)
}

// openTestDatabase opens a SQLite database through one connection, as a
// rusqlite Connection is one connection, so per-connection pragmas and an
// open write-ahead log hold for its whole life.
func openTestDatabase(t testing.TB, path string) *sql.DB {
	t.Helper()
	db := checkpointMust(sql.Open("sqlite", sqliteURI(path, "")))(t)
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	db.SetConnMaxLifetime(0)
	checkpointNoError(t, db.PingContext(context.Background()))
	return db
}

// execTestDatabase runs statements on db, failing the test on an error.
func execTestDatabase(t testing.TB, db *sql.DB, query string, arguments ...any) {
	t.Helper()
	if _, err := db.Exec(query, arguments...); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
}

// queryStrings reads the one text column of every row query returns.
func queryStrings(t testing.TB, db *sql.DB, query string) []string {
	t.Helper()
	rows := checkpointMust(db.Query(query))(t)
	defer rows.Close()
	var values []string
	for rows.Next() {
		var value string
		checkpointNoError(t, rows.Scan(&value))
		values = append(values, value)
	}
	checkpointNoError(t, rows.Err())
	return values
}

// Hard-won: f7bf63fc: a new session with no accepted prompt could not be checkpointed because export required native history.
func TestEmptyNativeArtifactsAllowedOnlyForUnpromptedSessions(t *testing.T) {
	temp := t.TempDir()
	_, err := CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, false)
	if err == nil || err.Error() != "no session artifacts found" {
		t.Fatalf("error = %v", err)
	}
	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, true))(t)
	if len(artifacts) != 0 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

// dsh-acp writes `sessions/<id>.json` atomically through a `.<id>.*.tmp`
// file and refuses `session/load` from any directory but the one the record
// names. A checkpoint keeps the record alone, and a restore at another
// workspace path names that path while keeping the rest of the record as it
// was.
func TestDshCheckpointKeepsTheSessionRecordAndRestoresItAtTheNewWorkspace(t *testing.T) {
	const dshNative = "dsh-a17182fbf3f34a218842f9d5abb0702f"
	// A record as dsh-acp 0.2.0 wrote it, with its system prompt shortened.
	record := `{
		"id": "` + dshNative + `",
		"cwd": "/old/workspace/project",
		"additional_directories": ["/old/workspace/library"],
		"title": "hi",
		"created_at": "2026-10-22T15:28:27.177270667Z",
		"updated_at": "2026-10-22T15:28:27.177860889Z",
		"config": {"model": "deepseek-flash", "mode": "agent", "permission_mode": "ask", "network": true},
		"system_prompt": null,
		"messages": [
			{"role": "system", "content": "You are DeepSeek Harness"},
			{"role": "user", "content": "hi"}
		],
		"prompt_blocks": {"1": [{"type": "text", "text": "hi"}]},
		"message_ids": {"1": "history-message-1"},
		"pending_steering": [],
		"tool_outcomes": {},
		"compactions": [],
		"usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
		          "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 0},
		"context_used": 0,
		"parent_session_id": null
	}`
	var pretty bytes.Buffer
	checkpointNoError(t, json.Indent(&pretty, []byte(record), "", "  "))
	temp := t.TempDir()
	sessions := filepath.Join(temp, "sessions")
	checkpointMkdirAll(t, sessions)
	checkpointWriteFile(t, filepath.Join(sessions, dshNative+".json"), pretty.Bytes())
	checkpointWriteFile(t, filepath.Join(sessions, "."+dshNative+".0f3c.tmp"), []byte(`{"id"`))
	checkpointWriteFile(t, filepath.Join(sessions, "dsh-another.json"), []byte("{}"))
	checkpointWriteFile(t, filepath.Join(temp, "AGENTS.md"), []byte("instructions"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindDsh, temp, dshNative, false))(t)
	relative := core.Join("sessions", dshNative+".json")
	if !checkpointSamePaths(artifactPaths(artifacts), []string{relative}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}

	moved := filepath.Join(temp, "moved")
	if path := checkpointMust(restoredNativeRelativePath(core.HarnessKindDsh, relative, &moved))(t); !checkpointPathEqual(path, relative) {
		t.Fatalf("relative path = %s", path)
	}
	restoredBytes := checkpointMust(restoredNativeArtifactBytes(core.HarnessKindDsh, relative, artifactBytes(t, &artifacts[0]), &moved, temp))(t)
	restored := checkpointMust(jsonvalue.Parse(restoredBytes))(t)
	expectedValue := checkpointMust(jsonvalue.Parse([]byte(record)))(t)
	expected := expectedValue.(jsonvalue.Object)
	expected.Set("cwd", moved)
	if !jsonvalue.Equal(restored, expected) {
		t.Fatalf("restored = %s", restoredBytes)
	}
}

// OpenCode 2 keeps its logins in the same database as its sessions. A
// checkpoint taken while OpenCode runs carries the sessions, including the
// pages still only in the write-ahead log, and no byte of any login.
// Resuming it into a home that kept its files, log included, restores those
// sessions without replaying the old log over them.
func TestAnOpencodeCheckpointKeepsLiveSessionsAndNoLoginSecret(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Session.HarnessKind = core.HarnessKindOpenCode
	spec.HarnessHome = filepath.Join(temp, "opencode")
	data := filepath.Join(spec.HarnessHome, ".data/opencode")
	checkpointMkdirAll(t, data)
	secrets := []string{
		"sk-checkpoint-api-key",
		"access-checkpoint-token",
		"refresh-checkpoint-token",
		"sk-checkpoint-newer-key",
	}
	// OpenCode's open connection keeps the newest pages in its log.
	live := openTestDatabase(t, filepath.Join(data, "opencode.db"))
	execTestDatabase(t, live, "PRAGMA journal_mode = WAL")
	execTestDatabase(t, live, "PRAGMA wal_autocheckpoint = 0")
	execTestDatabase(t, live, `CREATE TABLE credential (id text PRIMARY KEY, integration_id text, label text NOT NULL,
             value text NOT NULL, connector_id text, method_id text, active integer,
             time_created integer NOT NULL, time_updated integer NOT NULL);
         CREATE TABLE account (id text PRIMARY KEY, email text, url text, access_token text,
             refresh_token text, token_expiry integer, time_created integer, time_updated integer);
         CREATE TABLE control_account (email text, url text, access_token text, refresh_token text,
             token_expiry integer, active integer, time_created integer, time_updated integer);
         CREATE TABLE session_v2 (id text PRIMARY KEY, title text);`)
	credential := func(id, key string) {
		execTestDatabase(t, live,
			"INSERT INTO credential VALUES (?1, 'deepseek', 'DeepSeek', ?2, NULL, NULL, 1, 1, 1)",
			id, fmt.Sprintf(`{"type":"key","key":"%s"}`, key))
	}
	credential("cred", secrets[0])
	execTestDatabase(t, live,
		"INSERT INTO account VALUES ('acct', 'me@example.com', 'https://opencode.ai', ?1, ?2, 0, 1, 1)",
		secrets[1], secrets[2])
	execTestDatabase(t, live,
		"INSERT INTO control_account VALUES ('me@example.com', 'https://opencode.ai', ?1, ?2, 0, 1, 1, 1)",
		secrets[1], secrets[2])
	execTestDatabase(t, live, "INSERT INTO session_v2 VALUES ('ses_old', 'in the file')")
	// These logins reach the database file itself; the newest login and
	// session are only in the log.
	execTestDatabase(t, live, "PRAGMA wal_checkpoint(TRUNCATE);")
	credential("cred_newer", secrets[3])
	execTestDatabase(t, live, "INSERT INTO session_v2 VALUES ('ses_live', 'still in the log')")
	if info := checkpointMust(os.Stat(filepath.Join(data, "opencode.db-wal")))(t); info.Size() == 0 {
		t.Fatal("the newest pages are not in the log")
	}
	// The home a resume finds: the files as they stood while OpenCode ran.
	retained := filepath.Join(temp, "retained")
	checkpointMkdirAll(t, filepath.Join(retained, ".data/opencode"))
	for _, file := range []string{"opencode.db", "opencode.db-wal", "opencode.db-shm"} {
		checkpointWriteFile(t, filepath.Join(retained, ".data/opencode", file), checkpointReadFile(t, filepath.Join(data, file)))
	}

	checkpointMust(ExportCheckpoint(&spec))(t)
	checkpointNoError(t, live.Close())

	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	var native []string
	for _, descriptor := range archive.Manifest.Payloads {
		if descriptor.Role.Kind == core.PayloadRoleNativeArtifact {
			native = append(native, descriptor.Role.RelativePath)
		}
	}
	if !checkpointSamePaths(native, []string{".data/opencode/opencode.db"}) {
		t.Fatalf("native artifacts = %v", native)
	}
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		payload := checkpointMust(archive.Payload(descriptor))(t)
		for _, secret := range secrets {
			if bytes.Contains(payload, []byte(secret)) {
				t.Fatalf("the checkpoint carries %s in %+v", secret, descriptor.Role)
			}
		}
	}

	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           filepath.Join(temp, "restored-relay"),
		HarnessHome:         retained,
		RestoreRepositories: false,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{}))

	restored := openTestDatabase(t, filepath.Join(retained, ".data/opencode/opencode.db"))
	defer restored.Close()
	if sessions := queryStrings(t, restored, "SELECT id FROM session_v2 ORDER BY id"); !slices.Equal(sessions, []string{"ses_live", "ses_old"}) {
		t.Fatalf("sessions = %v", sessions)
	}
	for _, table := range []string{"credential", "account", "control_account"} {
		var rows int64
		checkpointNoError(t, restored.QueryRow(fmt.Sprintf("SELECT count(*) FROM %s", table)).Scan(&rows))
		if rows != 0 {
			t.Fatalf("%s holds %d rows", table, rows)
		}
	}
}

// antigravityWorkspaceRecord is the workspace record agy writes at
// `trajectory_metadata_blob.main`, as a protobuf: every workspace root
// nested in a repeated field 1 and again in a repeated field 7, around
// fields that relocation must leave as they are. Encoded here independently
// of the editor that rewrites it.
func antigravityWorkspaceRecord(session string, roots []string) []byte {
	return antigravityWorkspaceRecordNaming(session, roots, roots)
}

func antigravityWorkspaceRecordNaming(session string, nestedRoots []string, roots []string) []byte {
	varint := func(value uint64, out *[]byte) {
		for value >= 0x80 {
			*out = append(*out, byte(value&0x7f)|0x80)
			value >>= 7
		}
		*out = append(*out, byte(value))
	}
	str := func(number uint64, payload []byte, out *[]byte) {
		varint(number<<3|2, out)
		varint(uint64(len(payload)), out)
		*out = append(*out, payload...)
	}
	var stamp []byte
	varint(1<<3, &stamp)
	varint(1_794_582_301, &stamp)
	varint(2<<3, &stamp)
	varint(592_695_273, &stamp)
	var record []byte
	for _, uri := range nestedRoots {
		var root []byte
		str(1, []byte(uri), &root)
		str(3, []byte{}, &root)
		str(1, root, &record)
	}
	str(2, stamp, &record)
	str(3, []byte("05300845-02a9-4765-ab5f-3b8ce6691497"), &record)
	str(6, []byte(session), &record)
	for _, uri := range roots {
		str(7, []byte(uri), &record)
	}
	// Fields this checkpoint does not understand are copied as they were.
	str(15, []byte{1, 2, 3, 4, 5, 0xff, 0xfe, 0xfd}, &record)
	str(18, []byte("default-cli-project"), &record)
	return record
}

// agy keeps each conversation in a SQLite database it holds open in WAL
// mode, so the newest turn is only in the log while agy runs, and records
// every workspace it was given inside the database (the launch directory and
// any additional ones, with nothing to tell which is which). It runs
// commands in the first root it knows, so a restore at another path records
// the new launch directory as the only root, even when a recorded root still
// exists where the session is restored (an old copy of the tree would run
// its commands); the request that opens the conversation names the
// additional directories again. A checkpoint keeps the conversation's
// database and its `brain` files and nothing else in the home, a restore
// removes the log of the database it replaces, and a workspace record it
// cannot read refuses the restore.
func TestAnAntigravityCheckpointKeepsTheLiveConversationAndFollowsTheWorkspace(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Session.HarnessKind = core.HarnessKindAntigravity
	spec.HarnessHome = filepath.Join(temp, "antigravity")
	agy := filepath.Join(spec.HarnessHome, "antigravity-cli")
	conversations := filepath.Join(agy, "conversations")
	checkpointMkdirAll(t, conversations)
	oldURI := "file:///old/workspace/app"
	// Roots that still exist where the session is restored: an additional
	// directory, and the tree the session was launched in, now an old copy.
	extra := filepath.Join(temp, "extra library")
	checkpointMkdirAll(t, extra)
	extraURI := fmt.Sprintf("file://%s/extra%%20library", temp)
	oldCopyURI := fmt.Sprintf("file://%s/app", spec.WorkspaceRoot)
	database := filepath.Join(conversations, checkpointTestNative+".db")
	// The tables agy 1.5.8 creates that hold what the checkpoint relocates.
	live := openTestDatabase(t, database)
	execTestDatabase(t, live, "PRAGMA journal_mode = WAL")
	execTestDatabase(t, live, "PRAGMA wal_autocheckpoint = 0")
	execTestDatabase(t, live, "CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,`trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));"+
		"CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,`step_payload` blob,PRIMARY KEY (`idx`));"+
		"CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT \"main\",`data` blob,PRIMARY KEY (`id`));")
	execTestDatabase(t, live, "INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)",
		antigravityWorkspaceRecord(checkpointTestNative, []string{extraURI, oldCopyURI, oldURI}))
	execTestDatabase(t, live, "INSERT INTO steps VALUES (0, 14, 3, 'first turn')")
	execTestDatabase(t, live, "PRAGMA wal_checkpoint(TRUNCATE);")
	execTestDatabase(t, live, "INSERT INTO steps VALUES (1, 15, 3, 'turn only in the log')")
	if info := checkpointMust(os.Stat(filepath.Join(conversations, checkpointTestNative+".db-wal")))(t); info.Size() == 0 {
		t.Fatal("the newest turn is not in the log")
	}

	// Everything else agy keeps in its home, none of which a checkpoint takes.
	secrets := []string{"agy-access-token", "agy-refresh-token", "me@example.test"}
	write := func(relative, body string) {
		path := filepath.Join(agy, relative)
		checkpointMkdirAll(t, filepath.Dir(path))
		checkpointWriteFile(t, path, []byte(body))
	}
	write("antigravity-oauth-token", fmt.Sprintf(`{"token":{"access_token":"%s","refresh_token":"%s"}}`, secrets[0], secrets[1]))
	write("log/acp-server-1.log", "account "+secrets[2])
	write("conversation_summaries.db", "index of every conversation")
	write("annotations/other.pbtxt", `title:"other"`)
	write("conversations/other.db", "another conversation")
	write("conversations/other.db-wal", "another conversation's log")
	write("brain/other/.system_generated/steps/2/output.txt", "another conversation's output")
	// The conversation's own files: the full output its database points at.
	write("brain/"+checkpointTestNative+"/.system_generated/steps/2/output.txt", "the full output")
	write("brain/"+checkpointTestNative+"/notes/plan.md", "an artifact")

	checkpointMust(ExportCheckpoint(&spec))(t)
	checkpointNoError(t, live.Close())

	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	var native []string
	for _, descriptor := range archive.Manifest.Payloads {
		if descriptor.Role.Kind == core.PayloadRoleNativeArtifact {
			native = append(native, descriptor.Role.RelativePath)
		}
	}
	expectedNative := []string{
		"antigravity-cli/brain/" + checkpointTestNative + "/.system_generated/steps/2/output.txt",
		"antigravity-cli/brain/" + checkpointTestNative + "/notes/plan.md",
		"antigravity-cli/conversations/" + checkpointTestNative + ".db",
	}
	if !checkpointSamePaths(native, expectedNative) {
		t.Fatalf("native artifacts = %v", native)
	}
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		payload := checkpointMust(archive.Payload(descriptor))(t)
		for _, secret := range secrets {
			if bytes.Contains(payload, []byte(secret)) {
				t.Fatalf("the checkpoint carries %s in %+v", secret, descriptor.Role)
			}
		}
	}

	// The home a resume finds still holds the log of the conversation's
	// previous run, which SQLite would replay over the restored pages.
	retained := filepath.Join(temp, "retained")
	retainedConversations := filepath.Join(retained, "antigravity-cli/conversations")
	checkpointMkdirAll(t, retainedConversations)
	for _, sidecar := range []string{"db-wal", "db-shm"} {
		checkpointWriteFile(t, filepath.Join(retainedConversations, checkpointTestNative+"."+sidecar), []byte("stale pages"))
	}
	checkpointWriteFile(t, filepath.Join(retainedConversations, "other.db-wal"), []byte("not ours"))
	// A path of another length, with a space, a non-ASCII letter and
	// characters agy escapes and the `url` crate does not.
	moved := filepath.Join(temp, "new wörk space (copy)/app")
	restoreSpec := func(archivePath, home string) *CheckpointRestoreSpec {
		return &CheckpointRestoreSpec{
			ArchivePath:           archivePath,
			WorkspaceRoot:         spec.WorkspaceRoot,
			RelayRoot:             filepath.Join(temp, "relay-"+filepath.Base(home)),
			HarnessHome:           home,
			RestoreRepositories:   false,
			RestoreNative:         true,
			QueuePolicy:           QueueRestorePolicyRestore,
			PrimaryRepositoryRoot: &moved,
		}
	}
	checkpointNoError(t, RestoreCheckpoint(restoreSpec(spec.OutputPath, retained), SystemGit{}))

	restoredPath := filepath.Join(retainedConversations, checkpointTestNative+".db")
	for _, sidecar := range []string{".db-wal", ".db-shm"} {
		if checkpointExists(filepath.Join(retainedConversations, checkpointTestNative+sidecar)) {
			t.Fatalf("the stale %s was left beside the restored database", sidecar)
		}
	}
	if !checkpointExists(filepath.Join(retainedConversations, "other.db-wal")) {
		t.Fatal("another conversation's log was removed")
	}
	restored := openTestDatabase(t, restoredPath)
	if turns := queryStrings(t, restored, "SELECT step_payload FROM steps ORDER BY idx"); !slices.Equal(turns, []string{"first turn", "turn only in the log"}) {
		t.Fatalf("turns = %v", turns)
	}
	var record []byte
	checkpointNoError(t, restored.QueryRow("SELECT data FROM trajectory_metadata_blob WHERE id = 'main'").Scan(&record))
	newURI := fmt.Sprintf("file://%s/new%%20w%%C3%%B6rk%%20space%%20%%28copy%%29/app", temp)
	if !bytes.Equal(record, antigravityWorkspaceRecord(checkpointTestNative, []string{newURI})) {
		t.Fatalf("record = %q", record)
	}
	if output := checkpointReadFile(t, filepath.Join(retained, "antigravity-cli/brain/"+checkpointTestNative+"/.system_generated/steps/2/output.txt")); string(output) != "the full output" {
		t.Fatalf("brain output = %q", output)
	}
	checkpointNoError(t, restored.Close())

	// A record whose two lists of roots name different workspaces is not one
	// to guess at.
	overwrite := func(record []byte) {
		connection := openTestDatabase(t, database)
		defer connection.Close()
		execTestDatabase(t, connection, "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = 'main'", record)
	}
	exportAgain := func(name string) string {
		again := spec
		again.OutputPath = filepath.Join(temp, "worker/"+name+".hel.zip")
		checkpointMust(ExportCheckpoint(&again))(t)
		return again.OutputPath
	}
	overwrite(antigravityWorkspaceRecordNaming(checkpointTestNative, []string{oldURI}, []string{"file:///elsewhere/app"}))
	err := RestoreCheckpoint(restoreSpec(exportAgain("mismatch"), filepath.Join(temp, "refused")), SystemGit{})
	if err == nil || !strings.Contains(err.Error(), "two different sets of workspaces") {
		t.Fatalf("error = %v", err)
	}

	// Without its database, the conversation's `brain` files are no history.
	checkpointNoError(t, os.Remove(database))
	withoutDatabase := spec
	withoutDatabase.OutputPath = filepath.Join(temp, "worker/no-database.hel.zip")
	if _, err := ExportCheckpoint(&withoutDatabase); err == nil || !strings.Contains(err.Error(), NoSessionArtifacts) {
		t.Fatalf("error = %v", err)
	}
}

// R4-5: a Codex session whose only turn failed at the usage limit, followed
// by `/clear`, could not be suspended ("no session artifacts found"). The
// clear had replaced the native session with one that never received a
// prompt, so it had nothing to archive, but the prompt from before the clear
// made the export require native artifacts.
// Hard-won: adfee2e4: a pre-clear failed prompt made the new empty native thread incorrectly require artifacts.
func TestASessionClearedAfterItsOnlyPromptCheckpointsWithoutNativeArtifacts(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	// The thread `/clear` opened; Codex has written nothing for it.
	spec.Session.NativeSessionID = "0190aabb-ccdd-7eef-9000-abcdef0123ff"
	spec.CanonicalSession.EventFrontier = 2
	spec.CanonicalSession.Transcript = append(spec.CanonicalSession.Transcript, core.CanonicalTranscriptItem{
		StableID:        core.ContextBoundaryPrefix + "clear-1",
		Position:        2,
		CreatedAtMs:     2,
		LastChangedAtMs: 2,
		Body:            core.CanonicalTranscriptBodySystem{Text: "Context cleared — a new conversation starts here."},
	})
	if _, err := ExportCheckpoint(&spec); err != nil {
		t.Fatalf("a cleared, unprompted native session checkpoints empty: %v", err)
	}

	// A prompt sent to the current native session still requires its history.
	spec.CanonicalSession.Transcript = spec.CanonicalSession.Transcript[:len(spec.CanonicalSession.Transcript)-1]
	spec.OutputPath = filepath.Join(spec.RelayRoot, "prompted.hel.zip")
	if _, err := ExportCheckpoint(&spec); err == nil || !strings.Contains(err.Error(), NoSessionArtifacts) {
		t.Fatalf("error = %v", err)
	}
}

func TestCodexCollectionIgnoresMalformedUnrelatedRollouts(t *testing.T) {
	temp := t.TempDir()
	sessions := filepath.Join(temp, "sessions/2026/08/10")
	checkpointMkdirAll(t, sessions)
	checkpointWriteFile(t, filepath.Join(sessions, "rollout-unrelated.jsonl"), []byte("{malformed\n"))
	selected := filepath.Join(sessions, "rollout-renamed.jsonl")
	checkpointWriteFile(t, selected, []byte(`{"type":"session_meta","payload":{"session_id":"`+checkpointTestNative+`"}}`+"\n"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, false))(t)
	if !checkpointSamePaths(artifactPaths(artifacts), []string{"sessions/2026/08/10/rollout-renamed.jsonl"}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

// checkpointTestNativeCreatedMs is the Unix milliseconds encoded in
// checkpointTestNative, a UUIDv7.
const (
	checkpointTestNativeCreatedMs int64 = 0x0190_aabb_ccdd
	checkpointTestOtherNative           = "0190aabb-ccdd-7eef-9000-ffffffffffff"
	checkpointTestThirdNative           = "0190aabb-ccdd-7eef-9000-eeeeeeeeeeee"
)

func writeLegacyRollout(t testing.TB, path string, sessionID string) {
	t.Helper()
	checkpointMkdirAll(t, filepath.Dir(path))
	checkpointWriteFile(t, path, []byte(`{"type":"session_meta","payload":{"session_id":"`+sessionID+`"}}`+"\n"))
}

func writeModernRollout(t testing.TB, path string, id string, sessionID string) {
	t.Helper()
	checkpointMkdirAll(t, filepath.Dir(path))
	checkpointWriteFile(t, path, []byte(`{"type":"session_meta","payload":{"id":"`+id+`","session_id":"`+sessionID+`"}}`+"\n"))
}

func TestCodexCollectionKeepsOnlyTheRootThreadFromASessionTree(t *testing.T) {
	temp := t.TempDir()
	archivedRoot := filepath.Join(temp, "archived_sessions/rollout-renamed-root.jsonl")
	child := filepath.Join(temp, "sessions/2026/08/10/rollout-child.jsonl")
	descendant := filepath.Join(temp, "sessions/2026/08/10/rollout-descendant.jsonl")
	writeModernRollout(t, archivedRoot, checkpointTestNative, checkpointTestNative)
	writeModernRollout(t, child, checkpointTestOtherNative, checkpointTestNative)
	writeModernRollout(t, descendant, checkpointTestThirdNative, checkpointTestNative)

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, false))(t)

	if !checkpointSamePaths(artifactPaths(artifacts), []string{"archived_sessions/rollout-renamed-root.jsonl"}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestCodexModernRolloutNeverFallsBackToTheSessionTreeID(t *testing.T) {
	temp := t.TempDir()
	child := filepath.Join(temp, "sessions/2026/08/10/rollout-child.jsonl")
	writeModernRollout(t, child, checkpointTestOtherNative, checkpointTestNative)

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, true))(t)

	if len(artifacts) != 0 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func setModifiedMs(t testing.TB, path string, millis int64) {
	t.Helper()
	checkpointNoError(t, os.Chtimes(path, time.Time{}, time.UnixMilli(millis)))
}

func TestUUIDV7TimestampDecodesOnlyVersionSevenUUIDs(t *testing.T) {
	if millis, ok := uuidV7TimestampMs(checkpointTestNative); !ok || millis != checkpointTestNativeCreatedMs {
		t.Fatalf("native = %d, %v", millis, ok)
	}
	if millis, ok := uuidV7TimestampMs(checkpointTestSession); !ok || millis != 0x018f_9dd2_a3b4 {
		t.Fatalf("session = %d, %v", millis, ok)
	}
	if _, ok := uuidV7TimestampMs("0190aabb-ccdd-4eef-9000-abcdef012345"); ok {
		t.Fatal("a version 4 UUID decoded")
	}
	for _, malformed := range []string{
		"",
		"not-a-uuid",
		"0190aabb-ccdd-7eef-9000-abcdef01234",
		"0190aabb-ccdd-7eef-9000-abcdef012345-extra",
		"0190AABB-CCDD-7EEF-9000-ABCDEF012345",
		"0190aabbccdd7eef9000abcdef012345",
		"0190aabg-ccdd-7eef-9000-abcdef012345",
	} {
		if _, ok := uuidV7TimestampMs(malformed); ok {
			t.Fatalf("%s decoded", malformed)
		}
	}
}

// Hard-won: 28d0ebfc: a full Codex-home scan parsed rollouts that predated the session and cost about 15 GB per checkpoint.
func TestCodexContentProbeSkipsRolloutsOlderThanTheSession(t *testing.T) {
	temp := t.TempDir()
	rollout := filepath.Join(temp, "sessions/2026/08/10/rollout-fork.jsonl")
	writeLegacyRollout(t, rollout, checkpointTestNative)

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, false))(t)
	if len(artifacts) != 1 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}

	// Three days before the session's own UUIDv7 creation time, so past the
	// 48 hour skew slack the floor allows.
	setModifiedMs(t, rollout, checkpointTestNativeCreatedMs-72*3600*1000)
	artifacts = checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, true))(t)
	if len(artifacts) != 0 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestCodexNameMatchedRolloutIsCollectedWhateverItsMtime(t *testing.T) {
	temp := t.TempDir()
	rollout := filepath.Join(temp, "sessions/2026/08/10", "rollout-"+checkpointTestNative+".jsonl")
	writeLegacyRollout(t, rollout, checkpointTestOtherNative)
	setModifiedMs(t, rollout, 1_000_000_000_000)

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, false))(t)
	if len(artifacts) != 1 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

// Hard-won: 28d0ebfc: unrelated rollouts were parsed again on every checkpoint despite their permanent negative header verdict.
func TestCodexScanCacheMakesANegativeProbeVerdictPermanent(t *testing.T) {
	temp := t.TempDir()
	foreign := filepath.Join(temp, "sessions/2026/08/10/rollout-foreign.jsonl")
	writeLegacyRollout(t, foreign, checkpointTestOtherNative)
	cache := codexScanCacheEmpty(checkpointTestNative)

	artifacts := checkpointMust(collectNativeArtifactsCached(core.HarnessKindCodex, temp, checkpointTestNative, true, &cache))(t)
	if len(artifacts) != 0 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	if _, ok := cache.NotOurs[checkpointNormalize("sessions/2026/08/10/rollout-foreign.jsonl")]; !ok {
		t.Fatalf("cache = %v", cache.NotOurs)
	}

	writeLegacyRollout(t, foreign, checkpointTestNative)
	later := filepath.Join(temp, "sessions/2026/08/10/rollout-later.jsonl")
	writeLegacyRollout(t, later, checkpointTestNative)
	artifacts = checkpointMust(collectNativeArtifactsCached(core.HarnessKindCodex, temp, checkpointTestNative, true, &cache))(t)
	if !checkpointSamePaths(artifactPaths(artifacts), []string{"sessions/2026/08/10/rollout-later.jsonl"}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

// Hard-won: 28d0ebfc: Codex probe parsed lines past the one session_meta header in each rollout.
func TestCodexProbeStopsAtTheFirstSessionMetaHeader(t *testing.T) {
	temp := t.TempDir()
	rollout := filepath.Join(temp, "sessions/2026/08/10/rollout-fork.jsonl")
	checkpointMkdirAll(t, filepath.Dir(rollout))
	checkpointWriteFile(t, rollout, []byte(
		`{"type":"session_meta","payload":{"session_id":"`+checkpointTestOtherNative+`"}}`+"\n"+
			`{"type":"event_msg"}`+"\n"+
			`{"type":"session_meta","payload":{"session_id":"`+checkpointTestNative+`"}}`+"\n"+
			"{malformed\n"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindCodex, temp, checkpointTestNative, true))(t)
	if len(artifacts) != 0 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestCodexExportRebuildsACorruptScanCacheAndRecordsVerdicts(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	cachePath := filepath.Join(spec.RelayRoot, codexScanCacheFile)
	checkpointWriteFile(t, cachePath, []byte("{not json"))
	writeLegacyRollout(t, filepath.Join(spec.HarnessHome, "sessions/2026/08/09/rollout-x.jsonl"), checkpointTestOtherNative)

	checkpointMust(ExportCheckpoint(&spec))(t)

	var cache codexScanCache
	checkpointNoError(t, json.Unmarshal(checkpointReadFile(t, cachePath), &cache))
	if cache.SessionID != checkpointTestNative {
		t.Fatalf("session = %s", cache.SessionID)
	}
	expected := map[string]struct{}{checkpointNormalize("sessions/2026/08/09/rollout-x.jsonl"): {}}
	if len(cache.NotOurs) != len(expected) {
		t.Fatalf("not ours = %v", cache.NotOurs)
	}
	for path := range expected {
		if _, ok := cache.NotOurs[path]; !ok {
			t.Fatalf("not ours = %v", cache.NotOurs)
		}
	}
	if foreign := loadCodexScanCache(spec.RelayRoot, checkpointTestOtherNative); len(foreign.NotOurs) != 0 {
		t.Fatalf("a foreign session's cache = %v", foreign.NotOurs)
	}
}

func TestNativeAllowlistExcludesCredentialsAndOtherSessions(t *testing.T) {
	temp := t.TempDir()
	session := filepath.Join(temp, "sessions/workspace", "session_"+checkpointTestNative)
	checkpointMkdirAll(t, filepath.Join(session, "agents/main"))
	checkpointWriteFile(t, filepath.Join(session, "state.json"), []byte("state"))
	checkpointWriteFile(t, filepath.Join(session, "agents/main/wire.jsonl"), []byte("events"))
	checkpointMkdirAll(t, filepath.Join(session, "agents/main/tasks/bash-noise"))
	checkpointWriteFile(t, filepath.Join(session, "agents/main/tasks/bash-noise/output.log"), []byte("tool output"))
	checkpointWriteFile(t, filepath.Join(session, "agents/main/wire.jsonl.bak-before-edit"), []byte("backup"))
	checkpointMkdirAll(t, filepath.Join(session, "logs"))
	checkpointWriteFile(t, filepath.Join(session, "logs/kimi-code.log"), []byte("log"))
	checkpointWriteFile(t, filepath.Join(session, "credentials.json"), []byte("secret"))
	other := filepath.Join(temp, "sessions/workspace/other")
	checkpointMkdirAll(t, other)
	checkpointWriteFile(t, filepath.Join(other, "state.json"), []byte("other"))
	checkpointWriteFile(t, filepath.Join(temp, "workspaces.json"),
		[]byte(`{"version":1,"deleted_workspace_ids":[],"workspaces":{"workspace":{"root":"/work/app","name":"app"}}}`))
	checkpointWriteFile(t, filepath.Join(temp, "session_index.jsonl"), []byte(
		`{"sessionId":"`+checkpointTestNative+`","workDir":"/work/app","sessionDir":"/sessions/workspace/session_`+checkpointTestNative+`"}`+"\n"+
			`{"sessionId":"other"}`+"\n"))
	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindKimi, temp, checkpointTestNative, false))(t)
	if len(artifacts) != 4 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointPathEqual(artifact.RelativePath, "workspaces.json")
	}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	index := slices.IndexFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointPathEqual(artifact.RelativePath, "session_index.jsonl")
	})
	if index < 0 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	indexBytes := string(checkpointMust(core.NativeArtifactDataBytesOf(artifacts[index].Data))(t))
	if !strings.Contains(indexBytes, checkpointTestNative) || strings.Contains(indexBytes, "other") {
		t.Fatalf("index = %s", indexBytes)
	}
	for _, artifact := range artifacts {
		path := checkpointLossy([]byte(artifact.RelativePath))
		if strings.Contains(path, "credentials") || strings.Contains(path, "tasks") ||
			strings.Contains(path, ".bak") || strings.Contains(path, "logs") {
			t.Fatalf("collected %s", path)
		}
	}
}

func TestGrokAllowlistCollectsOneSessionDirectoryWithoutRuntimeState(t *testing.T) {
	const native = "01a00c3a-553f-71e0-95ab-aa04396d3ad7"
	temp := t.TempDir()
	session := filepath.Join(temp, "sessions/%2Fhome%2Fme%2Fapp", native)
	checkpointMkdirAll(t, session)
	names := []string{
		"chat_history.jsonl",
		"events.jsonl",
		"prompt_context.json",
		"summary.json",
		"system_prompt.txt",
	}
	for _, name := range names {
		checkpointWriteFile(t, filepath.Join(session, name), []byte("payload"))
	}
	checkpointWriteFile(t, filepath.Join(session, "summary.json.lock"), []byte{})
	other := filepath.Join(temp, "sessions/%2Fhome%2Fme%2Fapp/01a00c40-55c5-78b0-85c8-ac1b99985fd0")
	checkpointMkdirAll(t, other)
	checkpointWriteFile(t, filepath.Join(other, "summary.json"), []byte("other"))
	checkpointWriteFile(t, filepath.Join(temp, "sessions/session_search.sqlite"), []byte("index"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindGrok, temp, native, false))(t)

	paths := make([]string, len(artifacts))
	for i := range artifacts {
		paths[i] = checkpointLossy([]byte(artifacts[i].RelativePath))
	}
	expected := make([]string, len(names))
	for i, name := range names {
		expected[i] = "sessions/%2Fhome%2Fme%2Fapp/" + native + "/" + name
	}
	if !slices.Equal(paths, expected) {
		t.Fatalf("paths = %v", paths)
	}
}

func TestMuseCheckpointKeepsSelectedChildStreamsWithoutCredentialsOrOtherSessions(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, ".data/muse/sessions/2026/09/07")
	selected := filepath.Join(root, checkpointTestNative)
	checkpointMkdirAll(t, filepath.Join(selected, "subagent/child"))
	checkpointMkdirAll(t, filepath.Join(root, "other"))
	checkpointWriteFile(t, filepath.Join(selected, "session.jsonl"), bytes.Repeat([]byte("x"), 128*1024))
	checkpointWriteFile(t, filepath.Join(selected, "session.peer-history.sqlite3"), []byte("derived"))
	checkpointWriteFile(t, filepath.Join(selected, "runtime.lock"), []byte("ephemeral"))
	checkpointWriteFile(t, filepath.Join(selected, "subagent/child/session.jsonl"), []byte("child history"))
	checkpointWriteFile(t, filepath.Join(root, "other/session.jsonl"), []byte("unrelated"))
	checkpointWriteFile(t, filepath.Join(temp, "auth.json"), []byte("secret"))
	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindMuse, temp, checkpointTestNative, false))(t)
	if len(artifacts) != 2 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointMust(artifact.Size())(t) == 128*1024
	}) {
		t.Fatal("the root stream is missing")
	}
	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return string(artifactBytes(t, &artifact)) == "child history"
	}) {
		t.Fatal("the child stream is missing")
	}
	restored := t.TempDir()
	for i := range artifacts {
		path := filepath.Join(restored, artifacts[i].RelativePath)
		checkpointMkdirAll(t, filepath.Dir(path))
		checkpointWriteFile(t, path, artifactBytes(t, &artifacts[i]))
	}
	if again := checkpointMust(CollectNativeArtifacts(core.HarnessKindMuse, restored, checkpointTestNative, false))(t); len(again) != 2 {
		t.Fatalf("restored artifacts = %v", artifactPaths(again))
	}
	if checkpointExists(filepath.Join(restored, "auth.json")) {
		t.Fatal("credentials were restored")
	}
	if _, err := CollectNativeArtifacts(core.HarnessKindMuse, restored, "missing", false); err == nil {
		t.Fatal("a missing session collected")
	}
}

func TestMuseImportNormalizesSelectedTreeToWorkerStorage(t *testing.T) {
	temp := t.TempDir()
	home := filepath.Join(temp, "private/muse")
	root := filepath.Join(home, ".data/muse/sessions/2026/09/08")
	session := filepath.Join(root, checkpointTestNative)
	checkpointMkdirAll(t, filepath.Join(session, "subagent/child"))
	checkpointMkdirAll(t, filepath.Join(root, "other"))
	checkpointWriteFile(t, filepath.Join(session, "session.jsonl"), []byte("root"))
	checkpointWriteFile(t, filepath.Join(session, "subagent/child/session.jsonl"), []byte("child"))
	checkpointWriteFile(t, filepath.Join(session, "runtime.lock"), []byte("lock"))
	checkpointWriteFile(t, filepath.Join(root, "other/session.jsonl"), []byte("other"))
	artifacts := checkpointMust(CollectImportNativeArtifacts(core.HarnessKindMuse, home, checkpointTestNative, filepath.Join(session, "session.jsonl")))(t)
	if len(artifacts) != 2 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	if !checkpointPathEqual(artifacts[0].RelativePath, ".data/muse/sessions/2026/09/08/"+checkpointTestNative+"/session.jsonl") {
		t.Fatalf("first artifact = %s", artifacts[0].RelativePath)
	}
	if body := artifactBytes(t, &artifacts[1]); string(body) != "child" {
		t.Fatalf("second artifact = %q", body)
	}
	if _, err := CollectImportNativeArtifacts(core.HarnessKindMuse, home, "wrong", filepath.Join(session, "session.jsonl")); err == nil {
		t.Fatal("a mismatched session ID imported")
	}
	if body := checkpointReadFile(t, filepath.Join(session, "session.jsonl")); string(body) != "root" {
		t.Fatalf("source = %q", body)
	}
}

// testAppRepositories is the one-repository manifest list the restore path
// tests use: repository "app" at destination "app".
func testAppRepositories() []core.RepositoryManifest {
	branch := "main"
	return []core.RepositoryManifest{{
		Metadata: core.RepositoryMetadata{
			ID:                  "app",
			RelativeDestination: "app",
			Origin:              "owner/app",
			PushURLs:            []string{},
			BaseCommit:          strings.Repeat("a", 40),
			HeadCommit:          strings.Repeat("a", 40),
			Branch:              &branch,
			StashStack:          []core.SavedStashEntry{},
		},
		CommittedBundlePath: "repositories/app/committed.bundle",
		StagedPatchPath:     "repositories/app/staged.patch",
		UnstagedPatchPath:   "repositories/app/unstaged.patch",
		UntrackedTarPath:    "repositories/app/untracked.tar",
		LinkedWorktrees:     []core.LinkedWorktreeManifest{},
	}}
}

// checkpointJSONString reads the string at a path of members in a decoded
// JSON value.
func checkpointJSONString(t testing.TB, data []byte, keys ...string) any {
	t.Helper()
	value := checkpointMust(jsonvalue.Parse(data))(t)
	for _, key := range keys {
		object, _ := value.(jsonvalue.Object)
		value, _ = object.Get(key)
	}
	return value
}

func TestRestoreRewritesGrokCwdKeyAndSessionSummaryForTargetWorkspace(t *testing.T) {
	const native = "01a00c3a-553f-71e0-95ab-aa04396d3ad7"
	repositories := testAppRepositories()
	path := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindGrok,
		"sessions/%2Fhome%2Fjonathan%2FProjects%2Fapp/"+native+"/summary.json",
		targetPrimaryCwd("app", repositories, "/workspace"),
	))(t)
	if !checkpointPathEqual(path, "sessions/%2Fworkspace%2Fapp/"+native+"/summary.json") {
		t.Fatalf("path = %s", path)
	}

	summary := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindGrok,
		path,
		[]byte(`{"info":{"id":"01a00c3a","cwd":"/home/jonathan/Projects/app"},"grok_home":"/home/jonathan/.grok","num_messages":3}`),
		targetPrimaryCwd("app", repositories, "/workspace"),
		"/profiles/imported",
	))(t)
	if cwd := checkpointJSONString(t, summary, "info", "cwd"); cwd != "/workspace/app" {
		t.Fatalf("cwd = %v", cwd)
	}
	if home := checkpointJSONString(t, summary, "grok_home"); home != "/profiles/imported" {
		t.Fatalf("grok_home = %v", home)
	}
	if messages := checkpointJSONString(t, summary, "num_messages"); messages != int64(3) {
		t.Fatalf("num_messages = %v", messages)
	}

	// Transcript files travel unchanged.
	history := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindGrok,
		"sessions/%2Fworkspace%2Fapp/"+native+"/chat_history.jsonl",
		[]byte(`{"role":"user"}`+"\n"),
		targetPrimaryCwd("app", repositories, "/workspace"),
		"/profiles/imported",
	))(t)
	if string(history) != `{"role":"user"}`+"\n" {
		t.Fatalf("history = %q", history)
	}
}

func TestGrokCwdKeyURLEncodesShortPathsAndHashesLongOnes(t *testing.T) {
	if key := grokCwdKey("/home/jonathan"); key != "%2Fhome%2Fjonathan" {
		t.Fatalf("key = %s", key)
	}
	if key := grokCwdKey("/workspace/app"); key != "%2Fworkspace%2Fapp" {
		t.Fatalf("key = %s", key)
	}
	// Unreserved characters survive; everything else is percent-encoded.
	if key := grokCwdKey("/a-b_c.d~e/f g"); key != "%2Fa-b_c.d~e%2Ff%20g" {
		t.Fatalf("key = %s", key)
	}
	long := "/Users/test/" + strings.Repeat("中", 30)
	key := grokCwdKey(long)
	if len(key) > 255 {
		t.Fatalf("key is %d bytes", len(key))
	}
	if strings.HasPrefix(key, "%2F") {
		t.Fatalf("long paths use the hash form: %s", key)
	}
	if !strings.HasPrefix(key, "workspace-") {
		t.Fatalf("unslugifiable leaf: %s", key)
	}
}

// A bare session launches its harness in its project directory exactly as
// recorded, and a checkout at the repository root is recorded with a
// trailing separator (`<repo>/.mj/clones/<id>/`). Grok Build keys its
// session storage by the cwd text it receives, so the restored session has
// to land under that text or `session/resume` reports FS_NOT_FOUND. Kimi
// Code and Claude Code key by the directory, so the separator must not
// change theirs.
// Hard-won: ea0c9720: Grok resume used a cwd spelling without the trailing separator and failed to find its live session.
func TestRestoredNativeSessionsFollowEachHarnessReadingOfTheLaunchCwd(t *testing.T) {
	const native = "01a0ee22-d1b8-72a0-9c50-e34eab622597"
	launchCwd := "/srv/project/.mj/clones/s1/"
	home := "/workers/s1/profile"

	// The layout Grok Build 1.0.40 wrote in a live bare session.
	summary := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindGrok,
		"sessions/%2Fold%2Fproject%2F.mj%2Fclones%2Fs1%2F/"+native+"/summary.json",
		&launchCwd,
	))(t)
	if !checkpointPathEqual(summary, "sessions/%2Fsrv%2Fproject%2F.mj%2Fclones%2Fs1%2F/"+native+"/summary.json") {
		t.Fatalf("summary path = %s", summary)
	}
	rewritten := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindGrok,
		summary,
		[]byte(`{"info":{"id":"`+native+`","cwd":"/old/project/.mj/clones/s1/"},"grok_home":"/old/profile"}`),
		&launchCwd,
		home,
	))(t)
	if cwd := checkpointJSONString(t, rewritten, "info", "cwd"); cwd != "/srv/project/.mj/clones/s1/" {
		t.Fatalf("grok cwd = %v", cwd)
	}

	directory := "/srv/project/.mj/clones/s1"
	kimi := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindKimi,
		"sessions/old-key/session_"+native+"/state.json",
		&launchCwd,
	))(t)
	if !checkpointPathEqual(kimi, core.Join(core.Join("sessions", kimiWorkspaceKey(directory)), "session_"+native+"/state.json")) {
		t.Fatalf("kimi path = %s", kimi)
	}
	state := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindKimi,
		"sessions/key/session_"+native+"/state.json",
		[]byte(`{"workDir":"/old"}`),
		&launchCwd,
		home,
	))(t)
	if workDir := checkpointJSONString(t, state, "workDir"); workDir != "/srv/project/.mj/clones/s1" {
		t.Fatalf("kimi workDir = %v", workDir)
	}
	claude := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindClaude,
		"projects/-old/"+native+".jsonl",
		&launchCwd,
	))(t)
	if !checkpointPathEqual(claude, "projects/-srv-project--mj-clones-s1/"+native+".jsonl") {
		t.Fatalf("claude path = %s", claude)
	}
}

func TestClaudeAllowlistCollectsTranscriptAndSessionSubtreeOnly(t *testing.T) {
	temp := t.TempDir()
	project := filepath.Join(temp, "projects/-workspace-app")
	subagents := filepath.Join(project, checkpointTestNative, "subagents")
	checkpointMkdirAll(t, subagents)
	checkpointWriteFile(t, filepath.Join(project, checkpointTestNative+".jsonl"), []byte("transcript"))
	checkpointWriteFile(t, filepath.Join(subagents, "agent-a.jsonl"), []byte("subagent"))
	checkpointWriteFile(t, filepath.Join(project, "other-session.jsonl"), []byte("other"))
	checkpointWriteFile(t, filepath.Join(project, "settings.json"), []byte("secret config"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindClaude, temp, checkpointTestNative, false))(t)
	if len(artifacts) != 2 {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointPathEndsWith(artifact.RelativePath, checkpointTestNative+".jsonl")
	}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointPathEndsWith(artifact.RelativePath, "subagents/agent-a.jsonl")
	}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestClaudeAllowlistCollectsProjectMemoryForTheSessionSlugOnly(t *testing.T) {
	temp := t.TempDir()
	project := filepath.Join(temp, "projects/-workspace-app")
	nested := filepath.Join(project, "memory/notes")
	checkpointMkdirAll(t, nested)
	checkpointWriteFile(t, filepath.Join(project, checkpointTestNative+".jsonl"), []byte("transcript"))
	checkpointWriteFile(t, filepath.Join(project, "memory/root.md"), []byte("root memory"))
	checkpointWriteFile(t, filepath.Join(nested, "deep.md"), []byte("nested memory"))

	// Memory belonging to an unrelated project in the same harness home.
	other := filepath.Join(temp, "projects/-workspace-other/memory")
	checkpointMkdirAll(t, other)
	checkpointWriteFile(t, filepath.Join(other, "leak.md"), []byte("other project memory"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindClaude, temp, checkpointTestNative, false))(t)
	paths := artifactPaths(artifacts)
	for _, expected := range []string{
		"projects/-workspace-app/" + checkpointTestNative + ".jsonl",
		"projects/-workspace-app/memory/root.md",
		"projects/-workspace-app/memory/notes/deep.md",
	} {
		if !slices.ContainsFunc(paths, func(path string) bool { return checkpointPathEqual(path, expected) }) {
			t.Fatalf("%s is missing from %v", expected, paths)
		}
	}
	if slices.ContainsFunc(paths, func(path string) bool { return checkpointPathStartsWith(path, "projects/-workspace-other") }) {
		t.Fatalf("unrelated project memory leaked: %v", paths)
	}
}

// Hard-won: b63fedbf31: filename filtering discarded selected native artifacts and project memory.
func TestClaudeCheckpointPreservesSessionAndProjectMemoryFilenames(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Session.HarnessKind = core.HarnessKindClaude
	spec.HarnessHome = filepath.Join(temp, "claude")
	project := filepath.Join(spec.HarnessHome, "projects/-workspace-app")
	checkpointMkdirAll(t, filepath.Join(project, "memory"))
	checkpointMkdirAll(t, filepath.Join(project, checkpointTestNative, "credentials"))
	checkpointWriteFile(t, filepath.Join(project, checkpointTestNative+".jsonl"), []byte("transcript"))
	files := []string{
		"memory/settings.json",
		"memory/.env",
		filepath.Join(checkpointTestNative, "config.json"),
		filepath.Join(checkpointTestNative, ".credentials.json"),
		filepath.Join(checkpointTestNative, "credentials/notes.txt"),
	}
	for _, relative := range files {
		checkpointWriteFile(t, filepath.Join(project, relative), []byte("preserved contents"))
	}
	checkpointMust(ExportCheckpoint(&spec))(t)

	harnessHome := filepath.Join(temp, "restored-claude")
	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           filepath.Join(temp, "restored-claude-relay"),
		HarnessHome:         harnessHome,
		RestoreRepositories: false,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{}))
	restoredProject := filepath.Join(harnessHome, "projects", ClaudeProjectSlug(filepath.Join(spec.WorkspaceRoot, "app")))
	if body := checkpointReadFile(t, filepath.Join(restoredProject, checkpointTestNative+".jsonl")); string(body) != "transcript" {
		t.Fatalf("transcript = %q", body)
	}
	for _, relative := range files {
		if body := checkpointReadFile(t, filepath.Join(restoredProject, relative)); string(body) != "preserved contents" {
			t.Fatalf("native artifact %s was not preserved", relative)
		}
	}
}

func TestClaudeProjectMemoryIsSkippedWithoutASessionTranscript(t *testing.T) {
	temp := t.TempDir()
	project := filepath.Join(temp, "projects/-workspace-app")
	checkpointMkdirAll(t, filepath.Join(project, "memory"))
	checkpointWriteFile(t, filepath.Join(project, "memory/root.md"), []byte("root memory"))

	artifacts := checkpointMust(CollectNativeArtifacts(core.HarnessKindClaude, temp, checkpointTestNative, true))(t)
	if len(artifacts) != 0 {
		t.Fatalf("collected %v", artifactPaths(artifacts))
	}
}

func TestClaudeProjectSlugMatchesCapturedLocalRolloutFixtures(t *testing.T) {
	// These cwd/directory pairs come from the local Claude home used to
	// establish the import format. Dots are substituted just like slash.
	if slug := ClaudeProjectSlug("/home/jonathan/Projects/mjolnir/.mjolnir/repro"); slug != "-home-jonathan-Projects-mjolnir--mjolnir-repro" {
		t.Fatalf("slug = %s", slug)
	}
	if slug := ClaudeProjectSlug("/tmp/mj-live-transcript.59w2Hg"); slug != "-tmp-mj-live-transcript-59w2Hg" {
		t.Fatalf("slug = %s", slug)
	}
}

func TestRestoreRewritesClaudeProjectArtifactsForTargetWorkspace(t *testing.T) {
	repositories := testAppRepositories()
	path := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindClaude,
		"projects/-home-me-app/session.jsonl",
		targetPrimaryCwd("app", repositories, "/workspace"),
	))(t)
	if !checkpointPathEqual(path, "projects/-workspace-app/session.jsonl") {
		t.Fatalf("path = %s", path)
	}

	// Project memory rides along under the rewritten slug.
	memory := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindClaude,
		"projects/-home-me-app/memory/foo.md",
		targetPrimaryCwd("app", repositories, "/workspace"),
	))(t)
	if !checkpointPathEqual(memory, "projects/-workspace-app/memory/foo.md") {
		t.Fatalf("memory = %s", memory)
	}
}

func TestRestoreRewritesKimiWorkspaceAndStateForTargetWorkspace(t *testing.T) {
	repositories := testAppRepositories()
	path := checkpointMust(restoredNativeRelativePath(
		core.HarnessKindKimi,
		"sessions/wd_kimi-code_78153cfca00c/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2/state.json",
		targetPrimaryCwd("app", repositories, "/workspace"),
	))(t)
	if !checkpointPathEqual(path, "sessions/wd_app_af7e243d70b1/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2/state.json") {
		t.Fatalf("path = %s", path)
	}
	state := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindKimi,
		path,
		[]byte(`{"workDir":"/home/jonathan/Projects/kimi-code","cwd":"/home/jonathan/Projects/kimi-code"}`),
		targetPrimaryCwd("app", repositories, "/workspace"),
		"/profiles/imported",
	))(t)
	if workDir := checkpointJSONString(t, state, "workDir"); workDir != "/workspace/app" {
		t.Fatalf("workDir = %v", workDir)
	}
	if cwd := checkpointJSONString(t, state, "cwd"); cwd != "/workspace/app" {
		t.Fatalf("cwd = %v", cwd)
	}
	registry := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindKimi,
		"workspaces.json",
		[]byte(`{"version":1,"deleted_workspace_ids":[],"workspaces":{"wd_kimi-code_78153cfca00c":{"root":"/home/jonathan/Projects/kimi-code","name":"kimi-code"}}}`),
		targetPrimaryCwd("app", repositories, "/workspace"),
		"/profiles/imported",
	))(t)
	if root := checkpointJSONString(t, registry, "workspaces", "wd_app_af7e243d70b1", "root"); root != "/workspace/app" {
		t.Fatalf("root = %v", root)
	}
	if old := checkpointJSONString(t, registry, "workspaces", "wd_kimi-code_78153cfca00c"); old != nil {
		t.Fatalf("the archived workspace survived: %v", old)
	}

	index := checkpointMust(restoredNativeArtifactBytes(
		core.HarnessKindKimi,
		"session_index.jsonl",
		[]byte(`{"sessionId":"session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2","workDir":"/home/jonathan/Projects/kimi-code","sessionDir":"/home/jonathan/.kimi-code/sessions/wd_kimi-code_78153cfca00c/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2"}`),
		targetPrimaryCwd("app", repositories, "/workspace"),
		"/profiles/imported",
	))(t)
	if workDir := checkpointJSONString(t, index, "workDir"); workDir != "/workspace/app" {
		t.Fatalf("workDir = %v", workDir)
	}
	if dir := checkpointJSONString(t, index, "sessionDir"); dir != "/profiles/imported/sessions/wd_app_af7e243d70b1/session_1b6c3192-2480-48e0-8f49-4b8a1572f5b2" {
		t.Fatalf("sessionDir = %v", dir)
	}
}
