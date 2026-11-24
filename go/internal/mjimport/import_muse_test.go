// Port of the tests in mj-import/src/import/muse.rs.

package mjimport

import (
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
)

func museTestRecord(id string, sequence uint64, payloadType string, payload any) jsonvalue.Object {
	return jsonvalue.Object{
		{Key: "schema_version", Value: int64(1)},
		{Key: "id", Value: id},
		{Key: "stream", Value: jsonvalue.Object{{Key: "kind", Value: "session"}, {Key: "id", Value: "muse-session-1"}}},
		{Key: "sequence", Value: sequence},
		{Key: "recorded_at", Value: int64(1_788_871_599_426_557) + int64(sequence)},
		{Key: "record_type", Value: "event"},
		{Key: "durability", Value: "durable"},
		{Key: "payload_type", Value: payloadType},
		{Key: "payload_schema_version", Value: int64(1)},
		{Key: "payload", Value: payload},
	}
}

func museTestSessionBytes(t *testing.T, active bool) []byte {
	t.Helper()
	run := func(event jsonvalue.Object) jsonvalue.Object {
		return jsonvalue.Object{{Key: "kind", Value: "run"}, {Key: "run_id", Value: "run-1"}, {Key: "event", Value: event}}
	}
	records := []jsonvalue.Object{
		museTestRecord("metadata", 1, "runtime.session.metadata", jsonvalue.Object{
			{Key: "kind", Value: "metadata"},
			{Key: "record", Value: jsonvalue.Object{{Key: "workspace_root", Value: "/work"}}},
		}),
		museTestRecord("intent", 2, "runtime.user_intent.accepted", jsonvalue.Object{
			{Key: "intent_id", Value: "run-1"},
			{Key: "model_messages", Value: []any{jsonvalue.Object{{Key: "content", Value: []any{
				jsonvalue.Object{{Key: "kind", Value: "text"}, {Key: "text", Value: "hello Muse"}},
			}}}}},
		}),
		museTestRecord("started", 3, "runtime.session", run(jsonvalue.Object{{Key: "kind", Value: "started"}})),
		museTestRecord("assistant", 4, "runtime.session", run(jsonvalue.Object{
			{Key: "kind", Value: "assistant_message_committed"},
			{Key: "message_id", Value: "message-1"},
			{Key: "text", Value: "hello back"},
		})),
	}
	if !active {
		records = append(records, museTestRecord("terminal", 5, "runtime.session", run(jsonvalue.Object{
			{Key: "kind", Value: "terminal"},
			{Key: "terminal", Value: "completed"},
		})))
	}
	var body strings.Builder
	for _, record := range records {
		encoded, err := jsonvalue.Marshal(record)
		if err != nil {
			t.Fatal(err)
		}
		body.Write(encoded)
		body.WriteString("\n")
	}
	return []byte(body.String())
}

// museTestWith is value with the member at path replaced, as Rust's
// `value[a][b] = ...` assigns into a Value: every object on the path is
// rebuilt, so no other record shares the change.
func museTestWith(value any, path []string, replacement any) any {
	object, _ := value.(jsonvalue.Object)
	object = object.Clone()
	if len(path) == 1 {
		object.Set(path[0], replacement)
		return object
	}
	member, _ := object.Get(path[0])
	object.Set(path[0], museTestWith(member, path[1:], replacement))
	return object
}

func TestSummaryRejectsRelativeWorkspaceAndMixedSessionStreams(t *testing.T) {
	records, err := checkpoint.ReadRecords(museTestSessionBytes(t, false))
	if err != nil {
		t.Fatal(err)
	}
	records[0].Value = museTestWith(records[0].Value, []string{"payload", "record", "workspace_root"}, "relative")
	_, err = museSummaryFromRecords("session.jsonl", records)
	if err == nil || !strings.Contains(err.Error(), "not absolute") {
		t.Fatal(err)
	}
	records[0].Value = museTestWith(records[0].Value, []string{"payload", "record", "workspace_root"}, NativeCwd("/work"))
	records[1].Value = museTestWith(records[1].Value, []string{"stream", "id"}, "another-session")
	_, err = museSummaryFromRecords("session.jsonl", records)
	if err == nil || !strings.Contains(err.Error(), "different session stream") {
		t.Fatal(err)
	}
}
