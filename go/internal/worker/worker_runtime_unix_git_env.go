// Port of mj-worker/src/worker_runtime/unix/git_env.rs.
//
// The session-owned GitHub CLI and slopcop wrappers and the worker-owned Git
// configuration the harness process tree reads.

//go:build unix

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// GithubCliPaths are the session's wrapper directory, its `gh` wrapper, and
// its Git configuration file.
type GithubCliPaths struct {
	bin       string
	wrapper   string
	gitConfig string
}

func newGithubCliPaths(root string) GithubCliPaths {
	bin := filepath.Join(root, "bin")
	return GithubCliPaths{
		wrapper:   filepath.Join(bin, "gh"),
		bin:       bin,
		gitConfig: filepath.Join(root, "gitconfig"),
	}
}

// gitConfigEntry is one `section.subsection.key` setting and its value
// (Rust: a (String, String) pair).
type gitConfigEntry struct {
	key   string
	value string
}

// prependGithubCLIPath puts bin first on the environment's PATH, removing
// any other occurrence of it.
func prependGithubCLIPath(bin string, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs prepend_github_cli_path")
}

// SessionSetupGuidance is what a person can do about a broken or missing
// session Git setup. Every producer of that failure appends this, and the
// branch-export test matches on it, so the guidance and the tests cannot
// drift apart.
const SessionSetupGuidance = "resume the session to restore its setup"

// AttachSessionGitEnvironment attaches an export to existing session
// authentication without rewriting it. The wrapper reads the latest token
// only when Git invokes its helper.
func AttachSessionGitEnvironment(root string, environment map[string]string, noGithubCredentials bool) error {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs attach_session_git_environment")
}

// wrapperPreamble is the start of every wrapper in the session's `bin`
// directory: drop session launcher directories so an inherited parent's
// wrapper cannot restore its authentication when this wrapper runs the
// target's own command.
const wrapperPreamble = `#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
clean_path=
old_ifs=$IFS
IFS=:
for entry in $PATH; do
    if [ "$entry" != "$script_dir" ] && [ ! -f "$entry/../github-shell-env" ]; then
        if [ -z "$clean_path" ]; then clean_path=$entry; else clean_path=$clean_path:$entry; fi
    fi
done
IFS=$old_ifs
PATH=$clean_path
export PATH
`

// ConfigureSlopcopCLI installs the session's `slopcop` wrapper beside the
// GitHub wrapper, after ConfigureGithubCLI has put their directory first on
// PATH.
//
// When login exists, the wrapper exports the controller's slopcop login from
// it on every invocation, so a login that credential sync replaces reaches
// the running session; otherwise the session's own environment and slopcop
// configuration apply, as on the localhost target.
func ConfigureSlopcopCLI(root, login string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs configure_slopcop_cli")
}

// githubCLITokenWrapper is the `gh` wrapper body of a session with GitHub
// credentials: it exports the session's latest token.
const githubCLITokenWrapper = `token_file=$script_dir/../github-token
if [ -f "$token_file" ]; then
    IFS= read -r GH_TOKEN < "$token_file"
    export GH_TOKEN
    unset GITHUB_TOKEN
else
    unset GH_TOKEN GITHUB_TOKEN
fi
exec gh "$@"
`

// githubCLICredentialFreeWrapper is the `gh` wrapper body of a session
// without GitHub credentials.
const githubCLICredentialFreeWrapper = `unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
GH_CONFIG_DIR=$script_dir/../gh-without-credentials
GH_PROMPT_DISABLED=1
export GH_CONFIG_DIR GH_PROMPT_DISABLED
exec gh "$@"
`

// ConfigureGithubCLI installs the session-owned GitHub wrapper and Git
// configuration at startup. savedHome and extraGlobalInclude are nil when
// absent.
func ConfigureGithubCLI(root string, environment map[string]string, savedHome, extraGlobalInclude *string, noGithubCredentials bool) error {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs configure_github_cli")
}

// gitAuthorIdentity retains identity without including any source
// authentication settings.
func gitAuthorIdentity(root string, environment map[string]string, savedHome, extraGlobalInclude *string) ([]gitConfigEntry, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs git_author_identity")
}

// gitSSHWrapper is the `git-ssh` wrapper body of a session without GitHub
// credentials. OpenSSH owns alias and Include interpretation; ask it for the
// effective destination before applying the policy, including for a reused
// socket.
const gitSSHWrapper = `configuration=$(ssh -G "$@")
hostname=$(printf '%s\n' "$configuration" | awk '$1 == "hostname" { print $2; exit }' | tr '[:upper:]' '[:lower:]')
hostname=${hostname%.}
case "$hostname" in
    github.com|gist.github.com|ssh.github.com)
        exec ssh -o PubkeyAuthentication=no -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no -o BatchMode=yes -o ControlMaster=no -o ControlPath=none -o IdentityAgent=none "$@" ;;
    *) exec ssh "$@" ;;
esac
`

// gitCredentialFreeWrapper is the start of the `git` wrapper body of a
// session without GitHub credentials. It matches every URL scope in the
// selected repository: Git's URL matcher can prefer a path-specific local
// setting over a host-wide command override.
const gitCredentialFreeWrapper = `unset GIT_CONFIG_COUNT GIT_SSH GIT_SSH_COMMAND
GIT_CONFIG_GLOBAL=$script_dir/../gitconfig
GIT_CONFIG_NOSYSTEM=1
GIT_TERMINAL_PROMPT=0
GIT_ASKPASS=/bin/false
export GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM GIT_TERMINAL_PROMPT GIT_ASKPASS
real_git=$(command -v git)
repository_authentication_keys() (
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -C) [ "$#" -ge 2 ] || break; if [ -n "$2" ]; then cd -- "$2" || exit; fi; shift 2 ;;
            --git-dir|--work-tree) [ "$#" -ge 2 ] || break
                if [ "$1" = --git-dir ]; then GIT_DIR=$2; export GIT_DIR; else GIT_WORK_TREE=$2; export GIT_WORK_TREE; fi
                shift 2 ;;
            --git-dir=*) GIT_DIR=${1#*=}; export GIT_DIR; shift ;;
            --work-tree=*) GIT_WORK_TREE=${1#*=}; export GIT_WORK_TREE; shift ;;
            --bare) GIT_DIR=.; export GIT_DIR; shift ;;
            -c|--config-env|--namespace|--super-prefix) [ "$#" -ge 2 ] || break; shift 2 ;;
            -*) shift ;;
            *) break ;;
        esac
    done
    "$real_git" config --name-only --get-regexp '^(http|credential)\..*\.(extraheader|helper)$'
)
if keys=$(repository_authentication_keys "$@"); then :; else
    status=$?
    [ "$status" -eq 1 ] || exit "$status"
fi
while IFS= read -r key; do
    url=${key#*.}; url=${url%.*}
    host=${url#*://}; host=${host%%/*}; host=${host##*@}; host=${host%%:*}
    host=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')
    case "$host" in github.com|gist.github.com) set -- -c "$key=" "$@" ;; esac
done <<EOF
$keys
EOF
# Hooks and aliases must route their own Git calls through this policy too.
PATH=$script_dir:$PATH
export PATH
`

func configureGitWithoutCredentials(root string, environment map[string]string, paths *GithubCliPaths, savedHome, extraGlobalInclude *string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs configure_git_without_credentials")
}

func credentialFreeEnvironment(root string, environment map[string]string) {
	core.RemoveGithubAuthenticationEnvironment(environment)
	for _, name := range []string{"GIT_CONFIG_PARAMETERS", "GIT_SSH", "GIT_SSH_COMMAND"} {
		delete(environment, name)
	}
	environment["GH_CONFIG_DIR"] = filepath.Join(root, "gh-without-credentials")
	environment["GIT_CONFIG_GLOBAL"] = filepath.Join(root, "gitconfig")
	environment["MJ_GITHUB_CREDENTIALS"] = "disabled"
	environment["GIT_CONFIG_NOSYSTEM"] = "1"
	environment["GIT_TERMINAL_PROMPT"] = "0"
	environment["GIT_ASKPASS"] = "/bin/false"
}

// SeedContainerGithubToken keeps the GitHub token that the controller put in
// a container's environment as the session's token, so the first turn has
// it.
//
// The worker re-executes itself with a cleared environment before it sets up
// the session, so this must run before that re-exec, while the launcher's
// environment is still the container's. The container's value is fixed when
// the container is created. The controller's credential sync owns every
// later change, so a token that is already there is left alone.
func SeedContainerGithubToken(root string, launcherEnvironment map[string]string) error {
	path := filepath.Join(root, "github-token")
	if _, err := os.Lstat(path); err == nil {
		return nil
	}
	for _, name := range []string{"GH_TOKEN", "GITHUB_TOKEN"} {
		if token := launcherEnvironment[name]; token != "" {
			if _, err := core.WriteGithubToken(path, []byte(token)); err != nil {
				return fmt.Errorf("keep the container's GitHub token for the session: %w", err)
			}
			return nil
		}
	}
	return nil
}

// originalGitConfigGlobal remembers the caller's own GIT_CONFIG_GLOBAL once
// the session's file has replaced it.
const originalGitConfigGlobal = "MJ_ORIGINAL_GIT_CONFIG_GLOBAL"

// configureGitConfigFile gives the harness process tree its Git settings
// through a worker-owned global configuration file named by
// GIT_CONFIG_GLOBAL.
//
// The environment form those settings used to take cannot survive a harness
// that drops credential-shaped variable names. A harness that spawns every
// tool with a parent environment scrubbed of names matching
// `KEY|PASSWORD|SECRET|TOKEN` removes `GIT_CONFIG_KEY_*` while keeping
// `GIT_CONFIG_COUNT`, so each git command in the session fails with
// "missing config key GIT_CONFIG_KEY_0". One path variable carries no
// credential-shaped name, and a file cannot be partly delivered.
func configureGitConfigFile(root string, environment map[string]string, paths *GithubCliPaths, savedHome, extraGlobalInclude *string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs configure_git_config_file")
}

// takeEnvironmentGitConfig moves inherited GIT_CONFIG_COUNT entries out of
// the environment and into configuration entries. They must not stay behind:
// a harness that drops only some of those variables would leave Git a count
// it cannot satisfy, which fails every git command rather than only losing a
// setting.
func takeEnvironmentGitConfig(environment map[string]string) []gitConfigEntry {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs take_environment_git_config")
}

// isIndexedGitConfigName is whether name is GIT_CONFIG_<kind>_<digits>.
func isIndexedGitConfigName(name, kind string) bool {
	rest, ok := strings.CutPrefix(name, "GIT_CONFIG_")
	if !ok {
		return false
	}
	if rest, ok = strings.CutPrefix(rest, kind); !ok {
		return false
	}
	index, ok := strings.CutPrefix(rest, "_")
	if !ok || index == "" {
		return false
	}
	for _, digit := range index {
		if digit < '0' || digit > '9' {
			return false
		}
	}
	return true
}

// renderGitConfig renders entries as a Git configuration file. Keys arrive
// in the `section.subsection.key` form `GIT_CONFIG_KEY_*` and `git -c`
// accept.
func renderGitConfig(entries []gitConfigEntry) (string, error) {
	var text strings.Builder
	text.WriteString("# Written by Mjolnir for this session; edits are overwritten.\n")
	openSection, open := "", false
	for _, entry := range entries {
		section, name, err := splitGitConfigKey(entry.key)
		if err != nil {
			return "", err
		}
		if !open || openSection != section {
			text.WriteString(section + "\n")
			openSection, open = section, true
		}
		quoted, err := quoteGitConfigValue(entry.value)
		if err != nil {
			return "", err
		}
		text.WriteString("\t" + name + " = " + quoted + "\n")
	}
	return text.String(), nil
}

// splitGitConfigKey splits a configuration key into its rendered section
// header and its name.
func splitGitConfigKey(key string) (header, name string, err error) {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs split_git_config_key")
}

func quoteGitConfigValue(value string) (string, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/git_env.rs quote_git_config_value")
}

func escapeGitConfigText(text string) string {
	return strings.ReplaceAll(strings.ReplaceAll(text, `\`, `\\`), `"`, `\"`)
}
