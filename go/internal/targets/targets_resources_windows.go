// Port of mj-targets/src/targets/resources.rs: Windows has no POSIX shell,
// so local storage is measured through its volume APIs.

//go:build windows

package targets

import "github.com/BrokkAi/mjolnir/go/internal/core"

func measureLocalStorage(paths []string, _ core.CommandExecutor) (*string, []core.FilesystemSpace, error) {
	return nil, core.MeasureWindowsFilesystems(paths), nil
}
