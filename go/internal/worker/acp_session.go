// Port of mj-worker/src/acp/session.rs.

package worker

import (
	"context"
	"fmt"
	"sync/atomic"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// startupRequestError keeps evidence from the ACP response before the
// worker records a text error.
func startupRequestError(err *acp.RPCError, step string) error {
	diagnostic := core.TurnDiagnosticFromACP(err)
	authFailure := !diagnostic.IsUsageLimit() &&
		(err.Code == acp.ErrorCodeAuthRequired || core.TurnDiagnosticReportsAuthFailure(&diagnostic))
	if authFailure {
		return fmt.Errorf("%s (%s): %w", step, core.PromptAuthRequiredMarker, acpDisplayError{err})
	}
	return fmt.Errorf("%s: %w", step, acpDisplayError{err})
}

// clientName is the name the worker gives an agent as its ACP client. It is
// the name of the package that ships the worker program, which this library
// was once part of; agents and their logs have always seen this name.
const clientName = "brokk-mj-worker"

// recordPromptUse records possible native history only when the prompt is
// ready to be sent.
func recordPromptUse(ctx context.Context, opened *Shared[*OpenedSession], events chan<- core.RuntimeEvent) error {
	// Mark before sending: even a failed reply cannot prove the agent did
	// not receive and persist this prompt.
	firstUse := false
	session, unlock := opened.Lock()
	if *session != nil {
		firstUse = !(*session).nativeSessionUsed.Swap(true)
	}
	unlock()
	// Persist the same fact durably, once, so a restart after this prompt
	// never treats the thread as an empty one to replace.
	if firstUse {
		return emitRuntimeEvent(ctx, events, core.RuntimeEventNativeSessionUsed{})
	}
	return nil
}

// acceptLiveHistory marks the boundary between a provider's replay of old
// history and the live history of this connection. Loading suppresses
// conversation replay, while current session metadata always passes. Every
// open path marks this boundary before accepting new conversation content.
func acceptLiveHistory(liveHistoryEnabled *atomic.Bool) {
	liveHistoryEnabled.Store(true)
}

// loadAtReplayBoundary sends `session/load` and decides the replay boundary
// where its response sits in the connection's incoming message order.
//
// The SDK's dispatch loop hands the next notification to its handler as soon
// as the response has been routed. A bridge that sends live updates right
// behind the response (Claude Code cycles that ran before the first prompt,
// for example) would therefore reach the notification handler before this
// session task resumed and flipped a flag. An ordered result callback runs
// inside the dispatch loop and holds it, so `ReplayCommit` and the flag
// change happen before any later notification is handled and every earlier
// one was handled as replay. The notification handler reads the flag; this
// callback is its only writer for `session/load`. A failed load decides
// nothing: the caller owns that error path.
func loadAtReplayBoundary(ctx context.Context, connection *acp.Connection, request schema.LoadSessionRequest, events chan<- core.RuntimeEvent, nativeChildren bool, liveHistoryEnabled *atomic.Bool) (schema.LoadSessionResponse, error) {
	panic("unported: mj-worker/src/acp/session.rs load_at_replay_boundary")
}

// opencodeToolRegistrySettle is how long after a session opens OpenCode
// needs before its tool registry has reloaded for the MCP servers it was
// given: its 100 ms debounce, doubled and rounded up.
const opencodeToolRegistrySettle = 250 * time.Millisecond

// opencodeMayMissFirstPromptTools says whether an OpenCode that named itself
// agentVersion in `initialize` may still start a turn before its MCP tools
// are registered. OpenCode 2.2.6 and later wait for starting MCP servers
// before each model call (anomalyco/opencode#60490); an older one, such as
// the 2.2.1 in a container built from an older image, does not. A missing or
// unparseable version takes the safe side and counts as older.
func opencodeMayMissFirstPromptTools(agentVersion *string) bool {
	panic("unported: mj-worker/src/acp/session.rs opencode_may_miss_first_prompt_tools")
}

func serveSession(ctx context.Context, connection *acp.Connection, spec *LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, terminals *TerminalRegistry, pendingElicitations *PendingElicitations, planImplementationSlot *PlanImplementationSlot, opened *Shared[*OpenedSession], agentOutputCount *AgentOutputCount, lastAgentMessage *LastAgentMessage, liveHistoryEnabled *atomic.Bool, nativeSessionUsed *atomic.Bool, sessionOpen BridgeSessionOpen, grokUsage *Collector) (SessionRestart, error) {
	panic("unported: mj-worker/src/acp/session.rs serve_session")
}

func shouldReplaceUnusedCodexThread(spec *LaunchSpec) bool {
	return spec.Harness == core.HarnessKindCodex &&
		spec.ResumeSession != nil &&
		spec.NativeSessionHistory == NativeSessionHistoryCreatedHereUnused &&
		!codexHasGoal(spec)
}

func codexHasGoal(spec *LaunchSpec) bool {
	if spec.Harness != core.HarnessKindCodex {
		return false
	}
	goal, unlock := spec.GoalRecovery.Lock()
	defer unlock()
	return goal.State.Snapshot != nil
}
