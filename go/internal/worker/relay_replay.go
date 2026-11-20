// Port of mj-worker/src/relay/replay.rs.
//
// Replay pages and cursor validation, planned under the relay lock and read
// without it.

package worker

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type RelayEventPage struct {
	events         []core.RelayEvent
	throughOrdinal uint64
	throughDigest  string
}

// RelayReplayPlan is a point-in-time view of the durable journal: everything
// needed to validate a replay cursor and assemble a replay page, and nothing
// that requires the relay lock to read.
//
// Sealed segments are immutable and the active segment is append-only, so a
// captured span list stays readable while the relay keeps recording events.
// Sealing and garbage collection do invalidate it, so every read here is
// written to fail loudly rather than return a short or torn page, and
// generation lets the caller recognize that failure as a stale plan instead
// of a real desynchronization.
type RelayReplayPlan struct {
	spans []RelayJournalSpan
	// hotDigests are digests of recent live events and proven replay-page
	// cursors, so the next sequential attachment validates without touching
	// old segments.
	hotDigests           []relayOrdinalDigest
	latestOrdinal        uint64
	latestDigest         string
	acknowledgedThrough  uint64
	acknowledgedDigest   string
	recoveryFloorOrdinal uint64
	recoveryFloorDigest  string
	retainedThrough      uint64
	retainedDigest       string
	generation           uint64
}

// UnreadableRelaySpan reports a journal span that could not be parsed during
// a replay. When newer readable history exists past it, readEventsAfter
// attaches this to the error so attach can answer with a Desynchronized
// recovery cursor rather than a retryable failure that the controller would
// loop on forever.
type UnreadableRelaySpan struct {
	recoverAfter uint64
}

func (e *UnreadableRelaySpan) Error() string {
	return fmt.Sprintf("relay history is unreadable; readable events resume after event %d", e.recoverAfter)
}

// DeferredRelayAttach is an attach whose disk work has been lifted out of the
// relay lock.
//
// DurableRelay.TakeDeferredAttach builds one while holding the lock; Finish
// then does the reading, decompressing and page assembly with the lock
// released, so live event recording keeps running underneath a controller's
// catch-up.
type DeferredRelayAttach struct {
	requestID        string
	protocolVersion  uint32
	plan             RelayReplayPlan
	state            core.RelayOperationalState
	afterOrdinal     uint64
	afterDigest      string
	knownStateDigest *string
}

// legacyNativeReplayPlan is the startup-only view of the pre-upgrade
// retained prefix, before ACK-only GC.
func (r *DurableRelay) legacyNativeReplayPlan(ordinal uint64, digest string) RelayReplayPlan {
	plan := r.replayPlan()
	plan.retainedThrough = ordinal
	plan.retainedDigest = digest
	return plan
}

// replayPlan captures everything a replay page needs from this relay, so
// the file reads and gzip decompression behind it can run without the relay
// lock.
func (r *DurableRelay) replayPlan() RelayReplayPlan {
	panic("unported: mj-worker/src/relay/replay.rs replay_plan")
}

// RememberReplayCursor retains a cursor this worker just proved while
// reading a replay page. This is an optimization only: losing the cache
// causes another journal validation scan, never a loss of durable history.
func (r *DurableRelay) RememberReplayCursor(response *core.RelayResponseEnvelope) {
	panic("unported: mj-worker/src/relay/replay.rs remember_replay_cursor")
}

// TakeDeferredAttach splits an attach into the cheap part that needs the
// relay lock and the expensive part that does not.
//
// Catch-up over a long offline history reads page after page from disk and
// decompresses sealed segments. Doing that under the relay lock blocks live
// event recording until it finishes, which is exactly what a controller
// attaching is not supposed to cost the session. Nil means this envelope is
// not an attach, or is one that cannot be served at all; the caller falls
// back to Handle, which answers it.
func (r *DurableRelay) TakeDeferredAttach(envelope *core.RelayRequestEnvelope) *DeferredRelayAttach {
	panic("unported: mj-worker/src/relay/replay.rs take_deferred_attach")
}

// attach answers an attach from this plan. knownStateDigest is the digest of
// the state the controller holds; an unchanged state is not sent again. A
// worker speaks only the current protocol, so every controller it serves
// understands the omission.
func (p *RelayReplayPlan) attach(afterOrdinal uint64, afterDigest string, state core.RelayOperationalState, knownStateDigest *string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	panic("unported: mj-worker/src/relay/replay.rs attach")
}

func (p *RelayReplayPlan) recoveryCursorAfter(corruptLastOrdinal uint64) (uint64, string) {
	panic("unported: mj-worker/src/relay/replay.rs recovery_cursor_after")
}

func (p *RelayReplayPlan) validateCursor(afterOrdinal uint64, afterDigest string) error {
	panic("unported: mj-worker/src/relay/replay.rs validate_cursor")
}

// digestAt is the digest of the event at ordinal, if this plan can prove it.
func (p *RelayReplayPlan) digestAt(ordinal uint64) (string, bool, error) {
	panic("unported: mj-worker/src/relay/replay.rs digest_at")
}

func (p *RelayReplayPlan) readEventsAfter(afterOrdinal uint64, afterDigest string, byteBudget int) (RelayEventPage, error) {
	panic("unported: mj-worker/src/relay/replay.rs read_events_after")
}

func (p *RelayReplayPlan) desynchronizedDetail(requestedAfter uint64, requestedDigest string) core.RelayErrorDetail {
	return core.RelayErrorDetailDesynchronized{
		RequestedAfter:    requestedAfter,
		RequestedDigest:   requestedDigest,
		EarliestAvailable: p.retainedThrough,
		EarliestDigest:    p.retainedDigest,
		Latest:            p.latestOrdinal,
		LatestDigest:      p.latestDigest,
	}
}

// JournalGeneration is the journal generation this attach was planned
// against. Compare it with DurableRelay.JournalGeneration after Finish
// fails: an unchanged generation means the failure is real, and a changed
// one means the journal was resealed or collected mid-read and the
// controller should simply attach again.
func (a *DeferredRelayAttach) JournalGeneration() uint64 {
	return a.plan.generation
}

// SetHarnessPreparation joins the worker's harness preparation, which lives
// outside the relay, to the state before Finish digests that state.
func (a *DeferredRelayAttach) SetHarnessPreparation(preparation core.HarnessPreparation) {
	a.state.HarnessPreparation = preparation
}

// Finish is blocking: it reads journal segments and decompresses sealed
// ones. Callers must run it off any event loop.
func (a *DeferredRelayAttach) Finish() core.RelayResponseEnvelope {
	var body core.RelayResponseBody
	payload, refusal, err := a.plan.attach(a.afterOrdinal, a.afterDigest, a.state, a.knownStateDigest)
	switch {
	case err != nil:
		body = core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:      core.RelayErrorCodeInternal,
			Message:   err.Error(),
			Retryable: true,
		}}
	case refusal != nil:
		body = core.RelayResponseBodyError{Error: *refusal}
	default:
		body = core.RelayResponseBodyOk{Payload: payload}
	}
	return core.RelayResponseEnvelope{RequestID: a.requestID, ProtocolVersion: a.protocolVersion, Body: body}
}

// DeferredRelayAttachStaleJournalResponse is the answer for an attach whose
// plan went stale while it was reading: nothing is wrong with the
// controller's cursor, so it retries against the journal as it now stands.
func DeferredRelayAttachStaleJournalResponse(requestID string, protocolVersion uint32) core.RelayResponseEnvelope {
	return core.RelayResponseEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Body: core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:      core.RelayErrorCodeInternal,
			Message:   "relay journal was resealed or collected while the replay page was being read; attach again",
			Retryable: true,
		}},
	}
}
