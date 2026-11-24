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
	panic("unported: mj-import/src/import/test_fixtures.rs grok_session")
}

// KimiSession writes a Kimi Code session directory with the
// `session_index.jsonl` entry that makes it visible, its `state.json`
// metadata and one agent wire log. It returns the session directory.
func KimiSession(home string, id string, cwd string, title string, wire string) string {
	panic("unported: mj-import/src/import/test_fixtures.rs kimi_session")
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
	panic("unported: mj-import/src/import/test_fixtures.rs write_muse_session")
}
