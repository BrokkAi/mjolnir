// Port of mj-core/src/relay/snapshot/apply.rs.
//
// Folding one relay event into the next snapshot.

package core

import (
	"errors"

	acpschema "github.com/BrokkAi/acp-go/schema"
)

// ObservationChangesState reports whether applying this observation moves
// durable relay state beyond the event frontier.
//
// Assistant text also updates bounded authorization evidence. Other
// transcript observations only advance the frontier. Every arm here mirrors
// an arm of ApplyRelayEvent;
// transcript_observations_move_nothing_but_the_frontier fails if the two
// ever disagree.
func ObservationChangesState(observation RelayObservation) bool {
	panic("unported: mj-core/src/relay/snapshot/apply.rs observation_changes_state")
}

func clearNotifications(snapshot *RelaySnapshot, ordinal uint64) {
	snapshot.PersonInputThrough = ordinal
	snapshot.Request = nil
	if len(snapshot.NotificationStack.Entries) > 0 {
		snapshot.NotificationStack.Entries = snapshot.NotificationStack.Entries[:0]
		snapshot.NotificationStack.Revision = ordinal
	}
}

func recordNotification(snapshot *RelaySnapshot, completedOrdinal uint64, completedAtMs int64, kind NotificationKind, message EntryMessage, ordinal uint64) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs record_notification")
}

// settleQuestionTarget records the located question message, if any (target
// nil when none was found).
func settleQuestionTarget(snapshot *RelaySnapshot, completedOrdinal uint64, target *string, ordinal uint64) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs settle_question_target")
}

func claimWakeMailboxEvents(snapshot *RelaySnapshot, events []MailboxEvent, ordinal uint64) error {
	panic("unported: mj-core/src/relay/snapshot/apply.rs claim_wake_mailbox_events")
}

func retainDeliveredMailboxEventKeys(snapshot *RelaySnapshot, eventKeys []string, ordinal uint64) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs retain_delivered_mailbox_event_keys")
}

func hookSteeringPrompt(snapshot *RelaySnapshot, steering *MailboxHookSteering) ([]acpschema.ContentBlock, error) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs hook_steering_prompt")
}

func completeHookSteering(snapshot *RelaySnapshot, steering *MailboxHookSteering, ordinal uint64) error {
	panic("unported: mj-core/src/relay/snapshot/apply.rs complete_hook_steering")
}

func ApplyRelayEvent(snapshot *RelaySnapshot, event *RelayEvent) error {
	panic("unported: mj-core/src/relay/snapshot/apply.rs apply_relay_event")
}

// CompactTerminalCommands drops execution payloads only after every side
// effect of a terminal event. Also upgrades legacy receipts when a stored
// snapshot is opened.
func (s *RelaySnapshot) CompactTerminalCommands() (bool, error) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs RelaySnapshot::compact_terminal_commands")
}

// ReleasesHistory: legacy archive completions also retry collection of an
// acknowledged prefix. Their coverage marker no longer determines current
// journal retention.
func ReleasesHistory(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandCompleteCheckpoint, RelayCommandAdvanceRecoveryFloor:
		return true
	}
	return false
}

func terminalizeRemovedPrompts(snapshot *RelaySnapshot, removedCommandIDs []string, terminalOrdinal uint64) error {
	panic("unported: mj-core/src/relay/snapshot/apply.rs terminalize_removed_prompts")
}

func ValidateRelaySnapshotFrontiers(snapshot *RelaySnapshot) error {
	if snapshot.AcknowledgedThrough > snapshot.LatestOrdinal {
		return errors.New("relay acknowledgement is ahead of the event frontier")
	}
	if snapshot.RecoveryFloorOrdinal > snapshot.LatestOrdinal {
		return errors.New("relay recovery floor is ahead of the event frontier")
	}
	for _, check := range []struct{ digest, name string }{
		{snapshot.LatestDigest, "relay latest digest"},
		{snapshot.AcknowledgedDigest, "relay acknowledgement digest"},
		{snapshot.RecoveryFloorDigest, "relay recovery floor digest"},
	} {
		if err := ValidateRelayDigest(check.digest, check.name); err != nil {
			return err
		}
	}
	if (snapshot.LatestOrdinal == 0) != (snapshot.LatestDigest == RelayEventGenesisDigest) {
		return errors.New("relay latest frontier and genesis digest disagree")
	}
	if (snapshot.AcknowledgedThrough == 0) != (snapshot.AcknowledgedDigest == RelayEventGenesisDigest) {
		return errors.New("relay acknowledgement frontier and genesis digest disagree")
	}
	if (snapshot.RecoveryFloorOrdinal == 0) != (snapshot.RecoveryFloorDigest == RelayEventGenesisDigest) {
		return errors.New("relay recovery floor and genesis digest disagree")
	}
	return nil
}

// cancelsCapacityRetry: explicit user work and lifecycle admission supersede
// automated recovery.
func cancelsCapacityRetry(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandPrompt, RelayCommandCancel, RelayCommandCancelTurn, RelayCommandSetConfig,
		RelayCommandRestoreExecutionMode, RelayCommandGoalControl, RelayCommandSetSessionMode, RelayCommandClose:
		return true
	}
	return false
}

func recordHandbackMailboxAssignments(snapshot *RelaySnapshot, events []MailboxEvent) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs record_handback_mailbox_assignments")
}

// applyAssessmentContext: classification boundaries and action invalidation
// share the journal owner.
func applyAssessmentContext(snapshot *RelaySnapshot, event *RelayEvent) {
	panic("unported: mj-core/src/relay/snapshot/apply.rs apply_assessment_context")
}

// beginDeliveredMessageTurn: the accepted delivery owns a new turn even after
// restore or cancellation. It may recover from quota without granting new
// authorization or renewing the ordinary nudge allowance.
func beginDeliveredMessageTurn(continuation *ContinuationState, commandID string) {
	if continuation.UserCommandID == nil {
		id := commandID
		continuation.UserCommandID = &id
		continuation.Suppressed = true
	}
	continuation.CompletedCommandID = nil
	continuation.HarnessTurn = nil
	continuation.QuotaSuppressed = false
	continuation.QuotaRecovery = nil
}
