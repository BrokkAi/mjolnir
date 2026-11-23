// Port of mj-worker/src/acp/antigravity_tasks.rs: its tests module.

package worker

import (
	"database/sql"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func antigravityVarint(n uint64, out *[]byte) {
	for n >= 128 {
		*out = append(*out, byte(n)|128)
		n >>= 7
	}
	*out = append(*out, byte(n))
}

func antigravityUint(field, n uint64, out *[]byte) {
	antigravityVarint(field<<3, out)
	antigravityVarint(n, out)
}

func antigravityBytes(field uint64, b []byte, out *[]byte) {
	antigravityVarint((field<<3)|2, out)
	antigravityVarint(uint64(len(b)), out)
	*out = append(*out, b...)
}

func antigravitySummary(status, idle uint64) []byte {
	var b []byte
	antigravityUint(5, status, &b)
	antigravityUint(21, idle, &b)
	antigravityUint(29, 7, &b)
	antigravityUint(30, 1, &b)
	return b
}

func antigravityDirs(t *testing.T, root string) string {
	t.Helper()
	d := filepath.Join(root, "antigravity-cli")
	if err := os.MkdirAll(filepath.Join(d, "conversations"), 0o755); err != nil {
		t.Fatal(err)
	}
	return d
}

// antigravityOpenWriter opens path read-write, creating it, as rusqlite's
// Connection::open does.
func antigravityOpenWriter(t *testing.T, path string) *sql.DB {
	t.Helper()
	db, err := sql.Open("sqlite", codexGoalURI(path, "mode=rwc"))
	if err != nil {
		t.Fatal(err)
	}
	db.SetMaxOpenConns(1)
	t.Cleanup(func() { db.Close() })
	return db
}

func antigravityExec(t *testing.T, db *sql.DB, query string, args ...any) {
	t.Helper()
	if _, err := db.Exec(query, args...); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
}

func antigravityRefresh(t *testing.T, follower *AntigravityStateFollower) AntigravitySnapshot {
	t.Helper()
	snapshot, err := follower.Refresh()
	if err != nil {
		t.Fatal(err)
	}
	return snapshot
}

func TestAntigravityWalDataVersionOnlyAdvancesSummaryForTheParentRow(t *testing.T) {
	tmp := t.TempDir()
	d := antigravityDirs(t, tmp)
	path := filepath.Join(d, "conversation_summaries.db")
	writer := antigravityOpenWriter(t, path)
	antigravityExec(t, writer, "PRAGMA journal_mode = WAL")
	antigravityExec(t, writer, `CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY, status TEXT, not_fully_idle, killed,
                step_count INTEGER, last_modified_time, raw_summary BLOB
            );`)
	follower := NewAntigravityStateFollower(tmp, "c1")
	t.Cleanup(follower.Close)
	antigravityExec(t, writer, "INSERT INTO conversation_summaries VALUES (?1,?2,?3,0,9,1,?4)",
		"c1", "CASCADE_RUN_STATUS_RUNNING", 1, antigravitySummary(2, 1))
	a := antigravityRefresh(t, follower)
	if !a.SummaryCommitChanged {
		t.Fatal("the first row is a commit")
	}
	s := a.Summary
	if s.Status != AntigravityRunStatusRunning || !s.NotFullyIdle || s.BackgroundTaskCount != 7 || !s.HasRunningSubagents {
		t.Fatalf("summary %+v", *s)
	}
	if antigravityRefresh(t, follower).SummaryCommitChanged {
		t.Fatal("an unchanged database is not a commit")
	}
	version := *follower.cursor.dataVersion
	antigravityExec(t, writer, `INSERT INTO conversation_summaries
                 SELECT 'subagent', status, not_fully_idle, killed, step_count, last_modified_time, raw_summary
                 FROM conversation_summaries WHERE conversation_id='c1'`)
	childCommit := antigravityRefresh(t, follower)
	if *follower.cursor.dataVersion == version {
		t.Fatal("the child commit advances data_version")
	}
	if childCommit.SummaryCommitChanged {
		t.Fatal("a child conversation's commit is not the parent's")
	}

	antigravityExec(t, writer, "UPDATE conversation_summaries SET last_modified_time=2 WHERE conversation_id='c1'")
	timestampCommit := antigravityRefresh(t, follower)
	if !timestampCommit.SummaryCommitChanged {
		t.Fatal("a timestamp change is a commit")
	}
	if !reflect.DeepEqual(timestampCommit.Summary, childCommit.Summary) {
		t.Fatalf("summary %+v, want %+v", timestampCommit.Summary, childCommit.Summary)
	}
	antigravityExec(t, writer, "UPDATE conversation_summaries SET status=?2,not_fully_idle=0,raw_summary=?3 WHERE conversation_id=?1",
		"c1", "CASCADE_RUN_STATUS_IDLE", antigravitySummary(1, 0))
	b := antigravityRefresh(t, follower)
	if !b.SummaryCommitChanged {
		t.Fatal("an idle row is a commit")
	}
	if s := b.Summary; s.Status != AntigravityRunStatusIdle || s.NotFullyIdle {
		t.Fatalf("summary %+v", *s)
	}

	antigravityExec(t, writer, "DELETE FROM conversation_summaries WHERE conversation_id='c1'")
	deleted := antigravityRefresh(t, follower)
	if deleted.SummaryCommitChanged || deleted.Summary != nil {
		t.Fatalf("deleted %+v", deleted)
	}
	antigravityExec(t, writer, "INSERT INTO conversation_summaries VALUES ('c1','CASCADE_RUN_STATUS_IDLE',0,0,9,2,?1)",
		antigravitySummary(1, 0))
	if !antigravityRefresh(t, follower).SummaryCommitChanged {
		t.Fatal("a reinserted row is a commit")
	}
}

func TestTrajectoryTasksDecodeFilterAndOrder(t *testing.T) {
	tmp := t.TempDir()
	d := antigravityDirs(t, tmp)
	path := filepath.Join(d, "conversations/c1.db")
	c := antigravityOpenWriter(t, path)
	antigravityExec(t, c, "CREATE TABLE steps (idx INTEGER,status INTEGER,task_details BLOB)")
	var stamp []byte
	antigravityUint(1, 123, &stamp)
	antigravityUint(2, 456_000_000, &stamp)
	var finite []byte
	antigravityBytes(1, []byte("task"), &finite)
	antigravityBytes(4, []byte("description"), &finite)
	antigravityBytes(7, []byte("title"), &finite)
	antigravityBytes(8, stamp, &finite)
	var daemon []byte
	antigravityUint(6, 1, &daemon)
	antigravityBytes(1, []byte("daemon"), &daemon)
	var fallback []byte
	antigravityBytes(1, []byte("fallback-id"), &fallback)
	antigravityExec(t, c, "INSERT INTO steps VALUES (1,2,?1)", finite)
	antigravityExec(t, c, "INSERT INTO steps VALUES (2,2,?1)", daemon)
	antigravityExec(t, c, "INSERT INTO steps VALUES (3,3,?1)", fallback)
	antigravityExec(t, c, "INSERT INTO steps VALUES (4,2,?1)", fallback)
	tasks, err := antigravityReadTasks(path)
	if err != nil {
		t.Fatal(err)
	}
	started := int64(123456)
	want := []AntigravityBackgroundTask{
		{StepIndex: 1, ID: "task", Command: "title", StartedAtMs: &started},
		{StepIndex: 4, ID: "fallback-id", Command: "fallback-id"},
	}
	if !reflect.DeepEqual(tasks, want) {
		t.Fatalf("tasks %+v, want %+v", tasks, want)
	}
}

func TestMissingAndBrokenDatabasesAndInvalidIDs(t *testing.T) {
	tmp := t.TempDir()
	f := NewAntigravityStateFollower(tmp, "c1")
	t.Cleanup(f.Close)
	snap := antigravityRefresh(t, f)
	if snap.Summary != nil || len(snap.RunningTasks) != 0 {
		t.Fatalf("snapshot %+v", snap)
	}
	d := antigravityDirs(t, tmp)
	writer := antigravityOpenWriter(t, filepath.Join(d, "conversation_summaries.db"))
	antigravityExec(t, writer, "CREATE TABLE conversation_summaries (conversation_id TEXT)")
	if _, err := f.Refresh(); err == nil {
		t.Fatal("a summary table without the columns is an error")
	}
	for _, id := range []string{"../x", "", "a/b"} {
		bad := NewAntigravityStateFollower(tmp, id)
		if _, err := bad.Refresh(); err == nil {
			t.Fatalf("conversation ID %q is accepted", id)
		}
		bad.Close()
	}
}
