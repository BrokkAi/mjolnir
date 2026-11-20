// Port of mj-worker/src/worker_runtime/unix/kimi.rs.
//
// Kimi's background tasks, native agents, turn usage and turn failures are
// read from its wire journal, which trails the ACP prompt response.

//go:build unix

package worker

import (
	"context"
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
	panic("unported: mj-worker/src/worker_runtime/unix/kimi.rs KimiTaskMonitor::turn_settled_since")
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
	panic("unported: mj-worker/src/worker_runtime/unix/kimi.rs KimiTaskMonitor::refresh")
}

// recordFailure backs off and reports, once per distinct message, that
// background task tracking is unavailable.
func (m *KimiTaskMonitor) recordFailure(relay *RelayCell, err error) error {
	panic("unported: mj-worker/src/worker_runtime/unix/kimi.rs KimiTaskMonitor::record_failure")
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
	panic("unported: mj-worker/src/worker_runtime/unix/kimi.rs prepare_kimi_runtime_event")
}
