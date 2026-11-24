// Port of mj-cli/src/pollers.rs (the part that does not need mj-daemon,
// mj-controller or the dashboard).
//
// Terminal-specific adapters for controller background feeds.
//
// Not declared yet, because what it takes is in crates the Go build does
// not have: spawn_worker_diagnosis (mj-controller's Controller and its
// worker diagnosis). Rust's re-export of `mj_daemon::pollers::*` waits for
// the same port; mj-daemon's WorkerPollUpdate is sessionruntime's
// SessionManagerUpdate, which it aliases.

package cli

import (
	"errors"
	"fmt"
	"log/slog"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// applyWorkerPollUpdate applies one worker poll's reachability to the
// dashboard. The daemon owns durable missing-target findings and publishes
// their notice. Returns the poll's projection, when it carried one.
func applyWorkerPollUpdate(dashboard *tui.DashboardState, update sessionruntime.SessionManagerUpdate) *materializedProjection {
	if viewError := update.View.Error; viewError == nil {
		dashboard.ReportSessionReachable(update.SessionID)
	} else {
		switch viewError.Kind {
		case client.ViewErrorKindUnreachable:
			dashboard.MarkTranscriptUnavailable(update.SessionID)
			slog.Warn(fmt.Sprintf("relay unreachable: %s", viewError.Detail()), "session_id", update.SessionID)
			dashboard.ReportSessionUnreachable(update.SessionID)
		case client.ViewErrorKindTargetMissing:
			dashboard.MarkTranscriptUnavailable(update.SessionID)
			dashboard.ClearSessionUnreachable(update.SessionID)
		case client.ViewErrorKindProjectionIntegrity:
			// Deterministic failure: no worker diagnostics. Like an
			// unreachable relay, it is a live poll fact and is never
			// persisted as a session last_error.
			dashboard.MarkTranscriptUnavailable(update.SessionID)
			dashboard.ClearSessionUnreachable(update.SessionID)
			dashboard.SetNotice(fmt.Sprintf(
				"Session %s: transcript projection failed: %s",
				update.SessionID[:min(len(update.SessionID), 8)],
				viewError.Detail(),
			))
		}
	}
	if update.View.Snapshot == nil {
		return nil
	}
	projection := materializedProjectionFromSnapshot(*update.View.Snapshot)
	return &projection
}

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
