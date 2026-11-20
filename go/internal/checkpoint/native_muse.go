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
	"bytes"
	"errors"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"unicode/utf8"

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
	if len(data) > maxNativeFileBytes {
		return nil, fmt.Errorf("Muse session log is too large (%d bytes; maximum is %d bytes)", len(data), maxNativeFileBytes)
	}
	records := []MuseRecord{}
	ids := map[string]struct{}{}
	sawValue := false

	for index, segment := range museSplitInclusive(data) {
		lineNumber := index + 1
		line := bytes.TrimSuffix(segment, []byte("\n"))
		line = bytes.TrimSuffix(line, []byte("\r"))
		if museAllASCIIWhitespace(line) {
			continue
		}
		sawValue = true
		value, err := jsonvalue.Parse(line)
		if err != nil {
			return nil, fmt.Errorf("parse Muse session record at line %d: %w", lineNumber, err)
		}
		if _, ok := museGet(value, "retained_frame"); ok {
			if err := validateFrame(value, lineNumber, ids, &records); err != nil {
				return nil, err
			}
		} else {
			if err := validateRecord(value, lineNumber, ids); err != nil {
				return nil, err
			}
			records = append(records, MuseRecord{Value: value, LineNumber: lineNumber})
		}
	}

	if !sawValue {
		return nil, errors.New("Muse session log is empty")
	}
	return records, nil
}

// museSplitInclusive splits data after each "\n", keeping it, as Rust's
// slice::split_inclusive does: no empty segment after a final newline, and
// none at all for empty data.
func museSplitInclusive(data []byte) [][]byte {
	segments := [][]byte{}
	for len(data) > 0 {
		end := bytes.IndexByte(data, '\n') + 1
		if end == 0 {
			end = len(data)
		}
		segments = append(segments, data[:end])
		data = data[end:]
	}
	return segments
}

// museAllASCIIWhitespace reports whether every byte of line is ASCII
// whitespace; true for an empty line.
func museAllASCIIWhitespace(line []byte) bool {
	for _, b := range line {
		if !museIsASCIIWhitespace(b) {
			return false
		}
	}
	return true
}

// museGet is serde_json's Value::get with a key: the member of an object,
// and nothing for any other value.
func museGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// museMember is the member key of object, if it has one of type T.
func museMember[T any](object jsonvalue.Object, key string) (T, bool) {
	var zero T
	value, ok := object.Get(key)
	if !ok {
		return zero, false
	}
	typed, ok := value.(T)
	return typed, ok
}

// museAsU64 is serde_json's Value::as_u64.
func museAsU64(value any, present bool) (uint64, bool) {
	if !present {
		return 0, false
	}
	switch number := value.(type) {
	case uint64:
		return number, true
	case int64:
		if number >= 0 {
			return uint64(number), true
		}
	}
	return 0, false
}

// museAsI64 is serde_json's Value::as_i64. jsonvalue holds every integer
// that fits i64 as an int64.
func museAsI64(value any, present bool) (int64, bool) {
	if !present {
		return 0, false
	}
	number, ok := value.(int64)
	return number, ok
}

// MuseRelocate relocates all ordinary Muse metadata records to targetCwd
// (Rust's muse::relocate; antigravity's keeps the bare name Relocate).
//
// Opaque retained frames, child JSON strings, and every byte outside the
// matching workspace-root JSON string are copied exactly. A child session
// stream may have no metadata; that is valid and returns the original bytes.
func MuseRelocate(data []byte, targetCwd string) ([]byte, error) {
	if !filepath.IsAbs(targetCwd) {
		return nil, errors.New("Muse target cwd is not absolute")
	}
	if !utf8.ValidString(targetCwd) {
		return nil, errors.New("Muse target cwd is not valid UTF-8")
	}
	targetJSON, err := jsonvalue.Marshal(targetCwd)
	if err != nil {
		return nil, fmt.Errorf("encode Muse target cwd: %w", err)
	}

	// Validate the complete stream first. This prevents a malformed suffix
	// from being silently copied after a valid metadata prefix.
	if _, err := ReadRecords(data); err != nil {
		return nil, err
	}

	output := make([]byte, 0, len(data))
	lineNumber := 0
	for _, segment := range museSplitInclusive(data) {
		line := segment
		var newline, carriageReturn []byte
		if bytes.HasSuffix(line, []byte("\n")) {
			line = line[:len(line)-1]
			newline = []byte("\n")
		}
		if bytes.HasSuffix(line, []byte("\r")) {
			line = line[:len(line)-1]
			carriageReturn = []byte("\r")
		}
		lineNumber++
		if museAllASCIIWhitespace(line) {
			output = append(output, segment...)
			continue
		}

		value, err := jsonvalue.Parse(line)
		if err != nil {
			return nil, fmt.Errorf("parse Muse session record at line %d: %w", lineNumber, err)
		}
		_, framed := museGet(value, "retained_frame")
		payloadType, _ := museGet(value, "payload_type")
		if framed || payloadType != "runtime.session.metadata" {
			output = append(output, segment...)
			continue
		}

		spans := [][2]int{}
		if err := findWorkspaceRootSpans(line, &spans); err != nil {
			return nil, fmt.Errorf("find Muse workspace metadata at line %d: %w", lineNumber, err)
		}
		if len(spans) == 0 {
			output = append(output, segment...)
			continue
		}
		cursor := 0
		for _, span := range spans {
			output = append(output, line[cursor:span[0]]...)
			output = append(output, targetJSON...)
			cursor = span[1]
		}
		output = append(output, line[cursor:]...)
		output = append(output, carriageReturn...)
		output = append(output, newline...)
	}
	return output, nil
}

// validateRecord checks one ordinary record's envelope and records its id in
// ids, refusing a duplicate.
func validateRecord(value any, lineNumber int, ids map[string]struct{}) error {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return fmt.Errorf("unsupported Muse record at line %d: expected an object", lineNumber)
	}
	switch version, ok := museAsU64(object.Get("schema_version")); {
	case !ok:
		return fmt.Errorf("Muse record at line %d has no schema_version", lineNumber)
	case version != recordSchemaVersion:
		return fmt.Errorf("unsupported Muse record schema version %d at line %d", version, lineNumber)
	}
	if err := ensureString(object, "id", lineNumber, "record"); err != nil {
		return err
	}
	id, _ := museMember[string](object, "id")
	if _, duplicate := ids[id]; duplicate {
		return fmt.Errorf("duplicate Muse record id %s at line %d", nativeDebugString(id), lineNumber)
	}
	ids[id] = struct{}{}
	stream, ok := museMember[jsonvalue.Object](object, "stream")
	if !ok {
		return fmt.Errorf("Muse record at line %d has no stream object", lineNumber)
	}
	if kind, _ := museMember[string](stream, "kind"); kind != "session" {
		return fmt.Errorf("unsupported Muse stream kind at line %d", lineNumber)
	}
	if err := ensureString(stream, "id", lineNumber, "stream"); err != nil {
		return err
	}
	if _, ok := museAsU64(object.Get("sequence")); !ok {
		return fmt.Errorf("Muse record at line %d has no sequence", lineNumber)
	}
	if _, ok := museAsI64(object.Get("recorded_at")); !ok {
		return fmt.Errorf("Muse record at line %d has no integer recorded_at", lineNumber)
	}
	for _, key := range []string{"record_type", "durability", "payload_type"} {
		if err := ensureString(object, key, lineNumber, "record"); err != nil {
			return err
		}
	}
	payloadSchemaVersion, ok := museAsU64(object.Get("payload_schema_version"))
	if !ok {
		return fmt.Errorf("Muse record at line %d has no payload_schema_version", lineNumber)
	}
	if _, ok := object.Get("payload"); !ok {
		return fmt.Errorf("Muse record at line %d has no payload", lineNumber)
	}
	payloadType, _ := museMember[string](object, "payload_type")
	var supported bool
	switch payloadType {
	case "runtime.session":
		// Muse has added fields to the generic runtime-session payload while
		// retaining the v1 run-event shape used by the import projection.
		supported = payloadSchemaVersion == 1 || payloadSchemaVersion == 2
	case "runtime.session.metadata",
		"runtime.user_intent.accepted",
		"runtime.user_intent.materialized",
		"session.name.changed":
		supported = payloadSchemaVersion == recordSchemaVersion
	default:
		// Unknown payloads remain opaque. Their bytes are retained in the
		// native artifact and must not make an otherwise readable session
		// unusable merely because their schema evolves independently.
		supported = true
	}
	if !supported {
		return fmt.Errorf("unsupported Muse payload schema version %d for Some(%s) at line %d",
			payloadSchemaVersion, nativeDebugString(payloadType), lineNumber)
	}
	// Child-agent metadata only declares its model/provider and inherits the
	// parent workspace. The top-level importer requires an explicit root.
	if payloadType == "runtime.session.metadata" {
		if workspace, ok := musePointer(value, workspaceRootPath[:]); ok {
			workspaceRoot, ok := workspace.(string)
			if !ok {
				return fmt.Errorf("Muse metadata record at line %d has no string payload.record.workspace_root", lineNumber)
			}
			if strings.TrimSpace(workspaceRoot) == "" {
				return fmt.Errorf("Muse metadata record at line %d has an empty workspace_root", lineNumber)
			}
		}
	}
	return nil
}

// musePointer is serde_json's Value::pointer for a path of object keys.
func musePointer(value any, path []string) (any, bool) {
	for _, key := range path {
		next, ok := museGet(value, key)
		if !ok {
			return nil, false
		}
		value = next
	}
	return value, true
}

// validateFrame checks a retained frame and appends each child record to
// records.
func validateFrame(value any, lineNumber int, ids map[string]struct{}, records *[]MuseRecord) error {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return fmt.Errorf("unsupported Muse retained frame at line %d: expected an object", lineNumber)
	}
	frameName, ok := museMember[string](object, "retained_frame")
	if !ok {
		return fmt.Errorf("Muse retained frame at line %d has no name", lineNumber)
	}
	if frameName == "" {
		return fmt.Errorf("Muse retained frame at line %d has an empty name", lineNumber)
	}
	switch version, ok := museAsU64(object.Get("frame_schema_version")); {
	case !ok:
		return fmt.Errorf("Muse retained frame at line %d has no schema version", lineNumber)
	case version != frameSchemaVersion:
		return fmt.Errorf("unsupported Muse retained-frame schema version %d at line %d", version, lineNumber)
	}
	children, ok := museMember[[]any](object, "children")
	if !ok {
		return fmt.Errorf("Muse retained frame at line %d has no children", lineNumber)
	}
	for childIndex, child := range children {
		childObject, ok := child.(jsonvalue.Object)
		if !ok {
			return fmt.Errorf("Muse retained frame at line %d child %d is not an object", lineNumber, childIndex)
		}
		if _, ok := museAsU64(childObject.Get("child_index")); !ok {
			return fmt.Errorf("Muse retained frame at line %d child %d has no child_index", lineNumber, childIndex)
		}
		recordJSON, ok := museMember[string](childObject, "record_json")
		if !ok {
			return fmt.Errorf("Muse retained frame at line %d child %d has no record_json", lineNumber, childIndex)
		}
		record, err := jsonvalue.Parse([]byte(recordJSON))
		if err != nil {
			return fmt.Errorf("parse Muse retained frame at line %d child %d record: %w", lineNumber, childIndex, err)
		}
		if err := validateRecord(record, lineNumber, ids); err != nil {
			return fmt.Errorf("validate Muse retained frame at line %d child %d record: %w", lineNumber, childIndex, err)
		}
		*records = append(*records, MuseRecord{Value: record, LineNumber: lineNumber, RetainedFrame: true})
	}
	return nil
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
	scanner := jsonScanner{bytes: line}
	path := []string{}
	if err := scanner.scanValue(&path, spans); err != nil {
		return err
	}
	scanner.skipWhitespace()
	if scanner.position != len(line) {
		return errors.New("Muse metadata line has trailing JSON data")
	}
	return nil
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

// at is the byte at position, if there is one.
func (s *jsonScanner) at() (byte, bool) {
	if s.position < len(s.bytes) {
		return s.bytes[s.position], true
	}
	return 0, false
}

func (s *jsonScanner) scanValue(path *[]string, spans *[][2]int) error {
	s.skipWhitespace()
	b, ok := s.at()
	switch {
	case !ok:
		return errors.New("unexpected end of Muse metadata JSON")
	case b == '{':
		return s.scanObject(path, spans)
	case b == '[':
		return s.scanArray(path, spans)
	case b == '"':
		span, err := s.scanString()
		if err != nil {
			return err
		}
		if slices.Equal(*path, workspaceRootPath[:]) {
			*spans = append(*spans, span)
		}
		return nil
	}
	return s.scanScalar()
}

func (s *jsonScanner) scanObject(path *[]string, spans *[][2]int) error {
	s.position++
	s.skipWhitespace()
	if b, ok := s.at(); ok && b == '}' {
		s.position++
		return nil
	}
	for {
		s.skipWhitespace()
		keySpan, err := s.scanString()
		if err != nil {
			return err
		}
		key, err := museDecodeString(s.bytes[keySpan[0]:keySpan[1]])
		if err != nil {
			return fmt.Errorf("decode Muse JSON key: %w", err)
		}
		s.skipWhitespace()
		if b, ok := s.at(); !ok || b != ':' {
			return errors.New("Muse metadata object is missing a colon")
		}
		s.position++
		*path = append(*path, key)
		if err := s.scanValue(path, spans); err != nil {
			return err
		}
		*path = (*path)[:len(*path)-1]
		s.skipWhitespace()
		switch b, _ := s.at(); b {
		case ',':
			s.position++
		case '}':
			s.position++
			return nil
		default:
			return errors.New("Muse metadata object is missing a comma or closing brace")
		}
	}
}

func (s *jsonScanner) scanArray(path *[]string, spans *[][2]int) error {
	s.position++
	s.skipWhitespace()
	if b, ok := s.at(); ok && b == ']' {
		s.position++
		return nil
	}
	for {
		if err := s.scanValue(path, spans); err != nil {
			return err
		}
		s.skipWhitespace()
		switch b, _ := s.at(); b {
		case ',':
			s.position++
		case ']':
			s.position++
			return nil
		default:
			return errors.New("Muse metadata array is missing a comma or closing bracket")
		}
	}
}

func (s *jsonScanner) scanScalar() error {
	start := s.position
	for s.position < len(s.bytes) {
		b := s.bytes[s.position]
		if b == ',' || b == '}' || b == ']' || museIsASCIIWhitespace(b) {
			break
		}
		s.position++
	}
	if s.position <= start {
		return errors.New("Muse metadata contains an empty JSON value")
	}
	return nil
}

// scanString returns the byte range of the JSON string at position,
// quotes included, after checking it decodes.
func (s *jsonScanner) scanString() ([2]int, error) {
	start := s.position
	if b, ok := s.at(); !ok || b != '"' {
		return [2]int{}, errors.New("Muse metadata expected a JSON string")
	}
	s.position++
	for s.position < len(s.bytes) {
		switch s.bytes[s.position] {
		case '\\':
			s.position++
			if s.position >= len(s.bytes) {
				return [2]int{}, errors.New("Muse metadata string ends after an escape")
			}
			s.position++
		case '"':
			s.position++
			span := [2]int{start, s.position}
			if _, err := museDecodeString(s.bytes[span[0]:span[1]]); err != nil {
				return [2]int{}, fmt.Errorf("decode Muse metadata string: %w", err)
			}
			return span, nil
		default:
			s.position++
		}
	}
	return [2]int{}, errors.New("Muse metadata string is unterminated")
}

// museDecodeString decodes one JSON string as serde_json reads a String.
func museDecodeString(text []byte) (string, error) {
	value, err := jsonvalue.Parse(text)
	if err != nil {
		return "", err
	}
	decoded, ok := value.(string)
	if !ok {
		return "", errors.New("invalid type: expected a string")
	}
	return decoded, nil
}

// museIsASCIIWhitespace is Rust's u8::is_ascii_whitespace: space, tab, line
// feed, form feed and carriage return (not vertical tab).
func museIsASCIIWhitespace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\f' || b == '\r'
}
