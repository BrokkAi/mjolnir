// Port of mj-targets/src/targets/resources.rs: Unix measures local storage
// with the storage probe script.

//go:build !windows

package targets

import "github.com/BrokkAi/mjolnir/go/internal/core"

func measureLocalStorage(paths []string, executor core.CommandExecutor) (*string, []core.FilesystemSpace, error) {
	return measureUnixStorage(paths, executor)
}
