// Port of mj-checkpoint/src/checkpoint/submodules.rs.
//
// Submodule rules for a repository snapshot.
//
// A snapshot records each gitlink as the commit the superproject points to,
// never the files in the submodule's own working tree.

package checkpoint

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"slices"
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
	links, err := gitlinks(runner, checkout)
	if err != nil {
		return err
	}
	if len(links) == 0 {
		return nil
	}
	registered, err := registeredSubmodulePaths(runner, checkout)
	if err != nil {
		return err
	}
	for _, gitlink := range links {
		named := core.Join(prefix, gitlink)
		if _, ok := registered[checkpointNormalize(gitlink)]; !ok {
			inspection.Unregistered = append(inspection.Unregistered, named)
			continue
		}
		submodule := core.Join(checkout, gitlink)
		// A submodule that was never checked out has no work to lose.
		if !checkpointExists(core.Join(submodule, ".git")) {
			continue
		}
		status, err := submoduleGit(runner, submodule, []string{"status", "--porcelain"})
		if err != nil {
			return err
		}
		if len(bytes.TrimLeft(status, " \t\n\f\r")) != 0 {
			inspection.Dirty = append(inspection.Dirty, named)
		}
		if err := inspectCheckout(runner, submodule, named, inspection); err != nil {
			return err
		}
	}
	return nil
}

// gitlinks are the gitlink paths in the checkout's index, sorted and
// deduplicated as a Rust BTreeSet<PathBuf>. A conflicted gitlink has one
// entry per stage, so the paths are collected as a set.
func gitlinks(runner GitCommandRunner, checkout string) ([]string, error) {
	listed, err := submoduleGit(runner, checkout, []string{"ls-files", "--stage", "-z"})
	if err != nil {
		return nil, err
	}
	var links []string
	for _, entry := range bytes.Split(listed, []byte{0}) {
		if len(entry) == 0 {
			continue
		}
		// Each entry is `<mode> <object> <stage>\t<path>`.
		tab := bytes.IndexByte(entry, '\t')
		if tab < 0 {
			return nil, errors.New("failed to inspect submodules: git ls-files printed an entry with no path")
		}
		if bytes.HasPrefix(entry, []byte("160000 ")) {
			path, err := core.FromGitBytes(entry[tab+1:])
			if err != nil {
				return nil, err
			}
			links = append(links, path)
		}
	}
	slices.SortStableFunc(links, checkpointPathCompare)
	return slices.CompactFunc(links, checkpointPathEqual), nil
}

// registeredSubmodulePaths are the submodule paths that `.gitmodules`
// registers, keyed by normalized path. Git's own submodule commands read the
// file from the working tree, and so does this.
func registeredSubmodulePaths(runner GitCommandRunner, checkout string) (map[string]struct{}, error) {
	registered := map[string]struct{}{}
	if !checkpointExists(core.Join(checkout, ".gitmodules")) {
		return registered, nil
	}
	command := gitCommand([]string{
		"config",
		"--file",
		".gitmodules",
		"--null",
		"--get-regexp",
		`^submodule\..*\.path$`,
	})
	output, err := runner.Run(checkout, &command)
	if err != nil {
		return nil, err
	}
	// `git config` exits 1 when no key matches.
	if output.Status == 1 {
		return registered, nil
	}
	if err := ensureInspected(&output); err != nil {
		return nil, err
	}
	// With `--null`, each entry is the key, a newline, the value, and a NUL.
	for _, entry := range bytes.Split(output.Stdout, []byte{0}) {
		if len(entry) == 0 {
			continue
		}
		newline := bytes.IndexByte(entry, '\n')
		if newline < 0 {
			return nil, errors.New("failed to inspect submodules: git config printed a key with no value")
		}
		path, err := core.FromGitBytes(entry[newline+1:])
		if err != nil {
			return nil, err
		}
		registered[checkpointNormalize(path)] = struct{}{}
	}
	return registered, nil
}

func submoduleGit(runner GitCommandRunner, directory string, arguments []string) ([]byte, error) {
	command := gitCommand(arguments)
	output, err := runner.Run(directory, &command)
	if err != nil {
		return nil, err
	}
	if err := ensureInspected(&output); err != nil {
		return nil, err
	}
	return output.Stdout, nil
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
