//go:build linux

// Port of mj-worker/src/microvm/replace_root.rs.
//
// Replace a stopped VM's root disk with a clone of another image, keeping
// its volumes: the engine half of the controller's retained-storage image
// upgrade.
//
// Podman's upgrade creates the replacement container while the old one still
// exists, because two containers may mount one volume. Two kernels writing one
// ext4 volume would corrupt it, so the engine never lets two VMs list a
// volume; the replacement is the same VM with a different root disk. The old
// root disk stays beside it as `root.previous` until the controller says the
// new worker is ready, which is the role of Podman's renamed source container.
//
// Three verbs, each safe to repeat, each under the VM's lock:
//
//   - `stage` records the target image in `replace-root.json` in the VM's
//     directory. The record is the pin: `image prune` keeps the image while a
//     record names it, and removing the VM removes the record.
//   - `apply` swaps the root disk of a stopped VM and gives the VM a new id (a
//     container created from another image has another id, and the controller
//     tells source from replacement by it). Every step is ordered so that a
//     crash at any point leaves a state a second `apply` finishes.
//   - `release` ends the operation: it deletes the previous root disk if the
//     swap happened, and the record either way.
//
// The record carries the id the VM will have after the swap, chosen when the
// record is written. "Applied" is `state.id == record.new_id`, a fact in the
// one file that `apply` writes last, so no pair of files has to agree.

package worker

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// microvmReplaceRootRecordFormat is the format of `replace-root.json` this
// build writes.
const microvmReplaceRootRecordFormat uint32 = 1

const microvmReplaceRootRecordFile = "replace-root.json"
const microvmReplaceRootRoot = "root.ext4"

// microvmReplaceRootNext is the clone of the target image, until it takes
// the place of the root disk.
const microvmReplaceRootNext = "root.next"

// microvmReplaceRootPrevious is the root disk the VM had before the swap.
const microvmReplaceRootPrevious = "root.previous"

// microvmReplaceRootRecord is what `stage` records.
type microvmReplaceRootRecord struct {
	Format uint32 `json:"format"`
	// Operation is the controller's operation id: every verb names it.
	Operation string `json:"operation"`
	ImageID   string `json:"image_id"`
	// ImageReference is the reference the controller asked for, shown by
	// `inspect` afterwards.
	ImageReference string `json:"image_reference"`
	// Labels are merged into the VM's labels by the swap.
	Labels map[string]string `json:"labels"`
	// NewID is the VM's id once the swap has happened.
	NewID string `json:"new_id"`
}

// MarshalJSON writes empty labels as `{}`, as serde does.
func (r microvmReplaceRootRecord) MarshalJSON() ([]byte, error) {
	type plain microvmReplaceRootRecord
	p := plain(r)
	p.Labels = microvmNonNilMap(p.Labels)
	return microvmMarshalJSON(p)
}

// UnmarshalJSON requires every member, as serde does.
func (r *microvmReplaceRootRecord) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "format", "operation", "image_id", "image_reference", "labels", "new_id"); err != nil {
		return err
	}
	type plain microvmReplaceRootRecord
	return json.Unmarshal(data, (*plain)(r))
}

func microvmReplaceRootRecordPath(layout MicrovmLayout, name string) string {
	return filepath.Join(layout.VM(name), microvmReplaceRootRecordFile)
}

// microvmReadReplaceRootRecord is the record of name, or nil when there is
// none.
func microvmReadReplaceRootRecord(layout MicrovmLayout, name string) (*microvmReplaceRootRecord, error) {
	path := microvmReplaceRootRecordPath(layout, name)
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, microvmIOError(err))
	}
	var record microvmReplaceRootRecord
	if err := microvmDecodeVersioned(data, path, "root replacement record", microvmReplaceRootRecordFormat, &record); err != nil {
		return nil, err
	}
	return &record, nil
}

// MicrovmPinnedImages is the image ids that a staged replacement still
// needs. `image prune` calls this under the image's lock, which `stage`
// also holds while it writes the record, so a prune cannot miss a record
// being written.
func MicrovmPinnedImages(layout MicrovmLayout) (map[string]bool, error) {
	pinned := map[string]bool{}
	states, err := MicrovmVmStateList(layout)
	if err != nil {
		return nil, err
	}
	for _, state := range states {
		record, err := microvmReadReplaceRootRecord(layout, state.Name)
		if err != nil {
			return nil, err
		}
		if record != nil {
			pinned[record.ImageID] = true
		}
	}
	return pinned, nil
}

func microvmNoSuchContainer(name string) error {
	return fmt.Errorf("no container with name or ID %s found: no such container", microvmDebugString(name))
}

func microvmReplaceRootReadState(layout MicrovmLayout, name string) (MicrovmVmState, error) {
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return MicrovmVmState{}, err
	}
	if state == nil {
		return MicrovmVmState{}, microvmNoSuchContainer(name)
	}
	return *state, nil
}

// microvmRefuseAbsent refuses an absent name under its lock, deleting the
// lock file the lock just created, so asking about a missing VM leaves
// nothing behind.
func microvmRefuseAbsent(layout MicrovmLayout, name string, held *core.FileLock) error {
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil {
		return err
	}
	if state == nil {
		if err := layout.ForgetVMLock(name, held); err != nil {
			return err
		}
		return microvmNoSuchContainer(name)
	}
	return nil
}

// MicrovmReplaceRootStage records the replacement of name's root disk by
// the image reference names, and pins that image. Returns the image id.
func MicrovmReplaceRootStage(layout MicrovmLayout, name string, operation string, reference string, labels map[string]string) (string, error) {
	return microvmReplaceRootStageChecked(layout, name, operation, reference, labels, MicrovmCheckImage)
}

// microvmReplaceRootStageChecked is stage with the image check given;
// tests, which have no mountable disk, give it a no-op.
func microvmReplaceRootStageChecked(layout MicrovmLayout, name string, operation string, reference string, labels map[string]string, check func(layout MicrovmLayout, imageID string, user string, path string) error) (string, error) {
	if err := MicrovmValidateName("container", name); err != nil {
		return "", err
	}
	if err := microvmValidateOperation(operation); err != nil {
		return "", err
	}
	if err := microvmValidateLabels(labels); err != nil {
		return "", err
	}
	vmLock, err := layout.LockVM(name)
	if err != nil {
		return "", err
	}
	defer vmLock.Drop()
	if err := microvmRefuseAbsent(layout, name, vmLock); err != nil {
		return "", err
	}
	state, err := microvmReplaceRootReadState(layout, name)
	if err != nil {
		return "", err
	}
	id, ok, err := MicrovmImageLookup(layout, reference)
	if err != nil {
		return "", err
	}
	if !ok {
		return "", fmt.Errorf("%s: image not known", reference)
	}
	// Held while the record is written: a prune decides under this lock.
	imageLock, err := layout.LockImage(id)
	if err != nil {
		return "", err
	}
	defer imageLock.Drop()
	if !microvmIsFile(filepath.Join(layout.Image(id), "config.json")) {
		return "", fmt.Errorf("%s: image not known", reference)
	}
	// Every stage, first or repeated, checks the image before anything is
	// stopped or swapped: the swap cannot be undone by the controller, and a
	// worker state that cannot be restored would strand the session.
	path := microvmDefaultPath
	for _, entry := range state.Env {
		if value, ok := strings.CutPrefix(entry, "PATH="); ok {
			path = value
			break
		}
	}
	if err := check(layout, id, state.User, path); err != nil {
		return "", fmt.Errorf("%s cannot replace the root disk of container %s: %w", reference, name, err)
	}
	existing, err := microvmReadReplaceRootRecord(layout, name)
	if err != nil {
		return "", err
	}
	if existing != nil {
		if existing.Operation != operation {
			return "", fmt.Errorf("container %s already has a root replacement in progress (operation %s); release it first", name, existing.Operation)
		}
		if existing.ImageID != id {
			return "", fmt.Errorf("operation %s staged another image for container %s", operation, name)
		}
		return id, nil
	}
	newID, err := microvmRandomID()
	if err != nil {
		return "", err
	}
	record := microvmReplaceRootRecord{
		Format:         microvmReplaceRootRecordFormat,
		Operation:      operation,
		ImageID:        id,
		ImageReference: reference,
		Labels:         labels,
		NewID:          newID,
	}
	if err := MicrovmWriteJSONAtomic(microvmReplaceRootRecordPath(layout, name), record); err != nil {
		return "", err
	}
	return id, nil
}

// microvmReplaceRootTakeOver gives state the identity the record names: the
// replacement's id, image and labels.
func microvmReplaceRootTakeOver(state MicrovmVmState, record microvmReplaceRootRecord) MicrovmVmState {
	state.ID = record.NewID
	state.ImageID = record.ImageID
	state.ImageReference = record.ImageReference
	labels := make(map[string]string, len(state.Labels)+len(record.Labels))
	for key, value := range state.Labels {
		labels[key] = value
	}
	for key, value := range record.Labels {
		labels[key] = value
	}
	state.Labels = labels
	return state
}

func microvmValidateOperation(operation string) error {
	valid := operation != "" && len(operation) <= 128
	for i := 0; valid && i < len(operation); i++ {
		b := operation[i]
		valid = b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '_' || b == '.' || b == '-'
	}
	if !valid {
		return fmt.Errorf("invalid operation id %s", microvmDebugString(operation))
	}
	return nil
}

// MicrovmReplaceRootObserved is the VM as an observer must see it: once
// the old root disk has been set aside, the swap has started and the VM is
// the replacement, whether or not the crash that interrupted it came before
// `state.json` was written. The controller tells source from replacement by
// this id; showing the source there would send recovery to capture control
// state from a half-swapped root.
func MicrovmReplaceRootObserved(layout MicrovmLayout, name string, state MicrovmVmState) (MicrovmVmState, error) {
	record, err := microvmReadReplaceRootRecord(layout, name)
	if err != nil {
		return MicrovmVmState{}, err
	}
	if record != nil && state.ID != record.NewID && microvmExists(filepath.Join(layout.VM(name), microvmReplaceRootPrevious)) {
		state = microvmReplaceRootTakeOver(state, *record)
	}
	return state, nil
}

// MicrovmResumeInterruptedLocked finishes a swap that a crash interrupted.
// The caller holds the VM's lock and calls this before it starts the VM, in
// the same critical section, so no other `apply` can interleave between the
// decision and the boot. Without a swap in progress this does nothing.
func MicrovmResumeInterruptedLocked(layout MicrovmLayout, name string) error {
	state, err := MicrovmVmStateRead(layout, name)
	if err != nil || state == nil {
		return err
	}
	record, err := microvmReadReplaceRootRecord(layout, name)
	if err != nil || record == nil {
		return err
	}
	if state.ID != record.NewID && microvmExists(filepath.Join(layout.VM(name), microvmReplaceRootPrevious)) {
		activity, err := MicrovmVMActivity(name)
		if err != nil {
			return err
		}
		if activity != MicrovmActivityStopped {
			return fmt.Errorf("container %s is running with an interrupted root replacement", name)
		}
		if _, err := microvmReplaceRootApplyStopped(layout, name, record.Operation, microvmReflink); err != nil {
			return err
		}
	}
	return nil
}

// MicrovmReplaceRootApply swaps the root disk of the stopped VM name.
// Returns the VM's new id.
func MicrovmReplaceRootApply(layout MicrovmLayout, name string, operation string) (string, error) {
	if err := MicrovmValidateName("container", name); err != nil {
		return "", err
	}
	if err := microvmValidateOperation(operation); err != nil {
		return "", err
	}
	if !MicrovmIsRoot() {
		return "", errors.New("the microvm engine must run as root (use sudo -n)")
	}
	vmLock, err := layout.LockVM(name)
	if err != nil {
		return "", err
	}
	defer vmLock.Drop()
	if err := microvmRefuseAbsent(layout, name, vmLock); err != nil {
		return "", err
	}
	activity, err := MicrovmVMActivity(name)
	if err != nil {
		return "", err
	}
	if activity != MicrovmActivityStopped {
		return "", fmt.Errorf("container %s is running; stop it before replacing its root disk", name)
	}
	return microvmReplaceRootApplyStopped(layout, name, operation, microvmReflink)
}

// microvmReplaceRootApplyStopped is apply after the checks that need the
// host. clone copies an image's root disk to a new file; tests give it a
// plain copy.
func microvmReplaceRootApplyStopped(layout MicrovmLayout, name string, operation string, clone func(source string, destination string) error) (string, error) {
	state, err := microvmReplaceRootReadState(layout, name)
	if err != nil {
		return "", err
	}
	record, err := microvmReadReplaceRootRecord(layout, name)
	if err != nil {
		return "", err
	}
	if record == nil {
		return "", fmt.Errorf("container %s has no staged root replacement", name)
	}
	if record.Operation != operation {
		return "", fmt.Errorf("container %s stages operation %s, not %s", name, record.Operation, operation)
	}
	if state.ID == record.NewID {
		return state.ID, nil
	}
	directory := layout.VM(name)
	root := filepath.Join(directory, microvmReplaceRootRoot)
	next := filepath.Join(directory, microvmReplaceRootNext)
	previous := filepath.Join(directory, microvmReplaceRootPrevious)
	if !microvmExists(previous) {
		// The swap has not started. A half-written clone from an interrupted
		// attempt is scratch.
		if err := microvmRemoveIfPresent(next); err != nil {
			return "", err
		}
		cloned := func() error {
			imageLock, err := layout.LockImage(record.ImageID)
			if err != nil {
				return err
			}
			defer imageLock.Drop()
			if !microvmIsFile(filepath.Join(layout.Image(record.ImageID), "config.json")) {
				return fmt.Errorf("the staged image %s is gone; the staging record should have pinned it", record.ImageID)
			}
			return clone(MicrovmRootImage(layout, record.ImageID), next)
		}()
		if cloned != nil {
			return "", cloned
		}
		if err := microvmSyncFile(next); err != nil {
			return "", err
		}
		// The clone's directory entry must be durable before the old root
		// moves: a recovery that finds `previous` skips the clone.
		if err := microvmSyncDirectory(directory); err != nil {
			return "", err
		}
		if err := os.Rename(root, previous); err != nil {
			return "", fmt.Errorf("set aside %s: %w", root, microvmIOError(err))
		}
		// Durable before the replacement is installed: a recovery that finds
		// no `previous` takes the installed clone for the original root.
		if err := microvmSyncDirectory(directory); err != nil {
			return "", err
		}
	}
	// From here the old root disk is `previous` and `next` is complete; a
	// second `apply` continues exactly here.
	if microvmExists(next) {
		if err := os.Rename(next, root); err != nil {
			return "", fmt.Errorf("install %s: %w", root, microvmIOError(err))
		}
	}
	if !microvmIsFile(root) {
		return "", fmt.Errorf("container %s has no root disk after the swap; %s holds the old one", name, previous)
	}
	if err := microvmSyncDirectory(directory); err != nil {
		return "", err
	}
	state = microvmReplaceRootTakeOver(state, *record)
	if err := state.Write(layout); err != nil {
		return "", err
	}
	return state.ID, nil
}

// MicrovmReplaceRootRelease ends the operation: it deletes the previous
// root disk if the swap happened, and the record. A container that no
// longer exists has nothing to release.
func MicrovmReplaceRootRelease(layout MicrovmLayout, name string, operation string) error {
	if err := MicrovmValidateName("container", name); err != nil {
		return err
	}
	if err := microvmValidateOperation(operation); err != nil {
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
	if state == nil {
		return layout.ForgetVMLock(name, vmLock)
	}
	record, err := microvmReadReplaceRootRecord(layout, name)
	if err != nil || record == nil {
		return err
	}
	if record.Operation != operation {
		return fmt.Errorf("container %s stages operation %s, not %s", name, record.Operation, operation)
	}
	directory := layout.VM(name)
	previous := filepath.Join(directory, microvmReplaceRootPrevious)
	if state.ID == record.NewID {
		if err := microvmRemoveIfPresent(previous); err != nil {
			return err
		}
	} else {
		// Never swapped, or interrupted mid-swap; only the former may be
		// released, because the old root disk is the only one in the latter.
		if microvmExists(previous) {
			return fmt.Errorf("the root replacement of container %s was interrupted; apply it again", name)
		}
		if err := microvmRemoveIfPresent(filepath.Join(directory, microvmReplaceRootNext)); err != nil {
			return err
		}
	}
	if err := microvmRemoveIfPresent(microvmReplaceRootRecordPath(layout, name)); err != nil {
		return err
	}
	return microvmSyncDirectory(directory)
}

func microvmRemoveIfPresent(path string) error {
	if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("remove %s: %w", path, microvmIOError(err))
	}
	return nil
}

func microvmSyncFile(path string) error {
	file, err := os.Open(path)
	if err == nil {
		err = file.Sync()
		if closeErr := file.Close(); err == nil {
			err = closeErr
		}
	}
	if err != nil {
		return fmt.Errorf("sync %s: %w", path, microvmIOError(err))
	}
	return nil
}

func microvmSyncDirectory(path string) error {
	return microvmSyncFile(path)
}
