//go:build linux

// Port of mj-microvm/src/state.rs.
//
// The engine's on-disk state: layout, formats, names, and locks.
//
// VMs and their guest agents outlive engine upgrades, so every file here is
// versioned and a newer engine reads every older format. An older engine
// that meets a newer format refuses to touch it and says why.

package microvm

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"golang.org/x/sys/unix"

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

func (l MicrovmLayout) MbxCache() string { return filepath.Join(l.root, "mbx-cache") }

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
	for _, directory := range []string{l.root, l.Images(), l.VMs(), l.Volumes(), l.Boot(), l.Locks()} {
		if err := os.MkdirAll(directory, 0o777); err != nil {
			return fmt.Errorf("create %s: %w", directory, microvmIOError(err))
		}
	}
	// Flattened images hold setuid binaries and guest data: nobody but root
	// may traverse the state tree, whatever the umask was.
	if err := os.Chmod(l.root, 0o700); err != nil {
		return fmt.Errorf("restrict %s: %w", l.root, microvmIOError(err))
	}
	return nil
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
	valid := len(id) == 64
	for i := 0; valid && i < len(id); i++ {
		valid = microvmIsASCIIHexDigit(id[i])
	}
	if !valid {
		return nil, fmt.Errorf("invalid image id %s", microvmDebugString(id))
	}
	return l.lockFile(fmt.Sprintf("image-%s.lock", id))
}

func microvmIsASCIIHexDigit(b byte) bool {
	return b >= '0' && b <= '9' || b >= 'a' && b <= 'f' || b >= 'A' && b <= 'F'
}

// microvmFlock is flock(2) that retries an interrupted wait.
func microvmFlock(file *os.File, how int) error {
	for {
		err := unix.Flock(int(file.Fd()), how)
		if err != unix.EINTR {
			return err
		}
	}
}

// LockBootShared reserves a boot-assets directory: held (shared) from
// before a launch names the assets until Firecracker has opened them, so a
// prune by another engine build cannot remove them in between.
func (l MicrovmLayout) LockBootShared(tag string) (*core.FileLock, error) {
	file, err := l.openLockFile(fmt.Sprintf("boot-%s.lock", tag))
	if err != nil {
		return nil, err
	}
	if err := microvmFlock(file, unix.LOCK_SH); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("lock boot assets %s: %w", tag, microvmIOError(err))
	}
	return core.NewFileLock(file), nil
}

// TryLockBootExclusive takes the exclusive lock on boot assets, or returns
// nil while a launch holds them.
func (l MicrovmLayout) TryLockBootExclusive(tag string) (*core.FileLock, error) {
	file, err := l.openLockFile(fmt.Sprintf("boot-%s.lock", tag))
	if err != nil {
		return nil, err
	}
	switch err := microvmFlock(file, unix.LOCK_EX|unix.LOCK_NB); {
	case err == nil:
		return core.NewFileLock(file), nil
	case errors.Is(err, unix.EWOULDBLOCK):
		_ = file.Close()
		return nil, nil
	default:
		_ = file.Close()
		return nil, fmt.Errorf("lock boot assets %s: %w", tag, microvmIOError(err))
	}
}

func (l MicrovmLayout) openLockFile(fileName string) (*os.File, error) {
	if err := os.MkdirAll(l.Locks(), 0o777); err != nil {
		return nil, fmt.Errorf("create %s: %w", l.Locks(), microvmIOError(err))
	}
	path := filepath.Join(l.Locks(), fileName)
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY, 0o666)
	if err != nil {
		return nil, fmt.Errorf("open lock %s: %w", path, microvmIOError(err))
	}
	return file, nil
}

// lockFile locks the named file. A resource's lock file is deleted with the
// resource (see the Forget* methods), by a holder of the lock; a locker
// that was waiting then holds a lock on an unlinked file, which excludes
// nobody. So after acquiring, check that the file held is still the one at
// the path, and start over if not. Each retry follows a delete by another
// process, so the loop cannot spin.
func (l MicrovmLayout) lockFile(fileName string) (*core.FileLock, error) {
	path := filepath.Join(l.Locks(), fileName)
	for {
		file, err := l.openLockFile(fileName)
		if err != nil {
			return nil, err
		}
		if err := microvmFlock(file, unix.LOCK_EX); err != nil {
			_ = file.Close()
			return nil, fmt.Errorf("lock %s: %w", path, microvmIOError(err))
		}
		linked, err := microvmLockFileIsLinked(file, path)
		if err != nil {
			_ = file.Close()
			return nil, err
		}
		if linked {
			return core.NewFileLock(file), nil
		}
		// Closing the descriptor releases its lock on the unlinked file.
		_ = file.Close()
	}
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
	held, err := file.Stat()
	if err != nil {
		return false, fmt.Errorf("stat lock %s: %w", path, microvmIOError(err))
	}
	linked, err := os.Stat(path)
	if os.IsNotExist(err) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("stat lock %s: %w", path, microvmIOError(err))
	}
	heldStat, ok1 := held.Sys().(*syscall.Stat_t)
	linkedStat, ok2 := linked.Sys().(*syscall.Stat_t)
	if !ok1 || !ok2 {
		return false, fmt.Errorf("stat lock %s: no device and inode", path)
	}
	return linkedStat.Dev == heldStat.Dev && linkedStat.Ino == heldStat.Ino, nil
}

func microvmIsASCIIAlphanumeric(b byte) bool {
	return b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9'
}

// MicrovmValidateName checks a container or volume name. Names reach file
// paths, unit names and kernel command lines, so they are restricted to a
// safe alphabet.
func MicrovmValidateName(kind, name string) error {
	valid := name != "" && len(name) <= 63 && microvmIsASCIIAlphanumeric(name[0])
	for i := 0; valid && i < len(name); i++ {
		b := name[i]
		valid = microvmIsASCIIAlphanumeric(b) || b == '_' || b == '.' || b == '-'
	}
	if !valid {
		return fmt.Errorf("invalid %s name %s: use 1 to 63 letters, digits, '_', '.' or '-', starting with a letter or digit", kind, microvmDebugString(name))
	}
	return nil
}

// MicrovmValidateLabel checks a label. Labels are metadata a person or the
// controller attaches; keys are restricted the way container engines
// restrict them.
func MicrovmValidateLabel(key, value string) error {
	valid := key != "" && len(key) <= 128
	for i := 0; valid && i < len(key); i++ {
		b := key[i]
		valid = microvmIsASCIIAlphanumeric(b) || b == '_' || b == '.' || b == '-' || b == '/'
	}
	if !valid {
		return fmt.Errorf("invalid label key %s", microvmDebugString(key))
	}
	if strings.ContainsAny(value, "\n\x00") {
		return fmt.Errorf("label %s has an invalid value", key)
	}
	return nil
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

// MicrovmMbxRemote is the S3 remote a VM's read-only mbx cache proxy reads.
type MicrovmMbxRemote struct {
	Bucket string  `json:"bucket"`
	Prefix string  `json:"prefix"`
	Region *string `json:"region"`
	Budget *uint64 `json:"budget,omitempty"`
	// Namespaces is non-empty. The first namespace is the guest client's
	// default.
	Namespaces []string `json:"namespaces"`
}

// MarshalJSON writes empty namespaces as `[]`, as serde does.
func (r MicrovmMbxRemote) MarshalJSON() ([]byte, error) {
	type plain MicrovmMbxRemote
	p := plain(r)
	p.Namespaces = microvmNonNil(p.Namespaces)
	return microvmMarshalJSON(p)
}

// UnmarshalJSON requires the members serde requires.
func (r *MicrovmMbxRemote) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "bucket", "prefix", "namespaces"); err != nil {
		return err
	}
	type plain MicrovmMbxRemote
	return json.Unmarshal(data, (*plain)(r))
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
	MbxRemote  *MicrovmMbxRemote `json:"mbx_remote,omitempty"`
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
	return microvmMarshalJSON(p)
}

// UnmarshalJSON requires every member but `mbx_remote` and `jailed`, as
// serde does.
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
	path := MicrovmVmStatePath(layout, name)
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, microvmIOError(err))
	}
	state, err := MicrovmVmStateParse(data)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	return &state, nil
}

// MicrovmVmStateParse decodes a `state.json`, refusing a format newer than
// this build knows.
func MicrovmVmStateParse(data []byte) (MicrovmVmState, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return MicrovmVmState{}, fmt.Errorf("decode state.json: %w", err)
	}
	if err := MicrovmRefuseNewerFormat("VM state", value, MicrovmStateFormat); err != nil {
		return MicrovmVmState{}, err
	}
	var state MicrovmVmState
	if err := json.Unmarshal(data, &state); err != nil {
		return MicrovmVmState{}, fmt.Errorf("decode state.json: %w", err)
	}
	return state, nil
}

// microvmListNames is the names in directory that do not start with a dot
// (dot entries are removal and creation scratch space), sorted; none when
// the directory does not exist.
func microvmListNames(directory string) ([]string, error) {
	entries, err := os.ReadDir(directory)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("list %s: %w", directory, microvmIOError(err))
	}
	var names []string
	for _, entry := range entries {
		if name := entry.Name(); !strings.HasPrefix(name, ".") {
			names = append(names, name)
		}
	}
	sort.Strings(names)
	return names, nil
}

// MicrovmVmStateList is every VM with a state file, in name order.
func MicrovmVmStateList(layout MicrovmLayout) ([]MicrovmVmState, error) {
	names, err := microvmListNames(layout.VMs())
	if err != nil {
		return nil, err
	}
	var states []MicrovmVmState
	for _, name := range names {
		state, err := MicrovmVmStateRead(layout, name)
		if err != nil {
			return nil, err
		}
		if state != nil {
			states = append(states, *state)
		}
	}
	return states, nil
}

// MicrovmRefuseNewerFormat fails with a clear message when a file was
// written by a newer engine. value is the file decoded as JSON.
func MicrovmRefuseNewerFormat(what string, value any, supported uint32) error {
	var format uint64
	found := false
	if object, ok := value.(jsonvalue.Object); ok {
		switch number := microvmObjectGet(object, "format").(type) {
		case int64:
			format, found = uint64(number), number >= 0
		case uint64:
			format, found = number, true
		}
	}
	if !found {
		return errors.New("the file has no format number")
	}
	if format > uint64(supported) {
		return fmt.Errorf("%s format %d was written by a newer mj-worker than this one (it understands up to format %d); install the newer mj-worker on this host", what, format, supported)
	}
	return nil
}

// microvmObjectGet is serde_json's Value::get on an object: the member, or
// nil when it is absent.
func microvmObjectGet(object jsonvalue.Object, key string) any {
	value, _ := object.Get(key)
	return value
}

// MicrovmWriteJSONAtomic writes value to path through a temporary file in
// the same directory, as serde_json's pretty writer does, with a newline.
func MicrovmWriteJSONAtomic(path string, value any) error {
	if path == "" || path == "/" {
		return fmt.Errorf("%s has no parent directory", path)
	}
	directory := filepath.Dir(path)
	temporary := filepath.Join(directory, "."+filepath.Base(path)+".tmp")
	file, err := os.Create(temporary)
	if err != nil {
		return fmt.Errorf("create %s: %w", temporary, microvmIOError(err))
	}
	defer file.Close()
	encoded, err := microvmPrettyJSON(value)
	if err != nil {
		return fmt.Errorf("write %s: %w", temporary, err)
	}
	if _, err := file.WriteString(encoded); err != nil {
		return fmt.Errorf("write %s: %w", temporary, microvmIOError(err))
	}
	if _, err := file.WriteString("\n"); err != nil {
		return microvmIOError(err)
	}
	if err := file.Sync(); err != nil {
		return fmt.Errorf("sync %s: %w", temporary, microvmIOError(err))
	}
	if err := os.Rename(temporary, path); err != nil {
		return fmt.Errorf("publish %s: %w", path, microvmIOError(err))
	}
	// The rename is only durable once the directory is: a caller may delete
	// what this file replaces, or answer a controller that acts on it.
	return microvmSyncDirectory(directory)
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
	return microvmMarshalJSON(p)
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

// microvmDecodeVersioned decodes a metadata file read from path the way
// every reader of one does: decode, refuse a newer format, decode into
// target.
func microvmDecodeVersioned(data []byte, path string, what string, supported uint32, target any) error {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return fmt.Errorf("decode %s: %w", path, err)
	}
	if err := MicrovmRefuseNewerFormat(what, value, supported); err != nil {
		return err
	}
	if err := json.Unmarshal(data, target); err != nil {
		return fmt.Errorf("decode %s: %w", path, err)
	}
	return nil
}

// MicrovmVolumeMetadataRead is the metadata of volume name, or nil when
// there is no such volume.
func MicrovmVolumeMetadataRead(layout MicrovmLayout, name string) (*MicrovmVolumeMetadata, error) {
	path := MicrovmVolumeMetadataPath(layout, name)
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, microvmIOError(err))
	}
	var metadata MicrovmVolumeMetadata
	if err := microvmDecodeVersioned(data, path, "volume metadata", MicrovmMetadataFormat, &metadata); err != nil {
		return nil, err
	}
	return &metadata, nil
}

// Write writes the metadata file.
func (m MicrovmVolumeMetadata) Write(layout MicrovmLayout) error {
	return MicrovmWriteJSONAtomic(MicrovmVolumeMetadataPath(layout, m.Name), m)
}

// MicrovmVolumeMetadataList is every volume with metadata, in name order.
func MicrovmVolumeMetadataList(layout MicrovmLayout) ([]MicrovmVolumeMetadata, error) {
	names, err := microvmListNames(layout.Volumes())
	if err != nil {
		return nil, err
	}
	var volumes []MicrovmVolumeMetadata
	for _, name := range names {
		metadata, err := MicrovmVolumeMetadataRead(layout, name)
		if err != nil {
			return nil, err
		}
		if metadata != nil {
			volumes = append(volumes, *metadata)
		}
	}
	return volumes, nil
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
	return microvmMarshalJSON(p)
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
	path := filepath.Join(layout.Image(id), "config.json")
	data, err := os.ReadFile(path)
	if err != nil {
		return MicrovmImageConfig{}, fmt.Errorf("read %s: %w", path, microvmIOError(err))
	}
	var config MicrovmImageConfig
	if err := microvmDecodeVersioned(data, path, "image metadata", MicrovmMetadataFormat, &config); err != nil {
		return MicrovmImageConfig{}, err
	}
	return config, nil
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
	return microvmMarshalJSON(p)
}

// MicrovmImageRefsRead is ImageRefs::read: an absent file is an empty table
// of the current format.
func MicrovmImageRefsRead(layout MicrovmLayout) (MicrovmImageRefs, error) {
	path := layout.ImageRefs()
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return MicrovmImageRefs{Format: MicrovmMetadataFormat, Refs: map[string]string{}}, nil
	}
	if err != nil {
		return MicrovmImageRefs{}, fmt.Errorf("read %s: %w", path, microvmIOError(err))
	}
	var refs MicrovmImageRefs
	if err := microvmDecodeVersioned(data, path, "image reference", MicrovmMetadataFormat, &refs); err != nil {
		return MicrovmImageRefs{}, err
	}
	if refs.Refs == nil {
		refs.Refs = map[string]string{}
	}
	return refs, nil
}

// Write writes the reference table.
func (r MicrovmImageRefs) Write(layout MicrovmLayout) error {
	return MicrovmWriteJSONAtomic(layout.ImageRefs(), r)
}
