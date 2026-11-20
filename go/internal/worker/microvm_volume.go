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
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MicrovmSessionLabel is the label that ties a resource to a Mjolnir
// session.
const MicrovmSessionLabel = "dev.mj.session"

// MicrovmVolumeDiskPath is the disk file of volume name.
func MicrovmVolumeDiskPath(layout MicrovmLayout, name string) string {
	return filepath.Join(layout.Volume(name), "disk.ext4")
}

// MicrovmVolumeCreate creates a volume. Fails if the name is taken.
func MicrovmVolumeCreate(layout MicrovmLayout, name string, labels map[string]string) (MicrovmVolumeMetadata, error) {
	panic("unported: mj-worker/src/microvm/volume.rs create")
}

// microvmVolumeCreateLocked is MicrovmVolumeCreate for a caller that
// already holds the volume's lock.
func microvmVolumeCreateLocked(layout MicrovmLayout, name string, labels map[string]string) (MicrovmVolumeMetadata, error) {
	panic("unported: mj-worker/src/microvm/volume.rs create_locked")
}

// MicrovmVolumePrepareForRun is what `run` needs of a volume: it exists,
// created now if it did not, and it does not belong to another session.
// Returns the volume's lock, which the caller holds until the VM state that
// lists the volume is written (a `volume rm` decides under the same lock),
// and whether this call created it.
func MicrovmVolumePrepareForRun(layout MicrovmLayout, name string, labels map[string]string) (*core.FileLock, bool, error) {
	panic("unported: mj-worker/src/microvm/volume.rs prepare_for_run")
}

func microvmVolumeCheckOwnership(existing MicrovmVolumeMetadata, wanted map[string]string) error {
	panic("unported: mj-worker/src/microvm/volume.rs check_ownership")
}

// MicrovmVolumeRemove removes a volume no VM uses.
func MicrovmVolumeRemove(layout MicrovmLayout, name string, ignoreMissing bool) error {
	panic("unported: mj-worker/src/microvm/volume.rs remove")
}

// microvmVolumeForgetIfAbsent drops the lock file after a failed create
// unless the volume exists. It runs on an error path, so its own failure is
// logged, not returned over the original.
func microvmVolumeForgetIfAbsent(layout MicrovmLayout, name string, lock *core.FileLock) {
	panic("unported: mj-worker/src/microvm/volume.rs forget_if_absent")
}
