// Port of mj-tui/src/keybinds.rs.

// The prefix-key router: what a key press means once the configured `[keys]`
// bindings are in force.
//
// mj follows tmux and herdr. One prefix key — `ctrl+b` unless the
// configuration says otherwise — arms a pending state, and the next key runs
// the command bound after the prefix. Pressing the prefix twice sends the
// literal key on to whatever has focus, so `Ctrl-B` still moves the
// composer's cursor back one character.
//
// The router runs in exactly one place per key press: the terminal event
// loop in mj-cli. Running it a second time further down would re-arm the
// prefix on the literal key the loop is forwarding.

package tui

import (
	"fmt"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// KeyRoute is what the router decided about one key press: [KeyRouteForward],
// [KeyRouteConsumed], or [KeyRouteCommand].
type KeyRoute interface {
	isKeyRoute()
}

// KeyRouteForward means nothing here claims the key; hand it to the panes
// or the composer.
type KeyRouteForward struct{}

// KeyRouteConsumed means the router answered the key itself and there is
// nothing more to do.
type KeyRouteConsumed struct{}

// KeyRouteCommand means the key runs a command. Index carries the slot of a
// numbered key.
type KeyRouteCommand struct {
	ID    CommandId
	Index *int
}

func (KeyRouteForward) isKeyRoute()  {}
func (KeyRouteConsumed) isKeyRoute() {}
func (KeyRouteCommand) isKeyRoute()  {}

// keyEventCombo is the configuration's spelling of one key press, or false
// for a key the `[keys]` grammar cannot describe.
//
// Shift-Tab becomes Tab carrying shift, which is how the configuration
// writes `shift+tab`. The Super/Command modifier is kept as itself: the
// accelerator remapping the dashboard does elsewhere is about menu
// accelerators, and a binding must mean the same key on every platform.
func keyEventCombo(key tea.KeyMsg) (core.KeyCombo, bool) {
	press := tea.KeyPressMsg(key.Key())
	code := chat.KeyPressCode(press)
	var name core.KeyName
	// Bubble Tea reports Shift-Tab as Tab carrying Shift, which is already the
	// configuration's `shift+tab`; crossterm's separate BackTab needs no case.
	switch code {
	case tea.KeyEnter:
		name = core.KeyNameEnter{}
	case tea.KeyEscape:
		name = core.KeyNameEsc{}
	case tea.KeyTab:
		name = core.KeyNameTab{}
	case tea.KeyBackspace:
		name = core.KeyNameBackspace{}
	case tea.KeyDelete:
		name = core.KeyNameDelete{}
	case tea.KeyInsert:
		name = core.KeyNameInsert{}
	case tea.KeyHome:
		name = core.KeyNameHome{}
	case tea.KeyEnd:
		name = core.KeyNameEnd{}
	case tea.KeyPgUp:
		name = core.KeyNamePageUp{}
	case tea.KeyPgDown:
		name = core.KeyNamePageDown{}
	case tea.KeyLeft:
		name = core.KeyNameLeft{}
	case tea.KeyRight:
		name = core.KeyNameRight{}
	case tea.KeyUp:
		name = core.KeyNameUp{}
	case tea.KeyDown:
		name = core.KeyNameDown{}
	default:
		switch {
		case code >= tea.KeyF1 && code <= tea.KeyF63:
			name = core.KeyNameF{Value: uint8(code-tea.KeyF1) + 1}
		case chat.IsCharKeyCode(code):
			name = core.KeyNameChar{Value: code}
		default:
			return core.KeyCombo{}, false
		}
	}
	modifiers := core.Modifiers{
		Ctrl:  press.Mod.Contains(tea.ModCtrl),
		Alt:   press.Mod.Contains(tea.ModAlt),
		Shift: press.Mod.Contains(tea.ModShift),
		Super: press.Mod.Contains(tea.ModSuper),
	}
	return core.NormalizeKeyCombo(core.NewKeyCombo(name, modifiers)), true
}

// commandForAction is the command one bindable action runs. Exhaustive on
// purpose: a new action in core must be given a command here.
func commandForAction(action core.KeyAction) CommandId {
	switch action {
	case core.KeyActionHelp:
		return CommandIdPalette
	case core.KeyActionOpenSettings:
		return CommandIdOpenConfig
	case core.KeyActionDetach:
		return CommandIdQuitDetach
	case core.KeyActionNewSession:
		return CommandIdNewSessionWizard
	case core.KeyActionResume:
		return CommandIdResumeDialog
	case core.KeyActionWorkspaceManager:
		return CommandIdWorkspaces
	case core.KeyActionFocusWorkspaces:
		return CommandIdFocusWorkspaces
	case core.KeyActionNextWorkspace:
		return CommandIdSelectWorkspaceNext
	case core.KeyActionPreviousWorkspace:
		return CommandIdSelectWorkspacePrevious
	case core.KeyActionSwitchWorkspace:
		return CommandIdSwitchWorkspace
	case core.KeyActionNextPane:
		return CommandIdCycleFocus
	case core.KeyActionPreviousPane:
		return CommandIdCycleFocusReverse
	case core.KeyActionPaneSize:
		return CommandIdCycleFocusedPaneSize
	case core.KeyActionPanePreset:
		return CommandIdTogglePanePreset
	case core.KeyActionRefresh:
		return CommandIdRefresh
	case core.KeyActionPalette:
		return CommandIdPalette
	case core.KeyActionCancelOperation:
		return CommandIdCancelOperation
	case core.KeyActionMarkAllRead:
		return CommandIdMarkAllRead
	case core.KeyActionNextAttention:
		return CommandIdNextAttention
	case core.KeyActionPreviousAttention:
		return CommandIdPreviousAttention
	case core.KeyActionWebViewer:
		return CommandIdWebViewer
	case core.KeyActionRenameSession:
		return CommandIdRenameSession
	case core.KeyActionToggleTranscriptRendering:
		return CommandIdToggleTranscriptRendering
	case core.KeyActionToggleDictation:
		return CommandIdToggleDictation
	case core.KeyActionChangedFiles:
		return CommandIdChangedFiles
	case core.KeyActionSplitVertical:
		return CommandIdOpenSessionSplitRight
	case core.KeyActionSplitHorizontal:
		return CommandIdOpenSessionSplitBelow
	case core.KeyActionClosePane:
		return CommandIdClosePane
	case core.KeyActionFocusPaneLeft:
		return CommandIdFocusPaneLeft
	case core.KeyActionFocusPaneDown:
		return CommandIdFocusPaneDown
	case core.KeyActionFocusPaneUp:
		return CommandIdFocusPaneUp
	case core.KeyActionFocusPaneRight:
		return CommandIdFocusPaneRight
	case core.KeyActionZoom:
		return CommandIdZoomPane
	case core.KeyActionLastPane:
		return CommandIdFocusLastPane
	case core.KeyActionResizeMode:
		return CommandIdResizeMode
	case core.KeyActionSwapPaneLeft:
		return CommandIdSwapPaneLeft
	case core.KeyActionSwapPaneDown:
		return CommandIdSwapPaneDown
	case core.KeyActionSwapPaneUp:
		return CommandIdSwapPaneUp
	case core.KeyActionSwapPaneRight:
		return CommandIdSwapPaneRight
	case core.KeyActionSuspendSession:
		return CommandIdSuspendSession
	case core.KeyActionRenameWorkspace:
		return CommandIdRenameWorkspace
	case core.KeyActionCloseWorkspace:
		return CommandIdCloseWorkspace
	case core.KeyActionResizePaneLeft:
		return CommandIdResizePaneLeft
	case core.KeyActionResizePaneDown:
		return CommandIdResizePaneDown
	case core.KeyActionResizePaneUp:
		return CommandIdResizePaneUp
	case core.KeyActionResizePaneRight:
		return CommandIdResizePaneRight
	case core.KeyActionOpenSubagents:
		return CommandIdOpenSubagents
	case core.KeyActionSessionActions:
		return CommandIdSessionActions
	case core.KeyActionInterruptTurn:
		return CommandIdInterruptTurn
	case core.KeyActionRestartSession:
		return CommandIdRestartSession
	case core.KeyActionMoveSession:
		return CommandIdMoveSession
	case core.KeyActionDestroySession:
		return CommandIdDestroySession
	case core.KeyActionContainerSettings:
		return CommandIdContainerSettings
	case core.KeyActionManageProfiles:
		return CommandIdManageProfiles
	case core.KeyActionManageTargets:
		return CommandIdManageTargets
	case core.KeyActionManageMachines:
		return CommandIdManageMachines
	case core.KeyActionRestartDaemon:
		return CommandIdRestartDaemon
	case core.KeyActionNoticeLog:
		return CommandIdNoticeLog
	case core.KeyActionChangeGoSetup:
		return CommandIdChangeGoSetup
	case core.KeyActionCycleSpinner:
		return CommandIdCycleSpinner
	}
	panic(fmt.Sprintf("key action %d has no command", action))
}

// digitRangeLen is how many bindings at the head of bindings form the nine
// numbered workspace keys, so the interface can print them as one row.
func digitRangeLen(bindings []core.Binding) int {
	if len(bindings) < 9 {
		return 0
	}
	first := bindings[0]
	for slot, binding := range bindings[:9] {
		digit, ok := binding.Combo.Name.(core.KeyNameChar)
		if binding.Trigger != first.Trigger ||
			binding.Index == nil || *binding.Index != slot ||
			binding.Combo.Modifiers != first.Combo.Modifiers ||
			!ok || digit.Value != rune('1'+slot) {
			return 0
		}
	}
	return 9
}

// bindingLabel is one binding's label. rhsOnly drops the prefix from a
// prefix binding, so the footer can print `c` under a `ctrl+b then:`
// heading.
func bindingLabel(keybinds *core.Keybinds, binding core.Binding, rhsOnly bool) string {
	key := core.FormatKeyCombo(binding.Combo)
	if binding.Trigger == core.TriggerPrefix && !rhsOnly {
		return keybinds.PrefixLabel() + " " + key
	}
	return key
}

// bindingLabels is every label for one action, with the nine numbered
// workspace keys collapsed into a single `1-9` row.
func bindingLabels(keybinds *core.Keybinds, bindings []core.Binding, rhsOnly bool) []string {
	var labels []string
	position := 0
	for position < len(bindings) {
		binding := bindings[position]
		run := digitRangeLen(bindings[position:])
		label := bindingLabel(keybinds, binding, rhsOnly)
		if run == 9 {
			// Every label in the run ends with its own digit; `1-9` replaces it.
			stem := strings.TrimSuffix(label, "1")
			labels = append(labels, stem+"1-9")
			position += run
		} else {
			labels = append(labels, label)
			position++
		}
	}
	return labels
}

// Keybinds is the bindings currently in force.
func (d *DashboardState) Keybinds() *core.Keybinds {
	return &d.keybinds
}

// PrefixPending reports whether the prefix key has been pressed and the
// surface is waiting for the key that completes the chord.
func (d *DashboardState) PrefixPending() bool {
	return d.prefixPending
}

// CancelPrefix disarms a pending prefix, for a pointer event or a lost
// focus.
func (d *DashboardState) CancelPrefix() {
	d.prefixPending = false
}

// RouteBoundKey reads one key press against the configured bindings.
//
// With no prefix pending a direct binding runs its command, the prefix
// itself arms the pending state, and anything else is forwarded. While
// pending, the prefix again forwards the literal key (tmux's rule, which is
// how `Ctrl-B` still reaches the composer), `Esc` cancels, a bound key runs
// its command, and anything else is swallowed with a notice saying so.
func (d *DashboardState) RouteBoundKey(key tea.KeyMsg) KeyRoute {
	if d.ModalOpen() {
		d.resizeMode = false
	}
	press, ok := key.(tea.KeyPressMsg)
	if !ok {
		return KeyRouteForward{}
	}
	if !press.IsRepeat {
		// A key press ends the pointer bookkeeping a modal-closing click left
		// behind, exactly as handleKeyAt does for the keys that reach it.
		// Without this a command run from here would leave the next click
		// suppressed.
		d.modalClickTransition = nil
		d.suppressModalRelease = false
	}
	combo, ok := keyEventCombo(press)
	if !ok {
		return KeyRouteForward{}
	}
	if d.simpleMode {
		// Simple mode has no prefix state. Every Mjolnir command is one
		// direct chord, and unbound keys continue into the focused surface
		// unchanged (including a bare Ctrl+B for tmux).
		d.prefixPending = false
		if _, bound := d.keybinds.ResolveDirect(combo); !bound && d.ResizeModeActive() {
			return d.routeResizeModeKey(press)
		}
		d.resizeMode = false
		if matched, bound := d.keybinds.ResolveDirect(combo); bound {
			return KeyRouteCommand{ID: commandForAction(matched.Action), Index: matched.Index}
		}
		return KeyRouteForward{}
	}
	if !d.prefixPending {
		if combo == d.keybinds.Prefix {
			d.prefixPending = true
			return KeyRouteConsumed{}
		}
		if _, bound := d.keybinds.ResolveDirect(combo); !bound && d.ResizeModeActive() {
			return d.routeResizeModeKey(press)
		}
		d.resizeMode = false
		if matched, bound := d.keybinds.ResolveDirect(combo); bound {
			return KeyRouteCommand{ID: commandForAction(matched.Action), Index: matched.Index}
		}
		return KeyRouteForward{}
	}
	d.prefixPending = false
	if combo == d.keybinds.Prefix {
		if d.ResizeModeActive() {
			return KeyRouteConsumed{}
		}
		// tmux's rule: the doubled prefix is the literal key.
		return KeyRouteForward{}
	}
	if combo == core.KeyComboPlain(core.KeyNameEsc{}) {
		d.resizeMode = false
		return KeyRouteConsumed{}
	}
	if matched, bound := d.keybinds.ResolvePrefix(combo); bound {
		d.resizeMode = false
		return KeyRouteCommand{ID: commandForAction(matched.Action), Index: matched.Index}
	}
	prefix := d.keybinds.PrefixLabel()
	pressed := core.FormatKeyCombo(combo)
	paletteLabels := commandActionLabels(&d.keybinds, CommandIdPalette, core.KeyActionPalette, false)
	var notice string
	if len(paletteLabels) > 0 {
		notice = fmt.Sprintf("%s %s is not bound; %s opens the command palette", prefix, pressed, paletteLabels[0])
	} else {
		notice = fmt.Sprintf("%s %s is not bound", prefix, pressed)
	}
	d.SetNotice(notice)
	return KeyRouteConsumed{}
}

// routeResizeModeKey is what resize mode makes of a key no direct binding
// claims: h/j/k/l and the arrows resize the focused pane, Esc leaves resize
// mode, and anything else is swallowed.
func (d *DashboardState) routeResizeModeKey(press tea.KeyPressMsg) KeyRoute {
	var id CommandId
	found := false
	if press.Mod&^(tea.ModCapsLock|tea.ModNumLock|tea.ModScrollLock) == 0 {
		switch chat.KeyPressCode(press) {
		case 'h', tea.KeyLeft:
			id, found = CommandIdResizePaneLeft, true
		case 'j', tea.KeyDown:
			id, found = CommandIdResizePaneDown, true
		case 'k', tea.KeyUp:
			id, found = CommandIdResizePaneUp, true
		case 'l', tea.KeyRight:
			id, found = CommandIdResizePaneRight, true
		case tea.KeyEscape:
			d.resizeMode = false
		}
	}
	if !found {
		return KeyRouteConsumed{}
	}
	return KeyRouteCommand{ID: id}
}

// RouteBoundKeyEvent is [DashboardState.RouteBoundKey] for a whole terminal
// event.
func (d *DashboardState) RouteBoundKeyEvent(event tea.Msg) KeyRoute {
	switch event := event.(type) {
	case tea.KeyMsg:
		return d.RouteBoundKey(event)
	case tea.PasteMsg:
		if d.ResizeModeActive() {
			return KeyRouteConsumed{}
		}
	}
	return KeyRouteForward{}
}

// keyLabels is every key that runs a command: the pane keys first, then the
// live bindings, as `["Enter", "ctrl+b c"]`.
func (d *DashboardState) keyLabels(id CommandId) []string {
	commandSpec := spec(id)
	var labels []string
	for _, hint := range commandSpec.paneKeys {
		if d.focus != FocusNotifications || hint.acceptsFocus(d.focus) {
			labels = append(labels, hint.label)
		}
	}
	if commandSpec.action != nil {
		labels = append(labels, commandActionLabels(&d.keybinds, id, *commandSpec.action, false)...)
	}
	return labels
}

// FirstKeyLabel is the first key that runs a command, for a sentence that
// tells the reader what to press. False when nothing is bound to it.
//
// A configured binding comes before a pane key here, the other way round
// from keyLabels: prose like "press … to create a session" is read from the
// conversation as often as from a pane, and a bare pane letter is text
// everywhere else.
func (d *DashboardState) FirstKeyLabel(id CommandId) (string, bool) {
	commandSpec := spec(id)
	if commandSpec.action != nil {
		if labels := commandActionLabels(&d.keybinds, id, *commandSpec.action, false); len(labels) > 0 {
			return labels[0], true
		}
	}
	if len(commandSpec.paneKeys) > 0 {
		return commandSpec.paneKeys[0].label, true
	}
	return "", false
}

// footerKey is the key the footer prints for a command: the key after the
// prefix for a chord command, otherwise its first pane key.
func (d *DashboardState) footerKey(id CommandId) (string, bool) {
	commandSpec := spec(id)
	if id == CommandIdPalette {
		labels := d.paletteBindingLabels(core.TriggerPrefix, true)
		if len(labels) == 0 {
			return "", false
		}
		return strings.Join(labels, " / "), true
	}
	afterPrefix := func() (string, bool) {
		if commandSpec.action == nil {
			return "", false
		}
		labels := commandActionLabels(&d.keybinds, id, *commandSpec.action, true)
		if len(labels) == 0 {
			return "", false
		}
		return labels[0], true
	}
	if commandSpec.footerGroup == FooterGroupChord {
		return afterPrefix()
	}
	if len(commandSpec.paneKeys) > 0 {
		return commandSpec.paneKeys[0].label, true
	}
	return afterPrefix()
}

func (d *DashboardState) paletteBindingLabels(trigger core.Trigger, rhsOnly bool) []string {
	var bindings []core.Binding
	for _, binding := range commandActionBindings(&d.keybinds, CommandIdPalette, core.KeyActionPalette) {
		if binding.Trigger == trigger {
			bindings = append(bindings, binding)
		}
	}
	return bindingLabels(&d.keybinds, bindings, rhsOnly)
}

func (d *DashboardState) directPaletteFooterKey() (string, bool) {
	labels := d.paletteBindingLabels(core.TriggerDirect, false)
	if len(labels) == 0 {
		return "", false
	}
	return strings.Join(labels, " / "), true
}

func commandActionLabels(keybinds *core.Keybinds, id CommandId, action core.KeyAction, rhsOnly bool) []string {
	return bindingLabels(keybinds, commandActionBindings(keybinds, id, action), rhsOnly)
}

func commandActionBindings(keybinds *core.Keybinds, id CommandId, action core.KeyAction) []core.Binding {
	actions := []core.KeyAction{action}
	for _, candidate := range core.KeyActionAll {
		if candidate != action && commandForAction(candidate) == id {
			actions = append(actions, candidate)
		}
	}
	var bindings []core.Binding
	for _, candidate := range actions {
		bindings = append(bindings, keybinds.Bindings(candidate)...)
	}
	return bindings
}
