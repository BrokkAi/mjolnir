// Port of mj-review/src/driver.rs.
//
// The turn-review state machine. Its requests, phases, seed and snapshot are
// core's (Rust re-exports mj_core::review::driver::*).

package review

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// TurnReviewDriver is one turn review, from the capture that starts it to
// the action that ends it.
type TurnReviewDriver struct {
	reviewID string
	seed     core.TurnReviewSeed
	phase    core.TurnReviewPhase
	// lastVerdict is the verdict that led to the current resolution,
	// retained for the notice emitted after the resolved phase replaces it.
	// Nil before any verdict.
	lastVerdict core.ReviewVerdict
	deltas      []core.RepoDelta
	// roles is the frozen launch order and the sole owner of per-role
	// terminal outcomes.
	roles       []core.ReviewerOutcomeSlot
	sequence    uint64
	briefWriter *core.RoleState
	status      string
	// replayRequests is the exact requests the current stage emitted.
	replayRequests []core.ReviewRequest
}

// TurnReviewDriverStart opens a review of count reviewers (at least one;
// Rust's NonZeroUsize) and asks for the capture that defines its target.
func TurnReviewDriverStart(seed core.TurnReviewSeed, reviewID string, count int) (*TurnReviewDriver, []core.ReviewRequest) {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::start")
}

// Snapshot is the driver's durable state.
func (d *TurnReviewDriver) Snapshot() core.ReviewDriverSnapshot {
	var briefWriter *core.RoleState
	if d.briefWriter != nil {
		state := *d.briefWriter
		briefWriter = &state
	}
	return core.ReviewDriverSnapshot{
		ReviewID:       d.reviewID,
		Seed:           d.seed,
		Phase:          d.phase,
		LastVerdict:    d.lastVerdict,
		Deltas:         append([]core.RepoDelta(nil), d.deltas...),
		Roles:          append([]core.ReviewerOutcomeSlot(nil), d.roles...),
		Sequence:       d.sequence,
		BriefWriter:    briefWriter,
		ReplayRequests: append([]core.ReviewRequest(nil), d.replayRequests...),
	}
}

// RecoveryRequests replays the exact work this stage emitted before a daemon
// interruption. A settled role is never restarted while its peers are still
// running.
func (d *TurnReviewDriver) RecoveryRequests() []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::recovery_requests")
}

// emit records requests as the current stage's replay and returns them.
func (d *TurnReviewDriver) emit(requests []core.ReviewRequest) []core.ReviewRequest {
	d.replayRequests = append([]core.ReviewRequest(nil), requests...)
	return requests
}

// TurnReviewDriverRestore rebuilds a driver from its snapshot, refusing one
// whose facts disagree (no review id, duplicate or empty role or command
// ids, no slots, a phase its slots contradict).
func TurnReviewDriverRestore(snapshot core.ReviewDriverSnapshot) (*TurnReviewDriver, error) {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::restore")
}

// Phase is where the review has got to.
func (d *TurnReviewDriver) Phase() core.TurnReviewPhase { return d.phase }

// Status is the review's one-line status.
func (d *TurnReviewDriver) Status() string { return d.status }

// AwaitedCommands is the driver-local keys of unsettled role work, each a
// (role, command id) pair. The host uses these to decide which roles still
// await an outcome, and each registration's startup prompt ID to match a
// child's relay handback.
func (d *TurnReviewDriver) AwaitedCommands() [][2]string {
	if _, ok := d.phase.(core.TurnReviewPhaseWritingBrief); ok {
		return [][2]string{{core.BriefWriterRole, core.BriefWriterCommandID(d.reviewID)}}
	}
	if _, ok := d.phase.(core.TurnReviewPhaseRunning); !ok {
		return [][2]string{}
	}
	awaited := [][2]string{}
	for _, slot := range d.roles {
		if slot.Outcome == nil {
			awaited = append(awaited, [2]string{slot.Role, slot.CommandID})
		}
	}
	return awaited
}

// Finished reports whether the review has ended, which is when its pane
// closes and the composer comes back.
func (d *TurnReviewDriver) Finished() bool {
	_, ok := d.phase.(core.TurnReviewPhaseResolved)
	return ok
}

// Verdict is the verdict on screen while the user has actions to take; nil
// in any other phase.
func (d *TurnReviewDriver) Verdict() core.ReviewVerdict {
	if phase, ok := d.phase.(core.TurnReviewPhaseVerdict); ok {
		return phase.Verdict
	}
	return nil
}

// LastVerdict is the last verdict reached, including after the review has
// resolved; nil before any.
func (d *TurnReviewDriver) LastVerdict() core.ReviewVerdict { return d.lastVerdict }

// Deltas is the captured change.
func (d *TurnReviewDriver) Deltas() []core.RepoDelta { return d.deltas }

// Roles is the rows the review pane's strip shows. The brief writer's row
// comes first, so a user can see it working and open its session, and after
// it ends see what became of its brief.
func (d *TurnReviewDriver) Roles() []core.RoleStatus {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::roles")
}

// roleStatuses is each reviewer slot's row, in launch order.
func (d *TurnReviewDriver) roleStatuses() []core.RoleStatus {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::role_statuses")
}

// phaseStatus is the status line the current phase shows.
func (d *TurnReviewDriver) phaseStatus() string {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::phase_status")
}

// job is the prompt input for the captured change: every repository with a
// patch and a baseline.
func (d *TurnReviewDriver) job() *ReviewJob {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::job")
}

// DeltaCaptured records the capture. An empty capture ends the review before
// any agent runs; anything else starts the first agent.
func (d *TurnReviewDriver) DeltaCaptured(deltas []core.RepoDelta) []core.ReviewRequest {
	return d.DeltaCapturedWithWriter(deltas, false)
}

// DeltaCapturedWithWriter records the capture. With a brief writer, it runs
// first and the reviewers wait for its brief; without one, the reviewers
// start now.
func (d *TurnReviewDriver) DeltaCapturedWithWriter(deltas []core.RepoDelta, briefWriter bool) []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::delta_captured_with_writer")
}

// startReviewers enters Running and returns the start request of every
// reviewer.
func (d *TurnReviewDriver) startReviewers() []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::start_reviewers")
}

// BriefWritten records the brief writer's handback. Its text goes to every
// reviewer's prompt and the reviewers start. An empty brief counts as a
// failed writer.
func (d *TurnReviewDriver) BriefWritten(text string) []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::brief_written")
}

// BriefFailed records that the brief writer could not start, ended without a
// handback, or failed. The review goes on without a brief: reviewers start
// as they would have, and the requester is told why there is none.
func (d *TurnReviewDriver) BriefFailed(reason string) []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::brief_failed")
}

// EmptyPushDeltaCaptured ends a push whose tree matches its reviewed base,
// which needs no reviewer. The controller has already refused a capture with
// base == HEAD.
func (d *TurnReviewDriver) EmptyPushDeltaCaptured(deltas []core.RepoDelta) []core.ReviewRequest {
	if _, ok := d.phase.(core.TurnReviewPhaseCapturingDelta); !ok {
		return []core.ReviewRequest{}
	}
	d.deltas = deltas
	return d.reachVerdict(core.ReviewVerdictClean{})
}

// PushReviewWarnings is each repository's reason its push was recorded
// without review.
func (d *TurnReviewDriver) PushReviewWarnings() []string {
	warnings := []string{}
	for _, delta := range d.deltas {
		if delta.PushReviewWarning != nil {
			warnings = append(warnings, *delta.PushReviewWarning)
		}
	}
	return warnings
}

// RoleTurnCompleted records that the role prompted under commandID finished
// its turn with answer.
func (d *TurnReviewDriver) RoleTurnCompleted(commandID, childSessionID, answer string) []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::role_turn_completed")
}

// RoleFailed records an explicit lifecycle or harness failure of one role.
// childSessionID is nil when the role has no session.
func (d *TurnReviewDriver) RoleFailed(role string, childSessionID *string, reason string) []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::role_failed")
}

// roleLabel names the reviewer at index, with its session when known.
func (d *TurnReviewDriver) roleLabel(index int, childSessionID *string) string {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::role_label")
}

// roleSettled reaches the group's verdict once every slot has an outcome.
func (d *TurnReviewDriver) roleSettled() []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::role_settled")
}

// RequestFailed records that a group-wide request, such as capture, failed
// before role results can decide the verdict. Role-specific failures use
// RoleFailed instead.
func (d *TurnReviewDriver) RequestFailed(message string) []core.ReviewRequest {
	if d.Finished() {
		return []core.ReviewRequest{}
	}
	return d.reachVerdict(core.ReviewVerdictFailed{Reason: message})
}

// reachVerdict settles every open slot with verdict, shows it, and reaps
// every role.
func (d *TurnReviewDriver) reachVerdict(verdict core.ReviewVerdict) []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::reach_verdict")
}

// pauseEveryRole closes every reviewer, once there is a target.
func (d *TurnReviewDriver) pauseEveryRole() []core.ReviewRequest {
	requests := []core.ReviewRequest{}
	if len(d.deltas) == 0 {
		return requests
	}
	for _, slot := range d.roles {
		requests = append(requests, core.ReviewRequestCloseReviewer{Role: slot.Role})
	}
	return requests
}

// Complete stops the review. The baseline stays where it was, so the next
// review covers this turn as well as the next one.
func (d *TurnReviewDriver) Complete() {
	d.phase = core.TurnReviewPhaseResolved{Resolution: core.ResolutionCompleted}
}

// Cancel ends the review at the user's request, closing every role.
func (d *TurnReviewDriver) Cancel() []core.ReviewRequest {
	panic("unported: mj-review/src/driver.rs TurnReviewDriver::cancel")
}
