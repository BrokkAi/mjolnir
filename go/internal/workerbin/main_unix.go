// Part of the port of mj-worker-bin/src/main.rs: its `cfg(unix)` arms.

//go:build unix

package workerbin

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// acquireRootOwner takes ownership of a worker run's root: the lock
// descriptor the login re-exec carried, or the lock itself, then prepares
// startup.
func acquireRootOwner(root string, lockFD *int32, loginEnvironmentReady bool) (*worker.WorkerRootOwner, error) {
	var owner *worker.WorkerRootOwner
	var err error
	if lockFD != nil {
		owner, err = worker.WorkerRootOwnerFromReexec(root, int(*lockFD))
	} else {
		owner, err = worker.WorkerRootOwnerAcquire(root)
	}
	if err != nil {
		return nil, err
	}
	if err := owner.PrepareStartup(loginEnvironmentReady); err != nil {
		owner.Release()
		return nil, err
	}
	return owner, nil
}

// bootstrapLoginEnvironmentReexec discovers the target's login environment
// (or, when needsLogin is false, only the bootstrap environment a
// checkpoint-only worker needs), applies the launch's image environment,
// target settings and credential policy, and re-executes the worker with
// exactly that environment, `--login-environment-ready` and, for a worker
// run, `--worker-root-lock-fd`. It returns only when the exec failed; the
// root lock is then no longer inheritable.
//
// Lifecycle probes identify the installed `hel worker run --root` prefix,
// so the re-exec keeps every argument intact, argv[0] included. On Linux it
// executes /proc/self/exe.
func bootstrapLoginEnvironmentReexec(command workerCommand, needsLogin bool, owner *worker.WorkerRootOwner) error {
	var environment harnessinstall.LoginEnvironmentEnvironment
	var err error
	if needsLogin {
		environment, err = harnessinstall.Discover()
	} else {
		environment, err = harnessinstall.Bootstrap()
	}
	if err != nil {
		return err
	}
	var config string
	switch launch := command.(type) {
	case workerCommandRun:
		config = launch.config
	case workerCommandPrepareHarness:
		config = launch.config
	}
	if config != "" {
		launch, err := core.WorkerLaunchConfigRead(config)
		if err != nil {
			return err
		}
		// On a container target the worker's own environment is the image's
		// declared `ENV`, which a minimal-seed login shell cannot reproduce.
		// Restore it before the deliberate target settings, which win. This
		// runs before the environment-clearing re-exec, so the ambient
		// environment is still the container's.
		if launch.SeedImageEnvironment {
			harnessinstall.OverlayImageEnvironment(environment, envVars())
			if run, ok := command.(workerCommandRun); ok && !launch.NoGithubCredentials {
				if err := worker.SeedContainerGithubToken(run.root, envMap()); err != nil {
					return err
				}
			}
		}
		targetEnvironment := launch.TargetEnvironment
		if _, ok := targetEnvironment[core.SessionNitroMbxEnv]; ok {
			delete(targetEnvironment, core.SessionNitroMbxEnv)
			delete(environment, "MJ_MBX_CONFIG_DIR")
			// Drop settings inherited from login scripts and the image
			// overlay, then restore only the engine's remote settings.
			for name := range environment {
				if strings.HasPrefix(name, core.MbxRemoteEnvPrefix) {
					delete(environment, name)
				}
			}
			for _, name := range core.MbxRemoteEnvironment {
				if value, ok := os.LookupEnv(name); ok {
					environment[name] = value
				}
			}
		}
		for name, value := range targetEnvironment {
			environment[name] = value
		}
		if launch.NoGithubCredentials {
			core.RemoveGithubAuthenticationEnvironment(environment)
		}
	}
	if push, ok := command.(workerCommandPushBranch); ok {
		launch, err := core.WorkerLaunchConfigRead(filepath.Join(push.root, "launch.json"))
		if err != nil {
			return fmt.Errorf("load branch export target settings; %s: %w", worker.SessionSetupGuidance, err)
		}
		for name, value := range launch.TargetEnvironment {
			environment[name] = value
		}
		if err := worker.AttachSessionGitEnvironment(push.root, environment, launch.NoGithubCredentials); err != nil {
			return err
		}
	}
	// The worker consumes this setting from its launch map. It never enters
	// the process environment where subprocesses could inherit it.
	delete(environment, core.FastinoKeyEnvironment)
	delete(environment, core.SessionGitConfigIncludePath)
	// The reliability lab's crash hooks run inside the worker after this
	// environment-isolating re-exec. Carry only those feature-gated test
	// controls across; ordinary launcher variables must stay excluded.
	if testHooksFeature {
		for _, name := range testHookEnvironment {
			if value, ok := os.LookupEnv(name); ok {
				environment[name] = value
			}
		}
	}
	executable := "/proc/self/exe"
	if runtime.GOOS != "linux" {
		executable, err = os.Executable()
		if err != nil {
			return err
		}
	}
	if run, ok := command.(workerCommandRun); ok {
		worker.RecordStartupStep(run.root, "re-exec")
	}
	if len(os.Args) == 0 {
		return errors.New("worker executable argument is missing")
	}
	// Lifecycle probes identify the installed `hel worker run --root`
	// prefix. Keep it intact across re-exec, including argv[0].
	arguments := append(append([]string{}, os.Args...), "--login-environment-ready")
	if owner != nil {
		fd, err := owner.SetReexecInheritance(true)
		if err != nil {
			return err
		}
		arguments = append(arguments, "--worker-root-lock-fd", strconv.Itoa(fd))
	}
	variables := make([]string, 0, len(environment))
	for name, value := range environment {
		variables = append(variables, name+"="+value)
	}
	execErr := syscall.Exec(executable, arguments, variables)
	if owner != nil {
		if _, err := owner.SetReexecInheritance(false); err != nil {
			return err
		}
	}
	return fmt.Errorf("start worker with target login environment: %w", execErr)
}
