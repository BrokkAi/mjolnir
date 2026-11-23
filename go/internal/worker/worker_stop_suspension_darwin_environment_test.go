// Port of the tests in mj-worker/src/worker_stop/suspension/darwin_environment.rs.

//go:build linux || darwin

package worker

import (
	"encoding/binary"
	"testing"
)

func TestDarwinSuspendMarkerIsAnExactEnvironmentRecord(t *testing.T) {
	marker := []byte("MJ_SESSION_ID=a")
	arguments := binary.NativeEndian.AppendUint32(nil, 2)
	arguments = append(arguments, []byte("/worker\x00\x00worker\x00MJ_SESSION_ID=a\x00TEXT=foo MJ_SESSION_ID=a\x00\x00")...)
	if containsMarker(arguments, marker) {
		t.Fatal("an argument and a value that mention the marker are not the record")
	}

	arguments = arguments[:len(arguments)-1]
	arguments = append(arguments, []byte("MJ_SESSION_ID=a\x00\x00")...)
	if !containsMarker(arguments, marker) {
		t.Fatal("the exact environment record was not found")
	}
}
