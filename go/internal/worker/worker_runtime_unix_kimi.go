// Port of mj-worker/src/worker_runtime/unix/kimi.rs.
//
// Kimi's background tasks, native agents, turn usage and turn failures are
// read from its wire journal, which trails the ACP prompt response.

//go:build unix

package worker

import (
	"context"
	"fmt"
	"math"
	"path/filepath"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const kimiTaskPollInterval = 500 * time.Millisecond

const kimiTaskMaxRetryDelay = 8 * time.Second

// kimiTurnEndPollInterval and kimiTurnEndPolls bound the wait for the wire
// record that closes a finished turn.
const kimiTurnEndPollInterval = 100 * time.Millisecond

const kimiTurnEndPolls uint32 = 5

// KimiTaskMonitor follows the wire journal of the Kimi session the harness
// opened.
type KimiTaskMonitor struct {
	home                kimiHome
	nativeSessionID     *string
	follower            *KimiWireFollower
	consecutiveFailures uint32
	retryAt             time.Time
	lastWarning         *string
}

func newKimiTaskMonitor(home kimiHome) *KimiTaskMonitor {
	return &KimiTaskMonitor{home: home, retryAt: time.Now()}
}

// turnSettledSince is whether Kimi has closed the turn that belongs to a
// prompt started at startedAtMs; known is false when no such turn is
// journalled.
func (m *KimiTaskMonitor) turnSettledSince(startedAtMs int64) (settled, known bool) {
	if m.follower == nil {
		return false, false
	}
	return m.follower.TurnSettledSince(startedAtMs)
}

func (m *KimiTaskMonitor) detach() {
	m.nativeSessionID = nil
	m.follower = nil
	m.consecutiveFailures = 0
	m.retryAt = time.Now()
	m.lastWarning = nil
}

func (m *KimiTaskMonitor) attach(ctx context.Context, nativeSessionID string, relay *RelayCell) error {
	m.nativeSessionID = &nativeSessionID
	m.follower = nil
	m.consecutiveFailures = 0
	m.retryAt = time.Now()
	return m.refresh(ctx, relay, true)
}

// refresh rescans the wire journal when it is due, or at once when force,
// and publishes its tasks and native agents to the relay.
func (m *KimiTaskMonitor) refresh(ctx context.Context, relay *RelayCell, force bool) error {
	if m.nativeSessionID == nil || (!force && time.Now().Before(m.retryAt)) {
		return nil
	}
	if m.home.err != nil {
		return m.recordFailure(relay, fmt.Errorf("cannot locate Kimi home: %s", m.home.err))
	}
	home := m.home.path
	nativeSessionID := *m.nativeSessionID
	follower := m.follower
	m.follower = nil
	type scan struct {
		follower *KimiWireFollower
		snapshot KimiTaskSnapshot
		err      error
	}
	scanned, joinErr := core.SpawnBlocking(func() scan {
		sessionDir, err := ResolveSessionDir(home, nativeSessionID)
		if err != nil {
			return scan{err: err}
		}
		wirePath := filepath.Join(sessionDir, "agents/main/wire.jsonl")
		if follower == nil || follower.WirePath() != wirePath {
			if follower, err = KimiWireFollowerOpen(wirePath); err != nil {
				return scan{err: err}
			}
		}
		refreshed, err := follower.Refresh()
		if err != nil {
			return scan{err: err}
		}
		var snapshot KimiTaskSnapshot
		switch refreshed := refreshed.(type) {
		case KimiWireRefreshUpdated:
			snapshot = refreshed.Value
		default:
			if snapshot, err = follower.Rescan(); err != nil {
				return scan{err: err}
			}
		}
		return scan{follower: follower, snapshot: snapshot}
	}).Join(ctx)
	switch {
	case joinErr != nil:
		return m.recordFailure(relay, fmt.Errorf("Kimi background task scan stopped: %s", joinErr))
	case scanned.err != nil:
		return m.recordFailure(relay, scanned.err)
	}
	m.follower = scanned.follower
	m.consecutiveFailures = 0
	m.retryAt = time.Now().Add(kimiTaskPollInterval)
	m.lastWarning = nil
	agents := make(map[string]KimiNativeAgentEntry, len(scanned.snapshot.NativeAgents))
	for id, agent := range scanned.snapshot.NativeAgents {
		agents[id] = KimiNativeAgentEntry{Task: agent.Task, State: agent.State}
	}
	guard := relay.Lock()
	err := guard.KimiNativeAgentsChanged(agents)
	guard.Unlock()
	if err != nil {
		return err
	}
	guard = relay.Lock()
	defer guard.Unlock()
	return guard.KimiBackgroundTasksChanged(scanned.snapshot.Tasks, scanned.snapshot.ProviderToolIDs, scanned.snapshot.ObservedTaskIDs)
}

// recordFailure backs off and reports, once per distinct message, that
// background task tracking is unavailable.
func (m *KimiTaskMonitor) recordFailure(relay *RelayCell, err error) error {
	m.follower = nil
	if m.consecutiveFailures < math.MaxUint32 {
		m.consecutiveFailures++
	}
	exponent := min(max(m.consecutiveFailures, 1)-1, 4)
	delay := min(kimiTaskPollInterval*time.Duration(uint32(1)<<exponent), kimiTaskMaxRetryDelay)
	m.retryAt = time.Now().Add(delay)
	message := "Kimi background task tracking is unavailable; worker replacement is blocked: " + err.Error()
	guard := relay.Lock()
	defer guard.Unlock()
	if err := guard.KimiBackgroundTasksUnavailable(); err != nil {
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

// awaitTurnEnd waits, briefly, for the journal record that closes the turn
// of a prompt started at started.
//
// Kimi flushes its records asynchronously, so they can trail the ACP prompt
// response. Normally the wait lasts only while Kimi is journalling the turn,
// so a build that journals no turns costs nothing. A prompt that ended with
// no session updates also waits for a turn not journalled yet: an early
// failure, such as a refused login, is exactly when the whole turn can still
// be in flight (#1132).
func awaitTurnEnd(ctx context.Context, monitor *KimiTaskMonitor, relay *RelayCell, started int64, evenIfUnjournalled bool) error {
	for range kimiTurnEndPolls {
		settled, known := monitor.turnSettledSince(started)
		if (known && settled) || (!known && !evenIfUnjournalled) {
			break
		}
		timer := time.NewTimer(kimiTurnEndPollInterval)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
		if err := monitor.refresh(ctx, relay, true); err != nil {
			return err
		}
	}
	return nil
}

// prepareKimiRuntimeEvent brings a runtime event up to date with Kimi's
// journal before it is recorded: the session to follow, the turn's native
// usage and its failure. monitor is nil when the harness is not Kimi; event
// may be replaced.
func prepareKimiRuntimeEvent(ctx context.Context, monitor *KimiTaskMonitor, relay *RelayCell, event *core.RuntimeEvent) error {
	if monitor == nil {
		return nil
	}
	activePrompt := func() *core.ActiveRelayPrompt {
		guard := relay.Lock()
		defer guard.Unlock()
		return guard.OperationalState().ActivePrompt
	}
	switch current := (*event).(type) {
	case core.RuntimeEventSessionStarted:
		return monitor.attach(ctx, current.NativeSessionID, relay)
	case core.RuntimeEventSessionConfigured:
		return monitor.refresh(ctx, relay, true)
	case core.RuntimeEventWarning:
		// The ACP runtime says this before it finishes a prompt that ended
		// with no session updates. When Kimi journalled a failure for that
		// turn, the failure is what happened, and it is what the person and
		// the credential sync need to read (#1132).
		if !strings.Contains(current.Message, core.PromptEmptyResponseMarker) {
			return nil
		}
		prompt := activePrompt()
		if prompt == nil {
			return nil
		}
		started := prompt.StartedAtMs
		if err := monitor.refresh(ctx, relay, true); err != nil {
			return err
		}
		if err := awaitTurnEnd(ctx, monitor, relay, started, true); err != nil {
			return err
		}
		if monitor.follower != nil {
			if native := monitor.follower.TurnDiagnosticSince(started); native != nil {
				current.Message = "prompt failed: " + native.Message
				*event = current
			}
		}
		return nil
	case core.RuntimeEventPromptFinished:
		var started *int64
		if prompt := activePrompt(); prompt != nil && prompt.CommandID == current.RequestID {
			started = &prompt.StartedAtMs
		}
		if err := monitor.refresh(ctx, relay, true); err != nil {
			return err
		}
		// The last step's usage can still be in flight when the prompt
		// response arrives; the turn end follows every record of the turn.
		if started != nil {
			unanswered := current.StopReason == core.PromptUnansweredStopReason
			if err := awaitTurnEnd(ctx, monitor, relay, *started, unanswered); err != nil {
				return err
			}
		}
		// The ACP prompt response carries no usage, so the native records are
		// the only report of what the turn spent (#1064).
		if current.Usage == nil && started != nil && monitor.follower != nil {
			if native := monitor.follower.TurnUsageSince(*started); native != nil {
				current.Usage = native
			}
		}
		if started != nil && monitor.follower != nil {
			if native := monitor.follower.TurnDiagnosticSince(*started); native != nil {
				if native.IsUsageLimit() {
					current.StopReason = core.QuotaStopReason
				} else {
					current.StopReason = "error"
				}
				current.Diagnostic = native
			}
		}
		*event = current
		return nil
	case core.RuntimeEventHarnessRestarting, core.RuntimeEventStopped:
		monitor.detach()
		return nil
	}
	return nil
}
