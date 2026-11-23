// Port of mj-core/src/session_keep.rs.

// Per-session scratch directory `.mj/keep/<session-id>/`.
//
// The directory is created without following a symlink at `.mj`, `.mj/keep`,
// or the session directory, the same refusal the report-root cleanup uses
// for `.mj` and `.mj/agents` (mj-controller REMOVE_REPORT_DIRS_SCRIPT).
// Ancestors above `.mj` may be symlinks. `/.mj/keep/` is listed in
// `.git/info/exclude` with the same append rules as `/.mj/agents/`
// (PREPARE_REPORT_DIR_SCRIPT): one exact line, a newline before it when the
// file does not already end in one, and no write when the checkout is not a
// Git repository.

package core

import (
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"unicode/utf8"
)

// SessionStateInstructions are staged into every harness's instruction file.
//
//go:embed assets/session-state.md
var SessionStateInstructions string

// KeepExcludeEntry is the Git exclude line for the keep directory. Anchored
// at the work tree root, matching `/.mj/agents/`.
const KeepExcludeEntry = "/.mj/keep/"

const (
	keepParent = ".mj"
	keepName   = "keep"
)

// IsSessionDirectoryName reports whether sessionID is a single directory
// name, so joining it under `.mj/keep/` cannot leave the checkout or name
// another session's directory.
func IsSessionDirectoryName(sessionID string) bool {
	if sessionID == "" {
		return false
	}
	for i := 0; i < len(sessionID); i++ {
		b := sessionID[i]
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '-' || b == '_') {
			return false
		}
	}
	return true
}

func validateSessionDirectoryName(sessionID string) error {
	if !IsSessionDirectoryName(sessionID) {
		return fmt.Errorf("session id %s is not a single directory name", debugString(sessionID))
	}
	return nil
}

// SnapshotKeepDir is `<checkout>/.mj/keep` when sessionID is nil, otherwise
// `<checkout>/.mj/keep/<session-id>`.
func SnapshotKeepDir(checkout string, sessionID *string) (string, error) {
	path := rustPathPush(rustPathPush(checkout, keepParent), keepName)
	if sessionID != nil {
		if err := validateSessionDirectoryName(*sessionID); err != nil {
			return "", err
		}
		path = rustPathPush(path, *sessionID)
	}
	return path, nil
}

// MoveKeep is the source directory and path exclusions for a cross-target
// Move.
type MoveKeep struct {
	directory string
	// skipped is sorted by component and holds no duplicates (Rust's
	// BTreeSet<PathBuf>).
	skipped []string
}

func NewMoveKeep(directory string, skip []string) (MoveKeep, error) {
	keep := MoveKeep{directory: directory}
	for _, path := range skip {
		normalized, err := NormalizeKeepPath(path)
		if err != nil {
			return MoveKeep{}, err
		}
		keep.skipped = append(keep.skipped, normalized)
	}
	keep.skipped = sortedUniquePaths(keep.skipped)
	return keep, nil
}

func sortedUniquePaths(paths []string) []string {
	slices.SortFunc(paths, comparePaths)
	return slices.CompactFunc(paths, func(a, b string) bool { return comparePaths(a, b) == 0 })
}

func (k *MoveKeep) Directory() string { return k.directory }

func (k *MoveKeep) SkippedPaths() []string { return k.skipped }

// RelativePath matches a path under the checkout-wide keep directory against
// the one session directory the transport sends.
func (k *MoveKeep) RelativePath(keepRoot, path string) (string, bool) {
	prefix, ok := pathStripPrefix(k.directory, keepRoot)
	if !ok {
		return "", false
	}
	return pathStripPrefix(path, prefix)
}

func (k *MoveKeep) Carries(keepRoot, path string) bool {
	relative, ok := k.RelativePath(keepRoot, path)
	return ok && !k.Skips(relative)
}

func (k *MoveKeep) Skips(relative string) bool {
	for _, skip := range k.skipped {
		if pathStartsWith(relative, skip) {
			return true
		}
	}
	return false
}

func (k MoveKeep) MarshalJSON() ([]byte, error) {
	skipped := k.skipped
	if skipped == nil {
		skipped = []string{}
	}
	return json.Marshal(struct {
		Directory string   `json:"directory"`
		Skipped   []string `json:"skipped"`
	}{k.directory, skipped})
}

func (k *MoveKeep) UnmarshalJSON(data []byte) error {
	var wire struct {
		Directory *string  `json:"directory"`
		Skipped   []string `json:"skipped"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	switch {
	case wire.Directory == nil:
		return errors.New("missing field `directory`")
	case wire.Skipped == nil:
		return errors.New("missing field `skipped`")
	}
	*k = MoveKeep{directory: *wire.Directory, skipped: sortedUniquePaths(wire.Skipped)}
	return nil
}

func NormalizeKeepPath(path string) (string, error) {
	var parts []string
	for _, component := range pathComponents(path) {
		switch component.kind {
		case pathComponentNormal:
			parts = append(parts, component.text)
		case pathComponentCurDir:
		default:
			return "", fmt.Errorf("invalid relative path in keep stream: %s", path)
		}
	}
	if len(parts) == 0 {
		return "", errors.New("empty relative path in keep stream")
	}
	return filepath.Join(parts...), nil
}

// InstallSessionKeepDir creates the session keep directory and exports
// SessionKeepDirEnv.
//
// cwd may be anywhere inside the checkout. The directory is created at the
// Git top level when cwd is in a repository, and at cwd otherwise. A
// read-only checkout returns the filesystem error; the caller warns and
// continues without the variable.
func InstallSessionKeepDir(cwd, sessionID string, environment map[string]string) (string, error) {
	if err := validateSessionDirectoryName(sessionID); err != nil {
		return "", err
	}
	checkout, repository, err := checkoutTopLevel(cwd)
	if err != nil {
		return "", err
	}
	path, err := createKeepDirectory(checkout, sessionID)
	if err != nil {
		return "", err
	}
	if repository {
		if err := AppendGitInfoExclude(checkout, KeepExcludeEntry); err != nil {
			return "", err
		}
	}
	if !utf8.ValidString(path) {
		return "", fmt.Errorf("session keep directory %s is not Unicode", path)
	}
	environment[SessionKeepDirEnv] = path
	return path, nil
}

// InterpretCheckoutTopLevel interprets the result of `git -C <cwd> rev-parse
// --show-toplevel` when locating a session's keep directory: the top level
// and true inside a Git repository, cwd and false for a plain directory
// (`exit 128` with "not a git repository"), and an error for any other Git
// failure. status is nil when Git ended without an exit code.
func InterpretCheckoutTopLevel(cwd string, status *int, stdout, stderr []byte) (string, bool, error) {
	if status != nil && *status == 0 {
		if !utf8.Valid(stdout) {
			return "", false, errors.New("decode Git checkout top level: stream did not contain valid UTF-8")
		}
		top := strings.TrimSpace(string(stdout))
		if top == "" {
			return "", false, errors.New("Git returned an empty checkout top level")
		}
		return top, true, nil
	}
	text := lossyUTF8(stderr)
	if status != nil && *status == 128 && strings.Contains(text, "not a git repository") {
		return cwd, false, nil
	}
	return "", false, fmt.Errorf("resolve checkout %s: %s", cwd, strings.TrimSpace(text))
}

// InterpretCheckoutTopLevelAsGiven interprets `git -C <cwd> rev-parse
// --show-prefix --show-toplevel` as InterpretCheckoutTopLevel does, except
// that a cwd which is itself the top level is returned as given. Git spells
// the top level with symlinks resolved (`/var` is `/private/var` on macOS)
// and absolute where a session records it relative to the target user's
// home, while a Move's preview and capture find the keep directory under the
// session's recorded checkout.
func InterpretCheckoutTopLevelAsGiven(cwd string, status *int, stdout, stderr []byte) (string, bool, error) {
	if status == nil || *status != 0 {
		return InterpretCheckoutTopLevel(cwd, status, stdout, stderr)
	}
	if !utf8.Valid(stdout) {
		return "", false, errors.New("decode Git checkout top level: stream did not contain valid UTF-8")
	}
	prefix, top, found := strings.Cut(string(stdout), "\n")
	if !found {
		return "", false, errors.New("Git did not report the checkout prefix and top level")
	}
	if prefix == "" {
		return cwd, true, nil
	}
	return InterpretCheckoutTopLevel(cwd, status, []byte(top), stderr)
}

func checkoutTopLevel(cwd string) (string, bool, error) {
	absolute, err := filepath.Abs(cwd)
	if err != nil {
		return "", false, fmt.Errorf("resolve %s: %w", cwd, err)
	}
	output, err := sessionKeepGit(absolute, []string{"rev-parse", "--show-toplevel"}, "resolve checkout top level")
	if err != nil {
		return "", false, err
	}
	top, repository, err := InterpretCheckoutTopLevel(absolute, sessionKeepExitCode(output), output.Stdout, output.Stderr)
	if err != nil {
		return "", false, err
	}
	canonical, err := canonicalizePath(top)
	if err != nil {
		return "", false, fmt.Errorf("resolve checkout top level %s: %w", top, err)
	}
	return canonical, repository, nil
}

func sessionKeepExitCode(output ProcessOutput) *int {
	if output.Status == nil || output.Status.ExitCode() < 0 {
		return nil
	}
	code := output.Status.ExitCode()
	return &code
}

func createKeepDirectory(checkout, sessionID string) (string, error) {
	if err := refuseSymlinkedKeepPath(checkout, sessionID); err != nil {
		return "", err
	}
	relative := filepath.Join(keepParent, keepName, sessionID)
	root, err := ConfinedDirOpenRoot(checkout)
	if err != nil {
		return "", fmt.Errorf("open checkout %s: %w", checkout, err)
	}
	if root == nil {
		return "", fmt.Errorf("checkout %s is missing", checkout)
	}
	defer root.Close()
	created, err := root.CreateDirAll(relative)
	if err != nil {
		return "", fmt.Errorf("create session keep directory %s: %w", checkout, err)
	}
	defer created.Close()
	resolved, err := canonicalizePath(created.Path())
	if err != nil {
		return "", fmt.Errorf("resolve session keep directory %s: %w", created.Path(), err)
	}
	relativeResolved, ok := pathStripPrefix(resolved, checkout)
	if !ok {
		return "", fmt.Errorf("session keep directory %s is outside %s", resolved, checkout)
	}
	if comparePaths(relativeResolved, relative) != 0 {
		return "", fmt.Errorf("session keep directory %s escaped %s", resolved, checkout)
	}
	return resolved, nil
}

// refuseSymlinkedKeepPath refuses `.mj`, `.mj/keep`, or
// `.mj/keep/<session-id>` when that path is a symlink. A missing component
// stops the walk: nothing below it exists yet. The checkout itself is
// already canonical, so a symlink above `.mj` is not a component of this
// walk.
func refuseSymlinkedKeepPath(checkout, sessionID string) error {
	current := checkout
	for _, name := range []string{keepParent, keepName, sessionID} {
		current = rustPathPush(current, name)
		info, err := os.Lstat(current)
		switch {
		case err == nil && info.Mode()&fs.ModeSymlink != 0:
			return fmt.Errorf("refusing a symlinked session keep path %s", current)
		case err == nil:
		case errors.Is(err, fs.ErrNotExist):
			return nil
		default:
			return fmt.Errorf("inspect %s: %w", current, err)
		}
	}
	return nil
}

// AppendGitInfoExclude appends line to the repository's `info/exclude` when
// it is not already an exact line. A checkout that is not a Git repository
// is left unchanged.
func AppendGitInfoExclude(repository, line string) error {
	if line == "" || strings.ContainsAny(line, "\n\r") {
		return errors.New("git exclude entry must be one line")
	}
	output, err := sessionKeepGit(repository,
		[]string{"rev-parse", "--path-format=absolute", "--git-path", "info/exclude"},
		"resolve repository-local exclude file")
	if err != nil {
		return err
	}
	if !output.Status.Success() {
		text := lossyUTF8(output.Stderr)
		if output.Status.ExitCode() == 128 && strings.Contains(text, "not a git repository") {
			return nil
		}
		return fmt.Errorf("resolve info/exclude in %s: %s", repository, strings.TrimSpace(text))
	}
	if !utf8.Valid(output.Stdout) {
		return errors.New("decode Git exclude path: stream did not contain valid UTF-8")
	}
	exclude, err := ResolveGitPath(repository, string(output.Stdout))
	if err != nil {
		return err
	}
	existingBytes, err := os.ReadFile(exclude)
	switch {
	case errors.Is(err, fs.ErrNotExist):
		existingBytes = nil
	case err != nil:
		return fmt.Errorf("read %s: %w", exclude, err)
	case !utf8.Valid(existingBytes):
		return fmt.Errorf("read %s: stream did not contain valid UTF-8", exclude)
	}
	existing := string(existingBytes)
	for _, entry := range rustLines(existing) {
		if entry == line {
			return nil
		}
	}
	if parent, ok := pathParent(exclude); ok && parent != "" {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return fmt.Errorf("create %s: %w", parent, err)
		}
	}
	file, err := os.OpenFile(exclude, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o666)
	if err != nil {
		return fmt.Errorf("open %s: %w", exclude, err)
	}
	defer file.Close()
	if existing != "" && !strings.HasSuffix(existing, "\n") {
		if _, err := file.WriteString("\n"); err != nil {
			return err
		}
	}
	_, err = file.WriteString(line + "\n")
	return err
}

func sessionKeepGit(cwd string, args []string, purpose string) (ProcessOutput, error) {
	command := exec.Command("git", append([]string{"-C", cwd}, args...)...)
	// The checkout named by `-C` is the one this session works in. An ambient
	// Git directory would point the top level and the exclude file elsewhere.
	var environment []string
	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		switch name {
		case "LC_ALL", "GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR":
			continue
		}
		environment = append(environment, entry)
	}
	command.Env = append(environment, "LC_ALL=C")
	output, err := RunWithInput(command, nil)
	if err != nil {
		return ProcessOutput{}, fmt.Errorf("start git to %s in %s: %w", purpose, cwd, err)
	}
	return output, nil
}
