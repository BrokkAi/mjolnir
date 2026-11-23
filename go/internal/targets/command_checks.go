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
	"strings"

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
	reported := strings.TrimSpace(lossyText(stderr))
	detail := reported
	if index := strings.LastIndex(reported, "\nCaused by:\n"); index >= 0 {
		detail = reported[index+len("\nCaused by:\n"):]
	}
	detail = strings.TrimPrefix(detail, "Error: ")
	lines := libLines(detail)
	for i, line := range lines {
		lines[i] = strings.TrimPrefix(line, "    ")
	}
	return strings.TrimSpace(strings.Join(lines, "\n"))
}

// InstalledDigestMatches reports whether the digest command prints for an
// installed file equals expected. A command that fails, or prints nothing,
// does not match.
func InstalledDigestMatches(executor core.CommandExecutor, command *core.CommandSpec, expected string) bool {
	output, err := executor.Execute(command)
	if err != nil || output.Status != 0 {
		return false
	}
	fields := strings.Fields(lossyText(output.Stdout))
	return len(fields) > 0 && libEqualFoldASCII(fields[0], expected)
}

// IsMissingCommand reports whether a command failed because its program is
// not installed.
func IsMissingCommand(err error) bool {
	return errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound)
}
