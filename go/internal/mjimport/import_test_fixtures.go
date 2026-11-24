// Port of mj-import/src/import/test_fixtures.rs.
//
// Native session fixtures shared by the import tests and by the tests of
// anything else that reads a native session, such as the SessionWiki
// adapters. One writer per harness keeps every test writing the same shape.
// Like Rust's `pub mod test_fixtures`, this is part of the package so other
// packages' tests can use it; the writers panic on failure, as Rust's
// unwrap does.

package mjimport

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const MuseID = "01a08120-6536-7721-8ddf-e0df0e921c2c"

// NativeCwd is posix, an absolute path such as `/work/app`, as a native
// harness on this machine records it. Import requires a working directory
// absolute here, and on Windows that takes a drive; `C:/` needs no escaping
// inside JSON.
func NativeCwd(posix string) string {
	if runtime.GOOS == "windows" {
		return "C:" + posix
	}
	return posix
}

// NativeFixture is fixture with every quoted `/work/...` path made native,
// as NativeCwd.
func NativeFixture(fixture string) string {
	if runtime.GOOS == "windows" {
		return strings.ReplaceAll(fixture, `"/work/`, `"C:/work/`)
	}
	return fixture
}

// GrokSession writes a synthetic `chat_history.jsonl`, modeled on the shape
// Grok Build writes: internally tagged items, user content as typed parts,
// reasoning summaries beside the assistant turn, and `search_replace` tool
// calls paired with their results. It returns the session directory.
func GrokSession(directory string, cwd string, history string) string {
	// Join components so the seeded index key matches native directory
	// discovery.
	session := filepath.Join(directory, "sessions", "%2Fwork%2Fapp", "01a00c3a-553f-71e0-95ab-aa04396d3ad7")
	fixtureMust(os.MkdirAll(session, 0o777))
	fixtureMust(os.WriteFile(filepath.Join(session, "summary.json"), fixtureJSON(jsonvalue.Object{
		{Key: "info", Value: jsonvalue.Object{
			{Key: "id", Value: "01a00c3a-553f-71e0-95ab-aa04396d3ad7"},
			{Key: "cwd", Value: cwd},
		}},
		{Key: "session_summary", Value: ""},
		{Key: "num_chat_messages", Value: int64(4)},
		{Key: "current_model_id", Value: "grok-4.6"},
		{Key: "grok_home", Value: "/home/me/.grok"},
	}), 0o666))
	fixtureMust(os.WriteFile(filepath.Join(session, "chat_history.jsonl"), []byte(history), 0o666))
	return session
}

// KimiSession writes a Kimi Code session directory with the
// `session_index.jsonl` entry that makes it visible, its `state.json`
// metadata and one agent wire log. It returns the session directory.
func KimiSession(home string, id string, cwd string, title string, wire string) string {
	session := filepath.Join(home, "sessions", "project", id)
	fixtureMust(os.MkdirAll(filepath.Join(session, "agents", "main"), 0o777))
	fixtureMust(os.WriteFile(filepath.Join(session, "state.json"), fixtureJSON(jsonvalue.Object{
		{Key: "workDir", Value: cwd},
		{Key: "customTitle", Value: title},
	}), 0o666))
	fixtureMust(os.WriteFile(filepath.Join(session, "agents", "main", "wire.jsonl"), []byte(wire), 0o666))
	fixtureMust(os.WriteFile(filepath.Join(home, "session_index.jsonl"), fixtureJSON(jsonvalue.Object{
		{Key: "sessionId", Value: id},
		{Key: "sessionDir", Value: session},
		{Key: "workDir", Value: cwd},
	}), 0o666))
	return session
}

// fixtureMust is Rust's `unwrap` for the fixture writers.
func fixtureMust(err error) {
	if err != nil {
		panic(err)
	}
}

// fixtureJSON is serde_json's `to_string` of a fixture value.
func fixtureJSON(value any) []byte {
	encoded, err := jsonvalue.Marshal(value)
	fixtureMust(err)
	return encoded
}

// MuseRecord is one Muse session record of the fixture stream, as a
// jsonvalue value; payload is a jsonvalue value.
func MuseRecord(id string, sequence uint64, payloadType string, payload any) jsonvalue.Object {
	return jsonvalue.Object{
		{Key: "schema_version", Value: int64(1)},
		{Key: "id", Value: id},
		{Key: "stream", Value: jsonvalue.Object{{Key: "kind", Value: "session"}, {Key: "id", Value: MuseID}}},
		{Key: "sequence", Value: sequence},
		{Key: "recorded_at", Value: int64(1_788_872_000_000_000) + int64(sequence)},
		{Key: "record_type", Value: "event"},
		{Key: "durability", Value: "durable"},
		{Key: "payload_type", Value: payloadType},
		{Key: "payload_schema_version", Value: int64(1)},
		{Key: "payload", Value: payload},
	}
}

// WriteMuseSession writes the fixture Muse session MuseID, with workspace
// root cwd, to path.
func WriteMuseSession(path string, id string, cwd string) {
	metadata := func() jsonvalue.Object {
		return jsonvalue.Object{
			{Key: "kind", Value: "metadata"},
			{Key: "record", Value: jsonvalue.Object{
				{Key: "workspace_root", Value: cwd},
				{Key: "provider_id", Value: "test"},
			}},
		}
	}
	runEvent := func(event jsonvalue.Object) jsonvalue.Object {
		return jsonvalue.Object{
			{Key: "kind", Value: "run"},
			{Key: "run_id", Value: "run-1"},
			{Key: "event", Value: event},
		}
	}
	records := []jsonvalue.Object{
		MuseRecord("metadata-1", 1, "runtime.session.metadata", metadata()),
		MuseRecord("metadata-2", 2, "runtime.session.metadata", metadata()),
		MuseRecord("intent-1", 3, "runtime.user_intent.accepted", jsonvalue.Object{
			{Key: "intent_id", Value: "run-1"},
			{Key: "model_messages", Value: []any{
				jsonvalue.Object{{Key: "content", Value: []any{
					jsonvalue.Object{{Key: "kind", Value: "text"}, {Key: "text", Value: "remember Muse import"}},
				}}},
			}},
		}),
		MuseRecord("started-1", 4, "runtime.session", runEvent(jsonvalue.Object{{Key: "kind", Value: "started"}})),
		MuseRecord("tools-1", 5, "runtime.session", runEvent(jsonvalue.Object{
			{Key: "kind", Value: "assistant_tool_calls_committed"},
			{Key: "tool_calls", Value: []any{jsonvalue.Object{
				{Key: "id", Value: "call-1"},
				{Key: "name", Value: "write_file"},
				{Key: "args", Value: `{"path":"native-marker.txt","content":"ok"}`},
			}}},
		})),
		MuseRecord("assistant-1", 6, "runtime.session", runEvent(jsonvalue.Object{
			{Key: "kind", Value: "assistant_message_committed"},
			{Key: "message_id", Value: "message-1"},
			{Key: "text", Value: "hello back"},
		})),
		MuseRecord("terminal-1", 7, "runtime.session", runEvent(jsonvalue.Object{
			{Key: "kind", Value: "terminal"},
			{Key: "terminal", Value: "completed"},
		})),
	}
	var body strings.Builder
	for _, record := range records {
		body.Write(fixtureJSON(record))
		body.WriteString("\n")
	}
	if id != MuseID {
		panic(fmt.Sprintf("assertion `left == right` failed\n  left: %q\n right: %q", id, MuseID))
	}
	fixtureMust(os.WriteFile(path, []byte(body.String()), 0o666))
}
