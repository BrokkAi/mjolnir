// Port of mj-core/src/login_environment.rs (platforms without the Windows
// environment block).

//go:build !windows

package core

import "errors"

// discoverWindowsAccount exists only on Windows; Discover never calls this
// one.
func discoverWindowsAccount() (LoginEnvironmentEnvironment, error) {
	return nil, errors.New("target login environment requires a Unix account")
}
