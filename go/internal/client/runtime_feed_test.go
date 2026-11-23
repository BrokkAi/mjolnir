// Port of the tests in mj-client/src/runtime_feed.rs.

package client

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestShownQuestionSurvivesALiveFormOnTheSameCard(t *testing.T) {
	question := core.NotificationStackEntry{
		CompletedAtMs:    1,
		CompletedOrdinal: 7,
		Kind:             core.NotificationKindNeedsInput,
		Message:          core.EntryMessageNone{},
	}
	holder := "child"
	card := NotificationCard{
		SinceMs:   1,
		Badge:     core.NotificationKindNeedsInput,
		Entries:   []core.NotificationStackEntry{question},
		Dismissal: core.NotificationDismissalStack{Revision: 1},
		Holder:    &holder,
		Form: &CardForm{
			Source:     CardFormSourceElicitation,
			RaisedAtMs: 2,
			RequestID:  "request",
			Message:    "child form",
		},
	}
	shown := card.ShownQuestion()
	if shown == nil || !feedJSONEqual(shown, &question) {
		t.Fatalf("shown question %+v", shown)
	}
	var wire NotificationCard
	encoded, err := json.Marshal(card)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(encoded, &wire); err != nil {
		t.Fatal(err)
	}
	if got := wire.ItemDestination("parent", 0); got != "parent" {
		t.Errorf("entry destination %q", got)
	}
	if got := wire.ItemDestination("parent", 1); got != "child" {
		t.Errorf("form destination %q", got)
	}
	if _, ok := wire.ItemIdentity("parent", 1); !ok {
		t.Error("the form has no identity")
	}
	unknown := wire
	form := *wire.Form
	form.RaisedAtMs = 0
	unknown.Form = &form
	if identity, ok := unknown.ItemIdentity("parent", 1); ok {
		t.Errorf("a form with an unknown raise time has no notify identity: %s", identity)
	}
	if _, ok := unknown.ItemIdentity("parent", 0); !ok {
		t.Error("the entry lost its identity")
	}
}

func feedCursor(incarnation string, sequence uint64) RuntimeCursor {
	return RuntimeCursor{Incarnation: incarnation, Sequence: sequence}
}

func feedSession(id string) RuntimeSessionView {
	return RuntimeSessionView{SessionID: id, PolicyConfigKeys: []string{}}
}

// feedWire sends a value through the wire encoding, as the daemon
// connection does.
func feedWire[T any](t *testing.T, value T) T {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	var decoded T
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatalf("decode %s: %v", encoded, err)
	}
	return decoded
}

func TestOwnershipOnlyDeltaReachesScopedReplicaAndIgnoresHiddenRows(t *testing.T) {
	var before RuntimeProjection
	record := func(id, state string) core.SessionRecord {
		var record core.SessionRecord
		if err := json.Unmarshal([]byte(fmt.Sprintf(`{
			"id": %q, "title": %q, "state": %q, "harness_kind": "codex",
			"last_profile": "profile", "bundle_id": "project", "target_template_id": "local",
			"created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
		}`, id, id, state)), &record); err != nil {
			t.Fatal(err)
		}
		return record
	}
	before.Records.Insert("running", record("running", "running"))
	before.Records.Insert("stopped", record("stopped", "stopped"))
	legacySnapshot := feedWire(t, before)
	if !legacySnapshot.EnvironmentOwners.IsEmpty() {
		t.Fatal("a snapshot without owners decodes with some")
	}

	after := before
	after.EnvironmentOwners.Insert("running", "owner")
	after.EnvironmentOwners.Insert("stopped", "stopped")
	initial := NewScopedProjection(&before, map[string]struct{}{})
	next := initial.Advance(&before, &after)
	delta := RuntimeDeltaBetween(&initial.Projection, &next.Projection)
	if len(delta.Records) != 0 {
		t.Fatalf("records %v", delta.Records)
	}
	if len(delta.EnvironmentOwners) != 1 || delta.EnvironmentOwners[0].ID != "running" ||
		delta.EnvironmentOwners[0].Value == nil || *delta.EnvironmentOwners[0].Value != "owner" {
		t.Fatalf("owners %+v", delta.EnvironmentOwners)
	}
	if delta.IsEmpty() {
		t.Fatal("an ownership change is empty")
	}
	delta = feedWire(t, delta)
	replica := initial.Projection
	delta.apply(&replica)
	if !replica.EnvironmentOwners.Equal(next.Projection.EnvironmentOwners, feedComparableEqual[string]) {
		t.Fatal("the replica's owners differ")
	}

	deleted := after
	deleted.EnvironmentOwners.Remove("running")
	next = next.Advance(&after, &deleted)
	change := RuntimeDeltaBetween(&replica, &next.Projection)
	change.apply(&replica)
	if !replica.EnvironmentOwners.IsEmpty() {
		t.Fatal("the removed owner survived")
	}
}

func TestNativeBrowserProjectPathsDoNotBreakControlFrames(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only, as in Rust")
	}
	var before RuntimeProjection
	after := before
	directory := "/project-\xff"
	after.Metadata.MountHistory.Insert("local", []string{directory})
	delta := RuntimeDeltaBetween(&before, &after)
	held, _ := delta.Metadata.MountHistory.Get("local")
	if !slices.Equal(held, []string{directory}) {
		t.Fatalf("mount history %q", held)
	}
	encoded, err := json.Marshal(delta)
	if err != nil {
		t.Fatal(err)
	}
	var decoded RuntimeDelta
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	if !decoded.Metadata.MountHistory.IsEmpty() {
		t.Fatal("mount history crossed the wire")
	}
}

func TestBrowserPromptBodiesCannotExhaustTheControlFrame(t *testing.T) {
	t.Skip("blocked on client-daemon: RuntimeSessionView.Materialized and RuntimeMaterializedSession (daemon.rs) are not declared in this worktree")
}

// Hard-won: AFK review #3: a coalesced feed hid transitions with equal availability.
func TestReplicaAppliesAddUpdateDeleteAtomicallyAndIgnoresDuplicates(t *testing.T) {
	var first RuntimeProjection
	first.Metadata.OperatorPresence = core.OperatorPresence{Away: true, Revision: 1}
	first.Sessions.Insert("old", feedSession("old"))
	next := first
	// The intermediate here revision2 was coalesced away by the watch feed.
	next.Metadata.OperatorPresence.Revision = 3
	next.Sessions.Remove("old")
	next.Sessions.Insert("new", feedSession("new"))
	firstCopy := first
	initial := RuntimeFrame{Kind: RuntimeFrameSnapshot, Cursor: feedCursor("a", 1), Projection: &firstCopy}
	changes := RuntimeDeltaBetween(&first, &next)
	delta := feedWire(t, RuntimeFrame{Kind: RuntimeFrameDelta, From: feedCursor("a", 1), Cursor: feedCursor("a", 2), Changes: &changes})
	var replica RuntimeReplica
	if _, err := replica.Apply(initial); err != nil {
		t.Fatal(err)
	}
	retained := replica.Projection
	applied, err := replica.Apply(delta)
	if err != nil || !applied {
		t.Fatalf("applied %v, %v", applied, err)
	}
	if !replica.Projection.Equal(&next) {
		t.Fatal("the replica differs from the publication")
	}
	if applied, _ := replica.Apply(delta); applied {
		t.Fatal("a duplicate delta applied")
	}
	if !retained.Sessions.ContainsKey("old") || retained.Sessions.ContainsKey("new") {
		t.Fatal("applying changed a retained projection")
	}
	next.Sessions.GetMut("new").Connected = true
	update := RuntimeDeltaBetween(&replica.Projection, &next)
	if _, err := replica.Apply(RuntimeFrame{Kind: RuntimeFrameDelta, From: feedCursor("a", 2), Cursor: feedCursor("a", 3), Changes: &update}); err != nil {
		t.Fatal(err)
	}
	if !replica.Projection.Sessions.GetShared("new").Connected {
		t.Fatal("the update was not applied")
	}
}

// A daemon newer than this client serves configuration keys the client has
// no field for, at any depth. The client still reads the feed. Only the file
// loader refuses an unread key, because a person wrote it.
// Hard-won: #1557: an older TUI stopped refreshing sessions once a profile set a newer field
func TestAClientReadsConfigurationKeysAddedAfterItWasBuilt(t *testing.T) {
	var profile core.HarnessProfile
	if err := json.Unmarshal([]byte(`{"kind": "codex", "home": "/home/agent/codex"}`), &profile); err != nil {
		t.Fatal(err)
	}
	config := core.DefaultConfig().WithLocalTargets()
	config.Profiles["codex"] = profile
	metadata := RuntimeMetadata{Config: core.NewSharedSnapshot(config)}
	encoded, err := json.Marshal(metadata)
	if err != nil {
		t.Fatal(err)
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	settingsSet(&value, settingsJSON(t, `{"setting": 1}`), "config", "a_later_section")
	settingsSet(&value, true, "config", "profiles", "codex", "a_later_profile_field")
	settingsSet(&value, "on", "config", "review", "a_later_review_field")
	settingsSet(&value, uint64(1), "config", "targets", "docker", "a_later_runtime_setting")
	settingsSet(&value, settingsJSON(t, `{"a_later_cache_field": 1}`), "config", "targets", "docker", "build_cache")
	var received RuntimeMetadata
	if err := settingsFromValue(value, &received); err != nil {
		t.Fatal(err)
	}
	if !feedJSONEqual(&received.Config.Get().Profiles, &config.Profiles) {
		t.Fatal("the profiles changed")
	}
	if !feedJSONEqual(&received.Config.Get().Review, &config.Review) {
		t.Fatal("the review settings changed")
	}

	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(fmt.Sprintf(
		"version = %d\n[profiles.codex]\nkind = \"codex\"\nhome = \"/home/agent/codex\"\na_later_profile_field = true\n",
		core.ConfigVersion)), 0o600); err != nil {
		t.Fatal(err)
	}
	_, err = core.ConfigLoadFrom(path)
	if err == nil || !strings.Contains(err.Error(), "`profiles.codex.a_later_profile_field`") {
		t.Fatalf("refusal %v", err)
	}
}

func TestDefaultTargetOriginSurvivesTheWireAndAnOlderDaemonSendsNone(t *testing.T) {
	metadata := RuntimeMetadata{Config: core.NewSharedSnapshot(core.DefaultConfig().WithLocalTargets())}
	metadata.FormHolders.Insert("parent", "child")
	metadata.DefaultTargets = sortedStringSet(metadata.Config.Get().DefaultTargets)
	if _, ok := metadata.DefaultTargets["docker"]; !ok {
		t.Fatal("docker is not a default target")
	}
	encoded, err := json.Marshal(metadata)
	if err != nil {
		t.Fatal(err)
	}
	var received RuntimeMetadata
	if err := json.Unmarshal(encoded, &received); err != nil {
		t.Fatal(err)
	}
	if holder, _ := received.FormHolders.Get("parent"); holder != "child" {
		t.Fatalf("holder %q", holder)
	}
	// The config alone loses the origin; the field restores it.
	if received.Config.Get().IsDefaultTarget("docker") {
		t.Fatal("the config alone kept the origin")
	}
	installed := received.InstalledConfig()
	if !installed.IsDefaultTarget("docker") {
		t.Fatal("the installed config lost the origin")
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	object := value.(jsonvalue.Object)
	object.SwapDelete("default_targets")
	object.SwapDelete("form_holders")
	var older RuntimeMetadata
	if err := settingsFromValue(object, &older); err != nil {
		t.Fatal(err)
	}
	if !older.FormHolders.IsEmpty() {
		t.Fatal("an older daemon's holders are not empty")
	}
	olderInstalled := older.InstalledConfig()
	if olderInstalled.IsDefaultTarget("docker") {
		t.Fatal("an older daemon's config has a default origin")
	}
	held := metadata
	metadata.Config.Mut().Advanced.AutoAfkMinutes = 3
	if held.Config.Get().Advanced.AutoAfkMinutes != 0 {
		t.Fatal("editing the metadata changed a held copy")
	}
	if metadata.Config.Get().Advanced.AutoAfkMinutes != 3 {
		t.Fatal("the edit was lost")
	}
}

func TestRememberedSubagentPoliciesAcceptOldScalarAndNewProfileMap(t *testing.T) {
	encoded, err := json.Marshal(RuntimeMetadata{})
	if err != nil {
		t.Fatal(err)
	}
	oldValue, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	object := oldValue.(jsonvalue.Object)
	object.SwapDelete("last_subagent_policies")
	object.Set("last_subagent_policy", settingsJSON(t, `{"mode": "mjolnir"}`))
	var old RuntimeMetadata
	if err := settingsFromValue(object, &old); err != nil {
		t.Fatal(err)
	}
	if policy, ok := old.LastSubagentPolicies[""]; !ok || !policy.UsesMjolnir() {
		t.Fatalf("old policies %+v", old.LastSubagentPolicies)
	}

	newValue, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	settingsSet(&newValue, settingsJSON(t, `{"mode": {"mode": "none"}}`), "last_subagent_policies")
	var updated RuntimeMetadata
	if err := settingsFromValue(newValue, &updated); err != nil {
		t.Fatal(err)
	}
	if policy := updated.LastSubagentPolicies["mode"]; policy.Mode != core.SubagentPolicyNone {
		t.Fatalf("new policies %+v", updated.LastSubagentPolicies)
	}
}

func TestAGapOrWrongIncarnationNeverPartiallyAppliesABatch(t *testing.T) {
	cursor := feedCursor("a", 4)
	replica := RuntimeReplica{Cursor: &cursor}
	before := replica.Projection
	for _, from := range []RuntimeCursor{feedCursor("a", 3), feedCursor("b", 4)} {
		unexpected := feedSession("unexpected")
		changes := RuntimeDelta{Sessions: KeyChanges[RuntimeSessionView]{{ID: "unexpected", Value: &unexpected}}}
		if _, err := replica.Apply(RuntimeFrame{Kind: RuntimeFrameDelta, From: from, Cursor: feedCursor("a", 5), Changes: &changes}); err == nil {
			t.Fatalf("a delta from %+v applied", from)
		}
		if !replica.Projection.Equal(&before) {
			t.Fatal("a refused delta changed the projection")
		}
	}
	if _, err := replica.Apply(RuntimeFrame{Kind: RuntimeFrameResetRequired}); err != nil {
		t.Fatal(err)
	}
	if replica.Cursor != nil {
		t.Fatal("a reset kept the cursor")
	}
	if _, err := replica.Apply(RuntimeFrame{Kind: RuntimeFrameSnapshot, Cursor: feedCursor("b", 1), Projection: &RuntimeProjection{}}); err != nil {
		t.Fatal(err)
	}
	if replica.Cursor == nil || !replica.Cursor.Equal(feedCursor("b", 1)) {
		t.Fatalf("cursor %+v", replica.Cursor)
	}
}

func feedAgentItem(position uint64, text string) *core.TranscriptItem {
	chunk, _ := json.Marshal(map[string]any{"content": map[string]string{"type": "text", "text": text}})
	ordinal := position
	return &core.TranscriptItem{
		StableID:                  fmt.Sprintf("agent:%d", position),
		Position:                  position,
		LatestContentEventOrdinal: &ordinal,
		CreatedAtMs:               int64(position),
		LastChangedAtMs:           int64(position),
		Body:                      core.TranscriptBodyAgent{Chunks: []json.RawMessage{chunk}},
	}
}

func feedMaterialized(items []*core.TranscriptItem) core.MaterializedSession {
	session := core.MaterializedSessionEmpty("s")
	if len(items) > 0 {
		session.AppliedEventOrdinal = items[len(items)-1].Position
	}
	session.Transcript = items
	return session
}

func feedProjectionWith(tail *SessionTail) RuntimeProjection {
	var projection RuntimeProjection
	projection.Transcripts.Insert("s", *tail)
	return projection
}

func feedReplyJSON(t *testing.T, tail *SessionTail) string {
	t.Helper()
	encoded, err := json.Marshal(feedWire(t, SessionTailReplyOf(tail)))
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

// A client that fetched a tail at one cursor and then applies the deltas
// after it holds exactly the daemon's tail, through appends, in-place edits,
// removals, the tail bound, and a projection reloaded with new pointers.
// Unchanged items keep their pointers on the client.
func TestAFetchedTailFollowsItsDeltasToTheDaemonsTail(t *testing.T) {
	const limit = 40
	// 4 KiB per item: well past 64 KiB in the tail and in each step.
	text := func(n uint64) string { return fmt.Sprintf("%04d %s", n, strings.Repeat("lorem ipsum ", 340)) }
	var items []*core.TranscriptItem
	for position := uint64(1); position <= 48; position++ {
		items = append(items, feedAgentItem(position, text(position)))
	}
	window := core.ProjectionWindow{}
	session := feedMaterialized(items)
	daemon := SessionTailOf(&session, &window, limit)
	if daemon.Items.Len() != limit || daemon.Window.OmittedItems != 8 {
		t.Fatalf("items %d, omitted %d", daemon.Items.Len(), daemon.Window.OmittedItems)
	}
	cursor := feedCursor("a", 1)
	replica := RuntimeReplica{Cursor: &cursor}
	fetched := feedWire(t, SessionTailReplyOf(&daemon))
	if fetched.Kind != SessionTailReplyTail {
		t.Fatal("a tail was published")
	}
	replica.Projection.Transcripts.Insert("s", SessionTailFromParts(*fetched.Header, fetched.Window, fetched.Items))
	// Daemon-local watermarks are deliberately absent from the transport.
	held := replica.Projection.Transcripts.GetShared("s")
	if feedReplyJSON(t, held) != feedReplyJSON(t, &daemon) {
		t.Fatal("the fetched tail differs from the daemon's")
	}
	withAnswer := daemon
	answered, accepted, turnStart := uint64(50), uint64(1), uint64(2)
	withAnswer.Header.LastTurnOutcome = &core.MaterializedTurnOutcome{
		AnsweredThrough:   &answered,
		CommandID:         "task",
		AcceptedOrdinal:   &accepted,
		TurnStartPosition: &turnStart,
		CompletedOrdinal:  50,
		CompletedAtMs:     100,
		Outcome:           core.TurnOutcomeKindCompleted{StopReason: "end_turn"},
	}
	reply, err := json.Marshal(SessionTailReplyOf(&withAnswer))
	if err != nil {
		t.Fatal(err)
	}
	replyValue, err := jsonvalue.Parse(reply)
	if err != nil {
		t.Fatal(err)
	}
	if settingsHas(settingsPath(replyValue, "header", "last_turn_outcome"), "answered_through") {
		t.Fatal("a reply carried answered_through")
	}
	if *withAnswer.Header.LastTurnOutcome.AnsweredThrough != 50 {
		t.Fatal("serializing changed the held outcome")
	}
	header := withAnswer.Header
	encoded, err := json.Marshal(TranscriptItems{Header: &header})
	if err != nil {
		t.Fatal(err)
	}
	encodedValue, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if settingsHas(settingsPath(encodedValue, "header", "last_turn_outcome"), "answered_through") {
		t.Fatal("a delta header carried answered_through")
	}

	steps := []func(items *[]*core.TranscriptItem){
		// A streamed message gains text: a new pointer for one item.
		func(items *[]*core.TranscriptItem) {
			(*items)[len(*items)-1] = feedAgentItem(48, "grown")
		},
		// New messages arrive and push the oldest out of the bound.
		func(items *[]*core.TranscriptItem) {
			for n := uint64(49); n <= 55; n++ {
				*items = append(*items, feedAgentItem(n, "new"))
			}
		},
		// A message inside the tail is removed.
		func(items *[]*core.TranscriptItem) {
			*items = slices.DeleteFunc(*items, func(item *core.TranscriptItem) bool { return item.Position == 30 })
		},
		// The projection is reloaded from the store: every pointer is new,
		// and one item's content changed (retention compaction).
		func(items *[]*core.TranscriptItem) {
			for i, item := range *items {
				if item.Position == 20 {
					(*items)[i] = feedAgentItem(20, "compacted")
				} else {
					copied := *item
					(*items)[i] = &copied
				}
			}
		},
	}
	sequence := uint64(1)
	for _, step := range steps {
		beforeDaemon := daemon
		heldBefore := *replica.Projection.Transcripts.GetShared("s")
		step(&items)
		published := feedMaterialized(slices.Clone(items))
		daemon.Publish(&published, &window, limit)
		beforeProjection, afterProjection := feedProjectionWith(&beforeDaemon), feedProjectionWith(&daemon)
		delta := feedWire(t, RuntimeDeltaBetween(&beforeProjection, &afterProjection))
		sequence++
		if _, err := replica.Apply(RuntimeFrame{
			Kind: RuntimeFrameDelta, From: feedCursor("a", sequence-1), Cursor: feedCursor("a", sequence), Changes: &delta,
		}); err != nil {
			t.Fatal(err)
		}
		held := replica.Projection.Transcripts.GetShared("s")
		if feedReplyJSON(t, held) != feedReplyJSON(t, &daemon) {
			t.Fatalf("after step %d the replica differs", sequence-1)
		}
		heldMaterialized, daemonMaterialized := held.Materialized(), daemon.Materialized()
		if !feedJSONEqual(&heldMaterialized, &daemonMaterialized) {
			t.Fatalf("after step %d the materialized projections differ", sequence-1)
		}
		for key, now := range held.Items.All() {
			before := heldBefore.Items.GetShared(key)
			if before != nil && transcriptItemEqual(*before, *now) && *before != *now {
				t.Fatalf("unchanged item %v kept its pointer", key.TailKey())
			}
		}
	}
	reloaded := replica.Projection.Transcripts.GetShared("s")
	var compacted *core.TranscriptItem
	for item := range reloaded.itemValues() {
		if item.Position == 20 {
			compacted = item
		}
		if item.Position == 30 {
			t.Fatal("the removed item came back")
		}
	}
	if compacted == nil {
		t.Fatal("position 20 is inside the bound")
	}
	if !transcriptItemEqual(compacted, feedAgentItem(20, "compacted")) {
		t.Fatal("the compacted content was not applied")
	}
	if reloaded.Items.Len() != limit {
		t.Fatalf("items %d", reloaded.Items.Len())
	}
}

// A client follows only the tails it holds. A tail it does not hold is
// fetched at a cursor of its own, a refetch marker drops the held one, and a
// new snapshot drops them all.
func TestOnlyHeldTailsAreFollowedAndASnapshotDropsThem(t *testing.T) {
	one := feedMaterialized([]*core.TranscriptItem{feedAgentItem(1, "one")})
	tail := SessionTailOf(&one, &core.ProjectionWindow{}, 8)
	grown := tail
	two := feedMaterialized([]*core.TranscriptItem{feedAgentItem(1, "one"), feedAgentItem(2, "two")})
	grown.Publish(&two, &core.ProjectionWindow{}, 8)
	cursor := feedCursor("a", 1)
	replica := RuntimeReplica{Cursor: &cursor}
	tailProjection, grownProjection := feedProjectionWith(&tail), feedProjectionWith(&grown)
	changes := RuntimeDeltaBetween(&tailProjection, &grownProjection)
	if _, err := replica.Apply(RuntimeFrame{Kind: RuntimeFrameDelta, From: feedCursor("a", 1), Cursor: feedCursor("a", 2), Changes: &changes}); err != nil {
		t.Fatal(err)
	}
	if !replica.Projection.Transcripts.IsEmpty() {
		t.Fatal("a change to a tail the client never fetched is not applied")
	}
	replica.Projection.Transcripts.Insert("s", grown)
	refetch := RuntimeDelta{Transcripts: []KeyedTranscriptChange{{ID: "s", Change: TranscriptChange{Kind: TranscriptChangeRefetch}}}}
	if _, err := replica.Apply(RuntimeFrame{Kind: RuntimeFrameDelta, From: feedCursor("a", 2), Cursor: feedCursor("a", 3), Changes: &refetch}); err != nil {
		t.Fatal(err)
	}
	if !replica.Projection.Transcripts.IsEmpty() {
		t.Fatal("a refetch kept the tail")
	}
	replica.Projection.Transcripts.Insert("s", grown)
	snapshotProjection := feedProjectionWith(&tail)
	snapshot := feedWire(t, RuntimeFrame{Kind: RuntimeFrameSnapshot, Cursor: feedCursor("b", 1), Projection: &snapshotProjection})
	if _, err := replica.Apply(snapshot); err != nil {
		t.Fatal(err)
	}
	if !replica.Projection.Transcripts.IsEmpty() {
		t.Fatal("a snapshot carries no tails, so a new daemon's are fetched again")
	}
}
