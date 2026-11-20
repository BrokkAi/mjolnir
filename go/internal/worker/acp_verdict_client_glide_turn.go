// Port of mj-worker/src/acp/verdict_client/glide_turn.rs.

// Evidence-only GLiDE boundary, independent of the older classifiers'
// budgets.

package worker

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// project is the GLiDE completed-turn request body (a JSON value) for
// evidence, judged against history.
func project(evidence *core.TurnEvidence, history core.ContextHistory, facts *core.ActivityFacts, nativeAgentCount int) (any, error) {
	panic("unported: mj-worker/src/acp/verdict_client/glide_turn.rs project")
}

// shrink removes the oldest authorization message first, then progressively
// shortens the longest remaining message while preserving both ends. False
// when nothing is left to remove.
func shrink(history *core.ContextHistory) bool {
	panic("unported: mj-worker/src/acp/verdict_client/glide_turn.rs shrink")
}

// Probabilities is GLiDE's completed-turn answer (Rust re-exports
// mj_core::activity::eot_glide::types::Probabilities); parse it with
// core.ProbabilitiesParse.
type Probabilities = core.Probabilities
