// Port of mj-review/src/driver.rs.
//
// The turn-review state machine. Its requests, phases, seed and snapshot are
// core's (Rust re-exports mj_core::review::driver::*).

package review

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"

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
	if count < 1 {
		panic(fmt.Sprintf("a turn review needs at least one reviewer, not %d", count))
	}
	roles := make([]core.ReviewerOutcomeSlot, 0, count)
	for index := 1; index <= count; index++ {
		roles = append(roles, core.ReviewerOutcomeSlot{
			Role:      fmt.Sprintf("reviewer-%d", index),
			CommandID: fmt.Sprintf("%s%s-reviewer-%d", core.CommandIDPrefix, reviewID, index),
		})
	}
	requests := []core.ReviewRequest{core.ReviewRequestCaptureDelta{}}
	driver := &TurnReviewDriver{
		reviewID:       reviewID,
		seed:           seed,
		phase:          core.TurnReviewPhaseCapturingDelta{},
		deltas:         []core.RepoDelta{},
		roles:          roles,
		status:         "capturing committed work…",
		replayRequests: append([]core.ReviewRequest(nil), requests...),
	}
	return driver, requests
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
	keep := func(include func(core.ReviewRequest) bool) []core.ReviewRequest {
		requests := []core.ReviewRequest{}
		for _, request := range d.replayRequests {
			if include(request) {
				requests = append(requests, request)
			}
		}
		return requests
	}
	switch d.phase.(type) {
	case core.TurnReviewPhaseCapturingDelta:
		return keep(func(request core.ReviewRequest) bool {
			_, ok := request.(core.ReviewRequestCaptureDelta)
			return ok
		})
	case core.TurnReviewPhaseWritingBrief:
		return keep(func(request core.ReviewRequest) bool {
			_, ok := request.(core.ReviewRequestStartBriefWriter)
			return ok
		})
	case core.TurnReviewPhaseRunning:
		return keep(func(request core.ReviewRequest) bool {
			switch request := request.(type) {
			case core.ReviewRequestStartReviewer:
				return slices.ContainsFunc(d.roles, func(slot core.ReviewerOutcomeSlot) bool {
					return slot.Role == request.Role && slot.Outcome == nil
				})
			case core.ReviewRequestCloseBriefWriter, core.ReviewRequestNoteBriefSkipped:
				return true
			}
			return false
		})
	case core.TurnReviewPhaseVerdict, core.TurnReviewPhaseResolved:
		return append([]core.ReviewRequest{}, d.replayRequests...)
	}
	// LaunchingReviewer.
	return []core.ReviewRequest{}
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
	if snapshot.ReviewID == "" {
		return nil, errors.New("review snapshot has no review id")
	}
	names := map[string]struct{}{}
	commands := map[string]struct{}{}
	for _, slot := range snapshot.Roles {
		_, duplicateName := names[slot.Role]
		_, duplicateCommand := commands[slot.CommandID]
		if slot.Role == "" || slot.CommandID == "" || duplicateName || duplicateCommand {
			return nil, errors.New("review snapshot has duplicate or empty role/command ids")
		}
		names[slot.Role] = struct{}{}
		commands[slot.CommandID] = struct{}{}
	}
	if len(snapshot.Roles) == 0 {
		return nil, errors.New("review snapshot has no reviewer slots")
	}
	anySettled := slices.ContainsFunc(snapshot.Roles, func(slot core.ReviewerOutcomeSlot) bool { return slot.Outcome != nil })
	anyPending := slices.ContainsFunc(snapshot.Roles, func(slot core.ReviewerOutcomeSlot) bool { return slot.Outcome == nil })
	switch phase := snapshot.Phase.(type) {
	case core.TurnReviewPhaseRunning:
		if !anyPending {
			return nil, errors.New("running review snapshot has no pending reviewer")
		}
	case core.TurnReviewPhaseCapturingDelta:
		if anySettled || len(snapshot.Deltas) > 0 {
			return nil, errors.New("capturing review snapshot already has outcomes or a target")
		}
	case core.TurnReviewPhaseWritingBrief:
		if anySettled || len(snapshot.Deltas) == 0 {
			return nil, errors.New("brief-writing review snapshot has outcomes or no target")
		}
	case core.TurnReviewPhaseVerdict:
		if snapshot.LastVerdict == nil || !sameVerdict(snapshot.LastVerdict, phase.Verdict) || anyPending {
			return nil, errors.New("review snapshot verdict disagrees with its completed slots")
		}
	}
	driver := &TurnReviewDriver{
		reviewID:       snapshot.ReviewID,
		seed:           snapshot.Seed,
		phase:          snapshot.Phase,
		lastVerdict:    snapshot.LastVerdict,
		deltas:         snapshot.Deltas,
		roles:          snapshot.Roles,
		sequence:       snapshot.Sequence,
		briefWriter:    snapshot.BriefWriter,
		replayRequests: snapshot.ReplayRequests,
	}
	if _, ok := driver.phase.(core.TurnReviewPhaseRunning); ok {
		driver.phase = core.TurnReviewPhaseRunning{Roles: driver.roleStatuses()}
	}
	driver.status = driver.phaseStatus()
	return driver, nil
}

// sameVerdict is Rust's ReviewVerdict equality. The verdicts' encodings
// hold every member, and compare an absent list with an empty one as Rust's
// Vec equality does.
func sameVerdict(a, b core.ReviewVerdict) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	left, leftErr := json.Marshal(a)
	right, rightErr := json.Marshal(b)
	return leftErr == nil && rightErr == nil && bytes.Equal(left, right)
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
	reviewers := []core.RoleStatus{}
	switch d.phase.(type) {
	case core.TurnReviewPhaseRunning, core.TurnReviewPhaseVerdict:
		reviewers = d.roleStatuses()
	}
	if d.briefWriter == nil {
		return reviewers
	}
	switch d.phase.(type) {
	case core.TurnReviewPhaseWritingBrief, core.TurnReviewPhaseRunning, core.TurnReviewPhaseVerdict:
		rows := []core.RoleStatus{{
			Role:  core.BriefWriterRole,
			Label: "Brief writer",
			State: *d.briefWriter,
		}}
		return append(rows, reviewers...)
	}
	return reviewers
}

// roleStatuses is each reviewer slot's row, in launch order.
func (d *TurnReviewDriver) roleStatuses() []core.RoleStatus {
	_, capturing := d.phase.(core.TurnReviewPhaseCapturingDelta)
	statuses := make([]core.RoleStatus, 0, len(d.roles))
	for index, slot := range d.roles {
		var state core.RoleState
		switch slot.Outcome.(type) {
		case core.ReviewVerdictClean:
			state = core.RoleStateClean
		case core.ReviewVerdictFindings:
			state = core.RoleStateFindings
		case core.ReviewVerdictFailed:
			state = core.RoleStateFailed
		default:
			if capturing {
				state = core.RoleStatePending
			} else {
				state = core.RoleStateRunning
			}
		}
		statuses = append(statuses, core.RoleStatus{
			Role:  slot.Role,
			Label: fmt.Sprintf("Reviewer %d", index+1),
			State: state,
		})
	}
	return statuses
}

// phaseStatus is the status line the current phase shows.
func (d *TurnReviewDriver) phaseStatus() string {
	switch phase := d.phase.(type) {
	case core.TurnReviewPhaseCapturingDelta:
		return "capturing committed work…"
	case core.TurnReviewPhaseLaunchingReviewer:
		return "starting reviewers…"
	case core.TurnReviewPhaseWritingBrief:
		return "writing a relation brief for the reviewers…"
	case core.TurnReviewPhaseRunning:
		return "reviewers are reading the change…"
	case core.TurnReviewPhaseVerdict:
		return verdictStatus(phase.Verdict)
	case core.TurnReviewPhaseResolved:
		if phase.Resolution == core.ResolutionCancelled {
			return "review cancelled"
		}
		return "review complete"
	}
	panic(fmt.Sprintf("unknown turn review phase %T", d.phase))
}

// verdictStatus is the status line a verdict shows.
func verdictStatus(verdict core.ReviewVerdict) string {
	switch verdict := verdict.(type) {
	case core.ReviewVerdictClean:
		return "no material findings"
	case core.ReviewVerdictFindings:
		return "delivering review verdict"
	case core.ReviewVerdictFailed:
		return "the review failed: " + verdict.Reason
	}
	panic(fmt.Sprintf("unknown review verdict %T", verdict))
}

// job is the prompt input for the captured change: every repository with a
// patch and a baseline.
func (d *TurnReviewDriver) job() *ReviewJob {
	repositories := []RepositoryCapture{}
	for _, delta := range d.deltas {
		if strings.TrimSpace(delta.Patch) == "" || delta.BaselineTree == nil {
			continue
		}
		var unattributed *string
		if delta.Unattributed != nil {
			reason := *delta.Unattributed
			unattributed = &reason
		}
		repositories = append(repositories, RepositoryCapture{
			Root:         delta.Root,
			BaselineTree: *delta.BaselineTree,
			CaptureTree:  delta.CurrentTree,
			MergedIn:     append([]string{}, delta.MergedIn...),
			Unattributed: unattributed,
		})
	}
	var brief *string
	if d.seed.Brief != nil {
		text := *d.seed.Brief
		brief = &text
	}
	return &ReviewJob{
		Task:         d.seed.Task,
		UserMessages: append([]core.UserMessage{}, d.seed.UserMessages...),
		Repositories: repositories,
		ChangedFiles: ChangedFilesTable(d.deltas),
		ChangedLines: ChangedLineCount(d.deltas),
		Declined:     append([]core.ReviewOverride{}, d.seed.Declined...),
		Brief:        brief,
	}
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
	if _, ok := d.phase.(core.TurnReviewPhaseCapturingDelta); !ok {
		return []core.ReviewRequest{}
	}
	d.deltas = deltas
	if briefWriter {
		running := core.RoleStateRunning
		d.briefWriter = &running
		d.phase = core.TurnReviewPhaseWritingBrief{}
		d.status = d.phaseStatus()
		request := core.ReviewRequestStartBriefWriter{
			CommandID: core.BriefWriterCommandID(d.reviewID),
			Prompt:    BriefWriterPrompt(d.job()),
		}
		return d.emit([]core.ReviewRequest{request})
	}
	return d.emit(d.startReviewers())
}

// startReviewers enters Running and returns the start request of every
// reviewer.
func (d *TurnReviewDriver) startReviewers() []core.ReviewRequest {
	prompt := ReviewPrompt(d.job())
	d.phase = core.TurnReviewPhaseRunning{Roles: []core.RoleStatus{}}
	d.phase = core.TurnReviewPhaseRunning{Roles: d.roleStatuses()}
	d.status = d.phaseStatus()
	requests := make([]core.ReviewRequest, 0, len(d.roles))
	for _, slot := range d.roles {
		requests = append(requests, core.ReviewRequestStartReviewer{
			Role:      slot.Role,
			CommandID: slot.CommandID,
			Prompt:    prompt,
		})
	}
	return requests
}

// BriefWritten records the brief writer's handback. Its text goes to every
// reviewer's prompt and the reviewers start. An empty brief counts as a
// failed writer.
func (d *TurnReviewDriver) BriefWritten(text string) []core.ReviewRequest {
	if _, ok := d.phase.(core.TurnReviewPhaseWritingBrief); !ok {
		return []core.ReviewRequest{}
	}
	if trimmed := strings.TrimSpace(text); trimmed == "" || trimmed == "No findings." {
		return d.BriefFailed("the writer handed back an empty brief")
	}
	d.seed.Brief = &text
	clean := core.RoleStateClean
	d.briefWriter = &clean
	requests := d.startReviewers()
	requests = append(requests, core.ReviewRequestCloseBriefWriter{})
	return d.emit(requests)
}

// BriefFailed records that the brief writer could not start, ended without a
// handback, or failed. The review goes on without a brief: reviewers start
// as they would have, and the requester is told why there is none.
func (d *TurnReviewDriver) BriefFailed(reason string) []core.ReviewRequest {
	if _, ok := d.phase.(core.TurnReviewPhaseWritingBrief); !ok {
		return []core.ReviewRequest{}
	}
	d.seed.Brief = nil
	failed := core.RoleStateFailed
	d.briefWriter = &failed
	requests := []core.ReviewRequest{core.ReviewRequestNoteBriefSkipped{Reason: reason}}
	requests = append(requests, d.startReviewers()...)
	requests = append(requests, core.ReviewRequestCloseBriefWriter{})
	return d.emit(requests)
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
	if _, ok := d.phase.(core.TurnReviewPhaseRunning); !ok {
		return []core.ReviewRequest{}
	}
	index := slices.IndexFunc(d.roles, func(slot core.ReviewerOutcomeSlot) bool {
		return slot.CommandID == commandID && slot.Outcome == nil
	})
	if index < 0 {
		return []core.ReviewRequest{}
	}
	label := d.roleLabel(index, &childSessionID)
	outcome := ReviewOutputVerdict(answer)
	switch verdict := outcome.(type) {
	case core.ReviewVerdictFindings:
		outcome = core.ReviewVerdictFindings{
			Synthesis: label + "\n" + verdict.Synthesis,
			Evidence:  verdict.Evidence,
		}
	case core.ReviewVerdictFailed:
		outcome = core.ReviewVerdictFailed{Reason: label + ": " + verdict.Reason}
	}
	d.roles[index].Outcome = outcome
	return d.roleSettled()
}

// RoleFailed records an explicit lifecycle or harness failure of one role.
// childSessionID is nil when the role has no session.
func (d *TurnReviewDriver) RoleFailed(role string, childSessionID *string, reason string) []core.ReviewRequest {
	if _, ok := d.phase.(core.TurnReviewPhaseRunning); !ok {
		return []core.ReviewRequest{}
	}
	index := slices.IndexFunc(d.roles, func(slot core.ReviewerOutcomeSlot) bool {
		return slot.Role == role && slot.Outcome == nil
	})
	if index < 0 {
		return []core.ReviewRequest{}
	}
	label := d.roleLabel(index, childSessionID)
	d.roles[index].Outcome = core.ReviewVerdictFailed{Reason: label + ": " + reason}
	return d.roleSettled()
}

// roleLabel names the reviewer at index, with its session when known.
func (d *TurnReviewDriver) roleLabel(index int, childSessionID *string) string {
	if childSessionID != nil {
		return fmt.Sprintf("Reviewer %d (session %s)", index+1, *childSessionID)
	}
	return fmt.Sprintf("Reviewer %d", index+1)
}

// roleSettled reaches the group's verdict once every slot has an outcome.
func (d *TurnReviewDriver) roleSettled() []core.ReviewRequest {
	d.phase = core.TurnReviewPhaseRunning{Roles: d.roleStatuses()}
	if slices.ContainsFunc(d.roles, func(slot core.ReviewerOutcomeSlot) bool { return slot.Outcome == nil }) {
		return []core.ReviewRequest{}
	}
	sections := []string{}
	var failures []core.ReviewerFailure
	hasFindings := false
	for _, slot := range d.roles {
		switch outcome := slot.Outcome.(type) {
		case core.ReviewVerdictClean:
		case core.ReviewVerdictFindings:
			hasFindings = true
			sections = append(sections, outcome.Synthesis)
		case core.ReviewVerdictFailed:
			label, detail, ok := strings.Cut(outcome.Reason, ": ")
			if !ok {
				label, detail = slot.Role, outcome.Reason
			}
			sections = append(sections, label+"\nReview failed: "+detail)
			failures = append(failures, core.ReviewerFailure{Label: label, Reason: detail})
		default:
			panic(fmt.Sprintf("unknown review verdict %T", slot.Outcome))
		}
	}
	var verdict core.ReviewVerdict
	switch {
	case hasFindings:
		synthesis := strings.Join(sections, "\n\n")
		if len(d.roles) != 1 {
			synthesis = "Independent reviewer reports may overlap. Check each finding against the source.\n\n" + synthesis
		}
		verdict = core.ReviewVerdictFindings{
			Synthesis: synthesis,
			Evidence:  core.ReviewPassEvidence{ReviewerFailures: failures},
		}
	case len(failures) == 0:
		verdict = core.ReviewVerdictClean{}
	default:
		reasons := make([]string, 0, len(failures))
		for _, failure := range failures {
			reasons = append(reasons, failure.Label+": "+failure.Reason)
		}
		verdict = core.ReviewVerdictFailed{Reason: strings.Join(reasons, "; ")}
	}
	return d.reachVerdict(verdict)
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
	_, writerOpen := d.phase.(core.TurnReviewPhaseWritingBrief)
	// Empty captures and group-wide failures finish without individual
	// reports. Persist their terminal outcomes before delivery can race
	// daemon replacement.
	for index := range d.roles {
		if d.roles[index].Outcome == nil {
			d.roles[index].Outcome = verdict
		}
	}
	d.status = verdictStatus(verdict)
	d.lastVerdict = verdict
	d.phase = core.TurnReviewPhaseVerdict{Verdict: verdict}
	// Every role is reaped before the review resolves, whichever way it
	// resolves: a reviewing harness must never outlive the review.
	requests := d.pauseEveryRole()
	if writerOpen {
		requests = append(requests, core.ReviewRequestCloseBriefWriter{})
	}
	return d.emit(requests)
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
	if d.Finished() {
		return []core.ReviewRequest{}
	}
	requests := d.pauseEveryRole()
	if _, ok := d.phase.(core.TurnReviewPhaseWritingBrief); ok {
		requests = append(requests, core.ReviewRequestCloseBriefWriter{})
	}
	d.phase = core.TurnReviewPhaseResolved{Resolution: core.ResolutionCancelled}
	d.status = "review cancelled"
	requests = append(requests, core.ReviewRequestClose{})
	return d.emit(requests)
}
