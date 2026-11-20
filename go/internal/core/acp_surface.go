// Port of mj-core/src/acp/surface.rs.

// Normalized session controls exposed by an ACP agent.
//
// ACP agents can advertise the same user-facing control through configuration
// options, legacy session modes, or provider extensions. This module is the
// single place where Hel turns those protocol surfaces into chat semantics.

package core

import (
	"slices"

	"github.com/BrokkAi/acp-go/schema"
)

const fastModeConfigID = "fast-mode"
const fastModeOn = "on"
const fastModeOff = "off"

// PlanControl is the ACP call that turns plan mode on or off.
type PlanControl interface {
	isPlanControl()
}

type PlanControlSetConfig struct {
	Key   string
	Value string
}

type PlanControlSetSessionMode struct {
	ModeID string
}

// PlanControlRestoreExecutionMode: the worker restores the mode that was
// active before Plan began.
type PlanControlRestoreExecutionMode struct{}

func (PlanControlSetConfig) isPlanControl()            {}
func (PlanControlSetSessionMode) isPlanControl()       {}
func (PlanControlRestoreExecutionMode) isPlanControl() {}

type PlanControlError int

const (
	PlanControlErrorCodexIncompatible PlanControlError = iota
	PlanControlErrorGrokIncompatible
	PlanControlErrorIncompatible
)

// AcpSessionSurface is the ACP controls and commands available to one live
// session.
type AcpSessionSurface struct {
	harnessKind           *HarnessKind
	configOptions         []schema.SessionConfigOption
	sessionModes          *schema.SessionModeState
	currentMode           *string
	agentCommands         []schema.AvailableCommand
	currentModel          *string
	currentEffort         *string
	planModeChangePending bool
}

// AcpSessionSurfaceFromConfiguration builds a surface from projected
// configuration values (decoded JSON values).
func AcpSessionSurfaceFromConfiguration(configuration map[string]any) AcpSessionSurface {
	panic("unported: mj-core/src/acp/surface.rs AcpSessionSurface::from_configuration")
}

func (s *AcpSessionSurface) HarnessKind() *HarnessKind {
	return s.harnessKind
}

func (s *AcpSessionSurface) SetHarnessKind(harnessKind HarnessKind) {
	s.harnessKind = &harnessKind
	s.syncPlanMode()
}

func (s *AcpSessionSurface) SetConfigOptions(options []schema.SessionConfigOption) {
	s.configOptions = slices.Clone(options)
	s.currentModel = ConfigCurrentValue(options, "model")
	s.currentEffort = ConfigCurrentValue(options, "effort")
	s.syncPlanMode()
}

func (s *AcpSessionSurface) ConfigOptions() []schema.SessionConfigOption {
	return s.configOptions
}

func (s *AcpSessionSurface) SetSessionModes(modes *schema.SessionModeState) {
	s.sessionModes = modes
	s.syncPlanMode()
}

func (s *AcpSessionSurface) ApplyCurrentModeUpdate(mode string) {
	panic("unported: mj-core/src/acp/surface.rs AcpSessionSurface::apply_current_mode_update")
}

func (s *AcpSessionSurface) ApplyProjectedConfiguration(configuration map[string]any) {
	panic("unported: mj-core/src/acp/surface.rs AcpSessionSurface::apply_projected_configuration")
}

func (s *AcpSessionSurface) SetAgentCommands(commands []schema.AvailableCommand) {
	s.agentCommands = commands
}

func (s *AcpSessionSurface) AgentCommands() []schema.AvailableCommand {
	return s.agentCommands
}

func (s *AcpSessionSurface) AdvertisesCommand(name string) bool {
	return slices.ContainsFunc(s.agentCommands, func(command schema.AvailableCommand) bool {
		return command.Name == name
	})
}

// ForwardsPlanCommand reports whether `/plan` goes to Muse as prompt text,
// for its `plan` skill.
//
// muse-acp 0.10 offers Plan on its `mode` selector, and `/plan` then controls
// that mode like any other harness's. Container images built before it carry
// an adapter without Plan; there `/plan` stays Muse's own skill, which is
// "planning guidance, not a host-enforced mode", so none of mj's plan-mode
// state engages.
func (s *AcpSessionSurface) ForwardsPlanCommand() bool {
	return s.harnessKind != nil && *s.harnessKind == HarnessKindMuse &&
		s.AdvertisesCommand("plan") &&
		!s.SupportsPlanMode()
}

func (s *AcpSessionSurface) CurrentModel() (string, bool) {
	if s.currentModel == nil {
		return "", false
	}
	return *s.currentModel, true
}

func (s *AcpSessionSurface) CurrentEffort() (string, bool) {
	if s.currentEffort == nil {
		return "", false
	}
	return *s.currentEffort, true
}

func (s *AcpSessionSurface) SupportsFastMode() bool {
	return slices.ContainsFunc(s.configOptions, func(option schema.SessionConfigOption) bool {
		return string(option.ID) == fastModeConfigID &&
			SelectContains(&option, fastModeOn) &&
			SelectContains(&option, fastModeOff)
	})
}

func (s *AcpSessionSurface) FastModeActive() bool {
	current := ConfigCurrentValue(s.configOptions, fastModeConfigID)
	return s.SupportsFastMode() && current != nil && *current == fastModeOn
}

func (s *AcpSessionSurface) CurrentMode() (string, bool) {
	if s.currentMode == nil {
		return "", false
	}
	return *s.currentMode, true
}

func (s *AcpSessionSurface) BeginPlanModeChange(active bool) {
	s.planModeChangePending = true
	mode := s.executionModeID()
	if active {
		mode = "plan"
	}
	s.currentMode = &mode
}

func (s *AcpSessionSurface) FinishPlanModeChange(active bool) {
	panic("unported: mj-core/src/acp/surface.rs AcpSessionSurface::finish_plan_mode_change")
}

// executionModeID is the mode a harness returns to when Plan mode ends.
// OpenCode names its normal mode `build` and dsh-acp names it `agent`; every
// other harness Hel drives with a plan pair names it `default`.
func (s *AcpSessionSurface) executionModeID() string {
	if s.harnessKind != nil {
		switch *s.harnessKind {
		case HarnessKindOpenCode:
			return "build"
		case HarnessKindDsh:
			return "agent"
		}
	}
	return "default"
}

func (s *AcpSessionSurface) SupportsPlanMode() bool {
	_, failure := s.PlanControl(true)
	return failure == nil
}

func (s *AcpSessionSurface) PlanModeActive() bool {
	mode, ok := s.CurrentMode()
	return s.SupportsPlanMode() && ok && mode == "plan"
}

// PlanControl returns the call that turns plan mode on or off, or a non-nil
// error kind saying why this harness cannot.
func (s *AcpSessionSurface) PlanControl(active bool) (PlanControl, *PlanControlError) {
	panic("unported: mj-core/src/acp/surface.rs AcpSessionSurface::plan_control")
}

func (s *AcpSessionSurface) syncPlanMode() {
	panic("unported: mj-core/src/acp/surface.rs AcpSessionSurface::sync_plan_mode")
}

func (s *AcpSessionSurface) modeConfigKey() string {
	if s.harnessKind != nil && *s.harnessKind == HarnessKindCodex {
		return "collaboration_mode"
	}
	return "mode"
}

func (s *AcpSessionSurface) advertisedPlanModes() bool {
	if s.sessionModes == nil {
		return false
	}
	for _, desired := range []string{"plan", "default"} {
		if !slices.ContainsFunc(s.sessionModes.AvailableModes, func(mode schema.SessionMode) bool {
			return string(mode.ID) == desired
		}) {
			return false
		}
	}
	return true
}

func (s *AcpSessionSurface) configHasPlanPair(key string) bool {
	option := FindSessionConfigOption(s.configOptions, key)
	return option != nil && SelectContains(option, "plan") && SelectContains(option, "default")
}

func (s *AcpSessionSurface) exactConfigHasPlanPair(key string) bool {
	return s.exactConfigHasPair(key, "default")
}

func (s *AcpSessionSurface) exactConfigHasPair(key, other string) bool {
	return slices.ContainsFunc(s.configOptions, func(option schema.SessionConfigOption) bool {
		return string(option.ID) == key &&
			SelectContains(&option, "plan") &&
			SelectContains(&option, other)
	})
}

func ConfigCurrentValue(options []schema.SessionConfigOption, key string) *string {
	option := FindSessionConfigOption(options, key)
	if option == nil || option.Select == nil {
		return nil
	}
	value := string(option.Select.CurrentValue)
	return &value
}
