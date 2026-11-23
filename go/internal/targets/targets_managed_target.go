// Port of mj-targets/src/targets/managed_target.rs.
//
// Commands addressed to the machine that holds a managed worktree: this one,
// or another reached over SSH.

package targets

import (
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
	panic("unported: mj-targets/src/targets/managed_target.rs path_exists_on_managed_target")
}
