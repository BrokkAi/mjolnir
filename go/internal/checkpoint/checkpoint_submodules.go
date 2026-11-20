// Port of mj-checkpoint/src/checkpoint/submodules.rs.
//
// Submodule rules for a repository snapshot.
//
// A snapshot records each gitlink as the commit the superproject points to,
// never the files in the submodule's own working tree.

package checkpoint

import (
	"bytes"
	"fmt"
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SubmoduleInspection is what a checkout's gitlinks hold that its snapshot
// does not carry. Paths are relative to the top level of the inspected
// checkout.
type SubmoduleInspection struct {
	// Dirty are checked-out submodules, registered in `.gitmodules`, whose
	// working tree has changes. A snapshot would lose those changes.
	Dirty []string
	// Unregistered are gitlinks with no `.gitmodules` entry, which is what a
	// nested repository committed without `git submodule add` leaves. A
	// snapshot carries only the commit each one points to.
	Unregistered []string
}

// DirtySummary is "submodule <path> has uncommitted changes", or false when
// every checked-out submodule is clean.
func (i *SubmoduleInspection) DirtySummary() (string, bool) {
	switch len(i.Dirty) {
	case 0:
		return "", false
	case 1:
		return fmt.Sprintf("submodule %s has uncommitted changes", i.Dirty[0]), true
	}
	return fmt.Sprintf("submodules %s have uncommitted changes", strings.Join(i.Dirty, ", ")), true
}

// InspectSubmodules inspects every gitlink in the checkout that contains
// repository.
//
// The gitlinks come from the index, not from `git submodule foreach`, which
// fails outright when a gitlink has no URL in `.gitmodules`. A session's
// project directory can be a subdirectory of its checkout, so the inspection
// starts at the checkout's top level to see every gitlink.
func InspectSubmodules(runner GitCommandRunner, repository string) (SubmoduleInspection, error) {
	output, err := submoduleGit(runner, repository, []string{"rev-parse", "--show-toplevel"})
	if err != nil {
		return SubmoduleInspection{}, err
	}
	topLevel, err := core.FromGitBytes(bytes.TrimSuffix(output, []byte("\n")))
	if err != nil {
		return SubmoduleInspection{}, err
	}
	var inspection SubmoduleInspection
	if err := inspectCheckout(runner, topLevel, "", &inspection); err != nil {
		return SubmoduleInspection{}, err
	}
	return inspection, nil
}

// rejectDirtySubmodules refuses a repository with uncommitted submodule
// changes, which the snapshot would lose. A gitlink with no `.gitmodules`
// entry never blocks the checkpoint, but its files are not in the snapshot,
// so it is named.
func rejectDirtySubmodules(runner GitCommandRunner, repository string) error {
	inspection, err := InspectSubmodules(runner, repository)
	if err != nil {
		return err
	}
	for _, gitlink := range inspection.Unregistered {
		// This runs on the target, where standard error is the only log.
		fmt.Fprintf(os.Stderr, "gitlink %s has no entry in .gitmodules; the checkpoint records the commit it points to, not its files\n", gitlink)
	}
	if dirty, ok := inspection.DirtySummary(); ok {
		return fmt.Errorf("%s; commit or stash them, then try again", dirty)
	}
	return nil
}

// inspectCheckout inspects the gitlinks of one checkout, then of each
// checked-out submodule in turn. prefix is the checkout's path from the
// outermost top level.
func inspectCheckout(runner GitCommandRunner, checkout string, prefix string, inspection *SubmoduleInspection) error {
	panic("unported: mj-checkpoint/src/checkpoint/submodules.rs inspect_checkout")
}

// gitlinks are the gitlink paths in the checkout's index, sorted and
// deduplicated as a Rust BTreeSet<PathBuf>. A conflicted gitlink has one
// entry per stage, so the paths are collected as a set.
func gitlinks(runner GitCommandRunner, checkout string) ([]string, error) {
	panic("unported: mj-checkpoint/src/checkpoint/submodules.rs gitlinks")
}

// registeredSubmodulePaths are the submodule paths that `.gitmodules`
// registers. Git's own submodule commands read the file from the working
// tree, and so does this.
func registeredSubmodulePaths(runner GitCommandRunner, checkout string) (map[string]struct{}, error) {
	panic("unported: mj-checkpoint/src/checkpoint/submodules.rs registered_submodule_paths")
}

func submoduleGit(runner GitCommandRunner, directory string, arguments []string) ([]byte, error) {
	panic("unported: mj-checkpoint/src/checkpoint/submodules.rs submodule_git")
}

func gitCommand(arguments []string) GitCommand {
	return GitCommand{Arguments: append([]string(nil), arguments...)}
}

func ensureInspected(output *GitOutput) error {
	if output.Status != 0 {
		return fmt.Errorf("failed to inspect submodules: %s", strings.TrimSpace(checkpointLossy(output.Stderr)))
	}
	return nil
}
