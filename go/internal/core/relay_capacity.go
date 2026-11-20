// Port of mj-core/src/relay/capacity.rs.
//
// Durable server retry state. Legacy capacity names remain in stored and wire
// formats.

package core

import (
	"fmt"
	"math"
	"strings"
)

type RetryAssessment struct {
	CommandID string       `json:"command_id"`
	Ordinal   uint64       `json:"ordinal"`
	Evidence  TurnEvidence `json:"evidence"`
}

// CapacityStopReason: read-only compatibility with capacity completions
// written by older workers.
const CapacityStopReason = "ModelCapacity"

type CapacityRetry struct {
	Attempt   uint32 `json:"attempt"`
	RetryAtMs int64  `json:"retry_at_ms"`
	CommandID string `json:"command_id"`
	// Retained internally across the retry turn to advance its backoff.
	Submitted bool `json:"submitted"`
}

func NewCapacityRetry(attempt uint32, ordinal uint64, nowMs int64) CapacityRetry {
	shift := uint32(0)
	if attempt > 0 {
		shift = min(attempt-1, 4)
	}
	delayMs := int64(60_000) * (int64(1) << shift)
	retryAt := nowMs + delayMs
	if nowMs > math.MaxInt64-delayMs {
		retryAt = math.MaxInt64
	}
	return CapacityRetry{
		Attempt:   attempt,
		RetryAtMs: retryAt,
		CommandID: GeneratedPromptServerRetry.CommandID(ordinal),
	}
}

func (r CapacityRetry) Status(nowMs int64) string {
	remaining := int64(0)
	if r.RetryAtMs > nowMs {
		// Saturating: a difference past int64 still means "far away".
		remaining = r.RetryAtMs - nowMs
		if remaining < 0 {
			remaining = math.MaxInt64
		}
	}
	seconds := (uint64(remaining) + 999) / 1000
	return fmt.Sprintf("Provider unavailable · retrying in %dm%02ds", seconds/60, seconds%60)
}

// IsCapacityStopReason recognizes historical stop reasons written by an
// older worker.
func IsCapacityStopReason(stopReason string) bool {
	// CapacityStopReason has no letter with a non-ASCII case fold, so this
	// matches Rust's eq_ignore_ascii_case.
	return strings.EqualFold(stopReason, CapacityStopReason)
}

func IsCapacityRetryCommand(commandID string) bool {
	_, ok := RetriedTurn(commandID)
	return ok
}

// RetriedTurn is the completion ordinal of the turn a worker retry command
// sends again.
//
// The worker names each retry after the turn it retries: its id carries the
// ordinal at which that turn completed. Older workers used the
// capacity-retry- prefix. False when the command is not a retry.
func RetriedTurn(commandID string) (uint64, bool) {
	for _, producer := range []GeneratedPrompt{GeneratedPromptCapacityRetry, GeneratedPromptServerRetry} {
		if ordinal, ok := producer.RetryOrdinal(commandID); ok {
			return ordinal, true
		}
	}
	return 0, false
}
