//go:build unix

// Port of the Unix-only parts of mj-accounts/src/harness_login.rs: the ACP
// sign-in tried before the harness's own command-line login, and the pinned
// CLI that login runs.

package accounts

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"os"
	"slices"
	"syscall"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loginPlan is how the command-line login runs after the ACP sign-in did
// not finish it: with the pinned CLI when there is one, and with device-code
// sign-in when the ACP step asked for it.
type loginPlan struct {
	pinned     *pinnedCLI
	deviceAuth bool
}

// prepareLogin tries the ACP sign-in unless the command-line login was asked
// for. True when it signed the profile in, and nothing is left to run.
func prepareLogin(ctx context.Context, profileID string, profile *core.HarnessProfile, options LoginOptions, before *string, stored bool) (*loginPlan, bool, error) {
	plan := &loginPlan{}
	if options.CLI && profile.Kind == core.HarnessKindAntigravity {
		pinned, err := locatePinnedCLI(ctx, profile)
		if err != nil {
			return nil, false, err
		}
		plan.pinned = &pinned
	}
	if !options.CLI {
		outcome, err := acpLogin(ctx, profile, options.Method)
		if err != nil {
			plan.release()
			return nil, false, err
		}
		name := profile.Kind.DisplayName()
		switch outcome := outcome.(type) {
		case acpLoginOutcomeSignedIn:
			plan.release()
			fmt.Printf("Signed in to profile %s through the ACP method %s.\n", profileID, outcome.Method)
			if err := reportLoginChange(profileID, profile, before, stored); err != nil {
				return nil, false, err
			}
			return nil, true, nil
		case acpLoginOutcomeDeviceAuth:
			fmt.Println("Using device-code sign-in because this machine has no desktop browser.")
			pinned, err := locatePinnedCLI(ctx, profile)
			if err != nil {
				plan.release()
				return nil, false, err
			}
			plan.release()
			plan.pinned = &pinned
			plan.deviceAuth = true
		case acpLoginOutcomeNoLogin:
			plan.release()
			return nil, false, errors.New(outcome.Value)
		case acpLoginOutcomeUnavailable:
			if core.PreparationCancellation(ctx).Err() != nil {
				plan.release()
				return nil, false, errors.New("login cancelled")
			}
			if options.Method != nil {
				plan.release()
				return nil, false, fmt.Errorf(
					"ACP sign-in was requested but is unavailable: %s. Run `mj login --profile %s --cli` instead.",
					outcome.Value, profileID,
				)
			}
			fmt.Fprintf(os.Stderr, "ACP sign-in is unavailable for %s (%s); using its command-line login.\n", name, outcome.Value)
			pinned, err := locatePinnedCLI(ctx, profile)
			if err != nil {
				fmt.Fprintf(os.Stderr, "The pinned %s CLI is also unavailable (%v); using the command on PATH.\n", name, err)
			} else {
				plan.release()
				plan.pinned = &pinned
			}
		}
	}
	restoreDefaultSIGINT()
	return plan, false, nil
}

func (p *loginPlan) release() {
	if p != nil && p.pinned != nil {
		p.pinned.release()
		p.pinned = nil
	}
}

// environment is the pinned CLI's environment, or the profile's.
func (p *loginPlan) environment(profile *core.HarnessProfile) map[string]string {
	if p.pinned != nil {
		return maps.Clone(p.pinned.Environment)
	}
	return configuredLoginEnvironment(profile)
}

// arguments is the login command's arguments: device-code sign-in when the
// ACP step asked for it, and Grok's device code without a desktop browser.
func (p *loginPlan) arguments(profile *core.HarnessProfile, arguments []string, environment map[string]string) []string {
	if p.deviceAuth {
		arguments = []string{"login", "--device-auth"}
	}
	if profile.Kind == core.HarnessKindGrok {
		full := loginAmbientEnvironment()
		maps.Copy(full, environment)
		if !hasDesktopSession(full) && !slices.ContainsFunc(arguments, func(arg string) bool {
			return arg == "--device-auth" || arg == "--device-code"
		}) {
			arguments = append(arguments, "--device-auth")
		}
	}
	return arguments
}

// program is the pinned CLI, or the configured program.
func (p *loginPlan) program(configured string, environment map[string]string) string {
	if p.pinned != nil {
		return p.pinned.Command
	}
	return configured
}

// setupTokenProgram is the `claude` that mints a setup token, with its
// environment: the pinned CLI, or `claude` from PATH when that cannot be
// prepared. release gives the pinned installation back.
func setupTokenProgram(ctx context.Context, profile *core.HarnessProfile) (string, map[string]string, func()) {
	pinned, err := locatePinnedCLI(ctx, profile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Could not prepare the pinned Claude CLI (%v); using `claude` from PATH.\n", err)
		return "claude", configuredLoginEnvironment(profile), func() {}
	}
	return pinned.Command, maps.Clone(pinned.Environment), pinned.release
}

// loginExitStatus is Rust's ExitStatus Display on Unix: "exit status: N",
// or "signal: N (SIGNAME)" with " (core dumped)" when it dumped core.
func loginExitStatus(state *os.ProcessState) string {
	status, ok := state.Sys().(syscall.WaitStatus)
	if !ok || !status.Signaled() {
		return fmt.Sprintf("exit status: %d", state.ExitCode())
	}
	signal := status.Signal()
	text := fmt.Sprintf("signal: %d", int(signal))
	if name := unix.SignalName(signal); name != "" {
		text += " (" + name + ")"
	}
	if status.CoreDump() {
		text += " (core dumped)"
	}
	return text
}
