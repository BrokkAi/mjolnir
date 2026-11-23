// Port of the tests of mj-worker/src/memory_mcp.rs (`cfg(all(test, unix))`).

//go:build unix

package worker

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"slices"
	"syscall"
	"testing"
	"time"
)

// unixStreamPair is UnixStream::pair: two connected Unix stream sockets.
func unixStreamPair(t *testing.T) (*net.UnixConn, *net.UnixConn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatal(err)
	}
	conn := func(fd int, name string) *net.UnixConn {
		file := os.NewFile(uintptr(fd), name)
		defer file.Close()
		connection, err := net.FileConn(file)
		if err != nil {
			t.Fatal(err)
		}
		return connection.(*net.UnixConn)
	}
	return conn(fds[0], "server"), conn(fds[1], "client")
}

func memoryMCPSend(t *testing.T, stream *net.UnixConn, id uint64, method string) {
	t.Helper()
	body, err := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": id, "method": method, "params": map[string]any{}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := stream.Write(append(body, '\n')); err != nil {
		t.Fatal(err)
	}
}

func TestMcpRegistersHistoryToolsWithoutProjectDocumentTools(t *testing.T) {
	server, client := unixStreamPair(t)
	defer client.Close()
	if err := client.SetReadDeadline(time.Now().Add(5 * time.Second)); err != nil {
		t.Fatal(err)
	}
	serving := make(chan error, 1)
	go func() {
		defer server.Close()
		socket := "/missing.sock"
		serving <- memoryMCPServe(&socket, server, server)
	}()

	output := bufio.NewReader(client)
	memoryMCPSend(t, client, 0, "initialize")
	line, err := output.ReadString('\n')
	if err != nil {
		t.Fatal(err)
	}
	var initialized map[string]any
	if err := json.Unmarshal([]byte(line), &initialized); err != nil {
		t.Fatal(err)
	}
	if _, ok := mcpField(initialized, "result").(map[string]any)["instructions"]; ok {
		t.Fatal("the memory server must not send instructions")
	}
	if mcpField(initialized, "result", "serverInfo", "name") != "mj-memory" {
		t.Fatalf("%v", initialized)
	}
	memoryMCPSend(t, client, 1, "tools/list")
	if line, err = output.ReadString('\n'); err != nil {
		t.Fatal(err)
	}
	var reply map[string]any
	if err := json.Unmarshal([]byte(line), &reply); err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, tool := range mcpField(reply, "result", "tools").([]any) {
		names = append(names, mcpField(tool, "name").(string))
	}

	for _, name := range []string{"list", "read", "write"} {
		if slices.Contains(names, name) {
			t.Fatalf("unexpected document tool %s", name)
		}
	}
	for _, name := range []string{
		"search_sessions",
		"get_session_brief",
		"search_session",
		"read_session",
		"trace_file",
		"session_files",
		"blame_file",
	} {
		if !slices.Contains(names, name) {
			t.Fatalf("history tool %s is missing", name)
		}
	}

	if err := client.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-serving:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal(fmt.Errorf("the server did not stop at end of input"))
	}
}
