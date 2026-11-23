// Port of mj-tui/src/setup/paths.rs.

package tui

import (
	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// pathInputContext identifies the focused path field and its text, so a
// completion reply for another field or another dialog is dropped.
func (d *SetupDialog) pathInputContext() string {
	panic("unported: mj-tui/src/setup/paths.rs path_input_context")
}

// resolvePathAction starts expanding a target path's home directory on its
// machine. A nil action means the path needs no expansion.
func (d *SetupDialog) resolvePathAction(path []string) (DashboardAction, error) {
	panic("unported: mj-tui/src/setup/paths.rs resolve_path_action")
}

// machineForPath is the machine a draft path belongs to: the machine itself
// under `machines`, and the machine a runtime names under `targets`.
func (d *SetupDialog) machineForPath(path []string) (core.Machine, bool) {
	if len(path) < 2 {
		return nil, false
	}
	var machineID string
	switch path[0] {
	case "machines":
		machineID = path[1]
	case "targets":
		machineID = core.LocalMachineID
		if machine, ok := setupValueString(setupValuePath(d.draft, "targets", path[1], "machine")); ok {
			machineID = machine
		}
	default:
		return nil, false
	}
	return d.machine(machineID)
}

// machine is one machine from the draft. This machine is always available,
// whether or not the draft spells it out.
func (d *SetupDialog) machine(machineID string) (core.Machine, bool) {
	machines := setupValueGet(d.draft, "machines")
	if setupValueHas(machines, machineID) {
		machine, err := setupValueMachine(setupValueGet(machines, machineID))
		return machine, err == nil
	}
	if machineID == core.LocalMachineID {
		return core.MachineLocal{}, true
	}
	return nil, false
}

// focusedPathInput is CompletesPaths for Settings: a local setting completes
// on the controller; a target setting completes on the machine it
// configures. A destination inside a container that does not exist yet
// completes nowhere.
func (d *SetupDialog) focusedPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	panic("unported: mj-tui/src/setup/paths.rs focused_path_input")
}

// dismissUnfocusedCompletions closes the popup of every path field but the
// focused one.
func (d *SetupDialog) dismissUnfocusedCompletions() {
	focused, hasFocused := d.focusedFieldPath()
	for key, field := range d.fields {
		if hasFocused && key == setupPathKey(focused) {
			continue
		}
		if input := field.input.pathInput(); input != nil {
			input.DismissCompletion()
		}
	}
}
