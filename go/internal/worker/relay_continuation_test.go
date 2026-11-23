// Port of mj-worker/src/relay/continuation_tests.rs.

package worker

import (
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func continuationCompleted(t *testing.T, relay *DurableRelay, id string) {
	t.Helper()
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(claimed, func(command core.ClaimedRelayCommand) bool { return command.CommandID == id }) {
		t.Fatalf("%s was not claimed: %#v", id, claimed)
	}
	if _, err := relay.RecordCommandCompleted(id, core.RelayCommandOutcomePrompt{StopReason: "end_turn"}); err != nil {
		t.Fatal(err)
	}
}

func continuationAuthorize(t *testing.T, relay *DurableRelay, action core.AssessmentAction) {
	t.Helper()
	if relay.snapshot.Assessment == nil || !relay.snapshot.Assessment.Current() {
		return
	}
	assessment := *relay.snapshot.Assessment
	assessment.Action = &action
	assessment.Status = core.StatusDeferred
	if _, err := relay.appendRelayEvent(nil, core.RelayObservationTurnAssessmentUpdated{Assessment: assessment}); err != nil {
		t.Fatal(err)
	}
}

func continuationOpen(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, session, "test")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}); err != nil {
		t.Fatal(err)
	}
	return relay
}

// continuationSubmit is relay.submit_command(..).unwrap(): the outer error
// fails the test, and the refusal is returned.
func continuationSubmit(t *testing.T, relay *DurableRelay, commandID string, command core.RelayCommand) (core.RelayResponsePayload, *core.RelayProtocolError) {
	t.Helper()
	payload, refusal, err := relay.submitCommand(commandID, command)
	if err != nil {
		t.Fatal(err)
	}
	return payload, refusal
}

func continuationAccepted(t *testing.T, relay *DurableRelay, commandID string, command core.RelayCommand) core.RelayResponsePayload {
	t.Helper()
	payload, refusal := continuationSubmit(t, relay, commandID, command)
	if refusal != nil {
		t.Fatalf("%s refused: %#v", commandID, refusal)
	}
	return payload
}

func continuationRefused(t *testing.T, relay *DurableRelay, commandID string, command core.RelayCommand, context string) {
	t.Helper()
	if _, refusal := continuationSubmit(t, relay, commandID, command); refusal == nil {
		t.Fatalf("%s %s must be refused", context, commandID)
	}
}

func continuationString(value *string) string {
	if value == nil {
		return "<none>"
	}
	return *value
}

func TestGeneratedPromptsDoNotGrantAuthorization(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "capacity-retry-123", prompt("Continue"))
	continuationCompleted(t, relay, "capacity-retry-123")
	if relay.snapshot.Continuation.UserCommandID != nil {
		t.Fatalf("a capacity retry authorized %s", *relay.snapshot.Continuation.UserCommandID)
	}
	submitRelay(relay, "original-prompt", prompt("Implement and test"))
	continuationCompleted(t, relay, "original-prompt")
	submitRelay(relay, "review-forward-123", prompt("Fix this finding"))
	continuationCompleted(t, relay, "review-forward-123")
	// Hard-won: 8b08e8bc1: handback reminders and peer-session messages counted as user authorization
	for _, id := range []string{
		core.HandbackReminderCommandID(7),
		core.GeneratedPromptSessionMessageTurn.CommandID("peer-1"),
	} {
		submitRelay(relay, id, prompt("Please also update the docs"))
		continuationCompleted(t, relay, id)
	}
	if got := continuationString(relay.snapshot.Continuation.UserCommandID); got != "original-prompt" {
		t.Fatalf("user command = %s", got)
	}
}

func continuationQuotaSchedule(t *testing.T, relay *DurableRelay, resetAtMs *int64) core.RelayCommand {
	t.Helper()
	continuationAuthorize(t, relay, core.AssessmentActionRecoverQuota)
	state := relay.OperationalState()
	var retryAtMs *int64
	if resetAtMs != nil {
		retry := *resetAtMs + 60_000
		retryAtMs = &retry
	}
	if state.Continuation.UserCommandID == nil || state.Continuation.CompletedCommandID == nil {
		t.Fatalf("no completed user request to recover: %#v", state.Continuation)
	}
	return core.RelayCommandSetQuotaRecovery{
		Expected: core.RelayCursor{Ordinal: state.LatestOrdinal, Digest: state.LatestDigest},
		Recovery: &core.QuotaRecovery{
			UserCommandID:      *state.Continuation.UserCommandID,
			CompletedCommandID: *state.Continuation.CompletedCommandID,
			ProfileID:          "isolated-quota-test",
			ResetAtMs:          resetAtMs,
			RetryAtMs:          retryAtMs,
			Notice:             "Quota reset scheduled",
			Submitted:          false,
		},
	}
}

func continuationPastDeadline() *int64 {
	reset := core.EpochMillis() - 60_001
	return &reset
}

// Hard-won: #1562: a lost quota schedule acknowledgement was retried with different stale evidence.
func TestQuotaScheduleReplaysItsOriginalAcceptanceBeforeStaleEvidenceChecks(t *testing.T) {
	root := t.TempDir()
	relay := continuationOpen(t, root)
	submitRelay(relay, "original-prompt", core.RelayCommandPrompt{Prompt: core.PromptBlocks()})
	continuationCompleted(t, relay, "original-prompt")
	command := continuationQuotaSchedule(t, relay, nil)
	first := continuationAccepted(t, relay, "quota-schedule-3", command)
	accepted := relay.snapshot.HandledCommands["quota-schedule-3"].AcceptedOrdinal
	relay, err := DurableRelayOpen(root, session, "test")
	if err != nil {
		t.Fatal(err)
	}
	echoed := continuationAccepted(t, relay, "quota-schedule-3", command)
	if !reflect.DeepEqual(echoed, first) {
		t.Fatalf("echoed %#v, first %#v", echoed, first)
	}
	if got := relay.snapshot.HandledCommands["quota-schedule-3"].AcceptedOrdinal; got != accepted {
		t.Fatalf("accepted ordinal = %d, want %d", got, accepted)
	}
}

func continuationQuotaResume(t *testing.T, relay *DurableRelay) core.RelayCommand {
	t.Helper()
	state := relay.OperationalState()
	if state.Continuation.CompletedCommandID == nil {
		t.Fatal("no completed command to resume")
	}
	return core.RelayCommandResumeAfterQuota{
		Expected:           core.RelayCursor{Ordinal: state.LatestOrdinal, Digest: state.LatestDigest},
		CompletedCommandID: *state.Continuation.CompletedCommandID,
	}
}

// Hard-won: 3c8973b87: a mailbox wake hit Kimi's quota with no recoverable request after restore.
func TestMailboxQuotaRecoverySurvivesMissingUserRequestAndPriorCancellation(t *testing.T) {
	// Hard-won: 9c2b74a01: a peer message delivered as a prompt lost the wake's quota identity.
	for _, variant := range []struct{ noUserRequest, asPrompt bool }{
		{true, false}, {false, false}, {true, true}, {false, true},
	} {
		root := t.TempDir()
		relay := continuationOpen(t, root)
		if !variant.noUserRequest {
			submitRelay(relay, "user-request", prompt("Implement and test"))
			continuationCompleted(t, relay, "user-request")
		}
		submitRelay(relay, "cancel-old-work", core.RelayCommandCancelTurn{})
		if _, err := relay.ClaimPendingCommands(true); err != nil {
			t.Fatal(err)
		}
		if _, err := relay.RecordCommandCompleted("cancel-old-work", core.RelayCommandOutcomeCancelled{}); err != nil {
			t.Fatal(err)
		}
		if !relay.snapshot.Continuation.QuotaSuppressed {
			t.Fatal("a cancellation suppresses quota recovery")
		}
		var wakeID string
		if variant.asPrompt {
			// Workers without mailbox delivery get the message as a prompt.
			id := core.GeneratedPromptSessionMessageTurn.CommandID("peer-1")
			submitRelay(relay, id, prompt("Your PR was merged."))
			claimed, err := relay.ClaimPendingCommands(true)
			if err != nil {
				t.Fatal(err)
			}
			if len(claimed) != 1 || claimed[0].CommandID != id {
				t.Fatalf("claimed %#v", claimed)
			}
			wakeID = id
		} else {
			event := core.MailboxEvent{
				Key:         "merged-pr",
				Source:      "github",
				Wake:        true,
				CreatedAtMs: 1,
				Body:        core.MailboxEventBodyPlainText{Text: "Your PR was merged."},
			}
			submitRelay(relay, "deliver-merge", core.RelayCommandDeliverMailboxEvent{Event: event})
			claimed, err := relay.ClaimPendingCommands(true)
			if err != nil {
				t.Fatal(err)
			}
			if len(claimed) != 1 {
				t.Fatalf("claimed %#v", claimed)
			}
			if _, wake := claimed[0].Command.(core.RelayCommandMailboxWake); !wake {
				t.Fatalf("claimed %#v, want a mailbox wake", claimed[0].Command)
			}
			wakeID = claimed[0].CommandID
		}
		code := "provider.api_error"
		status := uint16(429)
		if _, err := relay.RecordCommandCompleted(wakeID, core.RelayCommandOutcomePrompt{
			StopReason: core.QuotaStopReason,
			Diagnostic: &core.TurnDiagnostic{
				Message:    "429 You've reached your 5-hour usage limit. Your quota will reset when the current 5-hour window ends.",
				Code:       &code,
				HTTPStatus: &status,
			},
		}); err != nil {
			t.Fatal(err)
		}
		relay = continuationOpen(t, root)
		if got := continuationString(relay.snapshot.Continuation.CompletedCommandID); got != wakeID {
			t.Fatalf("completed command = %s, want %s", got, wakeID)
		}
		wantUser := "user-request"
		if variant.noUserRequest {
			wantUser = wakeID
		}
		if got := continuationString(relay.snapshot.Continuation.UserCommandID); got != wantUser {
			t.Fatalf("user command = %s, want %s", got, wantUser)
		}
		command := continuationQuotaSchedule(t, relay, continuationPastDeadline())
		submitRelay(relay, "quota-schedule", command)
		relay = continuationOpen(t, root)
		command = continuationQuotaResume(t, relay)
		submitRelay(relay, "quota-retry", command)
		continuationCompleted(t, relay, "quota-retry")
	}
}

func TestQuotaRetrySurvivesRestartAndADuplicateIsRefused(t *testing.T) {
	root := t.TempDir()
	relay := continuationOpen(t, root)
	submitRelay(relay, "user-request", prompt("Implement and test"))
	continuationCompleted(t, relay, "user-request")
	command := continuationQuotaSchedule(t, relay, continuationPastDeadline())
	submitRelay(relay, "quota-schedule", command)
	relay = continuationOpen(t, root)
	if relay.snapshot.Continuation.QuotaRecovery == nil {
		t.Fatal("the scheduled recovery survives a restart")
	}
	command = continuationQuotaResume(t, relay)
	submitRelay(relay, "quota-retry-1", command)
	submitRelay(relay, "quota-retry-1", command)
	if got := continuationString(relay.snapshot.Continuation.UserCommandID); got != "user-request" {
		t.Fatalf("user command = %s", got)
	}
	continuationCompleted(t, relay, "quota-retry-1")
	if got := continuationString(relay.snapshot.Continuation.CompletedCommandID); got != "quota-retry-1" {
		t.Fatalf("completed command = %s", got)
	}
	continuationRefused(t, relay, "duplicate-quota", continuationQuotaResume(t, relay), "a duplicate resume")
	command = continuationQuotaSchedule(t, relay, continuationPastDeadline())
	submitRelay(relay, "quota-schedule-next", command)
	command = continuationQuotaResume(t, relay)
	submitRelay(relay, "quota-retry-2", command)
}

func TestQuotaRetryRequiresADueDeadlineAndIsCancelledByUserWork(t *testing.T) {
	for _, action := range []string{"early", "unknown", "cancel", "prompt"} {
		relay := continuationOpen(t, t.TempDir())
		submitRelay(relay, "user-request", prompt("Implement"))
		continuationCompleted(t, relay, "user-request")
		var reset *int64
		switch action {
		case "early":
			now := core.EpochMillis()
			reset = &now
		case "unknown":
		default:
			reset = continuationPastDeadline()
		}
		command := continuationQuotaSchedule(t, relay, reset)
		submitRelay(relay, "quota-schedule", command)
		retry := continuationQuotaResume(t, relay)
		switch action {
		case "cancel":
			submitRelay(relay, "cancel-quota", core.RelayCommandCancel{})
		case "prompt":
			submitRelay(relay, "new-user", prompt("Stop; explain first"))
		}
		continuationRefused(t, relay, "quota-retry", retry, action)
		if (action == "cancel" || action == "prompt") && relay.snapshot.Continuation.QuotaRecovery != nil {
			t.Fatalf("%s: user work withdraws the recovery", action)
		}
	}
}

// F18, session e33442fe: a quota refusal ended three turns the harness had
// started on its own, each scheduled a recovery, and a routine checkpoint
// then cleared the recovery and set quota_suppressed, so nothing resumed at
// the deadline. A checkpoint barrier is housekeeping, not the user taking
// over: the recovery survives it and is submittable once the barrier lifts.
func TestARoutineCheckpointKeepsTheQuotaRecoveryOfASelfStartedTurn(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "user-request", prompt("Implement"))
	continuationCompleted(t, relay, "user-request")
	start := continuationSelfStartedTurn(t, relay)
	// The refused turn is over: nothing about it may hold CanSubmit false.
	facts := relay.ActivityFacts()
	if facts.HarnessTurnStartedAtMs != nil || len(facts.ToolsInFlight) != 0 || !core.CanSubmit(&facts) {
		t.Fatalf("the settled turn still holds the session: %#v", facts)
	}
	command := continuationQuotaSchedule(t, relay, continuationPastDeadline())
	submitRelay(relay, "quota-schedule", command)
	recovery := relay.snapshot.Continuation.QuotaRecovery
	if recovery == nil || recovery.CompletedCommandID != core.HarnessTurnID(start) {
		t.Fatalf("quota recovery = %#v", recovery)
	}

	readyCheckpoint(relay, "routine-checkpoint")
	held := relay.snapshot.Continuation
	if held.QuotaRecovery == nil {
		t.Fatal("the checkpoint kept it")
	}
	if held.QuotaSuppressed {
		t.Fatal("the checkpoint must not suppress quota recovery")
	}
	// While the barrier stands the resume waits, as it must.
	facts = relay.ActivityFacts()
	if core.CanSubmit(&facts) {
		t.Fatal("the barrier holds the resume")
	}
	submitRelay(relay, "release-routine-checkpoint", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "routine-checkpoint"})
	facts = relay.ActivityFacts()
	if !core.CanSubmit(&facts) {
		t.Fatal("the released barrier lets the resume through")
	}
	continuationAccepted(t, relay, "quota-retry", continuationQuotaResume(t, relay))
}

// continuationSelfStartedTurn opens and ends a turn the harness starts on
// its own; it returns its start ordinal.
func continuationSelfStartedTurn(t *testing.T, relay *DurableRelay) uint64 {
	t.Helper()
	if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1}); err != nil {
		t.Fatal(err)
	}
	start := relay.snapshot.LatestOrdinal
	if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnSettled{}); err != nil {
		t.Fatal(err)
	}
	return start
}

func TestASelfStartedTurnBecomesTheCompletedTurn(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "user-request", prompt("Implement and test"))
	continuationCompleted(t, relay, "user-request")

	start := continuationSelfStartedTurn(t, relay)
	c := relay.snapshot.Continuation
	id := core.HarnessTurnID(start)
	if got := continuationString(c.CompletedCommandID); got != id {
		t.Fatalf("completed command = %s, want %s", got, id)
	}
	if c.HarnessTurn == nil || c.HarnessTurn.StartPosition != start || c.HarnessTurn.ID != id {
		t.Fatalf("harness turn = %#v", c.HarnessTurn)
	}
	if got := continuationString(c.UserCommandID); got != "user-request" {
		t.Fatalf("user command = %s", got)
	}
}

func TestAStartInsideOurPromptIsNotATurnOfItsOwn(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "user-request", prompt("Implement"))
	if _, err := relay.ClaimPendingCommands(true); err != nil {
		t.Fatal(err)
	}
	// Codex reports native execution for ordinary replies too, and it can
	// outlast the prompt's own result.
	if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordCommandCompleted("user-request", core.RelayCommandOutcomePrompt{StopReason: "end_turn"}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnSettled{}); err != nil {
		t.Fatal(err)
	}
	c := relay.snapshot.Continuation
	if got := continuationString(c.CompletedCommandID); got != "user-request" {
		t.Fatalf("completed command = %s", got)
	}
	if c.HarnessTurn != nil {
		t.Fatalf("harness turn = %#v", c.HarnessTurn)
	}
}

func TestASecondSelfStartedTurnClearsAWaitingRecoveryAndItsEndCanScheduleAgain(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "user-request", prompt("Implement"))
	continuationCompleted(t, relay, "user-request")
	continuationSelfStartedTurn(t, relay)
	later := core.EpochMillis() + 3_600_000
	command := continuationQuotaSchedule(t, relay, &later)
	submitRelay(relay, "quota-schedule-1", command)
	if relay.snapshot.Continuation.QuotaRecovery == nil {
		t.Fatal("the recovery is scheduled")
	}

	second := continuationSelfStartedTurn(t, relay)
	c := relay.snapshot.Continuation
	if c.QuotaRecovery != nil {
		t.Fatal("a new turn clears the waiting recovery")
	}
	if c.QuotaSuppressed {
		t.Fatal("the new turn's own check may schedule again")
	}
	if got := continuationString(c.CompletedCommandID); got != core.HarnessTurnID(second) {
		t.Fatalf("completed command = %s", got)
	}
	later = core.EpochMillis() + 3_600_000
	command = continuationQuotaSchedule(t, relay, &later)
	submitRelay(relay, "quota-schedule-2", command)
	if recovery := relay.snapshot.Continuation.QuotaRecovery; recovery == nil || recovery.CompletedCommandID != core.HarnessTurnID(second) {
		t.Fatalf("quota recovery = %#v", recovery)
	}
}

func continuationLimitedGoal(relay *DurableRelay, reason string) {
	controlMethod := "_session/goal"
	createdAt := int64(100_000)
	relay.snapshot.Goal.Capability = &core.GoalCapability{
		Version:       1,
		ControlMethod: "_session/goal",
		Actions:       []string{"resume", "pause"},
	}
	relay.snapshot.Goal.Snapshot = &core.GoalSnapshot{
		Objective:     "Finish the campaign",
		Status:        "limited",
		CreatedAt:     &createdAt,
		ControlMethod: &controlMethod,
		Details:       map[string]any{"limitReason": reason},
	}
}

func TestADueRecoveryResumesAUsageLimitedGoalAndASpentBudgetBlocksEverything(t *testing.T) {
	for _, reason := range []string{"usage", "budget"} {
		relay := continuationOpen(t, t.TempDir())
		submitRelay(relay, "user-request", prompt("Implement"))
		continuationCompleted(t, relay, "user-request")
		continuationSelfStartedTurn(t, relay)
		command := continuationQuotaSchedule(t, relay, continuationPastDeadline())
		submitRelay(relay, "quota-schedule", command)
		continuationLimitedGoal(relay, reason)
		resume := core.RelayCommandGoalControl{Action: core.GoalControlActionResume}
		id := fmt.Sprintf("%s7", core.QuotaGoalResumePrefix)
		_, refusal := continuationSubmit(t, relay, id, resume)
		if reason == "budget" {
			if refusal == nil {
				t.Fatal("a spent budget refuses the goal resume")
			}
			continuationRefused(t, relay, "quota-retry-7", continuationQuotaResume(t, relay), "a spent budget")
			continue
		}
		if refusal != nil {
			t.Fatalf("a usage-limited goal resumes: %#v", refusal)
		}
		c := relay.snapshot.Continuation
		if c.QuotaRecovery == nil || !c.QuotaRecovery.Submitted {
			t.Fatalf("quota recovery = %#v", c.QuotaRecovery)
		}
		if c.QuotaSuppressed || c.Suppressed {
			t.Fatal("the automated resume keeps recovery enabled")
		}
		if c.CompletedCommandID != nil {
			t.Fatalf("completed command = %s", *c.CompletedCommandID)
		}
	}
}

func TestAUsersOwnGoalResumeStillTakesOverFromAutomation(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "user-request", prompt("Implement"))
	continuationCompleted(t, relay, "user-request")
	continuationLimitedGoal(relay, "budget")
	submitRelay(relay, "goal-resume-by-user", core.RelayCommandGoalControl{Action: core.GoalControlActionResume})
	c := relay.snapshot.Continuation
	if !c.Suppressed || !c.QuotaSuppressed {
		t.Fatalf("a user's resume takes over: %#v", c)
	}
	// A user's native resume may remain in flight for its entire turn.
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatal(err)
	}
	if len(claimed) == 0 || claimed[0].CommandID != "goal-resume-by-user" {
		t.Fatalf("claimed %#v", claimed)
	}
	submitRelay(relay, "cancel-explicit-resume", core.RelayCommandCancel{})
	cancel, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatal(err)
	}
	if len(cancel) == 0 || cancel[0].CommandID != "cancel-explicit-resume" {
		t.Fatalf("claimed %#v", cancel)
	}
}

func continuationHandbackRequest(t *testing.T, relay *DurableRelay) core.RelayCommand {
	t.Helper()
	turn := relay.snapshot.TurnCompletion
	if turn == nil {
		t.Fatal("no completed turn")
	}
	return core.RelayCommandHandbackReminder{
		CompletedCommandID: turn.CommandID,
		CompletedOrdinal:   turn.CompletedOrdinal,
	}
}

func TestHandbackReminderAdmissionIsDurableAndDeduplicated(t *testing.T) {
	root := t.TempDir()
	relay := continuationOpen(t, root)
	submitRelay(relay, "original-prompt", prompt("Write the report"))
	continuationCompleted(t, relay, "original-prompt")
	request := continuationHandbackRequest(t, relay)
	accepted := continuationAccepted(t, relay, "handback-reminder-test", request)
	want := []acpschema.ContentBlock{relayTextBlock(core.SubagentHandbackReminderText)}
	if active := relay.snapshot.ActivePrompt; active == nil || !reflect.DeepEqual(active.Prompt, want) {
		t.Fatalf("active prompt = %#v", active)
	}
	for _, message := range relay.snapshot.AssessmentContext.Messages {
		if message.Text == core.SubagentHandbackReminderText {
			t.Fatal("the reminder must not enter the assessment context")
		}
	}
	if got := continuationString(relay.snapshot.Continuation.UserCommandID); got != "original-prompt" {
		t.Fatalf("user command = %s", got)
	}
	if again := continuationAccepted(t, relay, "handback-reminder-test", request); !reflect.DeepEqual(again, accepted) {
		t.Fatalf("retry answered %#v, want %#v", again, accepted)
	}
	relay = continuationOpen(t, root)
	if again := continuationAccepted(t, relay, "handback-reminder-test", request); !reflect.DeepEqual(again, accepted) {
		t.Fatalf("retry after restart answered %#v, want %#v", again, accepted)
	}
}

func TestHandbackReminderRejectsNewerInputEvenAfterQueueRemovalAndRestart(t *testing.T) {
	root := t.TempDir()
	relay := continuationOpen(t, root)
	submitRelay(relay, "original-prompt", prompt("Write the report"))
	continuationCompleted(t, relay, "original-prompt")
	request := continuationHandbackRequest(t, relay)
	// Hold input behind a checkpoint so it can be removed before execution.
	readyCheckpoint(relay, "checkpoint-hold")
	submitRelay(relay, "newer-user-prompt", prompt("New instructions"))
	submitRelay(relay, "remove-newer-prompt", core.RelayCommandClearQueuedPrompts{})
	submitRelay(relay, "release-checkpoint", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "checkpoint-hold"})
	relay = continuationOpen(t, root)
	facts := relay.ActivityFacts()
	if !core.CanSubmit(&facts) {
		t.Fatal("the session can take a prompt")
	}
	continuationRefused(t, relay, "handback-reminder-stale", request, "a reminder after newer input")
}

func TestHandbackReminderRejectsADifferentCompletedTurn(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "original-prompt", prompt("Write the report"))
	continuationCompleted(t, relay, "original-prompt")
	request := continuationHandbackRequest(t, relay)
	submitRelay(relay, "newer-user-prompt", prompt("New instructions"))
	continuationCompleted(t, relay, "newer-user-prompt")
	continuationRefused(t, relay, "handback-reminder-stale", request, "a reminder for an older turn")
}

func TestHandbackReminderRejectsANewerNativeTurnAfterItSettles(t *testing.T) {
	relay := continuationOpen(t, t.TempDir())
	submitRelay(relay, "original-prompt", prompt("Write the report"))
	continuationCompleted(t, relay, "original-prompt")
	request := continuationHandbackRequest(t, relay)
	if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: 1}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnSettled{}); err != nil {
		t.Fatal(err)
	}
	continuationRefused(t, relay, "handback-reminder-stale", request, "a reminder after a native turn")
}

func TestLegacySubagentWaitJournalCommandIsANoopAfterReplay(t *testing.T) {
	root := t.TempDir()
	relay := continuationOpen(t, root)
	submitRelay(relay, "parent-turn", prompt("Delegate the work"))
	command, err := core.UnmarshalRelayCommandJSON([]byte(`{"type":"set_subagent_wait","data":{"active_prompt_id":"parent-turn", "waiting":true}}`))
	if err != nil {
		t.Fatal(err)
	}
	submitRelay(relay, "legacy-wait", command)
	if relay.snapshot.Continuation.AwaitingChildrenSinceMs != nil {
		t.Fatal("a legacy wait no longer marks the parent")
	}
	continuationCompleted(t, relay, "parent-turn")
	encoded, err := json.Marshal(relay.snapshot)
	if err != nil {
		t.Fatal(err)
	}
	var legacy map[string]any
	if err := json.Unmarshal(encoded, &legacy); err != nil {
		t.Fatal(err)
	}
	legacy["continuation"].(map[string]any)["awaiting_children_since_ms"] = 123
	encoded, err = json.Marshal(legacy)
	if err != nil {
		t.Fatal(err)
	}
	var decoded core.RelaySnapshot
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	if since := decoded.Continuation.AwaitingChildrenSinceMs; since == nil || *since != 123 {
		t.Fatalf("decoded awaiting_children_since_ms = %v", since)
	}
	relay = continuationOpen(t, root)
	if relay.snapshot.Continuation.AwaitingChildrenSinceMs != nil {
		t.Fatal("replay keeps the legacy wait a no-op")
	}
}

func TestALegacyAutomaticContinuationIsRefusedLiveButItsJournalEntryStillReplays(t *testing.T) {
	root := t.TempDir()
	relay := continuationOpen(t, root)
	submitRelay(relay, "user-request", prompt("Implement and test"))
	continuationCompleted(t, relay, "user-request")
	state := relay.OperationalState()
	command := core.RelayCommandContinueAuthorizedWork{
		Expected:           core.RelayCursor{Ordinal: state.LatestOrdinal, Digest: state.LatestDigest},
		UserCommandID:      "user-request",
		CompletedCommandID: "user-request",
		Attempt:            2,
	}
	continuationRefused(t, relay, "auto-continue-live", command, "a live automatic continuation")
	// An older worker journalled the command before accepting it.
	commandID := "auto-continue-old"
	if _, err := relay.appendRelayEvent(&commandID, core.RelayObservationCommandQueued{
		CommandID:   commandID,
		Command:     command,
		CreatedAtMs: 1,
	}); err != nil {
		t.Fatal(err)
	}
	c := relay.snapshot.Continuation
	if c.Attempts != 2 || c.CompletedCommandID != nil {
		t.Fatalf("continuation = attempts %d, completed %v", c.Attempts, c.CompletedCommandID)
	}
	relay = continuationOpen(t, root)
	c = relay.snapshot.Continuation
	if c.Attempts != 2 || c.CompletedCommandID != nil {
		t.Fatalf("replayed continuation = attempts %d, completed %v", c.Attempts, c.CompletedCommandID)
	}
	if got := continuationString(c.UserCommandID); got != "user-request" {
		t.Fatalf("user command = %s", got)
	}
}
