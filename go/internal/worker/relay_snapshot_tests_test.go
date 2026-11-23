// Port of mj-worker/src/relay/snapshot_tests.rs.

package worker

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func snapshotTestDecode[T any](t *testing.T, encoded string) T {
	t.Helper()
	var value T
	if err := json.Unmarshal([]byte(encoded), &value); err != nil {
		t.Fatalf("decode %T from %s: %v", value, encoded, err)
	}
	return value
}

func snapshotTestThinkingOption(t *testing.T) acpschema.SessionConfigOption {
	t.Helper()
	return snapshotTestDecode[acpschema.SessionConfigOption](t,
		`{"id":"thinking","name":"Thinking","type":"select","currentValue":"on","options":[{"value":"on","name":"On"}]}`)
}

func snapshotTestState(t *testing.T) core.RelayOperationalState {
	t.Helper()
	snapshot := core.NewRelaySnapshot(session)
	return snapshot.OperationalState()
}

// Every way a session can still be holding work, each on its own, plus the
// one state in which replacing its worker destroys nothing.
func TestASessionIsQuietOnlyWhenNothingItOwnsIsInFlight(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	quiet := relay.OperationalState()
	if quiet.IsQuiet() {
		t.Fatal("startup is still in flight")
	}
	initializing := quiet
	notReady := false
	initializing.ACPReady = &notReady
	if initializing.IsQuiet() {
		t.Fatal("ACP initialization is still in flight")
	}
	ready := true
	quiet.ACPReady = &ready
	if !quiet.IsQuiet() {
		t.Fatalf("a ready idle relay owns nothing: %+v", quiet)
	}

	one := int64(1)
	busy := []struct {
		reason string
		make   func(state *core.RelayOperationalState)
	}{
		{"a prompt is running", func(state *core.RelayOperationalState) {
			state.Execution = core.RelayExecutionStateRunning
		}},
		{"a prompt is in flight", func(state *core.RelayOperationalState) {
			state.ActivePrompt = &core.ActiveRelayPrompt{CommandID: "prompt-1", CreatedAtMs: 1, StartedAtMs: 2}
		}},
		{"the harness started a turn of its own", func(state *core.RelayOperationalState) {
			state.HarnessTurn = &core.HarnessTurn{StartedAtMs: 1}
		}},
		{"a prompt is queued behind the current one", func(state *core.RelayOperationalState) {
			state.QueuedPrompts = []core.QueuedRelayPrompt{{CommandID: "prompt-2", CreatedAtMs: 1}}
		}},
		{"a user shell is open", func(state *core.RelayOperationalState) {
			state.ActiveUserShells = []core.ActiveUserShell{{CommandID: "shell-1", Command: "top", CreatedAtMs: 1, StartedAtMs: &one}}
		}},
		{"an agent terminal is live", func(state *core.RelayOperationalState) {
			state.ActiveAgentTerminals = []core.ActiveAgentTerminal{{TerminalID: "term-1", Command: "sleep 600", StartedAtMs: 1}}
		}},
		{"the agent left a command running", func(state *core.RelayOperationalState) {
			state.BackgroundCommands = []core.BackgroundCommand{{ID: "task-1", StartedAtMs: 1, Command: "sleep 600", CanStop: false}}
		}},
		{"a foreground tool is still in progress", func(state *core.RelayOperationalState) {
			state.ToolsInFlight = []core.InFlightToolCall{{
				ToolCallID:  "tool-1",
				Status:      acpschema.ToolCallStatusInProgress,
				StartedAtMs: 1,
			}}
			state.ForegroundToolStartedAtMs = &one
		}},
		// A worker too old to list its open tool calls reports only the
		// newest one's start, and that still means work.
		{"an older worker reports only a foreground tool start", func(state *core.RelayOperationalState) {
			state.ForegroundToolStartedAtMs = &one
		}},
		{"a checkpoint barrier is waiting", func(state *core.RelayOperationalState) {
			barrier := "checkpoint-1"
			state.CheckpointBarrier = &barrier
		}},
	}
	for _, testCase := range busy {
		state := quiet
		testCase.make(&state)
		if state.IsQuiet() {
			t.Fatal(testCase.reason)
		}
		// A held barrier is the one reason IsQuiet refuses that is not
		// itself work: a caller already holding a barrier asks
		// HasWorkInFlight whether anything *else* is running.
		if testCase.reason == "a checkpoint barrier is waiting" {
			if state.HasWorkInFlight() {
				t.Fatal("a held barrier is not work in flight")
			}
		} else if !state.HasWorkInFlight() {
			t.Fatal(testCase.reason)
		}
	}
}

func TestRelayOperationalStateTracksMutableAcpOptionsAndCommands(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	option := snapshotTestThinkingOption(t)
	journalTestSessionUpdate(t, relay, acpschema.SessionUpdate{
		Kind:               acpschema.SessionUpdateKindConfigOptionUpdate,
		ConfigOptionUpdate: &acpschema.ConfigOptionUpdate{ConfigOptions: []acpschema.SessionConfigOption{option}},
	})
	modes := snapshotTestDecode[acpschema.SessionModeState](t,
		`{"currentModeId":"default","availableModes":[{"id":"default","name":"Default"},{"id":"plan","name":"Plan"}]}`)
	journalTestRecord(t, relay, core.RelayObservationSessionModesConfigured{Modes: &modes})
	journalTestSessionUpdate(t, relay, snapshotTestDecode[acpschema.SessionUpdate](t,
		`{"sessionUpdate":"current_mode_update","currentModeId":"plan"}`))
	journalTestSessionUpdate(t, relay, acpschema.SessionUpdate{
		Kind: acpschema.SessionUpdateKindAvailableCommandsUpdate,
		AvailableCommandsUpdate: &acpschema.AvailableCommandsUpdate{AvailableCommands: []acpschema.AvailableCommand{
			{Name: "review", Description: "Review the current work"},
		}},
	})

	state := relay.OperationalState()
	if journalTestJSON(t, state.ConfigOptions) != journalTestJSON(t, []acpschema.SessionConfigOption{option}) {
		t.Fatalf("config options = %s", journalTestJSON(t, state.ConfigOptions))
	}
	if state.Config["mode"] != "plan" {
		t.Fatalf("config mode = %q", state.Config["mode"])
	}
	if state.Modes == nil || fmt.Sprint(state.Modes.CurrentModeID) != "plan" {
		t.Fatalf("current_mode_update keeps the legacy catalogue synchronized: %+v", state.Modes)
	}
	if state.AvailableCommands[0].Name != "review" {
		t.Fatalf("available commands = %+v", state.AvailableCommands)
	}
}

func TestSnapshotsWithoutLegacyModesStillDeserialize(t *testing.T) {
	snapshot := core.NewRelaySnapshot(session)
	encoded := snapshotTestDecode[jsonvalue.Object](t, journalTestJSON(t, snapshot))
	encoded.Delete("modes")

	restored := snapshotTestDecode[core.RelaySnapshot](t, journalTestJSON(t, encoded))

	if restored.Modes != nil {
		t.Fatalf("modes = %+v", restored.Modes)
	}
}

func TestNativeSessionReadinessRequiresCurrentAcpSession(t *testing.T) {
	state := snapshotTestState(t)
	native := "restored-native-session"
	state.NativeSessionID = &native
	notReady, ready := false, true
	state.ACPReady = &notReady
	if state.NativeSessionIsReady() {
		t.Fatal("ready before ACP")
	}

	state.ACPReady = &ready
	if !state.NativeSessionIsReady() {
		t.Fatal("not ready with ACP")
	}

	state.Execution = core.RelayExecutionStateClosed
	if state.NativeSessionIsReady() {
		t.Fatal("a closed session is ready")
	}
}

func TestLegacyOperationalStateWithoutAcpReadinessIsReady(t *testing.T) {
	state := snapshotTestState(t)
	native := "legacy-native-session"
	state.NativeSessionID = &native
	if state.ACPReady != nil {
		t.Fatalf("acp ready = %v", *state.ACPReady)
	}
	history := core.DefaultContextHistory()
	history.User("prompt", []acpschema.ContentBlock{})
	context := core.DeepClone(history)
	state.AssessmentContext = &context
	pending := core.TurnAssessmentPending("turn", 1, 2, core.CompletionEvidenceBounded("test", nil))
	state.Assessment = &pending
	state.AvailableCommands = []acpschema.AvailableCommand{{Name: "help", Description: "Help"}}
	state.ConfigOptions = []acpschema.SessionConfigOption{snapshotTestThinkingOption(t)}
	// Rust holds these behind Arc; Go holds pointers and slices. The shared
	// and the owned representation must encode identically.
	if journalTestJSON(t, state.Assessment) != journalTestJSON(t, core.DeepClone(*state.Assessment)) {
		t.Fatal("a shared assessment encodes differently")
	}
	if journalTestJSON(t, state.AssessmentContext) != journalTestJSON(t, history) {
		t.Fatal("a shared assessment context encodes differently")
	}
	encoded := snapshotTestDecode[jsonvalue.Object](t, journalTestJSON(t, state))
	roundTrip := snapshotTestDecode[core.RelayOperationalState](t, journalTestJSON(t, encoded))
	if journalTestJSON(t, roundTrip) != journalTestJSON(t, state) {
		t.Fatalf("round trip changed the state:\n%s\n%s", journalTestJSON(t, roundTrip), journalTestJSON(t, state))
	}
	next := roundTrip
	ready := true
	next.ACPReady = &ready
	next.ShareUnchangedFields(&state)
	if next.Assessment != state.Assessment {
		t.Fatal("the unchanged assessment is not shared")
	}
	if &next.AvailableCommands[0] != &state.AvailableCommands[0] {
		t.Fatal("the unchanged available commands are not shared")
	}
	if &next.ConfigOptions[0] != &state.ConfigOptions[0] {
		t.Fatal("the unchanged config options are not shared")
	}
	next.AvailableCommands = next.AvailableCommands[:0]
	if len(state.AvailableCommands) != 1 {
		t.Fatal("clearing the shared commands changed the previous state")
	}
	encoded.Delete("acp_ready")

	restored := snapshotTestDecode[core.RelayOperationalState](t, journalTestJSON(t, encoded))

	if restored.ACPReady != nil {
		t.Fatalf("acp ready = %v", *restored.ACPReady)
	}
	if !restored.NativeSessionIsReady() {
		t.Fatal("a legacy state without readiness is not ready")
	}
}

func TestKimiReplacementRequiresCurrentBackgroundWorkKnowledge(t *testing.T) {
	state := snapshotTestState(t)
	native := "native-session"
	state.NativeSessionID = &native
	ready := true
	state.ACPReady = &ready

	if !state.IsQuiet() {
		t.Fatal("a ready idle state is not quiet")
	}
	if state.SafeToReplace(core.HarnessKindCodex) {
		t.Fatal("Codex replacement is safe")
	}
	if state.SafeToReplace(core.HarnessKindKimi) {
		t.Fatal("an older Kimi worker cannot prove provider tasks are absent")
	}

	unknown := false
	state.BackgroundWorkKnown = &unknown
	if state.IsQuiet() || state.SafeToReplace(core.HarnessKindKimi) {
		t.Fatal("unsynchronized Kimi background work is quiet")
	}

	known := true
	state.BackgroundWorkKnown = &known
	if !state.IsQuiet() || !state.SafeToReplace(core.HarnessKindKimi) {
		t.Fatal("synchronized empty Kimi background work is not replaceable")
	}
}

func TestKimiCheckpointRequiresKnownEmptyBackgroundWork(t *testing.T) {
	state := snapshotTestState(t)
	if state.SafeForCheckpoint(core.HarnessKindCodex) {
		t.Fatal("a Codex checkpoint is safe")
	}
	if state.SafeForCheckpoint(core.HarnessKindKimi) {
		t.Fatal("an older Kimi worker cannot prove provider tasks are absent")
	}

	if blocker, blocked := state.CheckpointBackgroundBlocker(core.HarnessKindKimi); !blocked || !strings.Contains(blocker, "not reported") {
		t.Fatalf("blocker = %q", blocker)
	}
	unknown := false
	state.BackgroundWorkKnown = &unknown
	if state.SafeForCheckpoint(core.HarnessKindKimi) {
		t.Fatal("unsynchronized Kimi background work is checkpoint-safe")
	}

	if blocker, blocked := state.CheckpointBackgroundBlocker(core.HarnessKindKimi); !blocked || !strings.Contains(blocker, "not synchronized") {
		t.Fatalf("blocker = %q", blocker)
	}
	known := true
	state.BackgroundWorkKnown = &known
	if !state.SafeForCheckpoint(core.HarnessKindKimi) {
		t.Fatal("known empty Kimi background work is not checkpoint-safe")
	}
	state.BackgroundCommands = append(state.BackgroundCommands, core.BackgroundCommand{
		ID:          "kimi:agent-1",
		StartedAtMs: 1,
		Command:     "background agent",
	})
	if state.SafeForCheckpoint(core.HarnessKindKimi) {
		t.Fatal("a running Kimi agent is checkpoint-safe")
	}
}

func TestV1EventsRoundTripByteIdenticallyAndV2OmitsTheChain(t *testing.T) {
	observation := core.RelayObservationWarning{Message: "hi"}

	// v1: no format key on the wire, keeps previous_digest, chains to its
	// cursor. Existing journals stay byte-for-byte identical.
	v1 := journalTestSignedEvent(t, core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        1,
		PreviousDigest: core.RelayEventGenesisDigest,
		RecordedAtMs:   42,
		Observation:    observation,
	})
	v1JSON := journalTestJSON(t, v1)
	if strings.Contains(v1JSON, `"format"`) {
		t.Fatalf("v1 must not write a format key: %s", v1JSON)
	}
	if !strings.Contains(v1JSON, "previous_digest") {
		t.Fatalf("v1 lost its chain link: %s", v1JSON)
	}
	if err := core.ValidateRelayEvent(0, core.RelayEventGenesisDigest, &v1); err != nil {
		t.Fatal(err)
	}

	// v2: tags its format, carries no chain link, and self-validates
	// regardless of the cursor digest.
	v2 := journalTestSignedEvent(t, core.RelayEvent{
		Format:       core.RelayEventFormatV2,
		Ordinal:      1,
		RecordedAtMs: 42,
		Observation:  observation,
	})
	v2JSON := journalTestJSON(t, v2)
	if !strings.Contains(v2JSON, `"format":2`) {
		t.Fatalf("v2 must tag its format: %s", v2JSON)
	}
	if strings.Contains(v2JSON, "previous_digest") {
		t.Fatalf("v2 must not write a chain link: %s", v2JSON)
	}
	if err := core.ValidateRelayEvent(0, core.RelayEventGenesisDigest, &v2); err != nil {
		t.Fatal(err)
	}
	if err := core.ValidateRelayEvent(0, strings.Repeat("a", 64), &v2); err != nil {
		t.Fatalf("a v2 event has no in-record link, so any cursor digest is accepted: %v", err)
	}

	// Same ordinal + content, different format → different digest (domain
	// separation + payload), so v1 and v2 never collide.
	if v1.Digest == v2.Digest {
		t.Fatal("v1 and v2 digests collide")
	}

	// Round-trip both, and confirm an old record with no format key reads as
	// v1.
	v1Back := snapshotTestDecode[core.RelayEvent](t, v1JSON)
	if journalTestJSON(t, v1Back) != v1JSON || v1Back.Format != v1.Format {
		t.Fatalf("v1 round trip = %+v", v1Back)
	}
	v2Back := snapshotTestDecode[core.RelayEvent](t, v2JSON)
	if journalTestJSON(t, v2Back) != v2JSON || v2Back.Format != v2.Format {
		t.Fatalf("v2 round trip = %+v", v2Back)
	}
	if v2Back.PreviousDigest != "" {
		t.Fatalf("v2 previous digest = %q", v2Back.PreviousDigest)
	}
	legacy := snapshotTestDecode[core.RelayEvent](t,
		`{"ordinal":1,"previous_digest":"","digest":"x","recorded_at_ms":0,"observation":{"type":"warning","data":{"message":"m"}}}`)
	if legacy.Format != core.RelayEventFormatV1 {
		t.Fatalf("legacy format = %d", legacy.Format)
	}
}

func TestQueueEntriesWrittenBeforeConfigChangesStillLoad(t *testing.T) {
	stored := snapshotTestDecode[core.StoredQueuedRelayCommand](t,
		`{"command_id":"queued-1","prompt":[{"type":"text","text":"hello"}],"created_at_ms":7}`)
	if _, isPrompt := stored.Payload.(core.StoredQueuedRelayPayloadPrompt); !isPrompt {
		t.Fatalf("payload = %#v", stored.Payload)
	}

	config := core.StoredQueuedRelayCommand{
		CommandID:   "queued-2",
		Payload:     core.StoredQueuedRelayPayloadSetConfig{Key: "model", Value: "sonnet"},
		CreatedAtMs: 8,
	}
	encoded := journalTestJSON(t, config)
	value := snapshotTestDecode[jsonvalue.Object](t, encoded)
	if key, _ := value.Get("key"); key != "model" {
		t.Fatalf("encoded key = %v", key)
	}
	decoded := snapshotTestDecode[core.StoredQueuedRelayCommand](t, encoded)
	if journalTestJSON(t, decoded) != encoded {
		t.Fatalf("round trip = %+v", decoded)
	}
	if _, setsConfig := decoded.Payload.(core.StoredQueuedRelayPayloadSetConfig); !setsConfig {
		t.Fatalf("payload = %#v", decoded.Payload)
	}
}

func TestOversizedCommandsAreRejectedBeforeJournaling(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	response := relay.Handle(relayRequest("oversized-command", core.RelayRequestSubmit{
		CommandID: "oversized-command",
		Command:   prompt(strings.Repeat("x", core.RelayCommandByteBudget)),
	}))
	if !journalTestErrorCode(response.Body, core.RelayErrorCodeInvalidRequest) {
		t.Fatalf("response = %#v", response.Body)
	}
	if relay.LatestOrdinal() != 0 {
		t.Fatalf("latest ordinal %d", relay.LatestOrdinal())
	}
}

// Hard-won: be7c009d: one oversized ACP observation killed the relay and repeatedly restarted its worker.
func TestOversizedObservationsAreTruncatedInsteadOfFailing(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")

	ordinal, err := relay.RecordObservation(core.RelayObservationWarning{Message: strings.Repeat("x", core.RelayEventByteBudget)})
	if err != nil {
		t.Fatalf("an oversized observation is recorded, not rejected: %v", err)
	}
	if ordinal != 1 || relay.LatestOrdinal() != 1 {
		t.Fatalf("recorded at %d, latest %d", ordinal, relay.LatestOrdinal())
	}

	replayed := journalTestEventsAfter(t, relay, 0, core.RelayEventGenesisDigest)
	recorded := replayed[0]
	warning, ok := recorded.Observation.(core.RelayObservationWarning)
	if !ok {
		t.Fatalf("expected the truncated warning, found %#v", recorded.Observation)
	}
	if !strings.HasPrefix(warning.Message, "xxxx") {
		t.Fatal("the head of the payload is kept")
	}
	if !strings.Contains(warning.Message, "[mj truncated") {
		t.Fatal("truncation is disclosed")
	}
	if size := len(journalTestJSON(t, recorded)); size > core.RelayEventByteBudget {
		t.Fatalf("recorded event is %d bytes", size)
	}
}

// The journal is append-only, so what a recorded edit costs is what it costs
// forever. It records the patch, not two copies of the file.
// Hard-won: b63924a3: full old and new file copies inflated a real session journal past IPC limits.
func TestARecordedEditJournalsAPatchRatherThanTheWholeFile(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	var old strings.Builder
	for line := range 4_000 {
		fmt.Fprintf(&old, "line %d\n", line)
	}
	oldText := old.String()
	newText := strings.Replace(oldText, "line 2000\n", "line 2000 edited\n", 1)
	call := map[string]any{
		"sessionUpdate": "tool_call",
		"toolCallId":    "call-1",
		"title":         "Edit files",
		"content": []any{map[string]any{
			"type":    "diff",
			"path":    "/repo/src/main.rs",
			"oldText": oldText,
			"newText": newText,
		}},
	}
	encoded, err := json.Marshal(call)
	if err != nil {
		t.Fatal(err)
	}
	journalTestSessionUpdate(t, relay, snapshotTestDecode[acpschema.SessionUpdate](t, string(encoded)))

	replayed := journalTestEventsAfter(t, relay, 0, core.RelayEventGenesisDigest)
	recorded := replayed[0]
	update, ok := recorded.Observation.(core.RelayObservationSessionUpdate)
	if !ok {
		t.Fatalf("expected a session update, found %#v", recorded.Observation)
	}
	if update.Update.ToolCall == nil {
		t.Fatal("expected a tool call")
	}
	content := update.Update.ToolCall.Content
	if len(content) != 1 || content[0].Diff == nil {
		t.Fatal("expected one diff")
	}
	diff := content[0].Diff
	if diff.OldText != nil {
		t.Fatal("the old copy is not journalled")
	}
	if diff.NewText != "" {
		t.Fatal("the new copy is not journalled")
	}
	patch := core.PatchOf(diff)
	if patch.Insertions != 1 || patch.Deletions != 1 {
		t.Fatalf("patch +%d -%d", patch.Insertions, patch.Deletions)
	}
	if !strings.Contains(patch.Text, "+line 2000 edited\n") {
		t.Fatalf("patch = %s", patch.Text)
	}
	if len(journalTestJSON(t, recorded))*20 >= len(oldText) {
		t.Fatal("a one-line edit still cost a copy of the file")
	}
}

func TestOperationalStateRetainsBoundedAuthorizationWithoutDispatchPayloads(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	submitRelay(relay, "secret-prompt", prompt("payload-that-must-not-be-in-operational-state"))
	encoded := journalTestJSON(t, relay.OperationalState())
	if len(encoded) > core.RelayStateByteBudget {
		t.Fatalf("operational state is %d bytes", len(encoded))
	}
	if !strings.Contains(encoded, "secret-prompt") {
		t.Fatal("the operational state does not name the prompt")
	}
	state := snapshotTestDecode[jsonvalue.Object](t, encoded)
	if _, found := state.Get("dispatches"); found {
		t.Fatal("the operational state carries dispatches")
	}
	context, _ := state.Get("assessment_context")
	messages, _ := context.(jsonvalue.Object).Get("messages")
	text, _ := messages.([]any)[0].(jsonvalue.Object).Get("text")
	if text != "payload-that-must-not-be-in-operational-state" {
		t.Fatalf("authorization text = %v", text)
	}

	halfBudget := core.RelayStateByteBudget / 2
	journalTestRecord(t, relay, core.RelayObservationConfigurationUpdated{Key: "large-one", Value: strings.Repeat("a", halfBudget)})
	before := relay.LatestOrdinal()
	_, err := relay.RecordObservation(core.RelayObservationConfigurationUpdated{Key: "large-two", Value: strings.Repeat("b", halfBudget)})
	if err == nil || !strings.Contains(err.Error(), "operational state is too large") {
		t.Fatalf("error = %v", err)
	}
	if relay.LatestOrdinal() != before {
		t.Fatal("the refused observation moved the frontier")
	}
}

func TestEventChainDetectsCursorAndBodyDesynchronization(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "authentic")
	event := retainedEvents(relay)[0]
	if err := core.ValidateRelayEvent(0, core.RelayEventGenesisDigest, &event); err != nil {
		t.Fatal(err)
	}

	tampered := event
	tampered.Observation = core.RelayObservationWarning{Message: "tampered"}
	if core.ValidateRelayEvent(0, core.RelayEventGenesisDigest, &tampered) == nil {
		t.Fatal("a tampered body validated")
	}

	mismatch := relay.Handle(relayRequest("attach-wrong-digest", core.RelayRequestAttach{
		AfterOrdinal: 0,
		AfterDigest:  strings.Repeat("a", 64),
	}))
	if !journalTestErrorCode(mismatch.Body, core.RelayErrorCodeDesynchronized) {
		t.Fatalf("attach = %#v", mismatch.Body)
	}
}
