// Port of mj-import/src/import/native_import.rs.

package mjimport

import (
	"io"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NativeImportRequest is one already-located, already-parsed native session
// to import. The optional fields are nil when absent.
type NativeImportRequest struct {
	Harness           core.HarnessKind
	HarnessHome       string
	NativeSessionID   string
	SourcePath        string
	Transcript        *ClaudeTranscript
	BundleID          string
	ProfileID         *string
	Title             *string
	DaemonProjectRoot *string
	ArchiveDirectory  string
}

// ImportNativeSession builds, verifies, and installs a local archive for one
// already-located, already-parsed native session, for any harness, then
// updates the in-memory state. The caller saves state only after this
// returns successfully. control is nil when the caller neither cancels nor
// follows progress.
func ImportNativeSession(
	config *core.Config,
	state *core.State,
	request NativeImportRequest,
	control *ImportControl,
) (ImportedClaudeSession, error) {
	panic("unported: mj-import/src/import/native_import.rs import_native_session")
}

func defaultImportTargetID(config *core.Config) string {
	panic("unported: mj-import/src/import/native_import.rs default_import_target_id")
}

// rawImportTargetID is the target that hosts raw project sessions on this
// machine.
func rawImportTargetID(config *core.Config) (string, bool) {
	panic("unported: mj-import/src/import/native_import.rs raw_import_target_id")
}

// RawProjectImport is the project directory and target of a raw project
// import, ok false when the session is imported as a bundle session.
//
// A session that only wrote to its own repository can keep working in that
// directory, so import it as a raw project session instead of a bundle
// session. SessionEditTargetsFor always records the cwd root, so a single
// durable root is that root.
func RawProjectImport(config *core.Config, targets *SessionEditTargets) (directory string, targetID string, ok bool) {
	if len(targets.GitRoots) != 1 {
		return "", "", false
	}
	targetID, ok = rawImportTargetID(config)
	if !ok {
		return "", "", false
	}
	return targets.GitRoots[0], targetID, true
}

func collectLocalRepositories(
	bundle *core.ProjectBundle,
	detectedRoots []string,
	isolated bool,
	control *ImportControl,
) ([]checkpoint.RepositorySnapshot, error) {
	panic("unported: mj-import/src/import/native_import.rs collect_local_repositories")
}

func defaultProfile(config *core.Config, harness core.HarnessKind, home string) string {
	panic("unported: mj-import/src/import/native_import.rs default_profile")
}

// importProfileID is the profile an import records; requested is nil when
// the user named none.
func importProfileID(config *core.Config, requested *string, harness core.HarnessKind, home string) (string, error) {
	panic("unported: mj-import/src/import/native_import.rs import_profile_id")
}

// importDeltaBase is the upstream revision an imported repository deltas
// from. A repository without remote-tracking refs cannot tell us which
// ancestry a newly provisioned clone has, and Hel never bundles full
// history, so it fails here.
func importDeltaBase(path string, fetchURL string) (string, error) {
	panic("unported: mj-import/src/import/native_import.rs import_delta_base")
}

// gitOptionalText runs `git <arguments>` in cwd and returns its trimmed
// output; false when Git fails or prints nothing.
func gitOptionalText(cwd string, arguments ...string) (string, bool, error) {
	panic("unported: mj-import/src/import/native_import.rs git_optional_text")
}

// timestamp is now in RFC 3339 with whole seconds and a `Z` offset.
func timestamp() string {
	return time.Now().UTC().Format("2006-01-02T15:04:05Z")
}

// PersistImportedSessionLocally saves an imported session and its
// materialized transcript, read back from its verified archive.
func PersistImportedSessionLocally(session *core.SessionRecord) error {
	panic("unported: mj-import/src/import/native_import.rs persist_imported_session_locally")
}

// ImportProjectRoot is the recorded project root used by both the path
// prompt and archive relocation.
//
// Git reports the roots as physical paths, while the transcript keeps the
// working directory as the harness saw it, which can run through a symlink
// (macOS keeps /var and /tmp behind /private). Both are compared, and the
// root is returned, as physical paths.
func ImportProjectRoot(transcript *ClaudeTranscript, targets *SessionEditTargets) string {
	cwd := canonicalizeExistingAncestor(transcript.Cwd)
	for _, root := range targets.GitRoots {
		if importPathStartsWith(cwd, root) {
			return root
		}
	}
	return cwd
}

// importProjectRemap relocates operational project metadata together;
// native history retains its provenance.
type importProjectRemap struct {
	recorded string
	// recordedPhysical is recorded as a physical path. Every containment
	// test is made there: the paths being moved come spelled as configured,
	// as Git reported them or as the harness recorded them, and one spelling
	// may run through a symlink another resolved (macOS keeps /var and /tmp
	// behind /private).
	recordedPhysical string
	destination      string
}

// newImportProjectRemap is Rust's `ImportProjectRemap::new`.
func newImportProjectRemap(recorded string, destination string, roots []string) (importProjectRemap, error) {
	panic("unported: mj-import/src/import/native_import.rs ImportProjectRemap::new")
}

// within is path relative to the recorded root; false when it lies outside
// it or is not a filesystem path (a remote URL).
func (r *importProjectRemap) within(path string) (string, bool) {
	panic("unported: mj-import/src/import/native_import.rs ImportProjectRemap::within")
}

func (r *importProjectRemap) apply(project *core.ProjectBundleSnapshot, repositories []checkpoint.RepositorySnapshot) error {
	panic("unported: mj-import/src/import/native_import.rs ImportProjectRemap::apply")
}

// nativeImportArtifactsMatch compares captured contents, independently of
// whether bodies live in memory or private snapshot files. Reader chunk
// boundaries need not agree.
func nativeImportArtifactsMatch(captured []core.NativeArtifact, current []core.NativeArtifact) (bool, error) {
	panic("unported: mj-import/src/import/native_import.rs native_import_artifacts_match")
}

// cancellableRead checks import cancellation before every read, so
// cancelling stops a copy of an arbitrarily large native file instead of
// waiting for it to finish. control is nil when the import cannot be
// cancelled.
type cancellableRead struct {
	inner   io.Reader
	control *ImportControl
}

func (r *cancellableRead) Read(buffer []byte) (int, error) {
	if r.control != nil {
		if err := r.control.checkCancelled(); err != nil {
			return 0, err
		}
	}
	return r.inner.Read(buffer)
}
