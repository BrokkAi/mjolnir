// Port of mj-tui/src/availability.rs.
//
// Request launch checks and render the verdicts published by the daemon.

package tui

import (
	"fmt"
	"iter"
	"reflect"
	"slices"
	"strings"

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
	if reflect.DeepEqual(*d.config.Get(), config) &&
		d.availability.snapshot.Equal(availability, func(a, b *client.RuntimeAvailability) bool {
			return reflect.DeepEqual(a, b)
		}) {
		return
	}
	selected := d.wizardSelection()
	d.availability.snapshot = availability
	d.SetConfig(config)
	d.reselectWizard(selected)
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
	if target, ok := d.config.Get().Targets[id]; ok && target != nil {
		if notice, refused := target.LaunchRefusal(); refused {
			return notice, true
		}
	}
	if d.targetOffered(id) {
		return "", false
	}
	if line, ok := d.targetMissingLine(id); ok {
		return line, true
	}
	return id + " is not available.", true
}

func (d *DashboardState) targetOffered(id string) bool {
	if _, enabled := d.config.Get().EnabledTarget(id); !enabled {
		return false
	}
	_, missing := d.targetMissingLine(id)
	return !missing
}

func (d *DashboardState) targetMissingLine(id string) (string, bool) {
	if _, enabled := d.config.Get().EnabledTarget(id); !enabled {
		return "", false
	}
	verdict, ok := d.availability.snapshot.Get().Targets[id]
	if !ok {
		return "", false
	}
	return verdict.MissingReason()
}

func (d *DashboardState) defaultEngineMissing(id string) bool {
	if !d.config.Get().IsDefaultTarget(id) {
		return false
	}
	_, missing := d.availability.snapshot.Get().UninstalledEngines[id]
	return missing
}

func (d *DashboardState) machineRuntimeReason(id string) (string, bool) {
	reason, ok := d.availability.snapshot.Get().MachinesWithoutRuntime[id]
	return reason, ok
}

// profileOffered reports whether profile id is offered for new work.
func (d *DashboardState) profileOffered(id string) bool {
	if _, enabled := d.config.Get().EnabledProfile(id); !enabled {
		return false
	}
	_, missing := d.profileMissing(id)
	return !missing
}

// offeredProfiles is the profiles a new session can use, in configuration
// order.
func (d *DashboardState) offeredProfiles() iter.Seq2[string, core.HarnessProfile] {
	return func(yield func(string, core.HarnessProfile) bool) {
		for id, profile := range d.config.Get().EnabledProfiles() {
			if _, missing := d.profileMissing(id); missing {
				continue
			}
			if !yield(id, profile) {
				return
			}
		}
	}
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
	keys := availabilityTargetKeys(d.config.Get())
	if index := slices.Index(keys, wanted); index >= 0 && d.targetOffered(wanted) {
		return index
	}
	if index := slices.IndexFunc(keys, d.targetOffered); index >= 0 {
		return index
	}
	return 0
}

// availabilityTargetKeys is config.targets' keys in the BTreeMap's order.
func availabilityTargetKeys(config *core.Config) []string {
	keys := make([]string, 0, len(config.Targets))
	for id := range config.Targets {
		keys = append(keys, id)
	}
	slices.Sort(keys)
	return keys
}

// missingProfilesNotice is why no profile can be offered, when every enabled
// one is missing.
func (d *DashboardState) missingProfilesNotice() (string, bool) {
	lines := d.missingProfileLines()
	if len(lines) == 0 {
		return "", false
	}
	return "No profile is available here. " + strings.Join(lines, " "), true
}

// missingProfileLines is the line under a profile list for each enabled
// profile it leaves out.
func (d *DashboardState) missingProfileLines() []string {
	var lines []string
	for id := range d.config.Get().EnabledProfiles() {
		if reason, missing := d.profileMissing(id); missing {
			lines = append(lines, fmt.Sprintf("%s: %s", id, reason))
		}
	}
	return lines
}

// wizardSelection is the (profile id, target id) an open wizard has chosen,
// so an answer that removes one can move the wizard off it. Nil when no
// wizard is open; the profile id is nil when the wizard's profile index
// names no offered profile.
func (d *DashboardState) wizardSelection() *client.DaemonPair[*string, string] {
	switch mode := d.mode.(type) {
	case ModeNew:
		var profile *string
		if id, ok := d.nthOfferedProfile(mode.profile); ok {
			profile = &id
		}
		return &client.DaemonPair[*string, string]{First: profile, Second: nthKey(d.config.Get().Targets, mode.target)}
	case ModeResume:
		var profile *string
		if profiles := d.resumeWizardProfiles(mode.ResumeWizard); mode.profile >= 0 && mode.profile < len(profiles) {
			id := profiles[mode.profile].ID
			profile = &id
		}
		return &client.DaemonPair[*string, string]{First: profile, Second: nthKey(d.config.Get().Targets, mode.target)}
	}
	return nil
}

// reselectWizard keeps an open wizard on the profile and runtime it had
// chosen, by id. One that is no longer offered gives way to the first that
// is, and the wizard returns to that step so the change is seen before
// anything launches. A launch already submitted keeps its choices and is
// left to its own preflight. A wizard left with nothing to offer, before it
// launches, closes with the reason.
func (d *DashboardState) reselectWizard(selected *client.DaemonPair[*string, string]) {
	if selected == nil {
		return
	}
	profileID, targetID := selected.First, selected.Second
	var profiles []string
	var launching bool
	switch mode := d.mode.(type) {
	case ModeNew:
		for id := range d.offeredProfiles() {
			profiles = append(profiles, id)
		}
		launching = mode.step == WizardStepLaunching
	case ModeResume:
		for _, profile := range d.resumeWizardProfiles(mode.ResumeWizard) {
			profiles = append(profiles, profile.ID)
		}
		launching = mode.step == WizardStepLaunching
	default:
		return
	}
	targetKeys := availabilityTargetKeys(d.config.Get())
	firstTarget := slices.IndexFunc(targetKeys, d.targetOffered)
	if !launching && (len(profiles) == 0 || firstTarget < 0) {
		var reason string
		if profileID != nil && len(profiles) == 0 {
			reason = "No profile is available here."
			if missing, ok := d.profileMissing(*profileID); ok {
				reason = fmt.Sprintf("No profile is available here. %s: %s", *profileID, missing)
			}
		} else {
			reason = "No runtime is available here."
			if missing, ok := d.targetMissingLine(targetID); ok {
				reason = fmt.Sprintf("No runtime is available here. %s: %s", targetID, missing)
			}
		}
		d.mode = ModeDashboard{}
		d.SetFailureNotice(reason)
		return
	}
	profile := -1
	if profileID != nil {
		profile = slices.Index(profiles, *profileID)
	}
	targetIndex := slices.Index(targetKeys, targetID)
	target := -1
	if targetIndex >= 0 && d.targetOffered(targetID) {
		target = targetIndex
	}
	lastProfile := max(len(profiles)-1, 0)
	var gone *WizardStep
	place := func(step *WizardStep, wizardProfile *int, wizardTarget *int) {
		if *step == WizardStepLaunching {
			// Keep pointing at the same choices; the launch carries them.
			if profile >= 0 {
				*wizardProfile = profile
			} else {
				*wizardProfile = min(*wizardProfile, lastProfile)
			}
			if targetIndex >= 0 {
				*wizardTarget = targetIndex
			}
			return
		}
		*wizardProfile = max(profile, 0)
		switch {
		case target >= 0:
			*wizardTarget = target
		case firstTarget >= 0:
			*wizardTarget = firstTarget
		default:
			*wizardTarget = 0
		}
		if returned, moved := returnToChangedStep(step, profile < 0, target < 0); moved {
			gone = &returned
		}
	}
	revealNewProfile := -1
	switch mode := d.mode.(type) {
	case ModeNew:
		before := mode.profile
		place(&mode.step, &mode.profile, &mode.target)
		if mode.profile != before || profile < 0 {
			mode.focusedProvider = nil
			revealNewProfile = mode.profile
		}
	case ModeResume:
		if !launching && (profile < 0 || target < 0) {
			mode.noteDestinationChange()
		}
		place(&mode.step, &mode.profile, &mode.target)
	}
	if revealNewProfile >= 0 {
		provider := newWizardProviderForProfile(d, revealNewProfile)
		if wizard, ok := d.mode.(ModeNew); ok && provider != nil {
			if wizard.expandedProviders == nil {
				wizard.expandedProviders = map[core.QuotaProvider]struct{}{}
			}
			wizard.expandedProviders[provider] = struct{}{}
		}
	}
	if gone != nil {
		var id string
		var reason string
		var known bool
		if *gone == WizardStepProfile {
			if profileID != nil {
				id = *profileID
			}
			reason, known = d.profileMissing(id)
		} else {
			id = targetID
			reason, known = d.targetMissingLine(targetID)
		}
		if known {
			d.SetFailureNotice(fmt.Sprintf("%s is no longer available: %s", id, reason))
		} else {
			d.SetFailureNotice(fmt.Sprintf("%s is no longer available.", id))
		}
	}
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
