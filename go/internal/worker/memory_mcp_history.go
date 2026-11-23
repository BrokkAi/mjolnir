// Port of mj-worker/src/memory_mcp/history.rs.

// History tools and their connection-only worker transport.

package worker

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
	c.stopped.Store(true)
	c.mu.Lock()
	defer c.mu.Unlock()
	for _, stream := range c.streams {
		// Shutting both directions down, rather than closing, wakes a reader
		// blocked on the stream with end of input, as Rust's shutdown does.
		readErr := stream.CloseRead()
		writeErr := stream.CloseWrite()
		if err := errors.Join(readErr, writeErr); err != nil {
			slog.Debug("closing history connection", "error", err)
		}
	}
}

// call answers one history tools/call. A failed forward is a tool error the
// model sees, not a protocol error.
func (c *historyClient) call(params any) (any, bool, error) {
	object, _ := params.(jsonvalue.Object)
	nameValue, _ := object.Get("name")
	name, ok := nameValue.(string)
	if !ok {
		return nil, false, errors.New("missing history tool name")
	}
	arguments, ok := object.Get("arguments")
	if !ok {
		arguments = jsonvalue.Object{}
	}
	encoded, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "tool", Value: name},
		{Key: "arguments", Value: arguments},
	})
	if err != nil {
		return nil, false, err
	}
	query, err := core.DecodeHistoryQuery(encoded)
	if err != nil {
		return nil, false, err
	}
	if err := query.Validate(); err != nil {
		return nil, false, err
	}
	result, err := c.send(query)
	if err != nil {
		return jsonvalue.Object{{Key: "error", Value: err.Error()}}, true, nil
	}
	value, err := jsonvalue.Parse(result.Value)
	if err != nil {
		return jsonvalue.Object{{Key: "error", Value: fmt.Sprintf("decode history result: %v", err)}}, true, nil
	}
	return value, result.IsError, nil
}

// send forwards query to the worker and returns its result. Only Unix
// workers have the socket.
func (c *historyClient) send(query core.HistoryQuery) (core.HistoryResult, error) {
	if runtime.GOOS == "windows" {
		return core.HistoryResult{}, errors.New("history forwarding requires a supported worker socket")
	}
	if c.socket == nil {
		return core.HistoryResult{}, errors.New("history tools are unavailable; resume with a current controller")
	}
	id, err := core.NewSessionID()
	if err != nil {
		return core.HistoryResult{}, err
	}
	stream, err := core.ConnectUnixStream(*c.socket)
	if err != nil {
		return core.HistoryResult{}, err
	}
	defer stream.Close()
	connection := timeoutConn{conn: stream, read: core.QueryTimeout + 2*time.Second, write: 5 * time.Second}
	if err := c.register(id, stream); err != nil {
		return core.HistoryResult{}, err
	}
	defer c.unregister(id)
	envelope, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       id,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestHistoryQuery{Query: query},
	})
	if err != nil {
		return core.HistoryResult{}, err
	}
	if _, err := connection.Write(append(envelope, '\n')); err != nil {
		return core.HistoryResult{}, err
	}
	var line []byte
	read, complete, err := core.ReadBoundedLine(bufio.NewReader(connection), &line, core.MaxResponseBytes+4096)
	if err != nil {
		return core.HistoryResult{}, err
	}
	if read == 0 {
		return core.HistoryResult{}, errors.New("worker closed the history connection")
	}
	if !complete {
		return core.HistoryResult{}, errors.New("worker returned an incomplete history frame")
	}
	var response core.RelayResponseEnvelope
	if err := json.Unmarshal(line, &response); err != nil {
		return core.HistoryResult{}, err
	}
	if response.RequestID != id {
		return core.HistoryResult{}, errors.New("history reply identity mismatch")
	}
	if ok, isOk := response.Body.(core.RelayResponseBodyOk); isOk {
		if payload, isHistory := ok.Payload.(core.RelayResponsePayloadHistoryResult); isHistory {
			return payload.Result, nil
		}
	}
	return core.HistoryResult{}, fmt.Errorf("worker refused history query: %s", relayResponseBodyDebug(response.Body))
}

// register records an in-flight query's stream so stop can end it, unless
// the server is stopping or already has as many queries as a worker serves.
func (c *historyClient) register(id string, stream *net.UnixConn) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.stopped.Load() {
		return errors.New("memory MCP server is stopping")
	}
	if len(c.streams) >= core.MaxPending {
		return errors.New("too many concurrent history queries")
	}
	c.streams[id] = stream
	return nil
}

func (c *historyClient) unregister(id string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	delete(c.streams, id)
}

// relayResponseBodyDebug renders a relay response body as Rust's `{:?}`
// shows it, for the refusals the history and mailbox hook clients report.
func relayResponseBodyDebug(body core.RelayResponseBody) string {
	switch body := body.(type) {
	case core.RelayResponseBodyError:
		detail := "None"
		if body.Error.Detail != nil {
			detail = fmt.Sprintf("Some(%+v)", body.Error.Detail)
		}
		return fmt.Sprintf("Error { error: RelayProtocolError { code: %s, message: %s, retryable: %t, detail: %s } }",
			relayErrorCodeDebug(body.Error.Code), mcpDebugString(body.Error.Message), body.Error.Retryable, detail)
	case core.RelayResponseBodyOk:
		return fmt.Sprintf("Ok { payload: %s }", relayResponsePayloadDebug(body.Payload))
	}
	return fmt.Sprintf("%+v", body)
}

// relayResponsePayloadDebug names a payload as Rust's `{:?}` does, with its
// fields in Go's form.
func relayResponsePayloadDebug(payload core.RelayResponsePayload) string {
	name := fmt.Sprintf("%T", payload)
	name = name[strings.LastIndexByte(name, '.')+1:]
	name = strings.TrimPrefix(name, "RelayResponsePayload")
	return name + " " + fmt.Sprintf("%+v", payload)
}

// relayErrorCodeDebug is a relay error code's Rust variant name.
func relayErrorCodeDebug(code core.RelayErrorCode) string {
	var name strings.Builder
	for _, word := range strings.Split(string(code), "_") {
		if word != "" {
			name.WriteString(strings.ToUpper(word[:1]) + word[1:])
		}
	}
	return name.String()
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
	type member = jsonvalue.Member
	integer := func(members ...member) jsonvalue.Object {
		return append(jsonvalue.Object{{Key: "type", Value: "integer"}}, members...)
	}
	page := integer(member{Key: "minimum", Value: 1}, member{Key: "maximum", Value: 100}, member{Key: "default", Value: 20})
	chars := integer(member{Key: "minimum", Value: 1}, member{Key: "maximum", Value: 64000}, member{Key: "default", Value: 16000})
	id := jsonvalue.Object{{Key: "type", Value: "string"}, {Key: "description", Value: "Session ID returned by a history tool."}}
	text := jsonvalue.Object{{Key: "type", Value: "string"}}
	index := integer(member{Key: "minimum", Value: 0}, member{Key: "default", Value: 0})
	tool := func(name, description string, properties jsonvalue.Object, required ...string) any {
		requiredValues := make([]any, len(required))
		for i, field := range required {
			requiredValues[i] = field
		}
		return jsonvalue.Object{
			{Key: "name", Value: name},
			{Key: "description", Value: description},
			{Key: "annotations", Value: jsonvalue.Object{
				{Key: "readOnlyHint", Value: true},
				{Key: "destructiveHint", Value: false},
			}},
			{Key: "inputSchema", Value: jsonvalue.Object{
				{Key: "type", Value: "object"},
				{Key: "properties", Value: properties},
				{Key: "required", Value: requiredValues},
				{Key: "additionalProperties", Value: false},
			}},
		}
	}
	return []any{
		tool(
			"search_sessions",
			"Find earlier coding sessions across the controller's index. Search specific terms, then read the relevant evidence. Returns IDs and snippets.",
			jsonvalue.Object{{Key: "query", Value: text}, {Key: "limit", Value: page}},
			"query",
		),
		tool(
			"get_session_brief",
			"Read a bounded overview of an indexed session before choosing passages to inspect.",
			jsonvalue.Object{{Key: "session_id", Value: id}, {Key: "max_chars", Value: chars}},
			"session_id",
		),
		tool(
			"search_session",
			"Find case-insensitive literal text in one session, with message indices for read_session. Use next_start to continue.",
			jsonvalue.Object{
				{Key: "session_id", Value: id},
				{Key: "query", Value: text},
				{Key: "start", Value: index},
				{Key: "context", Value: integer(member{Key: "minimum", Value: 0}, member{Key: "maximum", Value: 5}, member{Key: "default", Value: 0})},
				{Key: "limit", Value: page},
				{Key: "max_chars", Value: chars},
			},
			"session_id", "query",
		),
		tool(
			"read_session",
			"Read a bounded transcript page. Follow next.start/offset to continue inside long messages. role=user produces an outline. Cite session ID and message indices.",
			jsonvalue.Object{
				{Key: "session_id", Value: id},
				{Key: "start", Value: index},
				{Key: "offset", Value: index},
				{Key: "role", Value: jsonvalue.Object{
					{Key: "type", Value: "string"},
					{Key: "enum", Value: []any{"user", "assistant", "tool"}},
				}},
				{Key: "limit", Value: page},
				{Key: "max_chars", Value: chars},
			},
			"session_id",
		),
		tool(
			"trace_file",
			"Find sessions that edited a recorded path or repository-relative suffix. Returns matched paths; equal suffixes do not establish repository identity.",
			jsonvalue.Object{{Key: "path", Value: text}, {Key: "limit", Value: page}},
			"path",
		),
		tool(
			"session_files",
			"List recorded files changed by one session, to judge whether its conversation is relevant. Empty evidence does not prove no files changed.",
			jsonvalue.Object{{Key: "session_id", Value: id}, {Key: "start", Value: index}, {Key: "limit", Value: page}},
			"session_id",
		),
		tool(
			"blame_file",
			"Attribute an inclusive range of up to 1000 lines using Git on this target and indexed session evidence. Paths are relative to the session working directory. Confidence is heuristic; inspect candidates' conversations.",
			jsonvalue.Object{
				{Key: "path", Value: text},
				{Key: "start_line", Value: integer(member{Key: "minimum", Value: 1})},
				{Key: "end_line", Value: integer(member{Key: "minimum", Value: 1})},
			},
			"path", "start_line", "end_line",
		),
	}
}
