// Port of mj-worker/src/review/hook.rs.
//
// Repository-local pre-push review gate.

package worker

import (
	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
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
      echo "mj: call the mj-review request_review tool with path \"$(git rev-parse --show-toplevel)\", wait for the verdict message, then push again." >&2
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
	panic("unported: mj-worker/src/review/hook.rs install_pre_push_hook")
}

// canonicalHooksDirectory resolves existing symlink ancestors without
// creating an untracked directory.
func canonicalHooksDirectory(directory string) (string, error) {
	panic("unported: mj-worker/src/review/hook.rs canonical_hooks_directory")
}
