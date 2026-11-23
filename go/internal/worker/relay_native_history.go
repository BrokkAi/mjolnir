// Port of mj-worker/src/relay/native_history.rs.
//
// Upgrade-time recovery of the native-session use evidence and pending
// questions older relay snapshots did not record.

package worker

import (
	"fmt"
	"log/slog"
	"maps"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recoverNativeHistoryEvidence: older snapshots predate the evidence that
// permits replacing an empty native session. Recover that evidence once,
// without replaying historical commands over the live snapshot or changing
// any journal frontiers.
func (r *DurableRelay) recoverNativeHistoryEvidence() bool {
	if r.snapshot.NativeUseEvidence != nil {
		return false
	}
	if r.snapshot.NativeSessionUsed {
		r.snapshot.RecordNativeUse(core.NativeSessionUseUsed)
		return true
	}
	// Shipped snapshots already proved this identity was opened after the
	// retained floor and never used. Startup text is not contrary evidence.
	if opened := r.snapshot.NativeSessionOpenedOrdinal; opened != nil && *opened > r.snapshot.RecoveryFloorOrdinal {
		usage := core.NativeSessionUseUnused
		for _, dispatch := range r.snapshot.Dispatches {
			if dispatch.PromptMayHaveReachedAgent() {
				r.snapshot.NativeSessionUsed = true
				usage = core.NativeSessionUseUnknown
				break
			}
		}
		r.snapshot.RecordNativeUse(usage)
		return true
	}
	evidence, err := r.readNativeHistoryEvidence()
	if err != nil {
		// Old sealed history may be damaged even while the current native
		// conversation is usable. Keep resume possible, but do not authorize
		// an empty replacement from unproven evidence.
		slog.Warn(fmt.Sprintf("could not recover native session history evidence: %v", err),
			"session_id", r.snapshot.SessionID)
		r.snapshot.NativeSessionUsed = true
		r.snapshot.RecordNativeUse(core.NativeSessionUseUnknown)
		return true
	}
	r.snapshot.NativeSessionOpenedOrdinal = nil
	if opened := evidence.opened; opened != nil &&
		r.snapshot.NativeSessionID != nil && opened.nativeSessionID == *r.snapshot.NativeSessionID {
		ordinal := opened.ordinal
		r.snapshot.NativeSessionOpenedOrdinal = &ordinal
	}
	openedOrdinal := r.snapshot.NativeSessionOpenedOrdinal
	r.snapshot.NativeSessionUsed = evidence.used ||
		openedOrdinal == nil || *openedOrdinal <= r.snapshot.RecoveryFloorOrdinal
	if r.snapshot.NativeSessionUsed {
		r.snapshot.RecordNativeUse(core.NativeSessionUseUnknown)
	} else {
		r.snapshot.RecordNativeUse(core.NativeSessionUseUnused)
	}
	if evidence.used {
		r.snapshot.RecordNativeUse(core.NativeSessionUseUsed)
	}
	return true
}

func (r *DurableRelay) readNativeHistoryEvidence() (NativeHistoryEvidence, error) {
	var evidence NativeHistoryEvidence
	err := r.visitLegacyEvidence(func(event core.RelayEvent) {
		evidence.observe(event, r.snapshot.Dispatches)
	})
	return evidence, err
}

func (r *DurableRelay) recoverPendingElicitations() bool {
	missing := map[string]struct{}{}
	for _, id := range r.snapshot.AssessmentQuestions {
		if _, ok := r.snapshot.PendingElicitations[id]; !ok {
			missing[id] = struct{}{}
		}
	}
	if len(missing) == 0 {
		return false
	}
	recovered := map[string]core.ElicitationRequest{}
	for _, event := range r.hotEvents {
		if requested, ok := event.Observation.(core.RelayObservationElicitationRequested); ok {
			if _, wanted := missing[requested.Request.ID]; wanted {
				recovered[requested.Request.ID] = requested.Request
			}
		}
	}
	if len(recovered) != len(missing) {
		if err := r.visitLegacyEvidence(func(event core.RelayEvent) {
			if requested, ok := event.Observation.(core.RelayObservationElicitationRequested); ok {
				if _, wanted := missing[requested.Request.ID]; wanted {
					recovered[requested.Request.ID] = requested.Request
				}
			}
		}); err != nil {
			slog.Warn(fmt.Sprintf("could not read legacy pending question history; preserving unavailable IDs: %v", err),
				"session_id", r.snapshot.SessionID)
		}
	}
	if r.snapshot.PendingElicitations == nil {
		r.snapshot.PendingElicitations = map[string]*core.ElicitationRequest{}
	}
	for _, id := range slices.Sorted(maps.Keys(missing)) {
		var body *core.ElicitationRequest
		if request, ok := recovered[id]; ok {
			body = &request
			delete(recovered, id)
		} else {
			slog.Warn("legacy pending question body is unavailable; the worker remains usable",
				"session_id", r.snapshot.SessionID, "question_id", id)
		}
		r.snapshot.PendingElicitations[id] = body
	}
	return true
}

func (r *DurableRelay) visitLegacyEvidence(visit func(core.RelayEvent)) error {
	ordinal := min(r.snapshot.AcknowledgedThrough, r.snapshot.RecoveryFloorOrdinal)
	digest := r.snapshot.RecoveryFloorDigest
	if ordinal == r.snapshot.AcknowledgedThrough {
		digest = r.snapshot.AcknowledgedDigest
	}
	plan := r.legacyNativeReplayPlan(ordinal, digest)
	// Reuse bounded, digest-validated replay, including sealed segments.
	// This runs during worker startup, before it serves any requests.
	for ordinal < r.snapshot.LatestOrdinal {
		page, err := plan.readEventsAfter(ordinal, digest, core.RelayReplayByteBudget)
		if err != nil {
			return err
		}
		for _, event := range page.events {
			visit(event)
		}
		ordinal = page.throughOrdinal
		digest = page.throughDigest
	}
	return nil
}

// nativeHistoryOpened is the native session a SessionOpened event named and
// that event's ordinal (Rust's (String, u64)).
type nativeHistoryOpened struct {
	nativeSessionID string
	ordinal         uint64
}

type NativeHistoryEvidence struct {
	opened *nativeHistoryOpened
	used   bool
	// commands keeps identities and kinds only; queued prompt bodies can be
	// large.
	commands map[string]core.RelayCommandKind
}

func (e *NativeHistoryEvidence) observe(event core.RelayEvent, dispatches map[string]core.RelayDispatchRecord) {
	if e.commands == nil {
		e.commands = map[string]core.RelayCommandKind{}
	}
	switch observation := event.Observation.(type) {
	case core.RelayObservationSessionOpened:
		e.opened = &nativeHistoryOpened{nativeSessionID: observation.NativeSessionID, ordinal: event.Ordinal}
		e.used = e.used || observation.Resumed
	case core.RelayObservationCommandQueued:
		e.commands[observation.CommandID] = core.RelayCommandKindOf(observation.Command)
	case core.RelayObservationCommandStarted:
		// An unknown command may have been queued below the retained
		// frontier; its absence cannot prove that nothing was sent.
		kind, known := e.commands[observation.CommandID]
		couldBePrompt := !known || kind == core.RelayCommandKindPrompt
		// CommandStarted promotes a prompt to Pending before ACP receives it.
		// The saved dispatch state can still prove that this accepted work
		// has never left the worker's queue.
		dispatch, dispatched := dispatches[observation.CommandID]
		e.used = e.used || (couldBePrompt && (!dispatched || dispatch.PromptMayHaveReachedAgent()))
	case core.RelayObservationCommandCompleted:
		delete(e.commands, observation.CommandID)
		switch outcome := observation.Outcome.(type) {
		case core.RelayCommandOutcomeContextCleared:
			e.opened = &nativeHistoryOpened{nativeSessionID: outcome.NativeSessionID, ordinal: event.Ordinal}
			e.used = false
		case core.RelayCommandOutcomePrompt, core.RelayCommandOutcomeSteered:
			e.used = true
		}
	case core.RelayObservationCommandInterrupted:
		delete(e.commands, observation.CommandID)
		e.used = e.used || observation.Command == core.RelayCommandKindPrompt
	case core.RelayObservationCommandRejected:
		delete(e.commands, observation.CommandID)
	case core.RelayObservationSessionUpdate:
		e.used = e.used || !core.SessionUpdateIsSessionState(&observation.Update)
	case core.RelayObservationNativeAgent:
		switch observation.Event.(type) {
		case core.NativeAgentEventSpawned, core.NativeAgentEventState, core.NativeAgentEventUpdate:
			e.used = true
		}
	case core.RelayObservationHarnessTurnStarted, core.RelayObservationPermissionAutoApproved,
		core.RelayObservationElicitationRequested:
		e.used = true
	}
}
