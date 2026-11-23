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
	"strings"

	tea "charm.land/bubbletea/v2"

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
	panic("unported: mj-tui/src/keybinds.rs key_event_combo")
}

// commandForAction is the command one bindable action runs. Exhaustive on
// purpose: a new action in core must be given a command here.
func commandForAction(action core.KeyAction) CommandId {
	panic("unported: mj-tui/src/keybinds.rs command_for_action")
}

// digitRangeLen is how many bindings at the head of bindings form the nine
// numbered workspace keys, so the interface can print them as one row.
func digitRangeLen(bindings []core.Binding) int {
	panic("unported: mj-tui/src/keybinds.rs digit_range_len")
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
	panic("unported: mj-tui/src/keybinds.rs binding_labels")
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
	panic("unported: mj-tui/src/keybinds.rs DashboardState::route_bound_key")
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
	panic("unported: mj-tui/src/keybinds.rs DashboardState::key_labels")
}

// FirstKeyLabel is the first key that runs a command, for a sentence that
// tells the reader what to press. False when nothing is bound to it.
//
// A configured binding comes before a pane key here, the other way round
// from keyLabels: prose like "press … to create a session" is read from the
// conversation as often as from a pane, and a bare pane letter is text
// everywhere else.
func (d *DashboardState) FirstKeyLabel(id CommandId) (string, bool) {
	panic("unported: mj-tui/src/keybinds.rs DashboardState::first_key_label")
}

// footerKey is the key the footer prints for a command: the key after the
// prefix for a chord command, otherwise its first pane key.
func (d *DashboardState) footerKey(id CommandId) (string, bool) {
	panic("unported: mj-tui/src/keybinds.rs DashboardState::footer_key")
}

func (d *DashboardState) paletteBindingLabels(trigger core.Trigger, rhsOnly bool) []string {
	panic("unported: mj-tui/src/keybinds.rs DashboardState::palette_binding_labels")
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
