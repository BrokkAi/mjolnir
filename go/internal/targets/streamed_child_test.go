// Port of the tests in mj-targets/src/streamed_child.rs.

package targets

import (
	"bytes"
	"io"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// The relay's stdout reader strips the marker even when it arrives in
// pieces, passes the protocol bytes after it through whole (past a pipe's
// worth), and reports the start before any protocol byte is read.
func TestTheStartMarkerIsStrippedFromARelayStreamSplitAcrossWrites(t *testing.T) {
	reader, writer := io.Pipe()
	gate := core.StartGateStandalone()
	stripped := &stripStart{inner: reader, scan: core.NewStartScan(), gate: gate}
	payload := make([]byte, 200_000)
	for n := range payload {
		payload[n] = byte(n % 251)
	}
	expected := bytes.Clone(payload)
	fed := make(chan error, 1)
	go func() {
		marker := []byte(core.SSHStartMarker)
		head, tail := marker[:9], marker[9:]
		if _, err := writer.Write(head); err != nil {
			fed <- err
			return
		}
		if _, err := writer.Write(tail); err != nil {
			fed <- err
			return
		}
		_, err := writer.Write(payload)
		fed <- err
		_ = writer.Close()
	}()
	received, err := io.ReadAll(stripped)
	if err != nil {
		t.Fatal(err)
	}
	if err := <-fed; err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(received, expected) {
		t.Fatalf("received %d bytes that differ from the %d sent", len(received), len(expected))
	}
	if phase := gate.Phase(); phase != core.StartPhaseStarted {
		t.Fatalf("phase = %v, want Started", phase)
	}
}
