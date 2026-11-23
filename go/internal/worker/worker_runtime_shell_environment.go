// Port of mj-worker/src/worker_runtime/shell_environment.rs.

// Session shell hooks and safely published executable wrappers.

//go:build unix

package worker

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const shellEnvironmentScript = `if [ -n "${MJ_ORIGINAL_BASH_ENV:-}" ] && [ "${MJ_ORIGINAL_BASH_ENV}" != "${BASH_ENV:-}" ] && [ -r "${MJ_ORIGINAL_BASH_ENV}" ]; then
    . "${MJ_ORIGINAL_BASH_ENV}"
fi
if [ -n "${MJ_GITHUB_CLI_BIN:-}" ]; then
    if [ "${MJ_GITHUB_CREDENTIALS:-}" = disabled ]; then
        unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
        unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS GIT_SSH GIT_SSH_COMMAND
        GH_CONFIG_DIR=$MJ_GITHUB_CLI_BIN/../gh-without-credentials
        GIT_CONFIG_GLOBAL=$MJ_GITHUB_CLI_BIN/../gitconfig
        GIT_CONFIG_NOSYSTEM=1
        GIT_TERMINAL_PROMPT=0
        GIT_ASKPASS=/bin/false
        export GH_CONFIG_DIR GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM GIT_TERMINAL_PROMPT GIT_ASKPASS
    fi
    case ":${PATH:-}:" in
        *:"${MJ_GITHUB_CLI_BIN}":*) ;;
        *) PATH="${MJ_GITHUB_CLI_BIN}${PATH:+:${PATH}}"; export PATH ;;
    esac
fi
if [ -n "${MJ_BUILD_TOOL_BIN:-}" ]; then
    case ":${PATH:-}:" in
        *:"${MJ_BUILD_TOOL_BIN}":*) ;;
        *) PATH="${MJ_BUILD_TOOL_BIN}${PATH:+:${PATH}}"; export PATH ;;
    esac
fi
`

// shellEnvironmentConfigure installs the session's BASH_ENV hook at `path`
// and points BASH_ENV at it through MJ_SESSION_BASH_ENV, keeping a
// user-supplied hook as MJ_ORIGINAL_BASH_ENV.
func shellEnvironmentConfigure(path string, environment map[string]string) error {
	const original = "MJ_ORIGINAL_BASH_ENV"
	const owned = "MJ_SESSION_BASH_ENV"
	const reference = "${MJ_SESSION_BASH_ENV}"
	if metadata, err := os.Lstat(path); err == nil && metadata.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("session shell environment %s is a symbolic link", path)
	}
	if err := core.AtomicWriteExisting(path, []byte(shellEnvironmentScript)); err != nil {
		return err
	}
	if err := os.Chmod(path, 0o600); err != nil {
		return err
	}
	text := path
	// Git preparation and cache preparation can both install this hook. Never
	// chain one generated hook to another: a restart could make a cycle.
	var chosen *string
	if configured, ok := environment["BASH_ENV"]; ok {
		ownedValue, hasOwned := environment[owned]
		if configured != reference && configured != text && (!hasOwned || ownedValue != configured) {
			chosen = &configured
		}
	}
	if chosen == nil {
		if previous, ok := environment[original]; ok {
			chosen = &previous
		}
	}
	if chosen != nil {
		environment[original] = *chosen
	} else {
		delete(environment, original)
	}
	environment[owned] = text
	// Bash expands BASH_ENV before opening it. Indirection prevents dollar
	// signs or command substitutions in a directory name from being expanded.
	environment["BASH_ENV"] = reference
	return nil
}

// writeExecutable publishes an executable script at `path` without ever
// holding a writable descriptor to it in this process.
func writeExecutable(path string, contents string) error {
	parent := filepath.Dir(path)
	if parent == "" {
		return errors.New("launcher has no parent")
	}
	staging, err := os.MkdirTemp(parent, ".tmp")
	if err != nil {
		return err
	}
	defer os.RemoveAll(staging)
	temporary := filepath.Join(staging, "launcher")
	// Never open an executable for writing in this multithreaded process.
	// An unrelated fork can inherit even a CLOEXEC writer until it execs,
	// making the published launcher fail with ETXTBSY after our own close.
	// This single-threaded shell uses only builtins, so it cannot pass the
	// writer to another process. Reaping it ends every writable description
	// before we publish the inode. Keep the body on stdin, without ARG_MAX.
	script := fmt.Sprintf("umask 077\nprintf '%%s' %s > \"$1\"\n", core.PosixQuote(contents))
	output, err := core.RunWithInput(exec.Command("/bin/sh", "-s", "--", temporary), []byte(script))
	if err != nil {
		return fmt.Errorf("write launcher %s: %w", path, err)
	}
	if !output.Status.Success() {
		return fmt.Errorf("write launcher %s: %s", path, fromUTF8Lossy(output.Stderr))
	}
	if err := os.Chmod(temporary, 0o700); err != nil {
		return err
	}
	if err := os.Rename(temporary, path); err != nil {
		return fmt.Errorf("publish launcher %s: %w", path, err)
	}
	return nil
}
