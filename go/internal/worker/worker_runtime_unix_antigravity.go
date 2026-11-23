// Port of mj-worker/src/worker_runtime/unix/antigravity.rs.
//
// Antigravity's Stop hook and its native task state, read from the
// databases the harness keeps in its home.

//go:build unix

package worker

import (
	"context"
	"fmt"
	"log/slog"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// handleAntigravityStop answers an Antigravity Stop hook, sampling the
// committed summary row first when the hook does not say the conversation
// is fully idle. home is nil when the Antigravity home is unknown.
func handleAntigravityStop(ctx context.Context, relay *RelayCell, envelope core.RelayRequestEnvelope, home *string) core.RelayResponseEnvelope {
	stop, ok := envelope.Request.(core.RelayRequestAntigravityStop)
	if !ok {
		panic("unreachable: handleAntigravityStop only serves Antigravity Stop hooks")
	}

	var baseline **[32]byte
	if (stop.FullyIdle == nil || !*stop.FullyIdle) && home != nil && stop.ConversationID != nil {
		guard := relay.Lock()
		matches := guard.matchesAntigravityConversation(stop.ConversationID)
		guard.Unlock()
		if matches {
			homePath, id := *home, *stop.ConversationID
			type sample struct {
				digest *[32]byte
				err    error
			}
			// Antigravity waits for the hook before publishing its deferred
			// Idle transition. Read the committed row now, even if the
			// monitor missed it.
			sampled, joinErr := core.SpawnBlocking(func() sample {
				digest, err := ReadSummaryRowDigest(homePath, id)
				return sample{digest: digest, err: err}
			}).Join(ctx)
			switch {
			case joinErr != nil:
				slog.Warn("could not read Antigravity Stop baseline",
					"error", fmt.Sprintf("sample Antigravity Stop baseline: %s", joinErr))
			case sampled.err != nil:
				slog.Warn("could not read Antigravity Stop baseline", "error", sampled.err)
			default:
				digest := sampled.digest
				baseline = &digest
			}
		}
	}

	guard := relay.Lock()
	defer guard.Unlock()
	return guard.handleWithAntigravityStopBaseline(envelope, baseline)
}

const antigravityTaskPollInterval = 500 * time.Millisecond

const antigravityTaskMaxRetryDelay = 8 * time.Second

// AntigravityTaskMonitor follows the native task state of the Antigravity
// conversation the harness opened.
type AntigravityTaskMonitor struct {
	// home is the Antigravity home, or why it cannot be located.
	home            kimiHome
	nativeSessionID *string
	follower        *AntigravityStateFollower
	retryAt         time.Time
	retryDelay      time.Duration
	lastWarning     *string
}

func newAntigravityTaskMonitor(home kimiHome) *AntigravityTaskMonitor {
	return &AntigravityTaskMonitor{home: home, retryAt: time.Now(), retryDelay: antigravityTaskPollInterval}
}

func (m *AntigravityTaskMonitor) detach() {
	m.nativeSessionID = nil
	m.follower = nil
	m.retryAt = time.Now()
	m.retryDelay = antigravityTaskPollInterval
	m.lastWarning = nil
}

func (m *AntigravityTaskMonitor) attach(ctx context.Context, nativeSessionID string, relay *RelayCell) error {
	if m.nativeSessionID != nil && *m.nativeSessionID == nativeSessionID {
		return nil
	}
	m.detach()
	m.nativeSessionID = &nativeSessionID
	return m.refresh(ctx, relay, true)
}

// refresh scans the databases when due, or at once when force, and applies
// the snapshot. A scan the relay rejects because a hook arrived meanwhile is
// retried once when forced; its cursor is never committed.
func (m *AntigravityTaskMonitor) refresh(ctx context.Context, relay *RelayCell, force bool) error {
	if m.nativeSessionID == nil {
		return nil
	}
	if !force && time.Now().Before(m.retryAt) {
		return nil
	}
	follower := m.follower
	m.follower = nil
	if follower == nil {
		if m.home.err != nil {
			return m.recordFailure(relay, fmt.Errorf("cannot locate Antigravity home: %s", m.home.err))
		}
		follower = NewAntigravityStateFollower(m.home.path, *m.nativeSessionID)
	}
	attempts := 1
	if force {
		attempts = 2
	}
	for attempt := range attempts {
		guard := relay.Lock()
		scanHookEpoch := guard.AntigravityHookEpoch()
		guard.Unlock()
		type scan struct {
			snapshot AntigravitySnapshot
			cursor   AntigravityRefreshCursor
			err      error
		}
		scanFollower := follower
		scanned, joinErr := core.SpawnBlocking(func() scan {
			snapshot, cursor, err := scanFollower.RefreshPending()
			return scan{snapshot: snapshot, cursor: cursor, err: err}
		}).Join(ctx)
		if joinErr != nil {
			return m.recordFailure(relay, fmt.Errorf("Antigravity background task scan stopped: %s", joinErr))
		}
		if scanned.err != nil {
			m.follower = scanFollower
			return m.recordFailure(relay, scanned.err)
		}
		m.retryDelay = antigravityTaskPollInterval
		m.retryAt = time.Now().Add(m.retryDelay)
		m.lastWarning = nil
		guard = relay.Lock()
		applied, err := guard.AntigravitySnapshotChanged(scanned.snapshot, scanHookEpoch)
		guard.Unlock()
		if err != nil {
			return err
		}
		if applied {
			scanFollower.CommitRefresh(scanned.cursor)
			m.follower = scanFollower
			return nil
		}
		if attempt == 1 || !force {
			m.retryAt = time.Now()
			m.follower = scanFollower
			return nil
		}
		follower = scanFollower
	}
	return nil
}

func (m *AntigravityTaskMonitor) recordFailure(relay *RelayCell, cause error) error {
	m.retryAt = time.Now().Add(m.retryDelay)
	m.retryDelay = min(m.retryDelay*2, antigravityTaskMaxRetryDelay)
	message := fmt.Sprintf("Antigravity background task tracking is unavailable; worker replacement is blocked: %v", cause)
	guard := relay.Lock()
	defer guard.Unlock()
	if err := guard.AntigravityTasksUnavailable(); err != nil {
		return err
	}
	if m.lastWarning == nil || *m.lastWarning != message {
		if _, err := guard.RecordObservation(core.RelayObservationWarning{Message: message}); err != nil {
			return err
		}
		m.lastWarning = &message
	}
	return nil
}

// prepareAntigravityRuntimeEvent keeps the monitor attached to the
// conversation the harness is in, before the coordinator applies event.
func prepareAntigravityRuntimeEvent(ctx context.Context, monitor *AntigravityTaskMonitor, relay *RelayCell, event core.RuntimeEvent) error {
	if monitor == nil {
		return nil
	}
	switch event := event.(type) {
	case core.RuntimeEventSessionStarted:
		if monitor.nativeSessionID == nil || *monitor.nativeSessionID != event.NativeSessionID {
			// Polls between session open and configuration must not
			// publish state from the previous conversation.
			monitor.detach()
		}
		return nil
	case core.RuntimeEventContextCleared:
		if monitor.nativeSessionID == nil || *monitor.nativeSessionID != event.NativeSessionID {
			monitor.detach()
		}
		return nil
	// The preceding SessionStarted or ContextCleared records the ID.
	// SessionConfigured is also emitted for later config changes, so attach
	// keeps the existing follower when that ID has not changed.
	case core.RuntimeEventSessionConfigured:
		guard := relay.Lock()
		id := guard.OperationalState().NativeSessionID
		guard.Unlock()
		if id != nil {
			return monitor.attach(ctx, *id, relay)
		}
		return nil
	case core.RuntimeEventHarnessRestarting, core.RuntimeEventContextClearing:
		monitor.detach()
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.resetAntigravityState()
	case core.RuntimeEventPromptFinished:
		return monitor.refresh(ctx, relay, true)
	}
	return nil
}
