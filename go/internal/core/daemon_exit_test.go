// Port of the tests in mj-core/src/daemon_exit.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"
)

func TestDaemonExitFixtureToleratesFutureAndMissingFields(t *testing.T) {
	path := filepath.Join(t.TempDir(), "42.json")
	if err := os.WriteFile(path, []byte(`{"pid":42,"future":{"version":2}}`), 0o666); err != nil {
		t.Fatal(err)
	}
	record, err := ReadDaemonExitRecord(path)
	if err != nil {
		t.Fatal(err)
	}
	if want := (DaemonExitRecord{Schema: DaemonExitSchema, Pid: 42}); !reflect.DeepEqual(record, want) {
		t.Fatalf("record = %+v", record)
	}
}

func TestDaemonExitRecordReplacesAtomicallyAndFormatsWaitStatus(t *testing.T) {
	exitDir := DaemonExitsDir(t.TempDir())
	waiter := uint32(41)
	signal := int32(6)
	name := "SIGABRT"
	dumped := true
	record := DaemonExitRecord{
		Schema:     DaemonExitSchema,
		Pid:        42,
		WaiterPid:  &waiter,
		ExitedAt:   "2026-11-23T12:00:00Z",
		Signal:     &signal,
		SignalName: &name,
		CoreDumped: &dumped,
	}
	path, err := WriteDaemonExitRecord(exitDir, &record)
	if err != nil {
		t.Fatal(err)
	}
	if path != filepath.Join(exitDir, "42.json") {
		t.Fatalf("path = %s", path)
	}
	if read, err := ReadDaemonExitRecord(path); err != nil || !reflect.DeepEqual(read, record) {
		t.Fatalf("read = %+v, %v", read, err)
	}
	if got := FormatExitReason(&record); got != "terminated by signal SIGABRT (6, core dumped)" {
		t.Fatalf("reason = %q", got)
	}
	signal9 := int32(9)
	kill := "SIGKILL"
	notDumped := false
	record.Signal, record.SignalName, record.CoreDumped = &signal9, &kill, &notDumped
	if got := FormatExitReason(&record); got != "terminated by signal SIGKILL (9)" {
		t.Fatalf("reason = %q", got)
	}
	originalFile, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer originalFile.Close()
	originalRecord, err := ReadDaemonExitRecord(path)
	if err != nil {
		t.Fatal(err)
	}
	code := int32(1)
	record.Signal, record.SignalName, record.CoreDumped = nil, nil, nil
	record.ExitCode = &code
	if replaced, err := WriteDaemonExitRecord(exitDir, &record); err != nil || replaced != path {
		t.Fatalf("replaced = %s, %v", replaced, err)
	}
	if read, err := ReadDaemonExitRecord(path); err != nil || !reflect.DeepEqual(read, record) {
		t.Fatalf("read = %+v, %v", read, err)
	}
	// A reader already holding the old file must still see the old record
	// after publication, even though the path now names the new one.
	var held DaemonExitRecord
	if err := json.NewDecoder(originalFile).Decode(&held); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(held, originalRecord) {
		t.Fatalf("held = %+v", held)
	}
	if got := FormatExitReason(&record); got != "exited with status 1" {
		t.Fatalf("reason = %q", got)
	}
	entries, err := os.ReadDir(exitDir)
	if err != nil || len(entries) != 1 {
		t.Fatalf("entries = %v, %v", entries, err)
	}
	if runtime.GOOS != "windows" {
		directory, err := os.Stat(exitDir)
		if err != nil || directory.Mode().Perm() != 0o700 {
			t.Fatalf("directory mode = %v, %v", directory.Mode(), err)
		}
		file, err := os.Stat(path)
		if err != nil || file.Mode().Perm() != 0o600 {
			t.Fatalf("file mode = %v, %v", file.Mode(), err)
		}
	}
}
