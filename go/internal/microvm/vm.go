//go:build linux

// Port of mj-microvm/src/vm.rs.
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

package microvm

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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

// microvmRequireRoot refuses an engine that is not root.
func microvmRequireRoot() error {
	if !MicrovmIsRoot() {
		return errors.New("the microvm engine must run as root (use sudo -n)")
	}
	return nil
}

// MicrovmVMActivity is vm::activity: the activity of VM name's unit.
func MicrovmVMActivity(name string) (MicrovmActivity, error) {
	systemctl, err := MicrovmFindTool("systemctl")
	if err != nil {
		return 0, err
	}
	output, err := MicrovmRunChecked(systemctl, []string{"show", "--property=ActiveState", "--value", MicrovmUnitName(name)})
	if err != nil {
		return 0, err
	}
	switch strings.TrimSpace(microvmLossy(output.Stdout)) {
	case "active", "activating", "reloading":
		return MicrovmActivityRunning, nil
	case "deactivating":
		return MicrovmActivityStopping, nil
	}
	return MicrovmActivityStopped, nil
}

func microvmStopUnit(name string) error {
	systemctl, err := MicrovmFindTool("systemctl")
	if err != nil {
		return err
	}
	unit := MicrovmUnitName(name)
	// A unit that is not loaded is already stopped; the check below is the test.
	if _, err := MicrovmRunUnchecked(systemctl, []string{"stop", unit}); err != nil {
		return err
	}
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return err
	}
	if activity != MicrovmActivityStopped {
		return fmt.Errorf("%s is still running after systemctl stop", unit)
	}
	return nil
}

// microvmPool is the Nitro Enclaves memory pool and PCIe slots.
type microvmPool struct {
	total      uint64
	free       uint64
	slots      uint32
	slotsTotal uint32
}

func microvmReadNumber(path string) (uint64, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return 0, fmt.Errorf("read %s: %w", path, microvmIOError(err))
	}
	if !utf8.Valid(data) {
		return 0, fmt.Errorf("read %s: stream did not contain valid UTF-8", path)
	}
	value, err := microvmParseUint(strings.TrimSpace(string(data)), 64)
	if err != nil {
		return 0, fmt.Errorf("parse %s: %w", path, err)
	}
	return value, nil
}

// microvmReadPool is the pool, or nil on a host without the Nitro Enclaves
// driver.
func microvmReadPool() (*microvmPool, error) {
	base := microvmNitroSysfs
	if !microvmExists(base) {
		return nil, nil
	}
	var values [4]uint64
	for i, file := range []string{"mem_pool/total", "mem_pool/free", "pcie_slots", "pcie_slots_total"} {
		value, err := microvmReadNumber(filepath.Join(base, file))
		if err != nil {
			return nil, err
		}
		values[i] = value
	}
	return &microvmPool{total: values[0], free: values[1], slots: uint32(values[2]), slotsTotal: uint32(values[3])}, nil
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
	pool, err := microvmReadPool()
	if err != nil {
		return err
	}
	if pool == nil {
		return fmt.Errorf("this host has no Nitro Enclaves driver (%s is missing); launch the instance with enclave support and boot with nitro_enclaves.mempool set", microvmNitroSysfs)
	}
	committed := uint64(0)
	running := uint32(0)
	states, err := MicrovmVmStateList(layout)
	if err != nil {
		return err
	}
	for _, other := range states {
		if other.Name == vm.Name {
			continue
		}
		activity, err := MicrovmVMActivity(other.Name)
		if err != nil {
			return err
		}
		if activity != MicrovmActivityStopped {
			committed += other.MemoryBytes + microvmPoolOverheadBytes
			running++
		}
	}
	needed := vm.MemoryBytes + microvmPoolOverheadBytes
	mib := func(bytes uint64) uint64 { return bytes >> 20 }
	if pool.total < committed+needed {
		return fmt.Errorf("the Nitro Sandbox memory pool cannot fit this VM: pool %d MiB, %d MiB committed to %d running VMs, %d MiB needed", mib(pool.total), mib(committed), running, mib(needed))
	}
	if pool.free < needed {
		return fmt.Errorf("the Nitro Sandbox memory pool has %d MiB free, %d MiB needed (the pool is %d MiB; memory outside this engine's VMs holds the rest)", mib(pool.free), mib(needed), mib(pool.total))
	}
	slotsNeeded := running + 1
	if slotsNeeded > pool.slotsTotal {
		return fmt.Errorf("this host supports %d Nitro Sandboxes at once and %d are running", pool.slotsTotal, running)
	}
	if slotsNeeded > pool.slots {
		raised := (slotsNeeded + microvmSlotStep - 1) / microvmSlotStep * microvmSlotStep
		raised = min(raised, pool.slotsTotal)
		if err := os.WriteFile(filepath.Join(microvmNitroSysfs, "pcie_slots"), []byte(strconv.FormatUint(uint64(raised), 10)), 0o666); err != nil {
			return fmt.Errorf("raise the Nitro Sandbox slot count to %d: %w", raised, microvmIOError(err))
		}
	}
	return nil
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
	MbxRemote   *MicrovmMbxRemote
	User        *string
	Workdir     *string
	Volumes     []MicrovmVolumeArg
	Command     []string
}

func microvmValidateDestination(destination string) error {
	valid := strings.HasPrefix(destination, "/") && len(destination) > 1 &&
		!strings.HasSuffix(destination, "/") && !strings.Contains(destination, "//")
	for i := 0; valid && i < len(destination); i++ {
		b := destination[i]
		valid = microvmIsASCIIAlphanumeric(b) || b == '/' || b == '.' || b == '_' || b == '-'
	}
	for _, part := range strings.Split(destination, "/") {
		if part == ".." || part == "." {
			valid = false
		}
	}
	if !valid {
		return fmt.Errorf("unsupported volume destination %s: use a plain absolute path of letters, digits, '/', '.', '_' and '-'", microvmDebugString(destination))
	}
	for _, reserved := range []string{"/proc", "/sys", "/dev", "/run", "/.mj-microvm"} {
		if destination == reserved || strings.HasPrefix(destination, reserved+"/") {
			return fmt.Errorf("cannot mount a volume at %s: the guest uses %s", destination, reserved)
		}
	}
	return nil
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
	states, err := MicrovmVmStateList(layout)
	if err != nil {
		return 0, err
	}
	used := map[uint32]bool{}
	for _, vm := range states {
		used[vm.Slot] = true
	}
	for slot := uint32(0); slot < MicrovmMaxSlots; slot++ {
		if !used[slot] {
			return slot, nil
		}
	}
	return 0, fmt.Errorf("no free network slot: %d VMs exist", MicrovmMaxSlots)
}

// microvmReflink makes a copy-on-write clone of a file on a
// reflink-capable filesystem (XFS with `reflink=1`, btrfs). Fails visibly
// elsewhere: a full copy of an 11 GiB image per VM is not a fallback worth
// hiding.
func microvmReflink(source string, destination string) error {
	sourceFile, err := os.Open(source)
	if err != nil {
		return fmt.Errorf("open %s: %w", source, microvmIOError(err))
	}
	defer sourceFile.Close()
	destinationFile, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
	if err != nil {
		return fmt.Errorf("create %s: %w", destination, microvmIOError(err))
	}
	defer destinationFile.Close()
	if err := unix.IoctlFileClone(int(destinationFile.Fd()), int(sourceFile.Fd())); err != nil {
		_ = os.Remove(destination)
		return fmt.Errorf("clone %s to %s: the state directory needs a reflink-capable filesystem (XFS created with reflink=1, or btrfs): %w", source, destination, microvmIOError(err))
	}
	return nil
}

// microvmAvailableParallelism is Rust's std::thread::available_parallelism:
// the CPUs this process may run on.
func microvmAvailableParallelism() int {
	return runtime.NumCPU()
}

// MicrovmBuildState is the state of a new VM from spec and its image.
func MicrovmBuildState(layout MicrovmLayout, spec MicrovmRunSpec, imageID string, slot uint32) (MicrovmVmState, error) {
	image, err := MicrovmImageConfigRead(layout, imageID)
	if err != nil {
		return MicrovmVmState{}, err
	}
	cpus := float64(min(microvmAvailableParallelism(), int(microvmMaxVCPUs)))
	if spec.CPUs != nil {
		cpus = *spec.CPUs
	}
	if !(cpus > 0) {
		return MicrovmVmState{}, errors.New("--cpus must be positive")
	}
	// A VM's vCPUs are whole cores handed to the Nitro hypervisor; nothing
	// on the host can throttle one to a fraction, so a fraction is refused
	// rather than quietly rounded up to more CPU than was asked for.
	if cpus-math.Trunc(cpus) != 0 || cpus > float64(microvmMaxVCPUs) {
		return MicrovmVmState{}, fmt.Errorf("--cpus must be a whole number of vCPUs from 1 to %d, not %s", microvmMaxVCPUs, microvmRustFloat(cpus))
	}
	vcpus := uint32(cpus)
	env := append([]string{}, image.Env...)
	for _, entry := range spec.Env {
		key, _, _ := strings.Cut(entry, "=")
		kept := env[:0]
		for _, existing := range env {
			if existingKey, _, found := strings.Cut(existing, "="); !found || existingKey != key {
				kept = append(kept, existing)
			}
		}
		env = append(kept, entry)
	}
	cmd := spec.Command
	if len(cmd) == 0 {
		cmd = image.Cmd
	}
	if len(image.Entrypoint) == 0 && len(cmd) == 0 {
		return MicrovmVmState{}, errors.New("the image has no command; give one after the image name")
	}
	id, err := microvmRandomID()
	if err != nil {
		return MicrovmVmState{}, err
	}
	user := image.User
	if spec.User != nil {
		user = *spec.User
	}
	workingDir := image.WorkingDir
	if spec.Workdir != nil && *spec.Workdir != "" {
		workingDir = *spec.Workdir
	}
	volumes := make([]MicrovmVmVolume, 0, len(spec.Volumes))
	for _, volume := range spec.Volumes {
		volumes = append(volumes, MicrovmVmVolume{
			Name:        volume.Name,
			Destination: volume.Destination,
			ReadWrite:   volume.ReadWrite,
			Chown:       volume.Chown,
		})
	}
	var mbxRemote *MicrovmMbxRemote
	if spec.MbxRemote != nil {
		remote := *spec.MbxRemote
		mbxRemote = &remote
	}
	return MicrovmVmState{
		Format:         MicrovmStateFormat,
		ID:             id,
		Name:           spec.Name,
		Created:        MicrovmNowRFC3339(),
		ImageReference: spec.Image,
		ImageID:        imageID,
		Labels:         spec.Labels,
		VCPUs:          vcpus,
		NanoCPUs:       int64(math.Round(cpus * 1e9)),
		MemoryBytes:    spec.MemoryBytes,
		Env:            env,
		MbxRemote:      mbxRemote,
		User:           user,
		WorkingDir:     workingDir,
		Entrypoint:     image.Entrypoint,
		Cmd:            cmd,
		Volumes:        volumes,
		Slot:           slot,
		EngineBuild:    core.WorkerBuildID(),
		Jailed:         true,
	}, nil
}

// MicrovmVMRun creates and starts a VM and returns its id. On any failure,
// it removes everything this call made.
func MicrovmVMRun(layout MicrovmLayout, spec MicrovmRunSpec) (string, error) {
	if err := MicrovmValidateName("container", spec.Name); err != nil {
		return "", err
	}
	if err := microvmValidateLabels(spec.Labels); err != nil {
		return "", err
	}
	if len(spec.Volumes) > 24 {
		return "", errors.New("at most 24 volumes can be attached to a VM")
	}
	destinations := map[string]bool{}
	for _, volume := range spec.Volumes {
		if err := MicrovmValidateName("volume", volume.Name); err != nil {
			return "", err
		}
		if err := microvmValidateDestination(volume.Destination); err != nil {
			return "", err
		}
		if destinations[volume.Destination] {
			return "", fmt.Errorf("two volumes are mounted at %s", volume.Destination)
		}
		destinations[volume.Destination] = true
		count := 0
		for _, other := range spec.Volumes {
			if other.Name == volume.Name {
				count++
			}
		}
		if count != 1 {
			return "", fmt.Errorf("volume %s is mounted twice", volume.Name)
		}
	}
	if spec.MemoryBytes < 64<<20 {
		return "", errors.New("--memory is required and must be at least 64m: a microVM's memory is reserved from the Nitro pool")
	}
	if err := microvmRequireRoot(); err != nil {
		return "", err
	}
	if err := layout.Ensure(); err != nil {
		return "", err
	}
	vmLock, err := layout.LockVM(spec.Name)
	if err != nil {
		return "", err
	}
	defer vmLock.Drop()
	if microvmExists(layout.VM(spec.Name)) {
		return "", fmt.Errorf("the container name %s is already in use", microvmDebugString(spec.Name))
	}
	// Pulling can take minutes and is not rolled back: a later run reuses the image.
	imageID, imageLock, err := MicrovmEnsureImage(layout, spec.Image, spec.Platform, spec.Pull)
	if err != nil {
		return "", err
	}
	MicrovmInstallInterruptHandlers()
	var createdVolumes []string
	state, err := func() (MicrovmVmState, error) {
		// The image's lock, taken when `run` resolved the image, is held
		// until its root disk is cloned, so a prune cannot remove it in
		// between, and every volume's lock until the state that lists them
		// is written, so a `volume rm` cannot remove one in between. Lock
		// order: VM, image, volumes in name order, global.
		defer imageLock.Drop()
		names := append([]MicrovmVolumeArg{}, spec.Volumes...)
		sort.SliceStable(names, func(i, j int) bool { return names[i].Name < names[j].Name })
		var volumeLocks []*core.FileLock
		dropVolumeLocks := func() {
			for _, lock := range volumeLocks {
				lock.Drop()
			}
			volumeLocks = nil
		}
		defer dropVolumeLocks()
		for _, volume := range names {
			if err := MicrovmCheckInterrupted(); err != nil {
				return MicrovmVmState{}, err
			}
			lock, created, err := MicrovmVolumePrepareForRun(layout, volume.Name, spec.Labels)
			if err != nil {
				return MicrovmVmState{}, fmt.Errorf("prepare volume %s: %w", volume.Name, err)
			}
			volumeLocks = append(volumeLocks, lock)
			if created {
				createdVolumes = append(createdVolumes, volume.Name)
			}
		}
		state, err := func() (MicrovmVmState, error) {
			global, err := layout.LockGlobal()
			if err != nil {
				return MicrovmVmState{}, err
			}
			defer global.Drop()
			// A volume is one ext4 image; two kernels writing it would
			// corrupt it. The volume locks held above make this check and the
			// state write below one decision.
			states, err := MicrovmVmStateList(layout)
			if err != nil {
				return MicrovmVmState{}, err
			}
			for _, other := range states {
				for _, mounted := range other.Volumes {
					for _, wanted := range spec.Volumes {
						if wanted.Name == mounted.Name {
							return MicrovmVmState{}, fmt.Errorf("volume %s is already attached to container %s", mounted.Name, other.Name)
						}
					}
				}
			}
			slot, err := microvmAllocateSlot(layout)
			if err != nil {
				return MicrovmVmState{}, err
			}
			state, err := MicrovmBuildState(layout, spec, imageID, slot)
			if err != nil {
				return MicrovmVmState{}, err
			}
			// The state file reserves the slot; the root disk follows.
			if err := os.Mkdir(layout.VM(spec.Name), 0o777); err != nil {
				return MicrovmVmState{}, fmt.Errorf("create %s: %w", layout.VM(spec.Name), microvmIOError(err))
			}
			if err := state.Write(layout); err != nil {
				return MicrovmVmState{}, err
			}
			return state, nil
		}()
		if err != nil {
			return MicrovmVmState{}, err
		}
		dropVolumeLocks()
		if err := microvmReflink(MicrovmRootImage(layout, imageID), filepath.Join(layout.VM(state.Name), "root.ext4")); err != nil {
			return MicrovmVmState{}, err
		}
		// The clone owns its blocks now; the base image can be pruned.
		imageLock.Drop()
		if err := microvmLaunch(layout, &state); err != nil {
			return MicrovmVmState{}, err
		}
		return state, nil
	}()
	if err == nil {
		return state.ID, nil
	}
	var problems []string
	if cleanup := microvmTeardown(layout, spec.Name); cleanup != nil {
		problems = append(problems, cleanup.Error())
	} else if cleanup := layout.ForgetVMLock(spec.Name, vmLock); cleanup != nil {
		// The VM is gone, and its lock file goes with it.
		problems = append(problems, cleanup.Error())
	}
	for _, name := range createdVolumes {
		if cleanup := MicrovmVolumeRemove(layout, name, true); cleanup != nil {
			problems = append(problems, fmt.Sprintf("remove volume %s: %s", name, cleanup.Error()))
		}
	}
	if len(problems) == 0 {
		return "", err
	}
	return "", fmt.Errorf("cleanup after the failure also failed: %s: %w", strings.Join(problems, "; "), err)
}

// MicrovmVMResolve is the name of the VM an operand refers to. Podman
// accepts the container id that `run` printed (or an unambiguous prefix of
// at least 12 characters) as well as the name; a name wins over an id, and
// an operand that matches nothing is returned as it is, for the caller to
// report.
func MicrovmVMResolve(layout MicrovmLayout, operand string) (string, error) {
	if MicrovmValidateName("container", operand) == nil && microvmIsFile(MicrovmVmStatePath(layout, operand)) {
		return operand, nil
	}
	hexadecimal := len(operand) >= 12
	for i := 0; hexadecimal && i < len(operand); i++ {
		hexadecimal = microvmIsASCIIHexDigit(operand[i])
	}
	if hexadecimal {
		wanted := strings.ToLower(operand)
		states, err := MicrovmVmStateList(layout)
		if err != nil {
			return "", err
		}
		var matches []string
		for _, vm := range states {
			if strings.HasPrefix(vm.ID, wanted) {
				matches = append(matches, vm.Name)
			}
		}
		if len(matches) == 1 {
			return matches[0], nil
		}
	}
	return operand, nil
}

// microvmNoSuchContainerError is Podman's refusal of an unknown name.
func microvmNoSuchContainerError(name string) error {
	return fmt.Errorf("no container with name or ID %s found: no such container", microvmDebugString(name))
}

// MicrovmVMStart starts an existing VM from its retained disks.
func MicrovmVMStart(layout MicrovmLayout, name string) error {
	name, err := MicrovmVMResolve(layout, name)
	if err != nil {
		return err
	}
	if err := MicrovmValidateName("container", name); err != nil {
		return err
	}
	if err := microvmRequireRoot(); err != nil {
		return err
	}
	vmLock, err := layout.LockVM(name)
	if err != nil {
		return err
	}
	defer vmLock.Drop()
	// A crash can leave a root replacement half done; finish it under this
	// lock before the VM boots from whichever disk is in place.
	if err := MicrovmResumeInterruptedLocked(layout, name); err != nil {
		return err
	}
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return err
	}
	if state == nil {
		if err := layout.ForgetVMLock(name, vmLock); err != nil {
			return err
		}
		return microvmNoSuchContainerError(name)
	}
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return err
	}
	switch activity {
	case MicrovmActivityRunning:
		return nil
	case MicrovmActivityStopping:
		return fmt.Errorf("container %s is stopping; try again when it has stopped", name)
	}
	if !microvmIsFile(filepath.Join(layout.VM(name), "root.ext4")) {
		return fmt.Errorf("container %s has no root disk; it was not created completely, remove it", name)
	}
	MicrovmInstallInterruptHandlers()
	if err := microvmLaunch(layout, state); err != nil {
		stopped := microvmStopUnit(name)
		if stopped == nil {
			stopped = MicrovmReleaseHostResources(layout, state.Slot)
		}
		if stopped == nil {
			return err
		}
		return fmt.Errorf("stopping the failed launch also failed: %s: %w", stopped.Error(), err)
	}
	return nil
}

// microvmFirecrackerConfig: the paths in a jailed VM's Firecracker
// configuration are relative to the jail: the engine hard-links the kernel,
// initrd and disks to these names.
func microvmFirecrackerConfig(layout MicrovmLayout, state MicrovmVmState, link MicrovmLink) (jsonvalue.Object, error) {
	drives := []any{jsonvalue.Object{
		{Key: "drive_id", Value: "rootfs"},
		{Key: "path_on_host", Value: "/root.ext4"},
		{Key: "is_root_device", Value: true},
		{Key: "is_read_only", Value: false},
	}}
	// (path depth, entry): init mounts shallow destinations first so a
	// volume at /workspace cannot hide one at /workspace/sub.
	type mapped struct {
		depth int
		entry string
	}
	var volumeMap []mapped
	for index, volume := range state.Volumes {
		device := fmt.Sprintf("vd%c", rune('b'+index))
		// `new` until a launch has started the container with the volume
		// attached; only then is the volume's content its own. A new
		// volume's drive is writable even when it will be mounted
		// read-only, so init can fill it from the image first.
		fresh := "new"
		if microvmExists(filepath.Join(layout.Volume(volume.Name), "initialized")) {
			fresh = "old"
		}
		drives = append(drives, jsonvalue.Object{
			{Key: "drive_id", Value: fmt.Sprintf("vol%d", index)},
			{Key: "path_on_host", Value: fmt.Sprintf("/vol%d.ext4", index)},
			{Key: "is_root_device", Value: false},
			{Key: "is_read_only", Value: !volume.ReadWrite && fresh == "old"},
		})
		access := "ro"
		if volume.ReadWrite {
			access = "rw"
		}
		volumeMap = append(volumeMap, mapped{
			depth: strings.Count(volume.Destination, "/"),
			entry: fmt.Sprintf("%s:%s:%s:%s", device, volume.Destination, access, fresh),
		})
	}
	sort.SliceStable(volumeMap, func(i, j int) bool { return volumeMap[i].depth < volumeMap[j].depth })
	entries := make([]string, len(volumeMap))
	for i, volume := range volumeMap {
		entries[i] = volume.entry
	}
	bootArgs := fmt.Sprintf("panic=1 initcall_blacklist=i8042_init sbx.name=%s sbx.ip=%s sbx.gw=%s sbx.dns=%s sbx.vols=%s",
		state.Name, link.GuestCIDR(), link.Host, MicrovmGuestDNS, strings.Join(entries, ","))
	return jsonvalue.Object{
		{Key: "boot-source", Value: jsonvalue.Object{
			{Key: "kernel_image_path", Value: "/vmlinuz"},
			{Key: "initrd_path", Value: "/initrd.img"},
			{Key: "boot_args", Value: bootArgs},
		}},
		{Key: "machine-config", Value: jsonvalue.Object{
			{Key: "vcpu_count", Value: uint64(state.VCPUs)},
			{Key: "mem_size_mib", Value: (state.MemoryBytes + 1<<20 - 1) >> 20},
		}},
		{Key: "drives", Value: drives},
		{Key: "network-interfaces", Value: []any{jsonvalue.Object{
			{Key: "iface_id", Value: "eth0"},
			{Key: "host_dev_name", Value: link.Tap},
			{Key: "guest_mac", Value: link.MAC},
		}}},
		{Key: "vsock", Value: jsonvalue.Object{
			{Key: "guest_cid", Value: uint64(3)},
			{Key: "uds_path", Value: "/v.sock"},
		}},
		{Key: "enclave", Value: jsonvalue.Object{
			{Key: "direct_mode", Value: true},
			{Key: "cpu_overcommit", Value: true},
			{Key: "debug_mode", Value: true},
		}},
	}, nil
}

// microvmPrepareJail builds the VM's jail: its root directory holds hard
// links to the kernel, the initrd and the disks, and the Firecracker
// configuration, all owned by the VM's user. The jailer adds the device
// nodes it needs (including `/dev/nitro_enclaves`) and its own copy of
// Firecracker when it starts. Hard links, so a disk is the same file inside
// and outside the jail and nothing is copied; they need the jail on the
// filesystem that holds the disks.
func microvmPrepareJail(layout MicrovmLayout, state MicrovmVmState, boot *MicrovmBootAssets, link MicrovmLink) error {
	uid, gid := microvmJailIDs(state.Slot)
	// Left over from a crash, or from the previous run of this VM.
	if err := MicrovmRemoveJail(layout, state.Slot); err != nil {
		return err
	}
	root := layout.JailRoot(state.Slot)
	if err := os.MkdirAll(root, 0o777); err != nil {
		return fmt.Errorf("create %s: %w", root, microvmIOError(err))
	}
	hardLink := func(source string, name string) error {
		target := filepath.Join(root, name)
		if err := os.Link(source, target); err != nil {
			return fmt.Errorf("link %s into the jail as %s (the jail and the disks must be on one filesystem): %w", source, target, microvmIOError(err))
		}
		return nil
	}
	// The disks are private to the VM's user. They are shared inodes, so
	// the ownership also holds outside the jail; the next launch sets it
	// again.
	type disk struct{ name, source string }
	disks := []disk{{"root.ext4", filepath.Join(layout.VM(state.Name), "root.ext4")}}
	for index, mounted := range state.Volumes {
		disks = append(disks, disk{fmt.Sprintf("vol%d.ext4", index), MicrovmVolumeDiskPath(layout, mounted.Name)})
	}
	for _, d := range disks {
		if err := os.Chown(d.source, int(uid), int(gid)); err != nil {
			return fmt.Errorf("give %s to the VM's user: %w", d.source, microvmIOError(err))
		}
		if err := os.Chmod(d.source, 0o600); err != nil {
			return fmt.Errorf("restrict %s: %w", d.source, microvmIOError(err))
		}
		if err := hardLink(d.source, d.name); err != nil {
			return err
		}
	}
	if err := hardLink(boot.Kernel, "vmlinuz"); err != nil {
		return err
	}
	if err := hardLink(boot.Initrd, "initrd.img"); err != nil {
		return err
	}
	config := filepath.Join(root, "fc.json")
	firecracker, err := microvmFirecrackerConfig(layout, state, link)
	if err != nil {
		return err
	}
	if err := MicrovmWriteJSONAtomic(config, firecracker); err != nil {
		return err
	}
	if err := os.Chown(config, int(uid), int(gid)); err != nil {
		return fmt.Errorf("give %s to the VM's user: %w", config, microvmIOError(err))
	}
	return nil
}

// MicrovmRemoveJail deletes a VM's jail. The links go; the disks they point
// to stay.
func MicrovmRemoveJail(layout MicrovmLayout, slot uint32) error {
	directory := layout.JailDir(slot)
	if err := os.RemoveAll(directory); err != nil {
		return fmt.Errorf("remove %s: %w", directory, microvmIOError(err))
	}
	return nil
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
	boot, err := MicrovmBootEnsure(layout)
	if err != nil {
		return err
	}
	defer boot.Reservation.Drop()
	link, err := MicrovmLinkForSlot(state.Slot)
	if err != nil {
		return err
	}
	firecracker, err := MicrovmFindTool("firecracker")
	if err != nil {
		return err
	}
	jailer, err := MicrovmFindTool("jailer")
	if err != nil {
		return err
	}
	engine, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate the engine binary: %w", err)
	}
	if !utf8.ValidString(engine) {
		return errors.New("engine path is not UTF-8")
	}
	if strings.ContainsFunc(engine, unicode.IsSpace) || strings.Contains(engine, `"`) {
		return fmt.Errorf("the engine path %s contains whitespace or quotes, which a systemd unit cannot carry", microvmDebugString(engine))
	}
	uid, gid := microvmJailIDs(state.Slot)
	started := func() error {
		global, err := layout.LockGlobal()
		if err != nil {
			return err
		}
		defer global.Drop()
		if err := MicrovmCheckInterrupted(); err != nil {
			return err
		}
		if err := MicrovmCheckNoRouteOverlap(); err != nil {
			return err
		}
		if err := MicrovmApplyHostPolicy(); err != nil {
			return err
		}
		if err := microvmAdmit(layout, *state); err != nil {
			return err
		}
		// From here the VM is jailed, including one an earlier engine
		// launched without the jailer; the state says where its socket is.
		if !state.Jailed || state.Format < MicrovmStateFormat {
			state.Jailed = true
			state.Format = MicrovmStateFormat
			if err := state.Write(layout); err != nil {
				return err
			}
		}
		if err := microvmPrepareJail(layout, *state, boot, link); err != nil {
			return err
		}
		if err := MicrovmTapUp(link, MicrovmOwner{UID: uid, GID: gid}); err != nil {
			return err
		}
		unit := MicrovmUnitName(state.Name)
		stopPost := fmt.Sprintf("ExecStopPost=%s microvm host-jail-down %d", engine, state.Slot)
		description := "Nitro Sandbox " + state.Name
		if _, err := MicrovmRunTool("systemd-run", []string{
			"--quiet",
			"--collect",
			"--unit=" + strings.TrimSuffix(unit, ".service"),
			"--description=" + description,
			"--property=KillMode=control-group",
			"--property=TimeoutStopSec=30",
			"--property=" + stopPost,
			jailer,
			"--id",
			MicrovmJailID(state.Slot),
			"--exec-file",
			firecracker,
			"--uid",
			strconv.FormatUint(uint64(uid), 10),
			"--gid",
			strconv.FormatUint(uint64(gid), 10),
			"--chroot-base-dir",
			layout.JailBase(),
			"--",
			"--no-api",
			"--config-file",
			"/fc.json",
		}); err != nil {
			return fmt.Errorf("start the VM unit: %w", err)
		}
		return nil
	}()
	if started != nil {
		return started
	}
	env := append([]string{}, state.Env...)
	if state.MbxRemote != nil {
		kept := env[:0]
		for _, entry := range env {
			if !strings.HasPrefix(entry, core.MbxRemoteEnvPrefix) {
				kept = append(kept, entry)
			}
		}
		env = kept
	}
	// The remote cache is optional: a proxy that cannot start leaves the VM
	// with its guest-local cache and no remote settings, and says so.
	if remote := state.MbxRemote; remote != nil {
		if err := microvmMbxProxyStartUnit(layout, *state, link, boot); err != nil {
			// The provisioning controller reads this warning as one line.
			lines := microvmLines(err.Error())
			for i, line := range lines {
				lines[i] = strings.TrimSpace(line)
			}
			fmt.Fprintf(os.Stderr, "warning: %s starts without the remote mbx cache: %s\n", state.Name, strings.Join(lines, " "))
		} else {
			env = append(env,
				fmt.Sprintf("MBX_REMOTE_URL=http://%s:%d", link.Host, microvmMbxProxyPort),
				"MBX_REMOTE_NAMESPACE="+remote.Namespaces[0],
				"MBX_REMOTE_MODE=read-only",
			)
		}
	}
	client, err := microvmWaitForAgent(state.Name, MicrovmAgentSocket(layout, *state))
	if err != nil {
		return err
	}
	// The agent starts after init has filled any new volume, and before the
	// command can write to one. Record that now, durably: a start that is
	// interrupted from here on must never be filled again over the
	// command's data.
	for _, mounted := range state.Volumes {
		marker := filepath.Join(layout.Volume(mounted.Name), "initialized")
		file, err := os.Create(marker)
		if err != nil {
			_ = client.transport.Close()
			return fmt.Errorf("record %s: %w", marker, microvmIOError(err))
		}
		err = file.Sync()
		_ = file.Close()
		if err != nil {
			_ = client.transport.Close()
			return fmt.Errorf("sync %s: %w", marker, microvmIOError(err))
		}
		// The directory entry too, or a crash could lose the file itself.
		if err := microvmSyncDirectory(layout.Volume(mounted.Name)); err != nil {
			_ = client.transport.Close()
			return fmt.Errorf("sync the directory of %s: %w", marker, errors.Unwrap(err))
		}
	}
	argv := append(append([]string{}, state.Entrypoint...), state.Cmd...)
	cwd := state.WorkingDir
	if cwd == "" {
		cwd = "/"
	}
	volumes := make([]MicrovmVolumeSpec, 0, len(state.Volumes))
	for _, volume := range state.Volumes {
		volumes = append(volumes, MicrovmVolumeSpec{Destination: volume.Destination, Chown: volume.Chown})
	}
	if _, err := client.Start(MicrovmStartRequest{
		Argv:    argv,
		Env:     env,
		Cwd:     cwd,
		User:    state.User,
		Volumes: volumes,
	}); err != nil {
		return fmt.Errorf("start the container command: %w", err)
	}
	return nil
}

// microvmWaitForAgent polls until the guest agent answers. Ends when the
// unit dies or a signal arrives.
func microvmWaitForAgent(name string, socket string) (*MicrovmClient, error) {
	for {
		if err := MicrovmCheckInterrupted(); err != nil {
			return nil, err
		}
		activity, err := MicrovmVMActivity(name)
		if err != nil {
			return nil, err
		}
		if activity == MicrovmActivityStopped {
			return nil, fmt.Errorf("the VM stopped before its guest agent answered:\n%s", microvmUnitJournal(MicrovmUnitName(name)))
		}
		if microvmExists(socket) {
			if client, err := MicrovmOpenAgent(socket); err == nil {
				return client, nil
			}
		}
		time.Sleep(50 * time.Millisecond)
	}
}

// microvmUnitJournal is the last lines a systemd unit logged.
func microvmUnitJournal(unit string) string {
	journalctl, err := MicrovmFindTool("journalctl")
	if err == nil {
		var output core.ProcessOutput
		output, err = MicrovmRunUnchecked(journalctl, []string{"-u", unit, "-n", "30", "--no-pager", "-o", "cat"})
		if err == nil {
			return microvmLossy(output.Stdout)
		}
	}
	return fmt.Sprintf("(journal unavailable: %s)", err.Error())
}

// MicrovmOpenAgent connects to a running VM's guest agent.
func MicrovmOpenAgent(socket string) (*MicrovmClient, error) {
	stream, err := MicrovmConnectUnix(socket, MicrovmAgentPort)
	if err != nil {
		return nil, err
	}
	client, err := MicrovmHandshake(MicrovmUnixTransport{UnixConn: stream})
	if err != nil {
		return nil, err
	}
	// The attempt's deadline covered the handshake; the request has none.
	if err := stream.SetDeadline(time.Time{}); err != nil {
		_ = stream.Close()
		return nil, microvmIOError(err)
	}
	return client, nil
}

// MicrovmAgentFor is the connected agent of a running VM, or the
// Podman-style reason there is none.
func MicrovmAgentFor(layout MicrovmLayout, name string) (MicrovmVmState, *MicrovmClient, error) {
	name, err := MicrovmVMResolve(layout, name)
	if err != nil {
		return MicrovmVmState{}, nil, err
	}
	if err := MicrovmValidateName("container", name); err != nil {
		return MicrovmVmState{}, nil, err
	}
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return MicrovmVmState{}, nil, err
	}
	if state == nil {
		return MicrovmVmState{}, nil, microvmNoSuchContainerError(name)
	}
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return MicrovmVmState{}, nil, err
	}
	if activity != MicrovmActivityRunning {
		return MicrovmVmState{}, nil, errors.New("can only use a running container: container state improper")
	}
	client, err := MicrovmOpenAgent(MicrovmAgentSocket(layout, *state))
	if err != nil {
		return MicrovmVmState{}, nil, fmt.Errorf("connect to the guest agent of %s: %w", name, err)
	}
	return *state, client, nil
}

// MicrovmVMStop stops a VM: flush the guest's filesystems, then end its
// unit.
//
// The guest gets grace to flush and power off; without it a stop would lose
// whatever the guest had not yet written. Stopping an absent or stopped VM
// succeeds only with ignoreMissing for the absent case.
func MicrovmVMStop(layout MicrovmLayout, name string, grace time.Duration, ignoreMissing bool) error {
	name, err := MicrovmVMResolve(layout, name)
	if err != nil {
		return err
	}
	if err := MicrovmValidateName("container", name); err != nil {
		return err
	}
	vmLock, err := layout.LockVM(name)
	if err != nil {
		return err
	}
	defer vmLock.Drop()
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return err
	}
	if state != nil {
		socket := MicrovmAgentSocket(layout, *state)
		return microvmStopLocked(name, &socket, grace)
	}
	if err := layout.ForgetVMLock(name, vmLock); err != nil {
		return err
	}
	if ignoreMissing {
		return nil
	}
	return microvmNoSuchContainerError(name)
}

// microvmStopLocked stops the unit of name, first asking the guest at
// socket (nil without a state) to flush.
func microvmStopLocked(name string, socket *string, grace time.Duration) error {
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return err
	}
	// Without a state there is no socket, and so no guest to ask to flush.
	if activity != MicrovmActivityRunning || socket == nil {
		return microvmStopUnit(name)
	}
	target := *socket
	flushed := make(chan error, 1)
	finished := make(chan struct{})
	go func() {
		defer close(finished)
		client, err := MicrovmOpenAgent(target)
		if err == nil {
			err = client.Stop()
		}
		// The receiver stops listening when the grace period ends.
		flushed <- err
	}()
	timer := time.NewTimer(grace)
	select {
	case err := <-flushed:
		timer.Stop()
		if err != nil {
			slog.Warn("guest flush failed; stopping the VM anyway", "error", err, "container", name)
		}
	case <-timer.C:
		slog.Warn("guest flush did not finish within the grace period; stopping the VM anyway", "container", name)
	}
	if err := microvmStopUnit(name); err != nil {
		return err
	}
	// Stopping the unit closed the guest's connection, so the flush is done.
	<-finished
	return nil
}

// microvmTeardown stops the unit and deletes the VM's directory and TAP.
// Idempotent; no state is required.
func microvmTeardown(layout MicrovmLayout, name string) error {
	if err := microvmStopUnit(name); err != nil {
		return err
	}
	if state, err := MicrovmVmStateRead(layout, name); err == nil && state != nil {
		if err := MicrovmReleaseHostResources(layout, state.Slot); err != nil {
			return err
		}
	}
	directory := layout.VM(name)
	if microvmExists(directory) {
		// Hide the VM atomically, then delete: a crash leaves scratch, never
		// a half VM.
		doomed := filepath.Join(layout.VMs(), fmt.Sprintf(".removing-%s-%s", name, MicrovmScratchSuffix()))
		if err := os.Rename(directory, doomed); err != nil {
			return fmt.Errorf("unpublish %s: %w", directory, microvmIOError(err))
		}
		if err := os.RemoveAll(doomed); err != nil {
			return fmt.Errorf("remove %s: %w", doomed, microvmIOError(err))
		}
	}
	return nil
}

// MicrovmVMRemove removes a VM. force stops a running one first; volumes
// also removes its named volumes.
func MicrovmVMRemove(layout MicrovmLayout, name string, force bool, ignoreMissing bool, volumes bool) error {
	name, err := MicrovmVMResolve(layout, name)
	if err != nil {
		return err
	}
	if err := MicrovmValidateName("container", name); err != nil {
		return err
	}
	if err := microvmRequireRoot(); err != nil {
		return err
	}
	vmLock, err := layout.LockVM(name)
	if err != nil {
		return err
	}
	defer vmLock.Drop()
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return err
	}
	if state == nil && !microvmExists(layout.VM(name)) {
		if err := layout.ForgetVMLock(name, vmLock); err != nil {
			return err
		}
		if ignoreMissing {
			return nil
		}
		return microvmNoSuchContainerError(name)
	}
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return err
	}
	if activity != MicrovmActivityStopped {
		if !force {
			return fmt.Errorf("cannot remove running container %s: stop it first, or use --force", name)
		}
		var socket *string
		if state != nil {
			path := MicrovmAgentSocket(layout, *state)
			socket = &path
		}
		if err := microvmStopLocked(name, socket, time.Duration(MicrovmDefaultStopSeconds)*time.Second); err != nil {
			return err
		}
	}
	if err := microvmTeardown(layout, name); err != nil {
		return err
	}
	if volumes && state != nil {
		for _, mounted := range state.Volumes {
			if err := MicrovmVolumeRemove(layout, mounted.Name, true); err != nil {
				return err
			}
		}
	}
	// The VM is gone, and its lock file goes with it.
	return layout.ForgetVMLock(name, vmLock)
}

// MicrovmVMInspect is the Podman-shaped description of a VM.
func MicrovmVMInspect(layout MicrovmLayout, name string) (jsonvalue.Object, error) {
	name, err := MicrovmVMResolve(layout, name)
	if err != nil {
		return nil, err
	}
	if err := MicrovmValidateName("container", name); err != nil {
		return nil, err
	}
	read, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return nil, err
	}
	if read == nil {
		return nil, microvmNoSuchContainerError(name)
	}
	state, err := MicrovmReplaceRootObserved(layout, name, *read)
	if err != nil {
		return nil, err
	}
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return nil, err
	}
	status := "exited"
	switch activity {
	case MicrovmActivityRunning:
		status = "running"
	case MicrovmActivityStopping:
		status = "stopping"
	}
	mounts := make([]any, 0, len(state.Volumes))
	for _, volume := range state.Volumes {
		mounts = append(mounts, jsonvalue.Object{
			{Key: "Type", Value: "volume"},
			{Key: "Name", Value: volume.Name},
			{Key: "Source", Value: layout.Volume(volume.Name)},
			{Key: "Destination", Value: volume.Destination},
			{Key: "Driver", Value: "local"},
			{Key: "RW", Value: volume.ReadWrite},
		})
	}
	isolation := "none"
	if state.Jailed {
		isolation = "jailer"
	}
	return jsonvalue.Object{
		{Key: "Id", Value: state.ID},
		{Key: "Name", Value: state.Name},
		{Key: "Created", Value: state.Created},
		{Key: "Image", Value: state.ImageID},
		{Key: "ImageName", Value: state.ImageReference},
		{Key: "State", Value: jsonvalue.Object{
			{Key: "Status", Value: status},
			{Key: "Running", Value: activity == MicrovmActivityRunning},
			{Key: "Paused", Value: false},
		}},
		{Key: "Config", Value: jsonvalue.Object{
			{Key: "Image", Value: state.ImageReference},
			{Key: "Labels", Value: microvmLabelsObject(state.Labels)},
			{Key: "User", Value: state.User},
			{Key: "Env", Value: microvmStringValues(state.Env)},
			{Key: "WorkingDir", Value: state.WorkingDir},
			{Key: "Cmd", Value: microvmStringValues(state.Cmd)},
			{Key: "Entrypoint", Value: microvmStringValues(state.Entrypoint)},
		}},
		{Key: "HostConfig", Value: jsonvalue.Object{
			{Key: "Memory", Value: state.MemoryBytes},
			{Key: "NanoCpus", Value: state.NanoCPUs},
			{Key: "AutoRemove", Value: false},
			{Key: "Privileged", Value: false},
			{Key: "ReadonlyRootfs", Value: false},
			{Key: "NetworkMode", Value: "nitro"},
			{Key: "Isolation", Value: isolation},
		}},
		{Key: "Mounts", Value: mounts},
	}, nil
}

// MicrovmVMExists is the state for `container exists`.
func MicrovmVMExists(layout MicrovmLayout, name string) (bool, error) {
	name, err := MicrovmVMResolve(layout, name)
	if err != nil {
		return false, err
	}
	if err := MicrovmValidateName("container", name); err != nil {
		return false, err
	}
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return false, err
	}
	return state != nil, nil
}

// MicrovmVMInfo is what `info` reports.
func MicrovmVMInfo(layout MicrovmLayout) (jsonvalue.Object, error) {
	pool, err := microvmReadPool()
	if err != nil {
		return nil, err
	}
	mib := func(bytes uint64) uint64 { return bytes >> 20 }
	toolPath := func(name string) any {
		path, err := MicrovmFindTool(name)
		if err != nil {
			return nil
		}
		return path
	}
	var firecrackerVersion any
	if path, err := MicrovmFindTool("firecracker"); err == nil {
		if output, err := MicrovmRunUnchecked(path, []string{"--version"}); err == nil {
			if lines := microvmLines(microvmLossy(output.Stdout)); len(lines) > 0 {
				line := lines[0]
				for strings.HasPrefix(line, "Firecracker") {
					line = strings.TrimPrefix(line, "Firecracker")
				}
				firecrackerVersion = strings.TrimLeft(strings.TrimSpace(line), "v")
			}
		}
	}
	var crane, craneVersion any
	if path, err := MicrovmFindTool("crane"); err == nil {
		crane = path
		if output, err := MicrovmRunChecked(path, []string{"version"}); err == nil {
			if lines := microvmLines(microvmLossy(output.Stdout)); len(lines) > 0 {
				craneVersion = strings.TrimSpace(lines[0])
			}
		}
	}
	running := uint32(0)
	committed := uint64(0)
	states, err := MicrovmVmStateList(layout)
	if err != nil {
		return nil, err
	}
	for _, vm := range states {
		activity, err := MicrovmVMActivity(vm.Name)
		if err != nil {
			return nil, err
		}
		if activity != MicrovmActivityStopped {
			running++
			committed += vm.MemoryBytes + microvmPoolOverheadBytes
		}
	}
	kernel, err := MicrovmKernelRelease()
	if err != nil {
		return nil, err
	}
	architecture, err := MicrovmHostArchitecture()
	if err != nil {
		return nil, err
	}
	var poolTotal, poolFree uint64
	var slotsTotal, slotsEnabled uint32
	if pool != nil {
		poolTotal, poolFree = mib(pool.total), mib(pool.free)
		slotsTotal, slotsEnabled = pool.slotsTotal, pool.slots
	}
	return jsonvalue.Object{
		{Key: "engine", Value: "mj-microvm"},
		{Key: "engine_version", Value: core.PackageVersion},
		{Key: "engine_build", Value: core.WorkerBuildID()},
		{Key: "state_format", Value: uint64(MicrovmStateFormat)},
		{Key: "protocol_version", Value: uint64(MicrovmProtocolVersion)},
		{Key: "state_root", Value: layout.Root()},
		{Key: "kernel_version", Value: kernel},
		{Key: "architecture", Value: architecture},
		{Key: "cpus", Value: uint64(microvmAvailableParallelism())},
		{Key: "nitro_enclaves", Value: pool != nil},
		{Key: "memory_pool_total_mib", Value: poolTotal},
		{Key: "memory_pool_free_mib", Value: poolFree},
		{Key: "memory_committed_mib", Value: mib(committed)},
		{Key: "slots_in_use", Value: uint64(running)},
		{Key: "slots_total", Value: uint64(slotsTotal)},
		{Key: "slots_enabled", Value: uint64(slotsEnabled)},
		{Key: "firecracker_path", Value: toolPath("firecracker")},
		{Key: "firecracker_version", Value: firecrackerVersion},
		{Key: "crane_path", Value: crane},
		{Key: "crane_version", Value: craneVersion},
		{Key: "jailer_path", Value: toolPath("jailer")},
		{Key: "notes", Value: []any{
			"--cap-add, --cap-drop and --security-opt are accepted and have no effect: the VM is the isolation boundary",
			"--user sets the default user of the container command and of exec",
			"volume option U changes the owner of everything in the volume, as Podman does",
			"inspect has no State.ExitCode: the guest powers off when the command exits and the status is not kept",
		}},
	}, nil
}
