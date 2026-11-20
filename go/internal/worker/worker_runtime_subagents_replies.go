// Port of mj-worker/src/worker_runtime/subagents/replies.rs.

// Full replies are private data; the queue publishes only compact receipts.

//go:build unix

package worker

import (
	"crypto/sha256"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (e *SubagentEndpoint) resultPath(requestID string) string {
	digest := sha256.Sum256([]byte(requestID))
	return filepath.Join(filepath.Dir(e.path), "subagent-replies", core.LowerHex(digest[:])+".json")
}

func (e *SubagentEndpoint) writeResult(result *core.SubagentToolResult) error {
	panic("unported: mj-worker/src/worker_runtime/subagents/replies.rs SubagentEndpoint::write_result")
}

func (e *SubagentEndpoint) readResult(state *queueState, id string) (*core.SubagentToolResult, error) {
	panic("unported: mj-worker/src/worker_runtime/subagents/replies.rs SubagentEndpoint::read_result")
}

func (e *SubagentEndpoint) importLegacyResults() error {
	panic("unported: mj-worker/src/worker_runtime/subagents/replies.rs SubagentEndpoint::import_legacy_results")
}

func (e *SubagentEndpoint) acknowledgeReplies(ids []string, caller bool) error {
	panic("unported: mj-worker/src/worker_runtime/subagents/replies.rs SubagentEndpoint::acknowledge_replies")
}

func (e *SubagentEndpoint) retireReceipts(state *queueState) error {
	panic("unported: mj-worker/src/worker_runtime/subagents/replies.rs SubagentEndpoint::retire_receipts")
}
