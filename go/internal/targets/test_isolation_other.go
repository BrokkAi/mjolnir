// Port of the non-Unix parts of mj-targets/src/test_isolation.rs.

//go:build !unix

package targets

import (
	"bytes"
	"errors"
	"fmt"
	"os/exec"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Output runs the child and returns its output without judging it.
func (t *IsolatedTest) Output() core.ProcessOutput {
	command := t.command
	command.Env = t.environment()
	var stdout, stderr bytes.Buffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	err := command.Run()
	var exited *exec.ExitError
	if err != nil && !errors.As(err, &exited) {
		panic(fmt.Sprintf("run isolated %s: %v", t.name, err))
	}
	return core.ProcessOutput{Status: command.ProcessState, Stdout: stdout.Bytes(), Stderr: stderr.Bytes()}
}
