// Port of mj-worker-staging/src/worker_binary/session_token.rs.

// The session GitHub token a person configured for their sessions: reading
// it on the controller's machine and installing it beside a worker.

package workerstaging

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ConfiguredSessionGithubToken reads the controller-side override afresh;
// missing is an error when configured. False when none is configured.
func ConfiguredSessionGithubToken(github *core.GithubConfig) (string, bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/session_token.rs configured_session_github_token")
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
	panic("unported: mj-worker-staging/src/worker_binary/session_token.rs install_configured_session_github_token")
}
