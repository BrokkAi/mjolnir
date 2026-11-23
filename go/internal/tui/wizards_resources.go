// Port of mj-tui/src/wizards/resources.rs.
//
// Editable container sizing.
package tui

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const gib uint64 = 1 << 30

// ResourcePicker is the sizing control a wizard's target step shows. A nil
// ResourcePicker is Rust's `None`: the target takes no size.
type ResourcePicker interface {
	isResourcePicker()
}

// ResourcePickerContainer sizes a container session. SaveAsDefault is nil
// when the wizard cannot remember the size, otherwise whether it will.
type ResourcePickerContainer struct {
	Editor        *ResourceEditor
	SaveAsDefault *bool
}

func (ResourcePickerContainer) isResourcePicker() {}

// HostLimits is a host's total CPUs and memory bytes, which an editor's
// values may not exceed (Rust's `Option<(u64, u64)>` limits pair; a nil
// *HostLimits is `None`).
type HostLimits struct {
	Cpus        uint64
	MemoryBytes uint64
}

func targetResources(dashboard *DashboardState, wizard WizardDraft) ResourcePicker {
	panic("unported: mj-tui/src/wizards/resources.rs target_resources")
}

func declareResourceControls(form *chat.Dialog[WizardControl], resources ResourcePicker) {
	container, ok := resources.(ResourcePickerContainer)
	if !ok {
		return
	}
	form.DeclareWithEnabled(WizardControlResourceCpu, chat.ControlKindTextField{}, true)
	form.DeclareWithEnabled(WizardControlResourceMemory, chat.ControlKindTextField{}, true)
	if container.SaveAsDefault != nil {
		form.DeclareWithEnabled(WizardControlRememberContainerSize, chat.ControlKindCheckbox{}, true)
	}
}

// ResourceEditor is the CPU and memory fields of a container size. The zero
// value is Rust's Default.
type ResourceEditor struct {
	targetID *string
	cpu      chat.TextInput
	memory   chat.TextInput
}

// Equal is Rust's derived PartialEq.
func (e *ResourceEditor) Equal(other *ResourceEditor) bool {
	if (e.targetID == nil) != (other.targetID == nil) ||
		(e.targetID != nil && *e.targetID != *other.targetID) {
		return false
	}
	return e.cpu.Equal(&other.cpu) && e.memory.Equal(&other.memory)
}

func (e *ResourceEditor) draftValues() []string {
	return []string{e.cpu.String(), e.memory.String()}
}

// reset fills the fields from allocation; a nil allocation clears them.
func (e *ResourceEditor) reset(allocation core.SessionResourceAllocation) {
	if container, ok := allocation.(core.SessionResourceAllocationContainer); ok {
		e.cpu = chat.TextInputFromValue(strconv.FormatUint(container.Cpus, 10))
		e.memory = chat.TextInputFromValue(memoryGibText(container.MemoryBytes))
		return
	}
	e.cpu.Clear()
	e.memory.Clear()
}

// allocation validates the fields into a container allocation, or returns
// the message the wizard shows. limits nil means the host's totals are
// unknown.
func (e *ResourceEditor) allocation(limits *HostLimits, requireNewMinimum bool) (core.SessionResourceAllocation, error) {
	panic("unported: mj-tui/src/wizards/resources.rs ResourceEditor::allocation")
}

// hostLimitGibText is a host limit for a message: rounded down to one
// decimal, so the number shown is itself accepted when typed back.
func hostLimitGibText(bytes uint64) string {
	return core.FloorGibTenths(bytes)
}

// memoryGibText is exact decimal GiB: even a host limit ending in a partial
// GiB round-trips.
func memoryGibText(bytes uint64) string {
	text := []byte(strconv.FormatUint(bytes/gib, 10))
	remainder := bytes % gib
	if remainder != 0 {
		text = append(text, '.')
		for remainder != 0 {
			remainder *= 10
			text = append(text, byte('0'+remainder/gib))
			remainder %= gib
		}
	}
	return string(text)
}

// capacityGibText: a capacity total is descriptive rather than an editable
// value, so show one decimal place and avoid the long repeating fraction of
// an exact byte count.
func capacityGibText(bytes uint64) string {
	return core.FloorGibTenths(bytes)
}

// parseMemoryGib reads decimal GiB (up to 30 decimal places) as bytes,
// rounded to the nearest byte; false when the text is not such a number or
// the result does not fit.
func parseMemoryGib(text string) (uint64, bool) {
	panic("unported: mj-tui/src/wizards/resources.rs parse_memory_gib")
}
