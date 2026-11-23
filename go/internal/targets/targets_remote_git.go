// Port of mj-targets/src/targets/remote_git.rs.

package targets

import (
	"io"
	"maps"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RemoteGitExecutor reuses the same Git configuration resolver on a remote
// bare host.
type RemoteGitExecutor struct {
	core.CommandExecutorDefaults
	Executor core.CommandExecutor
	SSH      core.SshTarget
}

func (e *RemoteGitExecutor) remote(command *core.CommandSpec) core.CommandSpec {
	arguments := []string{"env"}
	for _, key := range slices.Sorted(maps.Keys(command.Env)) {
		arguments = append(arguments, key+"="+command.Env[key])
	}
	arguments = append(arguments, command.Program)
	arguments = append(arguments, command.Args...)
	return core.SSHCommand(core.SshTrafficClassControl, &e.SSH, arguments).WithPurpose(command.Purpose)
}

func (e *RemoteGitExecutor) PauseDeadline() *core.DeadlinePause {
	return e.Executor.PauseDeadline()
}

func (e *RemoteGitExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.Executor.CancellationRequestedWithoutDeadline()
}

func (e *RemoteGitExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	remote := e.remote(command)
	return e.Executor.Execute(&remote)
}

// ExecuteCleanup is the Rust trait default: Execute.
func (e *RemoteGitExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

// ExecuteCleanupWithTimeout is the Rust trait default: ExecuteCleanup.
func (e *RemoteGitExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e *RemoteGitExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	remote := e.remote(command)
	return e.Executor.ExecuteWithoutDeadline(&remote)
}

func (e *RemoteGitExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	// Like ExecuteWithStdin, streamed input is not supported here.
	return e.ExecuteWithStdin(command, input)
}

func (e *RemoteGitExecutor) CancellationRequested() bool {
	return e.Executor.CancellationRequested()
}
