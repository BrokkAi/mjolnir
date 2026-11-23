// Port of mj-tui/src/wizards/dashboard.rs.

// The new-session and resume wizards' event handling on the dashboard: one
// persistent component form per wizard, shared step navigation, and the
// attachment editor both wizards use.
//
// Rust's handlers are generic over `W: WizardDraft` and take the wizard by
// value; Go methods cannot be generic, so they take the WizardDraft
// interface, whose implementations are the wizards' pointers.

package tui

import (
	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

func declareWizardControls(dashboard *DashboardState, wizard WizardDraft) {
	panic("unported: mj-tui/src/wizards/dashboard.rs declare_wizard_controls")
}

func invalidateMovePreparation(wizard *ResumeWizard) {
	if wizard.moving {
		wizard.preparation = nil
		wizard.preparing = false
		wizard.preparationRequestID = nil
		wizard.preparationError = nil
	}
}

func declareWizardButtons(form *chat.Dialog[WizardControl], hasBack bool, nextEnabled bool) {
	panic("unported: mj-tui/src/wizards/dashboard.rs declare_wizard_buttons")
}

func openMountAccess(mounts *MountWizard) {
	panic("unported: mj-tui/src/wizards/dashboard.rs open_mount_access")
}

func commitMountAccess(mounts *MountWizard, index int) {
	if choices := mounts.accessChoices(); index < len(choices) {
		mounts.access = choices[index]
	}
}

func declareMountControls(form *chat.Dialog[WizardControl], mounts *MountWizard) {
	panic("unported: mj-tui/src/wizards/dashboard.rs declare_mount_controls")
}

func declareReviewControls(form *chat.Dialog[WizardControl], canAttach bool, submitEnabled bool) {
	panic("unported: mj-tui/src/wizards/dashboard.rs declare_review_controls")
}

// handleWizardEvent handles a wizard event through its persistent component
// form.
func (d *DashboardState) handleWizardEvent(event tea.Msg, wizard WizardDraft) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::handle_wizard_event")
}

func (d *DashboardState) applyWizardInteraction(wizard WizardDraft, interaction chat.Interaction[WizardControl], isClick bool) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::apply_wizard_interaction")
}

func (d *DashboardState) applyWizardFieldEdit(wizard WizardDraft, id WizardControl, edit chat.FieldEdit) {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::apply_wizard_field_edit")
}

// applyMountFieldEdit edits the attachment editor's two path fields.
func (d *DashboardState) applyMountFieldEdit(mounts *MountWizard, id WizardControl, edit chat.FieldEdit) {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::apply_mount_field_edit")
}

func (d *DashboardState) activateWizardControl(wizard WizardDraft, id WizardControl) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::activate_wizard_control")
}

func (d *DashboardState) activateNewBundleControl(wizard *NewWizard, id WizardControl) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::activate_new_bundle_control")
}

func (d *DashboardState) addOrReportNewBundleRepository(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::add_or_report_new_bundle_repository")
}

func (d *DashboardState) submitNewBundle(wizard *NewWizard) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::submit_new_bundle")
}

func (d *DashboardState) handleWizardShortcut(key tea.KeyPressMsg, wizard WizardDraft) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::handle_wizard_shortcut")
}

func (d *DashboardState) activateWizardReview(id WizardControl, wizard WizardDraft) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::activate_wizard_review")
}

func (d *DashboardState) activateWizardMount(id WizardControl, wizard WizardDraft) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::activate_wizard_mount")
}

func (d *DashboardState) validateWizardMount(wizard WizardDraft, targetTemplateID string) DashboardAction {
	panic("unported: mj-tui/src/wizards/dashboard.rs DashboardState::validate_wizard_mount")
}
