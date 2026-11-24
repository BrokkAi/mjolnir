// Port of mj-cli/src/pollers.rs.
//
// Terminal-specific adapters for controller background feeds. Rust's
// re-export of `mj_daemon::pollers::*` is the go/internal/daemon package,
// used under its own name.

package cli

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// dashboardLifecycleUpdate is one lifecycle operation's result. Completion
// identity belongs to the UI operation that started the work; the daemon's
// lifecycle projection remains the authority for session state. Exactly one
// of success and failure is set. Declared for the dashboard (slice 2b).
type dashboardLifecycleUpdate struct {
	sessionID string
	success   daemon.LifecycleSuccess
	failure   *dashboardLifecycleFailure
	operation *atomic.Bool
}

type dashboardLifecycleFailureKind int

const (
	dashboardLifecycleFailed dashboardLifecycleFailureKind = iota
	dashboardLifecycleAdmissionUnconfirmed
	dashboardLifecycleObservationUnavailable
)

// dashboardLifecycleFailure is why a dashboard lifecycle operation did not
// report success; message is the error of a dashboardLifecycleFailed.
type dashboardLifecycleFailure struct {
	kind    dashboardLifecycleFailureKind
	message string
}

// dashboardLifecycleFailureFrom is Rust's `From<anyhow::Error>`: an
// unconfirmed admission or an unavailable observation is told apart from a
// failure, and logged.
func dashboardLifecycleFailureFrom(err error) dashboardLifecycleFailure {
	var unconfirmed client.LifecycleAdmissionUnconfirmed
	var unavailable client.LifecycleObservationUnavailable
	switch {
	case errors.As(err, &unconfirmed):
		slog.Warn("daemon lifecycle acceptance is unconfirmed", "error", err.Error())
		return dashboardLifecycleFailure{kind: dashboardLifecycleAdmissionUnconfirmed}
	case errors.As(err, &unavailable):
		slog.Warn("daemon lifecycle observation is unavailable", "error", err.Error())
		return dashboardLifecycleFailure{kind: dashboardLifecycleObservationUnavailable}
	}
	return dashboardLifecycleFailure{kind: dashboardLifecycleFailed, message: err.Error()}
}

func (f dashboardLifecycleFailure) Error() string {
	switch f.kind {
	case dashboardLifecycleAdmissionUnconfirmed:
		return "daemon acceptance was not confirmed; check session status before retrying"
	case dashboardLifecycleObservationUnavailable:
		return "operation observation is unavailable; check session status"
	}
	return f.message
}

const workerDiagnosisTimeout = 15 * time.Second

// applyWorkerPollUpdate applies one worker poll's reachability to the
// dashboard. The daemon owns durable missing-target findings and publishes
// their notice. Returns the poll's projection, moved out of the update
// rather than copied, when it carried one; nil otherwise.
func applyWorkerPollUpdate(dashboard *tui.DashboardState, update daemon.WorkerPollUpdate) *materializedProjection {
	switch failure := update.View.Error; {
	case failure != nil && failure.Kind == client.ViewErrorKindUnreachable:
		dashboard.MarkTranscriptUnavailable(update.SessionID)
		slog.Warn(fmt.Sprintf("relay unreachable: %s", failure.Message), "session_id", update.SessionID)
		dashboard.ReportSessionUnreachable(update.SessionID)
	case failure != nil && failure.Kind == client.ViewErrorKindTargetMissing:
		dashboard.MarkTranscriptUnavailable(update.SessionID)
		dashboard.ClearSessionUnreachable(update.SessionID)
	case failure != nil && failure.Kind == client.ViewErrorKindProjectionIntegrity:
		// Deterministic failure: no worker diagnostics. Like an unreachable
		// relay, it is a live poll fact and is never persisted as a session
		// last_error.
		dashboard.MarkTranscriptUnavailable(update.SessionID)
		dashboard.ClearSessionUnreachable(update.SessionID)
		dashboard.SetNotice(fmt.Sprintf(
			"Session %s: transcript projection failed: %s",
			update.SessionID[:min(len(update.SessionID), 8)],
			failure.Message,
		))
	case failure == nil:
		dashboard.ReportSessionReachable(update.SessionID)
	}
	if update.View.Snapshot == nil {
		return nil
	}
	projection := materializedProjectionFromSnapshot(*update.View.Snapshot)
	return &projection
}

// spawnWorkerDiagnosis probes an unreachable session's worker off the loop
// and reports what it found for episodeID. The probe holds quit, which
// cancels it.
func spawnWorkerDiagnosis(
	ctrl *controller.Controller,
	sessionID string,
	episodeID uint64,
	updates *core.Unbounded[dashboardIoUpdate],
	tracker client.CriticalOperationTracker,
) {
	cancelled := new(atomic.Bool)
	guard := tracker.BeginCancellable(fmt.Sprintf("diagnosing session %s", core.ShortID(sessionID)), cancelled)
	diagnostic := controllerSnapshot(ctrl)
	core.Spawn(func() struct{} {
		defer guard.Release()
		joined := core.SpawnBlocking(func() *string {
			executor := core.NewCancellableProcessExecutor(cancelled).WithDeadline(workerDiagnosisTimeout)
			return diagnostic.DiagnoseWorkerControlled(sessionID, executor)
		})
		diagnosis, err := joined.Join(context.Background())
		if err != nil {
			err = fmt.Errorf("worker diagnosis task failed: %w", err)
		}
		report[dashboardIoUpdate]("diagnosing a worker", updates, dashboardIoUpdateWorkerDiagnosis{
			sessionID: sessionID,
			episodeID: episodeID,
			diagnosis: diagnosis,
			err:       err,
		})
		return struct{}{}
	})
}
