// Port of mj-targets/src/command_checks.rs.
//
// Running a command and turning a failure into the error its output
// describes.

package targets

import (
	"errors"
	"fmt"
	"io/fs"
	"os/exec"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func ExecuteChecked(executor core.CommandExecutor, command core.CommandSpec) (core.CommandOutput, error) {
	output, err := executor.Execute(&command)
	if err != nil {
		return core.CommandOutput{}, err
	}
	return CheckOutput(&command, output)
}

// CheckOutput is output of command when it succeeded, otherwise the error
// its stderr describes.
func CheckOutput(command *core.CommandSpec, output core.CommandOutput) (core.CommandOutput, error) {
	if message, ok := core.StepScriptError(&output); ok {
		return core.CommandOutput{}, errors.New(message)
	}
	if output.Status != 0 {
		detail := CommandErrorDetail(output.Stderr)
		if detail == "" {
			return core.CommandOutput{}, fmt.Errorf("%s failed with status %d", command.Purpose, output.Status)
		}
		return core.CommandOutput{}, errors.New(detail)
	}
	return output, nil
}

func CommandErrorDetail(stderr []byte) string {
	panic("unported: mj-targets/src/command_checks.rs command_error_detail")
}

// InstalledDigestMatches reports whether the digest command prints for an
// installed file equals expected. A command that fails, or prints nothing,
// does not match.
func InstalledDigestMatches(executor core.CommandExecutor, command *core.CommandSpec, expected string) bool {
	panic("unported: mj-targets/src/command_checks.rs installed_digest_matches")
}

// IsMissingCommand reports whether a command failed because its program is
// not installed.
func IsMissingCommand(err error) bool {
	return errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound)
}
