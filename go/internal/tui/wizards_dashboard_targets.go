// Port of mj-tui/src/wizards/dashboard/targets.rs.

// The wizards' target step: which targets are offered, sandbox offers,
// project directory checks, and container sizing against host capacity.

package tui

import (
	"slices"

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
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::sandbox_offer")
}

func (d *DashboardState) selectWizardTarget(wizard WizardDraft, index int) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::select_wizard_target")
}

// beginProjectDirectoryChecks starts checks for the local directories of
// configured projects that have no current answer. A check in flight is
// never repeated.
func (d *DashboardState) beginProjectDirectoryChecks() (DashboardAction, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::begin_project_directory_checks")
}

// ApplyProjectDirectoryCheck records the answer for path: whether it is a
// directory, or nil when the check could not tell.
func (d *DashboardState) ApplyProjectDirectoryCheck(path string, exists *bool) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::apply_project_directory_check")
}

// missingProjectDirectories is the configured local directories of bundle
// that the last check found missing. A directory not yet checked is not
// called missing.
func (d *DashboardState) missingProjectDirectories(bundle *core.ProjectBundle) []string {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::missing_project_directories")
}

// offeredTargetIndices is the indexes into config.Targets (in key order) of
// the targets the wizards list: every one that is offered
// (DashboardState.targetOffered). wizard.target keeps indexing the full map;
// only the rows shown, and the position selected among them, go through this
// list.
func (d *DashboardState) offeredTargetIndices() []int {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::offered_target_indices")
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
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::resume_wizard_profiles")
}

// resumeTargetRejection is why this session cannot resume on targetID, or
// false when it can.
func (d *DashboardState) resumeTargetRejection(sessionID string, targetID string) (string, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::resume_target_rejection")
}

// resumeTargetIncompatibility is why this session could not resume on
// targetID even once the target is ready. An archived session has no record
// here, so nothing rules a target out for it.
func (d *DashboardState) resumeTargetIncompatibility(sessionID string, targetID string) (string, bool) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::resume_target_incompatibility")
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
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::lone_target")
}

// skipTargetStep selects the target step's only target when loneTarget finds
// one, and records whether it did. The caller then advances from the target
// step, as Next there would; on false it shows the step.
func (d *DashboardState) skipTargetStep(wizard WizardDraft) bool {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::skip_target_step")
}

// refreshWizardResourceLimits: capacity samples validate the same draft as
// keystrokes; they never replace typed values with a silently clamped
// allocation.
func (d *DashboardState) refreshWizardResourceLimits(affected []string) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::refresh_wizard_resource_limits")
}

func (d *DashboardState) refreshResourceDraft(wizard WizardDraft) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::refresh_resource_draft")
}

func (d *DashboardState) initializeWizardResources(wizard WizardDraft) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::initialize_wizard_resources")
}

func (d *DashboardState) prepareWizardTarget(wizard WizardDraft) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::prepare_wizard_target")
}

// prepareTarget is the default sizing for the target at targetIndex, seeded
// from previous when it is a container size. Rust writes the allocation and
// sizing error through `&mut Option` parameters; Go returns them (nil for
// None) with the action.
func (d *DashboardState) prepareTarget(targetIndex int, previous core.SessionResourceAllocation) (allocation core.SessionResourceAllocation, sizingError *string, action DashboardAction) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::prepare_target")
}

// hostLimits is the logical cores and total memory of the host that runs
// targetID, when its capacity reading has arrived; nil before then.
func (d *DashboardState) hostLimits(targetID string) *HostLimits {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::host_limits")
}

func (d *DashboardState) hostContainerCapacity(targetID string) *core.HostContainerCapacity {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::host_container_capacity")
}

func (d *DashboardState) SetContainerCapacities(capacities map[string]client.RuntimeContainerCapacity) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::set_container_capacities")
}

func (d *DashboardState) validateWizardResources(wizard WizardDraft) {
	panic("unported: mj-tui/src/wizards/dashboard/targets.rs DashboardState::validate_wizard_resources")
}
