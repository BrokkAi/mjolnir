// Port of the tests of mj-worker/src/worker_stop/process_table.rs.

package worker

import "testing"

func TestACommandNameWithSpacesAndParenthesesDoesNotShiftTheFields(t *testing.T) {
	entry, err := processTableParse(42, "42 (a) b (c) S 7 9 9 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0")
	if err != nil {
		t.Fatal(err)
	}
	if entry.ppid != 7 || entry.pgid != 9 || entry.start != 123456 {
		t.Fatalf("%+v", entry)
	}
	if entry.zombie {
		t.Fatal("not a zombie")
	}
}
