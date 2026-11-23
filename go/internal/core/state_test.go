// Port of mj-core/src/state/tests.rs.

package core

import (
	"encoding/json"
	"maps"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unicode"
)

func stateStr(text string) *string { return &text }

func stateU64(value uint64) *uint64 { return &value }

// Hard-won: e569a20c5: unanswered input must follow the latest outcome and user response.
func TestUnansweredInputOutcomeTracksAnswersAndCurrentCompletion(t *testing.T) {
	session := MaterializedSessionEmpty("input")
	session.LastTurnOutcome = &MaterializedTurnOutcome{
		CommandID:        "turn",
		CompletedOrdinal: 10,
		CompletedAtMs:    10,
		Outcome:          TurnOutcomeKindCompleted{StopReason: "AwaitingInput"},
	}
	if session.UnansweredInputOutcome(nil, nil) == nil {
		t.Fatal("awaiting input is not unanswered")
	}
	assessment := TurnAssessmentPending("turn", 10, 10, CompletionEvidence{StopReason: "EndTurn"})
	assessment.Status = StatusAssessed
	await := AssessmentActionAwaitInput
	assessment.Action = &await
	session.LastTurnOutcome.Outcome = TurnOutcomeKindCompleted{StopReason: "EndTurn"}
	if session.UnansweredInputOutcome(&assessment, nil) == nil {
		t.Fatal("assessed await input is not unanswered")
	}
	assessment.CompletedOrdinal = 9
	if session.UnansweredInputOutcome(&assessment, nil) != nil {
		t.Fatal("a stale assessment still asks")
	}
	assessment.CompletedOrdinal = 10
	session.Transcript = append(session.Transcript, stateUserItem(11, ""))
	if session.UnansweredInputOutcome(&assessment, nil) != nil {
		t.Fatal("a later user item did not answer")
	}
	var watermark *uint64
	if position, ok := session.LatestUserPosition(); ok {
		watermark = &position
	}
	session.Transcript = nil
	if session.UnansweredInputOutcome(&assessment, watermark) != nil {
		t.Fatal("the published user position did not answer")
	}
	session.QueuedPrompts = append(session.QueuedPrompts, MaterializedQueuedPrompt{
		CommandID:  "queued",
		Kind:       QueuedCommandKindPrompt{},
		Content:    []any{},
		QueuedAtMs: 11,
	})
	if session.UnansweredInputOutcome(&assessment, nil) != nil {
		t.Fatal("a queued prompt did not answer")
	}
	session.QueuedPrompts = nil
	session.ActiveTurn = &MaterializedTurn{CommandID: "active", TurnStartPosition: 11, StartedAtMs: 11}
	if session.UnansweredInputOutcome(&assessment, nil) != nil {
		t.Fatal("an active turn did not answer")
	}
}

func TestSessionBuildCacheDefaultsOptionalLegacyFields(t *testing.T) {
	var cache SessionBuildCache
	if err := json.Unmarshal([]byte(`{"host": "local", "directory": "/srv/mbx-cache"}`), &cache); err != nil {
		t.Fatal(err)
	}
	if cache.Directory != "/srv/mbx-cache" || cache.MaxSize != nil || cache.TargetRoot != nil {
		t.Fatalf("cache = %+v", cache)
	}
}

func stateUserItem(position uint64, text string) *TranscriptItem {
	content, _ := json.Marshal(map[string]string{"type": "text", "text": text})
	return &TranscriptItem{
		StableID:        "user:" + itoa(position),
		Position:        position,
		CreatedAtMs:     1_000,
		LastChangedAtMs: 1_000,
		Body:            TranscriptBodyUser{Content: []json.RawMessage{content}},
	}
}

func stateAgentItem(position uint64) *TranscriptItem {
	return &TranscriptItem{
		StableID:                  "agent:" + itoa(position),
		Position:                  position,
		LatestContentEventOrdinal: stateU64(position),
		CreatedAtMs:               1_000,
		LastChangedAtMs:           1_000,
		Body: TranscriptBodyAgent{
			Chunks: []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":"working"},"messageId":"answer"}`)},
		},
	}
}

func itoa(value uint64) string {
	encoded, _ := json.Marshal(value)
	return string(encoded)
}

func stateTurns(count uint64, text string) []*TranscriptItem {
	var items []*TranscriptItem
	for position := uint64(1); position <= count; position++ {
		if position%100 == 1 {
			items = append(items, stateUserItem(position, text))
		} else {
			items = append(items, stateAgentItem(position))
		}
	}
	return items
}

func TestProjectionWindowKeepsWholeTurnsAndUnsettledContent(t *testing.T) {
	session := MaterializedSessionEmpty("windowed")
	session.Transcript = stateTurns(2500, "original request")
	window := ProjectionWindowOf(&session)
	window.Trim(&session, 1024)
	if session.Transcript[0].Position != 1401 {
		t.Fatalf("first retained = %d", session.Transcript[0].Position)
	}
	if window.OmittedItems != 1400 {
		t.Fatalf("omitted = %d", window.OmittedItems)
	}
	if window.LatestTurnStartPosition == nil || *window.LatestTurnStartPosition != 2401 {
		t.Fatalf("latest turn start = %v", window.LatestTurnStartPosition)
	}
	if window.ProvisionalTitle == nil || *window.ProvisionalTitle != "original request" {
		t.Fatalf("provisional title = %v", window.ProvisionalTitle)
	}
	retained := append([]*TranscriptItem{}, session.Transcript...)
	window.Trim(&session, 1024)
	if len(session.Transcript) != len(retained) || session.Transcript[0] != retained[0] {
		t.Fatal("a second trim changed the window")
	}
	if window.OmittedItems != 1400 {
		t.Fatalf("omitted = %d", window.OmittedItems)
	}

	longTurn := MaterializedSessionEmpty("long-turn")
	longTurn.Transcript = append(longTurn.Transcript, stateUserItem(1, "large turn"))
	for position := uint64(2); position <= 2500; position++ {
		longTurn.Transcript = append(longTurn.Transcript, stateAgentItem(position))
	}
	longWindow := ProjectionWindowOf(&longTurn)
	longWindow.Trim(&longTurn, 1024)
	if len(longTurn.Transcript) != 2500 {
		t.Fatalf("never cut an active turn in half: %d", len(longTurn.Transcript))
	}

	pending := MaterializedSessionEmpty("pending")
	pending.Transcript = stateTurns(2500, "request")
	streaming := *pending.Transcript[1]
	body := streaming.Body.(TranscriptBodyAgent)
	body.Streaming = true
	streaming.Body = body
	pending.Transcript[1] = &streaming
	pendingWindow := ProjectionWindowOf(&pending)
	pendingWindow.Trim(&pending, 1024)
	if len(pending.Transcript) != 1200 {
		t.Fatalf("late streaming data keeps its original turn: %d", len(pending.Transcript))
	}
}

func stateSnapshot(session MaterializedSession, window ProjectionWindow) ManagedSessionSnapshot {
	// The Rust test decodes an idle operational state here; neither
	// assertion reads it.
	return ManagedSessionSnapshot{Materialized: session, Window: window}
}

// A polled projection carries only the end of the transcript. The title
// comes from the first user message and the completed turn from the last
// one, so both have to survive the head being outside the window.
func TestAWindowedProjectionAnswersTheSameTitleAndTurnAsAWholeOne(t *testing.T) {
	whole := MaterializedSessionEmpty("session-1")
	whole.Transcript = []*TranscriptItem{
		stateUserItem(1, "build the relay"),
		stateAgentItem(2),
		stateAgentItem(3),
		stateUserItem(4, "now test it"),
		stateAgentItem(5),
	}
	complete := stateSnapshot(whole, ProjectionWindowOf(&whole))

	// The same session, loaded as a two-item window: the head is gone and so
	// is the last user message.
	windowedSession := whole
	windowedSession.Transcript = whole.Transcript[3:]
	windowed := stateSnapshot(windowedSession, ProjectionWindowOf(&whole))
	windowed.Window.OmittedItems = 3

	title, ok := complete.ResolvedTitle()
	if !ok || title != "build the relay" {
		t.Fatalf("title = %q", title)
	}
	if other, otherOK := windowed.ResolvedTitle(); other != title || otherOK != ok {
		t.Fatalf("windowed title = %q", other)
	}
	turn, ok := complete.LatestCompletedTurnOrdinal()
	if !ok || turn != 4 {
		t.Fatalf("turn = %d", turn)
	}
	if other, otherOK := windowed.LatestCompletedTurnOrdinal(); other != turn || otherOK != ok {
		t.Fatalf("windowed turn = %d", other)
	}
}

// A session still working has not completed a turn, whatever its
// transcript says.
func TestARunningSessionReportsNoCompletedTurn(t *testing.T) {
	session := MaterializedSessionEmpty("session-1")
	session.Transcript = []*TranscriptItem{stateUserItem(1, "build it")}
	session.Execution = MaterializedExecutionStateRunning{StartedAtMs: 1}
	window := ProjectionWindowOf(&session)
	snapshot := stateSnapshot(session, window)
	if _, ok := snapshot.LatestCompletedTurnOrdinal(); ok {
		t.Fatal("a running session reported a completed turn")
	}
}

func stateSampleState() StateState {
	session := SessionRecord{
		Oracle:           true,
		WorkspaceID:      DefaultWorkspaceID,
		ID:               "0123456789abcdef",
		Title:            "Build Hel",
		HarnessKind:      HarnessKindCodex,
		LastProfile:      "codex-1",
		BundleID:         "hel",
		TargetTemplateID: "podman",
		AdditionalMounts: []AdditionalMount{{
			Source:      "/home/test/cache",
			Destination: "/mnt/cache",
			Access:      MountAccessCow,
		}},
		State: SessionStateRunning,
		Target: TargetLocatorLocalPodman{
			ContainerID:      "afb67d",
			WorkspaceStorage: PodmanWorkspaceLocatorContainerLayer{},
		},
		NativeSessionID: stateStr("native-1"),
		ACPSessionTitle: stateStr("Build Hel"),
		CreatedAt:       "2026-08-09T12:00:00Z",
		UpdatedAt:       "2026-08-09T12:01:00Z",
		Checkpoint: &CheckpointMetadata{
			ArchivePath:   "sessions/0123456789abcdef.hel.zip",
			Sha256:        strings.Repeat("a", 64),
			CreatedAt:     "2026-08-09T12:01:00Z",
			EventFrontier: 42,
		},
	}
	state := NewStateState()
	state.EnvironmentOwners.Insert(session.ID, session.ID)
	state.Sessions.Insert(session.ID, session)
	state.MountHistory.Insert("local", []string{"/home/test/cache"})
	return state
}

func stateSampleSession() SessionRecord {
	state := stateSampleState()
	session, ok := state.Sessions.Remove("0123456789abcdef")
	if !ok {
		panic("sample session")
	}
	return session
}

func stateSampleConfig() Config {
	// Rust's sample_config takes Default::default() for every section it
	// does not name; DefaultConfig supplies those defaults here.
	config := DefaultConfig()
	config.Profiles = map[string]HarnessProfile{
		"codex-1": NewHarnessProfile(HarnessKindCodex, "/home/test/.codex"),
	}
	config.Bundles = map[string]ProjectBundle{
		"hel": {
			PrimaryRepo: "hel",
			Repositories: []ProjectRepository{{
				ID:          "hel",
				Github:      stateStr("BrokkAi/hel"),
				Destination: "hel",
			}},
		},
	}
	config.Targets = map[string]TargetTemplate{
		"podman": TargetTemplateLocalPodman{ContainerTemplate: ContainerTemplate{
			Image:            "ubuntu:24.04",
			PullPolicy:       ImagePullPolicyAuto,
			Environment:      NewEnvironment(),
			WorkspaceStorage: PodmanWorkspaceStoragePodmanVolume{},
		}},
	}
	return config
}

func TestContainerCapacityCountsOwnersAndProvisioningButNotBorrowedChildren(t *testing.T) {
	state := stateSampleState()
	parent := *state.Sessions.Index("0123456789abcdef")
	parent.ResourceAllocation = SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 8 << 30}

	var runtime TargetRuntimeSettings
	if parent.TargetRuntime != nil {
		runtime = *parent.TargetRuntime
	} else {
		runtime = TargetRuntimeSettingsFromTemplate(stateSampleConfig().Targets["podman"])
	}
	requested := ContainerSettingsRequested(&parent)
	runtime.ContainerSettings = &requested
	parent.TargetRuntime = &runtime
	parent.ResourceAllocation = SessionResourceAllocationContainer{Cpus: 8, MemoryBytes: 16 << 30}
	child := parent
	child.ID = "fedcba9876543210"
	child.Target = TargetLocatorLocalPodman{
		BorrowedFrom:     stateStr(parent.ID),
		ContainerID:      "afb67d",
		WorkspaceStorage: PodmanWorkspaceLocatorContainerLayer{},
	}

	provisioning := parent
	provisioning.ID = "0011223344556677"
	provisioning.State = SessionStateProvisioning
	provisioning.Target = nil
	provisioning.ResourceAllocation = SessionResourceAllocationContainer{Cpus: 2, MemoryBytes: 4 << 30}
	// A removed locator during upgrade recovery does not erase its applied snapshot.
	if got, want := AppliedContainerSettingsOf(&provisioning).ResourceAllocation,
		parent.TargetRuntime.ContainerSettings.ResourceAllocation; got != want {
		t.Fatalf("applied allocation = %#v, want %#v", got, want)
	}

	total := HostContainerSize{Cpus: 10, MemoryBytes: 20 << 30}
	sessions := []*SessionRecord{&parent, &child, &provisioning}
	capacity := ComputeHostContainerCapacity(total, false, []string{"podman"}, sessions)
	// A boot-time pool is already net of the host's own needs: the same
	// commitments leave exactly the reserve more.
	pool := ComputeHostContainerCapacity(total, true, []string{"podman"}, sessions)
	if pool.Reserved.MemoryBytes != 0 {
		t.Fatalf("pool reserve = %d", pool.Reserved.MemoryBytes)
	}
	if pool.Remaining.MemoryBytes != capacity.Remaining.MemoryBytes+capacity.Reserved.MemoryBytes {
		t.Fatalf("pool remaining = %d", pool.Remaining.MemoryBytes)
	}
	if capacity.Committed != (HostContainerSize{Cpus: 6, MemoryBytes: 12 << 30}) {
		t.Fatalf("committed = %+v", capacity.Committed)
	}
	if capacity.Reserved != (HostContainerSize{Cpus: 0, MemoryBytes: 4 << 30}) {
		t.Fatalf("reserved = %+v", capacity.Reserved)
	}
	if capacity.Remaining != (HostContainerSize{Cpus: 4, MemoryBytes: 4 << 30}) {
		t.Fatalf("remaining = %+v", capacity.Remaining)
	}
	if !capacity.Complete {
		t.Fatal("capacity incomplete")
	}
	cpuOvercommit := SessionResourceAllocationContainer{Cpus: 8, MemoryBytes: 4 << 30}
	if capacity.Refuses(cpuOvercommit, true) {
		t.Fatal("CPU overcommit refused")
	}
	if warning, ok := capacity.OvercommitWarning(cpuOvercommit); ok {
		t.Fatalf("CPU overcommit warned: %s", warning)
	}
	ramOvercommit := SessionResourceAllocationContainer{Cpus: 8, MemoryBytes: 8 << 30}
	if !capacity.Refuses(ramOvercommit, true) {
		t.Fatal("RAM overcommit allowed on a hard limit")
	}
	if capacity.Refuses(ramOvercommit, false) {
		t.Fatal("RAM overcommit refused without a hard limit")
	}
	if warning, _ := capacity.OvercommitWarning(ramOvercommit); warning != "Asks for more RAM than the 4 GiB left on this host; it will still start." {
		t.Fatalf("warning = %q", warning)
	}

	// Hard-won: #1538: a container created at the target's defaults was
	// counted at a later, smaller Container settings request.
	defaults := parent
	defaultsRuntime := *defaults.TargetRuntime
	settings := *defaultsRuntime.ContainerSettings
	settings.ResourceAllocation = nil
	defaultsRuntime.ContainerSettings = &settings
	defaults.TargetRuntime = &defaultsRuntime
	unknown := ComputeHostContainerCapacity(total, false, []string{"podman"}, []*SessionRecord{&defaults})
	if unknown.Complete {
		t.Fatal("an unrecorded applied allocation was counted")
	}
}

func TestAnUnrecordedOwnerAllocationMakesRemainingCapacityUnknown(t *testing.T) {
	state := stateSampleState()
	var sessions []*SessionRecord
	for session := range state.Sessions.Values() {
		sessions = append(sessions, session)
	}
	capacity := ComputeHostContainerCapacity(HostContainerSize{Cpus: 10, MemoryBytes: 20 << 30}, false, []string{"podman"}, sessions)
	if capacity.Complete {
		t.Fatal("capacity complete")
	}
	if capacity.Remaining.Cpus != 0 || capacity.Remaining.MemoryBytes != 0 {
		t.Fatalf("remaining = %+v", capacity.Remaining)
	}
	allocation := SessionResourceAllocationContainer{Cpus: 2, MemoryBytes: 4 << 30}
	if !capacity.Refuses(allocation, true) {
		t.Fatal("unknown usage allowed on a hard limit")
	}
	if capacity.Refuses(allocation, false) {
		t.Fatal("unknown usage refused without a hard limit")
	}
	if warning, ok := capacity.OvercommitWarning(allocation); !ok || !strings.Contains(warning, "no recorded size") {
		t.Fatalf("warning = %q", warning)
	}
}

func TestCapacityGibTextNeverRoundsALimitUp(t *testing.T) {
	if got := FloorGibTenths((8 << 30) - 1); got != "7.9" {
		t.Fatalf("got %q", got)
	}
	if got := FloorGibTenths(64 << 30); got != "64" {
		t.Fatalf("got %q", got)
	}
}

// Hard-won: 1b293a48e: container sessions never used the YOLO sub-agent set because the predicate required ssh-bare.
func TestRecordedContainerExecutionPolicySelectsYoloSubagents(t *testing.T) {
	config := stateSampleConfig()
	container := stateSampleSession()
	container.TargetTemplateID = "podman"
	runtime := TargetRuntimeSettingsFromTemplate(config.Targets["podman"])
	if runtime.ExecutionPolicy != ExecutionPolicyUnconstrained {
		t.Fatalf("policy = %v", runtime.ExecutionPolicy)
	}
	container.TargetRuntime = &runtime
	if !container.IsYoloTarget(&config) {
		t.Fatal("container is not a YOLO target")
	}
	if _, ok := container.ExecutionEnforcement(&config); !ok {
		t.Fatal("no execution enforcement")
	}

	localhost := stateSampleSession()
	config.Targets["local"] = TargetTemplateLocalBare{}
	localhost.TargetTemplateID = "local"
	if localhost.IsYoloTarget(&config) {
		t.Fatal("localhost is a YOLO target")
	}
}

func TestSessionRecordsWrittenBeforeContainerOverridesStillLoad(t *testing.T) {
	session := stateSampleSession()
	session.NoGithubEvents = true
	encoded, err := json.Marshal(session)
	if err != nil {
		t.Fatal(err)
	}
	var object map[string]json.RawMessage
	if err := json.Unmarshal(encoded, &object); err != nil {
		t.Fatal(err)
	}
	object["no_mailbox"] = object["no_github_events"]
	delete(object, "no_github_events")
	if _, ok := object["container_cpus"]; ok {
		t.Fatal("container_cpus is written")
	}
	if _, ok := object["container_memory"]; ok {
		t.Fatal("container_memory is written")
	}
	older, _ := json.Marshal(object)
	var loaded SessionRecord
	if err := json.Unmarshal(older, &loaded); err != nil {
		t.Fatal(err)
	}
	stateAssertSameJSON(t, loaded, session)

	var roundTripped SessionRecord
	if err := json.Unmarshal(encoded, &roundTripped); err != nil {
		t.Fatal(err)
	}
	stateAssertSameJSON(t, roundTripped, session)
}

// stateAssertSameJSON compares records by their encoding, which covers every
// serialized field (the Rust test compares with derived equality).
func stateAssertSameJSON(t *testing.T, got, want any) {
	t.Helper()
	gotJSON, err := json.Marshal(got)
	if err != nil {
		t.Fatal(err)
	}
	wantJSON, err := json.Marshal(want)
	if err != nil {
		t.Fatal(err)
	}
	if string(gotJSON) != string(wantJSON) {
		t.Fatalf("got %s\nwant %s", gotJSON, wantJSON)
	}
}

func stateSubagent(kind SubagentKind, child, parent *SessionRecord, taskName, requestKey string) SubagentMetadata {
	return SubagentMetadata{
		Kind:            kind,
		ChildSessionID:  child.ID,
		ParentSessionID: parent.ID,
		TaskName:        taskName,
		ProfileID:       child.LastProfile,
		RequestKey:      requestKey,
		CreatedAt:       child.CreatedAt,
	}
}

func TestLedgerOwnershipControlsCheckoutProjectAndReviewAcrossLogicalAncestry(t *testing.T) {
	config := stateSampleConfig()
	parent := stateSampleSession()
	parent.ID = "parent-session"
	parent.ManagedWorktree = &ManagedWorktree{
		Kind:                   ManagedCheckoutKindWorktree,
		SourceProjectDirectory: "/home/test/Projects/source",
		SourceRepository:       "/home/test/Projects/source",
		WorktreeRoot:           "/worktrees/parent-session",
		Branch:                 "mj/parent-session",
		Target:                 ManagedWorktreeTargetLocal{},
	}
	parent.ProjectDirectory = stateStr("/worktrees/parent-session")
	var off SessionReview = SessionReviewOff{}
	parent.Review = &off
	// A child is launched into the parent's worktree checkout, which is
	// named after the parent session, and owns no worktree of its own.
	child := stateSampleSession()
	child.ID = "child-session"
	child.ManagedWorktree = nil
	child.ProjectDirectory = stateStr("/worktrees/parent-session")

	state := NewStateState()
	state.Sessions.Insert(parent.ID, parent)
	state.Sessions.Insert(child.ID, child)
	state.EnvironmentOwners.Insert(parent.ID, parent.ID)
	state.EnvironmentOwners.Insert(child.ID, parent.ID)
	state.Subagents.Insert(child.ID, stateSubagent(SubagentKindWorker, &child, &parent, "Inspect parser", "request-1"))
	owned := stateCheckoutSession("owned")
	owned.ProjectDirectory = stateStr("/worktrees/owned")
	var push SessionReview = SessionReviewPush{}
	owned.Review = &push
	state.Sessions.Insert(owned.ID, owned)
	state.EnvironmentOwners.Insert(owned.ID, owned.ID)
	relation, _ := state.Subagents.Get(child.ID)
	relation.ChildSessionID = owned.ID
	state.Subagents.Insert(owned.ID, relation)
	nested := stateCheckoutSession("nested")
	nested.State = SessionStateRunning
	state.Sessions.Insert(nested.ID, nested)
	state.EnvironmentOwners.Insert(nested.ID, owned.ID)
	relation.ChildSessionID = nested.ID
	relation.ParentSessionID = owned.ID
	state.Subagents.Insert(nested.ID, relation)
	state.Sessions.GetMut(child.ID).State = SessionStateRunning

	if name := child.ProjectName(&config); name != "parent-session" {
		t.Fatalf("child project name = %q", name)
	}
	if got, want := state.ProjectIdentitySession(&child).ProjectSource(&config).Key, parent.ProjectSource(&config).Key; got != want {
		t.Fatalf("a child groups under the project its parent works in: %q != %q", got, want)
	}
	if state.ProjectIdentitySession(&parent).ID != parent.ID {
		t.Fatal("a session that is not a sub-agent keeps its own project identity")
	}
	if state.ProjectIdentitySession(&owned).ID != owned.ID || state.ProjectIdentitySession(&nested).ID != owned.ID {
		t.Fatal("project identity follows the environment owner")
	}
	for _, c := range []struct{ session, owner string }{
		{child.ID, parent.ID}, {owned.ID, owned.ID}, {nested.ID, owned.ID},
	} {
		got, err := state.WorkspaceOwner(c.session)
		if err != nil || got.ID != c.owner {
			t.Fatalf("workspace owner of %s: %v, %v", c.session, got, err)
		}
		review, failed, ok := state.reviewOwner(c.session)
		if !ok || review == nil || review.ID != c.owner {
			t.Fatalf("review owner of %s: %v, %q, %v", c.session, review, failed, ok)
		}
	}
	if got := state.ActiveBorrowers(parent.ID); !slices.Equal(got, []string{child.ID}) {
		t.Fatalf("parent borrowers %v", got)
	}
	if got := state.ActiveBorrowers(owned.ID); !slices.Equal(got, []string{nested.ID}) {
		t.Fatalf("owned borrowers %v", got)
	}
	if got := state.DirectOwnedChildIDs(parent.ID); !slices.Equal(got, []string{owned.ID}) {
		t.Fatalf("parent's owned children %v", got)
	}
	if got := state.DirectOwnedChildIDs(owned.ID); len(got) != 0 {
		t.Fatalf("owned's owned children %v", got)
	}
	if owns, known := state.OwnsEnvironment(owned.ID); !known || !owns {
		t.Fatal("owned owns its environment")
	}
	if owns, known := state.OwnsEnvironment(child.ID); !known || owns {
		t.Fatal("child borrows its environment")
	}

	native := ViewID(parent.ID, "native")
	state.Sessions.Insert(native, stateCheckoutSession(native))
	state.EnvironmentOwners.Insert(native, native)
	state.EnvironmentOwners.Insert("unknown", "unknown")
	if _, known := state.OwnsEnvironment(native); known {
		t.Fatal("a native view has no environment ownership")
	}
	if _, known := state.OwnsEnvironment("unknown"); known {
		t.Fatal("an unknown session has no environment ownership")
	}
	state.Sessions.Remove(parent.ID)
	if owner, ok := state.EnvironmentOwnerID(child.ID); !ok || owner != parent.ID {
		t.Fatalf("the ledger owner outlives its record: %q, %v", owner, ok)
	}
	if _, err := state.WorkspaceOwner(child.ID); err == nil {
		t.Fatal("a missing owner record is unavailable")
	}
	state.Subagents.Remove(child.ID)
	state.EnvironmentOwners.Remove(child.ID)
	if _, known := state.OwnsEnvironment(child.ID); known {
		t.Fatal("an unbound session has no environment ownership")
	}
	if _, err := state.Checkout(child.ID); err == nil {
		t.Fatal("an unbound session has no checkout")
	}
}

func TestManagedAndNativeChildrenAreSubAgentsAndTheirOwnerIsNot(t *testing.T) {
	parent := stateSampleSession()
	parent.ID = "0123456789abcdef0123456789abcdef"
	child := stateSampleSession()
	child.ID = "fedcba9876543210fedcba9876543210"
	state := NewStateState()
	state.Sessions.Insert(parent.ID, parent)
	state.Sessions.Insert(child.ID, child)
	state.Subagents.Insert(child.ID, stateSubagent(SubagentKindWorker, &child, &parent, "Inspect parser", "request-1"))
	native := ViewID(parent.ID, "a0c7080aee7ead7c5:generation:2")

	if !IsViewID(native) || IsViewID(parent.ID) {
		t.Fatal("view id recognition")
	}
	if !state.IsSubagentSession(child.ID) {
		t.Fatal("a managed child")
	}
	if !state.IsSubagentSession(native) {
		t.Fatal("a harness-owned child")
	}
	if state.IsSubagentSession(parent.ID) {
		t.Fatal("the owner")
	}
}

func TestOnlyASubAgentWithoutALiveWorkerIsStopped(t *testing.T) {
	parent := stateSampleSession()
	parent.ID = "0123456789abcdef0123456789abcdef"
	parent.State = SessionStateStopped
	child := stateSampleSession()
	child.ID = "fedcba9876543210fedcba9876543210"
	state := NewStateState()
	state.Sessions.Insert(parent.ID, parent)
	state.Sessions.Insert(child.ID, child)
	state.Subagents.Insert(child.ID, stateSubagent(SubagentKindReviewer, &child, &parent, "Review", "request-1"))

	for _, c := range []struct {
		state   SessionState
		stopped bool
	}{
		{SessionStateRunning, false},
		{SessionStateClosing, false},
		{SessionStateParked, true},
		{SessionStateStopped, true},
		{SessionStateError, true},
	} {
		state.Sessions.GetMut(child.ID).State = c.state
		if state.IsStoppedSubagent(child.ID) != c.stopped {
			t.Fatalf("%v", c.state)
		}
	}
	if state.IsStoppedSubagent(parent.ID) {
		t.Fatal("a stopped owner")
	}
	if state.IsStoppedSubagent("missing") {
		t.Fatal("a missing session")
	}
}

func stateCheckoutSession(id string) SessionRecord {
	session := stateSampleSession()
	session.ID = id
	return session
}

func stateManagedCheckoutRecord(id string, kind ManagedCheckoutKind, projectDirectory *string, target ManagedWorktreeTarget) SessionRecord {
	session := stateCheckoutSession(id)
	directory, branch := "worktrees", "mj/"+id
	if kind == ManagedCheckoutKindClone {
		directory, branch = "clones", "main"
	}
	session.ProjectDirectory = projectDirectory
	session.ManagedWorktree = &ManagedWorktree{
		Kind:                   kind,
		SourceProjectDirectory: "/srv/project/packages/app",
		SourceRepository:       "/srv/project",
		WorktreeRoot:           "/srv/project/.mj/" + directory + "/" + id,
		Branch:                 branch,
		Target:                 target,
	}
	return session
}

func TestCheckoutDerivationCoversAttachedManagedBundleAndBorrowedRecords(t *testing.T) {
	state := NewStateState()

	localNonGit := stateCheckoutSession("local-non-git")
	localNonGit.ProjectDirectory = stateStr("/work/plain-directory")
	create := true
	localNonGit.CreateManagedWorktree = &create
	localNonGit.Target = TargetLocatorLocalBare{WorkerRoot: "/var/lib/mj/workers/local-non-git"}
	state.Sessions.Insert(localNonGit.ID, localNonGit)

	sshRaw := stateCheckoutSession("ssh-raw")
	sshRaw.ProjectDirectory = stateStr("/home/dev/project")
	sshRaw.Target = TargetLocatorSshBare{Host: "dev.example.test", Workspace: "/home/dev"}
	state.Sessions.Insert(sshRaw.ID, sshRaw)

	rawWorktreePath := "/srv/project/.mj/worktrees/managed-worktree/packages/app"
	managedWorktree := stateManagedCheckoutRecord("managed-worktree", ManagedCheckoutKindWorktree,
		stateStr(rawWorktreePath), ManagedWorktreeTargetLocal{})
	state.Sessions.Insert(managedWorktree.ID, managedWorktree)

	rawClonePath := "/srv/project/.mj/clones/managed-clone/packages/app"
	managedClone := stateManagedCheckoutRecord("managed-clone", ManagedCheckoutKindClone, stateStr(rawClonePath),
		ManagedWorktreeTargetSsh{Destination: "dev@example.test", SSHArgs: []string{"-p", "2222"}})
	state.Sessions.Insert(managedClone.ID, managedClone)

	bundleWorkspace := stateCheckoutSession("bundle-workspace")
	state.Sessions.Insert(bundleWorkspace.ID, bundleWorkspace)
	savedProjectWorkspace := stateCheckoutSession("saved-project-workspace")
	identity := RepositoryIdentityFromRemote("https://github.com/BrokkAi/hel.git")
	if identity == nil {
		t.Fatal("valid repository identity")
	}
	savedProjectWorkspace.Project = &ProjectBundleSnapshot{
		Identities: map[string]RepositoryIdentity{"hel": identity},
		Bundle:     stateSampleConfig().Bundles["hel"],
	}
	state.Sessions.Insert(savedProjectWorkspace.ID, savedProjectWorkspace)

	// A legacy record with neither checkout field is still a managed bundle
	// workspace, even when its accepted project snapshot was not persisted.
	legacyWorkspace := stateCheckoutSession("legacy-workspace")
	legacyWorkspace.Project = nil
	state.Sessions.Insert(legacyWorkspace.ID, legacyWorkspace)

	// Incomplete managed legacy data keeps managed ownership; its actual
	// project directory is unknown instead of being silently treated as
	// user-owned.
	incompleteManaged := stateManagedCheckoutRecord("incomplete-managed", ManagedCheckoutKindWorktree, nil, ManagedWorktreeTargetLocal{})
	state.Sessions.Insert(incompleteManaged.ID, incompleteManaged)

	// Invalid legacy data with a managed descriptor and unrelated raw path
	// still gets one deterministic result: cleanup follows the descriptor,
	// while location readers retain the recorded project path. State
	// validation rejects this shape before it can be persisted again.
	inconsistentManaged := stateManagedCheckoutRecord("inconsistent-managed", ManagedCheckoutKindWorktree,
		stateStr("/home/dev/other-project"), ManagedWorktreeTargetLocal{})
	state.Sessions.Insert(inconsistentManaged.ID, inconsistentManaged)

	for id := range state.Sessions.Keys() {
		state.EnvironmentOwners.Insert(id, id)
	}
	addChild := func(id string, parent *SessionRecord) {
		child := stateCheckoutSession(id)
		child.ProjectDirectory = cloneStringPointer(parent.ProjectDirectory)
		child.ManagedWorktree = nil
		state.Sessions.Insert(child.ID, child)
		state.EnvironmentOwners.Insert(child.ID, parent.ID)
		state.Subagents.Insert(child.ID, stateSubagent(SubagentKindWorker, &child, parent, "Borrow checkout", "request-"+id))
	}
	addChild("child-attached", &localNonGit)
	addChild("child-managed-worktree", &managedWorktree)
	addChild("child-managed-clone", &managedClone)
	addChild("child-bundle-workspace", &bundleWorkspace)
	addChild("child-saved-project-workspace", &savedProjectWorkspace)

	checkout := func(id string) Checkout {
		t.Helper()
		value, err := state.Checkout(id)
		if err != nil {
			t.Fatal(err)
		}
		return value
	}
	if c, ok := checkout("local-non-git").(CheckoutAttached); !ok || c.Path != "/work/plain-directory" {
		t.Fatalf("local-non-git = %#v", c)
	}
	if c, ok := checkout("ssh-raw").(CheckoutAttached); !ok || c.Path != "/home/dev/project" {
		t.Fatalf("ssh-raw = %#v", c)
	}
	if c, ok := checkout("managed-worktree").(CheckoutManagedWorktree); !ok ||
		c.Worktree.Kind != ManagedCheckoutKindWorktree || c.ProjectDir == nil || *c.ProjectDir != rawWorktreePath {
		t.Fatalf("managed-worktree = %#v", c)
	}
	if c, ok := checkout("managed-clone").(CheckoutManagedWorktree); !ok ||
		c.Worktree.Kind != ManagedCheckoutKindClone || c.ProjectDir == nil || *c.ProjectDir != rawClonePath {
		t.Fatalf("managed-clone = %#v", c)
	}
	for _, id := range []string{"bundle-workspace", "saved-project-workspace", "legacy-workspace"} {
		if _, ok := checkout(id).(CheckoutManagedWorkspace); !ok {
			t.Fatalf("%s is not a managed workspace", id)
		}
	}
	if c, ok := checkout("incomplete-managed").(CheckoutManagedWorktree); !ok || c.ProjectDir != nil {
		t.Fatalf("incomplete-managed = %#v", c)
	}
	if c, ok := checkout("inconsistent-managed").(CheckoutManagedWorktree); !ok ||
		c.ProjectDir == nil || *c.ProjectDir != "/home/dev/other-project" {
		t.Fatalf("inconsistent-managed = %#v", c)
	}

	for _, c := range []struct{ child, parent, kind string }{
		{"child-attached", "local-non-git", "attached"},
		{"child-managed-worktree", "managed-worktree", "managed-worktree"},
		{"child-managed-clone", "managed-clone", "managed-worktree"},
		{"child-bundle-workspace", "bundle-workspace", "managed-workspace"},
		{"child-saved-project-workspace", "saved-project-workspace", "managed-workspace"},
	} {
		value := checkout(c.child)
		borrowed, ok := value.(CheckoutBorrowed)
		if !ok {
			t.Fatalf("%s must borrow its parent's checkout", c.child)
		}
		if borrowed.OwnerRecord.ID != c.parent {
			t.Fatalf("%s owner = %s", c.child, borrowed.OwnerRecord.ID)
		}
		matches := false
		switch value.Effective().(type) {
		case CheckoutAttached:
			matches = c.kind == "attached"
		case CheckoutManagedWorktree:
			matches = c.kind == "managed-worktree"
		case CheckoutManagedWorkspace:
			matches = c.kind == "managed-workspace"
		}
		if !matches {
			t.Fatalf("%s effective checkout = %#v", c.child, value.Effective())
		}
	}

	projectedChild := *state.Sessions.Index("child-managed-worktree")
	projectedChild.ProjectDirectory = stateStr("/wrong/copied/path")
	value, err := state.CheckoutForRecord("child-managed-worktree", &projectedChild)
	if err != nil {
		t.Fatal(err)
	}
	borrowed, ok := value.(CheckoutBorrowed)
	if !ok {
		t.Fatalf("projected child = %#v", value)
	}
	if directory, ok := borrowed.Checkout.ProjectDirectory(); !ok || directory != rawWorktreePath {
		t.Fatalf("projected child directory = %q", directory)
	}
}

func TestProjectSourceUsesBundleRepositoryAndIgnoresManagedWorktreeDestinations(t *testing.T) {
	config := stateSampleConfig()
	session := stateSampleSession()
	source := session.ProjectSource(&config)
	if source.Key != "github:brokkai/hel" || source.Short != "hel" || source.Full != "BrokkAi/hel" {
		t.Fatalf("source = %+v", source)
	}
	ssh := ProjectSourceIdentityGitRemote("git@github.com:BrokkAi/bifrost-dev.git", ShorthandAllowed)
	https := ProjectSourceIdentityGitRemote("https://github.com/BrokkAi/bifrost-dev.git", ShorthandAllowed)
	if (ssh == nil) != (https == nil) || (ssh != nil && *ssh != *https) {
		t.Fatalf("ssh %+v != https %+v", ssh, https)
	}
	brokk := ProjectSourceIdentityGitRemote("BrokkAi/bifrost-dev", ShorthandAllowed)
	other := ProjectSourceIdentityGitRemote("OtherOrg/bifrost-dev", ShorthandAllowed)
	if (brokk == nil) == (other == nil) && (brokk == nil || *brokk == *other) {
		t.Fatal("different owners share an identity")
	}

	session.ProjectDirectory = stateStr("/home/test/Projects/source/.mj/worktrees/0123456789abcdef")
	session.ManagedWorktree = &ManagedWorktree{
		Kind:                   ManagedCheckoutKindWorktree,
		SourceProjectDirectory: "/home/test/Projects/source/crate",
		SourceRepository:       "/home/test/Projects/source",
		WorktreeRoot:           "/home/test/Projects/source/.mj/worktrees/0123456789abcdef",
		Branch:                 "mj/0123456789abcdef",
		Target:                 ManagedWorktreeTargetLocal{},
	}
	source = session.ProjectSource(&config)
	if source.Short != "source" || source.Full != "/home/test/Projects/source" || strings.Contains(source.Full, ".mj/worktrees") {
		t.Fatalf("source = %+v", source)
	}
}

func TestMultiRepositoryBundlesIncludeAllRepositoriesInSortedIdentityOrder(t *testing.T) {
	config := stateSampleConfig()
	primary := config.Bundles["hel"].Repositories[0]
	secondary := ProjectRepository{ID: "docs", Github: stateStr("BrokkAi/docs"), Destination: "docs"}
	config.Bundles["with-docs"] = ProjectBundle{PrimaryRepo: primary.ID, Repositories: []ProjectRepository{primary, secondary}}
	session := stateSampleSession()
	session.BundleID = "with-docs"

	if name := session.ProjectName(&config); name != "docs + hel" {
		t.Fatalf("name = %q", name)
	}
	want := ProjectSourceIdentity{
		Key:   `bundle:["github:brokkai/docs","github:brokkai/hel"]`,
		Short: "docs + hel",
		Full:  "BrokkAi/docs + BrokkAi/hel",
	}
	if got := session.ProjectSource(&config); got != want {
		t.Fatalf("source = %+v", got)
	}

	otherSecondary := secondary
	otherSecondary.Github = stateStr("OtherOrg/docs")
	config.Bundles["with-other-docs"] = ProjectBundle{PrimaryRepo: primary.ID, Repositories: []ProjectRepository{primary, otherSecondary}}
	otherSession := session
	otherSession.BundleID = "with-other-docs"
	if session.ProjectSource(&config).Key == otherSession.ProjectSource(&config).Key {
		t.Fatal("different bundles share a key")
	}
}

func TestMultiRepositoryBundleIdentityIgnoresRepositoryOrderAndPrimarySelection(t *testing.T) {
	config := stateSampleConfig()
	primary := config.Bundles["hel"].Repositories[0]
	secondary := ProjectRepository{ID: "docs", Github: stateStr("BrokkAi/docs"), Destination: "docs"}
	config.Bundles["first-order"] = ProjectBundle{PrimaryRepo: primary.ID, Repositories: []ProjectRepository{primary, secondary}}
	config.Bundles["second-order"] = ProjectBundle{PrimaryRepo: secondary.ID, Repositories: []ProjectRepository{secondary, primary}}

	first := stateSampleSession()
	first.BundleID = "first-order"
	second := first
	second.BundleID = "second-order"
	if first.ProjectSource(&config) != second.ProjectSource(&config) {
		t.Fatal("repository order changed the identity")
	}
}

// Stored identities (RepositoryIdentity in sessions, project locations and
// project-memory directory names) come from these functions, so each remote
// spelling must keep mapping to the identity recorded before the shared
// GitHub parser. The expected values were captured from the parsers it
// replaced. The last four rows are the defects it fixed.
func TestGitRemoteKeepsTheIdentitiesStoredForEachRemoteSpelling(t *testing.T) {
	type expected struct{ key, short, full string }
	table := []struct {
		input string
		want  *expected
	}{
		{"owner/repo", &expected{"github:owner/repo", "repo", "owner/repo"}},
		{"owner/repo.git", &expected{"github:owner/repo", "repo", "owner/repo"}},
		{"mirror/repo.git", &expected{"github:mirror/repo", "repo", "mirror/repo"}},
		{"https://github.com/o/r", &expected{"github:o/r", "r", "o/r"}},
		{"https://github.com/o/r.git", &expected{"github:o/r", "r", "o/r"}},
		{"https://github.com/o/r/", &expected{"github:o/r", "r", "o/r"}},
		{"https://github.com/o/r/extra", &expected{"git:https://github.com/o/r/extra", "extra", "https://github.com/o/r/extra"}},
		{"http://github.com/o/r", &expected{"github:o/r", "r", "o/r"}},
		{"https://user:pass@github.com/o/r.git", &expected{"github:o/r", "r", "o/r"}},
		{"https://github.com:443/o/r", &expected{"github:o/r", "r", "o/r"}},
		{"https://github.com:444/o/r", &expected{"git:https://github.com:444/o/r", "r", "https://github.com:444/o/r"}},
		{"git@github.com:o/r.git", &expected{"github:o/r", "r", "o/r"}},
		{"github.com:o/r.git", &expected{"github:o/r", "r", "o/r"}},
		{"ssh://git@github.com/o/r.git", &expected{"github:o/r", "r", "o/r"}},
		{"ssh://github.com/o/r.git", &expected{"github:o/r", "r", "o/r"}},
		{"foo@github.com:o/r", &expected{"github:o/r", "r", "o/r"}},
		{"ssh://git@github.com:22/o/r.git", &expected{"git:ssh://github.com:22/o/r", "r", "ssh://github.com:22/o/r"}},
		{"ssh://git@ssh.github.com:443/o/r.git", &expected{"git:ssh://ssh.github.com:443/o/r", "r", "ssh://ssh.github.com:443/o/r"}},
		{"ssh://ssh.github.com:443/o/r.git", &expected{"git:ssh://ssh.github.com:443/o/r", "r", "ssh://ssh.github.com:443/o/r"}},
		// `git://` is a GitHub spelling to the parser, never a GitHub identity.
		{"git://github.com/o/r.git", &expected{"git:git://github.com/o/r", "r", "git://github.com/o/r"}},
		// An owner/repo parse must not turn this route into the GitHub
		// identity: identities are stored, and it was a network remote.
		{"git@ssh.github.com:o/r.git", &expected{"git:ssh.github.com:o/r", "r", "ssh.github.com:o/r"}},
		{"https://GITHUB.com/O/R", &expected{"github:o/r", "R", "O/R"}},
		{"./o/r", nil},
		{"/o/r", nil},
		{"https://gist.github.com/o/r", &expected{"git:https://gist.github.com/o/r", "r", "https://gist.github.com/o/r"}},
		{"https://github.example.com/o/r", &expected{"git:https://github.example.com/o/r", "r", "https://github.example.com/o/r"}},
		{"https://notgithub.com/o/r", &expected{"git:https://notgithub.com/o/r", "r", "https://notgithub.com/o/r"}},
		// Fixed: the host's letter case defeated the GitHub match, which
		// stored `Network("github.com:O/R")` and no label.
		{"git@GitHub.com:O/R.git", &expected{"github:o/r", "R", "O/R"}},
		// Fixed: a query was taken for part of the repository name.
		{"https://github.com/o/r?x=1", &expected{"git:https://github.com/o/r?x=1", "r?x=1", "https://github.com/o/r?x=1"}},
		// Fixed: a name GitHub cannot have was a GitHub identity.
		{"https://github.com/o/r$", &expected{"git:https://github.com/o/r$", "r$", "https://github.com/o/r$"}},
	}
	asExpected := func(source *ProjectSourceIdentity) *expected {
		if source == nil {
			return nil
		}
		return &expected{source.Key, source.Short, source.Full}
	}
	for _, row := range table {
		actual := asExpected(ProjectSourceIdentityGitRemote(row.input, ShorthandAllowed))
		if (actual == nil) != (row.want == nil) || (actual != nil && *actual != *row.want) {
			t.Fatalf("%q: got %+v, want %+v", row.input, actual, row.want)
		}
		// A remote read from `git config` cannot be shorthand.
		rejected := asExpected(ProjectSourceIdentityGitRemote(row.input, ShorthandRejected))
		shorthand := row.input == "owner/repo" || row.input == "owner/repo.git" || row.input == "mirror/repo.git"
		wantRejected := actual
		if shorthand {
			wantRejected = nil
		}
		if (rejected == nil) != (wantRejected == nil) || (rejected != nil && *rejected != *wantRejected) {
			t.Fatalf("%q rejected shorthand: got %+v, want %+v", row.input, rejected, wantRejected)
		}
	}
	for _, c := range []struct{ input, want string }{
		{"git@github.com:O/R.git", `{"github":["o","r"]}`},
		{"ssh://git@ssh.github.com:443/o/r.git", `{"network":"ssh://ssh.github.com:443/o/r"}`},
	} {
		encoded, err := json.Marshal(RepositoryIdentityFromRemote(c.input))
		if err != nil {
			t.Fatal(err)
		}
		if string(encoded) != c.want {
			t.Fatalf("%q encodes as %s", c.input, encoded)
		}
	}
}

func TestDuplicateRepositorySourcesCollapseToTheSingleRepositoryIdentity(t *testing.T) {
	config := stateSampleConfig()
	primary := config.Bundles["hel"].Repositories[0]
	duplicate := ProjectRepository{ID: "hel-copy", Github: cloneStringPointer(primary.Github), Destination: "hel-copy"}
	config.Bundles["duplicate"] = ProjectBundle{PrimaryRepo: primary.ID, Repositories: []ProjectRepository{primary, duplicate}}
	session := stateSampleSession()
	session.BundleID = "duplicate"

	want := ProjectSourceIdentityGitRemote("BrokkAi/hel", ShorthandAllowed)
	if want == nil || session.ProjectSource(&config) != *want {
		t.Fatalf("source = %+v, want %+v", session.ProjectSource(&config), want)
	}
}

func TestSessionsOrderByCreationTimeAndFallBackToTheID(t *testing.T) {
	older := stateSampleSession()
	newer := stateSampleSession()
	newer.ID = "0000000000000001"
	newer.CreatedAt = "2026-08-09T13:00:00Z"
	unparsable := stateSampleSession()
	unparsable.ID = "0000000000000002"
	unparsable.CreatedAt = "not a timestamp"
	sameTime := stateSampleSession()
	sameTime.ID = "zzzzzzzzzzzzzzzz"

	sessions := []*SessionRecord{&unparsable, &newer, &sameTime, &older}
	slices.SortStableFunc(sessions, func(left, right *SessionRecord) int { return left.CompareByCreation(right) })
	var ids []string
	for _, session := range sessions {
		ids = append(ids, session.ID)
	}
	want := []string{older.ID, sameTime.ID, newer.ID, unparsable.ID}
	if strings.Join(ids, ",") != strings.Join(want, ",") {
		t.Fatalf("order = %v, want %v", ids, want)
	}
}

// stateMutateSessionJSON encodes the sample state, edits the sample
// session's object, and returns the edited encoding.
func stateMutateSessionJSON(t *testing.T, edit func(session map[string]json.RawMessage)) []byte {
	t.Helper()
	encoded, err := json.Marshal(stateSampleState())
	if err != nil {
		t.Fatal(err)
	}
	var state map[string]json.RawMessage
	if err := json.Unmarshal(encoded, &state); err != nil {
		t.Fatal(err)
	}
	var sessions map[string]map[string]json.RawMessage
	if err := json.Unmarshal(state["sessions"], &sessions); err != nil {
		t.Fatal(err)
	}
	edit(sessions["0123456789abcdef"])
	state["sessions"], _ = json.Marshal(sessions)
	edited, _ := json.Marshal(state)
	return edited
}

func TestRetiredCheckpointAndDetachCursorNamesAreRejected(t *testing.T) {
	oldCheckpoint := stateMutateSessionJSON(t, func(session map[string]json.RawMessage) {
		var checkpoint map[string]json.RawMessage
		_ = json.Unmarshal(session["checkpoint"], &checkpoint)
		checkpoint["event_sequence"] = checkpoint["event_frontier"]
		delete(checkpoint, "event_frontier")
		session["checkpoint"], _ = json.Marshal(checkpoint)
	})
	var state StateState
	if json.Unmarshal(oldCheckpoint, &state) == nil {
		t.Fatal("a retired checkpoint field loaded")
	}

	oldDetachCursor := stateMutateSessionJSON(t, func(session map[string]json.RawMessage) {
		session["last_viewed_event_sequence"] = session["viewed_through_event_ordinal"]
		delete(session, "viewed_through_event_ordinal")
	})
	if json.Unmarshal(oldDetachCursor, &state) == nil {
		t.Fatal("a retired cursor field loaded")
	}
}

func TestDetachedCursorFieldLoadsAsTheViewedCursor(t *testing.T) {
	legacy := stateMutateSessionJSON(t, func(session map[string]json.RawMessage) {
		session["detached_after_event_ordinal"] = session["viewed_through_event_ordinal"]
		delete(session, "viewed_through_event_ordinal")
	})
	var loaded StateState
	if err := json.Unmarshal(legacy, &loaded); err != nil {
		t.Fatal(err)
	}
	sample := stateSampleState()
	if loaded.Sessions.Index("0123456789abcdef").ViewedThroughEventOrdinal !=
		sample.Sessions.Index("0123456789abcdef").ViewedThroughEventOrdinal {
		t.Fatal("cursor changed")
	}
}

func TestStateWrittenBeforeDraftsLoadsWithAnEmptyDraft(t *testing.T) {
	withoutDraft := stateMutateSessionJSON(t, func(session map[string]json.RawMessage) {
		delete(session, "draft_input")
	})
	var state StateState
	if err := json.Unmarshal(withoutDraft, &state); err != nil {
		t.Fatal(err)
	}
	if draft := state.Sessions.Index("0123456789abcdef").DraftInput; draft != "" {
		t.Fatalf("draft = %q", draft)
	}
}

func TestMountHistoryKeepsUniqueRecentSourcesPerHost(t *testing.T) {
	state := NewStateState()
	state.RememberMountSources("builder.example.test", []AdditionalMount{
		{Source: "/srv/first", Destination: "/mnt/first", Access: MountAccessCow},
		{Source: "/srv/second", Destination: "/mnt/second", Access: MountAccessCow},
	})
	state.RememberMountSources("builder.example.test", []AdditionalMount{
		{Source: "/srv/first", Destination: "/mnt/again", Access: MountAccessCow},
	})
	if got := *state.MountHistory.Index("builder.example.test"); strings.Join(got, ",") != "/srv/first,/srv/second" {
		t.Fatalf("history = %v", got)
	}
}

func TestMaterializedActivityWatermarkDoesNotRegressWhenDetailIsRemoved(t *testing.T) {
	materialized := MaterializedSessionEmpty("session-1")
	if materialized.GetLastActivityAtMs() != nil {
		t.Fatal("an empty session has activity")
	}
	materialized.Execution = MaterializedExecutionStateRunning{StartedAtMs: 300}
	materialized.Transcript = append(materialized.Transcript, &TranscriptItem{
		StableID:        "system:1",
		Position:        1,
		CreatedAtMs:     350,
		LastChangedAtMs: 400,
		Body:            TranscriptBodySystem{Text: "working"},
	})
	materialized.QueuedPrompts = append(materialized.QueuedPrompts, MaterializedQueuedPrompt{
		CommandID:  "prompt-2",
		Kind:       QueuedCommandKindPrompt{},
		Content:    []any{},
		QueuedAtMs: 500,
	})
	watermark := int64(500)
	materialized.LastActivityAtMs = &watermark
	check := func() {
		t.Helper()
		if got := materialized.GetLastActivityAtMs(); got == nil || *got != 500 {
			t.Fatalf("activity = %v", got)
		}
	}
	check()
	materialized.QueuedPrompts = nil
	check()
	materialized.Transcript = nil
	check()
	materialized.Execution = MaterializedExecutionStateIdle{}
	check()
}

func TestMaterializedEventFrontierRequiresTheMatchingDigestKind(t *testing.T) {
	materialized := MaterializedSessionEmpty("session-1")
	if err := materialized.Validate(); err != nil {
		t.Fatal(err)
	}
	materialized.AppliedEventOrdinal = 1
	if err := materialized.Validate(); err == nil || !strings.Contains(err.Error(), "inconsistent ordinal") {
		t.Fatalf("err = %v", err)
	}
	materialized.AppliedEventDigest = strings.Repeat("A", 64)
	if err := materialized.Validate(); err == nil || !strings.Contains(err.Error(), "lowercase SHA-256") {
		t.Fatalf("err = %v", err)
	}
}

func TestProjectDirectoryHistoryIsRecentAndIsolatedPerRemoteHost(t *testing.T) {
	state := NewStateState()
	state.RememberProjectDirectory("builder-a", "/srv/one")
	state.RememberProjectDirectory("builder-a", "/srv/two")
	state.RememberProjectDirectory("builder-a", "/srv/one")
	state.RememberProjectDirectory("builder-b", "/work/other")
	if got := state.ProjectDirectories("builder-a"); strings.Join(got, ",") != "/srv/one,/srv/two" {
		t.Fatalf("builder-a = %v", got)
	}
	if got := state.ProjectDirectories("builder-b"); strings.Join(got, ",") != "/work/other" {
		t.Fatalf("builder-b = %v", got)
	}
}

// stateCloneConfig is Config::clone for the maps these tests edit.
func stateCloneConfig(config Config) Config {
	clone := config
	clone.Profiles = maps.Clone(config.Profiles)
	clone.Bundles = maps.Clone(config.Bundles)
	clone.Targets = maps.Clone(config.Targets)
	return clone
}

// A refused Setup change names the running session and what it uses the
// way the screen names them, and only what the change touches. Launch
// campaign finding C-20.
// Hard-won: 006bb14c: Setup refusal text exposed an internal session id and bundle key instead of display names.
func TestARefusedSetupChangeNamesTheSessionAndSettingByDisplayName(t *testing.T) {
	state := stateSampleState()
	before := stateSampleConfig()
	session := state.Sessions.FirstValueMut()
	session.SessionTitleOverride = stateStr("Fix the login page")
	project := session.ProjectName(&before)
	after := stateCloneConfig(before)
	delete(after.Bundles, "hel")
	err := state.ValidateSetupUpdate(&before, &after)
	if err == nil {
		t.Fatal("setup change accepted")
	}
	text := err.Error()
	if !strings.Contains(text, `"Fix the login page"`) || !strings.Contains(text, `project "`+project+`"`) {
		t.Fatal(text)
	}
	for _, internal := range []string{"0123456789abcdef", "codex-1", "podman"} {
		if strings.Contains(text, internal) {
			t.Fatal(text)
		}
	}
}

// A project stays while any session would read it again on resume, and
// only sessions that no longer need it let it go.
func TestBundleRemovalIsRefusedWhileASessionStillResumesFromIt(t *testing.T) {
	state := stateSampleState()
	session := state.Sessions.FirstValueMut()
	session.State = SessionStateStopped
	session.SessionTitleOverride = stateStr("Fix the login page")
	refusal, ok := state.BundleRemovalRefusal("hel")
	if !ok || !strings.Contains(refusal, `"hel" is used by a session: "Fix the login page"`) {
		t.Fatal(refusal)
	}
	if refusal, ok := state.BundleRemovalRefusal("other"); ok {
		t.Fatal(refusal)
	}
	for _, release := range []func(*SessionRecord){
		func(session *SessionRecord) { session.ProjectDirectory = stateStr("/work/hel") },
		func(session *SessionRecord) { session.State = SessionStateDestroyedWithDataLoss },
	} {
		released := state
		release(released.Sessions.FirstValueMut())
		if refusal, ok := released.BundleRemovalRefusal("hel"); ok {
			t.Fatal(refusal)
		}
	}
}

func TestSetupProtectsActiveDependenciesButAllowsAdditionsRepairsAndDefaults(t *testing.T) {
	state := stateSampleState()
	before := stateSampleConfig()
	session := state.Sessions.Index("0123456789abcdef")
	for _, section := range []string{"profile", "bundle", "target"} {
		after := stateCloneConfig(before)
		switch section {
		case "profile":
			delete(after.Profiles, session.LastProfile)
		case "bundle":
			delete(after.Bundles, session.BundleID)
		default:
			delete(after.Targets, session.TargetTemplateID)
		}
		if err := state.ValidateSetupUpdate(&before, &after); err == nil || !strings.Contains(err.Error(), "running session") {
			t.Fatalf("%s: %v", section, err)
		}
		// Restoring a removed entry is always permitted.
		if err := state.ValidateSetupUpdate(&after, &before); err != nil {
			t.Fatalf("%s restore: %v", section, err)
		}
	}
	after := stateCloneConfig(before)
	profile := after.Profiles[session.LastProfile]
	profile.Home = "/new/home"
	after.Profiles[session.LastProfile] = profile
	if state.ValidateSetupUpdate(&before, &after) == nil {
		t.Fatal("a changed profile home was accepted")
	}
	after = stateCloneConfig(before)
	profile = after.Profiles[session.LastProfile]
	profile.Enabled = false
	after.Profiles[session.LastProfile] = profile
	after.Advanced.DetailedActivityClocks = !before.Advanced.DetailedActivityClocks
	after.Targets["alternative"] = TargetTemplateLocalBare{}
	if err := state.ValidateSetupUpdate(&before, &after); err != nil {
		t.Fatal(err)
	}
	// Build cache settings are read when a session is provisioned, so
	// editing them is allowed while that session runs.
	after = stateCloneConfig(before)
	podman, ok := after.Targets[session.TargetTemplateID].(TargetTemplateLocalPodman)
	if !ok {
		t.Fatal("sample session uses a podman target")
	}
	podman.ContainerTemplate.BuildCache = &TargetBuildCache{MaxTotalSize: stateStr("20GB")}
	after.Targets[session.TargetTemplateID] = podman
	if err := state.ValidateSetupUpdate(&before, &after); err != nil {
		t.Fatal(err)
	}
	// Any other container change is still refused.
	after = stateCloneConfig(before)
	podman = after.Targets[session.TargetTemplateID].(TargetTemplateLocalPodman)
	podman.ContainerTemplate.Image = "ubuntu:22.04"
	after.Targets[session.TargetTemplateID] = podman
	if state.ValidateSetupUpdate(&before, &after) == nil {
		t.Fatal("a changed image was accepted")
	}
	stopped := state
	stopped.Sessions.FirstValueMut().State = SessionStateStopped
	defaults := DefaultConfig()
	if err := stopped.ValidateSetupUpdate(&before, &defaults); err != nil {
		t.Fatal(err)
	}
}

func TestConfigurationRepairReportsAllMissingEntriesAndClearsAfterRestoration(t *testing.T) {
	state := stateSampleState()
	session := state.Sessions.Index("0123456789abcdef")
	config := stateSampleConfig()
	config.Profiles = map[string]HarnessProfile{}
	config.Bundles = map[string]ProjectBundle{}
	config.Targets = map[string]TargetTemplate{}
	issue, ok := session.ConfigurationIssue(&config)
	if !ok {
		t.Fatal("no issue")
	}
	for _, part := range []string{"missing profile", "missing bundle", "missing target template", "config.toml"} {
		if !strings.Contains(issue, part) {
			t.Fatalf("%q lacks %q", issue, part)
		}
	}
	sample := stateSampleConfig()
	if issue, ok := session.ConfigurationIssue(&sample); ok {
		t.Fatal(issue)
	}
	raw := *session
	raw.ProjectDirectory = stateStr("/project")
	withoutBundles := stateSampleConfig()
	withoutBundles.Bundles = map[string]ProjectBundle{}
	if issue, ok := raw.ConfigurationIssue(&withoutBundles); ok {
		t.Fatal(issue)
	}
	stopped := *session
	stopped.State = SessionStateStopped
	defaults := DefaultConfig()
	if issue, ok := stopped.ConfigurationIssue(&defaults); ok {
		t.Fatal(issue)
	}
}

func TestActiveStateValidatesReferencesAndHarnessKind(t *testing.T) {
	state := stateSampleState()
	config := stateSampleConfig()
	if err := state.ValidateAgainstConfig(&config); err != nil {
		t.Fatal(err)
	}
	profile := config.Profiles["codex-1"]
	profile.Kind = HarnessKindClaude
	config.Profiles["codex-1"] = profile
	if err := state.ValidateAgainstConfig(&config); err == nil || !strings.Contains(err.Error(), "expects Codex") {
		t.Fatalf("err = %v", err)
	}
}

// A child's working directory is a launch choice, not a containment
// boundary, so a stored record may name any path on the parent's target.
func TestAStoredSubagentMayLaunchOutsideTheParentWorkspace(t *testing.T) {
	state := stateSampleState()
	parentID := "0123456789abcdef"
	childID := "fedcba9876543210"
	child := *state.Sessions.Index(parentID)
	child.ID = childID
	state.Sessions.Insert(childID, child)
	state.Subagents.Insert(childID, SubagentMetadata{
		Kind:            SubagentKindWorker,
		ChildSessionID:  childID,
		ParentSessionID: parentID,
		TaskName:        "lane",
		ProfileID:       "codex-1",
		RequestKey:      "request-1",
		CreatedAt:       "2026-09-16T00:00:00Z",
	})
	for _, workingDirectory := range []string{"/mnt/optane/bifrost-sg-c2", "../shared-checkout"} {
		state.Subagents.GetMut(childID).WorkingDirectory = workingDirectory
		if err := state.Validate(); err != nil {
			t.Fatal(err)
		}
	}
}

// Records written before the verb was renamed say "archived". They must
// still load, and they must be written back with the new name.
func TestTheStoppedStateReadsTheRetiredArchivedNameAndWritesTheNewOne(t *testing.T) {
	for _, text := range []string{`"archived"`, `"stopped"`} {
		var state SessionState
		if err := json.Unmarshal([]byte(text), &state); err != nil || state != SessionStateStopped {
			t.Fatalf("%s = %v, %v", text, state, err)
		}
	}
	if encoded, _ := json.Marshal(SessionStateStopped); string(encoded) != `"stopped"` {
		t.Fatalf("stopped encodes as %s", encoded)
	}
	if SessionStateStopped.IsActive() {
		t.Fatal("stopped is active")
	}
}

// The archived flag is a later addition, so records written without it
// load as visible and stay out of the serialized form until it is set.
func TestTheArchivedFlagDefaultsOffAndIsOmittedWhenItIsOff(t *testing.T) {
	state := stateSampleState()
	session := state.Sessions.FirstValueMut()
	if session.Archived {
		t.Fatal("archived by default")
	}
	encoded, _ := json.Marshal(session)
	if strings.Contains(string(encoded), "archived") {
		t.Fatal(string(encoded))
	}
	session.Archived = true
	encoded, _ = json.Marshal(session)
	if !strings.Contains(string(encoded), `"archived":true`) {
		t.Fatal(string(encoded))
	}
	var loaded SessionRecord
	if err := json.Unmarshal(encoded, &loaded); err != nil || !loaded.Archived {
		t.Fatalf("loaded archived = %v, %v", loaded.Archived, err)
	}
}

func TestStoppedSessionDoesNotPinRenamedConfigEntries(t *testing.T) {
	state := stateSampleState()
	state.Sessions.FirstValueMut().State = SessionStateStopped
	defaults := DefaultConfig()
	if err := state.ValidateAgainstConfig(&defaults); err != nil {
		t.Fatal(err)
	}
}

func TestOnlyInactiveSessionsCanBeRemovedFromTheArchive(t *testing.T) {
	state := stateSampleState()
	if _, err := state.DestroyStoppedSession("0123456789abcdef"); err == nil || !strings.Contains(err.Error(), "active session") {
		t.Fatalf("err = %v", err)
	}
	if !state.Sessions.ContainsKey("0123456789abcdef") {
		t.Fatal("the active session was removed")
	}
	state.Sessions.FirstValueMut().State = SessionStateStopped
	removed, err := state.DestroyStoppedSession("0123456789abcdef")
	if err != nil || removed.ID != "0123456789abcdef" || !state.Sessions.IsEmpty() {
		t.Fatalf("removed %q, %v", removed.ID, err)
	}
}

func TestForceRemovalPermitsAnActiveSession(t *testing.T) {
	state := stateSampleState()
	removed, err := state.DestroySessionForce("0123456789abcdef")
	if err != nil || removed.ID != "0123456789abcdef" || !state.Sessions.IsEmpty() {
		t.Fatalf("removed %q, %v", removed.ID, err)
	}
	if _, err := state.DestroySessionForce("0123456789abcdef"); err == nil || !strings.Contains(err.Error(), "unknown session") {
		t.Fatalf("err = %v", err)
	}
}

func TestFirstPromptIsNotUsedAsAnACPSessionTitle(t *testing.T) {
	events := []SequencedEvent{{
		Seq:       1,
		RequestID: stateStr("prompt-1"),
		Event: WorkerEventPromptAccepted{
			RequestID:   "prompt-1",
			Text:        "Do not use me as a title",
			Attachments: []Attachment{},
		},
	}}
	if title, ok := HarnessSessionTitle(events); ok {
		t.Fatal(title)
	}
}

func TestSessionTitlesAreBoundedByCharactersAtWordBoundaries(t *testing.T) {
	if title, ok := NormalizeSessionTitle(" \n\t "); ok {
		t.Fatal(title)
	}
	if title, _ := NormalizeSessionTitle("  short\n title \t"); title != "short title" {
		t.Fatal(title)
	}
	exact := strings.Repeat("界", 256)
	if title, _ := NormalizeSessionTitle(exact); title != exact {
		t.Fatal(title)
	}
	if title, _ := NormalizeSessionTitle(strings.Repeat("界", 257)); title != strings.Repeat("界", 255)+"…" {
		t.Fatal(title)
	}
	words := strings.Repeat("word ", 20_000)
	if len(words) != 100_000 {
		t.Fatal(len(words))
	}
	if title, _ := NormalizeSessionTitle(words); title != strings.Repeat("word ", 50)+"word…" {
		t.Fatal(title)
	}
}

// An agent sets its own title, and `mj sessions` prints stored titles to the
// operator's terminal, so a title must not carry a working escape sequence.
func TestSessionTitlesDropTerminalControlsAnAgentSupplies(t *testing.T) {
	clipboardWrite := "pwn\x1b]52;c;Y3VybCBldmlsfHNo\x07 build\x1b[31m red\x1b\\\u009b2J"
	if title, _ := NormalizeSessionTitle(clipboardWrite); title != "pwn build red2J" {
		t.Fatalf("%q", title)
	}
	if title, ok := NormalizeSessionTitle("\x1b]0;only a title\x07"); ok {
		t.Fatalf("%q", title)
	}
}

func TestProvisionalTitleIsCleanedAndBounded(t *testing.T) {
	if title, _ := ProvisionalSessionTitle("<mj-project-memory>private</mj-project-memory> " +
		"  fix the flaky\nresume test  "); title != "fix the flaky resume test" {
		t.Fatalf("%q", title)
	}
	prompt := strings.Repeat("word ", 20) + "overflow"
	if title, _ := ProvisionalSessionTitle(prompt); title != strings.Repeat("word ", 11)+"word…" {
		t.Fatalf("%q", title)
	}
}

func stateTitledEvent(title string) SequencedEvent {
	payload, _ := json.Marshal(map[string]any{
		"type": "session_update",
		"update": map[string]any{
			"sessionUpdate": "session_title",
			"title":         title,
		},
	})
	return SequencedEvent{Seq: 1, Event: WorkerEventAdapter{Kind: "session_update", Payload: payload}}
}

func TestHarnessTitleElidesHiddenContextInsteadOfNamingTheSessionFromIt(t *testing.T) {
	events := []SequencedEvent{stateTitledEvent("<mj-project-memory>private</mj-project-memory> " + "Visible session name")}
	if title, _ := HarnessSessionTitle(events); title != "Visible session name" {
		t.Fatalf("%q", title)
	}
	if title, ok := HarnessSessionTitle([]SequencedEvent{stateTitledEvent("<mj-project-memory>truncated")}); ok {
		t.Fatalf("%q", title)
	}
}

func TestHarnessTitlesAreNormalizedToOneCompleteLine(t *testing.T) {
	events := []SequencedEvent{stateTitledEvent("first\nsecond\tthird fourth fifth sixth seventh eighth ninth tenth eleventh twelfth thirteenth")}
	if title, _ := HarnessSessionTitle(events); title != "first second third fourth fifth sixth seventh eighth ninth tenth eleventh twelfth thirteenth" {
		t.Fatalf("%q", title)
	}
}

func TestLocatorRejectsParentTraversal(t *testing.T) {
	state := stateSampleState()
	state.Sessions.FirstValueMut().Target = TargetLocatorSshBare{Host: "builder", Workspace: "~/hel/../other"}
	if err := state.Validate(); err == nil || !strings.Contains(err.Error(), "safe path ending") {
		t.Fatalf("err = %v", err)
	}
}

// A sub-agent child on a bare SSH target runs its own worker in the parent's
// workspace (borrowed_locator), so its workspace ends in the parent's id and
// its worker_id names the child.
// Hard-won: 70c93757: A valid borrowed child locator stopped the DB writer and made later daemon starts reject the store.
func TestBorrowedSshBareLocatorValidatesByItsWorkerIdentity(t *testing.T) {
	state := stateSampleState()
	session := state.Sessions.FirstValueMut()
	childID := session.ID
	borrowed := func(workerID, workspace string) TargetLocator {
		return TargetLocatorSshBare{Host: "builder", Workspace: workspace, WorkerID: stateStr(workerID)}
	}

	session.Target = borrowed(childID, ".local/share/hel/workspaces/parent-session")
	if err := state.Validate(); err != nil {
		t.Fatal(err)
	}

	state.Sessions.FirstValueMut().Target = borrowed("someone-else", ".local/share/hel/workspaces/parent-session")
	if err := state.Validate(); err == nil || !strings.Contains(err.Error(), "worker identity") {
		t.Fatalf("err = %v", err)
	}

	state.Sessions.FirstValueMut().Target = borrowed(childID, ".local/share/hel/../parent-session")
	if state.Validate() == nil {
		t.Fatal("a traversing workspace validated")
	}
}

func TestAwaitingInputIsAHandoffInsteadOfAnError(t *testing.T) {
	for _, reason := range []string{AwaitingInputStopReason, "AwaitingInput", "awaiting-input"} {
		if ClassifyPromptCompletion(reason) != PromptCompletionInputRequired {
			t.Fatal(reason)
		}
	}
	if ClassifyPromptCompletion("harness_inactive") != PromptCompletionError {
		t.Fatal("harness_inactive")
	}
}

// Launch finding R2-8: the dashboard titled a session with the full project
// path and its trailing slash ("/tmp/lab/project/ via fake"), while `mj new`
// titled it "project via fake". Both now use this.
// Hard-won: e7eb288e: Dashboard creation used a trailing-slash path title that differed from the CLI title.
func TestANewSessionIsTitledByItsDirectoryNameAndProfile(t *testing.T) {
	if title := DefaultSessionTitle(stateStr("/tmp/lab/project/"), "fixture", "fake"); title != "project via fake" {
		t.Fatal(title)
	}
	if title := DefaultSessionTitle(nil, "fixture", "fake"); title != "fixture via fake" {
		t.Fatal(title)
	}
}

// R2-8: until the harness names a dashboard session, its row showed the hex
// id. It shows the title the session was created with, as the API has since
// F-12; a rename or a harness title still wins.
// Hard-won: e7eb288e: Unnamed dashboard rows showed a hex id until the harness supplied a title.
func TestASessionNobodyNamedIsListedByTheTitleItWasCreatedWith(t *testing.T) {
	session := stateSampleSession()
	session.Title = "project via fake"
	session.ACPSessionTitle = nil
	session.SessionTitleOverride = nil
	if session.ListedTitle() != "project via fake" {
		t.Fatal(session.ListedTitle())
	}
	session.ACPSessionTitle = stateStr("Harness name")
	if session.ListedTitle() != "Harness name" {
		t.Fatal(session.ListedTitle())
	}
	session.SessionTitleOverride = stateStr("My name")
	if session.ListedTitle() != "My name" {
		t.Fatal(session.ListedTitle())
	}
	// A name that happens to equal the id is still a name.
	session.SessionTitleOverride = stateStr(session.ID)
	if session.ListedTitle() != session.ID {
		t.Fatal(session.ListedTitle())
	}
}

// A name a person typed beats one the harness generated, and neither is
// needed for the columns-only helper to agree with the record.
func TestATypedNameBeatsAHarnessNameAndTheColumnHelperAgrees(t *testing.T) {
	session := stateSampleSession()
	session.Title = "created with"
	session.SessionTitleOverride = nil
	session.ACPSessionTitle = nil
	if session.HasGivenName() {
		t.Fatal("given name without one")
	}
	if name, ok := session.GivenName(); ok {
		t.Fatal(name)
	}
	session.ACPSessionTitle = stateStr("Harness name")
	if name, ok := session.GivenName(); !ok || !session.HasGivenName() || name != "Harness name" {
		t.Fatal(name)
	}
	session.SessionTitleOverride = stateStr("Typed name")
	if name, _ := session.GivenName(); name != "Typed name" || session.ListedTitle() != "Typed name" {
		t.Fatal(name)
	}
	for _, c := range []struct{ titleOverride, acp *string }{
		{nil, nil},
		{nil, stateStr("Harness name")},
		{stateStr("Typed name"), nil},
		{stateStr("Typed name"), stateStr("Harness name")},
	} {
		session.SessionTitleOverride = c.titleOverride
		session.ACPSessionTitle = c.acp
		if ListedSessionTitle(session.ID, session.Title, c.titleOverride, c.acp) != session.ListedTitle() {
			t.Fatal(session.ListedTitle())
		}
	}
	if title := ListedSessionTitle("the-id", "  ", nil, nil); title != "the-id" {
		t.Fatal(title)
	}
}

func TestAMessageNamesASessionOnOneBoundedLine(t *testing.T) {
	session := stateSampleSession()
	session.SessionTitleOverride = nil
	session.ACPSessionTitle = nil
	session.Title = "Fix the parser\n\nThen the long tail of the prompt follows here, in paragraphs, until it runs well past the limit for a name in a message header."
	name := session.MessageName()
	if strings.Contains(name, "\n") || !strings.HasPrefix(name, "Fix the parser Then the long tail") ||
		!strings.HasSuffix(name, "…") || len([]rune(name)) > MessageNameChars {
		t.Fatalf("%q", name)
	}
	session.SessionTitleOverride = stateStr("Renamed")
	if session.MessageName() != "Renamed" {
		t.Fatal(session.MessageName())
	}
	// The creation title is stored as typed, so a terminal control in it
	// must not reach a message header.
	session.SessionTitleOverride = nil
	session.Title = "Copy \u001b]52;c;QQ==\u0007this"
	name = session.MessageName()
	if strings.ContainsFunc(name, unicode.IsControl) || !strings.HasPrefix(name, "Copy ") {
		t.Fatalf("%q", name)
	}
}

func TestActiveSessionsAllowCreationDefaultsButProtectTheirProfileInstallation(t *testing.T) {
	state := stateSampleState()
	before := stateSampleConfig()
	profileID := state.Sessions.Index("0123456789abcdef").LastProfile
	after := stateCloneConfig(before)
	after.Subagents.Straightforward = []SubagentChoice{{Model: "chosen"}}
	if err := state.ValidateSetupUpdate(&before, &after); err != nil {
		t.Fatal(err)
	}
	for _, field := range []string{"harness", "home", "environment", "remove"} {
		changed := stateCloneConfig(after)
		profile := changed.Profiles[profileID]
		switch field {
		case "harness":
			profile.Kind = HarnessKindClaude
		case "home":
			profile.Home = "/different/home"
		case "environment":
			profile.Environment = EnvironmentFromMap(map[string]string{"NEW": "value"})
		}
		if field == "remove" {
			delete(changed.Profiles, profileID)
		} else {
			changed.Profiles[profileID] = profile
		}
		if state.ValidateSetupUpdate(&before, &changed) == nil {
			t.Fatal(field)
		}
	}
}

// statePolicyEqual is SubagentPolicy equality, where the zero mode is the
// default, native.
func statePolicyEqual(a, b SubagentPolicy) bool {
	if a.Mode == "" {
		a.Mode = SubagentPolicyNative
	}
	if b.Mode == "" {
		b.Mode = SubagentPolicyNative
	}
	return reflect.DeepEqual(a, b)
}

func TestSessionAndRememberedPolicyReadLegacySelectorAndBooleanJSON(t *testing.T) {
	both := &SubagentChoice{Model: "gpt-6-luna", Effort: stateStr("max")}
	mjolnir := SubagentPolicy{Mode: SubagentPolicyMjolnir}
	for _, c := range []struct {
		policy   string
		expected SubagentPolicy
	}{
		{`{"mode":"mjolnir", "model":"gpt-6-luna", "effort":"max"}`, SubagentPolicy{Mode: SubagentPolicyMjolnir, Straightforward: both, Subtle: both}},
		{`{"mode":"all_models"}`, mjolnir},
		{`{"mode":"native"}`, SubagentPolicy{Mode: SubagentPolicyNative}},
		{`{"mode":"none"}`, SubagentPolicy{Mode: SubagentPolicyNone}},
		{`{"mode":"single_model", "model":"chosen", "effort":"high"}`, mjolnir},
		{`{"mode":"single_model", "model":"chosen", "effort":"adaptive"}`, mjolnir},
		{`true`, mjolnir},
		{`false`, SubagentPolicy{Mode: SubagentPolicyNative}},
	} {
		session := stateSampleSession()
		encoded, _ := json.Marshal(session)
		var object map[string]json.RawMessage
		_ = json.Unmarshal(encoded, &object)
		object["subagents"] = json.RawMessage(c.policy)
		encoded, _ = json.Marshal(object)
		var loaded SessionRecord
		if err := json.Unmarshal(encoded, &loaded); err != nil {
			t.Fatalf("%s: %v", c.policy, err)
		}
		if loaded.Subagents == nil || !statePolicyEqual(*loaded.Subagents, c.expected) {
			t.Fatalf("%s: session policy = %+v", c.policy, loaded.Subagents)
		}

		encoded, _ = json.Marshal(stateSampleState())
		var stateObject map[string]json.RawMessage
		_ = json.Unmarshal(encoded, &stateObject)
		delete(stateObject, "last_subagent_policies")
		stateObject["last_subagent_policy"] = json.RawMessage(c.policy)
		encoded, _ = json.Marshal(stateObject)
		var state StateState
		if err := json.Unmarshal(encoded, &state); err != nil {
			t.Fatalf("%s: %v", c.policy, err)
		}
		if got := state.LastSubagentPolicyFor(""); !statePolicyEqual(got, c.expected) {
			t.Fatalf("%s: remembered policy = %+v", c.policy, got)
		}
	}
	for _, value := range []string{
		`{"version":` + itoa(uint64(StateVersion)) + `}`,
		`{"version":` + itoa(uint64(StateVersion)) + `,"last_subagent_policy":null}`,
	} {
		var state StateState
		if err := json.Unmarshal([]byte(value), &state); err != nil {
			t.Fatalf("%s: %v", value, err)
		}
		if got := state.LastSubagentPolicyFor("profile"); !statePolicyEqual(got, SubagentPolicy{}) {
			t.Fatalf("%s: %+v", value, got)
		}
	}
}

func TestOracleStateReadsOldSessionsEnabledAndOmitsEmptyPreferences(t *testing.T) {
	encoded, _ := json.Marshal(stateSampleSession())
	var object map[string]json.RawMessage
	_ = json.Unmarshal(encoded, &object)
	delete(object, "oracle")
	delete(object, "oracle_choice")
	encoded, _ = json.Marshal(object)
	var restored SessionRecord
	if err := json.Unmarshal(encoded, &restored); err != nil {
		t.Fatal(err)
	}
	if !restored.Oracle || restored.OracleChoice != nil {
		t.Fatalf("oracle = %v, choice = %v", restored.Oracle, restored.OracleChoice)
	}
	encoded, _ = json.Marshal(NewStateState())
	if strings.Contains(string(encoded), "last_oracle_choices") {
		t.Fatal(string(encoded))
	}
}

func TestRememberedSubagentPolicyReadsLegacyPolicyObjectsAndProfileMaps(t *testing.T) {
	var old StateState
	if err := json.Unmarshal([]byte(`{
		"version": `+itoa(uint64(StateVersion))+`,
		"last_subagent_policy": {
			"mode": "mjolnir",
			"straightforward": {"model": "fast", "effort": "low"}
		}
	}`), &old); err != nil {
		t.Fatal(err)
	}
	if !statePolicyEqual(old.LastSubagentPolicyFor("profile-a"), old.LastSubagentPolicyFor("")) {
		t.Fatal("the legacy global policy is not the fallback")
	}
	if policy := old.LastSubagentPolicies[""]; !policy.UsesMjolnir() {
		t.Fatalf("%+v", policy)
	}

	var mapped StateState
	if err := json.Unmarshal([]byte(`{
		"version": `+itoa(uint64(StateVersion))+`,
		"last_subagent_policies": {
			"mode": {"mode": "none"},
			"profile-b": {"mode": "native"}
		}
	}`), &mapped); err != nil {
		t.Fatal(err)
	}
	if !statePolicyEqual(mapped.LastSubagentPolicyFor("mode"), SubagentPolicy{Mode: SubagentPolicyNone}) {
		t.Fatal("mode")
	}
	if !statePolicyEqual(mapped.LastSubagentPolicyFor("profile-b"), SubagentPolicy{Mode: SubagentPolicyNative}) {
		t.Fatal("profile-b")
	}
	if !statePolicyEqual(mapped.LastSubagentPolicyFor("unknown"), SubagentPolicy{}) {
		t.Fatal("unknown")
	}
}

func TestOperationOwnershipHidesIntermediateMoveStatesButNotOrdinaryLiveWork(t *testing.T) {
	moving := SessionTransitionKindMoving
	for _, state := range []SessionState{SessionStateStopped, SessionStateRunning, SessionStateDisconnected} {
		if kind, ok := SessionTransitionKindForSession(state, &moving); !ok || kind != SessionTransitionKindMoving {
			t.Errorf("%s with a move: got %q %v", state, kind, ok)
		}
		if kind, ok := SessionTransitionKindForSession(state, nil); ok {
			t.Errorf("%s without an operation: got %q", state, kind)
		}
	}
	if kind, ok := SessionStateCheckpointing.TransitionKind(); ok {
		t.Errorf("checkpointing: got %q", kind)
	}
	if kind, ok := SessionStateClosing.TransitionKind(); !ok || kind != SessionTransitionKindSuspending {
		t.Errorf("closing: got %q %v", kind, ok)
	}
}

func TestResolveDefaultContainerSizePreservesPrecedenceAndClamping(t *testing.T) {
	size := func(cpus, memoryGiB uint64) HostContainerSize {
		return HostContainerSize{Cpus: cpus, MemoryBytes: memoryGiB << 30}
	}
	if got := ResolveDefaultContainerSize(nil, nil, nil); got != size(4, 8) {
		t.Fatalf("baseline %+v", got)
	}

	var template ContainerTemplate
	if err := json.Unmarshal([]byte(`{"cpus":"16","memory":"32768m"}`), &template); err != nil {
		t.Fatal(err)
	}
	configured := template.DefaultHostContainerSize()
	if got := ResolveDefaultContainerSize(configured, nil, nil); got != size(16, 32) {
		t.Fatalf("configured %+v", got)
	}
	remembered, limits := size(6, 64), size(4, 48)
	if got := ResolveDefaultContainerSize(configured, &remembered, &limits); got != size(4, 48) {
		t.Fatalf("remembered and clamped %+v", got)
	}
	zero := size(0, 0)
	if got := ResolveDefaultContainerSize(configured, nil, &zero); got != (HostContainerSize{Cpus: 1, MemoryBytes: 1}) {
		t.Fatalf("zero limits %+v", got)
	}
	if got, err := ParseContainerMemory("16Gi"); err != nil || got != 16<<30 {
		t.Fatalf("16Gi = %d, %v", got, err)
	}
	if got, err := ParseContainerMemory("8g"); err != nil || got != 8<<30 {
		t.Fatalf("8g = %d, %v", got, err)
	}
	if _, err := ParseContainerMemory("18446744073709551615g"); err == nil {
		t.Fatal("an overflowing limit parsed")
	}
}
