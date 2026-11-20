//go:build linux

// Port of mj-worker/src/microvm/vm.rs.
//
// VM lifecycle: run, start, stop, remove, and the facts about them.
//
// The engine is the only owner of VM state. Every mutation holds the VM's
// lock; slot allocation, admission and the firewall table hold the global
// lock, taken after the VM lock and never the other way round. A running VM
// is a systemd transient unit, so the Firecracker process dies before any
// file under it is removed, and the unit's runtime directory (which holds
// the vsock socket) is created and deleted by systemd.
//
// Descriptions (inspect, info, the Firecracker configuration) are JSON
// values as github.com/BrokkAi/acp-go/jsonvalue holds them, so their members
// keep the order Rust's json! macro gives them under preserve_order.

package worker

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"path/filepath"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// microvmPoolOverheadBytes is what the Nitro hypervisor takes from the pool
// on top of the guest's memory.
const microvmPoolOverheadBytes uint64 = 2 << 20

// microvmSlotStep: the kernel driver grants PCIe slots in groups of eight.
const microvmSlotStep uint32 = 8

// microvmMaxVCPUs is Firecracker's documented maximum for `vcpu_count`.
const microvmMaxVCPUs uint32 = 32

// MicrovmDefaultStopSeconds is Podman's documented default `stop` timeout,
// used to bound the guest flush.
const MicrovmDefaultStopSeconds uint64 = 10

const microvmNitroSysfs = "/sys/class/misc/nitro_enclaves"
const microvmRuntimeRoot = "/run/mj-microvm"

// microvmJailIDBase: each jailed VM runs Firecracker as its own user and
// group, so a compromised VMM can touch neither another VM's files nor its
// processes. The ids come from the network slot, which is unique among
// existing VMs, above the range of ordinary and subordinate (rootless
// container) users.
const microvmJailIDBase uint32 = 3_100_000

// MicrovmUnitName is the systemd unit of VM name.
func MicrovmUnitName(name string) string {
	return fmt.Sprintf("mj-microvm-%s.service", name)
}

// microvmLegacySocketPath: VMs launched before the jailer keep their vsock
// socket in a systemd runtime directory.
func microvmLegacySocketPath(name string) string {
	return filepath.Join(microvmRuntimeRoot, name, "v.sock")
}

// MicrovmAgentSocket is where the host reaches a VM's guest agent: in the
// jail for a jailed VM, in the runtime directory for one an earlier engine
// launched.
func MicrovmAgentSocket(layout MicrovmLayout, state MicrovmVmState) string {
	if state.Jailed {
		return filepath.Join(layout.JailRoot(state.Slot), "v.sock")
	}
	return microvmLegacySocketPath(state.Name)
}

func microvmJailIDs(slot uint32) (uid uint32, gid uint32) {
	return microvmJailIDBase + slot, microvmJailIDBase + slot
}

// MicrovmActivity is systemd's `ActiveState`, reduced to what the engine
// decides on.
type MicrovmActivity int

const (
	MicrovmActivityRunning MicrovmActivity = iota
	MicrovmActivityStopping
	MicrovmActivityStopped
)

// MicrovmVMActivity is vm::activity: the activity of VM name's unit.
func MicrovmVMActivity(name string) (MicrovmActivity, error) {
	panic("unported: mj-worker/src/microvm/vm.rs activity")
}

func microvmStopUnit(name string) error {
	panic("unported: mj-worker/src/microvm/vm.rs stop_unit")
}

// microvmPool is the Nitro Enclaves memory pool and PCIe slots.
type microvmPool struct {
	total      uint64
	free       uint64
	slots      uint32
	slotsTotal uint32
}

func microvmReadNumber(path string) (uint64, error) {
	panic("unported: mj-worker/src/microvm/vm.rs read_number")
}

// microvmReadPool is the pool, or nil on a host without the Nitro Enclaves
// driver.
func microvmReadPool() (*microvmPool, error) {
	panic("unported: mj-worker/src/microvm/vm.rs read_pool")
}

// microvmAdmit admits a launch or refuses it, naming the pool. Called with
// the global lock held.
//
// Memory is a commitment: the sum over running VMs of their memory plus the
// hypervisor's overhead must fit the pool. The engine's own list of running
// VMs decides this, not the kernel's momentary free figure, so two launches
// racing for the last gigabyte cannot both be admitted. The kernel's figure
// is still checked, to notice memory the engine did not hand out.
func microvmAdmit(layout MicrovmLayout, vm MicrovmVmState) error {
	panic("unported: mj-worker/src/microvm/vm.rs admit")
}

// MicrovmVolumeArg is one `--volume` of `run`.
type MicrovmVolumeArg struct {
	Name        string
	Destination string
	ReadWrite   bool
	Chown       bool
}

// MicrovmRunSpec is what `run` asks for.
type MicrovmRunSpec struct {
	Name        string
	Image       string
	Platform    *string
	Pull        MicrovmPullPolicy
	Labels      map[string]string
	CPUs        *float64
	MemoryBytes uint64
	Env         []string
	User        *string
	Workdir     *string
	Volumes     []MicrovmVolumeArg
	Command     []string
}

func microvmValidateDestination(destination string) error {
	panic("unported: mj-worker/src/microvm/vm.rs validate_destination")
}

// microvmRandomID is a new container id: 32 random bytes in hex.
func microvmRandomID() (string, error) {
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		return "", fmt.Errorf("generate a container id: %w", err)
	}
	return hex.EncodeToString(bytes), nil
}

func microvmAllocateSlot(layout MicrovmLayout) (uint32, error) {
	panic("unported: mj-worker/src/microvm/vm.rs allocate_slot")
}

// microvmReflink makes a copy-on-write clone of a file on a
// reflink-capable filesystem (XFS with `reflink=1`, btrfs). Fails visibly
// elsewhere: a full copy of an 11 GiB image per VM is not a fallback worth
// hiding.
func microvmReflink(source string, destination string) error {
	panic("unported: mj-worker/src/microvm/vm.rs reflink")
}

// MicrovmBuildState is the state of a new VM from spec and its image.
func MicrovmBuildState(layout MicrovmLayout, spec MicrovmRunSpec, imageID string, slot uint32) (MicrovmVmState, error) {
	panic("unported: mj-worker/src/microvm/vm.rs build_state")
}

// MicrovmVMRun creates and starts a VM and returns its id. On any failure,
// it removes everything this call made.
func MicrovmVMRun(layout MicrovmLayout, spec MicrovmRunSpec) (string, error) {
	panic("unported: mj-worker/src/microvm/vm.rs run")
}

// MicrovmVMResolve is the name of the VM an operand refers to. Podman
// accepts the container id that `run` printed (or an unambiguous prefix of
// at least 12 characters) as well as the name; a name wins over an id, and
// an operand that matches nothing is returned as it is, for the caller to
// report.
func MicrovmVMResolve(layout MicrovmLayout, operand string) (string, error) {
	panic("unported: mj-worker/src/microvm/vm.rs resolve")
}

// MicrovmVMStart starts an existing VM from its retained disks.
func MicrovmVMStart(layout MicrovmLayout, name string) error {
	panic("unported: mj-worker/src/microvm/vm.rs start")
}

// microvmFirecrackerConfig: the paths in a jailed VM's Firecracker
// configuration are relative to the jail: the engine hard-links the kernel,
// initrd and disks to these names.
func microvmFirecrackerConfig(layout MicrovmLayout, state MicrovmVmState, link MicrovmLink) (jsonvalue.Object, error) {
	panic("unported: mj-worker/src/microvm/vm.rs firecracker_config")
}

// microvmPrepareJail builds the VM's jail: its root directory holds hard
// links to the kernel, the initrd and the disks, and the Firecracker
// configuration, all owned by the VM's user. The jailer adds the device
// nodes it needs (including `/dev/nitro_enclaves`) and its own copy of
// Firecracker when it starts. Hard links, so a disk is the same file inside
// and outside the jail and nothing is copied; they need the jail on the
// filesystem that holds the disks.
func microvmPrepareJail(layout MicrovmLayout, state MicrovmVmState, boot *MicrovmBootAssets, link MicrovmLink) error {
	panic("unported: mj-worker/src/microvm/vm.rs prepare_jail")
}

// MicrovmRemoveJail deletes a VM's jail. The links go; the disks they point
// to stay.
func MicrovmRemoveJail(layout MicrovmLayout, slot uint32) error {
	panic("unported: mj-worker/src/microvm/vm.rs remove_jail")
}

// MicrovmReleaseHostResources removes what the host holds for a VM that is
// not running: its TAP device and its jail. Run by the VM's unit when it
// stops, and by the engine.
func MicrovmReleaseHostResources(layout MicrovmLayout, slot uint32) error {
	link, err := MicrovmLinkForSlot(slot)
	if err != nil {
		return err
	}
	if err := MicrovmTapDown(link.Tap); err != nil {
		return err
	}
	return MicrovmRemoveJail(layout, slot)
}

// microvmLaunch admits, wires up and starts the VM's unit, waits for the
// guest agent, and starts the container's main process. Leaves cleanup to
// the caller.
func microvmLaunch(layout MicrovmLayout, state *MicrovmVmState) error {
	panic("unported: mj-worker/src/microvm/vm.rs launch")
}

// microvmWaitForAgent polls until the guest agent answers. Ends when the
// unit dies or a signal arrives.
func microvmWaitForAgent(name string, socket string) (*MicrovmClient, error) {
	panic("unported: mj-worker/src/microvm/vm.rs wait_for_agent")
}

func microvmJournalTail(name string) string {
	panic("unported: mj-worker/src/microvm/vm.rs journal_tail")
}

// MicrovmOpenAgent connects to a running VM's guest agent.
func MicrovmOpenAgent(socket string) (*MicrovmClient, error) {
	panic("unported: mj-worker/src/microvm/vm.rs open_agent")
}

// MicrovmAgentFor is the connected agent of a running VM, or the
// Podman-style reason there is none.
func MicrovmAgentFor(layout MicrovmLayout, name string) (MicrovmVmState, *MicrovmClient, error) {
	panic("unported: mj-worker/src/microvm/vm.rs agent_for")
}

// MicrovmVMStop stops a VM: flush the guest's filesystems, then end its
// unit.
//
// The guest gets grace to flush and power off; without it a stop would lose
// whatever the guest had not yet written. Stopping an absent or stopped VM
// succeeds only with ignoreMissing for the absent case.
func MicrovmVMStop(layout MicrovmLayout, name string, grace time.Duration, ignoreMissing bool) error {
	panic("unported: mj-worker/src/microvm/vm.rs stop")
}

// microvmStopLocked stops the unit of name, first asking the guest at
// socket (nil without a state) to flush.
func microvmStopLocked(name string, socket *string, grace time.Duration) error {
	panic("unported: mj-worker/src/microvm/vm.rs stop_locked")
}

// microvmTeardown stops the unit and deletes the VM's directory and TAP.
// Idempotent; no state is required.
func microvmTeardown(layout MicrovmLayout, name string) error {
	panic("unported: mj-worker/src/microvm/vm.rs teardown")
}

// MicrovmVMRemove removes a VM. force stops a running one first; volumes
// also removes its named volumes.
func MicrovmVMRemove(layout MicrovmLayout, name string, force bool, ignoreMissing bool, volumes bool) error {
	panic("unported: mj-worker/src/microvm/vm.rs remove")
}

// MicrovmVMInspect is the Podman-shaped description of a VM.
func MicrovmVMInspect(layout MicrovmLayout, name string) (jsonvalue.Object, error) {
	panic("unported: mj-worker/src/microvm/vm.rs inspect")
}

// MicrovmVMExists is the state for `container exists`.
func MicrovmVMExists(layout MicrovmLayout, name string) (bool, error) {
	panic("unported: mj-worker/src/microvm/vm.rs exists")
}

// MicrovmVMInfo is what `info` reports.
func MicrovmVMInfo(layout MicrovmLayout) (jsonvalue.Object, error) {
	panic("unported: mj-worker/src/microvm/vm.rs info")
}
