// Port of mj-core/src/config/keys.rs.
//
// Configurable key bindings: a tmux-style prefix key plus one field per
// action.
//
// The key types here are terminal-agnostic on purpose. This package does not
// depend on a terminal library, so the terminal packages translate their own
// key events into a KeyCombo and ask Keybinds what that combination means.
//
// The string grammar matches herdr's, so a line can be copied between the two
// configuration files: tokens joined by `+`, an optional `prefix+` marker for
// a binding that only fires after the prefix key, and `1..9` for the numbered
// workspace range.

package core

import (
	"encoding/json"
	"fmt"
	"reflect"
	"runtime"
	"strings"
)

// DefaultPrefix is the default prefix key, spelled as it is written in
// config.toml.
const DefaultPrefix = "ctrl+b"

// KeyName is a key, without any modifier state. Its variants are comparable,
// so a KeyName works as a map key.
//
// `shift+tab` is KeyNameTab carrying `shift`; there is no separate back-tab
// name, so the terminal bridge maps its BackTab onto it.
type KeyName interface {
	isKeyName()
}

type KeyNameChar struct{ Value rune }
type KeyNameEnter struct{}
type KeyNameEsc struct{}
type KeyNameTab struct{}
type KeyNameBackspace struct{}
type KeyNameDelete struct{}
type KeyNameInsert struct{}
type KeyNameHome struct{}
type KeyNameEnd struct{}
type KeyNamePageUp struct{}
type KeyNamePageDown struct{}
type KeyNameLeft struct{}
type KeyNameRight struct{}
type KeyNameUp struct{}
type KeyNameDown struct{}
type KeyNameF struct{ Value uint8 }

func (KeyNameChar) isKeyName()      {}
func (KeyNameEnter) isKeyName()     {}
func (KeyNameEsc) isKeyName()       {}
func (KeyNameTab) isKeyName()       {}
func (KeyNameBackspace) isKeyName() {}
func (KeyNameDelete) isKeyName()    {}
func (KeyNameInsert) isKeyName()    {}
func (KeyNameHome) isKeyName()      {}
func (KeyNameEnd) isKeyName()       {}
func (KeyNamePageUp) isKeyName()    {}
func (KeyNamePageDown) isKeyName()  {}
func (KeyNameLeft) isKeyName()      {}
func (KeyNameRight) isKeyName()     {}
func (KeyNameUp) isKeyName()        {}
func (KeyNameDown) isKeyName()      {}
func (KeyNameF) isKeyName()         {}

// Modifiers is the modifier keys held down with a KeyName.
type Modifiers struct {
	Ctrl  bool
	Alt   bool
	Shift bool
	Super bool
}

var (
	ModifiersNone  = Modifiers{}
	ModifiersCtrl  = Modifiers{Ctrl: true}
	ModifiersShift = Modifiers{Shift: true}
)

func (m Modifiers) IsEmpty() bool { return !m.Ctrl && !m.Alt && !m.Shift && !m.Super }

// shiftOnly is true when nothing but `shift` is held.
func (m Modifiers) shiftOnly() bool { return !m.Ctrl && !m.Alt && !m.Super }

// Or is BitOr for Modifiers.
func (m Modifiers) Or(other Modifiers) Modifiers {
	return Modifiers{
		Ctrl:  m.Ctrl || other.Ctrl,
		Alt:   m.Alt || other.Alt,
		Shift: m.Shift || other.Shift,
		Super: m.Super || other.Super,
	}
}

// KeyCombo is one key together with the modifiers held with it. It is
// comparable, so it works as a map key.
type KeyCombo struct {
	Name      KeyName
	Modifiers Modifiers
}

func NewKeyCombo(name KeyName, modifiers Modifiers) KeyCombo {
	return KeyCombo{Name: name, Modifiers: modifiers}
}

func KeyComboPlain(name KeyName) KeyCombo { return NewKeyCombo(name, ModifiersNone) }

func (k KeyCombo) String() string { return FormatKeyCombo(k) }

func parseModifierToken(token string) (Modifiers, bool) {
	var modifiers Modifiers
	switch strings.ToLower(token) {
	case "ctrl", "control":
		modifiers.Ctrl = true
	case "alt", "option", "meta":
		modifiers.Alt = true
	case "shift":
		modifiers.Shift = true
	case "cmd", "command", "super":
		modifiers.Super = true
	default:
		return Modifiers{}, false
	}
	return modifiers, true
}

func parseKeyName(token string) (KeyName, bool) {
	panic("unported: mj-core/src/config/keys.rs parse_key_name")
}

// ParseKeyCombo parses one key combination, such as `ctrl+b`, `shift+tab`,
// `f5` or `?`.
func ParseKeyCombo(text string) (KeyCombo, bool) {
	panic("unported: mj-core/src/config/keys.rs parse_key_combo")
}

// NormalizeKeyCombo folds the two spellings a terminal may deliver into one.
//
// An uppercase letter becomes the lowercase letter plus `shift`, and `shift`
// on any other character is dropped because the terminal already delivered
// the shifted symbol itself.
func NormalizeKeyCombo(combo KeyCombo) KeyCombo {
	panic("unported: mj-core/src/config/keys.rs normalize_key_combo")
}

func superLabel() string {
	if runtime.GOOS == "darwin" {
		return "cmd"
	}
	return "super"
}

func keyNameLabel(name KeyName) string {
	panic("unported: mj-core/src/config/keys.rs key_name_label")
}

// FormatKeyCombo spells a combination the way config.toml does, so that
// FormatKeyCombo(ParseKeyCombo(s)) returns s for canonical strings.
func FormatKeyCombo(combo KeyCombo) string {
	var label strings.Builder
	if combo.Modifiers.Ctrl {
		label.WriteString("ctrl+")
	}
	if combo.Modifiers.Alt {
		label.WriteString("alt+")
	}
	if combo.Modifiers.Shift {
		label.WriteString("shift+")
	}
	if combo.Modifiers.Super {
		label.WriteString(superLabel())
		label.WriteByte('+')
	}
	label.WriteString(keyNameLabel(combo.Name))
	return label.String()
}

// isUnmodifiedPrintable is a printable character with no modifier but
// `shift`: plain typing.
func isUnmodifiedPrintable(combo KeyCombo) bool {
	panic("unported: mj-core/src/config/keys.rs is_unmodified_printable")
}

// BindingConfig is one binding value: a single key string, or several
// alternatives (serde untagged: a string or an array of strings).
type BindingConfig struct {
	// One is the single key string when IsMany is false.
	One string
	// Many is the alternatives when IsMany is true.
	Many   []string
	IsMany bool
}

// BindingConfigFrom is From<&str> for BindingConfig.
func BindingConfigFrom(value string) BindingConfig { return BindingConfig{One: value} }

// BindingConfigFromMany is From<[&str; N]> for BindingConfig.
func BindingConfigFromMany(values ...string) BindingConfig {
	return BindingConfig{Many: append([]string{}, values...), IsMany: true}
}

// Entries is the key strings that carry a binding; an empty string is
// "unbound".
func (b BindingConfig) Entries() []string {
	raw := []string{b.One}
	if b.IsMany {
		raw = b.Many
	}
	entries := []string{}
	for _, entry := range raw {
		if trimmed := strings.TrimSpace(entry); trimmed != "" {
			entries = append(entries, trimmed)
		}
	}
	return entries
}

func (b BindingConfig) MarshalJSON() ([]byte, error) {
	if b.IsMany {
		many := b.Many
		if many == nil {
			many = []string{}
		}
		return json.Marshal(many)
	}
	return json.Marshal(b.One)
}

func (b *BindingConfig) UnmarshalJSON(data []byte) error {
	var one string
	if json.Unmarshal(data, &one) == nil {
		*b = BindingConfig{One: one}
		return nil
	}
	var many []string
	if err := json.Unmarshal(data, &many); err == nil && many != nil {
		*b = BindingConfig{Many: many, IsMany: true}
		return nil
	}
	return fmt.Errorf("data did not match any variant of untagged enum BindingConfig")
}

// KeyAction is one named thing a key can be bound to, in declaration order.
type KeyAction int

const (
	KeyActionHelp KeyAction = iota
	KeyActionOpenSettings
	KeyActionDetach
	KeyActionNewSession
	KeyActionResume
	KeyActionWorkspaceManager
	KeyActionFocusWorkspaces
	KeyActionNextWorkspace
	KeyActionPreviousWorkspace
	KeyActionSwitchWorkspace
	KeyActionNextPane
	KeyActionPreviousPane
	KeyActionPaneSize
	KeyActionPanePreset
	KeyActionRefresh
	KeyActionPalette
	KeyActionCancelOperation
	KeyActionMarkAllRead
	KeyActionNextAttention
	KeyActionPreviousAttention
	KeyActionWebViewer
	KeyActionRenameSession
	KeyActionToggleTranscriptRendering
	KeyActionToggleDictation
	KeyActionChangedFiles
	KeyActionSplitVertical
	KeyActionSplitHorizontal
	KeyActionClosePane
	KeyActionFocusPaneLeft
	KeyActionFocusPaneDown
	KeyActionFocusPaneUp
	KeyActionFocusPaneRight
	KeyActionZoom
	KeyActionLastPane
	KeyActionRestartSession
	KeyActionMoveSession
	KeyActionDestroySession
	KeyActionContainerSettings
	KeyActionManageProfiles
	KeyActionManageTargets
	KeyActionManageMachines
	KeyActionRestartDaemon
	KeyActionNoticeLog
	KeyActionChangeGoSetup
	KeyActionCycleSpinner
	KeyActionResizeMode
	KeyActionSwapPaneLeft
	KeyActionSwapPaneDown
	KeyActionSwapPaneUp
	KeyActionSwapPaneRight
	KeyActionSuspendSession
	KeyActionRenameWorkspace
	KeyActionCloseWorkspace
	KeyActionResizePaneLeft
	KeyActionResizePaneDown
	KeyActionResizePaneUp
	KeyActionResizePaneRight
	KeyActionOpenSubagents
	KeyActionSessionActions
	KeyActionInterruptTurn
)

// KeyActionAll is every action, in declaration order.
var KeyActionAll = []KeyAction{
	KeyActionHelp,
	KeyActionOpenSettings,
	KeyActionDetach,
	KeyActionNewSession,
	KeyActionResume,
	KeyActionWorkspaceManager,
	KeyActionFocusWorkspaces,
	KeyActionNextWorkspace,
	KeyActionPreviousWorkspace,
	KeyActionSwitchWorkspace,
	KeyActionNextPane,
	KeyActionPreviousPane,
	KeyActionPaneSize,
	KeyActionPanePreset,
	KeyActionRefresh,
	KeyActionPalette,
	KeyActionCancelOperation,
	KeyActionMarkAllRead,
	KeyActionNextAttention,
	KeyActionPreviousAttention,
	KeyActionWebViewer,
	KeyActionRenameSession,
	KeyActionToggleTranscriptRendering,
	KeyActionToggleDictation,
	KeyActionChangedFiles,
	KeyActionSplitVertical,
	KeyActionSplitHorizontal,
	KeyActionClosePane,
	KeyActionFocusPaneLeft,
	KeyActionFocusPaneDown,
	KeyActionFocusPaneUp,
	KeyActionFocusPaneRight,
	KeyActionZoom,
	KeyActionLastPane,
	KeyActionRestartSession,
	KeyActionMoveSession,
	KeyActionDestroySession,
	KeyActionContainerSettings,
	KeyActionManageProfiles,
	KeyActionManageTargets,
	KeyActionManageMachines,
	KeyActionRestartDaemon,
	KeyActionNoticeLog,
	KeyActionChangeGoSetup,
	KeyActionCycleSpinner,
	KeyActionResizeMode,
	KeyActionSwapPaneLeft,
	KeyActionSwapPaneDown,
	KeyActionSwapPaneUp,
	KeyActionSwapPaneRight,
	KeyActionSuspendSession,
	KeyActionRenameWorkspace,
	KeyActionCloseWorkspace,
	KeyActionResizePaneLeft,
	KeyActionResizePaneDown,
	KeyActionResizePaneUp,
	KeyActionResizePaneRight,
	KeyActionOpenSubagents,
	KeyActionSessionActions,
	KeyActionInterruptTurn,
}

// FieldName is the [keys] field that binds this action.
func (a KeyAction) FieldName() string {
	switch a {
	case KeyActionHelp:
		return "help"
	case KeyActionOpenSettings:
		return "settings"
	case KeyActionDetach:
		return "detach"
	case KeyActionNewSession:
		return "new_session"
	case KeyActionResume:
		return "resume"
	case KeyActionWorkspaceManager:
		return "workspace_manager"
	case KeyActionFocusWorkspaces:
		return "focus_workspaces"
	case KeyActionNextWorkspace:
		return "next_workspace"
	case KeyActionPreviousWorkspace:
		return "previous_workspace"
	case KeyActionSwitchWorkspace:
		return "switch_workspace"
	case KeyActionNextPane:
		return "next_pane"
	case KeyActionPreviousPane:
		return "previous_pane"
	case KeyActionPaneSize:
		return "pane_size"
	case KeyActionPanePreset:
		return "pane_preset"
	case KeyActionRefresh:
		return "refresh"
	case KeyActionPalette:
		return "palette"
	case KeyActionCancelOperation:
		return "cancel_operation"
	case KeyActionMarkAllRead:
		return "mark_all_read"
	case KeyActionNextAttention:
		return "next_attention"
	case KeyActionPreviousAttention:
		return "previous_attention"
	case KeyActionWebViewer:
		return "web_viewer"
	case KeyActionRenameSession:
		return "rename_session"
	case KeyActionToggleTranscriptRendering:
		return "toggle_transcript_rendering"
	case KeyActionToggleDictation:
		return "toggle_dictation"
	case KeyActionChangedFiles:
		return "changed_files"
	case KeyActionSplitVertical:
		return "split_vertical"
	case KeyActionSplitHorizontal:
		return "split_horizontal"
	case KeyActionClosePane:
		return "close_pane"
	case KeyActionFocusPaneLeft:
		return "focus_pane_left"
	case KeyActionFocusPaneDown:
		return "focus_pane_down"
	case KeyActionFocusPaneUp:
		return "focus_pane_up"
	case KeyActionFocusPaneRight:
		return "focus_pane_right"
	case KeyActionZoom:
		return "zoom"
	case KeyActionLastPane:
		return "last_pane"
	case KeyActionRestartSession:
		return "restart_session"
	case KeyActionMoveSession:
		return "move_session"
	case KeyActionDestroySession:
		return "destroy_session"
	case KeyActionContainerSettings:
		return "container_settings"
	case KeyActionManageProfiles:
		return "manage_profiles"
	case KeyActionManageTargets:
		return "manage_targets"
	case KeyActionManageMachines:
		return "manage_machines"
	case KeyActionRestartDaemon:
		return "restart_daemon"
	case KeyActionNoticeLog:
		return "notice_log"
	case KeyActionChangeGoSetup:
		return "change_go_setup"
	case KeyActionCycleSpinner:
		return "cycle_spinner"
	case KeyActionResizeMode:
		return "resize_mode"
	case KeyActionSwapPaneLeft:
		return "swap_pane_left"
	case KeyActionSwapPaneDown:
		return "swap_pane_down"
	case KeyActionSwapPaneUp:
		return "swap_pane_up"
	case KeyActionSwapPaneRight:
		return "swap_pane_right"
	case KeyActionSuspendSession:
		return "suspend_session"
	case KeyActionRenameWorkspace:
		return "rename_workspace"
	case KeyActionCloseWorkspace:
		return "close_workspace"
	case KeyActionResizePaneLeft:
		return "resize_pane_left"
	case KeyActionResizePaneDown:
		return "resize_pane_down"
	case KeyActionResizePaneUp:
		return "resize_pane_up"
	case KeyActionResizePaneRight:
		return "resize_pane_right"
	case KeyActionOpenSubagents:
		return "open_subagents"
	case KeyActionSessionActions:
		return "session_actions"
	case KeyActionInterruptTurn:
		return "interrupt_turn"
	}
	panic(fmt.Sprintf("unknown key action %d", int(a)))
}

// KeysConfig is the [keys] section of config.toml.
type KeysConfig struct {
	Prefix                    string        `json:"prefix" toml:"prefix"`
	Help                      BindingConfig `json:"help" toml:"help"`
	Settings                  BindingConfig `json:"settings" toml:"settings"`
	Detach                    BindingConfig `json:"detach" toml:"detach"`
	NewSession                BindingConfig `json:"new_session" toml:"new_session"`
	Resume                    BindingConfig `json:"resume" toml:"resume"`
	WorkspaceManager          BindingConfig `json:"workspace_manager" toml:"workspace_manager"`
	FocusWorkspaces           BindingConfig `json:"focus_workspaces" toml:"focus_workspaces"`
	NextWorkspace             BindingConfig `json:"next_workspace" toml:"next_workspace"`
	PreviousWorkspace         BindingConfig `json:"previous_workspace" toml:"previous_workspace"`
	SwitchWorkspace           BindingConfig `json:"switch_workspace" toml:"switch_workspace"`
	NextPane                  BindingConfig `json:"next_pane" toml:"next_pane"`
	PreviousPane              BindingConfig `json:"previous_pane" toml:"previous_pane"`
	PaneSize                  BindingConfig `json:"pane_size" toml:"pane_size"`
	PanePreset                BindingConfig `json:"pane_preset" toml:"pane_preset"`
	Refresh                   BindingConfig `json:"refresh" toml:"refresh"`
	Palette                   BindingConfig `json:"palette" toml:"palette"`
	CancelOperation           BindingConfig `json:"cancel_operation" toml:"cancel_operation"`
	MarkAllRead               BindingConfig `json:"mark_all_read" toml:"mark_all_read"`
	NextAttention             BindingConfig `json:"next_attention" toml:"next_attention"`
	PreviousAttention         BindingConfig `json:"previous_attention" toml:"previous_attention"`
	WebViewer                 BindingConfig `json:"web_viewer" toml:"web_viewer"`
	RenameSession             BindingConfig `json:"rename_session" toml:"rename_session"`
	ToggleTranscriptRendering BindingConfig `json:"toggle_transcript_rendering" toml:"toggle_transcript_rendering"`
	ToggleDictation           BindingConfig `json:"toggle_dictation" toml:"toggle_dictation"`
	ChangedFiles              BindingConfig `json:"changed_files" toml:"changed_files"`
	SplitVertical             BindingConfig `json:"split_vertical" toml:"split_vertical"`
	SplitHorizontal           BindingConfig `json:"split_horizontal" toml:"split_horizontal"`
	ClosePane                 BindingConfig `json:"close_pane" toml:"close_pane"`
	FocusPaneLeft             BindingConfig `json:"focus_pane_left" toml:"focus_pane_left"`
	FocusPaneDown             BindingConfig `json:"focus_pane_down" toml:"focus_pane_down"`
	FocusPaneUp               BindingConfig `json:"focus_pane_up" toml:"focus_pane_up"`
	FocusPaneRight            BindingConfig `json:"focus_pane_right" toml:"focus_pane_right"`
	Zoom                      BindingConfig `json:"zoom" toml:"zoom"`
	LastPane                  BindingConfig `json:"last_pane" toml:"last_pane"`
	RestartSession            BindingConfig `json:"restart_session" toml:"restart_session"`
	MoveSession               BindingConfig `json:"move_session" toml:"move_session"`
	DestroySession            BindingConfig `json:"destroy_session" toml:"destroy_session"`
	ContainerSettings         BindingConfig `json:"container_settings" toml:"container_settings"`
	ManageProfiles            BindingConfig `json:"manage_profiles" toml:"manage_profiles"`
	ManageTargets             BindingConfig `json:"manage_targets" toml:"manage_targets"`
	ManageMachines            BindingConfig `json:"manage_machines" toml:"manage_machines"`
	RestartDaemon             BindingConfig `json:"restart_daemon" toml:"restart_daemon"`
	NoticeLog                 BindingConfig `json:"notice_log" toml:"notice_log"`
	ChangeGoSetup             BindingConfig `json:"change_go_setup" toml:"change_go_setup"`
	CycleSpinner              BindingConfig `json:"cycle_spinner" toml:"cycle_spinner"`
	ResizeMode                BindingConfig `json:"resize_mode" toml:"resize_mode"`
	SwapPaneLeft              BindingConfig `json:"swap_pane_left" toml:"swap_pane_left"`
	SwapPaneDown              BindingConfig `json:"swap_pane_down" toml:"swap_pane_down"`
	SwapPaneUp                BindingConfig `json:"swap_pane_up" toml:"swap_pane_up"`
	SwapPaneRight             BindingConfig `json:"swap_pane_right" toml:"swap_pane_right"`
	SuspendSession            BindingConfig `json:"suspend_session" toml:"suspend_session"`
	RenameWorkspace           BindingConfig `json:"rename_workspace" toml:"rename_workspace"`
	CloseWorkspace            BindingConfig `json:"close_workspace" toml:"close_workspace"`
	ResizePaneLeft            BindingConfig `json:"resize_pane_left" toml:"resize_pane_left"`
	ResizePaneDown            BindingConfig `json:"resize_pane_down" toml:"resize_pane_down"`
	ResizePaneUp              BindingConfig `json:"resize_pane_up" toml:"resize_pane_up"`
	ResizePaneRight           BindingConfig `json:"resize_pane_right" toml:"resize_pane_right"`
	OpenSubagents             BindingConfig `json:"open_subagents" toml:"open_subagents"`
	SessionActions            BindingConfig `json:"session_actions" toml:"session_actions"`
	InterruptTurn             BindingConfig `json:"interrupt_turn" toml:"interrupt_turn"`
}

// DefaultKeysConfig is KeysConfig's Default.
func DefaultKeysConfig() KeysConfig {
	return KeysConfig{
		Prefix:                    DefaultPrefix,
		Help:                      BindingConfigFrom("prefix+?"),
		Settings:                  BindingConfigFrom("prefix+s"),
		Detach:                    BindingConfigFrom("prefix+q"),
		NewSession:                BindingConfigFrom("prefix+c"),
		Resume:                    BindingConfigFrom("prefix+g"),
		WorkspaceManager:          BindingConfigFrom("prefix+shift+n"),
		FocusWorkspaces:           BindingConfigFrom("prefix+w"),
		NextWorkspace:             BindingConfigFrom("prefix+n"),
		PreviousWorkspace:         BindingConfigFrom("prefix+p"),
		SwitchWorkspace:           BindingConfigFrom("prefix+1..9"),
		NextPane:                  BindingConfigFrom("prefix+tab"),
		PreviousPane:              BindingConfigFrom("prefix+shift+tab"),
		PaneSize:                  BindingConfigFrom("prefix+shift+z"),
		PanePreset:                BindingConfigFrom("prefix+b"),
		Refresh:                   BindingConfigFrom("prefix+shift+r"),
		Palette:                   BindingConfigFrom("prefix+:"),
		CancelOperation:           BindingConfigFrom("prefix+shift+c"),
		MarkAllRead:               BindingConfigFrom("prefix+a"),
		NextAttention:             BindingConfigFrom("prefix+o"),
		PreviousAttention:         BindingConfigFrom("prefix+shift+o"),
		WebViewer:                 BindingConfigFrom("prefix+u"),
		RenameSession:             BindingConfigFrom("prefix+shift+t"),
		ToggleTranscriptRendering: BindingConfigFrom("prefix+t"),
		ToggleDictation:           BindingConfigFrom("prefix+m"),
		ChangedFiles:              BindingConfigFrom("prefix+d"),
		SplitVertical:             BindingConfigFrom("prefix+v"),
		SplitHorizontal:           BindingConfigFrom("prefix+-"),
		ClosePane:                 BindingConfigFrom("prefix+x"),
		FocusPaneLeft:             BindingConfigFrom("prefix+h"),
		FocusPaneDown:             BindingConfigFrom("prefix+j"),
		FocusPaneUp:               BindingConfigFrom("prefix+k"),
		FocusPaneRight:            BindingConfigFrom("prefix+l"),
		Zoom:                      BindingConfigFrom("prefix+z"),
		LastPane:                  BindingConfigFrom("prefix+;"),
		RestartSession:            BindingConfigFrom(""),
		MoveSession:               BindingConfigFrom(""),
		DestroySession:            BindingConfigFrom(""),
		ContainerSettings:         BindingConfigFrom(""),
		ManageProfiles:            BindingConfigFrom(""),
		ManageTargets:             BindingConfigFrom(""),
		ManageMachines:            BindingConfigFrom(""),
		RestartDaemon:             BindingConfigFrom(""),
		NoticeLog:                 BindingConfigFrom(""),
		ChangeGoSetup:             BindingConfigFrom(""),
		CycleSpinner:              BindingConfigFrom(""),
		ResizeMode:                BindingConfigFrom("prefix+r"),
		SwapPaneLeft:              BindingConfigFrom("prefix+shift+h"),
		SwapPaneDown:              BindingConfigFrom("prefix+shift+j"),
		SwapPaneUp:                BindingConfigFrom("prefix+shift+k"),
		SwapPaneRight:             BindingConfigFrom("prefix+shift+l"),
		SuspendSession:            BindingConfigFrom("prefix+shift+x"),
		RenameWorkspace:           BindingConfigFrom("prefix+shift+w"),
		CloseWorkspace:            BindingConfigFrom("prefix+shift+d"),
		ResizePaneLeft:            BindingConfigFrom(""),
		ResizePaneDown:            BindingConfigFrom(""),
		ResizePaneUp:              BindingConfigFrom(""),
		ResizePaneRight:           BindingConfigFrom(""),
		OpenSubagents:             BindingConfigFrom("prefix+shift+a"),
		SessionActions:            BindingConfigFrom("prefix+."),
		InterruptTurn:             BindingConfigFrom("prefix+i"),
	}
}

// fieldPointer is the configured value for one action, by address.
func (k *KeysConfig) fieldPointer(action KeyAction) *BindingConfig {
	switch action {
	case KeyActionHelp:
		return &k.Help
	case KeyActionOpenSettings:
		return &k.Settings
	case KeyActionDetach:
		return &k.Detach
	case KeyActionNewSession:
		return &k.NewSession
	case KeyActionResume:
		return &k.Resume
	case KeyActionWorkspaceManager:
		return &k.WorkspaceManager
	case KeyActionFocusWorkspaces:
		return &k.FocusWorkspaces
	case KeyActionNextWorkspace:
		return &k.NextWorkspace
	case KeyActionPreviousWorkspace:
		return &k.PreviousWorkspace
	case KeyActionSwitchWorkspace:
		return &k.SwitchWorkspace
	case KeyActionNextPane:
		return &k.NextPane
	case KeyActionPreviousPane:
		return &k.PreviousPane
	case KeyActionPaneSize:
		return &k.PaneSize
	case KeyActionPanePreset:
		return &k.PanePreset
	case KeyActionRefresh:
		return &k.Refresh
	case KeyActionPalette:
		return &k.Palette
	case KeyActionCancelOperation:
		return &k.CancelOperation
	case KeyActionMarkAllRead:
		return &k.MarkAllRead
	case KeyActionNextAttention:
		return &k.NextAttention
	case KeyActionPreviousAttention:
		return &k.PreviousAttention
	case KeyActionWebViewer:
		return &k.WebViewer
	case KeyActionRenameSession:
		return &k.RenameSession
	case KeyActionToggleTranscriptRendering:
		return &k.ToggleTranscriptRendering
	case KeyActionToggleDictation:
		return &k.ToggleDictation
	case KeyActionChangedFiles:
		return &k.ChangedFiles
	case KeyActionSplitVertical:
		return &k.SplitVertical
	case KeyActionSplitHorizontal:
		return &k.SplitHorizontal
	case KeyActionClosePane:
		return &k.ClosePane
	case KeyActionFocusPaneLeft:
		return &k.FocusPaneLeft
	case KeyActionFocusPaneDown:
		return &k.FocusPaneDown
	case KeyActionFocusPaneUp:
		return &k.FocusPaneUp
	case KeyActionFocusPaneRight:
		return &k.FocusPaneRight
	case KeyActionZoom:
		return &k.Zoom
	case KeyActionLastPane:
		return &k.LastPane
	case KeyActionRestartSession:
		return &k.RestartSession
	case KeyActionMoveSession:
		return &k.MoveSession
	case KeyActionDestroySession:
		return &k.DestroySession
	case KeyActionContainerSettings:
		return &k.ContainerSettings
	case KeyActionManageProfiles:
		return &k.ManageProfiles
	case KeyActionManageTargets:
		return &k.ManageTargets
	case KeyActionManageMachines:
		return &k.ManageMachines
	case KeyActionRestartDaemon:
		return &k.RestartDaemon
	case KeyActionNoticeLog:
		return &k.NoticeLog
	case KeyActionChangeGoSetup:
		return &k.ChangeGoSetup
	case KeyActionCycleSpinner:
		return &k.CycleSpinner
	case KeyActionResizeMode:
		return &k.ResizeMode
	case KeyActionSwapPaneLeft:
		return &k.SwapPaneLeft
	case KeyActionSwapPaneDown:
		return &k.SwapPaneDown
	case KeyActionSwapPaneUp:
		return &k.SwapPaneUp
	case KeyActionSwapPaneRight:
		return &k.SwapPaneRight
	case KeyActionSuspendSession:
		return &k.SuspendSession
	case KeyActionRenameWorkspace:
		return &k.RenameWorkspace
	case KeyActionCloseWorkspace:
		return &k.CloseWorkspace
	case KeyActionResizePaneLeft:
		return &k.ResizePaneLeft
	case KeyActionResizePaneDown:
		return &k.ResizePaneDown
	case KeyActionResizePaneUp:
		return &k.ResizePaneUp
	case KeyActionResizePaneRight:
		return &k.ResizePaneRight
	case KeyActionOpenSubagents:
		return &k.OpenSubagents
	case KeyActionSessionActions:
		return &k.SessionActions
	case KeyActionInterruptTurn:
		return &k.InterruptTurn
	}
	panic(fmt.Sprintf("unknown key action %d", int(action)))
}

func (k *KeysConfig) UnmarshalJSON(data []byte) error {
	type plain KeysConfig
	value := plain(DefaultKeysConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*k = KeysConfig(value)
	return nil
}

func (k KeysConfig) IsDefault() bool { return reflect.DeepEqual(k, DefaultKeysConfig()) }

// Field is the configured value for one action.
func (k KeysConfig) Field(action KeyAction) BindingConfig { return *k.fieldPointer(action) }

// Trigger is whether a binding fires on its own or only after the prefix key.
type Trigger int

const (
	TriggerDirect Trigger = iota
	TriggerPrefix
)

// Binding is one resolved binding. Index carries the 0-based slot of a `1..9`
// range.
type Binding struct {
	Trigger Trigger
	Combo   KeyCombo
	Index   *int
}

// KeyMatch is what a pressed key means.
type KeyMatch struct {
	Action KeyAction
	Index  *int
}

// Keybinds is the bindings in force: the prefix key and every action's
// resolved keys.
type Keybinds struct {
	Prefix  KeyCombo
	actions map[KeyAction][]Binding
}

// Bindings is the keys bound to one action, in the order they were written.
func (k Keybinds) Bindings(action KeyAction) []Binding { return k.actions[action] }

// ResolveDirect is the action a key runs on its own, with no prefix pending.
func (k Keybinds) ResolveDirect(combo KeyCombo) (KeyMatch, bool) {
	return k.lookup(TriggerDirect, combo)
}

// ResolvePrefix is the action a key runs while the prefix is pending.
func (k Keybinds) ResolvePrefix(combo KeyCombo) (KeyMatch, bool) {
	return k.lookup(TriggerPrefix, combo)
}

// lookup searches actions in KeyAction order, as the Rust BTreeMap does.
func (k Keybinds) lookup(trigger Trigger, combo KeyCombo) (KeyMatch, bool) {
	combo = NormalizeKeyCombo(combo)
	for _, action := range KeyActionAll {
		for _, binding := range k.actions[action] {
			if binding.Trigger == trigger && binding.Combo == combo {
				return KeyMatch{Action: action, Index: binding.Index}, true
			}
		}
	}
	return KeyMatch{}, false
}

// Labels is the labels for the user interface: "ctrl+b c" for a prefix
// binding and the plain combination for a direct one.
func (k Keybinds) Labels(action KeyAction) []string {
	prefix := k.PrefixLabel()
	labels := []string{}
	for _, binding := range k.Bindings(action) {
		if binding.Trigger == TriggerPrefix {
			labels = append(labels, prefix+" "+FormatKeyCombo(binding.Combo))
		} else {
			labels = append(labels, FormatKeyCombo(binding.Combo))
		}
	}
	return labels
}

// PrefixRHSLabels is the keys that follow the prefix, without the prefix
// itself: "c".
func (k Keybinds) PrefixRHSLabels(action KeyAction) []string {
	labels := []string{}
	for _, binding := range k.Bindings(action) {
		if binding.Trigger == TriggerPrefix {
			labels = append(labels, FormatKeyCombo(binding.Combo))
		}
	}
	return labels
}

// PrefixLabel is the prefix key as it is written and displayed: "ctrl+b".
func (k Keybinds) PrefixLabel() string { return FormatKeyCombo(k.Prefix) }

// DefaultKeybinds is Keybinds' Default.
func DefaultKeybinds() Keybinds {
	keybinds, err := DefaultKeysConfig().Resolve()
	if err != nil {
		panic("the default key bindings resolve without conflicts: " + err.Error())
	}
	return keybinds
}

// SimpleKeybinds is the direct command map used by `mj simple`.
//
// No key arms a prefix. Common commands use one Ctrl chord. Less common pane
// and workspace commands add Alt so they do not replace the composer's
// Ctrl+H/J/K/M editing keys. Ctrl+B is never bound.
func SimpleKeybinds() Keybinds {
	panic("unported: mj-core/src/config/keys.rs simple_keybinds")
}

func keyError(field, raw string, reason any) error {
	return fmt.Errorf("keys.%s = \"%s\": %v", field, raw, reason)
}

// parseRangeModifiers is the modifiers of a `1..9` range form, such as `1..9`
// itself or `ctrl+1..9`.
func parseRangeModifiers(text string) (Modifiers, bool) {
	panic("unported: mj-core/src/config/keys.rs parse_range_modifiers")
}

// Resolve turns the configured strings into the bindings the interface uses.
//
// Every failure is a configuration error, except that a binding the user
// wrote silently displaces a default binding on the same key.
func (k KeysConfig) Resolve() (Keybinds, error) {
	panic("unported: mj-core/src/config/keys.rs KeysConfig::resolve")
}

// expandedCombo is one combination an entry stands for, with its range slot.
type expandedCombo struct {
	Combo KeyCombo
	Index *int
}

// expandEntry is the combinations one entry stands for: one, or the nine of a
// `1..9` range.
func expandEntry(action KeyAction, field, raw, body string) ([]expandedCombo, error) {
	panic("unported: mj-core/src/config/keys.rs expand_entry")
}

func checkCombo(field, raw string, trigger Trigger, combo, prefix KeyCombo) error {
	panic("unported: mj-core/src/config/keys.rs check_combo")
}
