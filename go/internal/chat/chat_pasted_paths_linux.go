// Port of mj-chat/src/chat/pasted_paths.rs (Linux).

package chat

import (
	"errors"
	"fmt"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// windowsPathOnThisHost translates a pasted Windows path with `wslpath -u`,
// which knows custom automount roots and `\\wsl.localhost\…` paths that a
// fixed `/mnt/<drive>` mapping gets wrong.
func windowsPathOnThisHost(path string) (string, error) {
	if !runningUnderWSL() {
		return "", fmt.Errorf("%s is a Windows path, which this machine cannot open", path)
	}
	command := core.NewCommandSpec("wslpath", []string{"-u", path}).
		WithPurpose("translate a pasted Windows path")
	output, err := core.CancellableProcessExecutorWithTimeout(10 * time.Second).Execute(&command)
	if err != nil {
		return "", fmt.Errorf("run wslpath: %w", err)
	}
	if output.Status != 0 {
		return "", fmt.Errorf("wslpath could not translate %s: %s", path,
			strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�")))
	}
	if !utf8.Valid(output.Stdout) {
		return "", errors.New("wslpath output was not UTF-8")
	}
	translated := strings.TrimRight(string(output.Stdout), "\r\n")
	if translated == "" {
		return "", fmt.Errorf("wslpath printed no path for %s", path)
	}
	return translated, nil
}
