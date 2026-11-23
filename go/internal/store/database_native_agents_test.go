// Port of the tests in mj-store/src/database/native_agents.rs.

package store

import (
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func nativeTestEvent(t *testing.T, ordinal uint64, native core.NativeAgentEvent) *core.RelayEvent {
	t.Helper()
	event := &core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        ordinal,
		PreviousDigest: core.RelayEventGenesisDigest,
		RecordedAtMs:   int64(ordinal) * 100,
		Observation:    core.RelayObservationNativeAgent{Event: native},
	}
	digest, err := core.RelayEventDigest(event)
	if err != nil {
		t.Fatal(err)
	}
	event.Digest = digest
	return event
}

func nativeTestSpawn(child string, parent *string) core.NativeAgentEvent {
	return core.NativeAgentEventSpawned{
		SessionID:       child,
		ParentSessionID: parent,
		Name:            child,
		Task:            "Inspect code",
		Capabilities:    core.NativeAgentCapabilities{},
	}
}

// nativeTestUpdate decodes an ACP session update from its JSON form.
func nativeTestUpdate(t *testing.T, child string, update string) core.NativeAgentEvent {
	t.Helper()
	var decoded acpschema.SessionUpdate
	if err := json.Unmarshal([]byte(update), &decoded); err != nil {
		t.Fatal(err)
	}
	return core.NativeAgentEventUpdate{SessionID: child, Update: decoded}
}

func nativeTestText(t *testing.T, child string, message string) core.NativeAgentEvent {
	t.Helper()
	text, err := json.Marshal(message)
	if err != nil {
		t.Fatal(err)
	}
	return nativeTestUpdate(t, child, `{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":`+string(text)+`}}`)
}

// storeMust is the tests' unwrap: a failure panics, failing the test.
func storeMust[T any](value T, err error) T {
	if err != nil {
		panic(err)
	}
	return value
}

func TestLoadToolCallDetailResolvesNativeItemsByOwnerAndCommittedGeneration(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	for _, owner := range []string{"owner", "other"} {
		record := TestSession(owner, "project")
		if err := SaveSessionTo(path, &record); err != nil {
			t.Fatal(err)
		}
	}
	connection := storeMust(open(path))
	defer connection.Close()
	apply := func(owner string, event *core.RelayEvent) {
		t.Helper()
		if err := applyNativeAgentEvent(connection, owner, event); err != nil {
			t.Fatal(err)
		}
	}
	apply("owner", nativeTestEvent(t, 1, nativeTestSpawn("child", nil)))
	apply("other", nativeTestEvent(t, 1, nativeTestSpawn("child", nil)))
	tool := nativeTestUpdate(t, "child", `{"sessionUpdate":"tool_call","toolCallId":"edit","title":"Edit","status":"completed","rawInput":{"path":"notes.txt"}}`)
	apply("owner", nativeTestEvent(t, 2, tool))
	history := storeMust(nativeAgentHistoryFrom(connection, "owner", "child", nil))
	if len(history.Items) != 1 {
		t.Fatalf("expected one tool, got %d", len(history.Items))
	}
	item := history.Items[0]
	lookup := func(owner string) core.ToolCallDetailResult {
		t.Helper()
		return storeMust(loadNativeAgentToolCallDetailFrom(connection, owner, "child", item.StableID))
	}
	if got, want := lookup("owner"), core.ToolCallDetailResult(core.ToolCallDetailResultFull{Body: item.Body}); !reflect.DeepEqual(got, want) {
		t.Fatalf("owner detail = %#v, want %#v", got, want)
	}
	if got := lookup("other"); got != nil {
		t.Fatalf("other detail = %#v, want none", got)
	}
	apply("other", nativeTestEvent(t, 3, core.NativeAgentEventReplayBegin{}))
	apply("other", nativeTestEvent(t, 4, nativeTestSpawn("child", nil)))
	apply("other", nativeTestEvent(t, 5, tool))
	if got := lookup("other"); got != nil {
		t.Fatalf("staged detail = %#v, want none", got)
	}
	apply("other", nativeTestEvent(t, 6, core.NativeAgentEventReplayCommit{}))
	if got, want := lookup("other"), lookup("owner"); !reflect.DeepEqual(got, want) {
		t.Fatalf("committed detail = %#v, want %#v", got, want)
	}
}

func TestCommittedNativeAgentsKeepReplayPrivateAndPublishCascadingDeletion(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	owner := session("owner", "project")
	if err := SaveSessionTo(path, &owner); err != nil {
		t.Fatal(err)
	}
	writer := storeMust(startDatabaseWriterAt(path, false))
	execute := func(label string, operation func(*Connection) error) {
		t.Helper()
		if _, err := DatabaseWriterExecute(writer.writer, label, func(connection *Connection) (struct{}, error) {
			return struct{}{}, operation(connection)
		}); err != nil {
			t.Fatal(err)
		}
	}
	execute("spawn native child", func(connection *Connection) error {
		return applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 1, nativeTestSpawn("child", nil)))
	})
	// Hard-won: bf2b4a05f: activity after loading a legacy request moved its raise time.
	execute("load legacy pending native request", func(connection *Connection) error {
		view, err := loadNativeAgentViewFrom(connection, "owner", "child", 0)
		if err != nil {
			return err
		}
		if view == nil {
			return errors.New("native child was not materialized")
		}
		view.Projection.PendingElicitations = append(view.Projection.PendingElicitations, core.ElicitationRequest{
			ID:      "approval",
			Message: "Approve?",
			Fields:  []core.ElicitationField{},
		})
		if err := save(connection, view, 0); err != nil {
			return err
		}
		return applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 2, nativeTestText(t, "child", "Still working")))
	})
	held := storeMust(writer.writer.CommittedState())
	raised := held.NativeAgents.Index("owner").Index("child").PendingInputRaisedAtMs
	if raised == nil || *raised != 100 {
		t.Fatalf("pending input raised at %v, want 100", raised)
	}
	if err := writer.Shutdown(); err != nil {
		t.Fatal(err)
	}
	writer = storeMust(startDatabaseWriterAt(path, false))
	defer writer.Shutdown()
	reopened := storeMust(writer.writer.CommittedState())
	if !reflect.DeepEqual(reopened.NativeAgents, held.NativeAgents) {
		t.Fatalf("reopened native agents = %#v, want %#v", reopened.NativeAgents, held.NativeAgents)
	}
	execute("begin native replay", func(connection *Connection) error {
		return applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 3, core.NativeAgentEventReplayBegin{}))
	})
	before := storeMust(writer.writer.CommittedState())
	if state := before.NativeAgents.Index("owner").Index("child").Agent.State; state != core.NativeAgentStateDisconnected {
		t.Fatalf("replaying child state = %v, want disconnected", state)
	}
	execute("stage native child", func(connection *Connection) error {
		return applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 4, nativeTestSpawn("replacement", nil)))
	})
	staged := storeMust(writer.writer.CommittedState())
	if staged.Sequence != before.Sequence {
		t.Fatalf("staging published sequence %d, want %d", staged.Sequence, before.Sequence)
	}
	if !reflect.DeepEqual(staged.NativeAgents, before.NativeAgents) {
		t.Fatal("staging published native agents")
	}
	execute("publish native replay", func(connection *Connection) error {
		return applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 5, core.NativeAgentEventReplayCommit{}))
	})
	published := storeMust(writer.writer.CommittedState())
	if !published.NativeAgents.Index("owner").ContainsKey("child") || !published.NativeAgents.Index("owner").ContainsKey("replacement") {
		t.Fatal("published replay lost a child")
	}
	execute("delete native owner", func(connection *Connection) error {
		_, err := connection.Exec("DELETE FROM sessions WHERE session_id='owner'")
		return err
	})
	if !storeMust(writer.writer.CommittedState()).NativeAgents.IsEmpty() {
		t.Fatal("deleting the owner kept its native agents")
	}
	if !held.NativeAgents.Index("owner").ContainsKey("child") {
		t.Fatal("an earlier publication changed")
	}
}

func TestCompletedChildrenRemainReusableButReplayDoesNotProveAvailability(t *testing.T) {
	path := filepath.Join(t.TempDir(), "test.sqlite3")
	owner := session("owner", "project")
	if err := SaveSessionTo(path, &owner); err != nil {
		t.Fatal(err)
	}
	connection := storeMust(open(path))
	defer connection.Close()
	apply := func(ordinal uint64, update core.NativeAgentEvent) {
		t.Helper()
		if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, ordinal, update)); err != nil {
			t.Fatal(err)
		}
	}
	stableChild := "stable-child"
	apply(1, nativeTestSpawn("child", nil))
	apply(2, nativeTestText(t, "child", "retained context"))
	apply(3, core.NativeAgentEventState{SessionID: "child", State: core.NativeAgentStateCompleted})
	apply(4, core.NativeAgentEventAvailability{
		Reports: []core.NativeAgentAvailabilityReport{{
			SessionID:    "child",
			StableID:     &stableChild,
			Availability: core.NativeAgentAvailabilityAvailable,
		}},
		Complete: true,
	})
	reusable := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if reusable[0].Agent.State != core.NativeAgentStateCompleted {
		t.Fatalf("state = %v, want completed", reusable[0].Agent.State)
	}
	if reusable[0].Agent.Availability != core.NativeAgentAvailabilityAvailable {
		t.Fatalf("availability = %v, want available", reusable[0].Agent.Availability)
	}
	apply(5, core.NativeAgentEventState{SessionID: "child", State: core.NativeAgentStateRunning})
	if got := storeMust(loadNativeAgentsFrom(connection, "owner", 200))[0].Projection.Transcript; !reflect.DeepEqual(got, reusable[0].Projection.Transcript) {
		t.Fatalf("transcript changed: %#v", got)
	}
	apply(6, core.NativeAgentEventReplayBegin{})
	apply(7, core.NativeAgentEventReplayCommit{})
	retained := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if len(retained) != 1 {
		t.Fatalf("missing replay must retain inspectable history: %d children", len(retained))
	}
	if retained[0].Agent.Availability != core.NativeAgentAvailabilityUnknown {
		t.Fatalf("availability = %v, want unknown", retained[0].Agent.Availability)
	}
	if retained[0].Agent.State != core.NativeAgentStateDisconnected {
		t.Fatalf("state = %v, want disconnected", retained[0].Agent.State)
	}
	apply(8, core.NativeAgentEventAvailability{Reports: []core.NativeAgentAvailabilityReport{}, Complete: false})
	if got := storeMust(loadNativeAgentsFrom(connection, "owner", 200))[0].Agent.Availability; got != core.NativeAgentAvailabilityUnknown {
		t.Fatalf("availability = %v, want unknown", got)
	}
	apply(9, core.NativeAgentEventAvailability{Reports: []core.NativeAgentAvailabilityReport{}, Complete: true})
	missing := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if missing[0].Agent.Availability != core.NativeAgentAvailabilityUnavailable {
		t.Fatalf("availability = %v, want unavailable", missing[0].Agent.Availability)
	}
	if len(missing[0].Projection.Transcript) != 1 {
		t.Fatalf("transcript items = %d, want 1", len(missing[0].Projection.Transcript))
	}
}

func TestNativeReplayIsAtomicAndDoesNotDuplicateChildHistory(t *testing.T) {
	path := filepath.Join(t.TempDir(), "test.sqlite3")
	owner := session("owner", "project")
	if err := SaveSessionTo(path, &owner); err != nil {
		t.Fatal(err)
	}
	connection := storeMust(open(path))
	defer connection.Close()
	apply := func(ordinal uint64, update core.NativeAgentEvent) {
		t.Helper()
		if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, ordinal, update)); err != nil {
			t.Fatal(err)
		}
	}
	apply(1, nativeTestSpawn("child", nil))
	apply(2, nativeTestText(t, "child", strings.Repeat("x", 80_000)))
	original := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if len(original[0].Projection.Transcript) != 1 {
		t.Fatalf("transcript items = %d, want 1", len(original[0].Projection.Transcript))
	}
	apply(3, core.NativeAgentEventReplayBegin{})
	original[0].Agent.InvalidateAvailability()
	original[0].Agent.State = core.NativeAgentStateDisconnected
	apply(4, nativeTestSpawn("child", nil))
	apply(5, nativeTestText(t, "child", "replacement"))
	if got := storeMust(loadNativeAgentsFrom(connection, "owner", 200)); !reflect.DeepEqual(got, original) {
		t.Fatalf("staged replay leaked: %#v", got)
	}
	apply(6, core.NativeAgentEventReplayCommit{})
	replaced := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if len(replaced[0].Projection.Transcript) != 1 {
		t.Fatalf("transcript items = %d, want 1", len(replaced[0].Projection.Transcript))
	}
	encoded := storeMust(json.Marshal(replaced[0]))
	if !strings.Contains(string(encoded), "replacement") {
		t.Fatal("committed replay lost the replacement history")
	}
	apply(7, core.NativeAgentEventReplayBegin{})
	apply(8, nativeTestSpawn("child", nil))
	apply(9, nativeTestText(t, "child", "replacement"))
	apply(10, core.NativeAgentEventReplayCommit{})
	if got := len(storeMust(loadNativeAgentsFrom(connection, "owner", 200))[0].Projection.Transcript); got != 1 {
		t.Fatalf("transcript items = %d, want 1", got)
	}
	apply(11, core.NativeAgentEventReplayBegin{})
	apply(12, nativeTestSpawn("incomplete", nil))
	apply(13, core.NativeAgentEventDisconnected{})
	recovered := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if len(recovered) != 1 {
		t.Fatalf("recovered children = %d, want 1", len(recovered))
	}
	if recovered[0].Agent.SessionID != "child" {
		t.Fatalf("recovered child = %s", recovered[0].Agent.SessionID)
	}
	if recovered[0].Agent.State != core.NativeAgentStateDisconnected {
		t.Fatalf("state = %v, want disconnected", recovered[0].Agent.State)
	}
	if agent, ok := recovered[0].Projection.Transcript[0].Body.(core.TranscriptBodyAgent); !ok || agent.Streaming {
		t.Fatalf("recovered message = %#v, want a finished agent message", recovered[0].Projection.Transcript[0].Body)
	}
	materialized := storeMust(LoadMaterializedSessionFrom(path, "owner"))
	if materialized == nil || len(materialized.Transcript) != 0 {
		t.Fatal("native history leaked into the owner's transcript")
	}
}

func TestNativeChildrenIsolateToolIDsAndPreserveNestingAndTerminalStates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "test.sqlite3")
	owner := session("owner", "project")
	if err := SaveSessionTo(path, &owner); err != nil {
		t.Fatal(err)
	}
	connection := storeMust(open(path))
	defer connection.Close()
	parent := "child"
	for ordinal, native := range []core.NativeAgentEvent{nativeTestSpawn("child", nil), nativeTestSpawn("nested", &parent)} {
		if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, uint64(ordinal)+1, native)); err != nil {
			t.Fatal(err)
		}
	}
	for index, child := range []string{"child", "nested"} {
		update := nativeTestUpdate(t, child, fmt.Sprintf(`{"sessionUpdate":"tool_call","toolCallId":"same-id","title":%q,"kind":"read","status":"in_progress"}`, child))
		if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, uint64(index)+3, update)); err != nil {
			t.Fatal(err)
		}
	}
	if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 5, core.NativeAgentEventState{SessionID: "nested", State: core.NativeAgentStateFailed})); err != nil {
		t.Fatal(err)
	}
	views := storeMust(loadNativeAgentsFrom(connection, "owner", 200))
	if len(views) != 2 {
		t.Fatalf("children = %d, want 2", len(views))
	}
	if len(views[0].Projection.Transcript) != 1 || len(views[1].Projection.Transcript) != 1 {
		t.Fatal("each child should own one tool item")
	}
	if views[0].Agent.State != core.NativeAgentStateRunning {
		t.Fatalf("child state = %v, want running", views[0].Agent.State)
	}
	if views[1].Agent.State != core.NativeAgentStateFailed {
		t.Fatalf("nested state = %v, want failed", views[1].Agent.State)
	}
	if views[1].Agent.ParentViewID() != views[0].Agent.ViewID() {
		t.Fatalf("nested parent = %s, want %s", views[1].Agent.ParentViewID(), views[0].Agent.ViewID())
	}
}

func TestNativeHistoryPagesOlderMessagesWithoutDuplicates(t *testing.T) {
	path := filepath.Join(t.TempDir(), "test.sqlite3")
	owner := session("owner", "project")
	if err := SaveSessionTo(path, &owner); err != nil {
		t.Fatal(err)
	}
	connection := storeMust(open(path))
	defer connection.Close()
	if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, 1, nativeTestSpawn("child", nil))); err != nil {
		t.Fatal(err)
	}
	for ordinal := uint64(2); ordinal <= 232; ordinal++ {
		text, _ := json.Marshal(fmt.Sprintf("message %d: %s", ordinal, strings.Repeat("x", 1024)))
		update := nativeTestUpdate(t, "child", `{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":`+string(text)+`}}`)
		if err := applyNativeAgentEvent(connection, "owner", nativeTestEvent(t, ordinal, update)); err != nil {
			t.Fatal(err)
		}
	}
	page := storeMust(nativeAgentHistoryFrom(connection, "owner", "child", nil))
	if len(page.Items) != 200 {
		t.Fatalf("page items = %d, want 200", len(page.Items))
	}
	if encoded := storeMust(json.Marshal(page)); len(encoded) <= 64*1024 {
		t.Fatalf("page is %d bytes, want more than 64KiB", len(encoded))
	}
	if !page.HasMore {
		t.Fatal("page should have more")
	}
	first := page.Items[0]
	older := storeMust(nativeAgentHistoryFrom(connection, "owner", "child", &core.TranscriptCursor{Position: first.Position, StableID: first.StableID}))
	if len(older.Items) != 31 {
		t.Fatalf("older items = %d, want 31", len(older.Items))
	}
	if older.HasMore {
		t.Fatal("older page should be the last")
	}
	if older.GenerationOrdinal != page.GenerationOrdinal {
		t.Fatalf("generation = %d, want %d", older.GenerationOrdinal, page.GenerationOrdinal)
	}
	if older.Items[len(older.Items)-1].Position+1 != first.Position {
		t.Fatalf("pages are not contiguous: %d then %d", older.Items[len(older.Items)-1].Position, first.Position)
	}
	if older.Items[0].Position != 2 {
		t.Fatalf("oldest position = %d, want 2", older.Items[0].Position)
	}
}
