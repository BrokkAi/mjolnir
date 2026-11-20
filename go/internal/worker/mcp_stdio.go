// Port of mj-worker/src/mcp_stdio.rs.

// The JSON-lines stdio transport shared by the worker's MCP servers.
//
// Hel's MCP servers (session history, project memory, and sub-agents) are
// hand-rolled rather than built on an SDK. They differ only in their name,
// instructions, tools and call handler, so the JSON-RPC loop lives here once.
//
// JSON values are the untyped values of github.com/BrokkAi/acp-go/jsonvalue
// (serde_json::Value's Go form).

package worker

import (
	"io"
	"net"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// ProgressInterval is how often a long call reports that it is still
// working, when the client asked to be kept informed. Claude Code abandons a
// stdio MCP call that sends neither a response nor a progress notification
// for 1800 seconds, so a call that reports every half minute never runs into
// that limit.
const ProgressInterval = 30 * time.Second

// MCPCall answers one `tools/call`: the structured result and whether it is
// a tool error the model can correct. An error becomes a JSON-RPC
// invalid-params error. present is false when the request had no `params`
// (Rust's None), as distinct from `"params": null`. The Progress handle
// reports that a slow call is still working; a handler with nothing useful to
// say ignores it.
type MCPCall func(params any, present bool, progress *Progress) (structured any, isError bool, err error)

// MCPServer is one hand-rolled MCP server (Rust's McpServer<F>).
type MCPServer struct {
	Name  string
	Tools []any
	// ProgressInterval is how often an in-flight call may report progress.
	// Tests shorten it; in production it is the package's ProgressInterval.
	ProgressInterval time.Duration
	Call             MCPCall
}

// Progress reports that one in-flight `tools/call` is still working.
//
// MCP only allows progress for a request whose caller asked for it, by
// putting a `progressToken` in the call's `_meta`. Without that token this
// handle does nothing, so a client that never asked is never sent anything it
// did not expect.
type Progress struct {
	// token is the request's progress token; hasToken is false when it asked
	// for none.
	token    any
	hasToken bool
	interval time.Duration
	emit     func(value any)
}

// Interval is how often Notify should be called while a call is in flight.
func (p *Progress) Interval() time.Duration { return p.interval }

// ProgressSilent is a handle that reports nothing, for tests that call a
// handler directly with no client behind it.
func ProgressSilent(interval time.Duration) *Progress {
	return &Progress{interval: interval, emit: func(any) {}}
}

// Notify sends one `notifications/progress`. elapsed is how long the call
// has been running and total (nil for none) is the deadline it is working
// towards, both in seconds, so a client can show how far along the call is.
func (p *Progress) Notify(elapsed uint64, total *uint64, message string) {
	if !p.hasToken {
		return
	}
	params := jsonvalue.Object{
		{Key: "progressToken", Value: p.token},
		{Key: "progress", Value: elapsed},
		{Key: "message", Value: message},
	}
	if total != nil {
		params = append(params, jsonvalue.Member{Key: "total", Value: *total})
	}
	p.emit(jsonvalue.Object{
		{Key: "jsonrpc", Value: "2.0"},
		{Key: "method", Value: "notifications/progress"},
		{Key: "params", Value: params},
	})
}

// mcpOutput is the server's output stream, shared by the request loop, the
// call goroutines and their progress handles (Rust's Arc<Mutex<W>>).
type mcpOutput struct {
	mu     sync.Mutex
	writer io.Writer
}

// Serve serves server over reader and writer until the reader closes, then
// lets in-flight concurrent calls finish. A reader that is also an io.Closer
// is closed before those calls are joined, as Rust drops its reader there.
func Serve(reader io.Reader, writer io.Writer, server MCPServer) error {
	panic("unported: mj-worker/src/mcp_stdio.rs serve")
}

// mcpProgressHandle binds a progress handle to this server's output stream.
// Every notification it writes goes through the same lock as a response, so
// lines never interleave.
func mcpProgressHandle(output *mcpOutput, token any, hasToken bool, interval time.Duration) *Progress {
	panic("unported: mj-worker/src/mcp_stdio.rs progress_handle")
}

// mcpToolResponse is the JSON-RPC response to one call's result.
func mcpToolResponse(id any, structured any, isError bool, err error) any {
	panic("unported: mj-worker/src/mcp_stdio.rs tool_response")
}

func mcpRPCResult(id any, result any) any {
	return jsonvalue.Object{
		{Key: "jsonrpc", Value: "2.0"},
		{Key: "id", Value: id},
		{Key: "result", Value: result},
	}
}

func mcpRPCError(id any, code int64, message string) any {
	return jsonvalue.Object{
		{Key: "jsonrpc", Value: "2.0"},
		{Key: "id", Value: id},
		{Key: "error", Value: jsonvalue.Object{
			{Key: "code", Value: code},
			{Key: "message", Value: message},
		}},
	}
}

// ConnectTimeout is how long a connect may wait for the worker to accept. A
// Unix socket connect only blocks when the listener's backlog is full, which
// means the worker has stopped accepting.
const ConnectTimeout = 10 * time.Second

// SocketRequest sends one JSON request line over a worker Unix socket and
// reads one JSON reply line. what names the exchange in errors. The connect
// is bounded by ConnectTimeout; the reply must arrive within replyTimeout,
// and a nil reply with no error reports that it did not, so a worker that
// never answers turns into a bounded failure instead of an indefinite hang.
// Workers run on Unix; elsewhere this says plainly that it cannot run.
func SocketRequest[A any](socket string, request any, what string, replyTimeout time.Duration) (*A, error) {
	panic("unported: mj-worker/src/mcp_stdio.rs socket_request")
}

// mcpConnectWithTimeout connects on a helper goroutine so the wait is
// bounded. A goroutine still blocked in connect after the timeout is
// abandoned; it exits once the connect resolves.
func mcpConnectWithTimeout(socket string) (*net.UnixConn, error) {
	panic("unported: mj-worker/src/mcp_stdio.rs connect_with_timeout")
}

// mcpWriteLine serializes first and takes the lock for one write, so
// concurrent responses never interleave.
func mcpWriteLine(output *mcpOutput, value any) error {
	panic("unported: mj-worker/src/mcp_stdio.rs write_line")
}
