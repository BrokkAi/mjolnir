//go:build windows

// Port of mj-core/src/targets/ssh.rs: Windows has no SSH connection sharing,
// so nothing takes a master lock there.

package core

import (
	"errors"
	"os"
)

func sshTryLockFile(*os.File) (bool, error) {
	return false, errors.New("SSH connection sharing is not available on Windows")
}
