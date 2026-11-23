// Port of the tests in mj-core/src/subagent.rs.

package core

import (
	"encoding/json"
	"math"
	"math/big"
	"reflect"
	"slices"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// subagentTestJSONEqual compares encoded JSON with the expected JSON text as
// serde_json Values compare.
func subagentTestJSONEqual(t *testing.T, got []byte, want string) {
	t.Helper()
	if !transcriptValueEqual(transcriptParse(got), transcriptParse([]byte(want))) {
		t.Fatalf("got %s, want %s", got, want)
	}
}

func subagentTestEncode(t *testing.T, v any) []byte {
	t.Helper()
	encoded, err := transcriptMarshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func subagentTestDecodeAction(t *testing.T, text string) SubagentToolAction {
	t.Helper()
	action, err := DecodeSubagentToolAction([]byte(text))
	if err != nil {
		t.Fatalf("decode %s: %v", text, err)
	}
	return action
}

func TestRequestReviewPathPreservesLegacyEncoding(t *testing.T) {
	old := `{"action":"request_review","params":{}}`
	action := subagentTestDecodeAction(t, old)
	if review, ok := action.(SubagentToolActionRequestReview); !ok || len(review.Paths) != 0 {
		t.Fatalf("got %#v", action)
	}
	if got := string(subagentTestEncode(t, action)); got != old {
		t.Fatalf("got %s", got)
	}
	// A request an older worker queued named one worktree.
	queued := `{"action":"request_review","params":{"path":"/worktree"}}`
	if got := subagentTestDecodeAction(t, queued); !reflect.DeepEqual(got, SubagentToolAction(SubagentToolActionRequestReview{Paths: []string{"/worktree"}})) {
		t.Fatalf("got %#v", got)
	}
	scoped := SubagentToolActionRequestReview{Paths: []string{"/worktree", "/other"}}
	decoded := subagentTestDecodeAction(t, string(subagentTestEncode(t, scoped)))
	if !reflect.DeepEqual(decoded, SubagentToolAction(scoped)) {
		t.Fatalf("got %#v", decoded)
	}
}

// A request queued by an older worker carries one whole-verdict reason.
func TestAQueuedOverrideWithOneReasonDecodesAsOnePair(t *testing.T) {
	old := `{"action":"override_review","params":{"reason":"wrong premise"}}`
	action := subagentTestDecodeAction(t, old)
	want := SubagentToolActionOverrideReview{Overrides: []ReviewOverride{{
		Finding: LegacyWholeVerdictFinding,
		Reason:  "wrong premise",
	}}}
	if !reflect.DeepEqual(action, SubagentToolAction(want)) {
		t.Fatalf("got %#v", action)
	}
	pairs := `{"action":"override_review","params":{"overrides":[{"finding":"F","reason":"R"}]}}`
	action = subagentTestDecodeAction(t, pairs)
	again := subagentTestDecodeAction(t, string(subagentTestEncode(t, action)))
	if !reflect.DeepEqual(again, action) {
		t.Fatalf("got %#v, want %#v", again, action)
	}
}

func TestOnlyChildMutationsHoldAnInPlaceMoveDrain(t *testing.T) {
	mutations := []SubagentToolAction{
		SubagentToolActionSpawn{
			TaskType:     SubagentTaskTypeStraightforward,
			TaskName:     "task",
			Instructions: "work",
		},
		SubagentToolActionSendInput{ChildSessionID: "child", Message: "continue"},
		SubagentToolActionSendMessage{ChildSessionID: "child", Message: "legacy note"},
		SubagentToolActionLegacyInterruptAgent{ChildSessionID: "child"},
		SubagentToolActionCloseAgent{ChildSessionID: "child"},
		SubagentToolActionOverrideReview{Overrides: []ReviewOverride{{Finding: "finding", Reason: "reason"}}},
		SubagentToolActionSetReviewBaseline{Commit: "abc123"},
	}
	for _, mutation := range mutations {
		if !mutation.MutatesChildState() {
			t.Fatalf("%#v does not mutate", mutation)
		}
	}
	for _, observation := range []SubagentToolAction{
		SubagentToolActionListProfiles{},
		SubagentToolActionListAgents{},
		SubagentToolActionLegacyWaitAgents{},
		SubagentToolActionLegacyAwaitChildren{},
		SubagentToolActionHandback{Message: "done"},
	} {
		if observation.MutatesChildState() {
			t.Fatalf("%#v mutates", observation)
		}
	}
}

func TestPoliciesPreserveLegacyRecordsButPublicPolicyRejectsBooleans(t *testing.T) {
	// The Rust test decodes a struct whose `subagents` field (alias
	// `mjolnir_subagents`) uses deserialize_optional_policy.
	stored := func(text string) *SubagentPolicy {
		t.Helper()
		var members map[string]json.RawMessage
		if err := json.Unmarshal([]byte(text), &members); err != nil {
			t.Fatal(err)
		}
		raw, ok := members["subagents"]
		if !ok {
			raw, ok = members["mjolnir_subagents"]
		}
		if !ok {
			return nil
		}
		policy, err := DeserializeOptionalPolicy(raw)
		if err != nil {
			t.Fatalf("%s: %v", text, err)
		}
		return policy
	}
	mjolnir := &SubagentPolicy{Mode: SubagentPolicyMjolnir}
	for _, c := range []struct {
		json     string
		expected *SubagentPolicy
	}{
		{`{"mjolnir_subagents":true}`, mjolnir},
		{`{"mjolnir_subagents":false}`, &SubagentPolicy{Mode: SubagentPolicyNative}},
		{`{"subagents":{"mode":"all_models"}}`, mjolnir},
		{`{"subagents":{"mode":"single_model","model":"chosen","effort":"adaptive"}}`, mjolnir},
		{`{"subagents":{"mode":"single_model","model":"chosen","effort":"high"}}`, mjolnir},
		{"{}", nil},
	} {
		if got := stored(c.json); !reflect.DeepEqual(got, c.expected) {
			t.Fatalf("%s: got %#v, want %#v", c.json, got, c.expected)
		}
	}
	var rejected SubagentPolicy
	if json.Unmarshal([]byte("true"), &rejected) == nil {
		t.Fatal("a boolean policy decoded")
	}
	maxEffort := "max"
	fixed := SubagentPolicy{
		Mode:            SubagentPolicyMjolnir,
		Straightforward: &SubagentChoice{Model: "gpt-6-luna", Effort: &maxEffort},
		Subtle:          &SubagentChoice{Model: "gpt-6-sol"},
	}
	subagentTestJSONEqual(t, subagentTestEncode(t, SubagentPolicy{Mode: SubagentPolicyMjolnir}), `{"mode":"mjolnir"}`)
	subagentTestJSONEqual(t, subagentTestEncode(t, fixed),
		`{"mode":"mjolnir", "straightforward":{"model":"gpt-6-luna", "effort":"max"}, "subtle":{"model":"gpt-6-sol"}}`)
	defaultEffort := SubagentPolicy{
		Mode:            SubagentPolicyMjolnir,
		Straightforward: &SubagentChoice{Model: "gpt-6-luna"},
	}
	subagentTestJSONEqual(t, subagentTestEncode(t, defaultEffort),
		`{"mode":"mjolnir", "straightforward":{"model":"gpt-6-luna"}}`)
	for _, invalid := range []string{
		`{"mode":"mjolnir", "model":"chosen", "effort":"adaptive"}`,
		`{"mode":"mjolnir", "effort":"high"}`,
		`{"mode":"mjolnir", "subtle":{"model":"chosen", "effort":"adaptive"}}`,
	} {
		var policy SubagentPolicy
		if json.Unmarshal([]byte(invalid), &policy) == nil {
			t.Fatalf("%s decoded", invalid)
		}
	}
	var oldFlat SubagentPolicy
	if err := json.Unmarshal([]byte(`{"mode":"mjolnir", "model":"gpt-6-luna", "effort":"max"}`), &oldFlat); err != nil {
		t.Fatal(err)
	}
	duplicated := SubagentChoice{Model: "gpt-6-luna", Effort: &maxEffort}
	duplicatedToo := duplicated
	want := SubagentPolicy{Mode: SubagentPolicyMjolnir, Straightforward: &duplicated, Subtle: &duplicatedToo}
	if !reflect.DeepEqual(oldFlat, want) {
		t.Fatalf("got %#v", oldFlat)
	}
	for _, harness := range []HarnessKind{HarnessKindClaude, HarnessKindCodex, HarnessKindDsh, HarnessKindOpenCode} {
		for _, policy := range []SubagentPolicy{
			{Mode: SubagentPolicyNative},
			{Mode: SubagentPolicyMjolnir},
			fixed,
			defaultEffort,
			{Mode: SubagentPolicyNone},
		} {
			if got := policy.ForLaunch(harness, true); !reflect.DeepEqual(got, SubagentPolicy{Mode: SubagentPolicyNone}) {
				t.Fatalf("%s child: got %#v", harness, got)
			}
			if got := policy.ForLaunch(harness, false); !reflect.DeepEqual(got, policy) {
				t.Fatalf("%s parent: got %#v", harness, got)
			}
			var decoded SubagentPolicy
			if err := json.Unmarshal(subagentTestEncode(t, policy), &decoded); err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(decoded, policy) {
				t.Fatalf("round trip: got %#v, want %#v", decoded, policy)
			}
		}
	}
}

func subagentTestFinished(commandID, stopReason string) MaterializedTurnOutcome {
	one := uint64(1)
	position := uint64(1)
	return MaterializedTurnOutcome{
		CommandID:         commandID,
		AcceptedOrdinal:   &one,
		TurnStartPosition: &position,
		CompletedOrdinal:  2,
		CompletedAtMs:     10,
		Outcome:           TurnOutcomeKindCompleted{StopReason: stopReason},
	}
}

func subagentTestHandback(commandID string) *SubagentHandback {
	return &SubagentHandback{CommandID: commandID, Message: "the report", RecordedAtMs: 5}
}

func subagentTestReminder(forCommandID string) *HandbackReminder {
	return &HandbackReminder{CommandID: "handback-reminder-r", ForCommandID: forCommandID, SentAtMs: 1_000}
}

var subagentTestKnownNone = Liveness{Known: true}

func subagentTestReportState(t *testing.T, got, want ReportState) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %#v, want %#v", got, want)
	}
}

func TestAChildWithoutTheToolOrATurnReportsItsLastMessage(t *testing.T) {
	turn := subagentTestFinished("task", "end_turn")
	report := SubagentReport{}
	subagentTestReportState(t, ReportStateOf(false, &report, &turn, &subagentTestKnownNone, nil, 0), ReportStateFallback{})
	subagentTestReportState(t, ReportStateOf(true, &report, nil, &subagentTestKnownNone, nil, 0), ReportStateFallback{})
}

// Hard-won: ebb2e9611: a handback recorded before restart lost its completed turn.
func TestAHandbackForTheLastTurnIsTheReportWhateverTheTurnDid(t *testing.T) {
	report := SubagentReport{Handback: subagentTestHandback("task")}
	recovered := RestartContinuationID("task")
	if !report.Handback.Answers(recovered) {
		t.Fatal("the handback does not answer the recovered turn")
	}
	turn := subagentTestFinished(recovered, "end_turn")
	subagentTestReportState(t, ReportStateOf(true, &report, &turn, &subagentTestKnownNone, nil, 0),
		ReportStateDelivered{Message: "the report"})
	for _, stopReason := range []string{"end_turn", "cancelled", "refusal"} {
		turn := subagentTestFinished("task", stopReason)
		subagentTestReportState(t, ReportStateOf(true, &report, &turn, &subagentTestKnownNone, nil, 0),
			ReportStateDelivered{Message: "the report"})
	}
	// A report for an earlier task does not answer this one.
	next := subagentTestFinished("next", "end_turn")
	subagentTestReportState(t, ReportStateOf(true, &report, &next, &subagentTestKnownNone, nil, 0),
		ReportStatePending{Remind: true})
}

func TestOnlyANormallyFinishedTaskTurnIsOwedAReminder(t *testing.T) {
	report := SubagentReport{}
	task := subagentTestFinished("task", "end_turn")
	subagentTestReportState(t, ReportStateOf(true, &report, &task, &subagentTestKnownNone, nil, 0),
		ReportStatePending{Remind: true})
	for _, stopReason := range []string{"cancelled", "awaiting_input", "quota_limit", "max_tokens"} {
		turn := subagentTestFinished("task", stopReason)
		subagentTestReportState(t, ReportStateOf(true, &report, &turn, &subagentTestKnownNone, nil, 0),
			ReportStateFallback{})
	}
	interrupted := subagentTestFinished("task", "end_turn")
	interrupted.Outcome = TurnOutcomeKindInterrupted{Message: "stopped"}
	subagentTestReportState(t, ReportStateOf(true, &report, &interrupted, &subagentTestKnownNone, nil, 0),
		ReportStateFallback{})
}

func TestAChildGetsOneReminderAndThenReportsItsLastMessage(t *testing.T) {
	reminded := SubagentReport{Reminder: subagentTestReminder("task")}
	task := subagentTestFinished("task", "end_turn")
	// Sent and queued, or sent a moment ago: it is on its way.
	subagentTestReportState(t, ReportStateOf(true, &reminded, &task, &subagentTestKnownNone,
		[]string{"handback-reminder-r"}, 60_000), ReportStatePending{Remind: false})
	subagentTestReportState(t, ReportStateOf(true, &reminded, &task, &subagentTestKnownNone,
		nil, 1_000+HandbackReminderGraceMs-1), ReportStatePending{Remind: false})
	// Neither queued, running nor finished long after it was sent.
	subagentTestReportState(t, ReportStateOf(true, &reminded, &task, &subagentTestKnownNone,
		nil, 1_000+HandbackReminderGraceMs), ReportStateFallback{})
	// The reminder turn itself ended without a report.
	reminderTurn := subagentTestFinished("handback-reminder-r", "end_turn")
	subagentTestReportState(t, ReportStateOf(true, &reminded, &reminderTurn, &subagentTestKnownNone, nil, 2_000),
		ReportStateFallback{})
	// A reminder that could not be sent.
	taskID := "task"
	failed := SubagentReport{ReminderFailedFor: &taskID}
	subagentTestReportState(t, ReportStateOf(true, &failed, &task, &subagentTestKnownNone, nil, 0),
		ReportStateFallback{})
}

// Hard-won: 7c195cfd8: recovered reminders lost their reminder and give-up identity.
func TestAReminderTurnThatLeftProcessesRunningHoldsTheChild(t *testing.T) {
	for _, recovering := range []bool{false, true} {
		reminded := SubagentReport{Reminder: subagentTestReminder("task")}
		reminderID := "handback-reminder-r"
		if recovering {
			reminderID = RestartContinuationID(RestartContinuationID("handback-reminder-r"))
		}
		reminderTurn := subagentTestFinished(reminderID, "end_turn")
		ordinal := reminderTurn.CompletedOrdinal
		live := func(ordinal uint64, alive uint32) WorkLiveness {
			return WorkLiveness{CompletedOrdinal: ordinal, Alive: alive}
		}
		known := func(liveness WorkLiveness) *Liveness { return &Liveness{Known: true, Liveness: &liveness} }
		held := func(nowMs int64, liveness *Liveness) ReportState {
			return ReportStateOf(true, &reminded, &reminderTurn, liveness, nil, nowMs)
		}
		giveUpAtMs := reminderTurn.CompletedAtMs + HoldGiveUpMs
		hold := func(followUp *HoldFollowUpKind) ReportState {
			return ReportStateHeld{
				SinceMs:    reminderTurn.CompletedAtMs,
				GiveUpAtMs: giveUpAtMs,
				WaitingOn:  []string{},
				FollowUp:   followUp,
			}
		}
		giveUpKind := HoldFollowUpKindGiveUp
		exitedKind := HoldFollowUpKindProcessesExited
		subagentTestReportState(t, held(giveUpAtMs-1, known(live(ordinal, 2))), hold(nil))
		subagentTestReportState(t, held(giveUpAtMs, known(live(ordinal, 2))), hold(&giveUpKind))
		subagentTestReportState(t, held(5, known(live(ordinal, 0))), hold(&exitedKind))
		// A worker that publishes nothing, or liveness of another turn, never holds.
		subagentTestReportState(t, held(5, &Liveness{Known: true}), ReportStateFallback{})
		subagentTestReportState(t, held(5, known(live(ordinal+1, 2))), ReportStateFallback{})
		// A reader with no worker to ask cannot say the child is not waiting.
		subagentTestReportState(t, held(5, &Liveness{}), ReportStatePending{Remind: false})
		// A follow-up already on its way is pending, not due again.
		wake := HoldFollowUpCommandID(HoldFollowUpKindProcessesExited, ordinal)
		subagentTestReportState(t, ReportStateOf(true, &reminded, &reminderTurn, known(live(ordinal, 0)),
			[]string{wake}, 5), ReportStatePending{Remind: false})
		// The give-up turn ends the hold; a wake turn is an ordinary task turn
		// that owes a reminder, so the child can be held again.
		giveUp := HoldFollowUpCommandID(HoldFollowUpKindGiveUp, ordinal)
		if recovering {
			giveUp = RestartContinuationID(giveUp)
		}
		giveUpTurn := subagentTestFinished(giveUp, "end_turn")
		subagentTestReportState(t, ReportStateOf(true, &reminded, &giveUpTurn, &subagentTestKnownNone, nil, 5),
			ReportStateFallback{})
		wakeTurn := subagentTestFinished(wake, "end_turn")
		subagentTestReportState(t, ReportStateOf(true, &reminded, &wakeTurn, &subagentTestKnownNone, nil, 5),
			ReportStatePending{Remind: true})
		if kind, got, ok := HoldFollowUpOf(wake); !ok || kind != HoldFollowUpKindProcessesExited || got != ordinal {
			t.Fatalf("wake: %v %v %v", kind, got, ok)
		}
		if kind, got, ok := HoldFollowUpOf(giveUp); !ok || kind != HoldFollowUpKindGiveUp || got != ordinal {
			t.Fatalf("give up: %v %v %v", kind, got, ok)
		}
		if got, ok := giveUpTurn.AnsweredOrdinal(); !ok || got != ordinal {
			t.Fatalf("answered ordinal: %v %v", got, ok)
		}
	}
}

// Hard-won: a91cfd04: A live wait reported a child completed 45 ms before its first turn and hid failures.
func TestAChildIsNotDoneUntilAFinishedTurnReachesTheNewestPrompt(t *testing.T) {
	turn := subagentTestFinished("task", "end_turn")
	twenty := uint64(20)
	turn.AcceptedOrdinal = &twenty
	fortyFive := uint64(45)
	if AwaitingPrompt(nil, &turn) {
		t.Fatal("no prompt is awaited")
	}
	if !AwaitingPrompt(&fortyFive, nil) {
		t.Fatal("no turn has finished yet")
	}
	if !AwaitingPrompt(&fortyFive, &turn) {
		t.Fatal("the finished turn is older")
	}
	// A prompt steered into the running turn is answered by that turn, which
	// then carries the steered prompt's ordinal.
	turn.AcceptedOrdinal = &fortyFive
	if AwaitingPrompt(&fortyFive, &turn) {
		t.Fatal("the steered prompt was answered")
	}
}

// Hard-won: a91cfd04: A child failing on a usage limit looked completed and its parent never learned why.
func TestAFailedTurnSaysWhyAndAFinishedOneDidNotFail(t *testing.T) {
	text := func(value string) *string { return &value }
	check := func(turn MaterializedTurnOutcome, lastMessage *string, wantState, wantReason string, wantFailed bool) {
		t.Helper()
		state, reason, failed := FailedTurn(&turn, lastMessage)
		if state != wantState || reason != wantReason || failed != wantFailed {
			t.Fatalf("got (%q, %q, %v)", state, reason, failed)
		}
	}
	check(subagentTestFinished("t", "end_turn"), text("done"), "", "", false)
	check(subagentTestFinished("t", "awaiting_input"), text("?"), "", "", false)
	failure := subagentTestFinished("t", "error")
	check(failure, text("You've hit your usage limit."), "failed", "You've hit your usage limit.", true)
	failure.Diagnostic = &TurnDiagnostic{Message: "usageLimitExceeded"}
	check(failure, text("You've hit your usage limit."), "failed", "usageLimitExceeded", true)
	check(subagentTestFinished("t", "cancelled"), nil, "interrupted", "the turn was cancelled", true)
}

// #1160: a child whose profile could not sign in died on its first request,
// and its parent read the error as the child's report. The turns below are
// the shapes that failure takes.
// Hard-won: 1b077959: A live Codex login refusal looked like task output and caused repeated failed spawns.
func TestATurnTheProviderRefusedForItsLoginIsALoginFailure(t *testing.T) {
	failedWith := func(message string, code *string) *MaterializedTurnOutcome {
		turn := subagentTestFinished("t", "error")
		turn.Diagnostic = &TurnDiagnostic{Message: message, Code: code}
		return &turn
	}
	code := func(value string) *string { return &value }
	// Codex could not refresh its ChatGPT login (R14-1).
	if !TurnFailedOnLogin(failedWith("Your access token could not be refreshed. Please log out and sign in again.", code("unauthorized"))) {
		t.Fatal("refresh failure")
	}
	// Codex sent an API key the backend rejected.
	if !TurnFailedOnLogin(failedWith("unexpected status 401 Unauthorized: Incorrect API key provided: sk-svca****fvMA.", code("-32603"))) {
		t.Fatal("rejected key")
	}
	// The error kind alone is enough.
	if !TurnFailedOnLogin(failedWith("Your session ended.", code("unauthorized"))) {
		t.Fatal("error kind")
	}
	// A usage limit, another failure, and a turn that finished are not.
	if TurnFailedOnLogin(failedWith("You've hit your usage limit.", code("usageLimitExceeded"))) {
		t.Fatal("usage limit")
	}
	if TurnFailedOnLogin(failedWith("stream disconnected before completion", code("responseStreamDisconnected"))) {
		t.Fatal("disconnect")
	}
	finished := subagentTestFinished("t", "end_turn")
	if TurnFailedOnLogin(&finished) {
		t.Fatal("finished")
	}
	cancelled := subagentTestFinished("t", "cancelled")
	if TurnFailedOnLogin(&cancelled) {
		t.Fatal("cancelled")
	}
	if got := LoginInvalidReason("codex4"); got != "the login is no longer valid; run `mj login --profile codex4` and spawn again" {
		t.Fatalf("got %q", got)
	}
}

func TestARecordWithoutTheToolFlagReadsAsHavingNoTool(t *testing.T) {
	var record SubagentRecord
	if err := json.Unmarshal([]byte(`{"child_session_id":"c","parent_session_id":"p","task_name":"t","profile_id":"pr","working_directory":".","initial_prompt":"i","request_key":"k","created_at":"2026-09-15"}`), &record); err != nil {
		t.Fatalf("records written before the tool existed remain readable: %v", err)
	}
	if record.HandbackTool {
		t.Fatal("the record has the tool")
	}
	encoded := subagentTestEncode(t, record)
	if _, ok := transcriptGet(transcriptParse(encoded), "handback_tool"); ok {
		t.Fatalf("%s", encoded)
	}
}

func TestNoticedTurnKeepsTheStoredDeliveredTurnFieldName(t *testing.T) {
	var record SubagentRecord
	if err := json.Unmarshal([]byte(`{"child_session_id":"c","parent_session_id":"p","task_name":"t","profile_id":"pr","working_directory":".","initial_prompt":"i","request_key":"k","created_at":"2026-09-15","delivered_turn":3}`), &record); err != nil {
		t.Fatalf("stored relation payloads remain readable: %v", err)
	}
	if record.NoticedTurn == nil || *record.NoticedTurn != 3 {
		t.Fatalf("got %v", record.NoticedTurn)
	}
	encoded := transcriptParse(subagentTestEncode(t, record))
	if delivered, _ := transcriptGet(encoded, "delivered_turn"); !jsonvalue.Equal(delivered, 3) {
		t.Fatalf("delivered_turn is %v", delivered)
	}
	if _, ok := transcriptGet(encoded, "noticed_turn"); ok {
		t.Fatal("the wire field name must stay historical")
	}
}

func TestTerminalFinishIdentityIgnoresLegacyTimestampAndTracksCompletedTurns(t *testing.T) {
	decode := func(text string) SubagentFinishIdentity {
		t.Helper()
		var identity SubagentFinishIdentity
		if err := json.Unmarshal([]byte(text), &identity); err != nil {
			t.Fatal(err)
		}
		return identity
	}
	old := decode(`{"kind":"terminal","state":"error","detail":"startup failed","updated_at":"2026-10-06T12:00:00Z"}`)
	detail := "startup failed"
	if !reflect.DeepEqual(old, SubagentFinishIdentity{Kind: SubagentFinishIdentityTerminal, State: "error", Detail: &detail}) {
		t.Fatalf("got %#v", old)
	}
	before := decode(`{"kind":"terminal","state":"error","detail":"startup failed","last_completed_ordinal":8}`)
	after := decode(`{"kind":"terminal","state":"error","detail":"startup failed","last_completed_ordinal":9}`)
	if reflect.DeepEqual(before, after) {
		t.Fatal("distinct runs are equal")
	}
	if _, ok := transcriptGet(transcriptParse(subagentTestEncode(t, old)), "updated_at"); ok {
		t.Fatal("updated_at was written")
	}
}

func TestReviewToolsBelongToTheReviewServerAndNoMjAgentsRole(t *testing.T) {
	if ReviewMCPServer != "mj-review" {
		t.Fatal(ReviewMCPServer)
	}
	if !slices.Equal(ReviewMCPTools, []string{"request_review", "override_review", "set_baseline"}) {
		t.Fatal(ReviewMCPTools)
	}
	for _, role := range []SubagentMcpRole{SubagentMcpRoleParent, SubagentMcpRoleMessageOnly, SubagentMcpRoleFetchOnly, SubagentMcpRoleChild} {
		for _, tool := range ReviewMCPTools {
			if slices.Contains(role.ToolNames(), tool) {
				t.Fatalf("%s lists %s", role, tool)
			}
		}
	}
}

func TestAnOldInterruptRequestRemainsReadableButIsNotAnOfferedTool(t *testing.T) {
	var old SubagentToolRequest
	if err := json.Unmarshal([]byte(`{"request_id":"old-interrupt","created_at_ms":1,"action":{"action":"interrupt_agent","params":{"child_session_id":"child"}}}`), &old); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(old.Action, SubagentToolAction(SubagentToolActionLegacyInterruptAgent{ChildSessionID: "child"})) {
		t.Fatalf("got %#v", old.Action)
	}
	if slices.Contains(SubagentMcpRoleParent.ToolNames(), "interrupt") {
		t.Fatal("interrupt is offered")
	}
}

func TestAPersistedSendMessageRequestRemainsReadable(t *testing.T) {
	var old SubagentToolRequest
	if err := json.Unmarshal([]byte(`{"request_id":"old-message","created_at_ms":1,"action":{"action":"send_message","params":{"child_session_id":"child","message":"continue"}}}`), &old); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(old.Action, SubagentToolAction(SubagentToolActionSendMessage{ChildSessionID: "child", Message: "continue"})) {
		t.Fatalf("got %#v", old.Action)
	}
	action, _ := transcriptGet(transcriptParse(subagentTestEncode(t, old)), "action")
	encodedAction, _ := jsonvalue.Marshal(action)
	subagentTestJSONEqual(t, encodedAction, `{"action":"send_message","params":{"child_session_id":"child","message":"continue"}}`)
	var cachedAlias SubagentToolRequest
	if err := json.Unmarshal([]byte(`{"request_id":"cached-input","created_at_ms":2,"action":{"action":"send_input","params":{"child_session_id":"child","message":"continue too"}}}`), &cachedAlias); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(cachedAlias.Action, SubagentToolAction(SubagentToolActionSendInput{ChildSessionID: "child", Message: "continue too"})) {
		t.Fatalf("got %#v", cachedAlias.Action)
	}
	tools := SubagentMcpRoleParent.ToolNames()
	if !slices.Contains(tools, "send_message") || slices.Contains(tools, "send_input") || slices.Contains(tools, "interrupt") {
		t.Fatal(tools)
	}
}

func TestOracleQuestionIsAMutatingDurableToolAction(t *testing.T) {
	action := SubagentToolActionAskOracle{Message: "Choose B?"}
	if !action.MutatesChildState() {
		t.Fatal("ask_oracle does not mutate")
	}
	wire := subagentTestEncode(t, action)
	subagentTestJSONEqual(t, wire, `{"action":"ask_oracle","params":{"message":"Choose B?"}}`)
	if decoded := subagentTestDecodeAction(t, string(wire)); !reflect.DeepEqual(decoded, SubagentToolAction(action)) {
		t.Fatalf("got %#v", decoded)
	}
}

func TestWaitSerializesWithoutParamsAndOldShapesDeserialize(t *testing.T) {
	wait := SubagentToolActionLegacyWaitAgents{}
	subagentTestJSONEqual(t, subagentTestEncode(t, wait), `{"action":"wait_agents"}`)
	if decoded := subagentTestDecodeAction(t, `{"action":"await_children"}`); !reflect.DeepEqual(decoded, SubagentToolAction(SubagentToolActionLegacyAwaitChildren{})) {
		t.Fatalf("got %#v", decoded)
	}
	for _, role := range []SubagentMcpRole{SubagentMcpRoleParent, SubagentMcpRoleChild} {
		if slices.Contains(role.ToolNames(), "wait") || slices.Contains(role.ToolNames(), "await_children") {
			t.Fatal(role.ToolNames())
		}
	}
	for _, oldShape := range []string{
		`{"action":"wait_agents"}`,
		`{"action":"wait_agents","params":{}}`,
		`{"action":"wait_agents","params":{"child_session_ids":["c1"],"timeout_seconds":5,"return_when":"any"}}`,
	} {
		if decoded := subagentTestDecodeAction(t, oldShape); !reflect.DeepEqual(decoded, SubagentToolAction(wait)) {
			t.Fatalf("%s: got %#v", oldShape, decoded)
		}
	}
}

func TestAWaitRequestRejectsUnknownFieldsAfterRetiredFieldsAreStripped(t *testing.T) {
	_, err := DecodeSubagentToolAction([]byte(`{"action":"wait_agents","params":{"unexpected":true}}`))
	if err == nil || !strings.Contains(err.Error(), "WaitAgents") {
		t.Fatalf("got %v", err)
	}
	_, err = DecodeSubagentToolAction([]byte(`{"action":"send_input","params":{"child_session_id":"c1","message":"x","unexpected":true}}`))
	if err == nil || !strings.Contains(err.Error(), "unexpected") {
		t.Fatalf("got %v", err)
	}
}

func TestChildReportSharesFitTheCheckpointEvenAtConfigurationBoundaries(t *testing.T) {
	for _, concurrent := range []int{0, 1, 6, 7, math.MaxInt} {
		limit := MaxChildReportBytes(concurrent)
		product := new(big.Int).Mul(new(big.Int).SetUint64(limit), big.NewInt(int64(max(concurrent, 1))))
		if product.Cmp(new(big.Int).SetUint64(MaxCheckpointReportBytes)) > 0 {
			t.Fatalf("%d children overflow the checkpoint", concurrent)
		}
	}
	if MaxChildReportBytes(0) != MaxCheckpointReportBytes {
		t.Fatal(MaxChildReportBytes(0))
	}
	if MaxChildReportBytes(6) != 11_184_810 {
		t.Fatal(MaxChildReportBytes(6))
	}
}

// Hard-won: cc7e6a2a: Real child reports of 27 to 62 KB were inlined into waits and triggered 21 repeated polls.
func TestALongReportIsCutOnACharacterBoundaryAndSaysHowToGetTheRest(t *testing.T) {
	if cut, truncated := BoundedReport("done"); cut != "done" || truncated {
		t.Fatalf("got %q %v", cut, truncated)
	}
	exact := strings.Repeat("é", MaxHandbackChars)
	if cut, truncated := BoundedReport(exact); cut != exact || truncated {
		t.Fatal("an exact report was cut")
	}
	long := strings.Repeat("é", MaxHandbackChars+25)
	cut, truncated := BoundedReport(long)
	if !truncated || !strings.HasPrefix(cut, exact) ||
		!strings.Contains(cut, "[truncated: 25 more characters") || !strings.Contains(cut, "send_message") {
		t.Fatalf("got %q %v", cut, truncated)
	}
}

func TestATaskSummaryIsOneLineOfTheParentsOwnInstructions(t *testing.T) {
	reportDir := "/workspace/p/.mj-agents/c1"
	summary := func(prompt string, directory *string) string {
		t.Helper()
		line, ok := TaskSummary(prompt, directory)
		if !ok {
			t.Fatalf("no summary of %q", prompt)
		}
		return line
	}
	const want = "Fix the parser. Keep the tests green."
	prompt := HandbackPromptNote(reportDir) + "\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>\n\n--- source \"a.rs\", lines 1-2 (one-based, inclusive) ---\nfn a() {}"
	if got := summary(prompt, &reportDir); got != want {
		t.Fatalf("got %q", got)
	}
	assignment := ReportDirectoryNote(reportDir) + "\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>"
	if got := summary(assignment, &reportDir); got != want {
		t.Fatalf("got %q", got)
	}
	consultation := ReportDirectoryNote(reportDir) + "\n\n" + ReadOnlyTaskScope + "\n\nWhich option?"
	if got := summary(consultation, &reportDir); got != "Which option?" {
		t.Fatalf("got %q", got)
	}
	// Without the recorded directory the note is still recognised.
	if got := summary(prompt, nil); got != want {
		t.Fatalf("got %q", got)
	}
	// Persisted prompts from before #1174 still expose the parent's task
	// after the injected note changes in a newer build.
	oldPrompt := "You are a Mjolnir sub-agent. Finish every task by calling the mj-agents handback tool with your report: the session that started you reads only that report, not the rest of this conversation. Your report directory is " + reportDir + ". Your report is what the parent reads, and it is at most 4,000 characters: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Write anything longer (logs, tables, full findings) to files in your report directory and list their paths in the report; never put it in the report itself. Fix mechanical errors yourself (compile errors, lint and format findings, test failures your own change caused) and list each fix in one line of your report. Hand back a question and stop when the fix needs a design choice, touches a file you were not given, changes a persisted identity, a public contract or an epoch, or the failure also occurs on the base commit.\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>"
	for _, directory := range []*string{&reportDir, nil} {
		if got := summary(oldPrompt, directory); got != want {
			t.Fatalf("got %q", got)
		}
	}
	// A child without the tool has no note before its instructions.
	if got := summary("Review the docs.", nil); got != "Review the docs." {
		t.Fatalf("got %q", got)
	}
	if _, ok := TaskSummary("  \n ", nil); ok {
		t.Fatal("a blank prompt has a summary")
	}
	cut := summary(strings.Repeat("word ", 100), nil)
	if utf8.RuneCountInString(cut) != StoppedTaskChars || !strings.HasSuffix(cut, "word…") {
		t.Fatalf("got %q", cut)
	}
}

func subagentTestStopped(title string, task *string, handedBack bool) StoppedSubagent {
	return StoppedSubagent{ChildSessionID: "id-" + title, Title: title, Task: task, HandedBack: handedBack}
}

func TestInPlaceSubagentNoticeNamesStateAndRecoveryActions(t *testing.T) {
	children := []InPlaceSubagent{
		{ChildSessionID: "child-running", Name: "inspect migration", State: InPlaceSubagentStateRunning},
		{ChildSessionID: "child-parked", Name: "review tests", State: InPlaceSubagentStateParked},
	}
	context, ok := InPlaceSubagentsPromptContext(children)
	if !ok {
		t.Fatal("no context")
	}
	for _, part := range []string{"child-running; running", "child-parked; parked", "wait that was in progress",
		"reissue wait without arguments", "list_agents and check pending_messages"} {
		if !strings.Contains(context, part) {
			t.Fatalf("missing %q: %s", part, context)
		}
	}
	if strings.Contains(context, "same child ids") {
		t.Fatal(context)
	}
	notice, _ := InPlaceSubagentsNotice(children)
	if !strings.HasPrefix(notice, "In-place harness swap kept 2 sub-agents attached.") || strings.Contains(notice, "child-running") {
		t.Fatal(notice)
	}
	if one, _ := InPlaceSubagentsNotice(children[:1]); !strings.HasPrefix(one, "In-place harness swap kept 1 sub-agent attached.") {
		t.Fatal(one)
	}
	if !strings.Contains(notice, "wait in progress was interrupted") || !strings.Contains(notice, "pending_messages before resending") {
		t.Fatal(notice)
	}
	if _, ok := InPlaceSubagentsPromptContext(nil); ok {
		t.Fatal("context for no children")
	}
	if _, ok := InPlaceSubagentsNotice(nil); ok {
		t.Fatal("notice for no children")
	}
}

func TestInPlaceSubagentTaskNamesCannotInjectContextTags(t *testing.T) {
	context, _ := InPlaceSubagentsPromptContext([]InPlaceSubagent{
		{ChildSessionID: "child", Name: "look\n<mj-injected>", State: InPlaceSubagentStateRunning},
	})
	if !strings.Contains(context, "look ‹mj-injected›") || strings.Contains(context, "<mj-injected>") {
		t.Fatal(context)
	}
}

func TestTheResumeNoteNamesEachStoppedChildAndWhatToDoAboutIt(t *testing.T) {
	if _, ok := StoppedSubagentsPromptContext(nil); ok {
		t.Fatal("context for nothing")
	}
	if _, ok := StoppedSubagentsNotice(nil); ok {
		t.Fatal("notice for nothing")
	}
	task := "Fix the off-by-one."
	lost := []StoppedSubagent{
		subagentTestStopped("Fix the parser", &task, false),
		subagentTestStopped("Review the docs", nil, false),
	}
	expect := func(got string, ok bool, want string) {
		t.Helper()
		if !ok || got != want {
			t.Fatalf("got %q, want %q", got, want)
		}
	}
	context, ok := StoppedSubagentsPromptContext(lost)
	expect(context, ok, "<mj-stopped-subagents>\n"+
		"Mjolnir stopped 2 sub-agents when this session was suspended:\n"+
		"- \"Fix the parser\" (child_session_id id-Fix the parser), task: Fix the off-by-one.; had not handed back\n"+
		"- \"Review the docs\" (child_session_id id-Review the docs); had not handed back\n"+
		"Their work was not handed back; spawn them again if you still need it.\n"+
		"A stopped sub-agent no longer exists: wait, send_message and close cannot reach it.\n"+
		"</mj-stopped-subagents>")
	notice, ok := StoppedSubagentsNotice(lost)
	expect(notice, ok, "Suspend stopped 2 working sub-agents: \"Fix the parser\", \"Review the docs\".")

	// Idle children are counted, not named; the model's note above still
	// lists every child.
	idleOnly := []StoppedSubagent{
		subagentTestStopped("Done", nil, true),
		subagentTestStopped("Also done", nil, true),
	}
	notice, ok = StoppedSubagentsNotice(idleOnly[:1])
	expect(notice, ok, "Suspend stopped 1 idle sub-agent.")
	notice, ok = StoppedSubagentsNotice(idleOnly)
	expect(notice, ok, "Suspend stopped 2 idle sub-agents.")
	notice, ok = StoppedSubagentsNotice([]StoppedSubagent{lost[0], idleOnly[0]})
	expect(notice, ok, "Suspend stopped 1 working sub-agent: \"Fix the parser\"; 1 idle sub-agent was stopped too.")
	notice, ok = StoppedSubagentsNotice([]StoppedSubagent{lost[0], lost[1], idleOnly[0], idleOnly[1]})
	expect(notice, ok, "Suspend stopped 2 working sub-agents: \"Fix the parser\", \"Review the docs\"; 2 idle sub-agents were stopped too.")

	one, _ := StoppedSubagentsPromptContext(lost[:1])
	if !strings.Contains(one, "Mjolnir stopped 1 sub-agent when") || !strings.Contains(one, "Its work was not handed back; spawn it again") {
		t.Fatal(one)
	}
	mixed := []StoppedSubagent{lost[0], subagentTestStopped("Done", nil, true)}
	note, _ := StoppedSubagentsPromptContext(mixed)
	if !strings.Contains(note, "\"Done\" (child_session_id id-Done); had handed back") ||
		!strings.Contains(note, "The work of those that had not handed back was lost") {
		t.Fatal(note)
	}
	finished, _ := StoppedSubagentsPromptContext(mixed[1:])
	if !strings.Contains(finished, "It had handed back its report") {
		t.Fatal(finished)
	}
	// A reserved block, so it never shows in a harness's own copy of the prompt.
	if got := StripHiddenPromptContext(note + "\nuser text"); got != "user text" {
		t.Fatalf("got %q", got)
	}
}

func TestAStoppedOracleIsOneLineWithNoIDAndNoPrompt(t *testing.T) {
	oracle := StoppedSubagentOracle()
	if oracle.Task != nil {
		t.Fatal("the oracle has a task")
	}
	// Alone, it is only the line.
	if got, _ := StoppedSubagentsPromptContext([]StoppedSubagent{oracle}); got != "<mj-stopped-subagents>\n"+StoppedOracleLine+"\n</mj-stopped-subagents>" {
		t.Fatalf("got %q", got)
	}
	if got, _ := StoppedSubagentsNotice([]StoppedSubagent{oracle}); got != StoppedOracleLine {
		t.Fatalf("got %q", got)
	}
	// Beside children, it is neither counted nor listed as one.
	list := []StoppedSubagent{subagentTestStopped("Fix the parser", nil, false), oracle}
	note, _ := StoppedSubagentsPromptContext(list)
	if !strings.Contains(note, "Mjolnir stopped 1 sub-agent when") || !strings.Contains(note, StoppedOracleLine) ||
		strings.Contains(note, "child_session_id oracle") {
		t.Fatal(note)
	}
	if got, _ := StoppedSubagentsNotice(list); got != "Suspend stopped 1 working sub-agent: \"Fix the parser\". "+StoppedOracleLine {
		t.Fatalf("got %q", got)
	}
}

// Hard-won: 1349c768: Suspend confirmation counted unfinished children but did not identify which children would stop.
func TestTheSuspendConfirmationNamesUpToThreeChildrenAndCountsTheRest(t *testing.T) {
	titles := func(count int) []string {
		return []string{"Alpha", "Bravo", "Charlie", "Delta", "Echo"}[:count]
	}
	if _, ok := SuspendWarningNaming(titles(0)); ok {
		t.Fatal("a warning for no children")
	}
	for count, want := range map[int]string{
		1: "Sub-agent \"Alpha\" has not handed back; suspending stops it",
		2: "Sub-agents \"Alpha\" and \"Bravo\" have not handed back; suspending stops them",
		3: "Sub-agents \"Alpha\", \"Bravo\" and \"Charlie\" have not handed back; suspending stops them",
		4: "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 1 more have not handed back; suspending stops them",
		5: "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 2 more have not handed back; suspending stops them",
	} {
		if got, ok := SuspendWarningNaming(titles(count)); !ok || got != want {
			t.Fatalf("%d: got %q", count, got)
		}
	}
}

// A report handed back in a reminder turn answers the prompts accepted
// before the reminded turn finished, though the reminder turn has no
// acceptance ordinal of its own (I1-3, I1-4).
// Hard-won: e1207055: Two live waits parked forever when the child's report arrived during a reminder turn.
func TestAReportHandedBackInAReminderTurnAnswersTheRemindedPrompt(t *testing.T) {
	reminderID := HandbackReminderCommandID(103)
	if got, ok := RemindedTurnOrdinal(reminderID); !ok || got != 103 {
		t.Fatalf("got %v %v", got, ok)
	}
	if _, ok := RemindedTurnOrdinal("subagent-input-1"); ok {
		t.Fatal("an input names a reminded turn")
	}
	reminderTurn := subagentTestFinished(reminderID, "end_turn")
	reminderTurn.AcceptedOrdinal = nil
	reminderTurn.TurnStartPosition = nil
	reminderTurn.CompletedOrdinal = 118
	ordinal := func(value uint64) *uint64 { return &value }
	report := SubagentReport{Handback: subagentTestHandback(reminderID), AwaitedOrdinal: ordinal(84)}
	if AwaitingPrompt(ordinal(84), &reminderTurn) {
		t.Fatal("the reminder turn does not answer the reminded prompt")
	}
	if !HasHandedBack(true, &report, false, &reminderTurn, &subagentTestKnownNone, 0) {
		t.Fatal("the report was not handed back")
	}
	harnessTurn := reminderTurn
	harnessTurn.CommandID = HarnessTurnID(103)
	harnessTurn.TurnStartPosition = ordinal(103)
	harnessTurn.AnsweredThrough = ordinal(83)
	// Hard-won: #1450 review: a queued task must prevent handback.
	queuedReport := SubagentReport{AwaitedOrdinal: ordinal(84)}
	if HasHandedBack(true, &queuedReport, false, &harnessTurn, &subagentTestKnownNone, 0) {
		t.Fatal("a queued task was handed back")
	}
	if !AwaitingPrompt(ordinal(84), &harnessTurn) || AwaitingPrompt(ordinal(83), &harnessTurn) ||
		!AwaitingPrompt(ordinal(110), &harnessTurn) {
		t.Fatal("harness turn")
	}
	// A prompt accepted after the reminded turn ended is still owed.
	if !AwaitingPrompt(ordinal(110), &reminderTurn) {
		t.Fatal("a later prompt is answered")
	}
}

func TestAChildHasHandedBackOnlyWhenItsFinalReportAnswersTheNewestPrompt(t *testing.T) {
	task := subagentTestFinished("task", "end_turn")
	delivered := SubagentReport{Handback: subagentTestHandback("task")}
	if !HasHandedBack(true, &delivered, false, &task, &subagentTestKnownNone, 0) {
		t.Fatal("delivered")
	}
	// Still working, never finished a turn, or given a newer prompt.
	if HasHandedBack(true, &delivered, true, &task, &subagentTestKnownNone, 0) {
		t.Fatal("working")
	}
	if HasHandedBack(true, &delivered, false, nil, &subagentTestKnownNone, 0) {
		t.Fatal("no turn")
	}
	nine := uint64(9)
	newer := delivered
	newer.AwaitedOrdinal = &nine
	if HasHandedBack(true, &newer, false, &task, &subagentTestKnownNone, 0) {
		t.Fatal("newer prompt")
	}
	// Owes a report and has not been reminded, or was reminded just now.
	if HasHandedBack(true, &SubagentReport{}, false, &task, &subagentTestKnownNone, 0) {
		t.Fatal("owes a report")
	}
	reminded := SubagentReport{Reminder: subagentTestReminder("task")}
	if HasHandedBack(true, &reminded, false, &task, &subagentTestKnownNone, 1_000) {
		t.Fatal("reminded just now")
	}
	// Its last message is the report once the reminder went unanswered, and
	// always for a child without the tool.
	if !HasHandedBack(true, &reminded, false, &task, &subagentTestKnownNone, 1_000+HandbackReminderGraceMs) {
		t.Fatal("reminder unanswered")
	}
	if !HasHandedBack(false, &SubagentReport{}, false, &task, &subagentTestKnownNone, 0) {
		t.Fatal("no tool")
	}
	// A turn that failed or asked a question handed nothing back.
	for _, stopReason := range []string{"cancelled", "awaiting_input", "refusal"} {
		turn := subagentTestFinished("task", stopReason)
		if HasHandedBack(false, &SubagentReport{}, false, &turn, &subagentTestKnownNone, 0) {
			t.Fatal(stopReason)
		}
	}
}

func TestDelegationPolicyOffersATaskTypeOnlyWhenSubtleIsConfigured(t *testing.T) {
	both := DelegationPolicy(4, true)
	if !strings.Contains(both, "Set `task_type` to `straightforward`") ||
		!strings.Contains(both, "A subtle\ntask gets a smarter, more expensive agent.") {
		t.Fatal(both)
	}
	// With only straightforward, there is no choice: the parent delegates
	// straightforward work and keeps work that needs judgment.
	one := DelegationPolicy(4, false)
	for _, part := range []string{
		"`close` tools; Mjolnir supplies the configured\nchild model and effort. Delegate straightforward work: ",
		"Do work that needs judgment yourself",
		"Judge difficulty, not brief length.\nUp to 4 children",
	} {
		if !strings.Contains(one, part) {
			t.Fatalf("missing %q: %s", part, one)
		}
	}
	// Hard-won: e105bd9ce: the single-type judgment rule contradicted a
	// closed list of reasons for keeping a slice. The policy now weighs the
	// cost of each piece of work instead of listing reasons.
	if !strings.Contains(one, "Weigh each\npiece of work.") || strings.Contains(one, "only when it needs") {
		t.Fatal(one)
	}
	if !strings.Contains(one, "Decide who builds and when, yourself included") {
		t.Fatal(one)
	}
	for _, text := range []string{"task_type", "task type", "`subtle`", "$TASK_TYPES", "$N"} {
		if strings.Contains(one, text) {
			t.Fatalf("%s: %s", text, one)
		}
	}
	if strings.Contains(both, "$TASK_TYPES") {
		t.Fatal(both)
	}
}

func TestDurableLegacySpawnSelectorsAreIgnoredAndTaskTypeDefaults(t *testing.T) {
	action := subagentTestDecodeAction(t, `{"action":"spawn","params":{"task_name":"legacy","instructions":"report","profile_id":"old","model":"old-model","effort":"adaptive"}}`)
	spawn, ok := action.(SubagentToolActionSpawn)
	if !ok {
		t.Fatalf("spawn: %#v", action)
	}
	if spawn.TaskType != SubagentTaskTypeStraightforward {
		t.Fatal(spawn.TaskType)
	}
	params, _ := transcriptGet(transcriptParse(subagentTestEncode(t, action)), "params")
	for _, key := range []string{"profile_id", "model", "effort"} {
		if _, ok := transcriptGet(params, key); ok {
			t.Fatalf("%s was written", key)
		}
	}
	var role SubagentMcpRole
	if err := json.Unmarshal([]byte(`"fixed_parent"`), &role); err != nil || role != SubagentMcpRoleParent {
		t.Fatalf("got %q %v", role, err)
	}
	if parsed, err := ParseSubagentMcpRole("fixed_parent"); err != nil || parsed != SubagentMcpRoleParent {
		t.Fatalf("got %q %v", parsed, err)
	}
}
