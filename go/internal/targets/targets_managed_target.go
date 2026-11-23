// Port of mj-targets/src/targets/managed_target.rs.
//
// Commands addressed to the machine that holds a managed worktree: this one,
// or another reached over SSH.

package targets

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ManagedTargetSSH is the SSH host of a managed worktree, or nil for this
// machine.
func ManagedTargetSSH(target core.ManagedWorktreeTarget) *core.SshTarget {
	ssh, ok := target.(core.ManagedWorktreeTargetSsh)
	if !ok {
		return nil
	}
	return &core.SshTarget{Destination: ssh.Destination, SSHArgs: append([]string(nil), ssh.SSHArgs...)}
}

func ManagedTargetCommand(class core.SshTrafficClass, target core.ManagedWorktreeTarget, program string, args []string) core.CommandSpec {
	args = append([]string(nil), args...)
	ssh := ManagedTargetSSH(target)
	if ssh == nil {
		return core.NewCommandSpec(program, args)
	}
	return core.SSHCommand(class, ssh, append([]string{program}, args...))
}

func PathExistsOnManagedTarget(executor core.CommandExecutor, target core.ManagedWorktreeTarget, path string) (bool, error) {
	if _, remote := target.(core.ManagedWorktreeTargetSsh); !remote {
		_, err := os.Stat(path)
		switch {
		case err == nil:
			return true, nil
		case errors.Is(err, fs.ErrNotExist):
			return false, nil
		}
		return false, fmt.Errorf("check managed project path %s: %w", path, err)
	}
	command := ManagedTargetCommand(core.SshTrafficClassControl, target, "test", []string{"-e", path}).
		WithPurpose("check managed worktree path")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	switch output.Status {
	case 0:
		return true, nil
	case 1:
		return false, nil
	}
	return false, fmt.Errorf("check managed worktree path failed with status %d: %s", output.Status, strings.TrimSpace(lossyText(output.Stderr)))
}
