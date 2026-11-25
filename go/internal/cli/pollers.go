// Port of mj-cli/src/pollers.rs (the part that does not need mj-daemon,
// mj-controller or the dashboard).
//
// Terminal-specific adapters for controller background feeds.
//
// Not declared yet, because what they take is in crates or modules the Go
// build does not have: DashboardLifecycleUpdate (mj-daemon's pollers
// LifecycleSuccess), apply_worker_poll_update (mj-daemon's
// WorkerPollUpdate and the dashboard's MaterializedProjection), and
// spawn_worker_diagnosis (mj-controller's Controller and the dashboard's
// DashboardIoUpdate and CriticalOperationTracker). Rust's re-export of
// `mj_daemon::pollers::*` waits for the same port.

package cli

import (
	"errors"
	"log/slog"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
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
