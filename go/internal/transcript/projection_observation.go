// Port of mj-transcript/src/projection/observation.rs.

package transcript

import (
	"encoding/json"
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// externalEventsItemPrefix: sessions projected before delivery rows existed
// keep one accumulated row under `system:mailbox:external-events`. Delivery
// rows use this prefix and an event key, so the two can never share an
// identity.
const externalEventsItemPrefix = "system:mailbox:delivered:"

// QueuedCommandContent is the queue presentation shared by ordinary replay
// and current-state recovery: ACP content blocks in their JSON form.
func QueuedCommandContent(command core.RelayCommand) ([]json.RawMessage, error) {
	panic("unported: mj-transcript/src/projection/observation.rs queued_command_content")
}

// queuedContentValues is content in the form core.MaterializedQueuedPrompt
// holds it (Rust's Vec<Value>): each block's JSON text is one element.
func queuedContentValues(content []json.RawMessage) []any {
	values := make([]any, len(content))
	for i, block := range content {
		values[i] = block
	}
	return values
}

// queuedContentRaw is a queue entry's content as the JSON blocks transcript
// bodies hold.
func queuedContentRaw(content []any) ([]json.RawMessage, error) {
	raw := make([]json.RawMessage, len(content))
	for i, value := range content {
		encoded, err := json.Marshal(value)
		if err != nil {
			return nil, err
		}
		raw[i] = encoded
	}
	return raw, nil
}

func projectSteeredPrompt(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	queue *[]core.MaterializedQueuedPrompt,
	queuedPromptID string,
	mutation *core.MaterializedSessionMutation,
) error {
	panic("unported: mj-transcript/src/projection/observation.rs project_steered_prompt")
}

func projectObservation(current *core.MaterializedSession, index *ProjectionIndex, event *core.RelayEvent, mutation *core.MaterializedSessionMutation) error {
	panic("unported: mj-transcript/src/projection/observation.rs project_observation")
}

// answeredThrough: the projection alone decides which accepted prompts a
// completion answers: every prompt accepted before it, unless one is still
// queued or running, and never fewer than the previous outcome answered.
//
// One number records this, so a prompt settled out of order (a queued
// prompt rejected or removed while an earlier one still runs or waits) is
// answered by its own outcome, and by later outcomes only once the prompts
// ahead of it have ended. A wait for it can therefore end late, but never
// early.
//
// While another turn runs, its own prompt is still owed and every earlier
// prompt settled before it started, so the outcome adds nothing. The
// running turn's ordinal is never read: steering replaces it with the
// steered prompt's (#1450 review). The answer is always recorded, 0 meaning
// none, so a reader never falls back to the outcome's own acceptance
// ordinal; the result is therefore never nil.
func answeredThrough(current *core.MaterializedSession, event *core.RelayEvent, ending string) *uint64 {
	panic("unported: mj-transcript/src/projection/observation.rs answered_through")
}

// externalEventsItemID is the row that lists one delivery's external
// events, such as GitHub activity. It is keyed on the delivery's first
// external event, as a message row is keyed on its event, so a replayed
// delivery finds the row it already made. The relay claims each event key
// for exactly one wake, prompt, or hook delivery (claim_wake_mailbox_events
// and the delivered-key check in mj-core's relay apply), so no two
// deliveries share a row identity. False when the delivery carried only
// messages, or carried no bodies because an older worker recorded it.
func externalEventsItemID(events []core.MailboxEvent) (string, bool) {
	for i := range events {
		if _, message := events[i].MessageText(); !message {
			return externalEventsItemPrefix + events[i].Key, true
		}
	}
	return "", false
}

// deliveryRowExists: a replayed delivery leaves the rows it made where they
// are, whether they are in the projection already or earlier in this
// mutation.
func deliveryRowExists(index *ProjectionIndex, mutation *core.MaterializedSessionMutation, stableID string) bool {
	if index.get(stableID) != nil {
		return true
	}
	for _, change := range mutation.Transcript {
		if upsert, ok := change.(core.TranscriptMutationUpsert); ok && upsert.Item.StableID == stableID {
			return true
		}
	}
	return false
}

// deliveredExternalEvents shows external events where the agent received
// them: just before the turn a wake started, before the prompt they were
// attached to, or between the tool calls a hook delivered them after.
// Messages get rows of their own in deliveredMessages; this row lists
// everything else in the delivery.
func deliveredExternalEvents(index *ProjectionIndex, delivery *core.RelayEvent, events []core.MailboxEvent, mutation *core.MaterializedSessionMutation) {
	panic("unported: mj-transcript/src/projection/observation.rs delivered_external_events")
}

func deliveredMessages(index *ProjectionIndex, delivery *core.RelayEvent, events []core.MailboxEvent, mutation *core.MaterializedSessionMutation) {
	panic("unported: mj-transcript/src/projection/observation.rs delivered_messages")
}

func userShellItemID(commandID string) string {
	return "shell:" + commandID
}

// PlanProposalItemID is the stable id of the captured plan proposal created
// by the relay event at ordinal. The ordinal keys it because the
// harness-side review id restarts with every harness process, while the
// ordinal is durable and replay-stable.
func PlanProposalItemID(ordinal uint64) string {
	return fmt.Sprintf("plan-proposal:%d", ordinal)
}

func userShellText(command, status, stdout, stderr string, stdoutTruncated, stderrTruncated bool) string {
	var text strings.Builder
	fmt.Fprintf(&text, "Shell · %s\n$ %s", status, command)
	for _, stream := range []struct {
		name, output string
		truncated    bool
	}{{"stdout", stdout, stdoutTruncated}, {"stderr", stderr, stderrTruncated}} {
		if stream.output == "" {
			continue
		}
		text.WriteString("\n\n" + stream.name + ":\n" + stream.output)
		if stream.truncated {
			text.WriteString("\n[output continues; final tail will be shown on completion]")
		}
	}
	return text.String()
}

func userShellResultText(result *core.UserShellResult) string {
	panic("unported: mj-transcript/src/projection/observation.rs user_shell_result_text")
}

// controllerCoordination reports commands the daemon issues to the worker
// for its own bookkeeping, never on the person's behalf.
func controllerCoordination(command core.RelayCommandKind) bool {
	switch command {
	case core.RelayCommandKindBeginCheckpoint, core.RelayCommandKindCompleteCheckpoint,
		core.RelayCommandKindReleaseCheckpoint, core.RelayCommandKindAdvanceRecoveryFloor:
		return true
	}
	return false
}
