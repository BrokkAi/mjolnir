// Port of the test-only items of mj-worker-staging/src/worker_binary/launch.rs
// (`#[cfg(test)]`).

package workerstaging

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SubagentToolsEnabled is whether this session gets Mjolnir's delegation
// tools in place of its harness's own. The session's stored choice governs;
// nil means native sub-agents, so a session created before the per-session
// choice existed (or through `mj new` with neither flag given) gets its
// harness's own sub-agents. A child never gets them, and only a harness
// whose HarnessKind.SupportsDelegationTools holds can receive them at all.
func SubagentToolsEnabled(session *core.SessionRecord, isChild bool) bool {
	var child *core.SubagentRecord
	if isChild {
		child = &core.SubagentRecord{
			Kind:           core.SubagentKindWorker,
			ChildSessionID: session.ID,
			ProfileID:      session.LastProfile,
			HandbackTool:   true,
		}
	}
	policy := core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	if session.Subagents != nil {
		policy = *session.Subagents
	}
	launched := policy.ForLaunch(session.HarnessKind, child)
	return launched.UsesMjolnir()
}
