// Port of mj-tui/src/wizards/resources.rs.
//
// Editable container sizing.
package tui

import (
	"errors"
	"fmt"
	"math/big"
	"math/bits"
	"strconv"
	"strings"

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
	if wizard.Step() != WizardStepTarget {
		return nil
	}
	targets := dashboard.config.Get().Targets
	id := nthKey(targets, wizard.Target())
	if _, rejected := wizard.TargetRejection(dashboard, id); rejected {
		return nil
	}
	if !core.IsContainerTarget(targets[id]) {
		return nil
	}
	picker := ResourcePickerContainer{Editor: wizard.ResourceEditor()}
	if remember, ok := wizard.RememberContainerSize(); ok {
		picker.SaveAsDefault = &remember
	}
	return picker
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
	cpu := strings.TrimSpace(e.cpu.String())
	cpus, err := strconv.ParseUint(cpu, 10, 64)
	if err != nil || cpus == 0 || !resourceTextIsDigits(cpu) {
		return nil, errors.New("CPU must be a positive whole number.")
	}
	if requireNewMinimum && cpus < core.MinimumContainerCpus {
		return nil, fmt.Errorf("CPU must be at least %d.", core.MinimumContainerCpus)
	}
	memoryBytes, ok := parseMemoryGib(strings.TrimSpace(e.memory.String()))
	if !ok || memoryBytes == 0 {
		return nil, errors.New("MEM must be a positive number of GiB (up to 30 decimal places).")
	}
	if requireNewMinimum && memoryBytes < core.MinimumContainerMemoryBytes {
		return nil, errors.New("RAM must be at least 4 GiB.")
	}
	if limits != nil {
		if cpus > limits.Cpus {
			return nil, fmt.Errorf("CPU exceeds this host's %d CPUs.", limits.Cpus)
		}
		if memoryBytes > limits.MemoryBytes {
			return nil, fmt.Errorf("RAM exceeds this host's %s GiB.", hostLimitGibText(limits.MemoryBytes))
		}
	}
	return core.SessionResourceAllocationContainer{Cpus: cpus, MemoryBytes: memoryBytes}, nil
}

// resourceTextIsDigits is whether every byte of text is an ASCII digit
// (true for the empty text, as Rust's `bytes().all` is).
func resourceTextIsDigits(text string) bool {
	for i := 0; i < len(text); i++ {
		if text[i] < '0' || text[i] > '9' {
			return false
		}
	}
	return true
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
	whole, fraction, _ := strings.Cut(text, ".")
	if (whole == "" && fraction == "") ||
		!resourceTextIsDigits(whole) ||
		len(fraction) > 30 ||
		!resourceTextIsDigits(fraction) {
		return 0, false
	}
	if whole == "" {
		whole = "0"
	}
	wholeGib, err := strconv.ParseUint(whole, 10, 64)
	if err != nil {
		return 0, false
	}
	hi, wholeBytes := bits.Mul64(wholeGib, gib)
	if hi != 0 {
		return 0, false
	}
	if fraction == "" {
		return wholeBytes, true
	}
	// Cancel powers of two before multiplication to avoid overflow for the
	// exact 30-place decimal representation of a single byte.
	digits := len(fraction)
	denominator := new(big.Int).Exp(big.NewInt(5), big.NewInt(int64(digits)), nil)
	numerator, ok := new(big.Int).SetString(fraction, 10)
	if !ok {
		return 0, false
	}
	numerator.Lsh(numerator, uint(30-digits))
	half := new(big.Int).Rsh(denominator, 1)
	fractionBytes := numerator.Add(numerator, half).Quo(numerator, denominator)
	if !fractionBytes.IsUint64() {
		return 0, false
	}
	sum, carry := bits.Add64(wholeBytes, fractionBytes.Uint64(), 0)
	if carry != 0 {
		return 0, false
	}
	return sum, true
}
