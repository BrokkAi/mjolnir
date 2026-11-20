// Port of the tests in mj-core/src/relay/snapshot.rs.

package core

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

// recordedBeforeTheFlag is a session_opened record written before the flag
// existed, copied from a live journal. Its digest covers the encoding without
// the field, so the field must stay absent when it is false or every existing
// journal fails validation on the next worker start.
const recordedBeforeTheFlag = `{"format":2,"ordinal":493425,"digest":"28c6574464b1361ad00dedd7cb04dac0c617948e290936082f1fd354334b262e","recorded_at_ms":1789421283703,"observation":{"type":"session_opened","data":{"native_session_id":"fe6031fb-9af5-49ca-a587-5a816de61f86","resumed":false}}}`

// recordedByTheFlaggingBuild was written by the build that emitted the flag
// as false, copied from a live journal. Its digest covers that encoding.
const recordedByTheFlaggingBuild = `{"format":2,"ordinal":188,"digest":"83a8ded900c35360e8998e6b7710902475145370bdaa88ce1fa186cb5d108636","recorded_at_ms":1789436362441,"observation":{"type":"session_opened","data":{"native_session_id":"020bb831-ec40-49ac-bbcb-a702135deb5d","resumed":true,"native_continuity_lost":false}}}`

func decodeRelayEvent(t *testing.T, text string) RelayEvent {
	t.Helper()
	var event RelayEvent
	if err := json.Unmarshal([]byte(text), &event); err != nil {
		t.Fatal(err)
	}
	return event
}

// Hard-won: 370520bc: Adding a false field changed live journal digests and made workers refuse old journals.
func TestASessionOpenedRecordWithoutTheFlagStillVerifies(t *testing.T) {
	event := decodeRelayEvent(t, recordedBeforeTheFlag)
	if err := ValidateRelayEventSelf(&event); err != nil {
		t.Fatalf("the stored digest must still match: %v", err)
	}
	encoded, err := relayMarshalJSON(event.Observation)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "native_continuity_lost") {
		t.Fatalf("false must not be written: %s", encoded)
	}
}

// Hard-won: 131fa849: Three live journals contained digests over the intermediate false-field encoding.
func TestARecordThatWroteTheFlagAsFalseStillVerifies(t *testing.T) {
	event := decodeRelayEvent(t, recordedByTheFlaggingBuild)
	if err := ValidateRelayEventSelf(&event); err != nil {
		t.Fatalf("the legacy encoding must still verify: %v", err)
	}
	tampered := event
	tampered.RecordedAtMs++
	if ValidateRelayEventSelf(&tampered) == nil {
		t.Fatal("a tampered record verified")
	}
}

// The replaced session is written only when there is one, so every record
// written before the field existed keeps its digest.
// Hard-won: 2f8ec62e: Never-prompted Claude and Codex sessions could not resume after the harness opened a fresh native session.
func TestAReplacedUnusedSessionIsWrittenOnlyWhenThereIsOne(t *testing.T) {
	neverPrompted := "never-prompted"
	replacing := RelayObservationSessionOpened{
		NativeSessionID:               "fresh",
		ReplacedUnusedNativeSessionID: &neverPrompted,
	}
	encoded, err := relayMarshalJSON(replacing)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(encoded), `"replaced_unused_native_session_id":"never-prompted"`) {
		t.Fatal(string(encoded))
	}
	decoded, err := UnmarshalRelayObservationJSON(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(decoded, RelayObservation(replacing)) {
		t.Fatalf("decoded %#v", decoded)
	}
	event := decodeRelayEvent(t, recordedBeforeTheFlag)
	if encoded, _ := relayMarshalJSON(event.Observation); strings.Contains(string(encoded), "replaced_unused_native_session_id") {
		t.Fatal(string(encoded))
	}
}
