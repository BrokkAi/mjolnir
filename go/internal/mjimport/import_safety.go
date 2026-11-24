// Port of mj-import/src/import/safety.rs.

package mjimport

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ImportSafetyIssuesFor is Rust's `import_safety_issues` (renamed: the type
// ImportSafetyIssues keeps its name). It reads the Git status of every root
// the session edited.
func ImportSafetyIssuesFor(targets *SessionEditTargets) (ImportSafetyIssues, error) {
	dirtyGitRoots := [][2]string{}
	hasUntrackedFiles := false
	for _, root := range targets.GitRoots {
		command := exec.Command("git", "status", "--porcelain=v1", "--untracked-files=normal")
		command.Dir = root
		output, err := core.RunWithInput(command, nil)
		if err != nil {
			return ImportSafetyIssues{}, fmt.Errorf("inspect Git status in %s: %w", root, err)
		}
		if !output.Status.Success() {
			return ImportSafetyIssues{}, fmt.Errorf("could not inspect Git status in %s", root)
		}
		tracked, untracked := 0, 0
		for _, line := range importLines(string(output.Stdout)) {
			if strings.HasPrefix(line, "??") {
				untracked++
			} else {
				tracked++
			}
		}
		hasUntrackedFiles = hasUntrackedFiles || untracked > 0
		if tracked+untracked > 0 {
			parts := []string{}
			if tracked > 0 {
				parts = append(parts, fmt.Sprintf("%d tracked change%s", tracked, importPlural(tracked)))
			}
			if untracked > 0 {
				parts = append(parts, fmt.Sprintf("%d untracked path%s", untracked, importPlural(untracked)))
			}
			dirtyGitRoots = append(dirtyGitRoots, [2]string{root, strings.Join(parts, " · ")})
		}
	}
	return ImportSafetyIssues{
		DirtyGitRoots:     dirtyGitRoots,
		OmittedNonGitDirs: append([]string{}, targets.NonGitDirs...),
		ScratchGitRoots:   append([]string{}, targets.ScratchGitRoots...),
		HasUntrackedFiles: hasUntrackedFiles,
	}, nil
}

// importPlural is the "s" a count other than one takes.
func importPlural(count int) string {
	if count == 1 {
		return ""
	}
	return "s"
}

// importLines is Rust's `str::lines`: lines split at "\n" with a trailing
// "\r" removed, and no empty line after a final newline.
func importLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(text, "\n")
	if lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	for index, line := range lines {
		lines[index] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

type ImportedClaudeSession struct {
	// RemappedBundle is nil unless the import relocated the project.
	RemappedBundle  *core.ProjectBundle
	SessionID       string
	NativeSessionID string
	SourceJSONL     string
	SourceCwd       string
	BundleID        string
	ArchivePath     string
}

// ImportArchiveProgress is one step of writing an import archive. One of
// ImportArchiveProgressRepository, ImportArchiveProgressUntrackedFile or
// ImportArchiveProgressWritingArchive.
type ImportArchiveProgress interface {
	isImportArchiveProgress()
}

type ImportArchiveProgressRepository struct {
	Current int
	Total   int
	ID      string
}

type ImportArchiveProgressUntrackedFile struct {
	RepositoryID string
	Current      int
	Total        int
	Path         string
}

type ImportArchiveProgressWritingArchive struct{}

func (ImportArchiveProgressRepository) isImportArchiveProgress()     {}
func (ImportArchiveProgressUntrackedFile) isImportArchiveProgress()  {}
func (ImportArchiveProgressWritingArchive) isImportArchiveProgress() {}

// ImportControl lets the caller cancel an import and follow its progress.
// Progress must be safe for concurrent use (Rust: Sync): repositories are
// collected concurrently.
type ImportControl struct {
	Cancelled        *atomic.Bool
	Progress         func(ImportArchiveProgress)
	IncludeUntracked bool
}

func (c *ImportControl) checkCancelled() error {
	if c.Cancelled.Load() {
		return errors.New("import cancelled")
	}
	return nil
}

// Report checks for cancellation, then reports progress.
func (c *ImportControl) Report(progress ImportArchiveProgress) error {
	if err := c.checkCancelled(); err != nil {
		return err
	}
	c.Progress(progress)
	return nil
}

// HarnessConfigHome resolves a harness's configuration home without ever
// modifying it.
//
// The environment override wins; otherwise the harness's default directory
// beneath the user's home is used, the same pair first-run setup discovers.
func HarnessConfigHome(kind core.HarnessKind) (string, error) {
	if value, ok := os.LookupEnv(kind.HomeEnv()); ok {
		return kind.HomeFromEnvironment(value), nil
	}
	if home, ok := core.UserHome(); ok {
		return filepath.Join(home, kind.DefaultHomeLeaf()), nil
	}
	return "", fmt.Errorf("cannot determine %s home; set %s", kind.DisplayName(), kind.HomeEnv())
}
