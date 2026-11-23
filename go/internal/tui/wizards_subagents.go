// Port of mj-tui/src/wizards/subagents.rs.

package tui

import (
	"fmt"
	"reflect"
	"slices"
	"strings"

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

// subagentPolicyEqual is SubagentPolicy's derived PartialEq: the zero mode is
// Native, and only Mjolnir carries choices.
func subagentPolicyEqual(a, b *core.SubagentPolicy) bool {
	if a.IsNative() || b.IsNative() {
		return a.IsNative() && b.IsNative()
	}
	if a.Mode != b.Mode {
		return false
	}
	if a.Mode != core.SubagentPolicyMjolnir {
		return true
	}
	return a.Orchestrators == b.Orchestrators &&
		reflect.DeepEqual(a.Straightforward, b.Straightforward) &&
		reflect.DeepEqual(a.Subtle, b.Subtle)
}

// subagentDefaultMjolnir is Mjolnir with no orchestrators and no overrides.
func subagentDefaultMjolnir() core.SubagentPolicy {
	return core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
}

func (d *SubagentDraft) policies() []string {
	remembered := &d.policy
	if d.lastMjolnirPolicy != nil {
		remembered = d.lastMjolnirPolicy
	}
	label := func(name string, choice *core.SubagentChoice) (string, bool) {
		if choice == nil {
			return "", false
		}
		effort := ""
		if choice.Effort != nil {
			effort = ", " + *choice.Effort
		}
		return fmt.Sprintf("%s: %s%s", name, choice.Model, effort), true
	}
	var overrides []string
	if text, ok := label("straightforward", remembered.OverrideChoice(core.SubagentTaskTypeStraightforward)); ok {
		overrides = append(overrides, text)
	}
	if text, ok := label("subtle", remembered.OverrideChoice(core.SubagentTaskTypeSubtle)); ok {
		overrides = append(overrides, text)
	}
	mjolnir := "Mjolnir"
	if len(overrides) > 0 {
		mjolnir = fmt.Sprintf("Mjolnir (%s)", strings.Join(overrides, "; "))
	}
	modes := d.modes()
	policies := make([]string, 0, len(modes))
	for _, mode := range modes {
		if mode == DelegationModeMjolnir {
			policies = append(policies, mjolnir)
		} else {
			policies = append(policies, mode.label())
		}
	}
	return policies
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
	modes := d.modes()
	if index < 0 || index >= len(modes) {
		return false
	}
	mode := modes[index]
	if _, unavailable := d.unavailableReason(); mode == DelegationModeMjolnir && unavailable {
		return false
	}
	var policy core.SubagentPolicy
	switch mode {
	case DelegationModeNative:
		policy = core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	case DelegationModeMjolnir:
		policy = subagentDefaultMjolnir()
		if d.lastMjolnirPolicy != nil {
			policy = *d.lastMjolnirPolicy
		}
	default:
		policy = core.SubagentPolicy{Mode: core.SubagentPolicyNone}
	}
	selected := d.selected()
	shown := d.shown(&policy)
	changed := !subagentPolicyEqual(&selected, &shown)
	chosen := policy
	d.chosenPolicy = &chosen
	if policy.UsesMjolnir() {
		remembered := policy
		d.lastMjolnirPolicy = &remembered
	}
	d.policy = policy
	return changed
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
	switch interaction := interaction.(type) {
	case chat.InteractionComboBoxCommit[WizardControl]:
		switch {
		case interaction.ID == WizardControlSubagents:
			return d.selectPolicy(interaction.Index), true
		case interaction.ID == WizardControlOracle && d.oracleAvailable():
			next := interaction.Index == 0
			changed := d.oracle != next
			d.oracle = next
			return changed, true
		}
	case chat.InteractionToggle[WizardControl]:
		if interaction.ID == WizardControlOrchestrators && d.orchestratorsAvailable() {
			if !d.policy.UsesMjolnir() {
				return false, false
			}
			d.policy.Orchestrators = !d.policy.Orchestrators
			chosen := d.policy
			d.chosenPolicy = &chosen
			remembered := d.policy
			d.lastMjolnirPolicy = &remembered
			return true, true
		}
	}
	return false, false
}

func (d *SubagentDraft) declare(form *chat.Dialog[WizardControl]) {
	form.DeclareWithEnabled(
		WizardControlSubagents,
		chat.ControlKindComboBox{
			Len:      len(d.policies()),
			Selected: d.combo.Selection(WizardControlSubagents, d.policyIndex()),
			Expanded: d.combo.IsOpen(WizardControlSubagents),
		},
		true,
	)
	form.SetRowEnabled(WizardControlSubagents, d.enabledPolicies())
	if d.oracleAvailable() {
		committed := 0
		if !d.oracle {
			committed = 1
		}
		form.DeclareWithEnabled(
			WizardControlOracle,
			chat.ControlKindComboBox{
				Len:      2,
				Selected: d.combo.Selection(WizardControlOracle, committed),
				Expanded: d.combo.IsOpen(WizardControlOracle),
			},
			true,
		)
	}
	if d.orchestratorsAvailable() {
		form.DeclareWithEnabled(WizardControlOrchestrators, chat.ControlKindCheckbox{}, true)
	}
}

func (d *SubagentDraft) refreshCreate(profile string, config *core.Config, remembered *core.SubagentPolicy, rememberedOracle bool) {
	if d.profileID == nil || *d.profileID != profile {
		d.oracle = rememberedOracle
		id := profile
		d.profileID = &id
		d.chosenPolicy = nil
		d.lastMjolnirPolicy = nil
		// SubagentPolicy's Rust default is Native.
		d.policy = core.SubagentPolicy{Mode: core.SubagentPolicyNative}
		if remembered != nil {
			d.policy = *remembered
		}
		d.combo = chat.ComboBoxState[WizardControl]{}
	}
	d.hasRememberedPolicy = remembered != nil
	// Keep the user's intent separate from the available displayed policy.
	desired := subagentDefaultMjolnir()
	switch {
	case d.chosenPolicy != nil:
		desired = *d.chosenPolicy
	case remembered != nil:
		desired = *remembered
	}
	if desired.UsesMjolnir() {
		lastMjolnir := desired
		d.lastMjolnirPolicy = &lastMjolnir
	}
	d.updateChoices(profile, config)
	policy := desired
	if _, unavailable := d.unavailableReason(); desired.UsesMjolnir() && unavailable {
		policy = core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	}
	if !subagentPolicyEqual(&d.policy, &policy) {
		d.policy = policy
		d.combo = chat.ComboBoxState[WizardControl]{}
	}
}

func (d *SubagentDraft) updateChoices(profile string, config *core.Config) {
	autoPrimary := profile == ""
	d.harness = nil
	if !autoPrimary {
		if harness, ok := config.Profiles[profile]; ok {
			kind := harness.Kind
			d.harness = &kind
		}
	}
	d.oracleConfigured = config.Subagents.OracleConfigured()
	d.orchestratorsConfigured = config.OrchestratorsConfigured()
	policy := d.policy
	if d.lastMjolnirPolicy != nil {
		policy = *d.lastMjolnirPolicy
	}
	var result client.DaemonResult[core.SubagentOptions]
	harness, known := config.Profiles[profile]
	switch {
	case !autoPrimary && !(known && harness.Kind.SupportsDelegationTools()):
		result = client.DaemonErr[core.SubagentOptions]("Mjolnir sub-agents need a Claude, Codex, DSH, or OpenCode profile")
	case len(config.Subagents.EffectiveChoices(policy, core.SubagentTaskTypeStraightforward)) > 0:
		// Subtle and Oracle are optional; straightforward alone enables Mjolnir.
		result = client.DaemonOk(core.SubagentOptions{})
	default:
		result = client.DaemonErr[core.SubagentOptions]("Set up Mjolnir sub-agents in Settings -> Sub-agents")
	}
	d.discovery = &SubagentDiscovery{Result: &result}
}

// reviewSubagentDraft is the review step's delegation draft and profile,
// when the open wizard offers the choice.
func (d *DashboardState) reviewSubagentDraft() (draft *SubagentDraft, profile string, ok bool) {
	switch mode := d.mode.(type) {
	case ModeNew:
		profile, ok := mode.selectedProfileID(d)
		if !ok {
			return nil, "", false
		}
		return mode.subagents, profile, true
	case ModeResume:
		if mode.step != WizardStepReview || !mode.subagentChoiceApplies(d) {
			return nil, "", false
		}
		return mode.subagents, mode.destinationProfile(d), true
	}
	return nil, "", false
}

func (d *DashboardState) takeSubagentDiscovery() DashboardAction {
	_, creating := d.mode.(ModeNew)
	var remembered *core.SubagentPolicy
	if wizard, ok := d.mode.(ModeNew); ok {
		profile, ok := wizard.selectedProfileID(d)
		if !ok {
			return nil
		}
		if policy, found := d.state.Get().RememberedSubagentPolicyFor(profile); found {
			remembered = &policy
		}
	}
	config := d.config.Get()
	oracleChoices := d.state.Get().LastOracleChoices
	draft, profile, ok := d.reviewSubagentDraft()
	if !ok {
		return nil
	}
	rememberedOracle, found := oracleChoices[profile]
	if !found {
		rememberedOracle = true
	}
	if creating {
		draft.refreshCreate(profile, config, remembered, rememberedOracle)
	} else {
		draft.updateChoices(profile, config)
	}
	return nil
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
	var stored core.SubagentPolicy
	if session := dashboard.SessionRecord(w.sessionID); session != nil && session.Subagents != nil {
		stored = *session.Subagents
	}
	selected := w.subagents.selected()
	shown := w.subagents.shown(&stored)
	if !w.subagentChoiceApplies(dashboard) || subagentPolicyEqual(&selected, &shown) {
		return nil
	}
	return &selected
}
