// Port of mj-tui/src/wizards/dashboard/targets.rs.

// The wizards' target step: which targets are offered, sandbox offers,
// project directory checks, and container sizing against host capacity.

package tui

import (
	"fmt"
	"maps"
	"reflect"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SandboxOffer is the review step's warning that a guardian review runs
// directly on the host, with the sandboxed target to use instead when one is
// offered.
type SandboxOffer struct {
	warning     string
	alternative *int
}

func (d *DashboardState) sandboxOffer(wizard WizardDraft) *SandboxOffer {
	targets := d.config.Get().Targets
	ids := slices.Sorted(maps.Keys(targets))
	if wizard.Target() < 0 || wizard.Target() >= len(ids) {
		return nil
	}
	target := targets[ids[wizard.Target()]]
	if permission, ok := target.PermissionMode(); !ok || permission != core.PermissionModeGuardian {
		return nil
	}
	kind, ok := wizard.ReviewHarness(d)
	if !ok || kind.Guardian() != core.GuardianReviewerMjolnir {
		return nil
	}
	offered := d.offeredTargetIndices()
	var alternative *int
	for index, id := range ids {
		if !slices.Contains(offered, index) || !core.IsContainerTarget(targets[id]) {
			continue
		}
		if _, rejected := wizard.TargetRejection(d, id); rejected || !wizard.TargetCompatible(d, id) {
			continue
		}
		found := index
		alternative = &found
		break
	}
	return &SandboxOffer{
		warning: fmt.Sprintf(
			"%s runs directly on the host. Use a sandbox to isolate its actions.",
			kind.DisplayName(),
		),
		alternative: alternative,
	}
}

func (d *DashboardState) selectWizardTarget(wizard WizardDraft, index int) DashboardAction {
	if wizard.Target() != index {
		wizard.NoteDraftChange(d, DraftChangeTargetSelected)
		wizard.SetTarget(index)
		if wizard.PreparesTargetOnSelect(d) {
			action := d.prepareWizardTarget(wizard)
			d.mode = wizard.IntoMode()
			return action
		}
	}
	return d.keep(wizard)
}

// beginProjectDirectoryChecks starts checks for the local directories of
// configured projects that have no current answer. A check in flight is
// never repeated.
func (d *DashboardState) beginProjectDirectoryChecks() (DashboardAction, bool) {
	now := time.Now()
	wanted := map[string]struct{}{}
	for _, bundle := range d.config.Get().Bundles {
		for _, repository := range bundle.Repositories {
			if repository.Local == nil {
				continue
			}
			path := *repository.Local
			check, known := d.projectDirectoryChecks[path]
			if !known || (check.state != ProjectDirectoryStateChecking &&
				now.Sub(check.recordedAt) >= projectDirectoryCheckTTL) {
				wanted[path] = struct{}{}
			}
		}
	}
	paths := slices.Sorted(maps.Keys(wanted))
	if len(paths) == 0 {
		return nil, false
	}
	if d.projectDirectoryChecks == nil {
		d.projectDirectoryChecks = make(map[string]ProjectDirectoryCheck)
	}
	for _, path := range paths {
		d.projectDirectoryChecks[path] = ProjectDirectoryCheck{
			state:      ProjectDirectoryStateChecking,
			recordedAt: now,
		}
	}
	return DashboardActionCheckProjectDirectories{Paths: paths}, true
}

// ApplyProjectDirectoryCheck records the answer for path: whether it is a
// directory, or nil when the check could not tell.
func (d *DashboardState) ApplyProjectDirectoryCheck(path string, exists *bool) {
	state := ProjectDirectoryStateUnknown
	if exists != nil {
		if *exists {
			state = ProjectDirectoryStatePresent
		} else {
			state = ProjectDirectoryStateMissing
		}
	}
	if d.projectDirectoryChecks == nil {
		d.projectDirectoryChecks = make(map[string]ProjectDirectoryCheck)
	}
	d.projectDirectoryChecks[path] = ProjectDirectoryCheck{state: state, recordedAt: time.Now()}
}

// missingProjectDirectories is the configured local directories of bundle
// that the last check found missing. A directory not yet checked is not
// called missing.
func (d *DashboardState) missingProjectDirectories(bundle *core.ProjectBundle) []string {
	var missing []string
	for _, repository := range bundle.Repositories {
		if repository.Local == nil {
			continue
		}
		if check, ok := d.projectDirectoryChecks[*repository.Local]; ok && check.state == ProjectDirectoryStateMissing {
			missing = append(missing, *repository.Local)
		}
	}
	return missing
}

// offeredTargetIndices is the indexes into config.Targets (in key order) of
// the targets the wizards list: every one that is offered
// (DashboardState.targetOffered). wizard.target keeps indexing the full map;
// only the rows shown, and the position selected among them, go through this
// list.
func (d *DashboardState) offeredTargetIndices() []int {
	var offered []int
	for index, id := range slices.Sorted(maps.Keys(d.config.Get().Targets)) {
		if d.targetOffered(id) {
			offered = append(offered, index)
		}
	}
	return offered
}

// targetRow is the row target (an index into config.Targets) has among the
// offered targets. A target that is not offered has no row, so the first
// stands in and Next stays disabled by the target's own rejection.
func (d *DashboardState) targetRow(target int) int {
	return max(slices.Index(d.offeredTargetIndices(), target), 0)
}

// resumeWizardProfiles is the profiles the resume wizard offers. A record's
// resume is limited to the profiles compatible with it; an archived session
// has no record, so every enabled profile can carry its summary.
func (d *DashboardState) resumeWizardProfiles(wizard *ResumeWizard) []CompatibleProfile {
	if wizard.source == ResumeSourceSession {
		return d.compatibleProfiles(wizard.sessionID)
	}
	profiles := d.config.Get().Profiles
	var offered []CompatibleProfile
	for _, id := range slices.Sorted(maps.Keys(profiles)) {
		if d.profileOffered(id) {
			offered = append(offered, CompatibleProfile{ID: id, Kind: profiles[id].Kind})
		}
	}
	return offered
}

// resumeTargetRejection is why this session cannot resume on targetID, or
// false when it can.
func (d *DashboardState) resumeTargetRejection(sessionID string, targetID string) (string, bool) {
	if reason, ok := d.targetUnavailable(targetID); ok {
		return reason, true
	}
	return d.resumeTargetIncompatibility(sessionID, targetID)
}

// resumeTargetIncompatibility is why this session could not resume on
// targetID even once the target is ready. An archived session has no record
// here, so nothing rules a target out for it.
func (d *DashboardState) resumeTargetIncompatibility(sessionID string, targetID string) (string, bool) {
	session := d.SessionRecord(sessionID)
	if session == nil {
		return "", false
	}
	checkout, err := d.state.Get().Checkout(sessionID)
	if err != nil {
		checkout = session.RecordCheckout()
	}
	if _, err := client.ResumeCompatibilityWithCheckout(session, checkout, d.config.Get(), targetID); err != nil {
		return err.Error(), true
	}
	return "", false
}

// loneTarget is the index of the only target the target step would offer
// wizard, when that step has nothing else to decide.
//
// The step lists every offered target. One counts unless the draft cannot
// use it, such as a bundle session on a local bare target. A target no
// answer has reached yet is offered, so the step is shown while it might
// gain a choice. The one target must also have no size to set: a container
// or EC2 target is sized on that step, so its step is kept. So is the step
// of a configuration with a machine no runtime runs on, which the step
// names.
func (d *DashboardState) loneTarget(wizard WizardDraft) (int, bool) {
	config := d.config.Get()
	for range config.MachinesWithoutRuntimes() {
		return 0, false
	}
	ids := slices.Sorted(maps.Keys(config.Targets))
	found := -1
	for index, id := range ids {
		if !d.targetOffered(id) || !wizard.TargetCompatible(d, id) {
			continue
		}
		if found >= 0 {
			return 0, false
		}
		found = index
	}
	if found < 0 {
		return 0, false
	}
	noSize := false
	switch config.Targets[ids[found]].(type) {
	case core.TargetTemplateLocalBare, core.TargetTemplateSshBare:
		noSize = true
	}
	if _, rejected := wizard.TargetRejection(d, ids[found]); !noSize || rejected {
		return 0, false
	}
	return found, true
}

// skipTargetStep selects the target step's only target when loneTarget finds
// one, and records whether it did. The caller then advances from the target
// step, as Next there would; on false it shows the step.
func (d *DashboardState) skipTargetStep(wizard WizardDraft) bool {
	index, ok := d.loneTarget(wizard)
	if !ok {
		wizard.SetTargetStepSkipped(false)
		return false
	}
	if wizard.Target() != index {
		wizard.NoteDraftChange(d, DraftChangeTargetSelected)
		wizard.SetTarget(index)
	}
	// A bare target has no size, so this only clears a size left from the
	// target selected before, and never asks for EC2 sizes.
	d.prepareWizardTarget(wizard)
	wizard.SetTargetStepSkipped(true)
	return true
}

// refreshWizardResourceLimits: capacity samples validate the same draft as
// keystrokes; they never replace typed values with a silently clamped
// allocation.
func (d *DashboardState) refreshWizardResourceLimits(affected []string) {
	var index int
	var initialized bool
	switch mode := d.mode.(type) {
	case ModeNew:
		index, initialized = mode.target, mode.resourceEditor.targetID != nil
	case ModeResume:
		index, initialized = mode.target, mode.resourceEditor.targetID != nil
	default:
		return
	}
	targets := d.config.Get().Targets
	id := nthKey(targets, index)
	if !initialized || !slices.Contains(affected, id) || !core.IsContainerTarget(targets[id]) {
		return
	}
	mode := d.mode
	d.mode = ModeDashboard{}
	switch wizard := mode.(type) {
	case ModeNew:
		d.refreshResourceDraft(wizard.NewWizard)
	case ModeResume:
		d.refreshResourceDraft(wizard.ResumeWizard)
	}
	d.mode = mode
}

func (d *DashboardState) refreshResourceDraft(wizard WizardDraft) {
	before := wizard.ResourceAllocation()
	d.validateWizardResources(wizard)
	if !reflect.DeepEqual(wizard.ResourceAllocation(), before) {
		wizard.NoteDraftChange(d, DraftChangeResourcesAdjusted)
		if wizard.ResourceAllocation() == nil && wizard.Step() != WizardStepProfile {
			wizard.SetStep(WizardStepTarget)
			wizard.FormMut().Focus(WizardControlResourceCpu)
		}
	}
}

func (d *DashboardState) initializeWizardResources(wizard WizardDraft) DashboardAction {
	targets := d.config.Get().Targets
	targetID := nthKey(targets, wizard.Target())
	if editor := wizard.ResourceEditor(); editor.targetID != nil && *editor.targetID == targetID {
		return DashboardActionNone{}
	}
	if allocation := wizard.ResourceAllocation(); allocation != nil {
		editor := wizard.ResourceEditorMut()
		editor.reset(allocation)
		id := targetID
		editor.targetID = &id
		if core.IsContainerTarget(targets[targetID]) {
			d.validateWizardResources(wizard)
		}
		return DashboardActionNone{}
	}
	action := d.prepareWizardTarget(wizard)
	if core.IsContainerTarget(targets[targetID]) {
		d.validateWizardResources(wizard)
	}
	return action
}

func (d *DashboardState) prepareWizardTarget(wizard WizardDraft) DashboardAction {
	previous := wizard.PreviousAllocation(d)
	targetIndex := wizard.Target()
	prepared, sizingError, action := d.prepareTarget(targetIndex, previous)
	allocation, failure := wizard.SizingMut()
	*allocation = prepared
	*failure = sizingError
	targetID := nthKey(d.config.Get().Targets, targetIndex)
	editor := wizard.ResourceEditorMut()
	editor.reset(wizard.ResourceAllocation())
	editor.targetID = &targetID
	return action
}

// prepareTarget is the default sizing for the target at targetIndex, seeded
// from previous when it is a container size. Rust writes the allocation and
// sizing error through `&mut Option` parameters; Go returns them (nil for
// None) with the action.
func (d *DashboardState) prepareTarget(targetIndex int, previous core.SessionResourceAllocation) (allocation core.SessionResourceAllocation, sizingError *string, action DashboardAction) {
	targets := d.config.Get().Targets
	targetID := nthKey(targets, targetIndex)
	target := targets[targetID]
	switch target.(type) {
	case core.TargetTemplateLocalPodman,
		core.TargetTemplateLocalDocker,
		core.TargetTemplateAppleContainer,
		core.TargetTemplateSshPodman,
		core.TargetTemplateSshDocker,
		core.TargetTemplateSshAppleContainer,
		core.TargetTemplateSshNitro:
		var limits *core.HostContainerSize
		if host := d.hostLimits(targetID); host != nil {
			limits = &core.HostContainerSize{Cpus: host.Cpus, MemoryBytes: host.MemoryBytes}
		}
		var remembered *core.HostContainerSize
		if host, ok := core.ContainerSizeHost(target); ok {
			if size, ok := d.state.Get().ContainerSizes.Get(host); ok {
				remembered = &size
			}
		}
		previousSize := remembered
		if container, ok := previous.(core.SessionResourceAllocationContainer); ok {
			previousSize = &core.HostContainerSize{Cpus: container.Cpus, MemoryBytes: container.MemoryBytes}
		}
		var templateDefault *core.HostContainerSize
		if container := target.Container(); container != nil {
			templateDefault = container.DefaultHostContainerSize()
		}
		size := core.ResolveDefaultContainerSize(templateDefault, previousSize, limits)
		return core.SessionResourceAllocationContainer{Cpus: size.Cpus, MemoryBytes: size.MemoryBytes}, nil, DashboardActionNone{}
	case core.TargetTemplateAwsEc2:
		notice := core.LegacyEC2TargetNotice
		return nil, &notice, DashboardActionNone{}
	}
	// A bare target, local or over SSH, has no size to set.
	return nil, nil, DashboardActionNone{}
}

// hostLimits is the logical cores and total memory of the host that runs
// targetID, when its capacity reading has arrived; nil before then.
func (d *DashboardState) hostLimits(targetID string) *HostLimits {
	for _, key := range slices.Sorted(maps.Keys(d.capacityDetails)) {
		detail := d.capacityDetails[key]
		if !slices.Contains(detail.Reading.TargetIDs, targetID) {
			continue
		}
		if !detail.Reading.HasReading {
			return nil
		}
		return &HostLimits{Cpus: detail.Reading.LogicalCores, MemoryBytes: detail.Reading.MemoryTotalBytes}
	}
	return nil
}

func (d *DashboardState) hostContainerCapacity(targetID string) *core.HostContainerCapacity {
	for _, key := range slices.Sorted(maps.Keys(d.containerCapacities)) {
		capacity := d.containerCapacities[key]
		if slices.Contains(capacity.TargetIDs, targetID) {
			found := capacity.Capacity
			return &found
		}
	}
	return nil
}

func (d *DashboardState) SetContainerCapacities(capacities map[string]client.RuntimeContainerCapacity) {
	same := func(left, right client.RuntimeContainerCapacity) bool {
		return slices.Equal(left.TargetIDs, right.TargetIDs) && left.Capacity == right.Capacity
	}
	affected := map[string]struct{}{}
	for targetID, capacity := range d.containerCapacities {
		if next, ok := capacities[targetID]; !ok || !same(next, capacity) {
			for _, id := range capacity.TargetIDs {
				affected[id] = struct{}{}
			}
		}
	}
	for targetID, capacity := range capacities {
		if previous, ok := d.containerCapacities[targetID]; !ok || !same(previous, capacity) {
			for _, id := range capacity.TargetIDs {
				affected[id] = struct{}{}
			}
		}
	}
	d.containerCapacities = capacities
	d.refreshWizardResourceLimits(slices.Sorted(maps.Keys(affected)))
}

func (d *DashboardState) validateWizardResources(wizard WizardDraft) {
	targets := d.config.Get().Targets
	targetID := nthKey(targets, wizard.Target())
	_, usesRemaining := wizard.RememberContainerSize()
	capacity := d.hostContainerCapacity(targetID)
	// Remaining capacity is advisory except for Apple container RAM. Editors
	// still cap values at the physical host totals. Only a new session is
	// refused here: a resumed or moved session may already be counted in
	// the projection, and the daemon excludes it when deciding.
	var limits *HostLimits
	if capacity != nil {
		limits = &HostLimits{Cpus: capacity.Total.Cpus, MemoryBytes: capacity.Total.MemoryBytes}
	} else {
		limits = d.hostLimits(targetID)
	}
	value, err := wizard.ResourceEditor().allocation(limits, usesRemaining)
	allocation, failure := wizard.SizingMut()
	switch {
	case err == nil && usesRemaining && capacity != nil &&
		capacity.Refuses(value, core.ContainerMemoryIsHardLimit(targets[targetID])):
		message := capacity.RefusalMessage(value)
		*failure = &message
		*allocation = nil
	case err == nil:
		*allocation = value
		*failure = nil
	default:
		*allocation = nil
		message := err.Error()
		*failure = &message
	}
}
