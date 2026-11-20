// Port of the tests in mj-checkpoint/src/native/muse.rs.

package checkpoint

import (
	"bytes"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func museTestRecord(t *testing.T, sequence int64, payloadType string, payload any) string {
	t.Helper()
	encoded, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "schema_version", Value: int64(1)},
		{Key: "id", Value: fmt.Sprintf("record-%d", sequence)},
		{Key: "stream", Value: jsonvalue.Object{{Key: "kind", Value: "session"}, {Key: "id", Value: "session-1"}}},
		{Key: "sequence", Value: sequence},
		{Key: "recorded_at", Value: int64(1_788_871_599_426_557) + sequence},
		{Key: "record_type", Value: "event"},
		{Key: "durability", Value: "durable"},
		{Key: "payload_type", Value: payloadType},
		{Key: "payload_schema_version", Value: int64(1)},
		{Key: "payload", Value: payload},
	})
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

// museTestJSON parses a JSON literal for a test payload.
func museTestJSON(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatalf("%s: %v", text, err)
	}
	return value
}

func TestRelocationPreservesChildMetadataWithInheritedWorkspace(t *testing.T) {
	source := museTestRecord(t, 1, "runtime.session.metadata",
		museTestJSON(t, `{"kind":"metadata","record":{"provider_id":"muse","model_id":"test"}}`))
	relocated, err := MuseRelocate([]byte(source), "/new")
	if err != nil {
		t.Fatal(err)
	}
	if string(relocated) != source {
		t.Fatalf("relocated:\n%s\nsource:\n%s", relocated, source)
	}
}

func TestRelocationPreservesOpaqueFrameAndRewritesRepeatedMetadata(t *testing.T) {
	metadata := museTestRecord(t, 3, "runtime.session.metadata",
		museTestJSON(t, `{"kind":"metadata","record":{"workspace_root":"/old","other":"keep"}}`))
	secondMetadata := museTestRecord(t, 4, "runtime.session.metadata",
		museTestJSON(t, `{"kind":"metadata","record":{"workspace_root":"/old-again"}}`))
	child := museTestRecord(t, 1, "runtime.session.permission_format_declared",
		museTestJSON(t, `{"format":"profile_v1"}`))
	frameBytes, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "retained_frame", Value: "session_permission_transaction"},
		{Key: "frame_schema_version", Value: int64(1)},
		{Key: "outer_log_ordinal", Value: int64(1)},
		{Key: "transaction_id", Value: "tx-1"},
		{Key: "children", Value: []any{jsonvalue.Object{
			{Key: "child_index", Value: int64(0)},
			{Key: "record_json", Value: child},
		}}},
		{Key: "content_sha256", Value: "sha256:opaque"},
	})
	if err != nil {
		t.Fatal(err)
	}
	frame := string(frameBytes)
	source := []byte(frame + "\n" + metadata + "\r\n" + secondMetadata + "\n")
	relocated, err := MuseRelocate(source, "/new/workspace")
	if err != nil {
		t.Fatal(err)
	}
	text := string(relocated)
	if !strings.Contains(text, `"workspace_root":"/new/workspace"`) {
		t.Fatal(text)
	}
	if !strings.HasPrefix(text, frame+"\n") {
		t.Fatal(text)
	}
	if !strings.HasSuffix(text, "\n") {
		t.Fatal(text)
	}
	records, err := ReadRecords(relocated)
	if err != nil {
		t.Fatal(err)
	}
	if len(records) != 3 {
		t.Fatalf("%d records", len(records))
	}
}

func TestRelocationHandlesLargeStreamAndRejectsUnknownEnvelope(t *testing.T) {
	var source strings.Builder
	for sequence := int64(1); sequence <= 2_000; sequence++ {
		source.WriteString(museTestRecord(t, sequence, "runtime.session", jsonvalue.Object{
			{Key: "kind", Value: "padding"},
			{Key: "text", Value: strings.Repeat("x", 64)},
		}))
		source.WriteByte('\n')
	}
	if source.Len() <= 64*1024 {
		t.Fatalf("the stream is only %d bytes", source.Len())
	}
	relocated, err := MuseRelocate([]byte(source.String()), "/new")
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(relocated, []byte(source.String())) {
		t.Fatal("a stream without workspace metadata changed")
	}
	_, err = MuseRelocate([]byte("{\"not\":\"a Muse record\"}\n"), "/new")
	if err == nil || !strings.Contains(err.Error(), "schema_version") {
		t.Fatalf("%v", err)
	}
}

func TestInterpretedPayloadsRejectUnsupportedSchemaVersions(t *testing.T) {
	withVersion := func(record string, version int64) []byte {
		value, err := jsonvalue.Parse([]byte(record))
		if err != nil {
			t.Fatal(err)
		}
		object := value.(jsonvalue.Object)
		object.Set("payload_schema_version", version)
		encoded, err := jsonvalue.Marshal(object)
		if err != nil {
			t.Fatal(err)
		}
		return append(encoded, '\n')
	}

	metadata := museTestRecord(t, 1, "runtime.session.metadata", jsonvalue.Object{
		{Key: "kind", Value: "metadata"},
		{Key: "record", Value: jsonvalue.Object{{Key: "workspace_root", Value: "/old"}}},
	})
	_, err := ReadRecords(withVersion(metadata, 2))
	if err == nil || !strings.Contains(err.Error(), "unsupported Muse payload schema version 2") {
		t.Fatalf("%v", err)
	}

	runtime := museTestRecord(t, 1, "runtime.session", museTestJSON(t, `{"kind": "run", "event": {"kind": "started"}}`))
	_, err = ReadRecords(withVersion(runtime, 3))
	if err == nil || !strings.Contains(err.Error(), "unsupported Muse payload schema version 3") {
		t.Fatalf("%v", err)
	}

	if _, err := ReadRecords(withVersion(runtime, 2)); err != nil {
		t.Fatal(err)
	}
}
