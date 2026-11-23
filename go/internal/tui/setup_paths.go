// Port of mj-tui/src/setup/paths.rs.

package tui

import (
	"errors"
	"fmt"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// pathInputContext identifies the focused path field and its text, so a
// completion reply for another field or another dialog is dropped.
func (d *SetupDialog) pathInputContext() string {
	focused := "None"
	if path, ok := d.focusedFieldPath(); ok {
		if field, ok := d.fields.get(path); ok {
			focused = fmt.Sprintf("Some((%s, %s))", setupDebugStrings(path), strconv.Quote(field.input.text().Value()))
		}
	}
	return fmt.Sprintf("settings:%d:%s:%s", d.generation, setupValueDisplay(d.draft), focused)
}

// setupDebugStrings is Rust's Debug of a Vec<String>: `["a", "b"]`.
func setupDebugStrings(values []string) string {
	quoted := make([]string, len(values))
	for i, value := range values {
		quoted[i] = strconv.Quote(value)
	}
	return "[" + strings.Join(quoted, ", ") + "]"
}

// resolvePathAction starts expanding a target path's home directory on its
// machine. A nil action means the path needs no expansion.
func (d *SetupDialog) resolvePathAction(path []string) (DashboardAction, error) {
	field, ok := d.fields.get(path)
	if !ok {
		return nil, nil
	}
	if kind, ok := client.PathKindOf(path); !ok || kind != client.PathKindTarget {
		return nil, nil
	}
	needsHome, err := core.NeedsHome(field.input.text().Value())
	if err != nil {
		return nil, errors.New(err.Error())
	}
	if !needsHome {
		return nil, nil
	}
	machine, ok := d.machineForPath(path)
	if !ok {
		return nil, errors.New("This setting has no machine to resolve the path on")
	}
	notice := "Resolving path…"
	d.notice = &notice
	return DashboardActionResolveSetupPath{
		Generation: d.generation,
		Draft:      setupValueClone(d.draft),
		Path:       append([]string{}, path...),
		Value:      field.input.text().Value(),
		Machine:    machine,
	}, nil
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
	path, ok := d.focusedFieldPath()
	if !ok {
		return nil, nil, 0, false
	}
	field, ok := d.fields.get(path)
	if !ok || field.input.pathInput() == nil {
		return nil, nil, 0, false
	}
	pathKind, ok := client.PathKindOf(path)
	if !ok {
		return nil, nil, 0, false
	}
	var host core.CompletionHost
	var kind core.CompletionKind
	switch pathKind {
	case client.PathKindLocal:
		host, kind = core.CompletionHostLocal{}, client.CompletionKind(path)
	case client.PathKindTarget:
		// The machine is read before the input is borrowed mutably.
		machine, ok := d.machineForPath(path)
		if !ok {
			return nil, nil, 0, false
		}
		host, kind = core.CompletionHostMachine{Machine: machine}, core.CompletionKindDirectories
	default:
		return nil, nil, 0, false
	}
	return field.input.pathInput(), host, kind, true
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
