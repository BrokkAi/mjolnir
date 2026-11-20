// Port of the tests in mj-core/src/event_outcome.rs.

package core

import "testing"

func TestProviderFailureAndUnknownStopRetainDifferentEvidence(t *testing.T) {
	accepted, start := uint64(1), uint64(2)
	turn := MaterializedTurnOutcome{
		CommandID:         "opaque",
		AcceptedOrdinal:   &accepted,
		TurnStartPosition: &start,
		CompletedOrdinal:  3,
		CompletedAtMs:     4,
		Outcome:           TurnOutcomeKindCompleted{StopReason: "provider_error"},
	}
	if reason := turn.Result().Reason; reason == nil || *reason != OutcomeReasonUnrecognizedStopReason {
		t.Fatalf("reason = %v", reason)
	}
	code := "internal_error"
	status := uint16(500)
	turn.Diagnostic = &TurnDiagnostic{
		Message:    "provider returned 500",
		Code:       &code,
		HTTPStatus: &status,
	}
	result := turn.Result()
	if result.Reason == nil || *result.Reason != OutcomeReasonProviderFailure {
		t.Fatalf("reason = %v", result.Reason)
	}
	if result.Kind != TurnResultKindFailed {
		t.Fatalf("kind = %v", result.Kind)
	}
	if result.StopReason == nil || *result.StopReason != "provider_error" {
		t.Fatalf("stop reason = %v", result.StopReason)
	}
}

func TestShellExitFailureDoesNotMakeASuccessfulCancelCommandFail(t *testing.T) {
	exitCode := int32(7)
	shell := UserShellResult{
		Command:    "exit 7",
		ExitCode:   &exitCode,
		DurationMs: 1,
		Status:     UserShellStatusExited,
	}
	failure := CommandResultCompleted("shell", RelayCommandKindRunUserShell, RelayCommandOutcomeUserShell{Result: shell})
	if failure.Outcome != CommandResultKindFailed {
		t.Fatalf("outcome = %v", failure.Outcome)
	}
	if failure.Reason == nil || *failure.Reason != OutcomeReasonNonzeroExit {
		t.Fatalf("reason = %v", failure.Reason)
	}
	cancellation := CommandResultCompleted("stop", RelayCommandKindCancelTurn, RelayCommandOutcomeCancelled{})
	if cancellation.Outcome != CommandResultKindSucceeded {
		t.Fatalf("outcome = %v", cancellation.Outcome)
	}
	if cancellation.Reason != nil {
		t.Fatalf("reason = %v", *cancellation.Reason)
	}
}

func TestHistoricalRuntimeFailuresDeserializeWithAnExplicitUnknownReason(t *testing.T) {
	event, err := UnmarshalRuntimeEvent([]byte(`{"type":"command_interrupted","request_id":"opaque","message":"older adapter"}`))
	if err != nil {
		t.Fatal(err)
	}
	interrupted, ok := event.(RuntimeEventCommandInterrupted)
	if !ok || interrupted.Reason != OutcomeReasonLegacyUnclassified {
		t.Fatalf("event = %#v", event)
	}
}
