// Port of mj-import/src/import.rs.
//
// Import native harness sessions into Hel's durable archive format.
//
// Rust's `import` module and its submodules (`import/*.rs`) are files of this
// package named `import_<submodule>.go`. Rust calls into the `muse`
// submodule by its path (`muse::scan`, `muse::locate`); those functions carry
// a `muse` prefix here (museScan, museLocate, museValidateTranscript,
// museRecentDirectories).
//
// A function whose Go name would equal a type's keeps the type's name for the
// type and takes a `For` suffix: GithubRepositoryFor (Rust's
// github_repository), SessionEditTargetsFor, ImportSafetyIssuesFor,
// claudeNativeSummaryFor, codexSessionMetadataFor, codexSessionSummaryFor,
// kimiSessionSummaryFor and museSummaryFor.

package mjimport

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// GithubRepository is a repository on GitHub, as its owner and name.
type GithubRepository struct {
	Owner      string
	Repository string
}

// Source is the repository as `owner/repository`.
func (r GithubRepository) Source() string {
	return r.Owner + "/" + r.Repository
}

// GithubRepositoryFromOrigin reads a GitHub repository from an origin,
// accepting setup's `owner/repository` shorthand.
func GithubRepositoryFromOrigin(origin string) *GithubRepository {
	return GithubRepositoryFor(origin, core.ShorthandAllowed)
}

// GithubRepositoryFor is Rust's `github_repository` (renamed: the type
// GithubRepository keeps its name). nil when origin names no GitHub
// repository.
func GithubRepositoryFor(origin string, shorthand core.Shorthand) *GithubRepository {
	remote := core.ParseGithubRemote(origin, shorthand)
	if remote == nil {
		return nil
	}
	return &GithubRepository{Owner: remote.Owner, Repository: remote.Repository}
}

// NativeProjectSeed is only project metadata read from the newest native
// sessions. Never imports conversations, computes checkout sizes, or probes
// Git for this seed.
type NativeProjectSeed struct {
	Directories []string
	// Errors are (path, message) pairs.
	Errors [][2]string
}

// RecentProjectDirectories reads the project directories of the newest
// native sessions in a harness home, at most limit of them.
func RecentProjectDirectories(
	harness core.HarnessKind,
	home string,
	limit int,
	executor core.CommandExecutor,
) (NativeProjectSeed, error) {
	panic("unported: mj-import/src/import.rs recent_project_directories")
}

// NativeProjectDirectory reads a native log's project directory. Project
// metadata lives near the start of each log. Bound the read and stop as soon
// as it is found, without parsing the conversation after it.
func NativeProjectDirectory(
	path string,
	harness core.HarnessKind,
	executor core.CommandExecutor,
) (string, error) {
	panic("unported: mj-import/src/import.rs native_project_directory")
}

// importIsFile is Rust's `Path::is_file`: the path, following symlinks, is
// a regular file.
func importIsFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

// importIsDir is Rust's `Path::is_dir`: the path, following symlinks, is a
// directory.
func importIsDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

// importCanonicalize is Rust's `fs::canonicalize`: an absolute path with
// every link resolved, which must exist.
func importCanonicalize(path string) (string, error) {
	panic("unported: mj-import/src/import.rs fs::canonicalize")
}

// importPathStartsWith is Rust's `Path::starts_with`: whether base is a
// component prefix of path.
func importPathStartsWith(path, base string) bool {
	panic("unported: mj-import/src/import.rs Path::starts_with")
}

// importPathStripPrefix is Rust's `Path::strip_prefix`: path relative to
// base, when base is a component prefix of it.
func importPathStripPrefix(path, base string) (string, bool) {
	panic("unported: mj-import/src/import.rs Path::strip_prefix")
}

// importPathParent is Rust's `Path::parent`: false for a root or an empty
// path, and "" for a single relative component.
func importPathParent(path string) (string, bool) {
	panic("unported: mj-import/src/import.rs Path::parent")
}

// importPathFileName is Rust's `Path::file_name`: the final normal
// component, false when the path ends in `..` or is a root.
func importPathFileName(path string) (string, bool) {
	panic("unported: mj-import/src/import.rs Path::file_name")
}

// importComparePaths is Rust's `Ord for Path`: component by component.
func importComparePaths(a, b string) int {
	panic("unported: mj-import/src/import.rs Ord for Path")
}
