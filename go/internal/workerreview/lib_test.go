// Port of the tests module in mj-worker-review/src/lib.rs.

//go:build unix

package workerreview

import (
	"fmt"
	"os/exec"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// testsGit is tests::git: run Git in the test repository and return its
// trimmed standard output. It panics when Git cannot run or fails.
func testsGit(repository string, arguments ...string) string {
	command := exec.Command("git", arguments...)
	command.Dir = repository
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		panic(fmt.Sprintf("git %q: %v", arguments, err))
	}
	if !output.Status.Success() {
		panic(fmt.Sprintf("git %q: %s", arguments, captureLossy(output.Stderr)))
	}
	return strings.TrimSpace(string(output.Stdout))
}
