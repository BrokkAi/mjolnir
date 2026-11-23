// Port of mj-tui/src/availability.rs.
//
// Request launch checks and render the verdicts published by the daemon.

package tui

import (
	"iter"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Re-exports (pub use) of the daemon's machine runtime probe types.
type (
	MachineFinding      = client.MachineFinding
	MachineRuntimeProbe = client.MachineRuntimeProbe
)

// AvailabilityAvailability is availability.rs's Availability (prefixed:
// actions.rs also declares Availability): whether a launch check is wanted
// or held, and the daemon's last verdicts.
type AvailabilityAvailability struct {
	wanted   bool
	held     bool
	snapshot core.SharedSnapshot[client.RuntimeAvailability]
}

// newAvailabilityAvailability is Rust's Default: a check is wanted.
func newAvailabilityAvailability() AvailabilityAvailability {
	return AvailabilityAvailability{
		wanted:   true,
		snapshot: core.NewSharedSnapshot(client.RuntimeAvailability{}),
	}
}

func (d *DashboardState) wantAvailabilityCheck() {
	d.availability.wanted = true
}

// HoldAvailabilityCheck waits for first-run setup to decide whether the
// configuration is new.
func (d *DashboardState) HoldAvailabilityCheck() {
	d.availability.held = true
}

func (d *DashboardState) ReleaseAvailabilityCheck() {
	d.availability.held = false
}

// TakeAvailabilityCheck is the launch check to send now, nil when none is
// wanted or one is held.
func (d *DashboardState) TakeAvailabilityCheck() DashboardAction {
	if _, setup := d.mode.(ModeSetup); !d.availability.wanted || d.availability.held || setup {
		return nil
	}
	d.availability.wanted = false
	return DashboardActionCheckAvailability{}
}

// SetRuntimeConfig installs configuration and its verdicts from the same
// feed frame.
func (d *DashboardState) SetRuntimeConfig(config core.Config, availability core.SharedSnapshot[client.RuntimeAvailability]) {
	panic("unported: mj-tui/src/availability.rs DashboardState::set_runtime_config")
}

func (d *DashboardState) profileMissing(id string) (string, bool) {
	verdict, ok := d.availability.snapshot.Get().Profiles[id]
	if !ok {
		return "", false
	}
	return verdict.MissingReason()
}

func (d *DashboardState) missingTargetLines() []client.AvailabilityNote {
	return d.availability.snapshot.Get().TargetNotes
}

func (d *DashboardState) targetUnavailable(id string) (string, bool) {
	panic("unported: mj-tui/src/availability.rs DashboardState::target_unavailable")
}

func (d *DashboardState) targetOffered(id string) bool {
	panic("unported: mj-tui/src/availability.rs DashboardState::target_offered")
}

func (d *DashboardState) targetMissingLine(id string) (string, bool) {
	panic("unported: mj-tui/src/availability.rs DashboardState::target_missing_line")
}

func (d *DashboardState) defaultEngineMissing(id string) bool {
	panic("unported: mj-tui/src/availability.rs DashboardState::default_engine_missing")
}

func (d *DashboardState) machineRuntimeReason(id string) (string, bool) {
	reason, ok := d.availability.snapshot.Get().MachinesWithoutRuntime[id]
	return reason, ok
}

// profileOffered reports whether profile id is offered for new work.
func (d *DashboardState) profileOffered(id string) bool {
	panic("unported: mj-tui/src/availability.rs DashboardState::profile_offered")
}

// offeredProfiles is the profiles a new session can use, in configuration
// order.
func (d *DashboardState) offeredProfiles() iter.Seq2[string, core.HarnessProfile] {
	panic("unported: mj-tui/src/availability.rs DashboardState::offered_profiles")
}

// nthOfferedProfile is the id of the offered profile at index, if there is
// one.
func (d *DashboardState) nthOfferedProfile(index int) (string, bool) {
	position := 0
	for id := range d.offeredProfiles() {
		if position == index {
			return id, true
		}
		position++
	}
	return "", false
}

// offeredTargetIndexOrFirst is the index in config.targets of wanted when it
// is offered, or of the first offered runtime.
func (d *DashboardState) offeredTargetIndexOrFirst(wanted string) int {
	panic("unported: mj-tui/src/availability.rs DashboardState::offered_target_index_or_first")
}

// missingProfilesNotice is why no profile can be offered, when every enabled
// one is missing.
func (d *DashboardState) missingProfilesNotice() (string, bool) {
	panic("unported: mj-tui/src/availability.rs DashboardState::missing_profiles_notice")
}

// missingProfileLines is the line under a profile list for each enabled
// profile it leaves out.
func (d *DashboardState) missingProfileLines() []string {
	panic("unported: mj-tui/src/availability.rs DashboardState::missing_profile_lines")
}

// wizardSelection is the (profile id, target id) an open wizard has chosen,
// so an answer that removes one can move the wizard off it. Nil when no
// wizard is open; the profile id is nil when the wizard's profile index
// names no offered profile.
func (d *DashboardState) wizardSelection() *client.DaemonPair[*string, string] {
	panic("unported: mj-tui/src/availability.rs DashboardState::wizard_selection")
}

// reselectWizard keeps an open wizard on the profile and runtime it had
// chosen, by id. One that is no longer offered gives way to the first that
// is, and the wizard returns to that step so the change is seen before
// anything launches. A launch already submitted keeps its choices and is
// left to its own preflight. A wizard left with nothing to offer, before it
// launches, closes with the reason.
func (d *DashboardState) reselectWizard(selected *client.DaemonPair[*string, string]) {
	panic("unported: mj-tui/src/availability.rs DashboardState::reselect_wizard")
}

// returnToChangedStep sends a wizard past the step of a choice that just
// disappeared back to that step. It returns the step it returned to, if it
// moved. A launch already submitted is left to its own preflight.
func returnToChangedStep(step *WizardStep, profileGone bool, targetGone bool) (WizardStep, bool) {
	if *step == WizardStepLaunching {
		return *step, false
	}
	if profileGone && *step != WizardStepProfile {
		*step = WizardStepProfile
		return WizardStepProfile, true
	}
	if targetGone && *step != WizardStepProfile && *step != WizardStepTarget {
		*step = WizardStepTarget
		return WizardStepTarget, true
	}
	return *step, false
}
