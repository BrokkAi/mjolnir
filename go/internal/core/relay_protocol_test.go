// Port of the tests in mj-core/src/relay/protocol.rs.

package core

import (
	"bytes"
	"encoding/json"
	"reflect"
	"slices"
	"strings"
	"testing"
)

func TestMemoryWirePreservesLegacyBytesAndRejectsCorruptCompression(t *testing.T) {
	legacy := `{"files":{"/MEMORY.md":"notes\n"}}`
	plain, err := UnmarshalProjectMemoryWireTreeJSON([]byte(legacy))
	if err != nil {
		t.Fatal(err)
	}
	if encoded, err := relayMarshalJSON(plain); err != nil || string(encoded) != legacy {
		t.Fatalf("legacy bytes changed: %s (%v)", encoded, err)
	}
	tree, err := ProjectMemoryWireTreeDecode(plain)
	if err != nil {
		t.Fatal(err)
	}
	compressed, err := ProjectMemoryWireTreeEncode(tree, 44)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := relayMarshalJSON(compressed)
	if err != nil {
		t.Fatal(err)
	}
	decoded, err := UnmarshalProjectMemoryWireTreeJSON(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if round, err := ProjectMemoryWireTreeDecode(decoded); err != nil || !reflect.DeepEqual(round, tree) {
		t.Fatalf("compressed tree did not round trip: %#v (%v)", round, err)
	}
	if _, err := ProjectMemoryWireTreeDecode(ProjectMemoryWireTreeGzip{Gzip: "invalid base64"}); err == nil {
		t.Fatal("invalid base64 decoded")
	}
	if _, err := ProjectMemoryWireTreeDecode(ProjectMemoryWireTreeGzip{Gzip: "e30="}); err == nil {
		t.Fatal("uncompressed bytes decoded")
	}
	gzipped, ok := compressed.(ProjectMemoryWireTreeGzip)
	if !ok {
		t.Fatal("expected gzip")
	}
	gzipped.Gzip = gzipped.Gzip[:len(gzipped.Gzip)/2]
	if _, err := ProjectMemoryWireTreeDecode(gzipped); err == nil {
		t.Fatal("truncated gzip decoded")
	}
}

// The worker writes result before payload; another encoder may not. Both
// orders decode the same, an unknown field is ignored as the derived decoder
// ignored it, and a result that contradicts the body is refused.
func TestARelayResponseDecodesInAnyFieldOrder(t *testing.T) {
	three := uint64(3)
	ok := RelayResponseEnvelope{
		RequestID:       "r",
		ProtocolVersion: 7,
		Body:            RelayResponseBodyOk{Payload: RelayResponsePayloadIdleReservation{Ordinal: &three}},
	}
	failure := UnsupportedRelayMethodResponse("r", 7, "nope")
	for _, expected := range []RelayResponseEnvelope{ok, failure} {
		written, err := relayMarshalJSON(expected)
		if err != nil {
			t.Fatal(err)
		}
		members, err := relayOrderedObject(written)
		if err != nil {
			t.Fatal(err)
		}
		var reversed bytes.Buffer
		reversed.WriteString(`{"added_later":{"x":1}`)
		for _, member := range slices.Backward(members) {
			reversed.WriteByte(',')
			relayWriteKey(&reversed, member.key)
			reversed.Write(member.value)
		}
		reversed.WriteByte('}')
		text := reversed.String()
		body := strings.Index(text, `"payload"`)
		if body < 0 {
			body = strings.Index(text, `"error"`)
		}
		if body >= strings.Index(text, `"result"`) {
			t.Fatalf("body is not before result: %s", text)
		}
		for _, text := range []string{string(written), text} {
			var decoded RelayResponseEnvelope
			if err := json.Unmarshal([]byte(text), &decoded); err != nil {
				t.Fatalf("%s: %v", text, err)
			}
			if !reflect.DeepEqual(decoded, expected) {
				t.Fatalf("%s decoded as %#v", text, decoded)
			}
		}
	}
	contradiction := `{"request_id":"r","protocol_version":7,"result":"error","payload":{"type":"idle_reservation","data":{"ordinal":null}}}`
	var decoded RelayResponseEnvelope
	if err := json.Unmarshal([]byte(contradiction), &decoded); err == nil {
		t.Fatal("a contradicting result decoded")
	}
}
