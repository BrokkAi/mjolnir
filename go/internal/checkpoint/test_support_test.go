// Port of mj-checkpoint/src/test_support.rs.
//
// Fixtures shared by more than one test file in this package.

package checkpoint

import (
	"fmt"
	"os/exec"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// git runs Git in repository and returns its raw standard output. It panics
// when Git cannot run or fails, as the Rust fixture's unwrap and assert do.
func git(repository string, arguments ...string) []byte {
	command := exec.Command("git", arguments...)
	command.Dir = repository
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		panic(fmt.Sprintf("git %q: %v", arguments, err))
	}
	if !output.Status.Success() {
		panic(fmt.Sprintf("git %q: %s", arguments, strings.ToValidUTF8(string(output.Stderr), "�")))
	}
	return output.Stdout
}

// gitLine runs Git in repository and returns its trimmed standard output.
func gitLine(repository string, arguments ...string) string {
	return strings.TrimSpace(string(git(repository, arguments...)))
}

// git231 is Git 2.31, which does not support NUL-delimited worktree lists.
type git231 struct{}

// Run refuses `worktree ... -z` as Git 2.31 does and runs everything else
// with the system's Git.
func (git231) Run(repository string, command *GitCommand) (GitOutput, error) {
	if slices.Contains(command.Arguments, "worktree") && slices.Contains(command.Arguments, "-z") {
		return GitOutput{
			Status: 129,
			Stdout: []byte{},
			Stderr: []byte("error: unknown switch `z'"),
		}, nil
	}
	return SystemGit{}.Run(repository, command)
}
