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
	"fmt"
	"os"
	"path/filepath"

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
	return json.Marshal(p)
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
	panic("unported: mj-worker/src/microvm/replace_root.rs read_record")
}

// MicrovmPinnedImages is the image ids that a staged replacement still
// needs. `image prune` calls this under the image's lock, which `stage`
// also holds while it writes the record, so a prune cannot miss a record
// being written.
func MicrovmPinnedImages(layout MicrovmLayout) (map[string]bool, error) {
	panic("unported: mj-worker/src/microvm/replace_root.rs pinned_images")
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
	panic("unported: mj-worker/src/microvm/replace_root.rs stage_checked")
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
	panic("unported: mj-worker/src/microvm/replace_root.rs observed")
}

// MicrovmResumeInterruptedLocked finishes a swap that a crash interrupted.
// The caller holds the VM's lock and calls this before it starts the VM, in
// the same critical section, so no other `apply` can interleave between the
// decision and the boot. Without a swap in progress this does nothing.
func MicrovmResumeInterruptedLocked(layout MicrovmLayout, name string) error {
	panic("unported: mj-worker/src/microvm/replace_root.rs resume_interrupted_locked")
}

// MicrovmReplaceRootApply swaps the root disk of the stopped VM name.
// Returns the VM's new id.
func MicrovmReplaceRootApply(layout MicrovmLayout, name string, operation string) (string, error) {
	panic("unported: mj-worker/src/microvm/replace_root.rs apply")
}

// microvmReplaceRootApplyStopped is apply after the checks that need the
// host. clone copies an image's root disk to a new file; tests give it a
// plain copy.
func microvmReplaceRootApplyStopped(layout MicrovmLayout, name string, operation string, clone func(source string, destination string) error) (string, error) {
	panic("unported: mj-worker/src/microvm/replace_root.rs apply_stopped")
}

// MicrovmReplaceRootRelease ends the operation: it deletes the previous
// root disk if the swap happened, and the record. A container that no
// longer exists has nothing to release.
func MicrovmReplaceRootRelease(layout MicrovmLayout, name string, operation string) error {
	panic("unported: mj-worker/src/microvm/replace_root.rs release")
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
