// Port of mj-cli/src/daemon/wait_exit.rs.
//
// Wait for one daemon process and record its abnormal exit
// (`mj daemon-wait-exit`).

package cli

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// daemonWaitExitArgs are the arguments of `mj daemon-wait-exit` (Rust
// `DaemonWaitExitArgs`, a clap Args struct): `--exit-dir <EXIT_DIR>` and
// the required trailing command, which may begin with hyphens
// (`trailing_var_arg`, `allow_hyphen_values`).
type daemonWaitExitArgs struct {
	exitDir string
	command []string
}

// waiterSignals (Unix) are ignored by the waiter and restored to their
// defaults in the daemon it starts.
var waiterSignals = []os.Signal{syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP}

// waitExitRun runs the daemon command, waits for it, and records an exit
// other than 0 or core.StoreOwnedExitCode in args.exitDir (Rust
// `wait_exit::run`).
func waitExitRun(args daemonWaitExitArgs) error {
	if len(args.command) == 0 {
		return errors.New("missing daemon program")
	}
	program, childArgs := args.command[0], args.command[1:]
	arguments := append([]string(nil), childArgs...)
	if runtime.GOOS == "linux" {
		// systemd-run execs the live waiter's inode, which survives an
		// upgrade.
		executable := fmt.Sprintf("/proc/%d/exe", os.Getpid())
		for index, arg := range arguments {
			if program != "/proc/self/exe" && arg == "/proc/self/exe" {
				arguments[index] = executable
			}
		}
	}
	command := exec.Command(program, arguments...)
	if runtime.GOOS != "windows" {
		if (program == "/proc/self/exe" || isNamedMJ(program)) && len(childArgs) > 0 && childArgs[0] == "daemon-run" {
			command.Args[0] = "mj"
		}
		// SIG_IGN would survive exec into the daemon, and Go has no
		// pre_exec to restore the defaults there. Catching the signals here
		// ignores them in the waiter, and exec resets caught signals to
		// their defaults in the daemon, which installs its own shutdown
		// handlers.
		ignored := make(chan os.Signal, 1)
		signal.Notify(ignored, waiterSignals...)
		defer signal.Stop(ignored)
		go func() {
			for range ignored {
			}
		}()
	}
	configureWaiterChild(command)

	// Inherit stdio without creating any pipes. The waiter owns no daemon
	// state.
	command.Stdin, command.Stdout, command.Stderr = os.Stdin, os.Stdout, os.Stderr
	if err := command.Start(); err != nil {
		return fmt.Errorf("spawn daemon program %s: %w", core.RustDebugString(program), err)
	}
	childPID := uint32(command.Process.Pid)
	err := command.Wait()
	var exit *exec.ExitError
	if err != nil && !errors.As(err, &exit) {
		return fmt.Errorf("wait for daemon process: %w", err)
	}
	status := command.ProcessState
	code, hasCode := exitCode(status)
	if hasCode && (code == 0 || code == core.StoreOwnedExitCode) {
		return nil
	}

	signalNumber, signalName, coreDumped := waiterExitSignal(status)
	waiterPID := uint32(os.Getpid())
	record := core.DaemonExitRecord{
		Schema:     core.DaemonExitSchema,
		Pid:        childPID,
		WaiterPid:  &waiterPID,
		ExitedAt:   time.Now().UTC().Format("2006-01-02T15:04:05Z"),
		Signal:     signalNumber,
		SignalName: signalName,
		CoreDumped: coreDumped,
	}
	if hasCode {
		record.ExitCode = &code
	}
	if _, err := core.WriteDaemonExitRecord(args.exitDir, &record); err != nil {
		fmt.Fprintf(os.Stderr, "could not record exit of daemon %d: %s\n", childPID, err)
	}
	return nil
}

// isNamedMJ is whether program's file name is `mj`.
func isNamedMJ(program string) bool {
	name, ok := pathFileName(program)
	return ok && name == "mj"
}

// exitCode is Rust's ExitStatus::code: none for a process ended by a
// signal.
func exitCode(status *os.ProcessState) (int32, bool) {
	code := status.ExitCode()
	if code < 0 {
		return 0, false
	}
	return int32(code), true
}
