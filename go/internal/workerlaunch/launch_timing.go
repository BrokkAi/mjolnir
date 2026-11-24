// Port of mj-worker-launch/src/launch_timing.rs.

// Wall-clock launch phases, including work outside timed target commands.

package workerlaunch

import (
	"log/slog"
	"time"
)

// LaunchPhaseTimer logs the start of a launch phase when created and its end,
// with the elapsed time, when End runs. End is Rust's Drop: defer it where
// the Rust guard is bound (`let _timing = LaunchPhaseTimer::new(...)`), or
// call it where the Rust code drops the guard early.
type LaunchPhaseTimer struct {
	sessionID string
	phase     string
	started   time.Time
}

func NewLaunchPhaseTimer(sessionID string, phase string) *LaunchPhaseTimer {
	slog.Info("session launch phase started", "session_id", sessionID, "phase", phase)
	return &LaunchPhaseTimer{sessionID: sessionID, phase: phase, started: time.Now()}
}

// SetSessionID names the session the phase belongs to once it is known.
func (t *LaunchPhaseTimer) SetSessionID(sessionID string) {
	t.sessionID = sessionID
}

// End logs the end of the phase (Rust's Drop).
func (t *LaunchPhaseTimer) End() {
	slog.Info(
		"session launch phase ended",
		"session_id", t.sessionID,
		"phase", t.phase,
		"elapsed_ms", time.Since(t.started).Milliseconds(),
	)
}
