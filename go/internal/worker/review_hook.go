// Port of mj-worker/src/review/hook.rs.
//
// Repository-local pre-push review gate.

package worker

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// marker is the line that identifies Mjolnir's gate in a pre-push hook.
const marker = "# mj-review-gate"

// script is the pre-push hook Mjolnir installs.
const script = `#!/bin/sh
# mj-review-gate
hooks_dir=$(dirname "$0")
tmp=$(mktemp "${TMPDIR:-/tmp}/mj-review-gate.XXXXXXXXXX") || exit 1
trap 'rm -f "$tmp"' 0
trap 'exit 1' HUP INT TERM
cat > "$tmp" || exit 1
if [ "${MJ_REVIEW_GATE:-}" = "1" ]; then
  while read -r local_ref local_sha remote_ref remote_sha; do
    case "$local_sha" in
      *[!0]*|'') ;;
      *) continue ;;
    esac
    tree=$(git rev-parse --verify "$local_sha^{tree}") || exit 1
    if ! git show-ref --verify --quiet "refs/mj/reviewed/$tree" && ! git show-ref --verify --quiet "refs/mj/overridden/$tree"; then
      echo "mj: $local_ref (tree $tree) has not been reviewed." >&2
      echo "mj: call the mj-review request_review tool for the worktree \"$(git rev-parse --show-toplevel)\", wait for the verdict message, then push again." >&2
      exit 1
    fi
  done < "$tmp"
fi
if [ -x "$hooks_dir/pre-push.before-mj" ]; then
  "$hooks_dir/pre-push.before-mj" "$@" < "$tmp"
  exit $?
fi
exit 0
`

// HookInstall is what installing the gate did. One of
// HookInstallInstalled, HookInstallRewritten,
// HookInstallRefusedExistingBackup, HookInstallRefusedWorkTreeHooksDirectory.
// Variants are comparable values.
type HookInstall interface {
	isHookInstall()
}

type HookInstallInstalled struct{}

type HookInstallRewritten struct{}

// HookInstallRefusedExistingBackup: Path is the backup that already holds a
// different hook.
type HookInstallRefusedExistingBackup struct {
	Path string
}

// HookInstallRefusedWorkTreeHooksDirectory: Path is the hooks directory,
// inside the work tree and not ignored.
type HookInstallRefusedWorkTreeHooksDirectory struct {
	Path string
}

func (HookInstallInstalled) isHookInstall()                     {}
func (HookInstallRewritten) isHookInstall()                     {}
func (HookInstallRefusedExistingBackup) isHookInstall()         {}
func (HookInstallRefusedWorkTreeHooksDirectory) isHookInstall() {}

// InstallPrePushHook installs the gate where Git runs hooks, preserving an
// existing user hook.
func InstallPrePushHook(runner checkpoint.GitCommandRunner, root string) (HookInstall, error) {
	configured, err := captureRun(runner, root, []string{"config", "--path", "--get", "core.hooksPath"})
	if err != nil {
		return nil, err
	}
	var directoryOutput []byte
	switch configured.Status {
	case 0:
		directoryOutput = configured.Stdout
	case 1:
		directoryOutput, err = checkedGit(runner, root, []string{"rev-parse", "--path-format=absolute", "--git-path", "hooks"})
		if err != nil {
			return nil, err
		}
	default:
		return nil, fmt.Errorf("read core.hooksPath: %s", captureLossy(configured.Stderr))
	}
	workTree, err := hookGitOutputPath(runner, root, []string{"rev-parse", "--show-toplevel"})
	if err != nil {
		return nil, err
	}
	directoryText, err := captureFromUTF8(directoryOutput)
	if err != nil {
		return nil, err
	}
	directory, err := core.ResolveGitPath(workTree, directoryText)
	if err != nil {
		return nil, err
	}
	if directory, err = canonicalHooksDirectory(directory); err != nil {
		return nil, err
	}
	common, err := hookGitOutputPath(runner, root, []string{"rev-parse", "--path-format=absolute", "--git-common-dir"})
	if err != nil {
		return nil, err
	}
	hook := filepath.Join(directory, "pre-push")
	if capturePathStartsWith(directory, workTree) && !capturePathStartsWith(directory, common) {
		if !utf8.ValidString(hook) {
			return nil, errors.New("decode the hook path")
		}
		ignored, err := captureRun(runner, root, []string{"check-ignore", "-q", "--", hook})
		if err != nil {
			return nil, err
		}
		switch ignored.Status {
		case 0:
		case 1:
			return HookInstallRefusedWorkTreeHooksDirectory{Path: directory}, nil
		default:
			return nil, fmt.Errorf("check whether the hook is ignored: %s", captureLossy(ignored.Stderr))
		}
	}
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return nil, fmt.Errorf("create %s: %w", directory, err)
	}
	backup := filepath.Join(directory, "pre-push.before-mj")
	var outcome HookInstall
	contents, err := os.ReadFile(hook)
	switch {
	case err == nil && slices.ContainsFunc(bytes.Split(contents, []byte("\n")), func(line []byte) bool {
		return string(line) == marker
	}):
		outcome = HookInstallRewritten{}
	case err == nil:
		// Hard-linking reserves the backup name without overwriting a
		// concurrently created file. Removing the old name completes the move.
		switch linked := os.Link(hook, backup); {
		case linked == nil:
			if err := os.Remove(hook); err != nil {
				return nil, fmt.Errorf("move the previous pre-push hook: %w", err)
			}
		case errors.Is(linked, fs.ErrExist):
			original, err := os.Lstat(hook)
			if err != nil {
				return nil, err
			}
			saved, err := os.Lstat(backup)
			if err != nil {
				return nil, err
			}
			if !os.SameFile(original, saved) {
				return HookInstallRefusedExistingBackup{Path: backup}, nil
			}
			if err := os.Remove(hook); err != nil {
				return nil, fmt.Errorf("complete the previous hook backup: %w", err)
			}
		default:
			return nil, fmt.Errorf("preserve the previous pre-push hook: %w", linked)
		}
		outcome = HookInstallInstalled{}
	case errors.Is(err, fs.ErrNotExist):
		outcome = HookInstallInstalled{}
	default:
		return nil, fmt.Errorf("read the previous pre-push hook: %w", err)
	}
	if err := writeHookScript(directory, hook); err != nil {
		return nil, err
	}
	return outcome, nil
}

// hookGitOutputPath is the one path a Git command prints, in canonical form.
func hookGitOutputPath(runner checkpoint.GitCommandRunner, root string, args []string) (string, error) {
	output, err := checkedGit(runner, root, args)
	if err != nil {
		return "", err
	}
	text, err := captureFromUTF8(output)
	if err != nil {
		return "", err
	}
	return core.CanonicalGitOutputPath(strings.TrimRight(text, "\n"))
}

// writeHookScript sets the executable mode before the atomic rename makes
// the script visible at hook. The temporary file is removed on every
// failure, as Rust's NamedTempFile is when it is dropped.
func writeHookScript(directory, hook string) error {
	temporary, err := os.CreateTemp(directory, ".tmp")
	if err != nil {
		return err
	}
	persisted := false
	defer func() {
		if !persisted {
			_ = temporary.Close()
			_ = os.Remove(temporary.Name())
		}
	}()
	if _, err := temporary.WriteString(script); err != nil {
		return err
	}
	if err := temporary.Chmod(0o755); err != nil {
		return err
	}
	if err := temporary.Sync(); err != nil {
		return err
	}
	if err := temporary.Close(); err != nil {
		return err
	}
	if err := os.Rename(temporary.Name(), hook); err != nil {
		return fmt.Errorf("install the pre-push review gate: %w", err)
	}
	persisted = true
	return nil
}

// canonicalHooksDirectory resolves existing symlink ancestors without
// creating an untracked directory.
func canonicalHooksDirectory(directory string) (string, error) {
	ancestor := filepath.Clean(directory)
	var missing []string
	for {
		resolved, err := hookCanonicalize(ancestor)
		if err == nil {
			for index := len(missing) - 1; index >= 0; index-- {
				resolved = filepath.Join(resolved, missing[index])
			}
			return resolved, nil
		}
		if !errors.Is(err, fs.ErrNotExist) {
			return "", fmt.Errorf("resolve the hooks directory: %w", err)
		}
		// Rust's Path::file_name and Path::parent: neither exists for a
		// root, and a final `..` has no file name.
		parent, name := filepath.Dir(ancestor), filepath.Base(ancestor)
		if parent == ancestor || name == ".." {
			return "", errors.New("locate the hooks directory")
		}
		missing = append(missing, name)
		ancestor = parent
	}
}

// hookCanonicalize is Rust's fs::canonicalize: the absolute path with every
// symbolic link resolved, failing when the path does not exist.
func hookCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}
