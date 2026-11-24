//go:build unix

// Port of the tests in mj-accounts/src/login_terminal.rs (Rust:
// `#[cfg(all(test, unix))]`).

package accounts

import (
	"bytes"
	"context"
	"encoding/json"
	"net"
	"os/exec"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// Hard-won: 29354e991: a disconnect left the root blocked writing to a full PTY while cleanup waited for its exit.
func TestDisconnectCleanupDrainsOutputTheExitingRootWrites(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	connection, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	server, err := listener.Accept()
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close()
	// On hangup the root writes far more than the one-chunk channel and the
	// PTY buffer hold, as a root's exit output can when nobody reads it.
	command := exec.Command("/bin/sh", "-c",
		"trap 'head -c 1048576 /dev/zero | tr \"\\000\" x; exit 0' HUP; "+
			"printf ready; while :; do sleep 0.05; done")
	serving := make(chan error, 1)
	go func() {
		serving <- LoginTerminalServe(context.Background(), server, command, 80, 24)
	}()
	transcript := []byte{}
	for !bytes.HasSuffix(transcript, []byte("ready")) {
		body, err := client.ReadFrame[json.RawMessage](connection)
		if err != nil {
			t.Fatal(err)
		}
		frame, err := client.UnmarshalTerminalOutputJSON(body)
		if err != nil {
			t.Fatal(err)
		}
		output, ok := frame.(client.TerminalOutputBytes)
		if !ok {
			t.Fatalf("login ended before it was ready: %#v", frame)
		}
		transcript = append(transcript, output.Bytes...)
	}
	connection.Close()
	// Bounds only the hang this test exists to catch; cleanup itself takes
	// milliseconds.
	select {
	case err := <-serving:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("disconnect cleanup waited on a root blocked writing its output")
	}
}
