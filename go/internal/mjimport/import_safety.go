// Port of mj-import/src/import/safety.rs.

package mjimport

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ImportSafetyIssuesFor is Rust's `import_safety_issues` (renamed: the type
// ImportSafetyIssues keeps its name). It reads the Git status of every root
// the session edited.
func ImportSafetyIssuesFor(targets *SessionEditTargets) (ImportSafetyIssues, error) {
	panic("unported: mj-import/src/import/safety.rs import_safety_issues")
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
