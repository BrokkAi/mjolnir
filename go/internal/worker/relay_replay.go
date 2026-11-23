// Port of mj-worker/src/relay/replay.rs.
//
// Replay pages and cursor validation, planned under the relay lock and read
// without it.

package worker

import (
	"errors"
	"fmt"
	"math"
	"slices"

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
	hotDigests := make([]relayOrdinalDigest, 0, len(r.hotEvents)+len(r.replayCursors))
	for _, event := range r.hotEvents {
		hotDigests = append(hotDigests, relayOrdinalDigest{ordinal: event.Ordinal, digest: event.Digest})
	}
	hotDigests = append(hotDigests, r.replayCursors...)
	return RelayReplayPlan{
		spans:                slices.Clone(r.journalSpans),
		hotDigests:           hotDigests,
		latestOrdinal:        r.snapshot.LatestOrdinal,
		latestDigest:         r.snapshot.LatestDigest,
		acknowledgedThrough:  r.snapshot.AcknowledgedThrough,
		acknowledgedDigest:   r.snapshot.AcknowledgedDigest,
		recoveryFloorOrdinal: r.snapshot.RecoveryFloorOrdinal,
		recoveryFloorDigest:  r.snapshot.RecoveryFloorDigest,
		retainedThrough:      r.snapshot.RetainedThrough(),
		retainedDigest:       r.snapshot.RetainedDigest(),
		generation:           r.journalGeneration,
	}
}

// RememberReplayCursor retains a cursor this worker just proved while
// reading a replay page. This is an optimization only: losing the cache
// causes another journal validation scan, never a loss of durable history.
func (r *DurableRelay) RememberReplayCursor(response *core.RelayResponseEnvelope) {
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		return
	}
	var throughOrdinal uint64
	var throughDigest string
	switch payload := ok.Payload.(type) {
	case core.RelayResponsePayloadAttached:
		throughOrdinal, throughDigest = payload.ThroughOrdinal, payload.ThroughDigest
	case core.RelayResponsePayloadAttachedWithKnownState:
		throughOrdinal, throughDigest = payload.ThroughOrdinal, payload.ThroughDigest
	default:
		return
	}
	if count := len(r.replayCursors); count > 0 &&
		r.replayCursors[count-1].ordinal == throughOrdinal && r.replayCursors[count-1].digest == throughDigest {
		return
	}
	r.replayCursors = slices.DeleteFunc(r.replayCursors, func(cursor relayOrdinalDigest) bool {
		return cursor.ordinal == throughOrdinal
	})
	if len(r.replayCursors) == core.RelayReplayCursorCapacity {
		r.replayCursors = r.replayCursors[1:]
	}
	r.replayCursors = append(r.replayCursors, relayOrdinalDigest{ordinal: throughOrdinal, digest: throughDigest})
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
	attach, ok := envelope.Request.(core.RelayRequestAttach)
	if !ok {
		return nil
	}
	if r.envelopeRejection(envelope) != nil {
		return nil
	}
	var knownStateDigest *string
	if attach.KnownStateDigest != nil {
		digest := *attach.KnownStateDigest
		knownStateDigest = &digest
	}
	state := r.OperationalState()
	return &DeferredRelayAttach{
		requestID:        envelope.RequestID,
		protocolVersion:  envelope.ProtocolVersion,
		plan:             r.replayPlan(),
		state:            state,
		afterOrdinal:     attach.AfterOrdinal,
		afterDigest:      attach.AfterDigest,
		knownStateDigest: knownStateDigest,
	}
}

// unreadableSpanError is a replay failure marked with the span that could not
// be read, as Rust's error.context(UnreadableRelaySpan{..}) marks it. Its
// Error is anyhow's `{:#}` of that chain; relayErrorDisplay gives the span
// message alone, as anyhow's Display does.
type unreadableSpanError struct {
	span  *UnreadableRelaySpan
	cause error
}

func (e *unreadableSpanError) Error() string { return e.span.Error() + ": " + e.cause.Error() }

func (e *unreadableSpanError) Unwrap() error { return e.cause }

// attach answers an attach from this plan. knownStateDigest is the digest of
// the state the controller holds; an unchanged state is not sent again. A
// worker speaks only the current protocol, so every controller it serves
// understands the omission.
func (p *RelayReplayPlan) attach(afterOrdinal uint64, afterDigest string, state core.RelayOperationalState, knownStateDigest *string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	if err := p.validateCursor(afterOrdinal, afterDigest); err != nil {
		refusal := core.NewRelayProtocolError(
			core.RelayErrorCodeDesynchronized,
			relayErrorDisplay(err),
			false,
			p.desynchronizedDetail(afterOrdinal, afterDigest),
		)
		return nil, &refusal, nil
	}
	page, err := p.readEventsAfter(afterOrdinal, afterDigest, core.RelayReplayByteBudget)
	if err != nil {
		// An unreadable old span cannot be served, but newer history still
		// can. Answer with a desynchronization cursor past the corruption so
		// the controller resynchronizes from there instead of retrying the
		// same unparseable bytes forever.
		var gap *unreadableSpanError
		if errors.As(err, &gap) {
			earliestAvailable, earliestDigest := p.recoveryCursorAfter(gap.span.recoverAfter)
			refusal := core.NewRelayProtocolError(
				core.RelayErrorCodeDesynchronized,
				relayErrorDisplay(err),
				false,
				core.RelayErrorDetailDesynchronized{
					RequestedAfter:    afterOrdinal,
					RequestedDigest:   afterDigest,
					EarliestAvailable: earliestAvailable,
					EarliestDigest:    earliestDigest,
					Latest:            p.latestOrdinal,
					LatestDigest:      p.latestDigest,
				},
			)
			return nil, &refusal, nil
		}
		return nil, nil, err
	}
	stateDigest, err := core.RelayStateDigest(&state)
	if err != nil {
		return nil, nil, err
	}
	if knownStateDigest != nil && *knownStateDigest == stateDigest {
		return core.RelayResponsePayloadAttachedWithKnownState{
			Events:         page.events,
			ThroughOrdinal: page.throughOrdinal,
			ThroughDigest:  page.throughDigest,
		}, nil, nil
	}
	if err := core.EnsureSerializedBudget(state, core.RelayStateByteBudget, "relay operational state"); err != nil {
		return nil, nil, err
	}
	return core.RelayResponsePayloadAttached{
		State:          state,
		Events:         page.events,
		ThroughOrdinal: page.throughOrdinal,
		ThroughDigest:  page.throughDigest,
		StateDigest:    &stateDigest,
	}, nil, nil
}

// recoveryCursorAfter is a resync cursor the controller can attach at to
// recover history past an unreadable span. It must name an ordinal whose
// digest is resolvable without the corrupt span, so it points at the first
// readable, self-valid event strictly after the corruption. The controller
// re-attaches after it and recovers every later event; at most the corrupt
// span and that one boundary event are lost. When nothing readable remains,
// it resumes live at the frontier.
func (p *RelayReplayPlan) recoveryCursorAfter(corruptLastOrdinal uint64) (uint64, string) {
	for _, span := range p.spans {
		if span.fileLastOrdinal <= corruptLastOrdinal {
			continue
		}
		var found *relayOrdinalDigest
		_, _ = visitRelayJournalFile(span.path, JournalReadModeRecover, func(event core.RelayEvent, _ int) (bool, error) {
			if event.Ordinal > corruptLastOrdinal && core.ValidateRelayEventSelf(&event) == nil {
				found = &relayOrdinalDigest{ordinal: event.Ordinal, digest: event.Digest}
				return true, nil
			}
			return false, nil
		})
		if found != nil {
			return found.ordinal, found.digest
		}
	}
	return p.latestOrdinal, p.latestDigest
}

func (p *RelayReplayPlan) validateCursor(afterOrdinal uint64, afterDigest string) error {
	if afterOrdinal < p.retainedThrough {
		return fmt.Errorf("event %d is no longer available; relay retained events after %d", afterOrdinal, p.retainedThrough)
	}
	if afterOrdinal > p.latestOrdinal {
		return fmt.Errorf("event %d is newer than relay frontier %d", afterOrdinal, p.latestOrdinal)
	}
	if err := core.ValidateRelayDigest(afterDigest, "event cursor digest"); err != nil {
		return err
	}
	expected, ok, err := p.digestAt(afterOrdinal)
	if err != nil {
		return err
	}
	if !ok {
		return fmt.Errorf("relay digest missing at event %d", afterOrdinal)
	}
	if afterDigest != expected {
		return fmt.Errorf("event %d digest does not match the relay event chain", afterOrdinal)
	}
	return nil
}

// digestAt is the digest of the event at ordinal, if this plan can prove it.
func (p *RelayReplayPlan) digestAt(ordinal uint64) (string, bool, error) {
	switch {
	case ordinal == 0:
		return core.RelayEventGenesisDigest, true, nil
	case ordinal == p.latestOrdinal:
		return p.latestDigest, true, nil
	case ordinal == p.acknowledgedThrough:
		return p.acknowledgedDigest, true, nil
	case ordinal == p.recoveryFloorOrdinal:
		return p.recoveryFloorDigest, true, nil
	}
	for _, hot := range p.hotDigests {
		if hot.ordinal == ordinal {
			return hot.digest, true, nil
		}
	}
	// A v1 span caches the digest at its boundary in the first event's
	// previous_digest, so the digest before the span can be read without
	// touching the segment. v2 records carry no such back-reference
	// (fileFirstPreviousDigest is nil), so fall through to read the event at
	// ordinal directly.
	if index := slices.IndexFunc(p.spans, func(span RelayJournalSpan) bool {
		return span.fileFirstOrdinal > 0 && span.fileFirstOrdinal-1 == ordinal && ordinal >= span.afterOrdinal
	}); index >= 0 {
		span := p.spans[index]
		if span.fileFirstPreviousDigest != nil {
			return *span.fileFirstPreviousDigest, true, nil
		}
		var digest *string
		if _, err := visitRelayJournalFile(span.path, JournalReadModeStrict, func(event core.RelayEvent, _ int) (bool, error) {
			if err := core.ValidateRelayEventSelf(&event); err != nil {
				return false, relayWithContext(err, fmt.Sprintf("validate relay journal %s", span.path))
			}
			if event.Format == core.RelayEventFormatV1 && event.PreviousDigest != "" {
				previous := event.PreviousDigest
				digest = &previous
			}
			return true, nil
		}); err != nil {
			return "", false, err
		}
		if digest != nil {
			return *digest, true, nil
		}
		// v2 first event: no cached boundary digest; read the target itself.
	}
	index := slices.IndexFunc(p.spans, func(span RelayJournalSpan) bool {
		return ordinal > span.afterOrdinal && ordinal <= span.fileLastOrdinal
	})
	if index < 0 {
		return "", false, nil
	}
	span := p.spans[index]
	var digest *string
	var previous *core.RelayEvent
	if _, err := visitRelayJournalFile(span.path, JournalReadModeStrict, func(event core.RelayEvent, _ int) (bool, error) {
		// Each record is validated by its own digest (the corruption check).
		// Between consecutive records the v1 chain link is also enforced; v2
		// records have no link and are trusted on their own digest.
		if err := core.ValidateRelayEventSelf(&event); err != nil {
			return false, relayWithContext(err, fmt.Sprintf("validate relay journal %s", span.path))
		}
		if previous != nil && event.Format == core.RelayEventFormatV1 && event.PreviousDigest != previous.Digest {
			return false, fmt.Errorf("relay journal %s event %d does not chain from event %d",
				span.path, event.Ordinal, previous.Ordinal)
		}
		if event.Ordinal == ordinal {
			found := event.Digest
			digest = &found
			return true, nil
		}
		previous = &event
		return false, nil
	}); err != nil {
		return "", false, err
	}
	if digest == nil {
		return "", false, nil
	}
	return *digest, true, nil
}

func (p *RelayReplayPlan) readEventsAfter(afterOrdinal uint64, afterDigest string, byteBudget int) (RelayEventPage, error) {
	events := []core.RelayEvent{}
	used := 0
	throughOrdinal := afterOrdinal
	// attach validated this exact cursor immediately before entering the
	// reader. Reusing it avoids a second decompression pass over the
	// cursor's sealed segment.
	throughDigest := afterDigest
	pageFull := false

	for _, span := range p.spans {
		if pageFull || span.fileLastOrdinal <= throughOrdinal {
			continue
		}
		_, err := visitRelayJournalFile(span.path, JournalReadModeStrict, func(event core.RelayEvent, encodedLen int) (bool, error) {
			if event.Ordinal <= span.afterOrdinal || event.Ordinal <= throughOrdinal {
				return false, nil
			}
			if event.Ordinal > p.latestOrdinal {
				// The active segment kept growing after this plan was
				// captured. Those events are real, but the reply's
				// operational state describes the frontier the plan saw, so
				// the page stops there and the caller asks again.
				return true, nil
			}
			if throughOrdinal == math.MaxUint64 {
				return false, errors.New("relay event ordinal exhausted")
			}
			if event.Ordinal != throughOrdinal+1 {
				return false, fmt.Errorf("relay journal page has a gap after event %d: found %d", throughOrdinal, event.Ordinal)
			}
			// The page is assembled off the relay lock, so it carries its own
			// proof that it is one unbroken run the cursor named rather than
			// fragments of a journal that moved. For v1 the in-record chain
			// link proves this; a v2 page relies instead on both endpoints
			// being digest anchors (the cursor and the frontier) plus each
			// interior record self-validating and the ordinals being
			// contiguous.
			if event.Format == core.RelayEventFormatV1 && event.PreviousDigest != throughDigest {
				return false, fmt.Errorf("relay journal event %d does not chain from event %d", event.Ordinal, throughOrdinal)
			}
			if err := core.ValidateRelayEvent(throughOrdinal, throughDigest, &event); err != nil {
				return false, relayWithContext(err, "validate relay journal page event")
			}
			if len(events) > 0 && relaySaturatingAdd(used, encodedLen) > byteBudget {
				pageFull = true
				return true, nil
			}
			used = relaySaturatingAdd(used, encodedLen)
			throughOrdinal = event.Ordinal
			throughDigest = event.Digest
			events = append(events, event)
			return false, nil
		})
		if err != nil {
			// This span will not parse. If newer, readable history exists past
			// it, mark the error so attach can send the controller a recovery
			// cursor after this span instead of a retryable failure it would
			// loop on. The corrupt bytes are never served as valid.
			if span.fileLastOrdinal < p.latestOrdinal {
				return RelayEventPage{}, &unreadableSpanError{
					span:  &UnreadableRelaySpan{recoverAfter: span.fileLastOrdinal},
					cause: err,
				}
			}
			return RelayEventPage{}, err
		}
		// A canonical span contributes every ordinal through its last one.
		// Stopping short means the file no longer holds what this plan
		// captured: it was sealed, rewritten, or pruned under the reader.
		if !pageFull && throughOrdinal < min(span.fileLastOrdinal, p.latestOrdinal) {
			return RelayEventPage{}, fmt.Errorf("relay journal %s no longer covers event %d", span.path, span.fileLastOrdinal)
		}
	}
	if !pageFull && (throughOrdinal != p.latestOrdinal || throughDigest != p.latestDigest) {
		// The spans end at the frontier this plan captured. Anything else is
		// a page assembled from a journal that moved, never a short answer a
		// caller could mistake for a complete one.
		return RelayEventPage{}, fmt.Errorf("relay journal ended at event %d, expected frontier %d", throughOrdinal, p.latestOrdinal)
	}
	return RelayEventPage{
		events:         events,
		throughOrdinal: throughOrdinal,
		throughDigest:  throughDigest,
	}, nil
}

// relaySaturatingAdd is usize::saturating_add.
func relaySaturatingAdd(a, b int) int {
	if b > 0 && a > math.MaxInt-b {
		return math.MaxInt
	}
	return a + b
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
