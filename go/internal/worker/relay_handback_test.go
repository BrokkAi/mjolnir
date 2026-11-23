// Port of mj-worker/src/relay/handback_tests.rs.

package worker

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func handbackOpen(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, session, "test")
	if err != nil {
		t.Fatal(err)
	}
	relay.SetTurnVerdictHarness(core.HarnessKindCodex)
	return relay
}

func handbackConfigure(t *testing.T, relay *DurableRelay) {
	t.Helper()
	if _, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}); err != nil {
		t.Fatal(err)
	}
}

func handbackClaim(t *testing.T, relay *DurableRelay) []core.ClaimedRelayCommand {
	t.Helper()
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatal(err)
	}
	return claimed
}

func handbackAssistant(t *testing.T) acpschema.SessionUpdate {
	t.Helper()
	var update acpschema.SessionUpdate
	if err := json.Unmarshal([]byte(`{
		"sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"Assistant progress must not be mirrored."}
	}`), &update); err != nil {
		t.Fatal(err)
	}
	return update
}

func handbackRecord(t *testing.T, relay *DurableRelay, observation core.RelayObservation) {
	t.Helper()
	if _, err := relay.RecordObservation(observation); err != nil {
		t.Fatal(err)
	}
}

// handbackPrepare is prepare_handback_check(..).unwrap().
func handbackPrepare(t *testing.T, relay *DurableRelay, report string) (PreparedHandbackCheck, bool) {
	t.Helper()
	prepared, err := relay.prepareHandbackCheck(report)
	if err != nil {
		t.Fatal(err)
	}
	if prepared == nil {
		return PreparedHandbackCheck{}, false
	}
	return *prepared, true
}

func handbackAssignmentID(t *testing.T, event core.MailboxEvent) string {
	t.Helper()
	id, _, ok := event.HandbackAssignment()
	if !ok {
		t.Fatalf("event %s carries no handback assignment", event.Key)
	}
	return id
}

func handbackAssignmentIs(relay *DurableRelay, id string) bool {
	current := relay.snapshot.HandbackChecks.AssignmentID
	return current != nil && *current == id
}

func handbackUserMessagesOnly(t *testing.T, context *core.ContextHistory, count int) {
	t.Helper()
	if context == nil {
		t.Fatal("no handback instructions")
	}
	if len(context.Messages) != count {
		t.Fatalf("instructions hold %d messages, want %d", len(context.Messages), count)
	}
	for _, message := range context.Messages {
		if message.Role != "user" {
			t.Fatalf("instructions hold a %s message", message.Role)
		}
	}
}

func handbackReadSnapshot(t *testing.T, root string) []byte {
	t.Helper()
	stale, err := os.ReadFile(filepath.Join(root, core.RelayStateFile))
	if err != nil {
		t.Fatal(err)
	}
	return stale
}

func handbackWriteSnapshot(t *testing.T, root string, snapshot []byte) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(root, core.RelayStateFile), snapshot, 0o600); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: 1d399ac64: restarting a mailbox wake overwrote a newer hook assignment.
func TestHandbackAttemptSurvivesRestartAndRemindersButNewAssignmentGetsACheck(t *testing.T) {
	root := relayTempDir(t)
	relay := handbackOpen(t, root)
	handbackConfigure(t, relay)
	submitRelay(relay, "person-before-assignment", prompt("Top-level prompt must not start handback history."))
	handbackClaim(t, relay)
	if _, err := relay.RecordSessionUpdate(handbackAssistant(t)); err != nil {
		t.Fatal(err)
	}
	if relay.snapshot.HandbackChecks.Context != nil {
		t.Fatal("a top-level prompt must not start handback history")
	}
	finishPrompt(relay, "person-before-assignment")
	assignment := "subagent-spawn-original"
	submitRelay(relay, assignment, prompt("Implement and validate the requested change."))
	handbackClaim(t, relay)
	if _, err := relay.RecordSessionUpdate(handbackAssistant(t)); err != nil {
		t.Fatal(err)
	}
	instructions := relay.snapshot.HandbackChecks.Context
	handbackUserMessagesOnly(t, instructions, 1)
	if encoded, _ := json.Marshal(instructions); strings.Contains(string(encoded), "Assistant progress") {
		t.Fatal("assistant progress must not be mirrored into the instructions")
	}
	staleSnapshot := handbackReadSnapshot(t, root)
	prepared, ok := handbackPrepare(t, relay, "Implementation still needs validation.")
	if !ok {
		t.Fatal("the assignment's report is checked")
	}
	evidence := prepared.Evidence
	if evidence.Completion == nil || evidence.Completion.StopReason != "end_turn" || evidence.Completion.Diagnostic != nil {
		t.Fatalf("completion = %#v", evidence.Completion)
	}
	if evidence.Phase != core.TurnPhaseReplied {
		t.Fatalf("phase = %s", evidence.Phase)
	}
	messages := evidence.Authorization.Messages
	if last := messages[len(messages)-1].Text; last != "Implementation still needs validation." {
		t.Fatalf("last authorization message = %q", last)
	}
	if _, ok := handbackPrepare(t, relay, "A second report"); ok {
		t.Fatal("an assignment is checked once")
	}
	finishPrompt(relay, assignment)
	// Force journal replay rather than letting the materialized snapshot
	// prove durability.
	handbackWriteSnapshot(t, root, staleSnapshot)
	relay = handbackOpen(t, root)
	if _, ok := handbackPrepare(t, relay, "After restart"); ok {
		t.Fatal("the spent check survives a restart")
	}
	handbackConfigure(t, relay)
	instructions = relay.snapshot.HandbackChecks.Context
	if instructions == nil || len(instructions.Messages) != 1 {
		t.Fatal("replay must retain instructions alone")
	}
	handbackUserMessagesOnly(t, instructions, 1)
	completedOrdinal := relay.snapshot.TurnCompletion.CompletedOrdinal
	reminder := core.HandbackReminderCommandID(completedOrdinal)
	submitRelay(relay, reminder, core.RelayCommandHandbackReminder{
		CompletedCommandID: assignment,
		CompletedOrdinal:   completedOrdinal,
	})
	handbackClaim(t, relay)
	if _, ok := handbackPrepare(t, relay, "After reminder"); ok {
		t.Fatal("a reminder does not grant another check")
	}
	finishPrompt(relay, reminder)
	submitRelay(relay, "subagent-input-followup", prompt("Also validate the new case."))
	handbackClaim(t, relay)
	finishPrompt(relay, "subagent-input-followup")
	submitRelay(relay, "person-after-assignment", prompt("Keep the parent's instructions in scope."))
	handbackClaim(t, relay)
	if _, err := relay.RecordSessionUpdate(handbackAssistant(t)); err != nil {
		t.Fatal(err)
	}
	handbackUserMessagesOnly(t, relay.snapshot.HandbackChecks.Context, 3)
	// Hard-won: e44bee684: a report over 2,048 bytes broke the proxy's tail bound, so the check was spent unchecked
	longReport := fmt.Sprintf("Follow-up report. %sRemaining: none.", strings.Repeat("é", 3000))
	prepared, ok = handbackPrepare(t, relay, longReport)
	if !ok {
		t.Fatal("follow-up assignment is checked")
	}
	if len(prepared.Evidence.AssistantTextTail) > core.AssistantTextBytes {
		t.Fatalf("assistant text tail holds %d bytes", len(prepared.Evidence.AssistantTextTail))
	}
	if !strings.HasSuffix(prepared.Evidence.AssistantTextTail, "Remaining: none.") {
		t.Fatalf("assistant text tail = %q", prepared.Evidence.AssistantTextTail)
	}
	finishPrompt(relay, "person-after-assignment")

	// Hard-won: 9130f139e: parent follow-ups delivered through the mailbox bypassed the handback check
	for _, hook := range []bool{true, false} {
		event := core.MailboxEvent{
			Key:         fmt.Sprintf("parent-followup-%v", hook),
			Source:      "parent",
			Wake:        true,
			CreatedAtMs: 1,
			Body:        core.MailboxEventBodyParentMessage{Text: fmt.Sprintf("Validate mailbox delivery %v", hook)},
		}
		staleSnapshot := handbackReadSnapshot(t, root)
		handbackRecord(t, relay, core.RelayObservationCommandQueued{
			CommandID:   fmt.Sprintf("deliver-%v", hook),
			Command:     core.RelayCommandDeliverMailboxEvent{Event: event},
			CreatedAtMs: 1,
		})
		// Queuing alone must not create an assignment.
		if handbackAssignmentIs(relay, handbackAssignmentID(t, event)) {
			t.Fatal("queuing alone must not create an assignment")
		}
		postToolUse := "PostToolUse"
		if hook {
			handbackRecord(t, relay, core.RelayObservationMailboxEventsDelivered{
				EventKeys: []string{event.Key},
				Path:      core.MailboxDeliveryPathToolHook,
				HookEvent: &postToolUse,
				Events:    []core.MailboxEvent{event},
			})
		} else {
			handbackRecord(t, relay, core.RelayObservationCommandQueued{
				CommandID:   "mailbox-wake-followup",
				Command:     core.RelayCommandMailboxWake{Events: []core.MailboxEvent{event}},
				CreatedAtMs: 1,
			})
			handbackClaim(t, relay)
			newer := event
			newer.Key = "newer-hook-assignment"
			newer.Body = core.MailboxEventBodyParentMessage{Text: "Newer assignment supersedes the wake"}
			handbackRecord(t, relay, core.RelayObservationCommandQueued{
				CommandID:   "deliver-newer-hook",
				Command:     core.RelayCommandDeliverMailboxEvent{Event: newer},
				CreatedAtMs: 2,
			})
			handbackRecord(t, relay, core.RelayObservationMailboxEventsDelivered{
				EventKeys: []string{newer.Key},
				Path:      core.MailboxDeliveryPathToolHook,
				HookEvent: &postToolUse,
				Events:    []core.MailboxEvent{newer},
			})
			newerID := handbackAssignmentID(t, newer)
			relay = handbackOpen(t, root)
			handbackConfigure(t, relay)
			resumed := handbackClaim(t, relay)
			if len(resumed) != 1 || !core.IsRestartContinuation(resumed[0].CommandID) {
				t.Fatalf("resumed %#v, want the restart continuation", resumed)
			}
			if !handbackAssignmentIs(relay, newerID) {
				t.Fatal("the restart keeps the newer hook assignment")
			}
			finishPrompt(relay, resumed[0].CommandID)
			// Continue the existing checks against the latest assignment.
			event = newer
		}
		expectedID := handbackAssignmentID(t, event)
		if !handbackAssignmentIs(relay, expectedID) {
			t.Fatalf("hook %v: assignment = %v, want %s", hook, relay.snapshot.HandbackChecks.AssignmentID, expectedID)
		}
		prepared, ok := handbackPrepare(t, relay, "Mailbox report")
		if !ok {
			t.Fatalf("hook %v: the mailbox assignment is checked", hook)
		}
		encoded, err := json.Marshal(prepared.Evidence)
		if err != nil {
			t.Fatal(err)
		}
		wanted := "Newer assignment supersedes the wake"
		if hook {
			wanted = "Validate mailbox delivery true"
		}
		if !strings.Contains(string(encoded), wanted) {
			t.Fatalf("evidence lacks %q: %s", wanted, encoded)
		}
		if _, ok := handbackPrepare(t, relay, "Duplicate"); ok {
			t.Fatal("a mailbox assignment is checked once")
		}
		handbackWriteSnapshot(t, root, staleSnapshot)
		relay = handbackOpen(t, root)
		if !handbackAssignmentIs(relay, expectedID) {
			t.Fatal("replay restores the assignment")
		}
		if _, ok := handbackPrepare(t, relay, "Restart"); ok {
			t.Fatal("replay restores the spent check")
		}
		peer := event
		peer.Key = fmt.Sprintf("peer-%v", hook)
		peer.Body = core.MailboxEventBodyPlainText{Text: "Peer instructions are not an assignment"}
		peer.Wake = false
		submitRelay(relay, fmt.Sprintf("peer-delivery-%v", hook), core.RelayCommandDeliverMailboxEvent{Event: peer})
		handbackRecord(t, relay, core.RelayObservationMailboxEventsDelivered{
			EventKeys: []string{peer.Key},
			Path:      core.MailboxDeliveryPathToolHook,
			HookEvent: &postToolUse,
			Events:    []core.MailboxEvent{peer},
		})
		if !handbackAssignmentIs(relay, expectedID) {
			t.Fatal("a peer message is not an assignment")
		}
		if encoded, _ := json.Marshal(relay.snapshot.HandbackChecks.Context); strings.Contains(string(encoded), "Peer instructions") {
			t.Fatal("peer instructions must not enter the handback instructions")
		}
		if _, ok := handbackPrepare(t, relay, "Peer report"); ok {
			t.Fatal("a peer message grants no new check")
		}
	}
}

// Hard-won: b1e4ba6d8: an assignment GLiDE could not see whole was still classified and could bounce a valid report
func TestIncompleteAssignmentEvidenceDeliversUncheckedWithoutSpendingTheCheck(t *testing.T) {
	relay := handbackOpen(t, relayTempDir(t))
	handbackConfigure(t, relay)
	assignment := "subagent-spawn-with-image"
	submitRelay(relay, assignment, core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{
		relayTextBlock("Match the layout in the attached screenshot."),
		{Kind: acpschema.ContentBlockKindImage, Image: &acpschema.ImageContent{Data: "aGVsbG8=", MimeType: "image/png"}},
	}})
	handbackClaim(t, relay)
	if _, ok := handbackPrepare(t, relay, "Layout matches."); ok {
		t.Fatal("an assignment GLiDE cannot see whole is delivered unchecked")
	}
	if relaySortedSetContains(relay.snapshot.HandbackChecks.Checked, assignment) {
		t.Fatal("the unchecked delivery must not spend the check")
	}
}

func TestHandbackInstructionsBeyondTheOldJevRequestBudgetAreChecked(t *testing.T) {
	relay := handbackOpen(t, relayTempDir(t))
	handbackConfigure(t, relay)
	submitRelay(relay, "subagent-spawn-large", prompt("Implement and validate the requested change."))
	handbackClaim(t, relay)
	// A full user budget of newlines: 32 KiB of text, about 60 KiB once JSON
	// escapes it, so the old Jev request limit refused it with the questions.
	context := relay.snapshot.HandbackChecks.Context
	for i := range 6 {
		context.Messages = append(context.Messages, core.EvidenceMessage{
			ID:   fmt.Sprintf("user:extra%d", i),
			Role: "user",
			Text: strings.Repeat("\n", 5000),
		})
	}
	prepared, ok := handbackPrepare(t, relay, "Done.")
	if !ok {
		t.Fatal("whole instructions are checked however large")
	}
	authorization := prepared.Evidence.Authorization
	if !authorization.AuthorizationComplete {
		t.Fatal("the instructions are complete")
	}
	users := 0
	for _, message := range authorization.Messages {
		if message.Role == "user" {
			users++
		}
	}
	if users != 7 {
		t.Fatalf("authorization holds %d user messages", users)
	}
	// The v7 questions took about 12 KB of the old 64 KiB request.
	encoded, err := json.Marshal(authorization)
	if err != nil {
		t.Fatal(err)
	}
	if len(encoded) <= 52*1024 {
		t.Fatalf("authorization encodes to %d bytes", len(encoded))
	}
}
