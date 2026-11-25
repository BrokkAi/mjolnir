// Port of mj-import/src/import/edit_targets.rs.

package mjimport

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SessionEditTargetsFor is Rust's `session_edit_targets` (renamed: the type
// SessionEditTargets keeps its name): the Git roots and other directories a
// native session edited, outside its profile home.
func SessionEditTargetsFor(transcript *ClaudeTranscript, profileHome string) (SessionEditTargets, error) {
	return sessionEditTargetsWithScratchPrefixes(transcript, profileHome, scratchPrefixes())
}

// scratchPrefixes are directories whose repositories are throwaway
// workspaces rather than projects. A session that writes into one of them is
// still anchored on its own repository.
func scratchPrefixes() []string {
	prefixes := []string{}
	remember := func(path string) {
		path = importCanonicalizeOr(path)
		if !slices.ContainsFunc(prefixes, func(prefix string) bool { return importComparePaths(prefix, path) == 0 }) {
			prefixes = append(prefixes, path)
		}
	}
	remember(os.TempDir())
	for _, literal := range []string{"/tmp", "/var/tmp", "/dev/shm"} {
		remember(literal)
	}
	return prefixes
}

func sessionEditTargetsWithScratchPrefixes(
	transcript *ClaudeTranscript,
	profileHome string,
	scratchPrefixes []string,
) (SessionEditTargets, error) {
	profileHome = importCanonicalizeOr(profileHome)
	paths := []string{}
	for _, path := range transcript.EditedPaths {
		if !filepath.IsAbs(path) {
			path = importJoin(transcript.Cwd, path)
		}
		if comparable := canonicalizeExistingAncestor(path); !importPathStartsWith(comparable, profileHome) {
			paths = append(paths, path)
		}
	}
	if len(paths) == 0 {
		paths = append(paths, transcript.Cwd)
	}

	cwdRoot, ok, err := GitRootForPath(transcript.Cwd)
	if err != nil {
		return SessionEditTargets{}, err
	}
	if !ok {
		return SessionEditTargets{}, fmt.Errorf("session cwd is not in a usable Git worktree: %s", transcript.Cwd)
	}
	// The session's own repository is authoritative even when it lives under
	// a temporary directory.
	gitRoots := importPathSet{}
	gitRoots.insert(cwdRoot)
	scratchGitRoots := importPathSet{}
	nonGitDirs := importPathSet{}
	for _, path := range paths {
		root, ok, err := GitRootForPath(path)
		if err != nil {
			return SessionEditTargets{}, err
		}
		switch {
		case !ok:
			nonGitDirs.insert(editedDirectory(path))
		case importComparePaths(root, cwdRoot) != 0 && isScratchRoot(root, scratchPrefixes):
			scratchGitRoots.insert(root)
		default:
			gitRoots.insert(root)
		}
	}
	return SessionEditTargets{
		GitRoots:        gitRoots.paths(),
		ScratchGitRoots: scratchGitRoots.paths(),
		NonGitDirs:      nonGitDirs.paths(),
	}, nil
}

// importPathSet is Rust's `BTreeSet<PathBuf>`: paths in component order,
// each once.
type importPathSet struct {
	sorted []string
}

func (s *importPathSet) insert(path string) bool {
	index, found := slices.BinarySearchFunc(s.sorted, path, importComparePaths)
	if found {
		return false
	}
	s.sorted = slices.Insert(s.sorted, index, path)
	return true
}

// paths is the set in order, never nil.
func (s *importPathSet) paths() []string {
	return append([]string{}, s.sorted...)
}

// importJoin is Rust's `Path::join`: path pushed onto base without cleaning
// either, so a path reads in messages, and resolves, as Rust has it.
func importJoin(base string, path string) string {
	switch {
	case filepath.IsAbs(path):
		return path
	case base != "" && !os.IsPathSeparator(base[len(base)-1]):
		return base + string(filepath.Separator) + path
	default:
		return base + path
	}
}

func isScratchRoot(root string, scratchPrefixes []string) bool {
	for _, prefix := range scratchPrefixes {
		if importPathStartsWith(root, prefix) {
			return true
		}
	}
	return false
}

// canonicalizeExistingAncestor canonicalizes the nearest existing ancestor of
// path and appends the rest of path to it; path itself when no ancestor can
// be canonicalized.
func canonicalizeExistingAncestor(path string) string {
	existing := path
	suffix := []string{}
	for {
		if canonical, err := importCanonicalize(existing); err == nil {
			for index := len(suffix) - 1; index >= 0; index-- {
				canonical = importJoin(canonical, suffix[index])
			}
			return canonical
		}
		name, ok := importPathFileName(existing)
		if !ok {
			return path
		}
		suffix = append(suffix, name)
		parent, ok := importPathParent(existing)
		if !ok {
			return path
		}
		existing = parent
	}
}

// editedDirectory is path when it is a directory, otherwise its parent.
func editedDirectory(path string) string {
	if importIsDir(path) {
		return path
	}
	if parent, ok := importPathParent(path); ok {
		return parent
	}
	return path
}

// GitRootForPath finds the worktree containing a path, or its nearest
// existing ancestor. false when that directory is in no Git repository.
func GitRootForPath(path string) (string, bool, error) {
	probe := editedDirectory(path)
	for !importIsDir(probe) {
		parent, ok := importPathParent(probe)
		if !ok {
			return "", false, nil
		}
		probe = parent
	}
	command := exec.Command("git", "rev-parse", "--show-toplevel")
	command.Env = append(os.Environ(), "LC_ALL=C")
	command.Dir = probe
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		return "", false, fmt.Errorf("start git in %s: %w", probe, err)
	}
	if !output.Status.Success() {
		stderr := strings.ToValidUTF8(string(output.Stderr), "�")
		if strings.Contains(stderr, "not a git repository") {
			return "", false, nil
		}
		return "", false, fmt.Errorf("inspect Git repository in %s: %s", probe, strings.TrimSpace(stderr))
	}
	if !utf8.Valid(output.Stdout) {
		return "", false, fmt.Errorf("decode Git repository root: invalid utf-8 sequence")
	}
	root := strings.TrimSpace(string(output.Stdout))
	return importCanonicalizeOr(root), true, nil
}
