// Port of mj-worker/src/acp/operator_away.rs: the `test-hooks` feature.

//go:build testhooks

package worker

import "github.com/BrokkAi/mjolnir/go/internal/core"

// keepFixtureQuestionLive registers question on a bridge of its own and
// returns what keeps it live: the bridge and its answer receiver (Rust
// `Box<dyn Send>`). The question stays live while the caller holds it.
func (o *OperatorAway) keepFixtureQuestionLive(question core.ElicitationRequest) any {
	bridge := o.pendingForBridgeWithSource(VerdictSourceDisabled{}, nil)
	answer := bridge.register(question, "", AwayRequestQuestion{})
	return []any{bridge, answer}
}
