//go:build linux

// Port of mj-worker/src/microvm/volume.rs.
//
// Named volumes: sparse ext4 disk files that outlive the VMs using them.
//
// A volume is sized to the filesystem that holds it. Firecracker has no
// shared-directory device, so a volume is a block device; being sparse, it
// costs host space only as the guest writes, and the host disk is the real
// limit.

package worker

import (
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"sort"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MicrovmSessionLabel is the label that ties a resource to a Mjolnir
// session.
const MicrovmSessionLabel = "dev.mj.session"

// MicrovmVolumeDiskPath is the disk file of volume name.
func MicrovmVolumeDiskPath(layout MicrovmLayout, name string) string {
	return filepath.Join(layout.Volume(name), "disk.ext4")
}

// microvmValidateLabels checks every label in key order, as Rust walks its
// BTreeMap.
func microvmValidateLabels(labels map[string]string) error {
	keys := make([]string, 0, len(labels))
	for key := range labels {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		if err := MicrovmValidateLabel(key, labels[key]); err != nil {
			return err
		}
	}
	return nil
}

// MicrovmVolumeCreate creates a volume. Fails if the name is taken.
func MicrovmVolumeCreate(layout MicrovmLayout, name string, labels map[string]string) (MicrovmVolumeMetadata, error) {
	if err := MicrovmValidateName("volume", name); err != nil {
		return MicrovmVolumeMetadata{}, err
	}
	if err := microvmValidateLabels(labels); err != nil {
		return MicrovmVolumeMetadata{}, err
	}
	if err := layout.Ensure(); err != nil {
		return MicrovmVolumeMetadata{}, err
	}
	lock, err := layout.LockVolume(name)
	if err != nil {
		return MicrovmVolumeMetadata{}, err
	}
	defer lock.Drop()
	created, err := microvmVolumeCreateLocked(layout, name, labels)
	if err != nil {
		microvmVolumeForgetIfAbsent(layout, name, lock)
	}
	return created, err
}

// microvmVolumeCreateLocked is MicrovmVolumeCreate for a caller that
// already holds the volume's lock.
func microvmVolumeCreateLocked(layout MicrovmLayout, name string, labels map[string]string) (MicrovmVolumeMetadata, error) {
	existing, err := MicrovmVolumeMetadataRead(layout, name)
	if err != nil {
		return MicrovmVolumeMetadata{}, err
	}
	if existing != nil {
		return MicrovmVolumeMetadata{}, fmt.Errorf("volume %s already exists", name)
	}
	scratch := filepath.Join(layout.Volumes(), fmt.Sprintf(".tmp-%s-%s", name, MicrovmScratchSuffix()))
	if err := os.Mkdir(scratch, 0o777); err != nil {
		return MicrovmVolumeMetadata{}, fmt.Errorf("create %s: %w", scratch, microvmIOError(err))
	}
	built := func() error {
		disk := filepath.Join(scratch, "disk.ext4")
		file, err := os.Create(disk)
		if err != nil {
			return microvmIOError(err)
		}
		size, err := MicrovmFilesystemBytes(layout.Volumes())
		if err == nil {
			if truncateErr := file.Truncate(int64(size)); truncateErr != nil {
				err = microvmIOError(truncateErr)
			}
		}
		_ = file.Close()
		if err != nil {
			return err
		}
		if _, err := MicrovmRunTool("mkfs.ext4", []string{
			"-q",
			"-F",
			"-m",
			"0",
			"-E",
			"lazy_itable_init=1,lazy_journal_init=1,nodiscard",
			disk,
		}); err != nil {
			return err
		}
		metadata := MicrovmVolumeMetadata{
			Format:  MicrovmMetadataFormat,
			Name:    name,
			Created: MicrovmNowRFC3339(),
			Labels:  labels,
		}
		return MicrovmWriteJSONAtomic(filepath.Join(scratch, "labels.json"), metadata)
	}()
	if built != nil {
		if cleanup := os.RemoveAll(scratch); cleanup != nil {
			slog.Warn("could not remove volume scratch directory", "cleanup", cleanup, "scratch", scratch)
		}
		return MicrovmVolumeMetadata{}, fmt.Errorf("create volume %s: %w", name, built)
	}
	if err := os.Rename(scratch, layout.Volume(name)); err != nil {
		return MicrovmVolumeMetadata{}, fmt.Errorf("publish volume %s: %w", name, microvmIOError(err))
	}
	return MicrovmVolumeMetadata{
		Format:  MicrovmMetadataFormat,
		Name:    name,
		Created: MicrovmNowRFC3339(),
		Labels:  labels,
	}, nil
}

// MicrovmVolumePrepareForRun is what `run` needs of a volume: it exists,
// created now if it did not, and it does not belong to another session.
// Returns the volume's lock, which the caller holds until the VM state that
// lists the volume is written (a `volume rm` decides under the same lock),
// and whether this call created it.
func MicrovmVolumePrepareForRun(layout MicrovmLayout, name string, labels map[string]string) (*core.FileLock, bool, error) {
	if err := MicrovmValidateName("volume", name); err != nil {
		return nil, false, err
	}
	if err := layout.Ensure(); err != nil {
		return nil, false, err
	}
	lock, err := layout.LockVolume(name)
	if err != nil {
		return nil, false, err
	}
	existing, err := MicrovmVolumeMetadataRead(layout, name)
	if err != nil {
		lock.Drop()
		return nil, false, err
	}
	if existing != nil {
		if err := microvmVolumeCheckOwnership(*existing, labels); err != nil {
			lock.Drop()
			return nil, false, err
		}
		return lock, false, nil
	}
	if err := microvmValidateLabels(labels); err != nil {
		lock.Drop()
		return nil, false, err
	}
	if _, err := microvmVolumeCreateLocked(layout, name, labels); err != nil {
		microvmVolumeForgetIfAbsent(layout, name, lock)
		lock.Drop()
		return nil, false, err
	}
	return lock, true, nil
}

func microvmVolumeCheckOwnership(existing MicrovmVolumeMetadata, wanted map[string]string) error {
	theirs, theirsOK := existing.Labels[MicrovmSessionLabel]
	ours, oursOK := wanted[MicrovmSessionLabel]
	if theirsOK && oursOK && theirs != ours {
		return fmt.Errorf("refusing volume %s: it belongs to session %s, not %s", existing.Name, theirs, ours)
	}
	if theirsOK != oursOK {
		return fmt.Errorf("refusing volume %s: its %s label does not match this container's", existing.Name, MicrovmSessionLabel)
	}
	return nil
}

// MicrovmVolumeRemove removes a volume no VM uses.
func MicrovmVolumeRemove(layout MicrovmLayout, name string, ignoreMissing bool) error {
	if err := MicrovmValidateName("volume", name); err != nil {
		return err
	}
	lock, err := layout.LockVolume(name)
	if err != nil {
		return err
	}
	defer lock.Drop()
	metadata, err := MicrovmVolumeMetadataRead(layout, name)
	if err != nil {
		return err
	}
	if metadata == nil {
		if err := layout.ForgetVolumeLock(name, lock); err != nil {
			return err
		}
		if ignoreMissing {
			return nil
		}
		return fmt.Errorf("no volume with name %s found: no such volume", microvmDebugString(name))
	}
	states, err := MicrovmVmStateList(layout)
	if err != nil {
		return err
	}
	for _, vm := range states {
		for _, volume := range vm.Volumes {
			if volume.Name == name {
				return fmt.Errorf("volume %s is being used by container %s", name, vm.Name)
			}
		}
	}
	// Unpublish first so a crash leaves scratch, not a volume without a disk.
	doomed := filepath.Join(layout.Volumes(), fmt.Sprintf(".removing-%s-%s", name, MicrovmScratchSuffix()))
	if err := os.Rename(layout.Volume(name), doomed); err != nil {
		return fmt.Errorf("unpublish volume %s: %w", name, microvmIOError(err))
	}
	if err := os.RemoveAll(doomed); err != nil {
		return fmt.Errorf("remove %s: %w", doomed, microvmIOError(err))
	}
	return layout.ForgetVolumeLock(name, lock)
}

// microvmVolumeForgetIfAbsent drops the lock file after a failed create
// unless the volume exists. It runs on an error path, so its own failure is
// logged, not returned over the original.
func microvmVolumeForgetIfAbsent(layout MicrovmLayout, name string, lock *core.FileLock) {
	metadata, err := MicrovmVolumeMetadataRead(layout, name)
	if err == nil && metadata == nil {
		err = layout.ForgetVolumeLock(name, lock)
	}
	if err != nil {
		slog.Warn("could not remove a volume lock file", "error", err, "volume", name)
	}
}
