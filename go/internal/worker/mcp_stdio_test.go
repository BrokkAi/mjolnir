// Port of the tests of mj-worker/src/mcp_stdio.rs (`cfg(all(test, unix))`).

//go:build unix

package worker

import (
	"bufio"
	"encoding/json"
	"errors"
	"io"
	"net"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// mcpHangGuard is only a guard against a hang, never a deadline a result is
// judged by, so it can be as generous as a loaded machine needs.
const mcpHangGuard = 120 * time.Second

// mcpSharedWriter collects everything the server writes, so a test can read
// it after Serve has consumed the writer.
type mcpSharedWriter struct {
	mu     sync.Mutex
	buffer []byte
}

func (w *mcpSharedWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	w.buffer = append(w.buffer, data...)
	return len(data), nil
}

// serveASlowCall serves one request with a fixed number of progress ticks,
// independent of scheduler delays, and returns everything the server wrote.
func serveASlowCall(t *testing.T, request string, interval time.Duration, ticks uint64) []map[string]any {
	t.Helper()
	output := &mcpSharedWriter{}
	err := Serve(strings.NewReader(request+"\n"), output, MCPServer{
		Name: "test",
		Tools: []any{jsonvalue.Object{
			{Key: "name", Value: "slow"},
			{Key: "inputSchema", Value: jsonvalue.Object{{Key: "type", Value: "object"}}},
		}},
		ProgressInterval: interval,
		Call: func(_ any, _ bool, progress *Progress) (any, bool, error) {
			total := uint64(60)
			for tick := range ticks {
				time.Sleep(progress.Interval())
				progress.Notify(tick, &total, "still working")
			}
			return jsonvalue.Object{{Key: "done", Value: true}}, false, nil
		},
	})
	if err != nil {
		t.Fatalf("the server serves the request: %v", err)
	}
	return mcpWrittenLines(t, output)
}

func mcpWrittenLines(t *testing.T, output *mcpSharedWriter) []map[string]any {
	t.Helper()
	output.mu.Lock()
	defer output.mu.Unlock()
	var lines []map[string]any
	for _, line := range strings.Split(string(output.buffer), "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		var value map[string]any
		if err := json.Unmarshal([]byte(line), &value); err != nil {
			t.Fatalf("each line is one JSON message: %v", err)
		}
		lines = append(lines, value)
	}
	return lines
}

// mcpField follows path through decoded JSON objects.
func mcpField(value any, path ...string) any {
	for _, key := range path {
		object, ok := value.(map[string]any)
		if !ok {
			return nil
		}
		value = object[key]
	}
	return value
}

// Hard-won: da95decb: long sub-agent waits exceeded the MCP client silence limit without progress.
func TestACallWithAProgressTokenGetsProgressLinesBeforeItsResponse(t *testing.T) {
	lines := serveASlowCall(t,
		`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slow","arguments":{},"_meta":{"progressToken":"tok-7"}}}`,
		20*time.Millisecond, 3)
	var notifications []map[string]any
	for _, line := range lines {
		if line["method"] == "notifications/progress" {
			notifications = append(notifications, line)
		}
	}
	if len(notifications) < 2 {
		t.Fatalf("a slow call must report more than once: %v", lines)
	}
	if mcpField(notifications[0], "params", "progressToken") != "tok-7" ||
		mcpField(notifications[0], "params", "total") != 60.0 ||
		mcpField(notifications[0], "params", "message") != "still working" {
		t.Fatalf("%v", notifications[0])
	}
	if mcpField(lines[len(lines)-1], "result", "structuredContent", "done") != true {
		t.Fatalf("the response must come last: %v", lines)
	}
}

func TestACallWithoutAProgressTokenIsAnsweredWithNoNotifications(t *testing.T) {
	lines := serveASlowCall(t,
		`{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slow","arguments":{}}}`,
		20*time.Millisecond, 3)
	for _, line := range lines {
		if line["method"] != nil {
			t.Fatalf("a client that asked for no progress must be sent none: %v", lines)
		}
	}
	if len(lines) != 1 {
		t.Fatalf("%v", lines)
	}
}

// mcpFakeWorker accepts one connection, reads the request line, then runs
// answer on the stream and holds it open until the client goes away.
func mcpFakeWorker(t *testing.T, socket string, answer func(net.Conn)) {
	t.Helper()
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	go func() {
		stream, err := listener.Accept()
		if err != nil {
			return
		}
		defer stream.Close()
		reader := bufio.NewReader(stream)
		if _, err := reader.ReadString('\n'); err != nil {
			return
		}
		answer(stream)
		stream.Read(make([]byte, 1))
	}()
}

// Hard-won: ff3d662a: a lost worker reply after daemon restart left MCP calls blocked forever.
func TestSocketRequestReportsAMissingReplyWithinTheTimeout(t *testing.T) {
	socket := filepath.Join(t.TempDir(), "worker.sock")
	// The worker never replies. It reports the event that proves the timeout
	// fired instead of racing a wall clock: the client hung up, having been
	// sent nothing.
	hungUp := make(chan bool, 1)
	mcpFakeWorker(t, socket, func(stream net.Conn) {
		read, err := stream.Read(make([]byte, 1))
		hungUp <- read == 0 && errors.Is(err, io.EOF)
	})

	reply, err := SocketRequest[any](socket, jsonvalue.Object{{Key: "ping", Value: true}}, "test", 200*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	// A nil reply comes only from the read-timeout arm; a closed socket or an
	// unreadable reply is an error instead.
	if reply != nil {
		t.Fatalf("reply = %v", *reply)
	}
	select {
	case ended := <-hungUp:
		if !ended {
			t.Fatal("the call must drop the connection once the reply timeout passes")
		}
	case <-time.After(mcpHangGuard):
		t.Fatal("the call must drop the connection once the reply timeout passes")
	}
}

func TestSocketRequestReportsAClosedSocketAsAnError(t *testing.T) {
	socket := filepath.Join(t.TempDir(), "worker.sock")
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	go func() {
		stream, err := listener.Accept()
		if err != nil {
			return
		}
		bufio.NewReader(stream).ReadString('\n')
		// Drop without answering.
		stream.Close()
	}()

	_, err = SocketRequest[any](socket, jsonvalue.Object{{Key: "ping", Value: true}}, "test", 5*time.Second)
	if err == nil || !strings.Contains(err.Error(), "closed without a reply") {
		t.Fatalf("error = %v", err)
	}
}
