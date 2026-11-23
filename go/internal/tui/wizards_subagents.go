// Port of mj-tui/src/wizards/subagents.rs.

package tui

import (
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SubagentDiscovery is what configuration says about Mjolnir sub-agents for
// the chosen profile.
type SubagentDiscovery struct {
	// Result is nil until checked; its Err says why Mjolnir sub-agents are
	// unavailable.
	Result *client.DaemonResult[core.SubagentOptions]
}

// SubagentDraft is the delegation choice on a review step: the policy being
// drafted, its open combo box, and global configuration availability. Move
// initializes it from the session record.
type SubagentDraft struct {
	oracle                  bool
	oracleChoice            *core.SubagentChoice
	oracleConfigured        bool
	orchestratorsConfigured bool
	// policy is the drafted intent. A harness without sub-agents of its own
	// shows and submits Native as None; see selected.
	policy              core.SubagentPolicy
	chosenPolicy        *core.SubagentPolicy
	hasRememberedPolicy bool
	lastMjolnirPolicy   *core.SubagentPolicy
	combo               chat.ComboBoxState[WizardControl]
	discovery           *SubagentDiscovery
	// harness is the harness of the profile the choice is for, once known.
	harness *core.HarnessKind
	// profileID is the profile whose remembered policy initialized this
	// draft.
	profileID *string
}

// DelegationMode is one entry of the delegation combo box.
type DelegationMode uint8

const (
	DelegationModeNative DelegationMode = iota
	DelegationModeMjolnir
	DelegationModeNone
)

// label is the name the combo box and launch notices give this mode.
func (m DelegationMode) label() string {
	switch m {
	case DelegationModeNative:
		return "Native"
	case DelegationModeMjolnir:
		return "Mjolnir"
	default:
		return "None"
	}
}

func delegationModeOf(policy *core.SubagentPolicy) DelegationMode {
	switch {
	case policy.UsesMjolnir():
		return DelegationModeMjolnir
	case policy.IsNative():
		return DelegationModeNative
	default:
		return DelegationModeNone
	}
}

var (
	delegationModesWithoutNative = []DelegationMode{DelegationModeMjolnir, DelegationModeNone}
	delegationModesAll           = []DelegationMode{DelegationModeNative, DelegationModeMjolnir, DelegationModeNone}
)

func (d *SubagentDraft) oracleAvailable() bool {
	return d.oracleConfigured || d.oracleChoice != nil
}

func (d *SubagentDraft) orchestratorsAvailable() bool {
	return d.policy.UsesMjolnir() && d.orchestratorsConfigured
}

func newSubagentDraft(policy core.SubagentPolicy) SubagentDraft {
	draft := SubagentDraft{oracle: true, policy: policy}
	if policy.UsesMjolnir() {
		remembered := policy
		draft.lastMjolnirPolicy = &remembered
	}
	return draft
}

func (d SubagentDraft) withOracle(enabled bool, choice *core.SubagentChoice) SubagentDraft {
	d.oracle = enabled
	d.oracleChoice = choice
	return d
}

// modes is the entries the combo box lists. Native is hidden for a harness
// without sub-agents of its own, where it would do nothing.
func (d *SubagentDraft) modes() []DelegationMode {
	if d.harness != nil && !d.harness.HasNativeSubagents() {
		return delegationModesWithoutNative
	}
	return delegationModesAll
}

// shown is policy as this choice shows it for the profile's harness.
func (d *SubagentDraft) shown(policy *core.SubagentPolicy) core.SubagentPolicy {
	if d.harness != nil {
		return policy.AsChoiceFor(*d.harness)
	}
	return *policy
}

// selected is the policy the draft shows.
func (d *SubagentDraft) selected() core.SubagentPolicy {
	return d.shown(&d.policy)
}

// createSelection is nil for an untouched Create default, which admission
// resolves against current capabilities.
func (d *SubagentDraft) createSelection() *core.SubagentPolicy {
	if d.chosenPolicy == nil && !d.hasRememberedPolicy {
		return nil
	}
	selected := d.selected()
	return &selected
}

// isMjolnirIndex reports whether the combo box entry at index is Mjolnir.
func (d *SubagentDraft) isMjolnirIndex(index int) bool {
	modes := d.modes()
	return index >= 0 && index < len(modes) && modes[index] == DelegationModeMjolnir
}

func (d *SubagentDraft) policies() []string {
	panic("unported: mj-tui/src/wizards/subagents.rs SubagentDraft::policies")
}

// policyLabel is the combo box's name for the mode of policy, as shown for
// the profile's harness.
func (d *SubagentDraft) policyLabel(policy *core.SubagentPolicy) string {
	shown := d.shown(policy)
	return delegationModeOf(&shown).label()
}

func (d *SubagentDraft) policyIndex() int {
	selected := d.selected()
	return max(slices.Index(d.modes(), delegationModeOf(&selected)), 0)
}

func (d *SubagentDraft) enabledPolicies() []bool {
	_, unavailable := d.unavailableReason()
	modes := d.modes()
	enabled := make([]bool, len(modes))
	for index, mode := range modes {
		enabled[index] = mode != DelegationModeMjolnir || !unavailable
	}
	return enabled
}

func (d *SubagentDraft) unavailableReason() (string, bool) {
	if d.discovery == nil || d.discovery.Result == nil || d.discovery.Result.Err == nil {
		return "", false
	}
	return *d.discovery.Result.Err, true
}

func (d *SubagentDraft) error() (string, bool) {
	if !d.policy.UsesMjolnir() {
		return "", false
	}
	return d.unavailableReason()
}

func (d *SubagentDraft) selectPolicy(index int) bool {
	panic("unported: mj-tui/src/wizards/subagents.rs SubagentDraft::select_policy")
}

func (d *SubagentDraft) activate(id WizardControl) bool {
	if id == WizardControlOracle && d.oracleAvailable() {
		selected := 0
		if !d.oracle {
			selected = 1
		}
		d.combo.Open(id, selected)
		return true
	}
	if id != WizardControlSubagents {
		return false
	}
	d.combo.Open(id, d.policyIndex())
	return true
}

// apply applies an interaction with one of the draft's controls; ok is false
// for an interaction that is not the draft's, otherwise changed reports
// whether the draft changed.
func (d *SubagentDraft) apply(interaction chat.Interaction[WizardControl]) (changed bool, ok bool) {
	panic("unported: mj-tui/src/wizards/subagents.rs SubagentDraft::apply")
}

func (d *SubagentDraft) declare(form *chat.Dialog[WizardControl]) {
	panic("unported: mj-tui/src/wizards/subagents.rs SubagentDraft::declare")
}

func (d *SubagentDraft) refreshCreate(profile string, config *core.Config, remembered *core.SubagentPolicy, rememberedOracle bool) {
	panic("unported: mj-tui/src/wizards/subagents.rs SubagentDraft::refresh_create")
}

func (d *SubagentDraft) updateChoices(profile string, config *core.Config) {
	panic("unported: mj-tui/src/wizards/subagents.rs SubagentDraft::update_choices")
}

// reviewSubagentDraft is the review step's delegation draft and profile,
// when the open wizard offers the choice.
func (d *DashboardState) reviewSubagentDraft() (draft *SubagentDraft, profile string, ok bool) {
	panic("unported: mj-tui/src/wizards/subagents.rs DashboardState::review_subagent_draft")
}

func (d *DashboardState) takeSubagentDiscovery() DashboardAction {
	panic("unported: mj-tui/src/wizards/subagents.rs DashboardState::take_subagent_discovery")
}

// subagentSelection is the displayed effective policy for comparison with a
// prepared Move; nil when the choice does not apply.
func (w *ResumeWizard) subagentSelection(dashboard *DashboardState) *core.SubagentPolicy {
	if !w.subagentChoiceApplies(dashboard) {
		return nil
	}
	selected := w.subagents.selected()
	return &selected
}

// subagentChoiceApplies: Move always shows delegation, including the reason
// Mjolnir is unavailable. A plain resume keeps the session's own policy.
func (w *ResumeWizard) subagentChoiceApplies(dashboard *DashboardState) bool {
	return w.moving
}

// subagentChange is the policy a Move request carries: nil keeps the
// session's own, so an untouched choice changes nothing. A stored Native that
// the destination shows as None is untouched, and keeps working there.
func (w *ResumeWizard) subagentChange(dashboard *DashboardState) *core.SubagentPolicy {
	panic("unported: mj-tui/src/wizards/subagents.rs ResumeWizard::subagent_change")
}
