// Port of mj-worker/src/worker_runtime/unix/git_env.rs.
//
// The session-owned GitHub CLI and slopcop wrappers and the worker-owned Git
// configuration the harness process tree reads.

//go:build unix

package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

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
	inherited := environment["PATH"]
	entries := []string{bin}
	if inherited != "" {
		for _, entry := range filepath.SplitList(inherited) {
			if entry != bin {
				entries = append(entries, entry)
			}
		}
	}
	// Rust's env::join_paths refuses an entry that contains the separator.
	for _, entry := range entries {
		if strings.ContainsRune(entry, os.PathListSeparator) {
			return fmt.Errorf("prepend session GitHub CLI to PATH: path segment contains separator `%c`", os.PathListSeparator)
		}
	}
	environment["PATH"] = strings.Join(entries, string(os.PathListSeparator))
	return nil
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
	root, err := gitEnvAbsolute(root)
	if err != nil {
		return fmt.Errorf("resolve session worker root: %w", err)
	}
	paths := newGithubCliPaths(root)
	validate := func() error {
		info, err := os.Lstat(paths.bin)
		if err != nil {
			return err
		}
		if !info.IsDir() {
			return errors.New("GitHub wrapper directory is not a directory")
		}
		for _, path := range []string{paths.wrapper, paths.gitConfig} {
			info, err := os.Lstat(path)
			if err != nil {
				return fmt.Errorf("inspect %s: %w", path, err)
			}
			if !info.Mode().IsRegular() {
				return fmt.Errorf("%s is not a regular file", path)
			}
			file, err := os.Open(path)
			if err != nil {
				return fmt.Errorf("read %s: %w", path, err)
			}
			_ = file.Close()
		}
		return nil
	}
	if err := validate(); err != nil {
		return fmt.Errorf("load session Git authentication from %s; %s: %w", root, SessionSetupGuidance, err)
	}
	if err := prependGithubCLIPath(paths.bin, environment); err != nil {
		return err
	}
	environment["GIT_CONFIG_GLOBAL"] = paths.gitConfig
	// Startup already migrated these entries into the generated configuration.
	// Reapplying them would outrank its absolute credential helper.
	for name := range environment {
		if name == "GIT_CONFIG_COUNT" ||
			isIndexedGitConfigName(name, "KEY") ||
			isIndexedGitConfigName(name, "VALUE") ||
			name == "GH_TOKEN" ||
			name == "GITHUB_TOKEN" {
			delete(environment, name)
		}
	}
	if noGithubCredentials {
		credentialFreeEnvironment(root, environment)
	}
	return nil
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
	wrapper := filepath.Join(newGithubCliPaths(root).bin, "slopcop")
	if gitEnvIsSymlink(wrapper) {
		return fmt.Errorf("slopcop wrapper %s is a symbolic link", wrapper)
	}
	body := "login_file=" + core.PosixQuote(login) + `
if [ -f "$login_file" ]; then
    SLOPCOP_TOKEN=
    SLOPCOP_SERVICE=
    { IFS= read -r SLOPCOP_TOKEN || :; IFS= read -r SLOPCOP_SERVICE || :; } < "$login_file"
    export SLOPCOP_TOKEN
    if [ -n "$SLOPCOP_SERVICE" ]; then export SLOPCOP_SERVICE; else unset SLOPCOP_SERVICE; fi
fi
if ! command -v slopcop >/dev/null 2>&1; then
    echo "slopcop is not installed where this session runs" >&2
    exit 127
fi
exec slopcop "$@"
`
	return writeExecutable(wrapper, wrapperPreamble+body)
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
	paths := newGithubCliPaths(root)
	bin := paths.bin
	if gitEnvIsSymlink(bin) {
		return fmt.Errorf("GitHub CLI wrapper directory %s is a symbolic link", bin)
	}
	if err := os.MkdirAll(bin, 0o777); err != nil {
		return fmt.Errorf("create GitHub CLI wrapper directory %s: %w", bin, err)
	}
	if err := os.Chmod(bin, 0o700); err != nil {
		return err
	}

	wrapper := paths.wrapper
	if gitEnvIsSymlink(wrapper) {
		return fmt.Errorf("GitHub CLI wrapper %s is a symbolic link", wrapper)
	}
	body := githubCLITokenWrapper
	if noGithubCredentials {
		if err := core.RemoveGithubToken(filepath.Join(root, "github-token")); err != nil {
			return err
		}
		core.RemoveGithubAuthenticationEnvironment(environment)
		config := filepath.Join(root, "gh-without-credentials")
		if err := core.CreateOwnerOnlyDirectory(config); err != nil {
			return err
		}
		hosts := filepath.Join(config, "hosts.yml")
		if err := os.Remove(hosts); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return fmt.Errorf("remove session-owned GitHub login: %w", err)
		}
		body = githubCLICredentialFreeWrapper
	}
	if err := writeExecutable(wrapper, wrapperPreamble+body); err != nil {
		return err
	}

	if err := prependGithubCLIPath(bin, environment); err != nil {
		return err
	}
	environment[githubCLIBinEnv] = bin
	if err := shellEnvironmentConfigure(filepath.Join(root, "github-shell-env"), environment); err != nil {
		return err
	}
	if noGithubCredentials {
		return configureGitWithoutCredentials(root, environment, &paths, savedHome, extraGlobalInclude)
	}
	return configureGitConfigFile(root, environment, &paths, savedHome, extraGlobalInclude)
}

// gitAuthorIdentity retains identity without including any source
// authentication settings.
func gitAuthorIdentity(root string, environment map[string]string, savedHome, extraGlobalInclude *string) ([]gitConfigEntry, error) {
	var arguments []string
	if extraGlobalInclude != nil {
		arguments = append(arguments, "-c", "include.path="+*extraGlobalInclude)
	}
	arguments = append(arguments, "config", "--null", "--get-regexp", `^user\.(name|email)$`)
	command := exec.Command("git", arguments...)
	command.Dir = root
	// Rust's Command::envs adds to the inherited environment; a later entry
	// of the same name wins.
	command.Env = os.Environ()
	for name, value := range environment {
		command.Env = append(command.Env, name+"="+value)
	}
	if savedHome != nil {
		command.Env = append(command.Env, "HOME="+*savedHome)
	}
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		return nil, fmt.Errorf("read Git author identity for a credential-free session: %w", err)
	}
	if output.Status.ExitCode() == 1 {
		return nil, nil
	}
	if !output.Status.Success() {
		return nil, fmt.Errorf("could not read Git author identity (%s)", workFactsExitStatus(output.Status))
	}
	if !utf8.Valid(output.Stdout) {
		return nil, errors.New("decode Git author identity: invalid utf-8 sequence")
	}
	var entries []gitConfigEntry
	for _, entry := range strings.Split(string(output.Stdout), "\x00") {
		if key, value, found := strings.Cut(entry, "\n"); found {
			entries = append(entries, gitConfigEntry{key: key, value: value})
		}
	}
	return entries, nil
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
	entries, err := gitAuthorIdentity(root, environment, savedHome, extraGlobalInclude)
	if err != nil {
		return err
	}
	takeEnvironmentGitConfig(environment)
	delete(environment, originalGitConfigGlobal)
	sshWrapper := filepath.Join(paths.bin, "git-ssh")
	if err := writeExecutable(sshWrapper, wrapperPreamble+gitSSHWrapper); err != nil {
		return err
	}
	sshCommand := core.PosixQuote(sshWrapper)
	for _, host := range []string{"github.com", "gist.github.com"} {
		entries = append(entries,
			gitConfigEntry{key: "credential.https://" + host + ".helper"},
			gitConfigEntry{key: "http.https://" + host + ".extraheader"})
	}
	entries = append(entries, gitConfigEntry{key: "core.sshCommand", value: sshCommand})
	rendered, err := renderGitConfig(entries)
	if err != nil {
		return err
	}
	if err := core.AtomicWriteExisting(paths.gitConfig, []byte(rendered)); err != nil {
		return err
	}
	credentialFreeEnvironment(root, environment)
	wrapper := wrapperPreamble + gitCredentialFreeWrapper +
		`exec "$real_git" -c credential.https://github.com.helper= -c credential.https://gist.github.com.helper= -c http.https://github.com.extraheader= -c http.https://gist.github.com.extraheader= -c core.sshCommand=` +
		core.PosixQuote(sshCommand) + " \"$@\"\n"
	return writeExecutable(filepath.Join(paths.bin, "git"), wrapper)
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
	path := paths.gitConfig
	if gitEnvIsSymlink(path) {
		return fmt.Errorf("Git configuration %s is a symbolic link", path)
	}
	pathText := path

	// This file replaces the global configuration Git would otherwise read,
	// so it includes what it replaces. Remember a caller's own choice
	// separately: after the first call GIT_CONFIG_GLOBAL names this file, and
	// the original would otherwise be lost on the next call.
	var originalGlobal *string
	if configured, ok := environment["GIT_CONFIG_GLOBAL"]; ok && configured != pathText {
		originalGlobal = &configured
	} else if original, ok := environment[originalGitConfigGlobal]; ok {
		originalGlobal = &original
	}
	var entries []gitConfigEntry
	if originalGlobal != nil {
		environment[originalGitConfigGlobal] = *originalGlobal
		entries = []gitConfigEntry{{key: "include.path", value: *originalGlobal}}
	} else {
		delete(environment, originalGitConfigGlobal)
		// Git expands `~` in include paths when each command runs. Pin the
		// config paths now, using the worker's saved HOME, so a harness login
		// shell that changes HOME cannot redirect these includes.
		var home string
		if savedHome != nil {
			home = *savedHome
		} else if value, ok := environment["HOME"]; ok {
			home = value
		} else if value, ok := os.LookupEnv("HOME"); ok {
			home = value
		} else {
			return errors.New("resolve HOME for session Git configuration")
		}
		home, err := gitEnvAbsolute(home)
		if err != nil {
			return fmt.Errorf("make session HOME absolute: %w", err)
		}
		xdgHome := filepath.Join(home, ".config")
		if configHome := environment["XDG_CONFIG_HOME"]; configHome != "" {
			if xdgHome, err = gitEnvAbsolute(configHome); err != nil {
				return fmt.Errorf("make XDG_CONFIG_HOME absolute: %w", err)
			}
		}
		entries = []gitConfigEntry{
			{key: "include.path", value: filepath.Join(xdgHome, "git/config")},
			{key: "include.path", value: filepath.Join(home, ".gitconfig")},
		}
	}
	if extraGlobalInclude != nil {
		include, err := gitEnvAbsolute(*extraGlobalInclude)
		if err != nil {
			return fmt.Errorf("make extra session Git config include absolute: %w", err)
		}
		present := false
		for _, entry := range entries {
			if entry.key == "include.path" && entry.value == include {
				present = true
				break
			}
		}
		if !present {
			entries = append(entries, gitConfigEntry{key: "include.path", value: include})
		}
	}
	// Inherited entries get their own included file. Folding them into this
	// one would lose them the next time it is written, because by then they
	// are gone from the environment. A missing include is ignored, so the
	// include is unconditional.
	inheritedPath := filepath.Join(root, "gitconfig-inherited")
	if gitEnvIsSymlink(inheritedPath) {
		return fmt.Errorf("Git configuration %s is a symbolic link", inheritedPath)
	}
	inherited := takeEnvironmentGitConfig(environment)
	if len(inherited) > 0 {
		rendered, err := renderGitConfig(inherited)
		if err != nil {
			return err
		}
		if err := core.AtomicWriteExisting(inheritedPath, []byte(rendered)); err != nil {
			return err
		}
		if err := os.Chmod(inheritedPath, 0o600); err != nil {
			return err
		}
	}
	entries = append(entries, gitConfigEntry{key: "include.path", value: inheritedPath})

	// The empty value clears image and user helpers before the absolute Hel
	// helper is added. Settings later in the file win, so these outrank every
	// included file.
	helper := "!" + core.PosixQuote(paths.wrapper) + " auth git-credential"
	for _, host := range []string{"github.com", "gist.github.com"} {
		key := "credential.https://" + host + ".helper"
		entries = append(entries, gitConfigEntry{key: key}, gitConfigEntry{key: key, value: helper})
	}

	rendered, err := renderGitConfig(entries)
	if err != nil {
		return err
	}
	if err := core.AtomicWriteExisting(path, []byte(rendered)); err != nil {
		return err
	}
	if err := os.Chmod(path, 0o600); err != nil {
		return err
	}
	environment["GIT_CONFIG_GLOBAL"] = pathText
	return nil
}

// takeEnvironmentGitConfig moves inherited GIT_CONFIG_COUNT entries out of
// the environment and into configuration entries. They must not stay behind:
// a harness that drops only some of those variables would leave Git a count
// it cannot satisfy, which fails every git command rather than only losing a
// setting.
func takeEnvironmentGitConfig(environment map[string]string) []gitConfigEntry {
	count := 0
	if text, ok := environment["GIT_CONFIG_COUNT"]; ok {
		delete(environment, "GIT_CONFIG_COUNT")
		parsed, err := strconv.ParseUint(text, 10, strconv.IntSize)
		if err != nil {
			slog.Warn("ignoring inherited Git configuration with an unparsable count",
				"count", text, "error", gitEnvParseIntError(text, err))
		} else {
			count = int(parsed)
		}
	}
	entries := make([]gitConfigEntry, 0, min(count, len(environment)))
	for index := range count {
		keyName := fmt.Sprintf("GIT_CONFIG_KEY_%d", index)
		valueName := fmt.Sprintf("GIT_CONFIG_VALUE_%d", index)
		key, hasKey := environment[keyName]
		delete(environment, keyName)
		value, hasValue := environment[valueName]
		delete(environment, valueName)
		if hasKey && hasValue {
			entries = append(entries, gitConfigEntry{key: key, value: value})
			continue
		}
		// An incomplete pair cannot be recovered, and the session is more
		// useful without it than refusing to start.
		slog.Warn("ignoring an incomplete inherited Git configuration entry",
			"key_name", keyName, "value_name", valueName)
	}
	for name := range environment {
		if isIndexedGitConfigName(name, "KEY") || isIndexedGitConfigName(name, "VALUE") {
			delete(environment, name)
		}
	}
	return entries
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
	section, rest, found := strings.Cut(key, ".")
	if !found {
		return "", "", fmt.Errorf("Git configuration key %s names no section", sessionConfigDebugString(key))
	}
	var subsection *string
	name = rest
	if index := strings.LastIndex(rest, "."); index >= 0 {
		value := rest[:index]
		subsection, name = &value, rest[index+1:]
	}
	for _, part := range [...]struct{ text, label string }{{section, "section"}, {name, "name"}} {
		usable := part.text != ""
		for _, character := range part.text {
			if !gitEnvIsASCIIAlphanumeric(character) && character != '-' {
				usable = false
				break
			}
		}
		if !usable {
			return "", "", fmt.Errorf("Git configuration key %s has an unusable %s", sessionConfigDebugString(key), part.label)
		}
	}
	if subsection == nil {
		return "[" + section + "]", name, nil
	}
	// A subsection name keeps its own quoting rules: only a backslash and a
	// double quote are escapes, so no other character may be encoded.
	for _, character := range *subsection {
		if gitEnvIsControl(character) {
			return "", "", fmt.Errorf("Git configuration key %s has a control character %s in its subsection",
				sessionConfigDebugString(key), gitEnvDebugChar(character))
		}
	}
	return "[" + section + " \"" + escapeGitConfigText(*subsection) + "\"]", name, nil
}

func quoteGitConfigValue(value string) (string, error) {
	var quoted strings.Builder
	quoted.Grow(len(value) + 2)
	quoted.WriteByte('"')
	for _, character := range value {
		switch {
		case character == '\\':
			quoted.WriteString(`\\`)
		case character == '"':
			quoted.WriteString(`\"`)
		case character == '\n':
			quoted.WriteString(`\n`)
		case character == '\t':
			quoted.WriteString(`\t`)
		case character == '\b':
			quoted.WriteString(`\b`)
		case gitEnvIsControl(character):
			// Git has no escape for any other control character.
			return "", fmt.Errorf("Git configuration value %s has a control character %s",
				sessionConfigDebugString(value), gitEnvDebugChar(character))
		default:
			quoted.WriteRune(character)
		}
	}
	quoted.WriteByte('"')
	return quoted.String(), nil
}

func escapeGitConfigText(text string) string {
	return strings.ReplaceAll(strings.ReplaceAll(text, `\`, `\\`), `"`, `\"`)
}

// gitEnvIsSymlink is whether path itself is a symbolic link (Rust:
// symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())).
func gitEnvIsSymlink(path string) bool {
	info, err := os.Lstat(path)
	return err == nil && info.Mode()&os.ModeSymlink != 0
}

// gitEnvAbsolute is Rust's std::path::absolute on Unix: a relative path is
// joined to the current directory, `.` components and repeated separators
// are dropped, and `..` components are kept, without touching the
// filesystem.
func gitEnvAbsolute(path string) (string, error) {
	if path == "" {
		return "", errors.New("cannot make an empty path absolute")
	}
	if !filepath.IsAbs(path) {
		cwd, err := os.Getwd()
		if err != nil {
			return "", err
		}
		path = cwd + string(filepath.Separator) + path
	}
	var parts []string
	for _, part := range strings.Split(path, string(filepath.Separator)) {
		if part != "" && part != "." {
			parts = append(parts, part)
		}
	}
	return string(filepath.Separator) + strings.Join(parts, string(filepath.Separator)), nil
}

func gitEnvIsASCIIAlphanumeric(character rune) bool {
	return character >= '0' && character <= '9' ||
		character >= 'a' && character <= 'z' ||
		character >= 'A' && character <= 'Z'
}

// gitEnvIsControl is Rust's char::is_control: the Unicode Cc category.
func gitEnvIsControl(character rune) bool {
	return unicode.Is(unicode.Cc, character)
}

// gitEnvDebugChar formats a character as Rust's `{:?}` does for a char.
func gitEnvDebugChar(character rune) string {
	switch character {
	case '\t':
		return `'\t'`
	case '\r':
		return `'\r'`
	case '\n':
		return `'\n'`
	case '\\':
		return `'\\'`
	case '\'':
		return `'\''`
	case 0:
		return `'\0'`
	}
	if !unicode.IsGraphic(character) || unicode.In(character, unicode.Mn, unicode.Me) {
		return fmt.Sprintf(`'\u{%x}'`, character)
	}
	return "'" + string(character) + "'"
}

// gitEnvParseIntError is the Display of Rust's ParseIntError for text that
// failed to parse as an unsigned integer.
func gitEnvParseIntError(text string, err error) string {
	if text == "" {
		return "cannot parse integer from empty string"
	}
	var numeric *strconv.NumError
	if errors.As(err, &numeric) && errors.Is(numeric.Err, strconv.ErrRange) {
		return "number too large to fit in target type"
	}
	return "invalid digit found in string"
}
