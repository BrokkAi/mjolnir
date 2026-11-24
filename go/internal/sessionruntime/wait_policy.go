// Port of mj-session-runtime/src/wait_policy.rs (only the wire types the
// `/api/v1` contract carries so far).

// Package sessionruntime is the daemon's per-session runtime
// (mj-session-runtime). Only the wait types the API answers with are here
// so far, declared from the Rust source at 3dde0c11b for the `mj` CLI; the
// rest of the crate arrives with the Go daemon (milestone 3), in the same
// files. mj-server re-exports these (`pub use
// mj_session_runtime::wait_policy::*`); Go imports this package directly.
package sessionruntime

import "github.com/BrokkAi/mjolnir/go/internal/core"

// ApiBackgroundWork is the observed provider-owned background work; absent
// when no live snapshot is available.
type ApiBackgroundWork struct {
	Known *bool                    `json:"known"`
	Tasks []core.BackgroundCommand `json:"tasks"`
}

// ApiBackgroundWorkFrom is Rust's `From<&RelayOperationalState>`.
func ApiBackgroundWorkFrom(state *core.RelayOperationalState) ApiBackgroundWork {
	panic("unported: mj-session-runtime/src/wait_policy.rs ApiBackgroundWork::from")
}

// WaitRequest is the body of `POST /sessions/{id}/wait`. Rust:
// deny_unknown_fields.
type WaitRequest struct {
	// ReturnOnInput returns when the harness presents a structured input
	// request.
	ReturnOnInput bool `json:"return_on_input" serde:"default"`
	// TurnID waits for this specific prompt. Absent means "wait until the
	// session is idle with nothing queued", which is what a caller that lost
	// its turn id wants.
	TurnID      *uint64 `json:"turn_id" serde:"default"`
	TimeoutSecs *uint64 `json:"timeout_secs" serde:"default"`
}

// WaitOutcome is how a wait ended (snake_case).
type WaitOutcome string

const (
	// WaitOutcomeInputRequired: a structured elicitation needs an answer;
	// only returned by opt-in waits.
	WaitOutcomeInputRequired WaitOutcome = "input_required"
	// WaitOutcomeFinished: the turn completed normally.
	WaitOutcomeFinished WaitOutcome = "finished"
	// WaitOutcomeError: the turn failed, was rejected, or the session
	// reported an error.
	WaitOutcomeError WaitOutcome = "error"
	// WaitOutcomeCancelled: the turn was cancelled or interrupted.
	WaitOutcomeCancelled WaitOutcome = "cancelled"
	// WaitOutcomeQuotaLimit: the model was at capacity and no retry is armed.
	WaitOutcomeQuotaLimit WaitOutcome = "quota_limit"
	// WaitOutcomeTimeout: the wait's deadline passed with the turn still
	// running.
	WaitOutcomeTimeout WaitOutcome = "timeout"
	// WaitOutcomeStopped: the session stopped or is stopping, so no turn can
	// finish on it.
	WaitOutcomeStopped WaitOutcome = "stopped"
)

// ViewerLifecycleCategory is the small set of states a phone reasons about,
// alongside the precise state (kebab-case).
//
// A phone groups and filters by this; it shows the precise `state` string as
// the word it prints. Collapsing here rather than in the browser keeps one
// definition of "live" in the controller.
type ViewerLifecycleCategory string

const (
	ViewerLifecycleCategoryLive       ViewerLifecycleCategory = "live"
	ViewerLifecycleCategoryStarting   ViewerLifecycleCategory = "starting"
	ViewerLifecycleCategorySuspending ViewerLifecycleCategory = "suspending"
	ViewerLifecycleCategorySuspended  ViewerLifecycleCategory = "suspended"
	ViewerLifecycleCategoryFailed     ViewerLifecycleCategory = "failed"
)

// ViewerLifecycleCategoryOf is the category of a precise session state.
func ViewerLifecycleCategoryOf(state core.SessionState) ViewerLifecycleCategory {
	switch state {
	case core.SessionStateProvisioning:
		return ViewerLifecycleCategoryStarting
	// A parked sub-agent stays on the dashboard with its parent.
	case core.SessionStateRunning, core.SessionStateDisconnected, core.SessionStateCheckpointing, core.SessionStateParked:
		return ViewerLifecycleCategoryLive
	case core.SessionStateClosing, core.SessionStateDestroying, core.SessionStateStartupCleanup:
		return ViewerLifecycleCategorySuspending
	case core.SessionStateStopped:
		return ViewerLifecycleCategorySuspended
	case core.SessionStateLost, core.SessionStateError, core.SessionStateDestroyedWithDataLoss:
		return ViewerLifecycleCategoryFailed
	}
	panic("unknown session state " + string(state))
}

// IsDashboardVisible is whether this session belongs on the dashboard.
// Stopped and failed sessions belong to the resume flow instead, which is
// where a person can do something about them.
func (c ViewerLifecycleCategory) IsDashboardVisible() bool {
	switch c {
	case ViewerLifecycleCategoryLive, ViewerLifecycleCategoryStarting, ViewerLifecycleCategorySuspending:
		return true
	}
	return false
}
