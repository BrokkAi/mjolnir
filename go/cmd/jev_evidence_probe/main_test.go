// Port of the tests in mj-transcript/examples/jev_evidence_probe.rs.

package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func probeRequest(t *testing.T, mode string) request {
	t.Helper()
	bodies := []string{
		`{"kind":"user", "content":[{"type":"text","text":"OLD_USER"}]}`,
		`{"kind":"tool", "call":{"toolCallId":"old","title":"old tool","status":"completed"}}`,
		`{"kind":"user", "content":[{"type":"text","text":"NEW_USER"}]}`,
		`{"kind":"tool", "call":{"toolCallId":"new","title":"command","status":"completed","rawInput":{"command":"` + strings.Repeat("NOISE", 30000) + `"}}}`,
		`{"kind":"agent", "chunks":[{"content":{"type":"text","text":"DONE"}}],"streaming":false}`,
	}
	items := make([]string, len(bodies))
	for position, body := range bodies {
		items[position] = fmt.Sprintf(`{"stable_id":"item-%d","position":%d,"latest_content_event_ordinal":null,"created_at_ms":0,"last_changed_at_ms":0,"body":%s}`,
			position, position+1, body)
	}
	text := `{"snapshot":{"event_frontier":5,"event_frontier_digest":"",` +
		`"session":{"execution":{"state":"idle"},"last_activity_at_ms":0,"session_title":null,"configuration":{}},` +
		`"transcript":[` + strings.Join(items, ",") + `],"queued_prompts":[]},"mode":"` + mode + `"}`
	var parsed request
	if err := json.Unmarshal([]byte(text), &parsed); err != nil {
		t.Fatal(err)
	}
	return parsed
}

func probeProject(t *testing.T, mode string) jsonvalue.Object {
	t.Helper()
	value, err := project(probeRequest(t, mode))
	if err != nil {
		t.Fatal(err)
	}
	return value.(jsonvalue.Object)
}

func probeMember[T any](t *testing.T, object jsonvalue.Object, key string) T {
	t.Helper()
	member, ok := object.Get(key)
	if !ok {
		t.Fatalf("no %s in %v", key, object)
	}
	value, ok := member.(T)
	if !ok {
		t.Fatalf("%s is %T", key, member)
	}
	return value
}

func TestScopesBeforeBudgetingAndRemovesLargeToolBodies(t *testing.T) {
	baseline := probeProject(t, "baseline")
	if position := probeMember[uint64](t, baseline, "latest_user_position"); position != 3 {
		t.Fatalf("latest user position %d", position)
	}
	scoped := probeProject(t, "scoped")
	if strings.Contains(probeMember[string](t, scoped, "text"), "OLD_USER") {
		t.Fatal("the scoped text holds the old request")
	}
	noTools := probeProject(t, "no_tools")
	text := probeMember[string](t, noTools, "text")
	if !strings.Contains(text, "NEW_USER") || !strings.Contains(text, "DONE") ||
		strings.Contains(text, "NOISE") || strings.Contains(text, "bytes omitted") {
		t.Fatalf("no_tools text %.300s", text)
	}
	if candidates := probeMember[[]any](t, scoped, "candidates"); len(candidates) != 1 {
		t.Fatalf("scoped candidates %v", candidates)
	}
	if candidates := probeMember[[]any](t, noTools, "candidates"); len(candidates) != 0 {
		t.Fatalf("no_tools candidates %v", candidates)
	}
}
