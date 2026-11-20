// Port of mj-core/src/local_git.rs.

// Controller-side support for repositories configured with `local` sources.

package core

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

// SessionGitStatus is what a session's checkout looks like right now: the
// branch, how far it is from its upstream, and the files that differ from
// HEAD. Produced by the controller from the target's own `git`, consumed by
// the session rows and the changed-files overlay.
type SessionGitStatus struct {
	Checkout string `json:"checkout"`
	// The branch name, `detached HEAD`, or a short reason there is none.
	Branch string `json:"branch"`
	// Commits ahead of and behind the upstream, when there is one.
	Ahead   *uint32       `json:"ahead"`
	Behind  *uint32       `json:"behind"`
	Changed []ChangedFile `json:"changed"`
}

// MarshalJSON writes no changed files as `[]`, as serde writes an empty Vec.
func (s SessionGitStatus) MarshalJSON() ([]byte, error) {
	type wire SessionGitStatus
	value := wire(s)
	if value.Changed == nil {
		value.Changed = []ChangedFile{}
	}
	return marshalSerdeJSON(value)
}

func (s *SessionGitStatus) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "checkout", "branch", "changed"); err != nil {
		return err
	}
	type wire SessionGitStatus
	return json.Unmarshal(data, (*wire)(s))
}

// RowText is the compact form a session row carries beside its target:
// `⎇ main ↑1 ↓2 ±4`. Empty when the checkout is not a repository.
func (s *SessionGitStatus) RowText() string { return s.RowTextWith(false) }

// RowTextWith is RowText with plain ASCII markers when `ascii` is set:
// `br main +1 -2 ~4`.
func (s *SessionGitStatus) RowTextWith(ascii bool) string {
	if s.Branch == "" || strings.HasPrefix(s.Branch, "not a git") || strings.HasPrefix(s.Branch, "unavailable") {
		return ""
	}
	branch, ahead, behind, changed := "⎇", "↑", "↓", "±"
	if ascii {
		branch, ahead, behind, changed = "br", "+", "-", "~"
	}
	text := branch + " " + s.Branch
	if s.Ahead != nil && *s.Ahead > 0 {
		text += fmt.Sprintf(" %s%d", ahead, *s.Ahead)
	}
	if s.Behind != nil && *s.Behind > 0 {
		text += fmt.Sprintf(" %s%d", behind, *s.Behind)
	}
	if len(s.Changed) > 0 {
		text += fmt.Sprintf(" %s%d", changed, len(s.Changed))
	}
	return text
}

// Totals is the lines added and removed across every changed file, from what
// git could count (binary files count nothing).
func (s *SessionGitStatus) Totals() (uint32, uint32) {
	var added, removed uint32
	for _, file := range s.Changed {
		if file.Added != nil {
			added += *file.Added
		}
		if file.Removed != nil {
			removed += *file.Removed
		}
	}
	return added, removed
}

// ChangedFile is one file that differs from HEAD, as `git status --porcelain`
// names it.
type ChangedFile struct {
	Path string `json:"path"`
	// The two-letter porcelain code: `M ` modified in the index, ` M` in the
	// worktree, `A ` added, `D ` deleted, `R ` renamed, `??` untracked.
	Status string `json:"status"`
	// Lines added and removed against HEAD; nil for a binary file or one git
	// did not count (an untracked file).
	Added   *uint32 `json:"added"`
	Removed *uint32 `json:"removed"`
}

func (c *ChangedFile) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "path", "status"); err != nil {
		return err
	}
	type wire ChangedFile
	return json.Unmarshal(data, (*wire)(c))
}

// Kind is one word for the change, for a list a person reads.
func (c *ChangedFile) Kind() string {
	code := c.Status
	switch {
	case code == "??":
		return "new"
	case strings.HasPrefix(code, "A"):
		return "added"
	case strings.HasPrefix(code, "D") || strings.HasSuffix(code, "D"):
		return "deleted"
	case strings.HasPrefix(code, "R"):
		return "renamed"
	case strings.HasPrefix(code, "C"):
		return "copied"
	case strings.Contains(code, "U"):
		return "conflict"
	}
	return "modified"
}

// NumstatArgs and StatusArgs are the git arguments whose output
// ParseGitStatus reads. Both are `-z` so git prints names as they are, not
// C-quoted with octal escapes.
var NumstatArgs = [5]string{"--no-optional-locks", "diff", "--numstat", "-z", "HEAD"}
var StatusArgs = [5]string{
	"--no-optional-locks",
	"status",
	"--porcelain",
	"-z",
	"--untracked-files=normal",
}

// ParseGitStatus builds a SessionGitStatus from the raw output of four git
// commands: `rev-parse --abbrev-ref HEAD`, `rev-list --left-right --count
// @{upstream}...HEAD` (or nil when there is no upstream), `diff --numstat -z
// HEAD`, and `status --porcelain -z`. Pure, so the parsing has tests that
// need no repository.
func ParseGitStatus(checkout string, branch string, aheadBehind *string, numstat string, porcelain string) SessionGitStatus {
	var ahead, behind *uint32
	if aheadBehind != nil {
		parts := strings.Fields(*aheadBehind)
		if len(parts) >= 2 {
			left, leftOK := parseUint32(parts[0])
			right, rightOK := parseUint32(parts[1])
			if leftOK && rightOK {
				behind, ahead = &left, &right
			}
		}
	}
	type counts struct{ added, removed *uint32 }
	optional := func(text string) *uint32 {
		if value, ok := parseUint32(text); ok {
			return &value
		}
		return nil
	}
	// Both listings are NUL-separated (`-z`), so git does not quote names.
	numstats := map[string]counts{}
	records := nonEmptyRecords(numstat)
	for i := 0; i < len(records); i++ {
		parts := strings.SplitN(records[i], "\t", 3)
		if len(parts) < 3 {
			continue
		}
		path := parts[2]
		// A rename has an empty path here, followed by the old and new names.
		if path == "" {
			i++ // the old name
			path = ""
			if i+1 < len(records) {
				i++
				path = records[i]
			} else {
				i = len(records)
			}
		}
		numstats[path] = counts{optional(parts[0]), optional(parts[1])}
	}
	changed := []ChangedFile{}
	records = nonEmptyRecords(porcelain)
	for i := 0; i < len(records); i++ {
		record := records[i]
		if len(record) < 4 || !isUTF8Boundary(record[3]) {
			continue
		}
		status := record[:2]
		// With `-z`, a rename or copy lists the new name, then the old one.
		if strings.ContainsAny(status, "RC") {
			i++
		}
		path := record[3:]
		count := numstats[path]
		changed = append(changed, ChangedFile{Path: path, Status: status, Added: count.added, Removed: count.removed})
	}
	return SessionGitStatus{
		Checkout: checkout,
		Branch:   strings.TrimSpace(branch),
		Ahead:    ahead,
		Behind:   behind,
		Changed:  changed,
	}
}

// nonEmptyRecords is the NUL-separated records of a `-z` listing.
func nonEmptyRecords(text string) []string {
	var records []string
	for _, record := range strings.Split(text, "\x00") {
		if record != "" {
			records = append(records, record)
		}
	}
	return records
}

// parseUint32 is Rust's `str::parse::<u32>`.
func parseUint32(text string) (uint32, bool) {
	text = strings.TrimPrefix(text, "+")
	if text == "" || text[0] == '+' {
		return 0, false
	}
	value, err := strconv.ParseUint(text, 10, 32)
	if err != nil {
		return 0, false
	}
	return uint32(value), true
}

type DirtyLocalRepository struct {
	ID      string
	Path    string
	Summary string
}

func DirtyLocalRepositories(bundle *ProjectBundle) ([]DirtyLocalRepository, error) {
	var dirty []DirtyLocalRepository
	for _, repository := range bundle.Repositories {
		if repository.Local == nil {
			continue
		}
		path := *repository.Local
		summary, err := localStatus(path)
		if err != nil {
			return nil, fmt.Errorf("inspect local repository %s: %w", debugString(repository.ID), err)
		}
		if summary != nil {
			dirty = append(dirty, DirtyLocalRepository{ID: repository.ID, Path: path, Summary: *summary})
		}
	}
	return dirty, nil
}

// runGit runs `git args...` in `directory` on this machine, capturing its
// output as Rust's `Command::output` does.
func runGit(directory string, args ...string) (ProcessOutput, error) {
	command := exec.Command("git", args...)
	command.Dir = directory
	return Capture(context.Background(), command)
}

// fromUTF8Lossy is Rust's `String::from_utf8_lossy`.
func fromUTF8Lossy(bytes []byte) string {
	if utf8.Valid(bytes) {
		return string(bytes)
	}
	var text strings.Builder
	for len(bytes) > 0 {
		r, size := utf8.DecodeRune(bytes)
		if r == utf8.RuneError && size == 1 {
			text.WriteRune(utf8.RuneError)
		} else {
			text.Write(bytes[:size])
		}
		bytes = bytes[size:]
	}
	return text.String()
}

// decodeUTF8 is Rust's `String::from_utf8`, with the context Rust adds.
func decodeUTF8(bytes []byte, context string) (string, error) {
	if !utf8.Valid(bytes) {
		return "", fmt.Errorf("%s: invalid utf-8 sequence", context)
	}
	return string(bytes), nil
}

func CanonicalRepository(path string) (string, error) {
	output, err := runGit(path, "rev-parse", "--show-toplevel")
	if err != nil {
		return "", fmt.Errorf("start git in %s: %w", path, err)
	}
	if !output.Status.Success() {
		return "", fmt.Errorf("%s is not a Git repository with a readable worktree: %s", path, strings.TrimSpace(fromUTF8Lossy(output.Stderr)))
	}
	root, err := decodeUTF8(output.Stdout, "decode Git repository root")
	if err != nil {
		return "", err
	}
	canonical, err := CanonicalGitOutputPath(strings.TrimSpace(root))
	if err != nil {
		return "", err
	}
	return MainWorktreeRoot(canonical)
}

// canonicalizePath is Rust's `fs::canonicalize`: an absolute path with every
// link resolved, which must exist.
func canonicalizePath(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// CanonicalGitOutputPath is a path on this machine that Git printed, in the
// one spelling local repository paths are stored and compared in: what
// canonicalization returns. Git for Windows prints `C:/Users/...`, which
// names the same directory as canonicalize's `\\?\C:\Users\...` but never
// compares equal.
func CanonicalGitOutputPath(path string) (string, error) {
	canonical, err := canonicalizePath(path)
	if err != nil {
		return "", fmt.Errorf("canonicalize local repository path %s: %w", path, err)
	}
	return canonical, nil
}

// MainWorktreeRoot maps a repository top level to the top level of its main
// working tree.
//
// A linked worktree created by `git worktree add` reports its own top level,
// but Hel treats it as the same repository as the main working tree.
func MainWorktreeRoot(root string) (string, error) {
	directory, err := canonicalizePath(root)
	if err != nil {
		return "", fmt.Errorf("resolve Git working directory %s: %w", root, err)
	}
	command := exec.Command("git", "rev-parse", "--path-format=absolute", "--git-common-dir")
	command.Dir = directory
	output, err := RunWithInput(command, nil)
	if err != nil {
		return "", fmt.Errorf("start git in %s: %w", root, err)
	}
	if !output.Status.Success() {
		return "", fmt.Errorf("could not read the common Git directory for %s: %s", root, strings.TrimSpace(fromUTF8Lossy(output.Stderr)))
	}
	common, err := decodeUTF8(output.Stdout, "decode common Git directory")
	if err != nil {
		return "", err
	}
	common, err = ResolveGitPath(directory, common)
	if err != nil {
		return "", err
	}
	// A bare or otherwise unusual layout has no `.git` parent to fall back to.
	if name, ok := pathFileName(common); !ok || name != ".git" {
		return root, nil
	}
	parent, ok := pathParent(common)
	if !ok {
		return root, nil
	}
	if info, err := os.Stat(parent); err != nil || !info.IsDir() {
		return root, nil
	}
	canonicalParent, err := canonicalizePath(parent)
	if err != nil {
		return "", fmt.Errorf("canonicalize main worktree %s: %w", parent, err)
	}
	if comparePaths(canonicalParent, root) == 0 {
		return root, nil
	}
	return canonicalParent, nil
}

// ResolveGitPath validates and normalizes a Git path or a relative Git
// configuration path against its working directory.
func ResolveGitPath(directory string, output string) (string, error) {
	if !IsAbsoluteOnHostOrTarget(directory) {
		return "", errors.New("Git working directory must be absolute")
	}
	lines := rustLines(output)
	if len(lines) == 0 {
		return "", errors.New("Git omitted the path")
	}
	value := lines[0]
	if value == "" || strings.HasPrefix(value, "--") || len(lines) > 1 {
		return "", fmt.Errorf("Git returned an invalid path: %s", debugString(output))
	}
	// Git separates its output with `/`. Parsed on its own rather than joined
	// as text: inside a verbatim `\\?\` directory `/` is a file name
	// character.
	valueComponents := pathComponents(value)
	hasRoot := false
	for _, component := range valueComponents {
		if component.kind == pathComponentRootDir {
			hasRoot = true
		}
	}
	var all []pathComponent
	if !hasRoot {
		all = append(all, pathComponents(directory)...)
	}
	all = append(all, valueComponents...)
	var common []pathComponent
	for _, component := range all {
		switch component.kind {
		case pathComponentCurDir:
		case pathComponentParentDir:
			// PathBuf::pop removes a last name and fails at a root or prefix.
			if len(common) == 0 || common[len(common)-1].kind != pathComponentNormal {
				return "", errors.New("Git path escapes its filesystem root")
			}
			common = common[:len(common)-1]
		default:
			common = append(common, component)
		}
	}
	path := ""
	for _, component := range common {
		path = rustPathPush(path, component.text)
	}
	if !IsAbsoluteOnHostOrTarget(path) {
		return "", errors.New("Git path must be absolute")
	}
	// Pushing separates components with `\` on Windows; a path on a session
	// target keeps POSIX separators.
	if IsAbsolute(directory) {
		return Text(path), nil
	}
	return path, nil
}

func localStatus(path string) (*string, error) {
	root, err := CanonicalRepository(path)
	if err != nil {
		return nil, err
	}
	head, err := runGit(root, "rev-parse", "--verify", "HEAD")
	if err != nil {
		return nil, fmt.Errorf("read HEAD in %s: %w", root, err)
	}
	if !head.Status.Success() {
		return nil, fmt.Errorf("local repository %s has no commit at HEAD", root)
	}
	output, err := runGit(root, "status", "--porcelain=v1", "--untracked-files=normal")
	if err != nil {
		return nil, fmt.Errorf("read Git status in %s: %w", root, err)
	}
	if !output.Status.Success() {
		return nil, fmt.Errorf("git status failed in %s: %s", root, strings.TrimSpace(fromUTF8Lossy(output.Stderr)))
	}
	status, err := decodeUTF8(output.Stdout, "decode Git status")
	if err != nil {
		return nil, err
	}
	lines := rustLines(status)
	if len(lines) == 0 {
		return nil, nil
	}
	summary := lines[0]
	if remaining := len(lines) - 1; remaining > 0 {
		summary = fmt.Sprintf("%s (and %d more)", lines[0], remaining)
	}
	return &summary, nil
}

// ResolveLocalRepository resolves the fetch and push endpoints Git would use
// for a host checkout.
//
// Git performs URL rewriting while answering `remote get-url`, so the values
// returned here are the actual network endpoints that should be persisted
// for the target checkout. The local checkout may be dirty or have commits
// that are not published; neither affects endpoint selection.
func ResolveLocalRepository(path string, executor CommandExecutor) (NetworkGitSource, error) {
	return resolveLocalRepositoryWithRemote(path, executor, nil)
}

// IdentityFetchURL: the default branch's fetch remote identifies the
// project. Feature-branch tracking and push destinations describe execution,
// not repository identity.
func IdentityFetchURL(path string, executor CommandExecutor) (*string, error) {
	listed, err := gitOutput(path, []string{"remote"}, executor, "list project remotes")
	if err != nil {
		return nil, err
	}
	remotes, err := parseLines(listed.Stdout, "project remotes")
	if err != nil {
		return nil, err
	}
	if len(remotes) == 0 {
		return nil, nil
	}
	initial, err := defaultFetchRemote(remotes)
	if err != nil {
		return nil, err
	}
	initialURL, err := remoteURL(path, initial, false, executor)
	if err != nil {
		return nil, err
	}
	// A filesystem remote cannot identify a network-backed project.
	if ValidateNetworkURL(initialURL) != nil {
		return nil, nil
	}
	trackingCommand := gitCommand(path, []string{"config", "--get-regexp", `^branch\..*\.remote$`}).
		WithPurpose("read project branch remotes")
	tracking, err := executor.Execute(&trackingCommand)
	if err != nil {
		return nil, err
	}
	if tracking.Status != 0 && tracking.Status != 1 {
		return nil, fmt.Errorf("could not read project branch remotes (status %d)", tracking.Status)
	}
	trackingText, err := decodeUTF8(tracking.Stdout, "decode project branch remotes")
	if err != nil {
		return nil, err
	}
	// Avoid a network probe when every possible branch selects the same
	// remote. This also makes ordinary directory discovery work offline.
	divergent := false
	for _, line := range rustLines(trackingText) {
		if fields := strings.Fields(line); len(fields) > 1 && fields[1] != initial {
			divergent = true
			break
		}
	}
	if !divergent {
		return &initialURL, nil
	}
	reference := fmt.Sprintf("refs/remotes/%s/HEAD", initial)
	headCommand := gitCommand(path, []string{"symbolic-ref", "--quiet", reference}).
		WithPurpose("read cached project default branch")
	head, err := executor.Execute(&headCommand)
	if err != nil {
		return nil, err
	}
	var defaultBranch string
	switch head.Status {
	case 0:
		text, err := decodeUTF8(head.Stdout, "decode project default branch")
		if err != nil {
			return nil, err
		}
		branch, found := strings.CutPrefix(strings.TrimSpace(text), fmt.Sprintf("refs/remotes/%s/", initial))
		if !found {
			return nil, errors.New("project remote HEAD does not name a branch on its remote")
		}
		defaultBranch = branch
	case 1:
		branch, err := DefaultBranch(&NetworkGitSource{FetchURL: initialURL}, executor)
		if err != nil {
			return nil, err
		}
		defaultBranch = branch
	default:
		return nil, fmt.Errorf("could not read project default branch (status %d)", head.Status)
	}
	configured, err := gitConfig(path, fmt.Sprintf("branch.%s.remote", defaultBranch), executor)
	if err != nil {
		return nil, err
	}
	remote := initial
	if configured != nil {
		remote = *configured
	}
	if err := ensureRemoteName(remote, "default branch"); err != nil {
		return nil, err
	}
	if !containsString(remotes, remote) {
		return nil, fmt.Errorf("default branch %s tracks missing Git remote %s", debugString(defaultBranch), debugString(remote))
	}
	url, err := remoteURL(path, remote, false, executor)
	if err != nil {
		return nil, err
	}
	if err := ValidateNetworkURL(url); err != nil {
		return nil, fmt.Errorf("default branch needs a network repository remote: %w", err)
	}
	return &url, nil
}

func containsString(values []string, value string) bool {
	for _, candidate := range values {
		if candidate == value {
			return true
		}
	}
	return false
}

// LocalRemoteRepair is a concrete, read-only proposal. Applying it requires a
// separate user action.
type LocalRemoteRepair struct {
	Path              string   `json:"path"`
	Branch            string   `json:"branch"`
	MissingRemote     string   `json:"missing_remote"`
	ReplacementRemote string   `json:"replacement_remote"`
	FetchURL          string   `json:"fetch_url"`
	PushURLs          []string `json:"push_urls"`
}

// MarshalJSON writes no push URLs as `[]`, as serde writes an empty Vec.
func (r LocalRemoteRepair) MarshalJSON() ([]byte, error) {
	type wire LocalRemoteRepair
	value := wire(r)
	value.PushURLs = nonNilStrings(value.PushURLs)
	return marshalSerdeJSON(value)
}

func (r *LocalRemoteRepair) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "path", "branch", "missing_remote", "replacement_remote", "fetch_url", "push_urls"); err != nil {
		return err
	}
	type wire LocalRemoteRepair
	return unmarshalDenyUnknown(data, (*wire)(r))
}

func (r LocalRemoteRepair) Error() string {
	return fmt.Sprintf(
		"%s: branch %s tracks missing remote %s. Repair tracking to %s? Fetch: %s; push: %s",
		r.Path, debugString(r.Branch), debugString(r.MissingRemote), debugString(r.ReplacementRemote), r.FetchURL, strings.Join(r.PushURLs, ", "),
	)
}

// Equal is the Rust derived PartialEq.
func (r LocalRemoteRepair) Equal(other LocalRemoteRepair) bool {
	if comparePaths(r.Path, other.Path) != 0 || r.Branch != other.Branch || r.MissingRemote != other.MissingRemote ||
		r.ReplacementRemote != other.ReplacementRemote || r.FetchURL != other.FetchURL ||
		len(r.PushURLs) != len(other.PushURLs) {
		return false
	}
	for i := range r.PushURLs {
		if r.PushURLs[i] != other.PushURLs[i] {
			return false
		}
	}
	return true
}

// RepositoryRemoteRepairs inspects every local source without changing host
// Git configuration.
func RepositoryRemoteRepairs(bundle *ProjectBundle, executor CommandExecutor) ([]LocalRemoteRepair, error) {
	var repairs []LocalRemoteRepair
	for _, repository := range bundle.Repositories {
		if repository.Local == nil {
			continue
		}
		path := *repository.Local
		if info, err := os.Stat(path); err != nil || !info.IsDir() {
			return nil, RefusalPrecondition(fmt.Sprintf(
				"Project %s: the directory %s does not exist.", debugString(repository.ID), path,
			))
		}
		if _, err := ResolveLocalRepository(path, executor); err != nil {
			var repair LocalRemoteRepair
			if !errors.As(err, &repair) {
				return nil, fmt.Errorf("repository %s: %w", debugString(repository.ID), err)
			}
			repairs = append(repairs, repair)
		}
	}
	return repairs, nil
}

// ApplyRepositoryRemoteRepairs applies only reviewed proposals belonging to
// this bundle, after rechecking their branch, stale setting, and destination
// against the current checkout.
func ApplyRepositoryRemoteRepairs(bundle *ProjectBundle, repairs []LocalRemoteRepair, executor CommandExecutor) error {
	for _, repair := range repairs {
		belongs := false
		for _, repository := range bundle.Repositories {
			if repository.Local != nil && comparePaths(*repository.Local, repair.Path) == 0 {
				belongs = true
				break
			}
		}
		if !belongs {
			return errors.New("remote repair repository is not part of the selected bundle")
		}
		_, err := ResolveLocalRepository(repair.Path, executor)
		var current LocalRemoteRepair
		if err == nil || !errors.As(err, &current) || !current.Equal(repair) {
			return errors.New("repository configuration changed; check the repository again before repairing tracking")
		}
		if _, err := gitOutput(repair.Path, []string{
			"config",
			"--local",
			"--replace-all",
			fmt.Sprintf("branch.%s.remote", repair.Branch),
			repair.ReplacementRemote,
		}, executor, "repair branch remote tracking"); err != nil {
			return err
		}
	}
	return nil
}

// noNetworkRemote is what the caller of a new session or resume is told when
// the project's repository has no remote the target can clone from. It names
// no path or host, so it may travel to any client.
const noNetworkRemote = "the project's Git repository has no remote; SSH and container targets clone the project from a network remote. Add one, for example `git remote add origin https://github.com/owner/repository.git`"

// localRemoteURL is the same, for a remote whose URL is a local path or
// another transport a target cannot reach.
const localRemoteURL = "the project's Git remote is not a network URL (it is a local path or an unsupported transport); SSH and container targets need an https://, ssh://, git://, or user@host:path remote"

func defaultFetchRemote(remotes []string) (string, error) {
	switch {
	case len(remotes) == 0:
		return "", RefusalUnusable(noNetworkRemote)
	case len(remotes) == 1:
		return remotes[0], nil
	case containsString(remotes, "origin"):
		return "origin", nil
	}
	return "", errors.New("repository has multiple Git remotes but no current-branch remote or `origin` to select")
}

func resolveLocalRepositoryWithRemote(path string, executor CommandExecutor, replacement *string) (NetworkGitSource, error) {
	branchText, err := gitText(path, []string{"branch", "--show-current"}, executor, "read current branch")
	if err != nil {
		return NetworkGitSource{}, err
	}
	var branch *string
	if trimmed := strings.TrimSpace(branchText); trimmed != "" {
		branch = &trimmed
	}

	remoteOutput, err := gitOutput(path, []string{"remote"}, executor, "list Git remotes")
	if err != nil {
		return NetworkGitSource{}, err
	}
	remotes, err := parseLines(remoteOutput.Stdout, "Git remote names")
	if err != nil {
		return NetworkGitSource{}, err
	}
	var branchRemote *string
	if branch != nil {
		branchRemote, err = gitConfig(path, fmt.Sprintf("branch.%s.remote", *branch), executor)
		if err != nil {
			return NetworkGitSource{}, err
		}
	}

	selected := replacement
	if selected == nil {
		selected = branchRemote
	}
	var fetchRemote string
	if selected != nil {
		remote := *selected
		if err := ensureRemoteName(remote, "current branch"); err != nil {
			return NetworkGitSource{}, err
		}
		if !containsString(remotes, remote) {
			if replacementRemote, err := defaultFetchRemote(remotes); err == nil {
				source, err := resolveLocalRepositoryWithRemote(path, executor, &replacementRemote)
				if err != nil {
					return NetworkGitSource{}, err
				}
				if branch == nil {
					return NetworkGitSource{}, errors.New("missing branch for remote tracking repair")
				}
				pushURLs := make([]string, len(source.PushURLs))
				for i, url := range source.PushURLs {
					pushURLs[i] = DisplayURL(url)
				}
				return NetworkGitSource{}, LocalRemoteRepair{
					Path:              path,
					Branch:            *branch,
					MissingRemote:     remote,
					ReplacementRemote: replacementRemote,
					FetchURL:          DisplayURL(source.FetchURL),
					PushURLs:          pushURLs,
				}
			}
			return NetworkGitSource{}, fmt.Errorf("current branch names Git remote %s, but that remote is not configured", debugString(remote))
		}
		fetchRemote = remote
	} else {
		fetchRemote, err = defaultFetchRemote(remotes)
		if err != nil {
			return NetworkGitSource{}, err
		}
	}

	var pushRemote string
	var configured *string
	if branch != nil {
		configured, err = gitConfig(path, fmt.Sprintf("branch.%s.pushRemote", *branch), executor)
		if err != nil {
			return NetworkGitSource{}, err
		}
	}
	if configured == nil {
		configured, err = gitConfig(path, "remote.pushDefault", executor)
		if err != nil {
			return NetworkGitSource{}, err
		}
	}
	pushRemote = fetchRemote
	if configured != nil {
		pushRemote = *configured
	}
	if err := ensureRemoteName(pushRemote, "push"); err != nil {
		return NetworkGitSource{}, err
	}
	if !containsString(remotes, pushRemote) {
		return NetworkGitSource{}, fmt.Errorf("push configuration names Git remote %s, but that remote is not configured", debugString(pushRemote))
	}

	fetchURL, err := remoteURL(path, fetchRemote, false, executor)
	if err != nil {
		return NetworkGitSource{}, err
	}
	pushURLs, err := remoteURLs(path, pushRemote, true, executor)
	if err != nil {
		return NetworkGitSource{}, err
	}
	if err := ValidateNetworkURL(fetchURL); err != nil {
		detail := fmt.Errorf("fetch URL %s for Git remote %s: %w", DisplayURL(fetchURL), debugString(fetchRemote), err)
		return NetworkGitSource{}, fmt.Errorf("%w: %w", RefusalUnusable(localRemoteURL), detail)
	}
	for _, pushURL := range pushURLs {
		if err := ValidateNetworkURL(pushURL); err != nil {
			return NetworkGitSource{}, fmt.Errorf("push URL %s for Git remote %s: %w", DisplayURL(pushURL), debugString(pushRemote), err)
		}
	}
	return NetworkGitSource{FetchURL: fetchURL, PushURLs: pushURLs}, nil
}

func gitCommand(path string, args []string) CommandSpec {
	command := NewCommandSpec("git", append([]string{"-C", path}, args...))
	UnattendedGitEnvironment(&command)
	return command
}

// UnattendedGitEnvironment is the environment for a local Git command that
// may reach a remote with no one present to answer: fail instead of
// prompting for credentials or host keys, and never fetch missing objects
// behind the caller's back.
func UnattendedGitEnvironment(command *CommandSpec) {
	if command.Env == nil {
		command.Env = map[string]string{}
	}
	command.Env["GIT_TERMINAL_PROMPT"] = "0"
	command.Env["GIT_NO_LAZY_FETCH"] = "1"
	if value, ok := LocalGitSSHCommand(); ok {
		command.Env["GIT_SSH_COMMAND"] = value
	}
}

// NonInteractiveGitSSHCommand: SSH asks about an unknown host key itself,
// which `GIT_TERMINAL_PROMPT` does not reach, so Git that no one is watching
// gets a batch-mode command.
const NonInteractiveGitSSHCommand = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=15"

// LocalGitSSHCommand is how Git that Mjolnir starts on this machine reaches
// an SSH remote: the operator's `GIT_SSH_COMMAND` when it is set, otherwise
// NonInteractiveGitSSHCommand. false means the operator's command applies
// and the child inherits it byte for byte; true returns the default to set.
// RemoteGitSSHCommand is the same decision for Git Mjolnir runs on another
// machine, and these two are the only places it is made (#1466).
func LocalGitSSHCommand() (string, bool) {
	if _, set := os.LookupEnv("GIT_SSH_COMMAND"); set {
		return "", false
	}
	return NonInteractiveGitSSHCommand, true
}

// RemoteGitSSHCommand is LocalGitSSHCommand's decision as text, for Git that
// runs on another machine, such as a clone-cache host or a session's target,
// where the command must travel in an argument. An operator command that is
// not UTF-8 cannot travel as text and is refused rather than altered; Git on
// this computer uses LocalGitSSHCommand and inherits it instead.
func RemoteGitSSHCommand() (string, error) {
	command, set := os.LookupEnv("GIT_SSH_COMMAND")
	switch {
	case !set:
		return NonInteractiveGitSSHCommand, nil
	// Git on this computer fails trying to run an empty command. Sent
	// elsewhere, an empty value would read as "use that machine's own", so
	// it is refused here to fail the same way.
	case command == "":
		return "", errors.New("GIT_SSH_COMMAND is set but empty, and Git cannot run an empty command; unset it or set it to an SSH command")
	case !utf8.ValidString(command):
		return "", errors.New("GIT_SSH_COMMAND is not valid UTF-8, so Mjolnir cannot send it to the machine that runs this Git command; set it to a UTF-8 command")
	}
	return command, nil
}

func gitOutput(path string, args []string, executor CommandExecutor, purpose string) (CommandOutput, error) {
	if executor.CancellationRequested() {
		return CommandOutput{}, fmt.Errorf("operation cancelled while %s", purpose)
	}
	command := gitCommand(path, args).WithPurpose(purpose)
	output, err := executor.Execute(&command)
	if err != nil {
		return CommandOutput{}, fmt.Errorf("%s in %s: %w", purpose, path, err)
	}
	if executor.CancellationRequested() {
		return CommandOutput{}, fmt.Errorf("operation cancelled while %s", purpose)
	}
	if output.Status != 0 {
		detail := strings.TrimSpace(fromUTF8Lossy(output.Stderr))
		if detail == "" {
			return CommandOutput{}, fmt.Errorf("%s in %s failed with status %d", purpose, path, output.Status)
		}
		return CommandOutput{}, fmt.Errorf("%s in %s failed: %s", purpose, path, detail)
	}
	return output, nil
}

func gitText(path string, args []string, executor CommandExecutor, purpose string) (string, error) {
	output, err := gitOutput(path, args, executor, purpose)
	if err != nil {
		return "", err
	}
	return decodeUTF8(output.Stdout, fmt.Sprintf("decode %s output", purpose))
}

func gitConfig(path string, key string, executor CommandExecutor) (*string, error) {
	command := gitCommand(path, []string{"config", "--get", key}).WithPurpose("read Git configuration key")
	if executor.CancellationRequested() {
		return nil, errors.New("operation cancelled while read Git configuration")
	}
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, fmt.Errorf("read Git configuration key %s: %w", debugString(key), err)
	}
	if executor.CancellationRequested() {
		return nil, errors.New("operation cancelled while read Git configuration")
	}
	switch output.Status {
	case 0:
		text, err := decodeUTF8(output.Stdout, fmt.Sprintf("decode Git configuration key %s", debugString(key)))
		if err != nil {
			return nil, err
		}
		value := strings.TrimSpace(text)
		return &value, nil
	case 1:
		return nil, nil
	}
	detail := strings.TrimSpace(fromUTF8Lossy(output.Stderr))
	if detail == "" {
		return nil, fmt.Errorf("read Git configuration key %s failed with status %d", debugString(key), output.Status)
	}
	return nil, fmt.Errorf("read Git configuration key %s failed: %s", debugString(key), detail)
}

func parseLines(output []byte, kind string) ([]string, error) {
	text, err := decodeUTF8(output, fmt.Sprintf("decode %s", kind))
	if err != nil {
		return nil, err
	}
	var lines []string
	for _, line := range rustLines(text) {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		if strings.IndexFunc(line, unicode.IsSpace) >= 0 {
			return nil, fmt.Errorf("%s contains an invalid whitespace-bearing entry", kind)
		}
		lines = append(lines, line)
	}
	return lines, nil
}

func ensureRemoteName(name string, role string) error {
	if name == "" || name == "." || strings.IndexFunc(name, unicode.IsSpace) >= 0 {
		return fmt.Errorf("%s Git remote selection is empty or refers to the local repository", role)
	}
	return nil
}

func remoteURL(path string, remote string, push bool, executor CommandExecutor) (string, error) {
	urls, err := remoteURLs(path, remote, push, executor)
	if err != nil {
		return "", err
	}
	if len(urls) == 0 {
		return "", fmt.Errorf("Git remote %s has no configured URL", debugString(remote))
	}
	return urls[0], nil
}

func remoteURLs(path string, remote string, push bool, executor CommandExecutor) ([]string, error) {
	args := []string{"remote", "get-url", remote}
	purpose, kind, role := "read Git fetch URL", "Git fetch URL", "fetch"
	if push {
		args = []string{"remote", "get-url", "--push", "--all", remote}
		purpose, kind, role = "read Git push URL", "Git push URLs", "push"
	}
	output, err := gitOutput(path, args, executor, purpose)
	if err != nil {
		return nil, err
	}
	urls, err := parseLines(output.Stdout, kind)
	if err != nil {
		return nil, err
	}
	if len(urls) == 0 {
		return nil, fmt.Errorf("Git remote %s has no %s URL", debugString(remote), role)
	}
	return urls, nil
}

// MinimumGitVersion is the oldest Git Mjolnir supports: Debian 12's. It has
// `git merge-tree --write-tree` (Git 2.38), which before-push review needs to
// start a merged branch's review from the merge of its reviewed work.
var MinimumGitVersion = [3]uint32{2, 39, 5}

// ParseGitVersion reads the version in `git version` output, such as
// `git version 2.39.5 (Apple Git-154)` or `git version 2.53.0.windows.1`.
// A missing patch number reads as 0.
func ParseGitVersion(output string) ([3]uint32, bool) {
	version, found := strings.CutPrefix(strings.TrimSpace(output), "git version ")
	if !found {
		return [3]uint32{}, false
	}
	// Rust's `split` on non-digits keeps the empty pieces between them.
	var parts []string
	start := 0
	for i, c := range version {
		if c < '0' || c > '9' {
			parts = append(parts, version[start:i])
			start = i + utf8.RuneLen(c)
		}
	}
	parts = append(parts, version[start:])
	if len(parts) < 2 {
		return [3]uint32{}, false
	}
	major, ok := parseUint32(parts[0])
	if !ok {
		return [3]uint32{}, false
	}
	minor, ok := parseUint32(parts[1])
	if !ok {
		return [3]uint32{}, false
	}
	var patch uint32
	if len(parts) > 2 {
		if value, ok := parseUint32(parts[2]); ok {
			patch = value
		}
	}
	return [3]uint32{major, minor, patch}, true
}

// CompareGitVersions orders versions as Rust's tuple comparison does.
func CompareGitVersions(a, b [3]uint32) int {
	for i := range a {
		if a[i] != b[i] {
			if a[i] < b[i] {
				return -1
			}
			return 1
		}
	}
	return 0
}
