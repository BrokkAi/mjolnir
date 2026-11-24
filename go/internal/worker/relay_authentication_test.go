// Port of mj-worker/src/relay/authentication_tests.rs.

package worker

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	acp "github.com/BrokkAi/acp-go"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func authenticationOpen(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, session, "test")
	if err != nil {
		t.Fatal(err)
	}
	setVerdictHarness(t, relay, core.HarnessKindClaude)
	if _, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}); err != nil {
		t.Fatal(err)
	}
	return relay
}

func authenticationFailure(t *testing.T, relay *DurableRelay, id, login string) {
	t.Helper()
	diagnostic := core.TurnDiagnosticFromACP(&acp.RPCError{Code: acp.ErrorCodeAuthRequired, Message: "Authentication required"})
	diagnostic.LoginFingerprint = &login
	if _, err := relay.RecordCommandCompleted(id, core.RelayCommandOutcomePrompt{
		StopReason: "error",
		Diagnostic: &diagnostic,
	}); err != nil {
		t.Fatal(err)
	}
}

func authenticationClaim(t *testing.T, relay *DurableRelay) []core.ClaimedRelayCommand {
	t.Helper()
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatal(err)
	}
	return claimed
}

// authenticationRetry is submit_authentication_retry(..).unwrap().
func authenticationRetry(t *testing.T, relay *DurableRelay, ordinal uint64, login string) bool {
	t.Helper()
	submitted, err := relay.submitAuthenticationRetry(ordinal, login)
	if err != nil {
		t.Fatal(err)
	}
	return submitted
}

func authenticationPublishedIntent(relay *DurableRelay) string {
	intent := relay.OperationalState().AuthenticationRetryIntent
	if intent == nil {
		return "<none>"
	}
	return *intent
}

// Hard-won: #1515: a refreshed login left an auth-failed turn idle forever.
// Hard-won: #1547: a spawned sub-agent's model configure set quota_suppressed, so #1515 never retried its auth-failed turn.
// Hard-won: b62fe3b: a control-command flood evicted the still-owned auth receipt.
func TestAuthenticationRecoverySurvivesRestartAndNeverRetriesTheSameLogin(t *testing.T) {
	for _, original := range []string{"user-request", "subagent-spawn-auth-regression"} {
		root := relayTempDir(t)
		relay := authenticationOpen(t, root)
		if strings.HasPrefix(original, "subagent-spawn") {
			// A spawn configures the model before its generated prompt.
			submitRelay(relay, "subagent-spawn-auth-regression-model", core.RelayCommandSetConfig{Key: "model", Value: "sonnet"})
			authenticationClaim(t, relay)
			if _, err := relay.RecordCommandCompleted("subagent-spawn-auth-regression-model", core.RelayCommandOutcomeConfigured{}); err != nil {
				t.Fatal(err)
			}
		}
		submitRelay(relay, original, prompt("finish this task"))
		authenticationClaim(t, relay)
		authorization := relay.snapshot.Continuation.UserCommandID
		authenticationFailure(t, relay, original, "old-access-hash")
		for index := 0; index <= core.RelayRetryIDRetention; index++ {
			submitRelay(relay, fmt.Sprintf("auth-notice-%d", index), core.RelayCommandRecordNotice{Text: "later notice"})
		}
		through := relay.LatestOrdinal()
		acknowledgeRelay(relay, "ack-auth-notices", through)
		if _, ok := relay.snapshot.HandledCommands["auth-notice-0"]; ok {
			t.Fatal("the oldest notice receipt is collected")
		}
		ordinal, _, ok := relay.authenticationRetryIntent()
		if !ok {
			t.Fatal("auth failure is recoverable")
		}
		// The worker publishes the retry it owes; the retired field stays empty.
		if got := authenticationPublishedIntent(relay); got != "old-access-hash" {
			t.Fatalf("published intent = %s", got)
		}
		if waiting := relay.OperationalState().AuthenticationRetryWaiting; waiting != nil {
			t.Fatalf("retired field = %s", *waiting)
		}
		if authenticationRetry(t, relay, ordinal, "old-access-hash") {
			t.Fatal("the same login is never retried")
		}

		relay = authenticationOpen(t, root)
		if !authenticationRetry(t, relay, ordinal, "new-access-hash") {
			t.Fatal("a new login retries the failed turn")
		}
		if authenticationRetry(t, relay, ordinal, "new-access-hash") {
			t.Fatal("the retry is submitted once")
		}
		if !reflect.DeepEqual(relay.snapshot.Continuation.UserCommandID, authorization) {
			t.Fatalf("user command = %v, want %v", relay.snapshot.Continuation.UserCommandID, authorization)
		}
		claimed := authenticationClaim(t, relay)
		if len(claimed) == 0 {
			t.Fatal("the retry is claimable")
		}
		retry := claimed[len(claimed)-1]
		if want := fmt.Sprintf("auth-retry-%d", ordinal); retry.CommandID != want {
			t.Fatalf("retry command = %s, want %s", retry.CommandID, want)
		}
		if blocks, _ := core.RelayCommandPromptBlocks(retry.Command); !reflect.DeepEqual(blocks, core.PromptBlocks()) {
			t.Fatalf("retry prompt = %#v", blocks)
		}
		authenticationFailure(t, relay, retry.CommandID, "new-access-hash")
		second, _, ok := relay.authenticationRetryIntent()
		if !ok {
			t.Fatal("the failed retry is recoverable")
		}
		if authenticationRetry(t, relay, second, "new-access-hash") {
			t.Fatal("the same login is never retried")
		}
		if authenticationRetry(t, relay, ordinal, "third-access-hash") {
			t.Fatal("a stale ordinal is never retried")
		}
		if !authenticationRetry(t, relay, second, "third-access-hash") {
			t.Fatal("a third login retries the second failure")
		}
	}
}

// Hard-won: #1581: a checkpoint barrier dropped the published retry, so the
// daemon reported a child whose retry was about to be submitted. The barrier
// only gates the submit; a question or a paused goal withdraws the intent.
func TestAuthenticationRecoveryYieldsToNewInputCancellationAndCheckpoints(t *testing.T) {
	for _, intervention := range []string{
		"new input",
		"removed input",
		"cancel",
		"checkpoint",
		"harness turn",
		"question",
		"paused goal",
	} {
		relay := authenticationOpen(t, relayTempDir(t))
		submitRelay(relay, "user-request", prompt("finish this task"))
		authenticationClaim(t, relay)
		if intervention == "cancel" {
			submitRelay(relay, "cancel-request", core.RelayCommandCancelTurn{})
		}
		if intervention == "removed input" {
			submitRelay(relay, "new-request", prompt("stop and explain"))
			submitRelay(relay, "remove-new", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "new-request"})
		}
		authenticationFailure(t, relay, "user-request", "old-access-hash")
		terminal := relay.snapshot.HandledCommands["user-request"].TerminalOrdinal
		if terminal == nil {
			t.Fatal("the failed prompt is terminal")
		}
		ordinal := *terminal
		switch intervention {
		case "new input":
			submitRelay(relay, "new-request", prompt("stop and explain"))
		case "checkpoint":
			submitRelay(relay, "checkpoint-request", core.RelayCommandBeginCheckpoint{})
		case "harness turn":
			if _, err := relay.RecordObservation(core.RelayObservationHarnessTurnStarted{StartedAtMs: core.EpochMillis()}); err != nil {
				t.Fatal(err)
			}
		case "paused goal":
			relay.snapshot.Goal.Snapshot = &core.GoalSnapshot{Objective: "finish", Status: "paused"}
		case "question":
			question, err := core.ElicitationRequestFromACPParams("pending-question", json.RawMessage(`{
				"mode":"form", "sessionId":"`+session+`", "message":"Which target?",
				"requestedSchema":{"type":"object", "properties":{}}
			}`))
			if err != nil {
				t.Fatal(err)
			}
			if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: question}); err != nil {
				t.Fatal(err)
			}
		}
		if authenticationRetry(t, relay, ordinal, "new-access-hash") {
			t.Fatalf("%s: the retry must not be submitted", intervention)
		}
		// The worker stops publishing it the moment the retry is no longer
		// its to make. A checkpoint barrier only delays the submit.
		want := "<none>"
		if intervention == "checkpoint" {
			want = "old-access-hash"
		}
		if got := authenticationPublishedIntent(relay); got != want {
			t.Fatalf("%s: published intent = %s, want %s", intervention, got, want)
		}
		if intervention == "checkpoint" {
			authenticationClaim(t, relay)
			if _, err := relay.RecordCheckpointReady("checkpoint-request"); err != nil {
				t.Fatal(err)
			}
			// The barrier is held and the new login arrives during it: the
			// intent is still published, and nothing is submitted.
			if got := authenticationPublishedIntent(relay); got != "old-access-hash" {
				t.Fatalf("published intent during the barrier = %s", got)
			}
			if authenticationRetry(t, relay, ordinal, "new-access-hash") {
				t.Fatal("the barrier delays the submit")
			}
			submitRelay(relay, "release-checkpoint", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "checkpoint-request"})
			if !authenticationRetry(t, relay, ordinal, "new-access-hash") {
				t.Fatal("the released barrier lets the retry through")
			}
		}
	}
}

func TestARevision24SnapshotWithTheQuotaFlagKeepsTheAuthRetryBlocked(t *testing.T) {
	root := relayTempDir(t)
	relay := authenticationOpen(t, root)
	submitRelay(relay, "user-request", prompt("finish this task"))
	authenticationClaim(t, relay)
	authenticationFailure(t, relay, "user-request", "old-access-hash")
	if _, _, ok := relay.authenticationRetryIntent(); !ok {
		t.Fatal("auth failure is recoverable")
	}
	frontier := relay.snapshot.LatestOrdinal
	// What an older worker wrote after the user cancelled: the flag alone.
	encoded, err := json.Marshal(relay.snapshot)
	if err != nil {
		t.Fatal(err)
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	var legacy map[string]any
	if err := decoder.Decode(&legacy); err != nil {
		t.Fatal(err)
	}
	legacy["format_version"] = 24
	continuation := legacy["continuation"].(map[string]any)
	continuation["quota_suppressed"] = true
	delete(continuation, "withdrawn_ordinal")
	rewritten, err := json.Marshal(legacy)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, core.RelayStateFile), rewritten, 0o600); err != nil {
		t.Fatal(err)
	}

	upgraded := authenticationOpen(t, root)
	if withdrawn := upgraded.snapshot.Continuation.WithdrawnOrdinal; withdrawn == nil || *withdrawn != frontier {
		t.Fatalf("withdrawn ordinal = %v, want %d", withdrawn, frontier)
	}
	if _, _, ok := upgraded.authenticationRetryIntent(); ok {
		t.Fatal("the legacy quota flag keeps the retry blocked")
	}
}

// Hard-won: 70b45ac58: restart recovery appeared newer than an already queued auth-failed prompt.
func TestRestartRecoveryPreservesQueuedAuthenticationRetryIntent(t *testing.T) {
	root := relayTempDir(t)
	relay := authenticationOpen(t, root)
	submitRelay(relay, "prompt-original", prompt("first task"))
	authenticationClaim(t, relay)
	accepted := relay.snapshot.HandledCommands["prompt-original"].AcceptedOrdinal
	submitRelay(relay, "prompt-queued", prompt("next task"))
	latestInput := relay.snapshot.LatestPromptAcceptedOrdinal
	relay = authenticationOpen(t, root)
	claimed := authenticationClaim(t, relay)
	if len(claimed) == 0 {
		t.Fatal("the recovered prompt is claimable")
	}
	recovered := claimed[len(claimed)-1]
	if got := relay.snapshot.HandledCommands[recovered.CommandID].AcceptedOrdinal; got != accepted {
		t.Fatalf("recovered accepted ordinal = %d, want %d", got, accepted)
	}
	if !reflect.DeepEqual(relay.snapshot.LatestPromptAcceptedOrdinal, latestInput) {
		t.Fatalf("latest prompt accepted = %v, want %v", relay.snapshot.LatestPromptAcceptedOrdinal, latestInput)
	}
	// The newer queued request must still withdraw retries of recovered A.
	authenticationFailure(t, relay, recovered.CommandID, "old-access-hash")
	if _, _, ok := relay.authenticationRetryIntent(); ok {
		t.Fatal("the newer queued request withdraws the retry")
	}
	claimed = authenticationClaim(t, relay)
	if len(claimed) == 0 {
		t.Fatal("the queued prompt is claimable")
	}
	queued := claimed[len(claimed)-1]
	if queued.CommandID != "prompt-queued" {
		t.Fatalf("claimed %s", queued.CommandID)
	}
	authenticationFailure(t, relay, queued.CommandID, "old-access-hash")
	ordinal, _, ok := relay.authenticationRetryIntent()
	if !ok {
		t.Fatal("B still owns its authentication retry")
	}
	if !authenticationRetry(t, relay, ordinal, "new-access-hash") {
		t.Fatal("a new login retries B")
	}
}
