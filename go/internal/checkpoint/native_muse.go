// Port of mj-checkpoint/src/native/muse.rs.
//
// Reader and workspace relocation support for Muse Code session logs.
//
// Muse stores one JSON value per line. Most lines are ordinary event
// envelopes; permission transactions are retained as opaque frames whose
// children contain JSON text. The frame bytes are part of Muse's integrity
// surface, so relocation only rewrites the exact workspace-root string in
// ordinary metadata records.
//
// JSON values are the untyped values of github.com/BrokkAi/acp-go/jsonvalue
// (serde_json::Value's Go form): nil, bool, string, int64, uint64, float64,
// []any and jsonvalue.Object.

package checkpoint

import (
	"fmt"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const (
	recordSchemaVersion uint64 = 1
	frameSchemaVersion  uint64 = 1
	maxNativeFileBytes         = 1024 * 1024 * 1024
)

// workspaceRootPath is the member path of a metadata record's workspace
// root.
var workspaceRootPath = [3]string{"payload", "record", "workspace_root"}

// MuseRecord is one parsed Muse event. Retained-frame children are flattened
// into this stream for consumers that need to inspect lifecycle or
// conversation data.
type MuseRecord struct {
	// Value is the record in jsonvalue form.
	Value         any
	LineNumber    int
	RetainedFrame bool
}

// ReadRecords parses and validates a Muse session log, flattening records
// held in retained frames. The input is never modified.
func ReadRecords(data []byte) ([]MuseRecord, error) {
	panic("unported: mj-checkpoint/src/native/muse.rs read_records")
}

// MuseRelocate relocates all ordinary Muse metadata records to targetCwd
// (Rust's muse::relocate; antigravity's keeps the bare name Relocate).
//
// Opaque retained frames, child JSON strings, and every byte outside the
// matching workspace-root JSON string are copied exactly. A child session
// stream may have no metadata; that is valid and returns the original bytes.
func MuseRelocate(data []byte, targetCwd string) ([]byte, error) {
	panic("unported: mj-checkpoint/src/native/muse.rs relocate")
}

// validateRecord checks one ordinary record's envelope and records its id in
// ids, refusing a duplicate.
func validateRecord(value any, lineNumber int, ids map[string]struct{}) error {
	panic("unported: mj-checkpoint/src/native/muse.rs validate_record")
}

// validateFrame checks a retained frame and appends each child record to
// records.
func validateFrame(value any, lineNumber int, ids map[string]struct{}, records *[]MuseRecord) error {
	panic("unported: mj-checkpoint/src/native/muse.rs validate_frame")
}

// ensureString refuses an object whose member key is not a string.
func ensureString(object jsonvalue.Object, key string, lineNumber int, kind string) error {
	if value, ok := object.Get(key); ok {
		if _, ok := value.(string); ok {
			return nil
		}
	}
	return fmt.Errorf("Muse %s at line %d has no string %s", kind, lineNumber, key)
}

// findWorkspaceRootSpans appends the byte range [start, end) of every string
// at /payload/record/workspace_root of line, without serializing the
// surrounding JSON. The scanner is deliberately small and only returns spans
// after the line has already been validated as JSON.
func findWorkspaceRootSpans(line []byte, spans *[][2]int) error {
	panic("unported: mj-checkpoint/src/native/muse.rs find_workspace_root_spans")
}

// jsonScanner walks one JSON value of bytes from position.
type jsonScanner struct {
	bytes    []byte
	position int
}

func (s *jsonScanner) skipWhitespace() {
	for s.position < len(s.bytes) && museIsASCIIWhitespace(s.bytes[s.position]) {
		s.position++
	}
}

func (s *jsonScanner) scanValue(path *[]string, spans *[][2]int) error {
	panic("unported: mj-checkpoint/src/native/muse.rs JsonScanner::scan_value")
}

func (s *jsonScanner) scanObject(path *[]string, spans *[][2]int) error {
	panic("unported: mj-checkpoint/src/native/muse.rs JsonScanner::scan_object")
}

func (s *jsonScanner) scanArray(path *[]string, spans *[][2]int) error {
	panic("unported: mj-checkpoint/src/native/muse.rs JsonScanner::scan_array")
}

func (s *jsonScanner) scanScalar() error {
	panic("unported: mj-checkpoint/src/native/muse.rs JsonScanner::scan_scalar")
}

// scanString returns the byte range of the JSON string at position,
// quotes included, after checking it decodes.
func (s *jsonScanner) scanString() ([2]int, error) {
	panic("unported: mj-checkpoint/src/native/muse.rs JsonScanner::scan_string")
}

// museIsASCIIWhitespace is Rust's u8::is_ascii_whitespace: space, tab, line
// feed, form feed and carriage return (not vertical tab).
func museIsASCIIWhitespace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\f' || b == '\r'
}
