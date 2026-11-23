// Port of mj-worker/src/worker_runtime/subagents/replies.rs.

// Full replies are private data; the queue publishes only compact receipts.

//go:build unix

package worker

import (
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (e *SubagentEndpoint) resultPath(requestID string) string {
	digest := sha256.Sum256([]byte(requestID))
	return filepath.Join(filepath.Dir(e.path), "subagent-replies", core.LowerHex(digest[:])+".json")
}

func (e *SubagentEndpoint) writeResult(result *core.SubagentToolResult) error {
	path := e.resultPath(result.RequestID)
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		return err
	}
	body, err := workerRuntimeMarshalJSON(result)
	if err != nil {
		return err
	}
	if err := core.AtomicWrite(path, body); err != nil {
		return fmt.Errorf("persist sub-agent reply %s: %w", result.RequestID, err)
	}
	return nil
}

func (e *SubagentEndpoint) readResult(state *queueState, id string) (*core.SubagentToolResult, error) {
	receipt, ok := state.completions[id]
	if !ok {
		return nil, nil
	}
	body, err := os.ReadFile(e.resultPath(id))
	if err == nil {
		var result core.SubagentToolResult
		if err := json.Unmarshal(body, &result); err != nil {
			return nil, fmt.Errorf("decode sub-agent reply: %w", err)
		}
		return &result, nil
	}
	_, callerAcknowledged := state.callerAcknowledged[id]
	_, daemonAcknowledged := state.daemonAcknowledged[id]
	if errors.Is(err, fs.ErrNotExist) && callerAcknowledged && daemonAcknowledged {
		// A consumed reply cannot authorize repeating its mutation.
		return &core.SubagentToolResult{
			RequestID:     id,
			CompletedAtMs: receipt.CompletedAtMs,
			IsError:       true,
			Message:       "This request already completed and its reply was acknowledged; the reply body has been retired. Do not repeat the operation.",
		}, nil
	}
	return nil, fmt.Errorf("read sub-agent reply %s: %w", id, err)
}

func (e *SubagentEndpoint) importLegacyResults() error {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	state := &e.state.state
	if len(state.results) == 0 {
		return nil
	}
	next := state.clone()
	results := next.results
	next.results = map[string]core.SubagentToolResult{}
	for _, id := range slices.Sorted(maps.Keys(results)) {
		result := results[id]
		if err := e.writeResult(&result); err != nil {
			return err
		}
		next.completions[id] = core.SubagentCompletionFrom(&result)
	}
	// Publish metadata only after every referenced body is durable. A crash
	// before this write simply imports the old queue again.
	if err := e.persist(&next); err != nil {
		return err
	}
	*state = next
	return nil
}

func (e *SubagentEndpoint) acknowledgeReplies(ids []string, caller bool) error {
	e.state.mu.Lock()
	defer e.state.mu.Unlock()
	state := &e.state.state
	next := state.clone()
	changed := false
	for _, id := range ids {
		if _, ok := next.completions[id]; !ok {
			continue
		}
		acknowledged := next.daemonAcknowledged
		if caller {
			acknowledged = next.callerAcknowledged
		}
		if _, ok := acknowledged[id]; !ok {
			acknowledged[id] = struct{}{}
			changed = true
		}
	}
	if changed {
		// The receipt survives a crash after the body is removed.
		if err := e.persist(&next); err != nil {
			return err
		}
		*state = next
	}
	for _, id := range ids {
		_, callerAcknowledged := state.callerAcknowledged[id]
		_, daemonAcknowledged := state.daemonAcknowledged[id]
		if !callerAcknowledged || !daemonAcknowledged {
			continue
		}
		if err := os.Remove(e.resultPath(id)); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return fmt.Errorf("retire sub-agent reply %s: %w", id, err)
		}
	}
	return nil
}

func (e *SubagentEndpoint) retireReceipts(state *queueState) error {
	// Bodies without both receipts remain durable. Backpressure at enqueue
	// bounds outstanding work; only acknowledged identities are evictable.
	for len(state.completions) > core.RelayRetryIDRetention {
		var oldest *core.SubagentCompletion
		for _, receipt := range state.completions {
			_, callerAcknowledged := state.callerAcknowledged[receipt.RequestID]
			_, daemonAcknowledged := state.daemonAcknowledged[receipt.RequestID]
			if !callerAcknowledged || !daemonAcknowledged {
				continue
			}
			if oldest == nil || receipt.CompletedAtMs < oldest.CompletedAtMs ||
				(receipt.CompletedAtMs == oldest.CompletedAtMs && strings.Compare(receipt.RequestID, oldest.RequestID) < 0) {
				candidate := receipt
				oldest = &candidate
			}
		}
		if oldest == nil {
			break
		}
		id := oldest.RequestID
		// Both acknowledgments are already durable. Retry failed cleanup
		// before forgetting the identity, including after a crashed ACK.
		if err := os.Remove(e.resultPath(id)); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return fmt.Errorf("retire acknowledged sub-agent reply: %w", err)
		}
		delete(state.completions, id)
		delete(state.callerAcknowledged, id)
		delete(state.daemonAcknowledged, id)
	}
	return nil
}
