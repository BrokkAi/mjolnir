// Port of the Windows-only parts of mj-accounts/src/harness_login.rs.

package accounts

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"os"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// loginPlan is how the command-line login runs. Windows has no ACP sign-in
// and no pinned CLI, so it is the profile's configured command.
type loginPlan struct{}

// prepareLogin refuses a requested ACP method, which needs a Unix host.
func prepareLogin(ctx context.Context, profileID string, profile *core.HarnessProfile, options LoginOptions, before *string, stored bool) (*loginPlan, bool, error) {
	if options.Method != nil {
		return nil, false, errors.New("--method requires a Unix host")
	}
	return &loginPlan{}, false, nil
}

func (*loginPlan) release() {}

func (*loginPlan) environment(profile *core.HarnessProfile) map[string]string {
	return configuredLoginEnvironment(profile)
}

func (*loginPlan) arguments(profile *core.HarnessProfile, arguments []string, environment map[string]string) []string {
	return arguments
}

func (*loginPlan) program(configured string, environment map[string]string) string {
	return resolveHarnessProgram(configured, environment)
}

// setupTokenProgram is the `claude` that mints a setup token, with its
// environment.
func setupTokenProgram(ctx context.Context, profile *core.HarnessProfile) (string, map[string]string, func()) {
	environment := configuredLoginEnvironment(profile)
	return resolveHarnessProgram("claude", environment), maps.Clone(environment), func() {}
}

// loginExitStatus is Rust's ExitStatus Display on Windows: the exit code, in
// hexadecimal when its high bit marks an exception status.
func loginExitStatus(state *os.ProcessState) string {
	code := uint32(state.ExitCode())
	if code&0x80000000 != 0 {
		return fmt.Sprintf("exit code: %#x", code)
	}
	return fmt.Sprintf("exit code: %d", code)
}

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
