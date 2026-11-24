// Port of the tests in mj-cli/src/api_client/events.rs.

package cli

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestEventDecoderHandlesUnicodeLargeFramesAndArbitraryChunks(t *testing.T) {
	event := core.ApiEvent{
		Seq:          3,
		SessionID:    "session-1",
		RecordedAtMs: 1,
		Event: core.ApiEventDataSessionFault{
			Reason:    core.OutcomeReasonStartupFailed,
			Message:   strings.Repeat("é", 70000),
			CommandID: nil,
		},
	}
	encoded, err := json.Marshal(event)
	check(t, err)
	frame := ": heartbeat\r\n\r\nid: 3\r\nevent: session_fault\r\ndata: " + string(encoded) + "\r\n\r\n"
	activity := event
	activity.Event = core.ApiEventDataActivityChanged{
		Activity: core.ApiActivityState{
			State:           "running",
			Details:         nil,
			IsIdle:          false,
			WaitingForInput: false,
			CapacityRetry:   false,
		},
	}
	encodedActivity, err := json.Marshal(activity)
	check(t, err)
	// Adjacent transient updates share a durable resume cursor and have no
	// SSE IDs. A later durable event still requires its own matching ID.
	frame = frame + "event: activity_changed\ndata: " + string(encodedActivity) + "\n\n" + frame
	var decoder eventDecoder
	var decoded []core.ApiEvent
	bytes := []byte(frame)
	for start := 0; start < len(bytes); start += 317 {
		end := min(start+317, len(bytes))
		events, err := decoder.push(bytes[start:end])
		check(t, err)
		decoded = append(decoded, events...)
	}
	expected := []core.ApiEvent{event, activity, event}
	if !reflect.DeepEqual(decoded, expected) {
		t.Fatalf("decoded %d events that differ from the %d sent", len(decoded), len(expected))
	}
}

func TestEventDecoderRejectsConflictingIdentityAndStreamErrors(t *testing.T) {
	body := `{"seq":2,"session_id":"s","recorded_at_ms":1,"type":"session_fault","data":{"reason":"startup_failed","message":"failed","command_id":null}}`
	if _, err := (&eventDecoder{}).push([]byte("id: 1\nevent: session_fault\ndata: " + body + "\n\n")); err == nil {
		t.Fatal("a conflicting SSE ID was accepted")
	}
	if _, err := (&eventDecoder{}).push([]byte("event: session_fault\ndata: " + body + "\n\n")); err == nil {
		t.Fatal("only transient activity may omit its SSE ID")
	}
	_, err := (&eventDecoder{}).push([]byte("event: stream_error\ndata: reconnect\n\n"))
	if err == nil || !strings.Contains(err.Error(), "reconnect") {
		t.Fatalf("unexpected error: %v", err)
	}
}

// Hard-won: c87e5e88: events reached during daemon replacement were lost instead of yielding a resume cursor.
func TestAHandoffAnnouncementKeepsEarlierEventsAndNamesTheResumeCursor(t *testing.T) {
	body := `{"seq":7,"session_id":"s","recorded_at_ms":1,"type":"session_fault","data":{"reason":"startup_failed","message":"failed","command_id":null}}`
	stream := "id: 7\nevent: session_fault\ndata: " + body + "\n\n" +
		"id: 9\nevent: daemon_handoff\ndata: the daemon is being replaced\n\n" +
		"id: 10\nevent: session_fault\ndata: not decoded\n\n"
	var decoder eventDecoder
	events, err := decoder.push([]byte(stream))
	check(t, err)
	var seqs []uint64
	for _, event := range events {
		seqs = append(seqs, event.Seq)
	}
	if !reflect.DeepEqual(seqs, []uint64{7}) {
		t.Fatalf("decoded seqs = %v", seqs)
	}
	cursor, handoff := decoder.handoff()
	if !handoff || cursor != 9 {
		t.Fatalf("handoff = %d, %t", cursor, handoff)
	}
	more, err := decoder.push([]byte("id: 11\n\n"))
	check(t, err)
	if len(more) != 0 {
		t.Fatalf("decoded after the handoff: %v", more)
	}
}
