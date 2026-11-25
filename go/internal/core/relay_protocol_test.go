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

// Hard-won: 390e88f94: the Rust daemon refused the Go worker's
// command_retention_state response (null where serde wants a set), so
// every session stayed "launching". encoding/json writes a nil slice or
// map as null and serde refuses null for a Vec, set or map, so each type the
// worker writes to the relay, at any depth, must write an always-present
// collection as [] or {}.
func TestRelayWireWritesEmptyCollectionsAsSerdeDoes(t *testing.T) {
	registries := []any{
		relayResponseBodyVariants, relayResponsePayloadVariants, relayErrorDetailVariants,
		relayObservationVariants, relayCommandOutcomeVariants, relayCommandVariants,
		workerEventVariants, harnessPreparationVariants, reviewerRequestVariants,
		projectMemoryTransferActionVariants, projectMemoryUploadKindVariants, projectMemoryTransferResultVariants,
	}
	// The variants of each sealed interface, so the walk follows a field
	// that holds one.
	variants := map[reflect.Type][]reflect.Type{}
	for _, registry := range registries {
		decoders := reflect.ValueOf(registry).MapRange()
		for decoders.Next() {
			results := decoders.Value().Call([]reflect.Value{reflect.ValueOf(json.RawMessage("null"))})
			if !results[1].IsNil() {
				t.Fatalf("variant %v: %v", decoders.Key(), results[1])
			}
			sealed := decoders.Value().Type().Out(0)
			variants[sealed] = append(variants[sealed], results[0].Elem().Type())
		}
	}
	pending := []reflect.Type{
		reflect.TypeFor[RelayResponseEnvelope](), reflect.TypeFor[RelayEvent](),
		reflect.TypeFor[RelaySnapshot](), reflect.TypeFor[RelayOperationalState](),
		reflect.TypeFor[LiveSessionBaseline](),
	}
	for _, types := range variants {
		pending = append(pending, types...)
	}
	seen := map[reflect.Type]bool{}
	checked := 0
	for len(pending) > 0 {
		typ := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		for typ.Kind() == reflect.Pointer || typ.Kind() == reflect.Slice || typ.Kind() == reflect.Array || typ.Kind() == reflect.Map {
			if typ.Kind() == reflect.Map {
				pending = append(pending, typ.Key())
			}
			typ = typ.Elem()
		}
		if seen[typ] {
			continue
		}
		seen[typ] = true
		if typ.Kind() == reflect.Interface {
			pending = append(pending, variants[typ]...)
			continue
		}
		if typ.Kind() != reflect.Struct {
			continue
		}
		var collections []string
		for i := range typ.NumField() {
			field := typ.Field(i)
			pending = append(pending, field.Type)
			name, options, _ := strings.Cut(field.Tag.Get("json"), ",")
			kind := field.Type.Kind()
			if !field.IsExported() || name == "-" || serdeTagHasOption(options, "omitempty") ||
				serdeTagHasOption(options, "omitzero") ||
				(kind != reflect.Map && (kind != reflect.Slice || field.Type.Elem().Kind() == reflect.Uint8)) {
				continue
			}
			if name == "" {
				name = field.Name
			}
			collections = append(collections, name)
		}
		if len(collections) == 0 {
			continue
		}
		written, err := relayMarshalJSON(reflect.New(typ).Elem().Interface())
		if err != nil {
			// A type that cannot encode its zero value would otherwise
			// drop out of this check unnoticed.
			t.Errorf("%v cannot encode its zero value, so its collections go unchecked: %v", typ, err)
			continue
		}
		checked++
		for _, name := range collections {
			if bytes.Contains(written, []byte(`"`+name+`":null`)) {
				t.Errorf("%v writes an empty %s as null: %s", typ, name, written)
			}
		}
	}
	if checked == 0 {
		t.Fatal("no relay type holds a collection")
	}
}
