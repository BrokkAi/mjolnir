// Port of mj-worker-staging/src/worker_binary/session_token.rs.

// The session GitHub token a person configured for their sessions: reading
// it on the controller's machine and installing it beside a worker.

package workerstaging

import (
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// ConfiguredSessionGithubToken reads the controller-side override afresh;
// missing is an error when configured. False when none is configured.
func ConfiguredSessionGithubToken(github *core.GithubConfig) (string, bool, error) {
	if github.SessionTokenPath == nil {
		return "", false, nil
	}
	path, err := core.ExpandLocal(*github.SessionTokenPath)
	if err != nil {
		return "", false, fmt.Errorf("resolve [github] session_token_path: %w", err)
	}
	read := func() (string, error) {
		info, err := os.Stat(path)
		if err != nil {
			return "", rustIOError(err)
		}
		if !info.Mode().IsRegular() {
			return "", errors.New("session token must be a regular file")
		}
		file, err := os.Open(path)
		if err != nil {
			return "", rustIOError(err)
		}
		defer file.Close()
		bytes, err := io.ReadAll(io.LimitReader(file, core.MaxGithubTokenBytes+1))
		if err != nil {
			return "", rustIOError(err)
		}
		return core.ValidateGithubToken(bytes)
	}
	token, err := read()
	if err != nil {
		return "", false, fmt.Errorf("read [github] session_token_path %s: %w", path, err)
	}
	return token, true, nil
}

// sessionGithubTokenInstallScript is the INSTALL script of
// InstallConfiguredSessionGithubToken.
const sessionGithubTokenInstallScript = `set -eu
umask 077
root=$1
path=$root/github-token
if [ -L "$path" ] || { [ -e "$path" ] && [ ! -f "$path" ]; }; then
    echo 'session GitHub token is not a regular file' >&2; exit 1
fi
temporary=$(mktemp "$root/github-token.XXXXXXXX")
trap 'rm -f -- "$temporary"' EXIT
cat > "$temporary"
mv -f -- "$temporary" "$path"
`

// InstallConfiguredSessionGithubToken refreshes the override before a
// worker starts, including retained workers. Lifecycle ownership excludes
// periodic reconciliation while this runs.
func InstallConfiguredSessionGithubToken(github *core.GithubConfig, executor core.CommandExecutor, locator *core.TargetsTargetLocator, workerRoot string) error {
	token, ok, err := ConfiguredSessionGithubToken(github)
	if err != nil || !ok {
		return err
	}
	// A token is a few bytes and a session waits on it, so it must not queue
	// behind bulk transfers to the same target.
	command := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{
		"sh", "-c", sessionGithubTokenInstallScript, "mj-session-github-token", workerRoot,
	}).WithPurpose("install configured session GitHub token").
		WithSensitiveStdin([]byte(token + "\n"))
	_, err = targets.ExecuteChecked(executor, command)
	return err
}
