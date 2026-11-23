// Port of the tests in mj-store/src/database/state_io.rs.

package store

import (
	"path/filepath"
	"reflect"
	"testing"
)

func TestSingleSessionReadIncludesRelatedRowsAndIgnoresUnrelatedInvalidRecords(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	expected := session("selected", "project")
	if err := SaveSessionTo(path, &expected); err != nil {
		t.Fatal(err)
	}
	unrelated := session("unrelated", "project")
	if err := SaveSessionTo(path, &unrelated); err != nil {
		t.Fatal(err)
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	// A corrupt unrelated value makes a full-state read fail. A point read
	// must not decode it or any of its related rows.
	if _, err := connection.Exec("UPDATE sessions SET resource_allocation = '[]' WHERE session_id = 'unrelated'"); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadStateFrom(path); err == nil {
		t.Fatal("a full-state read decoded a corrupt record")
	}
	reader, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	snapshot, err := reader.Begin()
	if err != nil {
		t.Fatal(err)
	}
	defer snapshot.Rollback()
	selected, err := loadSessionWith(snapshot, "selected")
	if err != nil {
		t.Fatal(err)
	}
	if selected == nil || !reflect.DeepEqual(*selected, expected) {
		t.Fatalf("selected = %#v, want %#v", selected, expected)
	}
	missing, err := loadSessionWith(snapshot, "missing")
	if err != nil {
		t.Fatal(err)
	}
	if missing != nil {
		t.Fatalf("missing = %#v, want none", missing)
	}
}
