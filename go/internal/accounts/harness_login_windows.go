// Port of the Windows-only parts of mj-accounts/src/harness_login.rs.

package accounts

import (
	"maps"
	"os"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// resolveHarnessProgram resolves a configured harness command as the child
// would on Windows.
//
// npm installs command shims with `PATHEXT` suffixes such as `.cmd`. Resolve
// those explicitly so the same configured program works for `mj login` and
// `claude setup-token`.
func resolveHarnessProgram(program string, environment map[string]string) string {
	var path string
	pathOK := false
	for _, key := range slices.Sorted(maps.Keys(environment)) {
		if strings.EqualFold(key, "PATH") {
			path, pathOK = environment[key], true
			break
		}
	}
	if !pathOK {
		path, pathOK = os.LookupEnv("PATH")
	}
	if found, ok := harnessinstall.FindProgramOnPath(program, path, pathOK); ok {
		return found
	}
	return program
}
