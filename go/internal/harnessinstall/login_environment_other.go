// Port of mj-harness-install/src/login_environment.rs (platforms without the Windows
// environment block).

//go:build !windows

package harnessinstall

import (
	"errors"
)

// discoverWindowsAccount exists only on Windows; Discover never calls this
// one.
func discoverWindowsAccount() (LoginEnvironmentEnvironment, error) {
	return nil, errors.New("target login environment requires a Unix account")
}
