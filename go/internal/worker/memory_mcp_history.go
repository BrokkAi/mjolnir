// Port of mj-worker/src/memory_mcp/history.rs.

// History tools and their connection-only worker transport.

package worker

import (
	"errors"
	"io"
	"net"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// historyClient forwards history tool calls to the worker's history socket.
// Rust clones share it; in Go it is shared by pointer.
type historyClient struct {
	// socket is the worker's history socket; nil when the tools are
	// unavailable.
	socket  *string
	stopped atomic.Bool
	mu      sync.Mutex
	// streams are the in-flight queries' connections by request id, so stop
	// can end them (Unix only).
	streams map[string]*net.UnixConn
}

func newHistoryClient(socket *string) *historyClient {
	return &historyClient{socket: socket, streams: map[string]*net.UnixConn{}}
}

func (c *historyClient) enabled() bool { return c.socket != nil }

// stop refuses new queries and shuts down every in-flight connection.
func (c *historyClient) stop() {
	panic("unported: mj-worker/src/memory_mcp/history.rs Client::stop")
}

// call answers one history tools/call. A failed forward is a tool error the
// model sees, not a protocol error.
func (c *historyClient) call(params any) (any, bool, error) {
	panic("unported: mj-worker/src/memory_mcp/history.rs Client::call")
}

// send forwards query to the worker and returns its result. Only Unix
// workers have the socket.
func (c *historyClient) send(query core.HistoryQuery) (core.HistoryResult, error) {
	panic("unported: mj-worker/src/memory_mcp/history.rs Client::send")
}

// historyShutdownReader ends remote calls before the shared MCP server joins
// its call goroutines: it stops the client at end of input, and on Close
// (Rust's Drop), which Serve calls before joining.
type historyShutdownReader struct {
	reader io.Reader
	client *historyClient
}

func newHistoryShutdownReader(reader io.Reader, client *historyClient) *historyShutdownReader {
	return &historyShutdownReader{reader: reader, client: client}
}

// Read reads from the wrapped reader and stops the client at end of input.
func (r *historyShutdownReader) Read(bytes []byte) (int, error) {
	n, err := r.reader.Read(bytes)
	if errors.Is(err, io.EOF) {
		r.client.stop()
	}
	return n, err
}

// Close stops the client, as dropping the Rust reader does.
func (r *historyShutdownReader) Close() error {
	r.client.stop()
	return nil
}

// historyToolDefinitions is the MCP definitions of the history tools.
func historyToolDefinitions() []any {
	panic("unported: mj-worker/src/memory_mcp/history.rs tool_definitions")
}
