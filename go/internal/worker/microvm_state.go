//go:build linux

// Port of mj-worker/src/microvm/state.rs.
//
// The engine's on-disk state: layout, formats, names, and locks.
//
// VMs and their guest agents outlive engine upgrades, so every file here is
// versioned and a newer engine reads every older format. An older engine
// that meets a newer format refuses to touch it and says why.

package worker

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MicrovmStateFormat is the format of `state.json` this build writes.
// Format 2 added `jailed`; format 1 files (VMs launched before the jailer)
// still read, as `jailed: false`.
const MicrovmStateFormat uint32 = 2

// MicrovmMetadataFormat is the format of image and volume metadata files
// this build writes.
const MicrovmMetadataFormat uint32 = 1

// MicrovmDefaultRoot is the engine's state root.
const MicrovmDefaultRoot = "/var/lib/mj-microvm"

// MicrovmLayout is where everything lives.
type MicrovmLayout struct {
	root string
}

// NewMicrovmLayout is Layout::new.
func NewMicrovmLayout(root string) MicrovmLayout {
	return MicrovmLayout{root: root}
}

// MicrovmSystemLayout is Layout::system: the layout under
// MicrovmDefaultRoot.
func MicrovmSystemLayout() MicrovmLayout {
	return NewMicrovmLayout(MicrovmDefaultRoot)
}

func (l MicrovmLayout) Root() string { return l.root }

func (l MicrovmLayout) Images() string { return filepath.Join(l.root, "images") }

func (l MicrovmLayout) Image(id string) string { return filepath.Join(l.Images(), id) }

func (l MicrovmLayout) ImageRefs() string { return filepath.Join(l.Images(), "refs.json") }

func (l MicrovmLayout) VMs() string { return filepath.Join(l.root, "vms") }

func (l MicrovmLayout) VM(name string) string { return filepath.Join(l.VMs(), name) }

func (l MicrovmLayout) Volumes() string { return filepath.Join(l.root, "volumes") }

func (l MicrovmLayout) Volume(name string) string { return filepath.Join(l.Volumes(), name) }

func (l MicrovmLayout) Boot() string { return filepath.Join(l.root, "boot") }

// JailBase is the jailer's chroot base. It must be on the filesystem that
// holds the disks, because the jail holds hard links to them.
func (l MicrovmLayout) JailBase() string { return filepath.Join(l.root, "jail") }

// MicrovmJailID is Layout::jail_id: the jailer's id for a network slot
// (unique among existing VMs).
func MicrovmJailID(slot uint32) string { return fmt.Sprintf("vm%d", slot) }

// JailDir is `<base>/firecracker/<id>`: the jailer puts the chroot under
// the name of the executable it runs.
func (l MicrovmLayout) JailDir(slot uint32) string {
	return filepath.Join(l.JailBase(), "firecracker", MicrovmJailID(slot))
}

// JailRoot is the jailed process's `/`.
func (l MicrovmLayout) JailRoot(slot uint32) string { return filepath.Join(l.JailDir(slot), "root") }

func (l MicrovmLayout) Locks() string { return filepath.Join(l.root, "locks") }

// Ensure creates the directory tree. Safe to repeat.
func (l MicrovmLayout) Ensure() error {
	panic("unported: mj-worker/src/microvm/state.rs Layout::ensure")
}

// LockGlobal takes the engine-wide lock: slot allocation, admission, image
// references.
func (l MicrovmLayout) LockGlobal() (*core.FileLock, error) {
	return l.lockFile("global.lock")
}

// LockVM takes the lock for one VM name, held across every mutation of
// that VM.
func (l MicrovmLayout) LockVM(name string) (*core.FileLock, error) {
	if err := MicrovmValidateName("container", name); err != nil {
		return nil, err
	}
	return l.lockFile(fmt.Sprintf("vm-%s.lock", name))
}

// LockVolume takes the lock for one volume name.
func (l MicrovmLayout) LockVolume(name string) (*core.FileLock, error) {
	if err := MicrovmValidateName("volume", name); err != nil {
		return nil, err
	}
	return l.lockFile(fmt.Sprintf("volume-%s.lock", name))
}

// LockImage takes the lock for one image id.
func (l MicrovmLayout) LockImage(id string) (*core.FileLock, error) {
	panic("unported: mj-worker/src/microvm/state.rs Layout::lock_image")
}

// LockBootShared reserves a boot-assets directory: held (shared) from
// before a launch names the assets until Firecracker has opened them, so a
// prune by another engine build cannot remove them in between.
func (l MicrovmLayout) LockBootShared(tag string) (*core.FileLock, error) {
	panic("unported: mj-worker/src/microvm/state.rs Layout::lock_boot_shared")
}

// TryLockBootExclusive takes the exclusive lock on boot assets, or returns
// nil while a launch holds them.
func (l MicrovmLayout) TryLockBootExclusive(tag string) (*core.FileLock, error) {
	panic("unported: mj-worker/src/microvm/state.rs Layout::try_lock_boot_exclusive")
}

func (l MicrovmLayout) openLockFile(fileName string) (*os.File, error) {
	panic("unported: mj-worker/src/microvm/state.rs Layout::open_lock_file")
}

// lockFile locks the named file. A resource's lock file is deleted with the
// resource (see the Forget* methods), by a holder of the lock; a locker
// that was waiting then holds a lock on an unlinked file, which excludes
// nobody. So after acquiring, check that the file held is still the one at
// the path, and start over if not. Each retry follows a delete by another
// process, so the loop cannot spin.
func (l MicrovmLayout) lockFile(fileName string) (*core.FileLock, error) {
	panic("unported: mj-worker/src/microvm/state.rs Layout::lock_file")
}

// ForgetVMLock deletes a VM's lock file with the VM. The caller holds the
// VM's lock and must do nothing more to the VM afterwards: a locker
// arriving after the delete creates a new file and does not wait for the
// holder.
func (l MicrovmLayout) ForgetVMLock(name string, held *core.FileLock) error {
	return l.forgetLock(fmt.Sprintf("vm-%s.lock", name), held)
}

// ForgetVolumeLock deletes a volume's lock file with the volume. The caller
// holds its lock.
func (l MicrovmLayout) ForgetVolumeLock(name string, held *core.FileLock) error {
	return l.forgetLock(fmt.Sprintf("volume-%s.lock", name), held)
}

// ForgetImageLock deletes an image's lock file with the image. The caller
// holds its lock.
func (l MicrovmLayout) ForgetImageLock(id string, held *core.FileLock) error {
	return l.forgetLock(fmt.Sprintf("image-%s.lock", id), held)
}

// forgetLock unlinks a lock file while its lock is held; held is the proof.
func (l MicrovmLayout) forgetLock(fileName string, held *core.FileLock) error {
	path := filepath.Join(l.Locks(), fileName)
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove %s: %w", path, microvmIOError(err))
	}
	return nil
}

// microvmLockFileIsLinked reports whether file is still the file at path
// (same device and inode).
func microvmLockFileIsLinked(file *os.File, path string) (bool, error) {
	panic("unported: mj-worker/src/microvm/state.rs lock_file_is_linked")
}

// MicrovmValidateName checks a container or volume name. Names reach file
// paths, unit names and kernel command lines, so they are restricted to a
// safe alphabet.
func MicrovmValidateName(kind, name string) error {
	panic("unported: mj-worker/src/microvm/state.rs validate_name")
}

// MicrovmValidateLabel checks a label. Labels are metadata a person or the
// controller attaches; keys are restricted the way container engines
// restrict them.
func MicrovmValidateLabel(key, value string) error {
	panic("unported: mj-worker/src/microvm/state.rs validate_label")
}

// MicrovmVmVolume is a volume mounted into a VM.
type MicrovmVmVolume struct {
	Name        string `json:"name"`
	Destination string `json:"destination"`
	ReadWrite   bool   `json:"read_write"`
	// Chown is Podman's `U` option.
	Chown bool `json:"chown"`
}

// UnmarshalJSON requires the members serde requires.
func (v *MicrovmVmVolume) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "name", "destination", "read_write"); err != nil {
		return err
	}
	type plain MicrovmVmVolume
	return json.Unmarshal(data, (*plain)(v))
}

// MicrovmVmState is everything the engine remembers about a VM. Fields
// added in later formats default when absent so older files still load.
type MicrovmVmState struct {
	Format         uint32            `json:"format"`
	ID             string            `json:"id"`
	Name           string            `json:"name"`
	Created        string            `json:"created"`
	ImageReference string            `json:"image_reference"`
	ImageID        string            `json:"image_id"`
	Labels         map[string]string `json:"labels"`
	VCPUs          uint32            `json:"vcpus"`
	NanoCPUs       int64             `json:"nano_cpus"`
	MemoryBytes    uint64            `json:"memory_bytes"`
	// Env is the environment of the container: the image's plus `--env`.
	Env        []string          `json:"env"`
	User       string            `json:"user"`
	WorkingDir string            `json:"working_dir"`
	Entrypoint []string          `json:"entrypoint"`
	Cmd        []string          `json:"cmd"`
	Volumes    []MicrovmVmVolume `json:"volumes"`
	// Slot is the network slot: it selects the TAP device and the /30
	// subnet.
	Slot uint32 `json:"slot"`
	// EngineBuild is the engine build that created the VM.
	EngineBuild string `json:"engine_build"`
	// Jailed is whether the VM's Firecracker runs under the jailer. VMs
	// launched by an engine before the jailer are false until they are next
	// started.
	Jailed bool `json:"jailed"`
}

// MarshalJSON writes empty lists and maps as `[]` and `{}`, as serde does.
func (s MicrovmVmState) MarshalJSON() ([]byte, error) {
	type plain MicrovmVmState
	p := plain(s)
	p.Labels = microvmNonNilMap(p.Labels)
	p.Env, p.Entrypoint, p.Cmd = microvmNonNil(p.Env), microvmNonNil(p.Entrypoint), microvmNonNil(p.Cmd)
	p.Volumes = microvmNonNil(p.Volumes)
	return json.Marshal(p)
}

// UnmarshalJSON requires every member but `jailed`, as serde does.
func (s *MicrovmVmState) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "format", "id", "name", "created", "image_reference",
		"image_id", "labels", "vcpus", "nano_cpus", "memory_bytes", "env", "user", "working_dir",
		"entrypoint", "cmd", "volumes", "slot", "engine_build"); err != nil {
		return err
	}
	type plain MicrovmVmState
	return json.Unmarshal(data, (*plain)(s))
}

// MicrovmVmStatePath is VmState::path.
func MicrovmVmStatePath(layout MicrovmLayout, name string) string {
	return filepath.Join(layout.VM(name), "state.json")
}

// Write writes the state file.
func (s MicrovmVmState) Write(layout MicrovmLayout) error {
	return MicrovmWriteJSONAtomic(MicrovmVmStatePath(layout, s.Name), s)
}

// MicrovmVmStateRead is the state of name, or nil when there is no such VM.
func MicrovmVmStateRead(layout MicrovmLayout, name string) (*MicrovmVmState, error) {
	panic("unported: mj-worker/src/microvm/state.rs VmState::read")
}

// MicrovmVmStateParse decodes a `state.json`, refusing a format newer than
// this build knows.
func MicrovmVmStateParse(data []byte) (MicrovmVmState, error) {
	panic("unported: mj-worker/src/microvm/state.rs VmState::parse")
}

// MicrovmVmStateList is every VM with a state file, in name order.
func MicrovmVmStateList(layout MicrovmLayout) ([]MicrovmVmState, error) {
	panic("unported: mj-worker/src/microvm/state.rs VmState::list")
}

// MicrovmRefuseNewerFormat fails with a clear message when a file was
// written by a newer engine. value is the file decoded as JSON.
func MicrovmRefuseNewerFormat(what string, value any, supported uint32) error {
	panic("unported: mj-worker/src/microvm/state.rs refuse_newer_format")
}

// MicrovmWriteJSONAtomic writes value to path through a temporary file in
// the same directory, as serde_json's pretty writer does, with a newline.
func MicrovmWriteJSONAtomic(path string, value any) error {
	panic("unported: mj-worker/src/microvm/state.rs write_json_atomic")
}

// MicrovmVolumeMetadata is a volume's metadata.
type MicrovmVolumeMetadata struct {
	Format  uint32            `json:"format"`
	Name    string            `json:"name"`
	Created string            `json:"created"`
	Labels  map[string]string `json:"labels"`
}

// MarshalJSON writes empty labels as `{}`, as serde does.
func (m MicrovmVolumeMetadata) MarshalJSON() ([]byte, error) {
	type plain MicrovmVolumeMetadata
	p := plain(m)
	p.Labels = microvmNonNilMap(p.Labels)
	return json.Marshal(p)
}

// UnmarshalJSON requires every member, as serde does.
func (m *MicrovmVolumeMetadata) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "format", "name", "created", "labels"); err != nil {
		return err
	}
	type plain MicrovmVolumeMetadata
	return json.Unmarshal(data, (*plain)(m))
}

// MicrovmVolumeMetadataPath is VolumeMetadata::path.
func MicrovmVolumeMetadataPath(layout MicrovmLayout, name string) string {
	return filepath.Join(layout.Volume(name), "labels.json")
}

// MicrovmVolumeMetadataRead is the metadata of volume name, or nil when
// there is no such volume.
func MicrovmVolumeMetadataRead(layout MicrovmLayout, name string) (*MicrovmVolumeMetadata, error) {
	panic("unported: mj-worker/src/microvm/state.rs VolumeMetadata::read")
}

// Write writes the metadata file.
func (m MicrovmVolumeMetadata) Write(layout MicrovmLayout) error {
	return MicrovmWriteJSONAtomic(MicrovmVolumeMetadataPath(layout, m.Name), m)
}

// MicrovmVolumeMetadataList is every volume with metadata, in name order.
func MicrovmVolumeMetadataList(layout MicrovmLayout) ([]MicrovmVolumeMetadata, error) {
	panic("unported: mj-worker/src/microvm/state.rs VolumeMetadata::list")
}

// MicrovmImageConfig is the OCI image configuration kept next to a
// flattened image.
type MicrovmImageConfig struct {
	Format       uint32            `json:"format"`
	ID           string            `json:"id"`
	Architecture string            `json:"architecture"`
	Env          []string          `json:"env"`
	User         string            `json:"user"`
	WorkingDir   string            `json:"working_dir"`
	Entrypoint   []string          `json:"entrypoint"`
	Cmd          []string          `json:"cmd"`
	Labels       map[string]string `json:"labels"`
	// RootBytes is the size of the flattened root filesystem image in bytes.
	RootBytes uint64 `json:"root_bytes"`
}

// MarshalJSON writes empty lists and maps as `[]` and `{}`, as serde does.
func (c MicrovmImageConfig) MarshalJSON() ([]byte, error) {
	type plain MicrovmImageConfig
	p := plain(c)
	p.Env, p.Entrypoint, p.Cmd = microvmNonNil(p.Env), microvmNonNil(p.Entrypoint), microvmNonNil(p.Cmd)
	p.Labels = microvmNonNilMap(p.Labels)
	return json.Marshal(p)
}

// UnmarshalJSON requires the members serde requires.
func (c *MicrovmImageConfig) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "format", "id", "architecture"); err != nil {
		return err
	}
	type plain MicrovmImageConfig
	return json.Unmarshal(data, (*plain)(c))
}

// MicrovmImageConfigRead is ImageConfig::read.
func MicrovmImageConfigRead(layout MicrovmLayout, id string) (MicrovmImageConfig, error) {
	panic("unported: mj-worker/src/microvm/state.rs ImageConfig::read")
}

// MicrovmImageRefs maps reference names to image ids.
type MicrovmImageRefs struct {
	Format uint32            `json:"format"`
	Refs   map[string]string `json:"refs"`
}

// MarshalJSON writes empty refs as `{}`, as serde does.
func (r MicrovmImageRefs) MarshalJSON() ([]byte, error) {
	type plain MicrovmImageRefs
	p := plain(r)
	p.Refs = microvmNonNilMap(p.Refs)
	return json.Marshal(p)
}

// MicrovmImageRefsRead is ImageRefs::read: an absent file is an empty table
// of the current format.
func MicrovmImageRefsRead(layout MicrovmLayout) (MicrovmImageRefs, error) {
	panic("unported: mj-worker/src/microvm/state.rs ImageRefs::read")
}

// Write writes the reference table.
func (r MicrovmImageRefs) Write(layout MicrovmLayout) error {
	return MicrovmWriteJSONAtomic(layout.ImageRefs(), r)
}
