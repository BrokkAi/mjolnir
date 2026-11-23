// Port of mj-store/src/database/mounts.rs.

package store

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RememberMountSources atomically applies the controller's MRU policy for
// newly used mount sources.
func RememberMountSources(host string, mounts []core.AdditionalMount) error {
	panic("unported: mj-store/src/database/mounts.rs remember_mount_sources")
}

func ReplaceMountHistory(host string, sources []string) error {
	panic("unported: mj-store/src/database/mounts.rs replace_mount_history")
}

func replaceMountHistoryIn(path, host string, sources []string) error {
	panic("unported: mj-store/src/database/mounts.rs replace_mount_history_in")
}

func writeMountHistory(tx *Transaction, host string, sources []string) error {
	panic("unported: mj-store/src/database/mounts.rs write_mount_history")
}

func writeHostContainerSize(tx *Transaction, host string, size core.HostContainerSize) error {
	panic("unported: mj-store/src/database/mounts.rs write_host_container_size")
}

func RememberProjectDirectory(host string, directory string) error {
	panic("unported: mj-store/src/database/mounts.rs remember_project_directory")
}

func rememberSources(path, host string, newSources []string) error {
	panic("unported: mj-store/src/database/mounts.rs remember_sources")
}
