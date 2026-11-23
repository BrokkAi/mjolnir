// Port of mj-worker/src/relay/checkpoint_hold.rs.
//
// The worker's own journal writes wait while a checkpoint barrier holds its
// cut.
//
// A ready barrier names the cut a checkpoint archives: the relay frontier at
// its CheckpointReady event. The daemon exports that cut and, for a close,
// seals the relay at exactly that frontier. The barrier already freezes
// command dispatch; this freezes the rest of what the worker writes on its
// own, so the worker is the only owner of the cut and nothing it does can
// move it between the daemon's look and its Close (campaign finding I2-3).
//
// What waits is output that changes no agent work: harness notifications,
// notices and warnings, native-agent reports. The relay decides this at the
// one place every journal write passes, appendRelayEvent: anything it is
// about to journal other than those, and other than the barrier's own
// commands and a Close sealed at the cut, ends the hold, with the held
// writes journaled first in their order. A runtime event that journals
// nothing therefore leaves the hold alone. A turn the agent starts, a goal
// change, a question for a person or a restart spoils the cut anyway: a
// routine checkpoint sees the new harness turn and abandons its archive, and
// a close is refused and returns the session to work, exactly as before the
// hold existed.
//
// Background work that starts after the cut (an agent terminal, an adapter
// task) journals nothing, so the frontier cannot show it. The relay sees it
// where all background work changes, persistActivityTransition, and spoils
// the barrier's cut: the hold ends and a Close at the cut is refused, like a
// Close after a journaled turn. Work already running when the barrier became
// ready does not spoil it; a Move that accepted interrupting it closes over
// it on purpose. The mark is process-local, like the work: terminals and
// adapter tasks do not survive a worker restart, and work that starts after
// one is seen again.
//
// When the barrier ends, the held writes are journaled in arrival order,
// unless a Close was accepted at the cut. The sealed relay's archive ends at
// the cut, so what the harness said after it is dropped rather than written
// behind the seal.

package worker

import (
	"log/slog"
	"maps"
	"reflect"
	"slices"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// HeldWrite is one write that waits for the checkpoint barrier to end.
type HeldWrite interface {
	isHeldWrite()
}

type HeldWriteSessionUpdate struct {
	Update acpschema.SessionUpdate
}

type HeldWriteObservation struct {
	Observation core.RelayObservation
}

func (HeldWriteSessionUpdate) isHeldWrite() {}
func (HeldWriteObservation) isHeldWrite()   {}

// observationCanWait is whether an observation can wait behind a checkpoint
// cut. Everything not listed is journaled at once and ends the hold, so a
// new kind of observation is never delayed until someone decides it can
// wait.
func observationCanWait(observation core.RelayObservation) bool {
	switch observation.(type) {
	case core.RelayObservationSessionUpdate, core.RelayObservationNotice,
		core.RelayObservationWarning, core.RelayObservationNativeAgent:
		return true
	}
	return false
}

// WorkerWritesHeld is whether the worker's own writes wait for the
// checkpoint barrier: a barrier is ready, and nothing has ended its hold
// early.
func (r *DurableRelay) WorkerWritesHeld() bool {
	barrier := r.snapshot.CheckpointBarrier
	return r.snapshot.CheckpointReadyThrough != nil &&
		barrier != nil &&
		(r.holdEndedFor == nil || *r.holdEndedFor != *barrier)
}

// closeCutRefusal is why a Close at this cut must be refused, or false when
// the cut is still the one the barrier made. The only place Close acceptance
// reads the cut: the barrier, its ready cursor, the journal frontier, and
// whether work started after it.
func (r *DurableRelay) closeCutRefusal(barrierCommandID string, expected *core.RelayCursor) (string, bool) {
	if !r.atCheckpointCut(barrierCommandID, expected) {
		return "close does not match the current checkpoint cut", true
	}
	spoiled, barrier := r.cutSpoiledFor, r.snapshot.CheckpointBarrier
	if (spoiled == nil) == (barrier == nil) && (spoiled == nil || *spoiled == *barrier) {
		return "background work started after the checkpoint cut", true
	}
	return "", false
}

// atCheckpointCut is whether expected is still the ready cut of the barrier
// barrierCommandID, with nothing journaled after it.
func (r *DurableRelay) atCheckpointCut(barrierCommandID string, expected *core.RelayCursor) bool {
	s := &r.snapshot
	return s.CheckpointBarrier != nil && *s.CheckpointBarrier == barrierCommandID &&
		s.CheckpointReadyThrough != nil && s.CheckpointReadyDigest != nil &&
		*s.CheckpointReadyThrough == expected.Ordinal && *s.CheckpointReadyDigest == expected.Digest &&
		s.LatestOrdinal == expected.Ordinal &&
		s.LatestDigest == expected.Digest
}

// checkpointMailbox is the mail the relay holds at the barrier's cut. A
// non-empty refusal is the reason the cut cannot be read (Rust's inner Err).
//
// The mail this relay holds at a ready barrier's cut, for the checkpoint
// that archives the cut. Events leased to a hook come back first, where
// returning the lease would put them: the hook never confirmed that the
// agent saw them. err is a frame-size failure.
func (r *DurableRelay) checkpointMailbox(barrierCommandID string, cut *core.RelayCursor) (mailbox core.CanonicalMailbox, refusal string, err error) {
	if !r.atCheckpointCut(barrierCommandID, cut) {
		return core.CanonicalMailbox{}, "the relay is no longer at this checkpoint cut", nil
	}
	var pendingEvents []core.MailboxEvent
	if lease := r.snapshot.MailboxHookLease; lease != nil {
		pendingEvents = slices.Clone(lease.Events)
	}
	pendingEvents = append(pendingEvents, r.snapshot.PendingMailboxEvents...)
	mailbox = core.CanonicalMailbox{
		PendingEvents:      pendingEvents,
		DeliveredEventKeys: maps.Clone(r.snapshot.DeliveredMailboxEventKeys),
		PromptContexts:     maps.Clone(r.snapshot.MailboxPromptContexts),
	}
	// The answer travels in one relay frame, which the relay snapshot that
	// holds these fields may exceed. Refuse with an answer rather than a
	// frame the transport would drop.
	if err := core.EnsureSerializedBudget(mailbox, core.MaxFrameBytes-core.RelayEventEnvelopeReserve, "checkpoint mailbox"); err != nil {
		return core.CanonicalMailbox{}, "", err
	}
	return mailbox, "", nil
}

// checkpointNativeUse is the worker-owned native-session-use evidence at a
// ready barrier's cut, or the reason it cannot be read there.
func (r *DurableRelay) checkpointNativeUse(barrierCommandID string, cut *core.RelayCursor) (*core.NativeSessionUseEvidence, string) {
	if !r.atCheckpointCut(barrierCommandID, cut) {
		return nil, "the relay is no longer at this checkpoint cut"
	}
	if r.snapshot.NativeUseEvidence == nil {
		return nil, ""
	}
	evidence := *r.snapshot.NativeUseEvidence
	return &evidence, ""
}

// spoilCheckpointCut marks the ready barrier's cut spoiled by background
// work that started after it, and ends its hold.
func (r *DurableRelay) spoilCheckpointCut() error {
	if r.snapshot.CheckpointReadyThrough == nil || r.snapshot.CheckpointBarrier == nil {
		return nil
	}
	barrier := *r.snapshot.CheckpointBarrier
	r.cutSpoiledFor = &barrier
	return r.EndCheckpointHold()
}

// currentBackgroundWork is the background work running now, by identity.
func (r *DurableRelay) currentBackgroundWork() map[string]struct{} {
	current := map[string]struct{}{}
	for _, command := range r.backgroundCommands() {
		current[command.ID] = struct{}{}
	}
	for id := range r.activeAgentTerminals {
		current["terminal:"+id] = struct{}{}
	}
	return current
}

// baselineBackgroundWorkAtCut records the work running when a barrier
// becomes ready, which does not spoil its cut.
func (r *DurableRelay) baselineBackgroundWorkAtCut() {
	r.knownBackgroundWork = r.currentBackgroundWork()
}

// noteBackgroundWork spoils the ready cut when background work started after
// it.
func (r *DurableRelay) noteBackgroundWork() error {
	current := r.currentBackgroundWork()
	started := false
	for id := range current {
		if _, known := r.knownBackgroundWork[id]; !known {
			started = true
			break
		}
	}
	r.knownBackgroundWork = current
	if started {
		return r.spoilCheckpointCut()
	}
	return nil
}

// EndCheckpointHold ends the hold early and journals the held writes in
// their order.
func (r *DurableRelay) EndCheckpointHold() error {
	if !r.WorkerWritesHeld() {
		return nil
	}
	barrier := *r.snapshot.CheckpointBarrier
	r.holdEndedFor = &barrier
	return r.replayHeldWrites()
}

// endCheckpointHoldFor ends the hold before journaling an observation that
// cannot wait, unless it is the barrier's own control or a Close sealed at
// the cut is pending.
func (r *DurableRelay) endCheckpointHoldFor(commandID *string, observation core.RelayObservation) error {
	if !r.WorkerWritesHeld() || r.ClosePending() || r.isCheckpointControl(commandID, observation) {
		return nil
	}
	return r.EndCheckpointHold()
}

// isCheckpointControl is whether the event belongs to a checkpoint barrier
// or Close command. These are how the cut is made, completed and sealed, not
// work that spoils it.
func (r *DurableRelay) isCheckpointControl(commandID *string, observation core.RelayObservation) bool {
	var command core.RelayCommand
	if queued, ok := observation.(core.RelayObservationCommandQueued); ok {
		command = queued.Command
	} else if commandID != nil {
		if dispatch, ok := r.snapshot.Dispatches[*commandID]; ok {
			command = dispatch.Command
		}
	}
	switch command.(type) {
	case core.RelayCommandBeginCheckpoint, core.RelayCommandCompleteCheckpoint,
		core.RelayCommandReleaseCheckpoint, core.RelayCommandClose:
		return true
	}
	return false
}

// settleHeldWrites journals the held writes once the barrier ended, or drops
// them behind the cut a close sealed.
func (r *DurableRelay) settleHeldWrites() error {
	if len(r.heldWrites) == 0 || r.WorkerWritesHeld() {
		return nil
	}
	if r.ClosePending() {
		slog.Info("dropped worker writes held behind the cut a close sealed",
			"session_id", r.snapshot.SessionID, "dropped", len(r.heldWrites))
		r.heldWrites = nil
		return nil
	}
	return r.replayHeldWrites()
}

// holdObservation returns the observation to journal now, or nil when it
// waits behind the cut.
func (r *DurableRelay) holdObservation(observation core.RelayObservation) (core.RelayObservation, error) {
	if !r.WorkerWritesHeld() {
		return observation, nil
	}
	if !observationCanWait(observation) {
		if err := r.EndCheckpointHold(); err != nil {
			return nil, err
		}
		return observation, nil
	}
	r.heldWrites = append(r.heldWrites, HeldWriteObservation{Observation: observation})
	return nil, nil
}

// holdSessionUpdate returns the update to journal now, or nil when it waits
// behind the cut.
func (r *DurableRelay) holdSessionUpdate(update acpschema.SessionUpdate) (*acpschema.SessionUpdate, error) {
	if !r.WorkerWritesHeld() {
		return &update, nil
	}
	needed, err := r.sessionUpdateNeedsTheJournalNow(&update)
	if err != nil {
		return nil, err
	}
	if needed {
		if err := r.EndCheckpointHold(); err != nil {
			return nil, err
		}
		return &update, nil
	}
	r.heldWrites = append(r.heldWrites, HeldWriteSessionUpdate{Update: update})
	return nil, nil
}

// sessionUpdateNeedsTheJournalNow is whether an update changes agent work: a
// goal change, or output that opens a harness turn.
//
// The same decisions RecordSessionUpdate makes, asked before it makes them:
// a goal change, a Codex goal that starts running, or Claude output that
// opens a turn of the harness's own.
func (r *DurableRelay) sessionUpdateNeedsTheJournalNow(update *acpschema.SessionUpdate) (bool, error) {
	goal := r.snapshot.Goal
	if _, err := goal.Apply(update); err != nil {
		return false, err
	}
	if !reflect.DeepEqual(goal, r.snapshot.Goal) {
		return true, nil
	}
	claude := r.harnessTurns == HarnessTurnPolicyClaudeAdapter
	if !claude || r.isClaudeStopAcknowledgement(update) {
		return false, nil
	}
	if _, notice := r.noticeOutsideAnyTurn(update); notice {
		return false, nil
	}
	return r.opensHarnessTurn(update), nil
}

func (r *DurableRelay) replayHeldWrites() error {
	writes := r.heldWrites
	r.heldWrites = nil
	for _, write := range writes {
		var err error
		switch write := write.(type) {
		case HeldWriteSessionUpdate:
			_, err = r.RecordSessionUpdate(write.Update)
		case HeldWriteObservation:
			_, err = r.RecordObservation(write.Observation)
		}
		if err != nil {
			return err
		}
	}
	return nil
}
