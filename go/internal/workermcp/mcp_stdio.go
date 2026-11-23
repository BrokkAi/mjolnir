// Port of mj-worker-mcp/src/mcp_stdio.rs.

// The JSON-lines stdio transport shared by the worker's MCP servers.
//
// Hel's MCP servers (session history, project memory, and sub-agents) are
// hand-rolled rather than built on an SDK. They differ only in their name,
// instructions, tools and call handler, so the JSON-RPC loop lives here once.
//
// JSON values are the untyped values of github.com/BrokkAi/acp-go/jsonvalue
// (serde_json::Value's Go form).

package workermcp

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"runtime"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	output := &mcpOutput{writer: writer}
	var calls sync.WaitGroup
	lines := bufio.NewReader(reader)
	err := func() error {
		// Closing the reader is Rust's drop of the line iterator, which ends
		// the loop on every path, the early returns included.
		if closer, ok := reader.(io.Closer); ok {
			defer closer.Close()
		}
		for {
			line, ok, err := mcpReadLine(lines)
			if err != nil {
				return fmt.Errorf("read MCP request: %w", err)
			}
			if !ok {
				return nil
			}
			if strings.TrimSpace(line) == "" {
				continue
			}
			parsed, err := jsonvalue.Parse([]byte(line))
			if err != nil {
				if err := mcpWriteLine(output, mcpRPCError(nil, -32700, err.Error())); err != nil {
					return err
				}
				continue
			}
			request, _ := parsed.(jsonvalue.Object)
			id, ok := request.Get("id")
			if !ok {
				continue
			}
			method := ""
			if value, ok := request.Get("method"); ok {
				method, _ = value.(string)
			}
			var response any
			switch method {
			case "initialize":
				protocolVersion, ok := mcpJSONPointer(request, "params", "protocolVersion")
				if !ok {
					protocolVersion = "2025-03-26"
				}
				response = mcpRPCResult(id, jsonvalue.Object{
					{Key: "protocolVersion", Value: protocolVersion},
					{Key: "capabilities", Value: jsonvalue.Object{
						{Key: "tools", Value: jsonvalue.Object{{Key: "listChanged", Value: false}}},
					}},
					{Key: "serverInfo", Value: jsonvalue.Object{
						{Key: "name", Value: server.Name},
						{Key: "version", Value: core.PackageVersion},
					}},
				})
			case "ping":
				response = mcpRPCResult(id, jsonvalue.Object{})
			case "tools/list":
				tools := server.Tools
				if tools == nil {
					tools = []any{}
				}
				response = mcpRPCResult(id, jsonvalue.Object{{Key: "tools", Value: tools}})
			case "tools/call":
				params, present := request.Get("params")
				token, hasToken := mcpJSONPointer(params, "_meta", "progressToken")
				interval := server.ProgressInterval
				calls.Add(1)
				go func() {
					defer calls.Done()
					defer func() {
						if recovered := recover(); recovered != nil {
							slog.Warn("an MCP tool call thread panicked", "panic", fmt.Sprint(recovered))
						}
					}()
					progress := mcpProgressHandle(output, token, hasToken, interval)
					structured, isError, err := server.Call(params, present, progress)
					response := mcpToolResponse(id, structured, isError, err)
					if err := mcpWriteLine(output, response); err != nil {
						slog.Warn("could not write an MCP tool response", "error", err)
					}
				}()
				continue
			default:
				response = mcpRPCError(id, -32601, fmt.Sprintf("unknown MCP method %s", mcpDebugString(method)))
			}
			if err := mcpWriteLine(output, response); err != nil {
				return err
			}
		}
	}()
	if err != nil {
		// Rust drops the call handles on an early return: the calls run on
		// without being joined.
		return err
	}
	calls.Wait()
	return nil
}

// mcpReadLine is one item of BufRead::lines: the next line without its
// "\n" or "\r\n", false at end of input, and an error for text that is not
// UTF-8.
func mcpReadLine(reader *bufio.Reader) (string, bool, error) {
	line, err := reader.ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return "", false, err
	}
	if line == "" && err != nil {
		return "", false, nil
	}
	if !utf8.ValidString(line) {
		return "", false, errors.New("stream did not contain valid UTF-8")
	}
	line = strings.TrimSuffix(line, "\n")
	line = strings.TrimSuffix(line, "\r")
	return line, true, nil
}

// mcpJSONPointer is serde_json's Value::pointer for a path of object keys:
// the value found by following path from value.
func mcpJSONPointer(value any, path ...string) (any, bool) {
	for _, key := range path {
		object, ok := value.(jsonvalue.Object)
		if !ok {
			return nil, false
		}
		if value, ok = object.Get(key); !ok {
			return nil, false
		}
	}
	return value, true
}

// mcpDebugString is a string as Rust's `{:?}` shows it.
func mcpDebugString(text string) string {
	var quoted strings.Builder
	quoted.WriteByte('"')
	for _, r := range text {
		switch r {
		case '"':
			quoted.WriteString(`\"`)
		case '\\':
			quoted.WriteString(`\\`)
		case '\n':
			quoted.WriteString(`\n`)
		case '\r':
			quoted.WriteString(`\r`)
		case '\t':
			quoted.WriteString(`\t`)
		case 0:
			quoted.WriteString(`\0`)
		default:
			if r < 0x20 || r == 0x7f {
				fmt.Fprintf(&quoted, `\u{%x}`, r)
			} else {
				quoted.WriteRune(r)
			}
		}
	}
	quoted.WriteByte('"')
	return quoted.String()
}

// mcpProgressHandle binds a progress handle to this server's output stream.
// Every notification it writes goes through the same lock as a response, so
// lines never interleave.
func mcpProgressHandle(output *mcpOutput, token any, hasToken bool, interval time.Duration) *Progress {
	return &Progress{
		token:    token,
		hasToken: hasToken,
		interval: interval,
		emit: func(value any) {
			if err := mcpWriteLine(output, value); err != nil {
				slog.Warn("could not write an MCP progress notification", "error", err)
			}
		},
	}
}

// mcpToolResponse is the JSON-RPC response to one call's result.
func mcpToolResponse(id any, structured any, isError bool, err error) any {
	if err != nil {
		return mcpRPCError(id, -32602, err.Error())
	}
	text, prettyErr := mcpPrettyJSON(structured)
	if prettyErr != nil {
		compact, _ := jsonvalue.Marshal(structured)
		text = string(compact)
	}
	return mcpRPCResult(id, jsonvalue.Object{
		{Key: "content", Value: []any{jsonvalue.Object{
			{Key: "type", Value: "text"},
			{Key: "text", Value: text},
		}}},
		{Key: "structuredContent", Value: structured},
		{Key: "isError", Value: isError},
	})
}

// mcpPrettyJSON is serde_json::to_string_pretty: two-space indentation,
// members in order, and serde_json's string and number forms.
func mcpPrettyJSON(value any) (string, error) {
	compact, err := jsonvalue.Marshal(value)
	if err != nil {
		return "", err
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, compact, "", "  "); err != nil {
		return "", err
	}
	return pretty.String(), nil
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
	if runtime.GOOS == "windows" {
		return nil, fmt.Errorf("the %s socket %s needs a Unix platform", what, socket)
	}
	stream, err := mcpConnectWithTimeout(socket)
	if err != nil {
		return nil, fmt.Errorf("connect to the %s socket %s: %w", what, socket, err)
	}
	defer stream.Close()
	connection := timeoutConn{conn: stream, read: replyTimeout}
	body, err := jsonvalue.Marshal(request)
	if err != nil {
		return nil, err
	}
	if _, err := connection.Write(append(body, '\n')); err != nil {
		return nil, fmt.Errorf("send the %s request: %w", what, err)
	}
	line, err := bufio.NewReader(connection).ReadString('\n')
	switch {
	case errors.Is(err, os.ErrDeadlineExceeded):
		return nil, nil
	case errors.Is(err, io.EOF) && line == "":
		return nil, fmt.Errorf("the %s socket closed without a reply", what)
	case err != nil && !errors.Is(err, io.EOF):
		return nil, fmt.Errorf("read the %s reply: %w", what, err)
	}
	if !utf8.ValidString(line) {
		return nil, fmt.Errorf("read the %s reply: stream did not contain valid UTF-8", what)
	}
	var reply A
	if err := json.Unmarshal([]byte(strings.TrimSpace(line)), &reply); err != nil {
		return nil, fmt.Errorf("parse the %s reply: %w", what, err)
	}
	return &reply, nil
}

// mcpConnectWithTimeout connects on a helper goroutine so the wait is
// bounded. A goroutine still blocked in connect after the timeout is
// abandoned; it exits once the connect resolves.
func mcpConnectWithTimeout(socket string) (*net.UnixConn, error) {
	type connected struct {
		conn *net.UnixConn
		err  error
	}
	result := make(chan connected, 1)
	go func() {
		conn, err := core.ConnectUnixStream(socket)
		result <- connected{conn: conn, err: err}
	}()
	timeout := time.NewTimer(ConnectTimeout)
	defer timeout.Stop()
	select {
	case outcome := <-result:
		return outcome.conn, outcome.err
	case <-timeout.C:
		// The abandoned connect closes its stream if it ever succeeds.
		go func() {
			if outcome := <-result; outcome.conn != nil {
				outcome.conn.Close()
			}
		}()
		return nil, fmt.Errorf("no accept within %ds", int(ConnectTimeout/time.Second))
	}
}

// mcpWriteLine serializes first and takes the lock for one write, so
// concurrent responses never interleave.
func mcpWriteLine(output *mcpOutput, value any) error {
	body, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	body = append(body, '\n')
	output.mu.Lock()
	defer output.mu.Unlock()
	if _, err := output.writer.Write(body); err != nil {
		return fmt.Errorf("write MCP response: %w", err)
	}
	if flusher, ok := output.writer.(interface{ Flush() error }); ok {
		if err := flusher.Flush(); err != nil {
			return fmt.Errorf("flush MCP response: %w", err)
		}
	}
	return nil
}
