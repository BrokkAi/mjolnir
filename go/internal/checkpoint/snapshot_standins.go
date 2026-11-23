// Stand-in ports of the items snapshot.go needs from other mj-checkpoint
// modules at Rust 3dde0c11b: the new helpers in archive/git.rs
// (GitCommandRunner::run_to_file's default, synthetic_git_identity,
// synthetic_git_commit_identity, absolute_git_path, unquote_git_path,
// hidden_tracked_paths) and the snapshot inspection added to
// checkpoint/submodules.rs (inspect_snapshot_submodules,
// snapshot_gitlink_commits, inspect_snapshot_git_structure,
// snapshot_index_gitlinks and their helpers).
//
// The checkpoint-snapshot sync ported them here so its area compiles on its
// own. The checkpoint-rest sync ports the same items into archive_git.go and
// checkpoint_submodules.go. When both are merged, keep those ports, delete
// the duplicates from this file, and adjust snapshot.go if a Go name or
// signature differs.

package checkpoint

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ---- archive/git.rs ----

// gitFileRunner is a GitCommandRunner that writes a command's standard
// output directly to a file rather than buffering it in memory (an override
// of Rust's GitCommandRunner::run_to_file).
type gitFileRunner interface {
	RunToFile(repository string, command *GitCommand, output *os.File) (GitOutput, error)
}

// runToFile runs command with its stdout written to output. A runner without
// its own RunToFile runs the command and writes the captured output, as the
// Rust trait's default method does. The returned Stdout is empty.
func runToFile(runner GitCommandRunner, repository string, command *GitCommand, output *os.File) (GitOutput, error) {
	if fileRunner, ok := runner.(gitFileRunner); ok {
		return fileRunner.RunToFile(repository, command, output)
	}
	result, err := runner.Run(repository, command)
	if err != nil {
		return GitOutput{}, err
	}
	if _, err := output.Write(result.Stdout); err != nil {
		return GitOutput{}, err
	}
	return GitOutput{Status: result.Status, Stdout: []byte{}, Stderr: result.Stderr}, nil
}

// syntheticGitIdentity is a stable identity for internal commits and
// reflogs; it is never written to user config.
func syntheticGitIdentity() [][2]string {
	return [][2]string{
		{"GIT_AUTHOR_NAME", "Mjolnir"},
		{"GIT_AUTHOR_EMAIL", "mjolnir@localhost"},
		{"GIT_COMMITTER_NAME", "Mjolnir"},
		{"GIT_COMMITTER_EMAIL", "mjolnir@localhost"},
	}
}

// syntheticGitCommitIdentity gives commit objects a fixed date for stable
// IDs; reflogs must use current time because Git does not list epoch-zero
// stash reflog entries.
func syntheticGitCommitIdentity() [][2]string {
	return append(syntheticGitIdentity(),
		[2]string{"GIT_AUTHOR_DATE", "@0 +0000"},
		[2]string{"GIT_COMMITTER_DATE", "@0 +0000"},
	)
}

// absoluteGitPath resolves Git's path output without requiring
// --path-format (Git 2.31).
func absoluteGitPath(repository string, output []byte) (string, error) {
	output = bytes.TrimSuffix(output, []byte("\n"))
	path, err := core.FromGitBytes(output)
	if err != nil {
		return "", err
	}
	return snapshotAbsolute(gitJoin(repository, path))
}

// unquoteGitPath decodes Git C-quoted paths, including octal-escaped native
// bytes.
func unquoteGitPath(path []byte) (string, error) {
	quoted, ok := bytes.CutPrefix(path, []byte(`"`))
	if !ok {
		return core.FromGitBytes(path)
	}
	quoted, ok = bytes.CutSuffix(quoted, []byte(`"`))
	if !ok {
		return "", errors.New("unterminated quoted worktree path")
	}
	var decoded []byte
	for len(quoted) > 0 {
		character := quoted[0]
		quoted = quoted[1:]
		if character != '\\' {
			decoded = append(decoded, character)
			continue
		}
		if len(quoted) == 0 {
			return "", errors.New("incomplete worktree path escape")
		}
		escape := quoted[0]
		quoted = quoted[1:]
		switch {
		case escape == 'a':
			decoded = append(decoded, 7)
		case escape == 'b':
			decoded = append(decoded, 8)
		case escape == 't':
			decoded = append(decoded, '\t')
		case escape == 'n':
			decoded = append(decoded, '\n')
		case escape == 'v':
			decoded = append(decoded, 11)
		case escape == 'f':
			decoded = append(decoded, 12)
		case escape == 'r':
			decoded = append(decoded, '\r')
		case escape == '\\' || escape == '"':
			decoded = append(decoded, escape)
		case escape >= '0' && escape <= '7':
			if len(quoted) < 2 || quoted[0] < '0' || quoted[0] > '7' || quoted[1] < '0' || quoted[1] > '7' {
				return "", errors.New("invalid octal worktree path escape")
			}
			value := uint16(escape-'0')*64 + uint16(quoted[0]-'0')*8 + uint16(quoted[1]-'0')
			quoted = quoted[2:]
			if value > 0xff {
				return "", errors.New("worktree path octal escape exceeds one byte")
			}
			decoded = append(decoded, byte(value))
		default:
			return "", errors.New("invalid worktree path escape")
		}
	}
	return core.FromGitBytes(decoded)
}

// HiddenTrackedPaths lists tracked paths with the skip-worktree or
// assume-unchanged flag, whose edits Git does not report.
func HiddenTrackedPaths(runner GitCommandRunner, checkout string) ([]string, error) {
	listed, err := gitBytes(runner, checkout, []string{"ls-files", "-v", "-z"}, nil, "inspect hidden tracked files")
	if err != nil {
		return nil, err
	}
	paths := []string{}
	for _, entry := range bytes.Split(listed, []byte{0}) {
		if len(entry) == 0 || !(entry[0] == 'S' || (entry[0] >= 'a' && entry[0] <= 'z')) {
			continue
		}
		if len(entry) < 2 || entry[1] != ' ' {
			return nil, errors.New("malformed Git index flag entry")
		}
		path, err := core.FromGitBytes(entry[2:])
		if err != nil {
			return nil, err
		}
		paths = append(paths, path)
	}
	return paths, nil
}

// ---- checkpoint/submodules.rs (snapshot inspection) ----

type snapshotSubmoduleInspection struct {
	dirty                 []string
	unregistered          []string
	initialized           []SnapshotSubmodule
	indexGitlinks         []SnapshotGitlink
	untracked             []string
	hiddenTrackedEdits    []string
	intentToAdd           [][2]string
	remoteRefsStale       bool
	uninitializedOrigins  []uninitializedOrigin
	untrackedRegistration []string
}

func inspectSnapshotSubmodules(runner GitCommandRunner, repository string, originOverride *string, managed bool) (snapshotSubmoduleInspection, error) {
	var inspection snapshotSubmoduleInspection
	if err := inspectSnapshotCheckout(runner, repository, "", originOverride, managed, &inspection); err != nil {
		return snapshotSubmoduleInspection{}, err
	}
	return inspection, nil
}

func inspectSnapshotCheckout(runner GitCommandRunner, checkout string, prefix string, originOverride *string, managed bool, inspection *snapshotSubmoduleInspection) error {
	var indexed []SnapshotGitlink
	if prefix == "" {
		links, err := snapshotIndexGitlinks(runner, checkout)
		if err != nil {
			return err
		}
		indexed = links
	}
	paths, err := gitlinks(runner, checkout)
	if err != nil {
		return err
	}
	changed, err := submoduleGit(runner, checkout, []string{
		"diff", "--cached", "--ignore-submodules=none", "--raw", "--no-renames", "--no-ext-diff", "-z",
	})
	if err != nil {
		return err
	}
	fields := snapshotNonEmptyFields(changed, 0)
	for index := 0; index+1 < len(fields); index += 2 {
		if bytes.HasPrefix(fields[index], []byte(":160000 ")) {
			path, err := core.FromGitBytes(fields[index+1])
			if err != nil {
				return err
			}
			paths = append(paths, path)
		}
	}
	slices.SortStableFunc(paths, checkpointPathCompare)
	paths = slices.CompactFunc(paths, checkpointPathEqual)
	if len(paths) == 0 {
		return nil
	}
	config, err := snapshotRegisteredSubmodules(runner, checkout)
	if err != nil {
		return err
	}
	if managed && config.untrackedRegistration && slices.ContainsFunc(paths, config.registered) {
		inspection.untrackedRegistration = append(inspection.untrackedRegistration, gitJoin(prefix, ".gitmodules"))
	}
	for _, link := range indexed {
		link.Path = gitJoin(prefix, link.Path)
		inspection.indexGitlinks = append(inspection.indexGitlinks, link)
	}
	var uninitialized []string
	for _, path := range paths {
		if config.registered(path) && !checkpointExists(gitJoin(gitJoin(checkout, path), ".git")) {
			uninitialized = append(uninitialized, path)
		}
	}
	if err := inventoryUninitializedFiles(runner, checkout, prefix, uninitialized, &inspection.untracked); err != nil {
		return err
	}
	for _, path := range paths {
		named := gitJoin(prefix, path)
		key, ok := config.pathKey(path)
		if !ok {
			inspection.unregistered = append(inspection.unregistered, named)
			continue
		}
		directory := gitJoin(checkout, path)
		if !checkpointExists(gitJoin(directory, ".git")) {
			// Only survival of the configured local origin is audited. Native
			// submodule update later decides whether an uninitialized pin exists.
			if managed {
				keyText, err := gitDecodeUTF8(key)
				if err != nil {
					return err
				}
				stem, ok := strings.CutSuffix(keyText, ".path")
				if !ok {
					return errors.New("invalid submodule key")
				}
				urlKey := stem + ".url"
				if url, ok := config.urls[urlKey]; ok {
					resolved, err := resolveSubmoduleURL(runner, checkout, url, originOverride)
					if err != nil {
						return err
					}
					if err := func() error {
						neutral, err := os.MkdirTemp("", ".tmp")
						if err != nil {
							return err
						}
						defer os.RemoveAll(neutral)
						command := gitCommand([]string{"ls-remote", "--get-url", resolved})
						output, err := runner.Run(neutral, &command)
						if err != nil {
							return err
						}
						if err := ensureInspected(&output); err != nil {
							return err
						}
						effective, err := gitDecodeUTF8(bytes.TrimSuffix(output.Stdout, []byte("\n")))
						if err != nil {
							return err
						}
						route, ok, err := localOriginRoute(neutral, effective)
						if err != nil {
							return err
						}
						if ok {
							inspection.uninitializedOrigins = append(inspection.uninitializedOrigins, uninitializedOrigin{
								owner:     prefix,
								configKey: urlKey,
								route:     route,
							})
						}
						return nil
					}(); err != nil {
						return err
					}
				}
			}
			continue
		}
		headBytes, err := submoduleGit(runner, directory, []string{"rev-parse", "HEAD"})
		if err != nil {
			return err
		}
		headText, err := gitDecodeUTF8(headBytes)
		if err != nil {
			return err
		}
		head := strings.TrimSpace(headText)
		inspection.initialized = append(inspection.initialized, SnapshotSubmodule{Path: named, Commit: head})
		untracked, err := submoduleGit(runner, directory, []string{"ls-files", "--others", "--exclude-standard", "-z"})
		if err != nil {
			return err
		}
		for _, raw := range snapshotNonEmptyFields(untracked, 0) {
			file, err := core.FromGitBytes(raw)
			if err != nil {
				return err
			}
			inspection.untracked = append(inspection.untracked, gitJoin(named, file))
		}
		if managed {
			hidden, err := HiddenTrackedPaths(runner, directory)
			if err != nil {
				return err
			}
			for _, file := range hidden {
				inspection.hiddenTrackedEdits = append(inspection.hiddenTrackedEdits, gitJoin(named, file))
			}
			intent, err := intentToAddPaths(runner, directory)
			if err != nil {
				return err
			}
			for _, file := range intent {
				inspection.intentToAdd = append(inspection.intentToAdd, [2]string{named, file})
			}
			status, err := submoduleGit(runner, directory, []string{
				"status", "--porcelain", "--untracked-files=no", "--ignore-submodules=none",
			})
			if err != nil {
				return err
			}
			tips, err := snapshotGitlinkCommits(runner, checkout, path)
			if err != nil {
				return err
			}
			tips = append(tips, head)
			delta, stale, err := submoduleSelectionHasDelta(runner, directory, tips)
			if err != nil {
				return err
			}
			inspection.remoteRefsStale = inspection.remoteRefsStale || stale
			if len(status) != 0 || delta {
				inspection.dirty = append(inspection.dirty, named)
			}
		}
		if err := inspectSnapshotCheckout(runner, directory, named, nil, managed, inspection); err != nil {
			return err
		}
	}
	return nil
}

// snapshotGitlinkCommits are the objects needed from this submodule:
// committed and staged gitlinks. HEAD may have no entry for a newly added
// submodule; conflicted stages all count.
func snapshotGitlinkCommits(runner GitCommandRunner, checkout string, path string) ([]string, error) {
	commits := []string{}
	for _, query := range []struct {
		arguments []string
		oidIndex  int
	}{
		{[]string{"ls-tree", "-z", "HEAD", "--"}, 2},
		{[]string{"ls-files", "--stage", "-z", "--"}, 1},
	} {
		arguments := append([]string{"--literal-pathspecs"}, query.arguments...)
		arguments = append(arguments, path)
		output, err := runner.Run(checkout, &GitCommand{Arguments: arguments})
		if err != nil {
			return nil, err
		}
		if err := ensureInspected(&output); err != nil {
			return nil, err
		}
		for _, entry := range bytes.Split(output.Stdout, []byte{0}) {
			if !bytes.HasPrefix(entry, []byte("160000 ")) {
				continue
			}
			header, _, _ := bytes.Cut(entry, []byte("\t"))
			fields := bytes.Split(header, []byte(" "))
			if query.oidIndex >= len(fields) {
				return nil, errors.New("missing gitlink object")
			}
			oid, err := gitDecodeUTF8(fields[query.oidIndex])
			if err != nil {
				return nil, err
			}
			commits = append(commits, oid)
		}
	}
	return commits, nil
}

type snapshotModulesConfig struct {
	// paths maps each registered path to its `submodule.<name>.path` key,
	// sorted by path as a BTreeMap<PathBuf, _>.
	paths []snapshotModulePath
	urls  map[string]string
	// untrackedRegistration is a working-tree .gitmodules Git does not track.
	untrackedRegistration bool
}

type snapshotModulePath struct {
	path string
	key  []byte
}

func (c snapshotModulesConfig) pathKey(path string) ([]byte, bool) {
	for _, entry := range c.paths {
		if checkpointPathEqual(entry.path, path) {
			return entry.key, true
		}
	}
	return nil, false
}

func (c snapshotModulesConfig) registered(path string) bool {
	_, ok := c.pathKey(path)
	return ok
}

func snapshotRegisteredSubmodules(runner GitCommandRunner, checkout string) (snapshotModulesConfig, error) {
	unmerged, err := submoduleGit(runner, checkout, []string{"ls-files", "--unmerged", "--", ".gitmodules"})
	if err != nil {
		return snapshotModulesConfig{}, err
	}
	var source [2]string
	switch {
	case len(unmerged) != 0:
		// Conflict markers cannot be parsed; the operation policy needs the
		// committed registration before managed abort/raw committed-only cuts.
		committed, err := submoduleGit(runner, checkout, []string{"ls-tree", "HEAD", "--", ".gitmodules"})
		if err != nil {
			return snapshotModulesConfig{}, err
		}
		if len(committed) == 0 {
			return snapshotModulesConfig{}, nil
		}
		source = [2]string{"--blob", "HEAD:.gitmodules"}
	case checkpointExists(gitJoin(checkout, ".gitmodules")):
		source = [2]string{"--file", ".gitmodules"}
	default:
		// Match Git's registration fallback when working-tree deletion leaves
		// the index or HEAD version available, including staged deletions.
		indexed, err := submoduleGit(runner, checkout, []string{"ls-files", "--stage", "-z", "--", ".gitmodules"})
		if err != nil {
			return snapshotModulesConfig{}, err
		}
		if len(indexed) != 0 {
			source = [2]string{"--blob", ":.gitmodules"}
		} else {
			committed, err := submoduleGit(runner, checkout, []string{"ls-tree", "HEAD", "--", ".gitmodules"})
			if err != nil {
				return snapshotModulesConfig{}, err
			}
			if len(committed) == 0 {
				return snapshotModulesConfig{}, nil
			}
			source = [2]string{"--blob", "HEAD:.gitmodules"}
		}
	}
	command := gitCommand([]string{"config", source[0], source[1], "--null", "--get-regexp", `^submodule\..*\.(path|url)$`})
	output, err := runner.Run(checkout, &command)
	if err != nil {
		return snapshotModulesConfig{}, err
	}
	if output.Status == 1 {
		return snapshotModulesConfig{}, nil
	}
	if err := ensureInspected(&output); err != nil {
		return snapshotModulesConfig{}, err
	}
	config := snapshotModulesConfig{urls: map[string]string{}}
	for _, entry := range snapshotNonEmptyFields(output.Stdout, 0) {
		newline := bytes.IndexByte(entry, '\n')
		if newline < 0 {
			return snapshotModulesConfig{}, errors.New("invalid submodule config")
		}
		key, value := entry[:newline], entry[newline+1:]
		if bytes.HasSuffix(key, []byte(".path")) {
			path, err := core.FromGitBytes(value)
			if err != nil {
				return snapshotModulesConfig{}, err
			}
			// A BTreeMap insert replaces an earlier key for the same path.
			replaced := false
			for index := range config.paths {
				if checkpointPathEqual(config.paths[index].path, path) {
					config.paths[index].key = slices.Clone(key)
					replaced = true
				}
			}
			if !replaced {
				config.paths = append(config.paths, snapshotModulePath{path: path, key: slices.Clone(key)})
			}
		} else {
			keyText, err := gitDecodeUTF8(key)
			if err != nil {
				return snapshotModulesConfig{}, err
			}
			valueText, err := gitDecodeUTF8(value)
			if err != nil {
				return snapshotModulesConfig{}, err
			}
			config.urls[keyText] = valueText
		}
	}
	slices.SortStableFunc(config.paths, func(left, right snapshotModulePath) int {
		return checkpointPathCompare(left.path, right.path)
	})
	if source[0] == "--file" {
		tracked, err := runner.Run(checkout, new(gitCommand([]string{"ls-files", "--error-unmatch", "--", ".gitmodules"})))
		if err != nil {
			return snapshotModulesConfig{}, err
		}
		if tracked.Status != 0 && tracked.Status != 1 {
			return snapshotModulesConfig{}, fmt.Errorf("failed to inspect submodule registration: %s", checkpointLossy(tracked.Stderr))
		}
		config.untrackedRegistration = tracked.Status == 1
	}
	return config, nil
}

// inventoryUninitializedFiles lists the ordinary files that uninitialized
// gitlinks hide from Git. It inspects all nonempty directories through one
// private index, without copying an index for absent or empty paths. NUL
// stdin avoids argument limits and preserves native paths.
func inventoryUninitializedFiles(runner GitCommandRunner, checkout string, prefix string, paths []string, untracked *[]string) error {
	var directories []string
	for _, path := range paths {
		location := gitJoin(checkout, path)
		metadata, err := os.Lstat(location)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return fmt.Errorf("inspect uninitialized module directory: %w", err)
		}
		if !metadata.IsDir() {
			directories = append(directories, path)
			continue
		}
		entries, err := os.ReadDir(location)
		if err != nil {
			return err
		}
		if len(entries) > 0 {
			directories = append(directories, path)
		}
	}
	slices.SortStableFunc(directories, checkpointPathCompare)
	directories = slices.CompactFunc(directories, checkpointPathEqual)
	if len(directories) == 0 {
		return nil
	}
	index, err := privateIndex(runner, checkout)
	if err != nil {
		return err
	}
	defer index.remove()
	env := [][2]string{{"GIT_INDEX_FILE", index.path}}
	var stdin []byte
	for _, path := range directories {
		text, err := slashPath(path)
		if err != nil {
			return err
		}
		stdin = append(stdin, text...)
		stdin = append(stdin, 0)
	}
	removed, err := runner.Run(checkout, &GitCommand{
		Arguments: []string{"update-index", "--force-remove", "-z", "--stdin"},
		Env:       slices.Clone(env),
		Stdin:     stdin,
	})
	if err != nil {
		return err
	}
	if err := ensureInspected(&removed); err != nil {
		return err
	}
	files, err := runner.Run(checkout, &GitCommand{
		Arguments: []string{"ls-files", "--others", "--exclude-standard", "-z"},
		Env:       env,
	})
	if err != nil {
		return err
	}
	if err := ensureInspected(&files); err != nil {
		return err
	}
	for _, raw := range snapshotNonEmptyFields(files.Stdout, 0) {
		path, err := core.FromGitBytes(raw)
		if err != nil {
			return err
		}
		for ancestor, ok := path, true; ok; ancestor, ok = checkpointParent(ancestor) {
			if slices.ContainsFunc(directories, func(directory string) bool { return checkpointPathEqual(directory, ancestor) }) {
				*untracked = append(*untracked, gitJoin(prefix, path))
				break
			}
		}
	}
	return nil
}

type snapshotGitStructure struct {
	directories []string
	worktrees   []UnrepresentedWorktree
}

type structuralInspection struct {
	runner        GitCommandRunner
	handled       []string
	rootWorktrees []string
	seenAdmins    []string
	directories   []string
	worktrees     []UnrepresentedWorktree
	checkout      string
}

func snapshotPathSetContains(set []string, path string) bool {
	return slices.ContainsFunc(set, func(entry string) bool { return checkpointPathEqual(entry, path) })
}

// snapshotPathSetInsert adds path to a sorted path set, reporting whether
// it was new.
func snapshotPathSetInsert(set *[]string, path string) bool {
	index, found := slices.BinarySearchFunc(*set, path, checkpointPathCompare)
	if found {
		return false
	}
	*set = slices.Insert(*set, index, path)
	return true
}

func structuralEntries(path string) ([]fs.DirEntry, error) {
	entries, err := os.ReadDir(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("list structural Git state: %w", err)
	}
	// os.ReadDir sorts by file name, as the Rust code does.
	return entries, nil
}

func (s *structuralInspection) modules(path string) error {
	entries, err := structuralEntries(path)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		entryPath, err := checkpointCanonicalize(gitJoin(path, entry.Name()))
		if err != nil {
			return err
		}
		switch {
		case snapshotPathSetContains(s.handled, entryPath):
			if err := s.admin(entryPath, false); err != nil {
				return err
			}
		case slices.ContainsFunc(s.handled, func(handled string) bool { return checkpointPathStartsWith(handled, entryPath) }):
			if err := s.modules(entryPath); err != nil {
				return err
			}
		default:
			// Presence alone blocks deletion; never inspect refs/index/operations.
			snapshotPathSetInsert(&s.directories, entryPath)
			if err := s.admin(entryPath, false); err != nil {
				return err
			}
		}
	}
	return nil
}

func (s *structuralInspection) admin(path string, root bool) error {
	if !snapshotPathSetInsert(&s.seenAdmins, path) {
		return nil
	}
	if snapshotPathSetContains(s.handled, path) {
		presence, err := gitPresencePaths(s.runner, path)
		if err != nil {
			return err
		}
		for _, entry := range presence {
			snapshotPathSetInsert(&s.directories, entry)
		}
	}
	if err := s.modules(gitJoin(path, "modules")); err != nil {
		return err
	}
	entries, err := structuralEntries(gitJoin(path, "worktrees"))
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		admin, err := checkpointCanonicalize(gitJoin(gitJoin(path, "worktrees"), entry.Name()))
		if err != nil {
			return err
		}
		if root && snapshotPathSetContains(s.rootWorktrees, admin) {
			if err := s.admin(admin, true); err != nil {
				return err
			}
			continue
		}
		raw, err := os.ReadFile(gitJoin(admin, "gitdir"))
		switch {
		case err == nil:
			link, err := core.FromGitBytes(bytes.TrimSuffix(raw, []byte("\n")))
			if err != nil {
				return err
			}
			if !snapshotIsAbsolute(link) {
				link = gitJoin(admin, link)
			}
			checkout, ok := checkpointParent(link)
			if !ok {
				return errors.New("worktree registration has no checkout")
			}
			canMoveInside := root &&
				checkpointIsDir(checkout) &&
				!checkpointPathStartsWith(checkout, s.checkout) &&
				!checkpointExists(gitJoin(checkout, ".gitmodules")) &&
				!checkpointExists(gitJoin(admin, "modules"))
			s.worktrees = append(s.worktrees, UnrepresentedWorktree{
				GitDir:        path,
				Locked:        checkpointExists(gitJoin(admin, "locked")),
				Path:          checkout,
				CanMoveInside: canMoveInside,
			})
		case errors.Is(err, fs.ErrNotExist):
			snapshotPathSetInsert(&s.directories, admin)
		default:
			return fmt.Errorf("read worktree registration path: %w", err)
		}
		if err := s.modules(gitJoin(admin, "modules")); err != nil {
			return err
		}
	}
	return nil
}

func inspectSnapshotGitStructure(runner GitCommandRunner, checkout string, roots []string, initializedModules []string) (snapshotGitStructure, error) {
	checkout, err := checkpointCanonicalize(checkout)
	if err != nil {
		return snapshotGitStructure{}, err
	}
	gitDir, common, err := gitDirectories(runner, checkout)
	if err != nil {
		return snapshotGitStructure{}, err
	}
	if gitDir, err = checkpointCanonicalize(gitDir); err != nil {
		return snapshotGitStructure{}, err
	}
	if common, err = checkpointCanonicalize(common); err != nil {
		return snapshotGitStructure{}, err
	}
	inspection := structuralInspection{runner: runner, checkout: checkout}
	for _, path := range roots {
		privateDir, _, err := gitDirectories(runner, path)
		if err != nil {
			return snapshotGitStructure{}, err
		}
		admin, err := checkpointCanonicalize(privateDir)
		if err != nil {
			return snapshotGitStructure{}, err
		}
		snapshotPathSetInsert(&inspection.handled, admin)
		snapshotPathSetInsert(&inspection.rootWorktrees, admin)
	}
	for _, path := range initializedModules {
		privateDir, _, err := gitDirectories(runner, path)
		if err != nil {
			return snapshotGitStructure{}, err
		}
		admin, err := checkpointCanonicalize(privateDir)
		if err != nil {
			return snapshotGitStructure{}, err
		}
		snapshotPathSetInsert(&inspection.handled, admin)
	}
	// The private root admin is deleted even for a legacy linked checkout;
	// its shared common directory outside these physical roots is not
	// scanned.
	var admins []string
	for _, path := range inspection.handled {
		if checkpointPathStartsWith(path, checkout) || checkpointPathStartsWith(path, gitDir) {
			admins = append(admins, path)
		}
	}
	for _, admin := range admins {
		if err := inspection.admin(admin, checkpointPathEqual(admin, common)); err != nil {
			return snapshotGitStructure{}, err
		}
	}
	slices.SortStableFunc(inspection.worktrees, func(left, right UnrepresentedWorktree) int {
		if order := checkpointPathCompare(left.GitDir, right.GitDir); order != 0 {
			return order
		}
		return checkpointPathCompare(left.Path, right.Path)
	})
	inspection.worktrees = slices.Compact(inspection.worktrees)
	return snapshotGitStructure{
		directories: inspection.directories,
		worktrees:   inspection.worktrees,
	}, nil
}

func snapshotIndexGitlinks(runner GitCommandRunner, checkout string) ([]SnapshotGitlink, error) {
	output, err := runner.Run(checkout, &GitCommand{Arguments: []string{"ls-files", "--stage", "-z"}})
	if err != nil {
		return nil, err
	}
	if err := ensureInspected(&output); err != nil {
		return nil, err
	}
	gitlinks := []SnapshotGitlink{}
	for _, entry := range bytes.Split(output.Stdout, []byte{0}) {
		if !bytes.HasPrefix(entry, []byte("160000 ")) {
			continue
		}
		tab := bytes.IndexByte(entry, '\t')
		if tab < 0 {
			return nil, errors.New("missing gitlink path")
		}
		header, err := gitDecodeUTF8(entry[:tab])
		if err != nil {
			return nil, err
		}
		fields := strings.Fields(header)
		if len(fields) > 2 && fields[2] == "0" {
			path, err := core.FromGitBytes(entry[tab+1:])
			if err != nil {
				return nil, err
			}
			if len(fields) < 2 {
				return nil, errors.New("missing gitlink object")
			}
			gitlinks = append(gitlinks, SnapshotGitlink{Path: path, Commit: fields[1]})
		}
	}
	return gitlinks, nil
}
