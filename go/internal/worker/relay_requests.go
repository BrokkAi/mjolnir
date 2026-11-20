// Port of mj-worker/src/relay/requests.rs.
//
// Answers relay request envelopes.

package worker

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (r *DurableRelay) Handle(envelope core.RelayRequestEnvelope) core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/relay/requests.rs handle")
}

// envelopeRejection is the answer to an envelope the relay refuses before
// handling it, or nil. TakeDeferredAttach consults the same checks so an
// attach it defers is refused exactly as Handle would refuse it.
func (r *DurableRelay) envelopeRejection(envelope *core.RelayRequestEnvelope) core.RelayResponseBody {
	panic("unported: mj-worker/src/relay/requests.rs envelope_rejection")
}

func (r *DurableRelay) handleInner(envelope *core.RelayRequestEnvelope) (core.RelayResponseBody, error) {
	panic("unported: mj-worker/src/relay/requests.rs handle_inner")
}

func (r *DurableRelay) InstallPromptContext(text string) error {
	panic("unported: mj-worker/src/relay/requests.rs install_prompt_context")
}

func (r *DurableRelay) attach(afterOrdinal uint64, afterDigest string, knownStateDigest *string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	state := r.OperationalState()
	plan := r.replayPlan()
	return plan.attach(afterOrdinal, afterDigest, state, knownStateDigest)
}

func (r *DurableRelay) acknowledge(throughOrdinal uint64, throughDigest string) (core.RelayResponsePayload, *core.RelayProtocolError, error) {
	panic("unported: mj-worker/src/relay/requests.rs acknowledge")
}
