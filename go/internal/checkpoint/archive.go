// Port of mj-checkpoint/src/archive.rs.
//
// Versioned, verified checkpoint archives for Hel sessions.
//
// This module deliberately accepts native harness artifacts one file at a
// time. Harness adapters select the session's artifacts; the archive preserves
// their contents and filenames without guessing which names belong.

package checkpoint

import (
	"archive/zip"
	"bytes"
	"compress/flate"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/klauspost/compress/zstd"
)

const manifestPath = "manifest.json"
const canonicalSessionPath = "canonical/session.json"
const maxManifestBytes uint64 = 8 * 1024 * 1024

// payloadPartBytes: compressible payloads larger than this are split into
// parts of this size. DEFLATE is sequential inside one stream, so parts are
// what let both the writer and the reader use every core on a large payload.
const payloadPartBytes = 16 * 1024 * 1024
const payloadPartSuffix = ".helpart."

// Small ZIP metadata uses fast DEFLATE. Large archive payloads use
// Zstandard, whose independent shards keep both compression and
// decompression parallel.
const deflateLevel = 1
const zstdLevel = 1

// zipMethodZstd is the ZIP compression method number of Zstandard (APPNOTE
// 4.4.5); archive/zip only names Store and Deflate.
const zipMethodZstd uint16 = 93

type RepositorySnapshot struct {
	Metadata        core.RepositoryMetadata
	CommittedBundle []byte
	StagedPatch     []byte
	UnstagedPatch   []byte
	UntrackedTar    []byte
	// OperationPayloads is present exactly when Metadata.GitOperation is.
	OperationPayloads *GitOperationPayloads
	// LinkedWorktrees are linked worktrees inside the checkout, parents
	// before children.
	LinkedWorktrees []LinkedWorktreeSnapshot
}

// LinkedWorktreeSnapshot is what a linked worktree holds beyond the commits
// in the repository's bundle: the same dirty state the checkout itself
// captures.
type LinkedWorktreeSnapshot struct {
	Metadata      core.LinkedWorktreeMetadata
	StagedPatch   []byte
	UnstagedPatch []byte
	UntrackedTar  []byte
	// OperationPayloads is present exactly when Metadata.GitOperation is.
	OperationPayloads *GitOperationPayloads
}

// GitOperationPayloads are the files of an unfinished Git operation that are
// not index entries.
type GitOperationPayloads struct {
	// ConflictedWorktreeTar holds the working-tree files of the unmerged paths.
	ConflictedWorktreeTar []byte
	// StateTar holds the operation's state files, named as
	// `git rev-parse --git-path` names them.
	StateTar []byte
}

type ArchiveInput struct {
	Session          core.SessionManifest
	Target           core.TargetManifest
	Bundle           core.BundleManifest
	CanonicalSession core.CanonicalSessionSnapshot
	NativeArtifacts  []core.NativeArtifact
	Repositories     []RepositorySnapshot
}

// archiveInputView is an ArchiveInput whose parts the caller owns separately.
type archiveInputView struct {
	session          *core.SessionManifest
	target           *core.TargetManifest
	bundle           *core.BundleManifest
	canonicalSession *core.CanonicalSessionSnapshot
	nativeArtifacts  []core.NativeArtifact
	repositories     []RepositorySnapshot
}

// view is Rust's From<&ArchiveInput> for ArchiveInputView.
func (input *ArchiveInput) view() archiveInputView {
	return archiveInputView{
		session:          &input.Session,
		target:           &input.Target,
		bundle:           &input.Bundle,
		canonicalSession: &input.CanonicalSession,
		nativeArtifacts:  input.NativeArtifacts,
		repositories:     input.Repositories,
	}
}

type VerifiedArchive struct {
	Manifest core.ArchiveManifest
	// Payloads are payload bytes keyed by the exact manifest path.
	Payloads      map[string][]byte
	ArchiveSha256 string
}

func (a *VerifiedArchive) Payload(descriptor *core.PayloadDescriptor) ([]byte, error) {
	payload, ok := a.Payloads[descriptor.Path]
	if !ok {
		return nil, fmt.Errorf("verified payload '%s' is missing", descriptor.Path)
	}
	return payload, nil
}

func (a *VerifiedArchive) PayloadByRole(role *core.PayloadRole) ([]byte, error) {
	for index := range a.Manifest.Payloads {
		if a.Manifest.Payloads[index].Role == *role {
			return a.Payload(&a.Manifest.Payloads[index])
		}
	}
	return nil, fmt.Errorf("archive does not contain payload role %s", payloadRoleDebug(role))
}

func (a *VerifiedArchive) CanonicalSession() (core.CanonicalSessionSnapshot, error) {
	payload, err := a.PayloadByRole(&core.PayloadRole{Kind: core.PayloadRoleCanonicalSession})
	if err != nil {
		return core.CanonicalSessionSnapshot{}, err
	}
	var snapshot core.CanonicalSessionSnapshot
	if err := json.Unmarshal(payload, &snapshot); err != nil {
		return core.CanonicalSessionSnapshot{}, fmt.Errorf("parse canonical session snapshot: %w", err)
	}
	if err := snapshot.Validate(); err != nil {
		return core.CanonicalSessionSnapshot{}, err
	}
	return snapshot, nil
}

// VerifiedArchiveMetadata is fully verified archive metadata for callers that
// do not need to restore payload bodies. Verification streams repository and
// native payloads instead of retaining them, so memory is bounded by the
// manifest, canonical session, and ZIP read buffers rather than the archive's
// expanded size.
type VerifiedArchiveMetadata struct {
	Manifest         core.ArchiveManifest
	CanonicalSession core.CanonicalSessionSnapshot
	ArchiveSha256    string
}

// VerifiedRepositoryBundle is repository state retained by the resume source
// preflight. The rest of the archive is still streamed, hashed, and validated
// without being held in memory.
type VerifiedRepositoryBundle struct {
	Metadata        core.RepositoryMetadata
	CommittedBundle []byte
}

type VerifiedRepositoryBundles struct {
	ArchiveSha256 string
	// PrimaryRepository is the id, within Repositories, of the repository the
	// session's work is primarily in.
	PrimaryRepository string
	Repositories      []VerifiedRepositoryBundle
}

type CheckpointRepositoryBundle struct {
	Metadata        core.RepositoryMetadata
	CommittedBundle []byte
}

// CheckpointBundlePrerequisites reads the source commits a checkpoint bundle
// requires without asking Git to import its pack. Importing into an empty
// repository can make partial-clone Git versions lazily contact the archived
// origin before Hel has installed the configured source's credentials.
func CheckpointBundlePrerequisites(repository *CheckpointRepositoryBundle) ([]string, error) {
	bundle := repository.CommittedBundle
	if len(bundle) == 0 {
		return []string{repository.Metadata.HeadCommit}, nil
	}

	headerEnd := bytes.Index(bundle, []byte("\n\n"))
	if headerEnd < 0 {
		return nil, errors.New("checkpoint Git bundle has no header terminator")
	}
	if !bytes.HasPrefix(bundle[headerEnd+2:], []byte("PACK")) {
		return nil, errors.New("checkpoint Git bundle has no pack payload")
	}
	lines := bytes.Split(bundle[:headerEnd], []byte("\n"))
	version := lines[0]
	if string(version) != "# v2 git bundle" && string(version) != "# v3 git bundle" {
		return nil, errors.New("unsupported checkpoint Git bundle version")
	}

	prerequisites := []string{}
	var advertisedHead *string
	for _, line := range lines[1:] {
		if prerequisite, ok := bytes.CutPrefix(line, []byte("-")); ok {
			objectID, err := bundleHeaderObjectID(prerequisite, "prerequisite")
			if err != nil {
				return nil, err
			}
			prerequisites = append(prerequisites, objectID)
			continue
		}
		if bytes.HasPrefix(line, []byte("@")) {
			if string(version) != "# v3 git bundle" {
				return nil, errors.New("Git bundle v2 contains a capability")
			}
			continue
		}
		separator := bytes.IndexFunc(line, func(r rune) bool { return r < utf8.RuneSelf && isASCIIWhitespace(byte(r)) })
		if separator < 0 {
			return nil, errors.New("checkpoint Git bundle contains an invalid reference")
		}
		objectID, err := bundleHeaderObjectID(line[:separator], "reference")
		if err != nil {
			return nil, err
		}
		reference := bytes.TrimLeftFunc(line[separator:], func(r rune) bool { return r < utf8.RuneSelf && isASCIIWhitespace(byte(r)) })
		if string(reference) == "HEAD" {
			if advertisedHead != nil {
				return nil, errors.New("checkpoint Git bundle advertises HEAD more than once")
			}
			advertisedHead = &objectID
		}
	}
	if advertisedHead == nil {
		return nil, errors.New("checkpoint Git bundle does not advertise HEAD")
	}
	if !archiveEqualFoldASCII(*advertisedHead, repository.Metadata.HeadCommit) {
		return nil, errors.New("checkpoint Git bundle HEAD does not match checkpoint metadata")
	}
	return prerequisites, nil
}

func bundleHeaderObjectID(line []byte, kind string) (string, error) {
	// The first field: Rust's split on ASCII whitespace always yields one,
	// empty when the line starts with whitespace.
	objectID := line
	if end := bytes.IndexFunc(line, func(r rune) bool { return r < utf8.RuneSelf && isASCIIWhitespace(byte(r)) }); end >= 0 {
		objectID = line[:end]
	}
	valid := len(objectID) == 40 || len(objectID) == 64
	for _, b := range objectID {
		if !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f' || b >= 'A' && b <= 'F') {
			valid = false
		}
	}
	if !valid {
		return "", fmt.Errorf("checkpoint Git bundle contains an invalid %s object ID", kind)
	}
	return strings.ToLower(string(objectID)), nil
}

// isASCIIWhitespace is Rust's u8::is_ascii_whitespace: space, tab, line
// feed, form feed and carriage return (not vertical tab).
func isASCIIWhitespace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\x0c' || b == '\r'
}

// CloseVerification is a sealed interface over CloseVerificationVerified and
// CloseVerificationBlocked.
type CloseVerification interface {
	isCloseVerification()
	// TeardownAllowed is whether the target may be torn down: only after a
	// verified archive.
	TeardownAllowed() bool
}

type CloseVerificationVerified struct {
	ArchivePath   string
	ArchiveSha256 string
}

// CloseVerificationBlocked: the target must remain live and retryable while
// this result is blocked.
type CloseVerificationBlocked struct {
	Err error
}

func (CloseVerificationVerified) isCloseVerification() {}
func (CloseVerificationBlocked) isCloseVerification()  {}

func (CloseVerificationVerified) TeardownAllowed() bool { return true }
func (CloseVerificationBlocked) TeardownAllowed() bool  { return false }

// pendingPayload is one payload on its way into the archive: its bytes, or
// the file they are read from.
type pendingPayload struct {
	descriptor core.PayloadDescriptor
	data       []byte
	// source is the file the payload is read from; nil when data holds it.
	source *string
}

// WriteArchiveAtomic writes, fsyncs, atomically replaces, reopens, and
// verifies an archive.
//
// Success means it is safe for the caller's close state machine to tear down
// the target. Failure leaves an existing destination untouched whenever the
// failure occurs before the final same-directory rename.
func WriteArchiveAtomic(path string, input *ArchiveInput) (VerifiedArchiveMetadata, error) {
	if err := writeArchiveInstalled(path, input); err != nil {
		return VerifiedArchiveMetadata{}, err
	}
	verified, err := VerifyArchiveStreaming(path)
	if err != nil {
		return VerifiedArchiveMetadata{}, fmt.Errorf("verify newly written archive %s: %w", path, err)
	}
	return verified, nil
}

// WriteArchiveHashed writes and installs an archive exactly as
// WriteArchiveAtomic does, then hashes it in one sequential pass instead of
// structurally re-reading it.
//
// The checkpoint export path uses this: the target just wrote the ZIP from
// validated input, and the controller checks the downloaded bytes against the
// returned digest. Resume/import structurally verifies the archive when it is
// read. Callers without either check must keep using WriteArchiveAtomic.
func WriteArchiveHashed(path string, input *ArchiveInput) (string, error) {
	return writeArchiveHashedView(path, input.view())
}

func WriteArchiveHashedBorrowed(
	path string,
	session *core.SessionManifest,
	target *core.TargetManifest,
	bundle *core.BundleManifest,
	canonicalSession *core.CanonicalSessionSnapshot,
	nativeArtifacts []core.NativeArtifact,
	repositories []RepositorySnapshot,
) (string, error) {
	return writeArchiveHashedView(path, archiveInputView{
		session:          session,
		target:           target,
		bundle:           bundle,
		canonicalSession: canonicalSession,
		nativeArtifacts:  nativeArtifacts,
		repositories:     repositories,
	})
}

func writeArchiveHashedView(path string, input archiveInputView) (string, error) {
	if err := writeArchiveInstalledView(path, input, payloadPartBytes); err != nil {
		return "", err
	}
	file, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open %s: %w", path, err)
	}
	defer file.Close()
	digest, err := digestReader(file)
	if err != nil {
		return "", fmt.Errorf("hash newly written archive %s: %w", path, err)
	}
	return digest, nil
}

func writeArchiveInstalled(path string, input *ArchiveInput) error {
	return writeArchiveInstalledWithPartSize(path, input, payloadPartBytes)
}

func writeArchiveInstalledWithPartSize(path string, input *ArchiveInput, partBytes int) error {
	return writeArchiveInstalledView(path, input.view(), partBytes)
}

func writeArchiveInstalledView(path string, input archiveInputView, partBytes int) error {
	manifest, payloads, err := prepareArchiveViewWithPartSize(input, partBytes)
	if err != nil {
		return err
	}
	parent := filepath.Dir(path)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return fmt.Errorf("create archive directory %s: %w", parent, err)
	}

	temporary, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return fmt.Errorf("create temporary archive in %s: %w", parent, err)
	}
	// The temporary file is removed on every path that does not install it
	// (Rust's NamedTempFile drop).
	installed := false
	defer func() {
		if !installed {
			temporary.Close()
			os.Remove(temporary.Name())
		}
	}()
	if err := restrictArchivePermissions(temporary.Name()); err != nil {
		return err
	}
	if err := writeZip(temporary, parent, &manifest, payloads); err != nil {
		return err
	}
	if err := temporary.Sync(); err != nil {
		return fmt.Errorf("fsync temporary archive in %s: %w", parent, err)
	}
	// Close before the rename: Windows cannot replace a file through a
	// handle that is still open.
	if err := temporary.Close(); err != nil {
		return fmt.Errorf("close temporary archive in %s: %w", parent, err)
	}
	if err := os.Rename(temporary.Name(), path); err != nil {
		return fmt.Errorf("atomically replace %s: %w", path, err)
	}
	installed = true
	if err := restrictArchivePermissions(path); err != nil {
		return err
	}
	return core.SyncDirectory(parent)
}

func CheckpointForClose(path string, input *ArchiveInput) CloseVerification {
	verified, err := WriteArchiveAtomic(path, input)
	if err != nil {
		return CloseVerificationBlocked{Err: err}
	}
	return CloseVerificationVerified{ArchivePath: path, ArchiveSha256: verified.ArchiveSha256}
}

func ReadArchiveVerified(path string) (VerifiedArchive, error) {
	contents, err := readVerifiedZip(path, payloadRetentionAll)
	if err != nil {
		return VerifiedArchive{}, err
	}
	file, err := os.Open(path)
	if err != nil {
		return VerifiedArchive{}, fmt.Errorf("open %s: %w", path, err)
	}
	defer file.Close()
	digest, err := digestReader(file)
	if err != nil {
		return VerifiedArchive{}, err
	}
	return VerifiedArchive{Manifest: contents.manifest, Payloads: contents.payloads, ArchiveSha256: digest}, nil
}

// readArchiveForRestore retains control and repository state while native
// files remain in the ZIP.
func readArchiveForRestore(path string) (VerifiedArchive, error) {
	return readArchiveRetaining(path, payloadRetentionRestore)
}

func readArchiveRepositories(path string) (VerifiedArchive, error) {
	return readArchiveRetaining(path, payloadRetentionRepositories)
}

// readArchiveRetaining is the shared body of readArchiveForRestore and
// readArchiveRepositories, which open the archive for its digest without
// naming the path in the error.
func readArchiveRetaining(path string, retention payloadRetention) (VerifiedArchive, error) {
	contents, err := readVerifiedZip(path, retention)
	if err != nil {
		return VerifiedArchive{}, err
	}
	file, err := os.Open(path)
	if err != nil {
		return VerifiedArchive{}, err
	}
	defer file.Close()
	digest, err := digestReader(file)
	if err != nil {
		return VerifiedArchive{}, err
	}
	return VerifiedArchive{Manifest: contents.manifest, Payloads: contents.payloads, ArchiveSha256: digest}, nil
}

// CheckpointPayloadReader is an open checkpoint directory shared by all
// streamed payload copies. Each copy checks the descriptor's whole size and
// digest as it writes.
type CheckpointPayloadReader struct {
	archive *archiveZip
}

// CheckpointPayloadReaderOpen is Rust's CheckpointPayloadReader::open.
func CheckpointPayloadReaderOpen(path string) (*CheckpointPayloadReader, error) {
	archive, err := openArchive(path)
	if err != nil {
		return nil, err
	}
	return &CheckpointPayloadReader{archive: archive}, nil
}

func (r *CheckpointPayloadReader) CopyPayload(descriptor *core.PayloadDescriptor, writer io.Writer) error {
	return copyPayloadFromArchive(r.archive, descriptor, writer)
}

// Close releases the archive file. (Rust closes it when the reader drops.)
func (r *CheckpointPayloadReader) Close() error {
	return r.archive.close()
}

func copyPayloadFromArchive(archive *archiveZip, descriptor *core.PayloadDescriptor, writer io.Writer) error {
	output := &payloadDestination{writer: writer, digest: sha256.New()}
	copyEntry := func(name string) error {
		entry, err := archive.byName(name)
		if err != nil {
			return err
		}
		reader, err := entry.Open()
		if err != nil {
			return err
		}
		defer reader.Close()
		_, err = io.Copy(output, reader)
		return err
	}
	if len(descriptor.Parts) == 0 {
		if err := copyEntry(descriptor.Path); err != nil {
			return err
		}
	} else {
		for index := range descriptor.Parts {
			if err := copyEntry(descriptor.Parts[index].Path); err != nil {
				return err
			}
		}
	}
	if output.size != descriptor.Size {
		return errors.New("native payload size changed during restore")
	}
	if core.LowerHex(output.digest.Sum(nil)) != descriptor.Sha256 {
		return fmt.Errorf("SHA-256 mismatch for payload '%s'", descriptor.Path)
	}
	return nil
}

// payloadDestination digests and counts exactly the bytes its writer took.
type payloadDestination struct {
	writer io.Writer
	digest hash.Hash
	size   uint64
}

func (d *payloadDestination) Write(data []byte) (int, error) {
	count, err := d.writer.Write(data)
	d.digest.Write(data[:count])
	d.size += uint64(count)
	return count, err
}

// VerifyArchiveStreaming verifies an archive without materializing repository
// or native payloads. Every ZIP entry is still fully read, hashed, and
// checked; the Git tar payloads (untracked files, and an unfinished
// operation's files) are parsed through the same path-safety validators while
// streaming.
//
// A sharded tar is the one exception to streaming: its parts are held in
// memory long enough to reassemble and parse the tar, because tar safety is
// a property of the whole payload rather than of one part.
func VerifyArchiveStreaming(path string) (VerifiedArchiveMetadata, error) {
	return VerifyArchiveStreamingCancellable(path, new(atomic.Bool))
}

// VerifyArchiveStreamingCancellable is VerifyArchiveStreaming that stops when
// cancelled is set. Cancellation is checked on every physical archive read,
// including parallel payload readers.
func VerifyArchiveStreamingCancellable(path string, cancelled *atomic.Bool) (VerifiedArchiveMetadata, error) {
	contents, err := readVerifiedZipCancellable(path, payloadRetentionCanonicalOnly, cancelled)
	if err != nil {
		return VerifiedArchiveMetadata{}, err
	}
	file, err := os.Open(path)
	if err != nil {
		return VerifiedArchiveMetadata{}, fmt.Errorf("open %s: %w", path, err)
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return VerifiedArchiveMetadata{}, err
	}
	reader := &archiveFile{cancelled: cancelled, file: file, len: uint64(info.Size())}
	digest, err := digestReader(reader.reader())
	if err != nil {
		return VerifiedArchiveMetadata{}, err
	}
	return VerifiedArchiveMetadata{Manifest: contents.manifest, CanonicalSession: contents.canonicalSession, ArchiveSha256: digest}, nil
}

// VerifyRepositoryBundlesStreaming verifies an archive while retaining only
// its Git bundle payloads.
//
// Resume uses this before provisioning to prove that the configured sources
// still provide every prerequisite of each thin bundle. Native artifacts and
// patches can be very large, so they stay on the streaming path.
func VerifyRepositoryBundlesStreaming(path string) (VerifiedRepositoryBundles, error) {
	contents, err := readVerifiedZip(path, payloadRetentionCanonicalAndGitBundles)
	if err != nil {
		return VerifiedRepositoryBundles{}, err
	}
	repositories := make([]VerifiedRepositoryBundle, 0, len(contents.manifest.Repositories))
	for index := range contents.manifest.Repositories {
		repository := &contents.manifest.Repositories[index]
		descriptor, err := committedBundleDescriptor(&contents.manifest, repository)
		if err != nil {
			return VerifiedRepositoryBundles{}, err
		}
		committedBundle, ok := contents.payloads[descriptor.Path]
		if !ok {
			return VerifiedRepositoryBundles{}, fmt.Errorf(
				"verified committed bundle for repository %s was not retained",
				archiveDebugString(repository.Metadata.ID))
		}
		repositories = append(repositories, VerifiedRepositoryBundle{
			Metadata:        repository.Metadata,
			CommittedBundle: committedBundle,
		})
	}
	file, err := os.Open(path)
	if err != nil {
		return VerifiedRepositoryBundles{}, fmt.Errorf("open %s: %w", path, err)
	}
	defer file.Close()
	digest, err := digestReader(file)
	if err != nil {
		return VerifiedRepositoryBundles{}, err
	}
	return VerifiedRepositoryBundles{
		ArchiveSha256:     digest,
		PrimaryRepository: contents.manifest.Bundle.PrimaryRepository,
		Repositories:      repositories,
	}, nil
}

// committedBundleDescriptor is the descriptor of repository's committed
// bundle payload.
func committedBundleDescriptor(manifest *core.ArchiveManifest, repository *core.RepositoryManifest) (*core.PayloadDescriptor, error) {
	role := core.PayloadRole{Kind: core.PayloadRoleGitBundle, RepositoryID: repository.Metadata.ID}
	for index := range manifest.Payloads {
		if manifest.Payloads[index].Role == role {
			return &manifest.Payloads[index], nil
		}
	}
	return nil, fmt.Errorf("repository %s has no committed bundle payload", archiveDebugString(repository.Metadata.ID))
}

// ReadCheckpointRepositoryBundles reads only the repository bundles from a
// checkpoint whose complete archive digest was already persisted after full
// verification.
//
// Resume uses this narrow reader for its source-availability preflight. The
// restore path still performs full payload verification before changing any
// target state. This deliberately avoids re-hashing the archive or bundle:
// Git validates the pack during the preflight, and restore validates every
// payload before using it.
func ReadCheckpointRepositoryBundles(path string) ([]CheckpointRepositoryBundle, error) {
	archive, err := openArchive(path)
	if err != nil {
		return nil, err
	}
	defer archive.close()
	manifest, err := readCheckpointManifestFrom(archive)
	if err != nil {
		return nil, err
	}
	repositories := make([]CheckpointRepositoryBundle, 0, len(manifest.Repositories))
	for index := range manifest.Repositories {
		repository := &manifest.Repositories[index]
		descriptor, err := committedBundleDescriptor(&manifest, repository)
		if err != nil {
			return nil, err
		}
		committedBundle, err := readCheckpointPayload(archive, descriptor)
		if err != nil {
			return nil, err
		}
		repositories = append(repositories, CheckpointRepositoryBundle{
			Metadata:        repository.Metadata,
			CommittedBundle: committedBundle,
		})
	}
	return repositories, nil
}

// ReadCheckpointManifest reads the manifest of an archive that the caller has
// already verified.
func ReadCheckpointManifest(path string) (core.ArchiveManifest, error) {
	archive, err := openArchive(path)
	if err != nil {
		return core.ArchiveManifest{}, err
	}
	defer archive.close()
	return readCheckpointManifestFrom(archive)
}

// ReadVerifiedCheckpointPayload reads one payload from an archive whose
// manifest was already verified.
//
// The limit is checked before allocating or reading payload data. The entry
// sizes and the complete payload digest are checked again against the
// descriptor so a changed archive cannot rely on the earlier verification.
func ReadVerifiedCheckpointPayload(path string, descriptor *core.PayloadDescriptor, maxBytes uint64) ([]byte, error) {
	if descriptor.Size > maxBytes {
		return nil, fmt.Errorf("payload '%s' exceeds read limit of %d bytes", descriptor.Path, maxBytes)
	}
	if len(descriptor.Parts) > 0 {
		var covered uint64
		for index := range descriptor.Parts {
			part := &descriptor.Parts[index]
			if part.Size > maxBytes {
				return nil, fmt.Errorf("payload part '%s' exceeds read limit of %d bytes", part.Path, maxBytes)
			}
			if covered+part.Size < covered {
				return nil, errors.New("payload part sizes overflow")
			}
			covered += part.Size
			if covered > descriptor.Size {
				return nil, errors.New("payload parts exceed declared size")
			}
		}
		if covered != descriptor.Size {
			return nil, errors.New("payload parts do not cover declared size")
		}
	}

	archive, err := openArchive(path)
	if err != nil {
		return nil, err
	}
	defer archive.close()
	payload, err := readCheckpointPayload(archive, descriptor)
	if err != nil {
		return nil, err
	}
	if digestBytes(payload) != descriptor.Sha256 {
		return nil, fmt.Errorf("SHA-256 mismatch for payload '%s'", descriptor.Path)
	}
	return payload, nil
}

func readCheckpointManifestFrom(archive *archiveZip) (core.ArchiveManifest, error) {
	manifestBytes, err := readArchiveManifestBytes(archive)
	if err != nil {
		return core.ArchiveManifest{}, err
	}
	return parseArchiveManifest(manifestBytes)
}

// readArchiveManifestBytes reads the manifest entry, refusing a directory
// entry or one larger than maxManifestBytes before reading it.
func readArchiveManifestBytes(archive *archiveZip) ([]byte, error) {
	entry, err := archive.byName(manifestPath)
	if err != nil {
		return nil, fmt.Errorf("archive is missing %s: %w", manifestPath, err)
	}
	if zipIsDir(entry.Name) {
		return nil, errors.New("archive manifest is a directory entry")
	}
	if entry.UncompressedSize64 > maxManifestBytes {
		return nil, errors.New("archive manifest is too large")
	}
	manifestBytes, err := readZipEntry(entry, entry.UncompressedSize64)
	if err != nil {
		return nil, fmt.Errorf("read archive manifest: %w", err)
	}
	return manifestBytes, nil
}

// readZipEntry reads the whole of entry, with room for expectedSize bytes.
func readZipEntry(entry *zip.File, expectedSize uint64) ([]byte, error) {
	reader, err := entry.Open()
	if err != nil {
		return nil, err
	}
	defer reader.Close()
	buffer := bytes.NewBuffer(make([]byte, 0, expectedSize))
	if _, err := buffer.ReadFrom(reader); err != nil {
		return nil, err
	}
	return buffer.Bytes(), nil
}

func readCheckpointPayload(archive *archiveZip, descriptor *core.PayloadDescriptor) ([]byte, error) {
	if len(descriptor.Parts) == 0 {
		return readCheckpointEntry(archive, descriptor.Path, descriptor.Size, descriptor.Mode)
	}
	payload := make([]byte, 0, descriptor.Size)
	for index := range descriptor.Parts {
		part := &descriptor.Parts[index]
		body, err := readCheckpointEntry(archive, part.Path, part.Size, descriptor.Mode)
		if err != nil {
			return nil, err
		}
		payload = append(payload, body...)
	}
	if uint64(len(payload)) != descriptor.Size {
		return nil, fmt.Errorf("size mismatch for payload '%s'", descriptor.Path)
	}
	if digestBytes(payload) != descriptor.Sha256 {
		return nil, fmt.Errorf("SHA-256 mismatch for payload '%s'", descriptor.Path)
	}
	return payload, nil
}

func readCheckpointEntry(archive *archiveZip, path string, expectedSize uint64, expectedMode uint32) ([]byte, error) {
	entry, err := archive.byName(path)
	if err != nil {
		return nil, fmt.Errorf("archive is missing payload '%s': %w", path, err)
	}
	if zipIsDir(entry.Name) {
		return nil, fmt.Errorf("archive payload '%s' is a directory", path)
	}
	enclosed, ok := zipEnclosedName(entry.Name)
	if !ok {
		return nil, fmt.Errorf("unsafe ZIP entry path '%s'", entry.Name)
	}
	name, err := slashPath(enclosed)
	if err != nil {
		return nil, err
	}
	if name != path {
		return nil, fmt.Errorf("archive payload path does not match '%s'", path)
	}
	if entry.UncompressedSize64 != expectedSize {
		return nil, fmt.Errorf("size mismatch for payload '%s'", path)
	}
	if zipModeOrDefault(&entry.FileHeader) != expectedMode {
		return nil, fmt.Errorf("mode mismatch for payload '%s'", path)
	}
	body, err := readZipEntry(entry, expectedSize)
	if err != nil {
		return nil, fmt.Errorf("read ZIP entry '%s': %w", path, err)
	}
	if uint64(len(body)) != expectedSize {
		return nil, fmt.Errorf("size mismatch for payload '%s'", path)
	}
	return body, nil
}

func prepareArchiveViewWithPartSize(input archiveInputView, partBytes int) (core.ArchiveManifest, []pendingPayload, error) {
	if partBytes <= 0 {
		return core.ArchiveManifest{}, nil, errors.New("archive payload part size is zero")
	}
	if strings.TrimSpace(input.session.ID) == "" {
		return core.ArchiveManifest{}, nil, errors.New("session id is empty")
	}
	if strings.TrimSpace(input.bundle.ID) == "" {
		return core.ArchiveManifest{}, nil, errors.New("bundle id is empty")
	}
	if err := validateSecretFreeMap(input.target.Details); err != nil {
		return core.ArchiveManifest{}, nil, err
	}
	if err := input.canonicalSession.Validate(); err != nil {
		return core.ArchiveManifest{}, nil, err
	}

	var payloads []pendingPayload
	canonicalBytes, err := archiveSerdeJSON(input.canonicalSession)
	if err != nil {
		return core.ArchiveManifest{}, nil, fmt.Errorf("serialize canonical session snapshot: %w", err)
	}
	if err := pushPayload(&payloads, canonicalSessionPath, canonicalBytes, 0o600,
		core.PayloadRole{Kind: core.PayloadRoleCanonicalSession}); err != nil {
		return core.ArchiveManifest{}, nil, err
	}

	for index := range input.nativeArtifacts {
		artifact := &input.nativeArtifacts[index]
		if err := validateArchiveRelativePath(artifact.RelativePath); err != nil {
			return core.ArchiveManifest{}, nil, err
		}
		relative, err := slashPath(artifact.RelativePath)
		if err != nil {
			return core.ArchiveManifest{}, nil, err
		}
		var data []byte
		var source *string
		switch artifactData := artifact.Data.(type) {
		case core.NativeArtifactDataBytes:
			data = artifactData
		case core.NativeArtifactDataFile:
			path := string(artifactData)
			source = &path
		default:
			return core.ArchiveManifest{}, nil, errors.New("native artifact has no data")
		}
		if err := pushPayload(&payloads, "native/"+relative, data, artifact.Mode,
			core.PayloadRole{Kind: core.PayloadRoleNativeArtifact, RelativePath: artifact.RelativePath}); err != nil {
			return core.ArchiveManifest{}, nil, err
		}
		if source != nil {
			payload := &payloads[len(payloads)-1]
			payload.source = source
			size, err := artifact.Size()
			if err != nil {
				return core.ArchiveManifest{}, nil, err
			}
			payload.descriptor.Size = size
		}
	}

	repositoryIDs := map[string]struct{}{}
	repositories := make([]core.RepositoryManifest, 0, len(input.repositories))
	for index := range input.repositories {
		repository := &input.repositories[index]
		manifest, err := prepareRepository(&payloads, repository, repositoryIDs)
		if err != nil {
			return core.ArchiveManifest{}, nil, err
		}
		repositories = append(repositories, manifest)
	}

	outcomes := archiveParallel(len(payloads), func(index int) error {
		return digestPendingPayload(&payloads[index], partBytes)
	})
	if err := firstError(outcomes); err != nil {
		return core.ArchiveManifest{}, nil, err
	}

	paths := map[string]struct{}{}
	for index := range payloads {
		path := payloads[index].descriptor.Path
		if _, duplicate := paths[path]; duplicate {
			return core.ArchiveManifest{}, nil, fmt.Errorf("duplicate archive payload path '%s'", path)
		}
		paths[path] = struct{}{}
	}
	// The manifest owns its own descriptors, as Rust clones them: editing
	// the manifest must not change what the pending payloads write.
	descriptors := make([]core.PayloadDescriptor, 0, len(payloads))
	for index := range payloads {
		descriptor := payloads[index].descriptor
		descriptor.Parts = slices.Clone(descriptor.Parts)
		descriptors = append(descriptors, descriptor)
	}
	var schemaVersion uint32
	switch {
	case input.canonicalSession.Mailbox != nil:
		schemaVersion = core.ArchiveSchemaVersionMailbox
	case input.canonicalSession.NotificationStack != nil:
		schemaVersion = core.ArchiveSchemaVersionNotificationStack
	case carriesLinkedWorktrees(repositories):
		schemaVersion = core.ArchiveSchemaVersionLinkedWorktrees
	case carriesReviewRefs(repositories):
		schemaVersion = core.ArchiveSchemaVersionReviewRefs
	case carriesMessages(input.canonicalSession):
		schemaVersion = core.ArchiveSchemaVersionMessages
	case carriesCheckoutSubdirectory(repositories):
		schemaVersion = core.ArchiveSchemaVersionCheckoutSubdirectory
	case carriesAgentReports(descriptors):
		schemaVersion = core.ArchiveSchemaVersionAgentReports
	case carriesCloneRefs(repositories):
		schemaVersion = core.ArchiveSchemaVersionCloneRefs
	case input.canonicalSession.CurrentContextStart() > 0:
		schemaVersion = core.ArchiveSchemaVersionContext
	default:
		schemaVersion = expectedSchemaVersion(descriptors)
	}
	target := *input.target
	target.Details = maps.Clone(target.Details)
	manifest := core.ArchiveManifest{
		SchemaVersion: schemaVersion,
		Format:        core.ArchiveFormat,
		Session:       *input.session,
		Target:        target,
		Bundle:        *input.bundle,
		Repositories:  repositories,
		Payloads:      descriptors,
	}
	if err := validateManifest(&manifest); err != nil {
		return core.ArchiveManifest{}, nil, err
	}
	return manifest, payloads, nil
}

// prepareRepository validates one repository snapshot, pushes its payloads,
// and returns its manifest entry.
func prepareRepository(payloads *[]pendingPayload, repository *RepositorySnapshot, repositoryIDs map[string]struct{}) (core.RepositoryManifest, error) {
	metadata := &repository.Metadata
	if err := ValidateComponent(metadata.ID, "repository id"); err != nil {
		return core.RepositoryManifest{}, err
	}
	if _, duplicate := repositoryIDs[metadata.ID]; duplicate {
		return core.RepositoryManifest{}, fmt.Errorf("duplicate repository id '%s'", metadata.ID)
	}
	repositoryIDs[metadata.ID] = struct{}{}
	if err := validateArchiveRelativePath(metadata.RelativeDestination); err != nil {
		return core.RepositoryManifest{}, err
	}
	if metadata.CheckoutSubdirectory != nil {
		if err := validateArchiveRelativePath(*metadata.CheckoutSubdirectory); err != nil {
			return core.RepositoryManifest{}, err
		}
	}
	if originContainsCredentials(metadata.Origin) {
		return core.RepositoryManifest{}, fmt.Errorf("repository '%s' origin contains credentials", metadata.ID)
	}
	for _, pushURL := range metadata.PushURLs {
		if originContainsCredentials(pushURL) {
			return core.RepositoryManifest{}, fmt.Errorf("repository '%s' push URL contains credentials", metadata.ID)
		}
	}
	if metadata.RemoteWorkspace && metadata.BaseCommit == "" {
		return core.RepositoryManifest{}, fmt.Errorf("repository '%s' managed workspace has no base commit", metadata.ID)
	}
	if metadata.PlainDirectory {
		// Nothing about a plain folder is captured, so any Git state here
		// would be state a restore could not honestly reproduce.
		if metadata.RemoteWorkspace ||
			metadata.CheckoutSubdirectory != nil ||
			metadata.Origin != "" ||
			len(metadata.PushURLs) > 0 ||
			metadata.BaseCommit != "" ||
			metadata.HeadCommit != "" ||
			metadata.Branch != nil ||
			metadata.ReviewRefsMetadataOnly ||
			metadata.ReviewBaselineRef != nil ||
			len(metadata.SavedRefs) > 0 ||
			len(metadata.StashStack) > 0 ||
			metadata.GitOperation != nil ||
			len(repository.CommittedBundle) > 0 ||
			len(repository.StagedPatch) > 0 ||
			len(repository.UnstagedPatch) > 0 ||
			len(repository.UntrackedTar) > 0 ||
			repository.OperationPayloads != nil {
			return core.RepositoryManifest{}, fmt.Errorf("repository '%s' is a plain directory but carries Git state", metadata.ID)
		}
	}
	if err := validateUntrackedTar(repository.UntrackedTar); err != nil {
		return core.RepositoryManifest{}, err
	}
	root := "repositories/" + metadata.ID
	var operation *GitOperationPayloads
	switch {
	case metadata.GitOperation == nil && repository.OperationPayloads == nil:
	case metadata.GitOperation != nil && repository.OperationPayloads != nil:
		if err := validateSavedGitOperation(metadata.GitOperation); err != nil {
			return core.RepositoryManifest{}, err
		}
		if err := validateConflictedWorktreeTar(repository.OperationPayloads.ConflictedWorktreeTar); err != nil {
			return core.RepositoryManifest{}, err
		}
		if err := validateOperationStateTar(repository.OperationPayloads.StateTar); err != nil {
			return core.RepositoryManifest{}, err
		}
		operation = repository.OperationPayloads
	default:
		return core.RepositoryManifest{}, fmt.Errorf("repository '%s' Git operation metadata and payloads disagree", metadata.ID)
	}

	manifest := core.RepositoryManifest{
		Metadata:            *metadata,
		CommittedBundlePath: root + "/committed.bundle",
		StagedPatchPath:     root + "/staged.patch",
		UnstagedPatchPath:   root + "/unstaged.patch",
		UntrackedTarPath:    root + "/untracked.tar",
	}
	for _, payload := range []struct {
		path string
		data []byte
		kind core.PayloadRoleKind
	}{
		{manifest.CommittedBundlePath, repository.CommittedBundle, core.PayloadRoleGitBundle},
		{manifest.StagedPatchPath, repository.StagedPatch, core.PayloadRoleGitStagedPatch},
		{manifest.UnstagedPatchPath, repository.UnstagedPatch, core.PayloadRoleGitUnstagedPatch},
		{manifest.UntrackedTarPath, repository.UntrackedTar, core.PayloadRoleGitUntrackedTar},
	} {
		if err := pushPayload(payloads, payload.path, payload.data, 0o600,
			core.PayloadRole{Kind: payload.kind, RepositoryID: metadata.ID}); err != nil {
			return core.RepositoryManifest{}, err
		}
	}
	if operation != nil {
		worktreePath := root + "/conflicted-worktree.tar"
		statePath := root + "/operation-state.tar"
		if err := pushPayload(payloads, worktreePath, operation.ConflictedWorktreeTar, 0o600,
			core.PayloadRole{Kind: core.PayloadRoleGitConflictedWorktreeTar, RepositoryID: metadata.ID}); err != nil {
			return core.RepositoryManifest{}, err
		}
		if err := pushPayload(payloads, statePath, operation.StateTar, 0o600,
			core.PayloadRole{Kind: core.PayloadRoleGitOperationStateTar, RepositoryID: metadata.ID}); err != nil {
			return core.RepositoryManifest{}, err
		}
		manifest.ConflictedWorktreeTarPath = &worktreePath
		manifest.OperationStateTarPath = &statePath
	}

	worktreePaths := map[string]struct{}{}
	for index := range repository.LinkedWorktrees {
		worktree := &repository.LinkedWorktrees[index]
		if err := validateLinkedWorktreeMetadata(&worktree.Metadata); err != nil {
			return core.RepositoryManifest{}, fmt.Errorf("repository '%s': %w", metadata.ID, err)
		}
		key := archivePathKey(worktree.Metadata.Path)
		if _, duplicate := worktreePaths[key]; duplicate {
			return core.RepositoryManifest{}, fmt.Errorf("repository '%s' lists linked worktree '%s' twice",
				metadata.ID, worktree.Metadata.Path)
		}
		worktreePaths[key] = struct{}{}
		if err := validateUntrackedTar(worktree.UntrackedTar); err != nil {
			return core.RepositoryManifest{}, err
		}
		worktreeRoot := fmt.Sprintf("%s/worktrees/%08d", root, index)
		worktreeIndex := uint32(index)
		linked := core.LinkedWorktreeManifest{
			Metadata:          worktree.Metadata,
			StagedPatchPath:   worktreeRoot + "/staged.patch",
			UnstagedPatchPath: worktreeRoot + "/unstaged.patch",
			UntrackedTarPath:  worktreeRoot + "/untracked.tar",
		}
		for _, payload := range []struct {
			path string
			data []byte
			kind core.PayloadRoleKind
		}{
			{linked.StagedPatchPath, worktree.StagedPatch, core.PayloadRoleGitLinkedWorktreeStagedPatch},
			{linked.UnstagedPatchPath, worktree.UnstagedPatch, core.PayloadRoleGitLinkedWorktreeUnstagedPatch},
			{linked.UntrackedTarPath, worktree.UntrackedTar, core.PayloadRoleGitLinkedWorktreeUntrackedTar},
		} {
			if err := pushPayload(payloads, payload.path, payload.data, 0o600,
				core.PayloadRole{Kind: payload.kind, RepositoryID: metadata.ID, Worktree: worktreeIndex}); err != nil {
				return core.RepositoryManifest{}, err
			}
		}
		switch {
		case worktree.Metadata.GitOperation == nil && worktree.OperationPayloads == nil:
		case worktree.Metadata.GitOperation != nil && worktree.OperationPayloads != nil:
			operation := worktree.OperationPayloads
			if err := validateConflictedWorktreeTar(operation.ConflictedWorktreeTar); err != nil {
				return core.RepositoryManifest{}, err
			}
			if err := validateOperationStateTar(operation.StateTar); err != nil {
				return core.RepositoryManifest{}, err
			}
			worktreePath := worktreeRoot + "/conflicted-worktree.tar"
			statePath := worktreeRoot + "/operation-state.tar"
			if err := pushPayload(payloads, worktreePath, operation.ConflictedWorktreeTar, 0o600,
				core.PayloadRole{Kind: core.PayloadRoleGitLinkedWorktreeConflictedWorktreeTar, RepositoryID: metadata.ID, Worktree: worktreeIndex}); err != nil {
				return core.RepositoryManifest{}, err
			}
			if err := pushPayload(payloads, statePath, operation.StateTar, 0o600,
				core.PayloadRole{Kind: core.PayloadRoleGitLinkedWorktreeOperationStateTar, RepositoryID: metadata.ID, Worktree: worktreeIndex}); err != nil {
				return core.RepositoryManifest{}, err
			}
			linked.ConflictedWorktreeTarPath = &worktreePath
			linked.OperationStateTarPath = &statePath
		default:
			return core.RepositoryManifest{}, fmt.Errorf(
				"repository '%s' linked worktree '%s' Git operation metadata and payloads disagree",
				metadata.ID, worktree.Metadata.Path)
		}
		manifest.LinkedWorktrees = append(manifest.LinkedWorktrees, linked)
	}
	return manifest, nil
}

// digestPendingPayload fills in a payload's whole digest and, for a
// compressible payload over partBytes, its parts. A payload read from a file
// is read once, part by part.
func digestPendingPayload(payload *pendingPayload, partBytes int) error {
	if payload.source == nil {
		payload.descriptor.Sha256 = digestBytes(payload.data)
		payload.descriptor.Parts = planPayloadParts(payload.descriptor.Path, payload.data,
			payloadCompression(&payload.descriptor.Role), partBytes)
		return nil
	}
	source, err := os.Open(*payload.source)
	if err != nil {
		return err
	}
	defer source.Close()
	whole := sha256.New()
	buffer := make([]byte, partBytes)
	split := payloadCompression(&payload.descriptor.Role) != zip.Store && payload.descriptor.Size > uint64(partBytes)
	for index := 0; ; index++ {
		count, err := io.ReadFull(source, buffer)
		if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrUnexpectedEOF) {
			return err
		}
		if count == 0 {
			break
		}
		whole.Write(buffer[:count])
		if split {
			payload.descriptor.Parts = append(payload.descriptor.Parts, core.PayloadPartDescriptor{
				Path:   payloadPartPath(payload.descriptor.Path, index),
				Sha256: digestBytes(buffer[:count]),
				Size:   uint64(count),
			})
		}
	}
	payload.descriptor.Sha256 = core.LowerHex(whole.Sum(nil))
	return nil
}

// carriesMessages is whether a canonical session's transcript holds a
// mailbox message, which only schema 9 and later may carry.
func carriesMessages(snapshot *core.CanonicalSessionSnapshot) bool {
	for index := range snapshot.Transcript {
		if _, ok := snapshot.Transcript[index].Body.(core.CanonicalTranscriptBodyMessage); ok {
			return true
		}
	}
	return false
}

// carriesCloneRefs is whether an archive holds saved refs or a stash stack.
func carriesCloneRefs(repositories []core.RepositoryManifest) bool {
	return slices.ContainsFunc(repositories, func(repository core.RepositoryManifest) bool {
		return len(repository.Metadata.SavedRefs) > 0 || len(repository.Metadata.StashStack) > 0
	})
}

func pushPayload(payloads *[]pendingPayload, path string, data []byte, mode uint32, role core.PayloadRole) error {
	if err := validateArchiveRelativePath(path); err != nil {
		return err
	}
	normalized, err := normalizedMode(mode)
	if err != nil {
		return err
	}
	*payloads = append(*payloads, pendingPayload{
		descriptor: core.PayloadDescriptor{Path: path, Size: uint64(len(data)), Mode: normalized, Role: role},
		data:       data,
	})
	return nil
}

// payloadCompression: Git bundles carry packfiles whose objects are already
// compressed, so they are stored verbatim. Text and tar payloads compress
// well with Zstandard, which is substantially faster than DEFLATE for
// checkpoint-sized history.
func payloadCompression(role *core.PayloadRole) uint16 {
	if role.Kind == core.PayloadRoleGitBundle {
		return zip.Store
	}
	return zipMethodZstd
}

// tarPayloadValidator is the checker for the tar payloads whose entries are
// checked while an archive is verified; nil for every other payload.
func tarPayloadValidator(role *core.PayloadRole) func(io.Reader) error {
	switch role.Kind {
	case core.PayloadRoleGitUntrackedTar, core.PayloadRoleGitLinkedWorktreeUntrackedTar:
		return func(reader io.Reader) error { return validateUntrackedTarReader(reader) }
	case core.PayloadRoleGitConflictedWorktreeTar, core.PayloadRoleGitLinkedWorktreeConflictedWorktreeTar:
		return func(reader io.Reader) error { return validateConflictedWorktreeTarReader(reader) }
	case core.PayloadRoleGitOperationStateTar, core.PayloadRoleGitLinkedWorktreeOperationStateTar:
		return func(reader io.Reader) error { return validateOperationStateTarReader(reader) }
	}
	return nil
}

func payloadPartPath(path string, index int) string {
	return fmt.Sprintf("%s%s%05d", path, payloadPartSuffix, index)
}

// planPayloadParts splits a compressible payload that is larger than
// partBytes into ordered parts. Stored payloads keep one entry: they cost no
// compression time, and splitting them would only add entries a reader has
// to stitch back together.
func planPayloadParts(path string, data []byte, method uint16, partBytes int) []core.PayloadPartDescriptor {
	if method == zip.Store || len(data) <= partBytes {
		return nil
	}
	parts := make([]core.PayloadPartDescriptor, (len(data)+partBytes-1)/partBytes)
	archiveParallel(len(parts), func(index int) error {
		chunk := data[index*partBytes : min((index+1)*partBytes, len(data))]
		parts[index] = core.PayloadPartDescriptor{
			Path:   payloadPartPath(path, index),
			Sha256: digestBytes(chunk),
			Size:   uint64(len(chunk)),
		}
		return nil
	})
	return parts
}

// newArchiveZipWriter is a ZIP writer whose DEFLATE and Zstandard entries
// use the archive's compression levels.
func newArchiveZipWriter(output io.Writer) *zip.Writer {
	writer := zip.NewWriter(output)
	writer.RegisterCompressor(zip.Deflate, func(out io.Writer) (io.WriteCloser, error) {
		return flate.NewWriter(out, deflateLevel)
	})
	writer.RegisterCompressor(zipMethodZstd, func(out io.Writer) (io.WriteCloser, error) {
		// One encoder goroutine per entry: parallelism comes from entries,
		// as in Rust, and a synchronous encoder keeps the output a function
		// of the input alone.
		return zstd.NewWriter(out,
			zstd.WithEncoderLevel(zstd.EncoderLevelFromZstd(zstdLevel)),
			zstd.WithEncoderConcurrency(1))
	})
	return writer
}

// zipDefaultDate is 1980-01-01 in MS-DOS date form: the time the Rust zip
// crate stamps every entry with when built without its `time` feature, as
// this workspace builds it. Writing it keeps archives reproducible.
const zipDefaultDate uint16 = 1<<5 | 1

// archiveFileHeader is the header of a ZIP entry the archive writes: the
// given method and Unix mode, and the zip crate's default timestamp.
func archiveFileHeader(name string, method uint16, mode fs.FileMode) *zip.FileHeader {
	header := &zip.FileHeader{Name: name, Method: method, ModifiedDate: zipDefaultDate}
	header.SetMode(mode)
	return header
}

func writeZip(output *os.File, spoolParent string, manifest *core.ArchiveManifest, payloads []pendingPayload) error {
	manifestBytes, err := archiveSerdeJSONPretty(manifest)
	if err != nil {
		return fmt.Errorf("serialize archive manifest: %w", err)
	}
	if uint64(len(manifestBytes)) > maxManifestBytes {
		return errors.New("archive manifest is too large")
	}
	writer := newArchiveZipWriter(output)
	if err := writeStreamEntry(writer, manifestPath, 0o600, zip.Deflate, uint64(len(manifestBytes)),
		bytes.NewReader(manifestBytes), digestBytes(manifestBytes)); err != nil {
		return err
	}
	// os.MkdirTemp creates the spool readable by its owner only (0700).
	spool, err := os.MkdirTemp(spoolParent, ".tmp")
	if err != nil {
		return fmt.Errorf("create ZIP compression spool: %w", err)
	}
	defer os.RemoveAll(spool)
	var entries []streamEntry
	for index := range payloads {
		payload := &payloads[index]
		descriptor := &payload.descriptor
		if len(descriptor.Parts) == 0 {
			entries = append(entries, streamEntry{
				payload: payload,
				name:    descriptor.Path,
				size:    descriptor.Size,
				digest:  descriptor.Sha256,
			})
			continue
		}
		var offset uint64
		for partIndex := range descriptor.Parts {
			part := &descriptor.Parts[partIndex]
			entries = append(entries, streamEntry{
				payload: payload,
				name:    part.Path,
				size:    part.Size,
				digest:  part.Sha256,
				offset:  offset,
			})
			if offset+part.Size < offset {
				return errors.New("payload parts overflow")
			}
			offset += part.Size
		}
		if offset != descriptor.Size {
			return errors.New("payload parts do not cover its body")
		}
	}
	// Compress one window of entries per round, one entry per processor, as
	// Rust's rayon pool does. One spool file per active worker bounds scratch
	// disk to a worker-sized window instead of the entire archive.
	window := runtime.GOMAXPROCS(0)
	for start := 0; start < len(entries); start += window {
		current := entries[start:min(start+window, len(entries))]
		compressed := make([]*os.File, len(current))
		outcomes := archiveParallel(len(current), func(index int) error {
			entry := &current[index]
			// Stored Git bundles are already compressed and unsharded;
			// spooling them would duplicate a potentially large bundle.
			if payloadCompression(&entry.payload.descriptor.Role) == zip.Store {
				return nil
			}
			file, err := compressStreamEntry(entry, spool)
			if err != nil {
				return fmt.Errorf("compress ZIP entry '%s': %w", entry.name, err)
			}
			compressed[index] = file
			return nil
		})
		if err := firstError(outcomes); err != nil {
			discardCompressed(compressed)
			return err
		}
		if err := appendStreamEntries(writer, current, compressed); err != nil {
			return err
		}
	}

	if err := writer.Close(); err != nil {
		return fmt.Errorf("finish Mjolnir archive ZIP: %w", err)
	}
	return nil
}

// discardCompressed closes and removes the private ZIPs still held, as
// Rust drops the NamedTempFiles.
func discardCompressed(compressed []*os.File) {
	for index, file := range compressed {
		if file != nil {
			file.Close()
			os.Remove(file.Name())
			compressed[index] = nil
		}
	}
}

// appendStreamEntries: assembly owns payload completion. Workers may finish
// parts out of order, so only this ordered pass can check EOF after every
// part has completed. compressed[i] is the private single-entry ZIP holding
// entries[i] already compressed, or nil for a stored entry. Every private
// ZIP is closed and removed before this returns.
func appendStreamEntries(writer *zip.Writer, entries []streamEntry, compressed []*os.File) error {
	defer discardCompressed(compressed)
	for index := range entries {
		entry := &entries[index]
		if temporary := compressed[index]; temporary != nil {
			if err := copyCompressedEntry(writer, entry, temporary); err != nil {
				return err
			}
			discardCompressed(compressed[index : index+1])
		} else if err := writePayloadEntry(writer, entry); err != nil {
			return fmt.Errorf("write ZIP entry '%s': %w", entry.name, err)
		}
		if entry.offset+entry.size == entry.payload.descriptor.Size {
			if err := ensureSourceComplete(entry.payload); err != nil {
				return err
			}
		}
	}
	return nil
}

// copyCompressedEntry copies the one entry of a private ZIP into writer
// without recompressing it, keeping its codec output and CRC.
func copyCompressedEntry(writer *zip.Writer, entry *streamEntry, temporary *os.File) error {
	info, err := temporary.Stat()
	if err != nil {
		return fmt.Errorf("reopen compressed ZIP entry '%s': %w", entry.name, err)
	}
	source, err := zip.NewReader(temporary, info.Size())
	if err != nil {
		return fmt.Errorf("reopen compressed ZIP entry '%s': %w", entry.name, err)
	}
	if len(source.File) == 0 {
		return fmt.Errorf("read compressed ZIP entry '%s': %w", entry.name, errZipFileNotFound)
	}
	if err := writer.Copy(source.File[0]); err != nil {
		return fmt.Errorf("write ZIP entry '%s': %w", entry.name, err)
	}
	return nil
}

// ensureSourceComplete refuses a payload whose source has more bytes than
// its descriptor, which means it grew while the archive was written.
func ensureSourceComplete(payload *pendingPayload) error {
	source, err := payload.readerAt(payload.descriptor.Size)
	if err != nil {
		return fmt.Errorf("reopen completed payload '%s': %w", payload.descriptor.Path, err)
	}
	defer source.Close()
	var probe [1]byte
	read, err := source.Read(probe[:])
	if err != nil && !errors.Is(err, io.EOF) {
		return err
	}
	if read != 0 {
		return errors.New("payload source grew during export")
	}
	return nil
}

type streamEntry struct {
	payload *pendingPayload
	name    string
	size    uint64
	digest  string
	offset  uint64
}

// compressStreamEntry writes entry into a private single-entry ZIP in
// directory, so assembly can reuse the codec output and CRC through a raw
// copy without retaining compressed payloads in memory.
func compressStreamEntry(entry *streamEntry, directory string) (*os.File, error) {
	temporary, err := os.CreateTemp(directory, ".tmp")
	if err != nil {
		return nil, err
	}
	discard := func() {
		temporary.Close()
		os.Remove(temporary.Name())
	}
	writer := newArchiveZipWriter(temporary)
	if err := writePayloadEntry(writer, entry); err != nil {
		discard()
		return nil, err
	}
	if err := writer.Close(); err != nil {
		discard()
		return nil, fmt.Errorf("finish compressed ZIP entry: %w", err)
	}
	return temporary, nil
}

func (p *pendingPayload) readerAt(offset uint64) (io.ReadCloser, error) {
	if p.source == nil {
		return io.NopCloser(bytes.NewReader(p.data[min(offset, uint64(len(p.data))):])), nil
	}
	file, err := core.NativeArtifactDataOpenFile(*p.source)
	if err != nil {
		return nil, err
	}
	if _, err := file.Seek(int64(offset), io.SeekStart); err != nil {
		file.Close()
		return nil, err
	}
	return file, nil
}

func writePayloadEntry(writer *zip.Writer, entry *streamEntry) error {
	source, err := entry.payload.readerAt(entry.offset)
	if err != nil {
		return err
	}
	defer source.Close()
	descriptor := &entry.payload.descriptor
	return writeStreamEntry(writer, entry.name, descriptor.Mode, payloadCompression(&descriptor.Role),
		entry.size, io.LimitReader(source, int64(entry.size)), entry.digest)
}

func writeStreamEntry(writer *zip.Writer, name string, mode uint32, method uint16, size uint64, reader io.Reader, expectedDigest string) error {
	// archive/zip switches to ZIP64 by itself once an entry crosses the
	// 32-bit size threshold, which the zip crate has to be told up front.
	output, err := writer.CreateHeader(archiveFileHeader(name, method, fs.FileMode(mode)))
	if err != nil {
		return err
	}
	checked := newDigestingReader(reader)
	copied, err := io.Copy(output, checked)
	if err != nil {
		return err
	}
	if uint64(copied) != size {
		return errors.New("payload source size changed")
	}
	if _, digest := checked.finish(); digest != expectedDigest {
		return errors.New("payload source changed during export")
	}
	return nil
}

type payloadRetention int

const (
	payloadRetentionAll payloadRetention = iota
	payloadRetentionRestore
	payloadRetentionRepositories
	payloadRetentionCanonicalOnly
	payloadRetentionCanonicalAndGitBundles
)

type verifiedZipContents struct {
	manifest         core.ArchiveManifest
	canonicalSession core.CanonicalSessionSnapshot
	payloads         map[string][]byte
}

// zipEntryMeta is structural facts about one ZIP entry, read from the central
// directory before any body is decompressed.
type zipEntryMeta struct {
	index int
	name  string
	size  uint64
	mode  uint32
}

// entryExpectation is what the manifest says a ZIP entry must contain: the
// whole payload when part is nil, otherwise that part of it.
type entryExpectation struct {
	payload *core.PayloadDescriptor
	part    *core.PayloadPartDescriptor
}

// archiveZip is an opened checkpoint ZIP (Rust's Archive, a
// zip::ZipArchive<ArchiveFile>). Opening parses the ZIP's whole central
// directory once; Go's ZIP reader reads every entry at explicit offsets, so
// parallel readers share the parsed directory and the file without cloning.
type archiveZip struct {
	file   *archiveFile
	reader *zip.Reader
	// files are the entries by name in central-directory order, as the zip
	// crate indexes them: a name listed twice keeps its first position and
	// its last entry.
	files []*zip.File
	names map[string]int
}

func openArchive(path string) (*archiveZip, error) {
	return openArchiveCancellable(path, nil)
}

func openArchiveCancellable(path string, cancelled *atomic.Bool) (*archiveZip, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, err)
	}
	info, err := file.Stat()
	if err != nil {
		file.Close()
		return nil, fmt.Errorf("read the size of %s: %w", path, err)
	}
	archive := &archiveFile{cancelled: cancelled, file: file, len: uint64(info.Size())}
	reader, err := zip.NewReader(archive, int64(archive.len))
	// Entry names are checked by the archive's own path rules, so an
	// insecure name is reported there rather than refused here.
	if err != nil && !errors.Is(err, zip.ErrInsecurePath) {
		file.Close()
		return nil, fmt.Errorf("open Mjolnir archive ZIP: %w", err)
	}
	reader.RegisterDecompressor(zipMethodZstd, zstdDecompressor)
	opened := &archiveZip{file: archive, reader: reader, names: map[string]int{}}
	for _, entry := range reader.File {
		if index, ok := opened.names[entry.Name]; ok {
			opened.files[index] = entry
			continue
		}
		opened.names[entry.Name] = len(opened.files)
		opened.files = append(opened.files, entry)
	}
	return opened, nil
}

// zstdDecompressor reads a Zstandard ZIP entry. Parallelism comes from
// entries, so each decoder runs synchronously.
func zstdDecompressor(reader io.Reader) io.ReadCloser {
	decoder, err := zstd.NewReader(reader, zstd.WithDecoderConcurrency(1))
	if err != nil {
		return io.NopCloser(&failedReader{err: err})
	}
	return decoder.IOReadCloser()
}

// failedReader fails every read with err.
type failedReader struct{ err error }

func (r *failedReader) Read([]byte) (int, error) { return 0, r.err }

// errZipFileNotFound is the zip crate's ZipError::FileNotFound.
var errZipFileNotFound = errors.New("specified file not found in archive")

// byName is the entry named name (zip's ZipArchive::by_name).
func (a *archiveZip) byName(name string) (*zip.File, error) {
	index, ok := a.names[name]
	if !ok {
		return nil, errZipFileNotFound
	}
	return a.files[index], nil
}

func (a *archiveZip) close() error {
	return a.file.file.Close()
}

// archiveFile is a file read at explicit offsets, so readers share one open
// file and each keeps its own position (through io.SectionReader). A
// duplicated handle would share the position. Every read fails once
// cancelled is set.
type archiveFile struct {
	cancelled *atomic.Bool
	file      *os.File
	len       uint64
}

var errArchiveVerificationCancelled = errors.New("archive verification cancelled")

func (f *archiveFile) ReadAt(buffer []byte, offset int64) (int, error) {
	if f.cancelled != nil && f.cancelled.Load() {
		return 0, errArchiveVerificationCancelled
	}
	if offset < 0 {
		return 0, errors.New("seek before the start of the archive")
	}
	if uint64(offset) >= f.len {
		return 0, io.EOF
	}
	want := buffer[:min(uint64(len(buffer)), f.len-uint64(offset))]
	read, err := f.file.ReadAt(want, offset)
	if err == nil && read < len(buffer) {
		err = io.EOF
	}
	return read, err
}

// reader reads the whole file from its start with its own position.
func (f *archiveFile) reader() *io.SectionReader {
	return io.NewSectionReader(f, 0, int64(f.len))
}

// retainsPayload is whether the caller keeps a payload's body.
func retainsPayload(retention payloadRetention, role *core.PayloadRole) bool {
	if retention == payloadRetentionAll || role.Kind == core.PayloadRoleCanonicalSession {
		return true
	}
	switch retention {
	case payloadRetentionRepositories:
		return role.Kind != core.PayloadRoleNativeArtifact
	case payloadRetentionRestore:
		return role.Kind != core.PayloadRoleNativeArtifact ||
			archivePathStartsWith(role.RelativePath, core.ArchiveAttachmentDir) ||
			archivePathStartsWith(role.RelativePath, core.ArchiveReportDir) ||
			archivePathStartsWith(role.RelativePath, core.NativeArtifactRoot)
	case payloadRetentionCanonicalAndGitBundles:
		return role.Kind == core.PayloadRoleGitBundle
	}
	return false
}

// retainsParts is whether the reader has to hold a payload's part bodies
// until reassembly. Tar safety is a property of the whole payload, so a
// sharded tar is reassembled even when the caller does not want the bytes.
func retainsParts(retention payloadRetention, role *core.PayloadRole) bool {
	return retainsPayload(retention, role) || tarPayloadValidator(role) != nil
}

func readVerifiedZip(path string, retention payloadRetention) (verifiedZipContents, error) {
	return readVerifiedZipCancellable(path, retention, nil)
}

func readVerifiedZipCancellable(path string, retention payloadRetention, cancelled *atomic.Bool) (verifiedZipContents, error) {
	archive, err := openArchiveCancellable(path, cancelled)
	if err != nil {
		return verifiedZipContents{}, err
	}
	defer archive.close()
	entries := make([]zipEntryMeta, 0, len(archive.files))
	actualPaths := map[string]struct{}{}
	manifestCount := 0
	var totalSize uint64
	for index, entry := range archive.files {
		// The zip crate reads the local header while it indexes an entry.
		if _, err := entry.DataOffset(); err != nil {
			return verifiedZipContents{}, fmt.Errorf("read ZIP entry metadata %d: %w", index, err)
		}
		if zipIsDir(entry.Name) {
			return verifiedZipContents{}, fmt.Errorf(
				"archive contains directory entry '%s'; only files are allowed", entry.Name)
		}
		enclosed, ok := zipEnclosedName(entry.Name)
		if !ok {
			return verifiedZipContents{}, fmt.Errorf("unsafe ZIP entry path '%s'", entry.Name)
		}
		if err := validateArchiveRelativePath(enclosed); err != nil {
			return verifiedZipContents{}, err
		}
		if totalSize+entry.UncompressedSize64 < totalSize {
			return verifiedZipContents{}, errors.New("archive expanded size overflow")
		}
		totalSize += entry.UncompressedSize64
		name, err := slashPath(enclosed)
		if err != nil {
			return verifiedZipContents{}, err
		}
		if _, duplicate := actualPaths[name]; duplicate {
			return verifiedZipContents{}, fmt.Errorf("duplicate ZIP entry '%s'", name)
		}
		actualPaths[name] = struct{}{}
		if name == manifestPath {
			manifestCount++
		}
		entries = append(entries, zipEntryMeta{
			index: index,
			name:  name,
			size:  entry.UncompressedSize64,
			mode:  zipModeOrDefault(&entry.FileHeader),
		})
	}
	if manifestCount != 1 {
		return verifiedZipContents{}, fmt.Errorf("archive must contain exactly one %s", manifestPath)
	}
	manifestBytes, err := readArchiveManifestBytes(archive)
	if err != nil {
		return verifiedZipContents{}, err
	}
	manifest, err := parseArchiveManifest(manifestBytes)
	if err != nil {
		return verifiedZipContents{}, err
	}

	expectations := map[string]entryExpectation{}
	for index := range manifest.Payloads {
		descriptor := &manifest.Payloads[index]
		if len(descriptor.Parts) == 0 {
			expectations[descriptor.Path] = entryExpectation{payload: descriptor}
			continue
		}
		for partIndex := range descriptor.Parts {
			part := &descriptor.Parts[partIndex]
			expectations[part.Path] = entryExpectation{payload: descriptor, part: part}
		}
	}

	type payloadEntry struct {
		meta        *zipEntryMeta
		expectation entryExpectation
	}
	payloadEntries := make([]payloadEntry, 0, len(entries))
	for index := range entries {
		meta := &entries[index]
		if meta.name == manifestPath {
			if meta.size != uint64(len(manifestBytes)) {
				return verifiedZipContents{}, errors.New("archive manifest size changed while it was read")
			}
			continue
		}
		expectation, ok := expectations[meta.name]
		if !ok {
			return verifiedZipContents{}, fmt.Errorf("archive contains unlisted payload '%s'", meta.name)
		}
		payloadEntries = append(payloadEntries, payloadEntry{meta, expectation})
	}
	// Entry names are unique and every one of them is listed, so equal counts
	// mean the manifest and the container describe the same entry set.
	if len(payloadEntries) != len(expectations) {
		return verifiedZipContents{}, errors.New("archive payload list does not match manifest")
	}

	// DEFLATE and Zstandard streams cannot be inflated in parallel inside one
	// entry, so read parallelism comes from entries: every worker reads
	// through the one parsed directory at its own offsets and verifies whole
	// entries end to end.
	bodies := map[string][]byte{}
	retained := make([][]byte, len(payloadEntries))
	kept := make([]bool, len(payloadEntries))
	outcomes := archiveParallel(len(payloadEntries), func(index int) error {
		body, ok, err := readVerifiedEntry(archive, payloadEntries[index].meta, payloadEntries[index].expectation, retention)
		retained[index], kept[index] = body, ok
		return err
	})
	if err := firstError(outcomes); err != nil {
		return verifiedZipContents{}, err
	}
	for index := range payloadEntries {
		if kept[index] {
			bodies[payloadEntries[index].meta.name] = retained[index]
		}
	}

	payloads := map[string][]byte{}
	var canonicalSession *core.CanonicalSessionSnapshot
	for index := range manifest.Payloads {
		descriptor := &manifest.Payloads[index]
		// Per-part hashes do not prove the whole-payload digest. Verify
		// omitted sharded bodies in order before returning any trusted
		// archive state.
		if len(descriptor.Parts) > 0 && !retainsParts(retention, &descriptor.Role) {
			if err := copyPayloadFromArchive(archive, descriptor, io.Discard); err != nil {
				return verifiedZipContents{}, err
			}
		}
		body, ok, err := reassemblePayload(descriptor, bodies, retention)
		if err != nil {
			return verifiedZipContents{}, err
		}
		if validate := tarPayloadValidator(&descriptor.Role); validate != nil && ok {
			if err := validate(bytes.NewReader(body)); err != nil {
				return verifiedZipContents{}, fmt.Errorf("validate payload '%s': %w", descriptor.Path, err)
			}
		}
		if descriptor.Role.Kind == core.PayloadRoleCanonicalSession {
			if !ok {
				panic("canonical session payload is always retained")
			}
			var snapshot core.CanonicalSessionSnapshot
			if err := json.Unmarshal(body, &snapshot); err != nil {
				return verifiedZipContents{}, fmt.Errorf("parse canonical session snapshot: %w", err)
			}
			if err := snapshot.Validate(); err != nil {
				return verifiedZipContents{}, err
			}
			if manifest.SchemaVersion < core.ArchiveSchemaVersionMessages && carriesMessages(&snapshot) {
				return verifiedZipContents{}, fmt.Errorf("message transcript entries require archive schema %d",
					core.ArchiveSchemaVersionMessages)
			}
			if manifest.SchemaVersion < core.ArchiveSchemaVersionNotificationStack && snapshot.NotificationStack != nil {
				return verifiedZipContents{}, fmt.Errorf("notification stack requires archive schema %d",
					core.ArchiveSchemaVersionNotificationStack)
			}
			if manifest.SchemaVersion < core.ArchiveSchemaVersionMailbox && snapshot.Mailbox != nil {
				return verifiedZipContents{}, fmt.Errorf("archived mailbox requires archive schema %d",
					core.ArchiveSchemaVersionMailbox)
			}
			if canonicalSession != nil {
				return verifiedZipContents{}, errors.New("archive contains duplicate canonical session payloads")
			}
			canonicalSession = &snapshot
		}
		if retainsPayload(retention, &descriptor.Role) {
			if !ok {
				panic("selected payloads are retained by the reader")
			}
			payloads[descriptor.Path] = body
		}
	}
	if canonicalSession == nil {
		return verifiedZipContents{}, errors.New("archive canonical session is missing")
	}
	return verifiedZipContents{manifest: manifest, canonicalSession: *canonicalSession, payloads: payloads}, nil
}

// readVerifiedEntry reads one ZIP entry, verifies it against the manifest,
// and returns its body (with true) when the caller needs it.
func readVerifiedEntry(archive *archiveZip, meta *zipEntryMeta, expectation entryExpectation, retention payloadRetention) ([]byte, bool, error) {
	payload := expectation.payload
	expectedSize, expectedSha256 := payload.Size, payload.Sha256
	retain := retainsPayload(retention, &payload.Role)
	label := fmt.Sprintf("payload '%s'", payload.Path)
	if expectation.part != nil {
		expectedSize, expectedSha256 = expectation.part.Size, expectation.part.Sha256
		retain = retainsParts(retention, &payload.Role)
		label = fmt.Sprintf("payload part '%s'", meta.name)
	}
	if meta.size != expectedSize {
		return nil, false, fmt.Errorf("size mismatch for %s", label)
	}
	if meta.mode != payload.Mode {
		return nil, false, fmt.Errorf("mode mismatch for %s", label)
	}
	// Only a whole tar can be parsed while it streams past; sharded ones are
	// reassembled after every part is verified.
	var streamTar func(io.Reader) error
	if !retain && expectation.part == nil {
		streamTar = tarPayloadValidator(&payload.Role)
	}

	name := meta.name
	if meta.index >= len(archive.files) {
		return nil, false, fmt.Errorf("read ZIP entry %d: %w", meta.index, errZipFileNotFound)
	}
	entry := archive.files[meta.index]
	// Confirm this handle still sees the entry the structural pass indexed
	// instead of reporting a replaced file as payload corruption.
	enclosed, ok := zipEnclosedName(entry.Name)
	if !ok {
		return nil, false, fmt.Errorf("unsafe ZIP entry path '%s'", entry.Name)
	}
	entryName, err := slashPath(enclosed)
	if err != nil {
		return nil, false, err
	}
	if entryName != meta.name {
		return nil, false, fmt.Errorf("ZIP entry %d changed while the archive was read", meta.index)
	}
	reader, err := entry.Open()
	if err != nil {
		return nil, false, fmt.Errorf("read ZIP entry %d: %w", meta.index, err)
	}
	defer reader.Close()
	digesting := newDigestingReader(reader)
	var body []byte
	switch {
	case retain:
		buffer := bytes.NewBuffer(make([]byte, 0, expectedSize))
		if _, err := buffer.ReadFrom(digesting); err != nil {
			return nil, false, fmt.Errorf("read ZIP entry '%s': %w", name, err)
		}
		body = buffer.Bytes()
	case streamTar != nil:
		if err := streamTar(digesting); err != nil {
			return nil, false, fmt.Errorf("validate payload '%s': %w", payload.Path, err)
		}
		if _, err := io.Copy(io.Discard, digesting); err != nil {
			return nil, false, fmt.Errorf("finish reading ZIP entry '%s': %w", name, err)
		}
	default:
		if _, err := io.Copy(io.Discard, digesting); err != nil {
			return nil, false, fmt.Errorf("read ZIP entry '%s': %w", name, err)
		}
	}
	actualSize, actualDigest := digesting.finish()
	if actualSize != expectedSize {
		return nil, false, fmt.Errorf("size mismatch for %s", label)
	}
	if actualDigest != expectedSha256 {
		return nil, false, fmt.Errorf("SHA-256 mismatch for %s", label)
	}
	return body, retain, nil
}

// reassemblePayload turns verified entry bodies back into one payload body
// (with true when there is one). Restore and import therefore never see part
// entries, whatever the archive layout is.
func reassemblePayload(descriptor *core.PayloadDescriptor, bodies map[string][]byte, retention payloadRetention) ([]byte, bool, error) {
	if len(descriptor.Parts) == 0 {
		body, ok := bodies[descriptor.Path]
		delete(bodies, descriptor.Path)
		return body, ok, nil
	}
	if !retainsParts(retention, &descriptor.Role) {
		return nil, false, nil
	}
	assembled := make([]byte, 0, descriptor.Size)
	for index := range descriptor.Parts {
		part := &descriptor.Parts[index]
		chunk, ok := bodies[part.Path]
		if !ok {
			return nil, false, fmt.Errorf("payload '%s' is missing part '%s'", descriptor.Path, part.Path)
		}
		delete(bodies, part.Path)
		assembled = append(assembled, chunk...)
	}
	if uint64(len(assembled)) != descriptor.Size {
		return nil, false, fmt.Errorf("size mismatch for payload '%s'", descriptor.Path)
	}
	if digestBytes(assembled) != descriptor.Sha256 {
		return nil, false, fmt.Errorf("SHA-256 mismatch for payload '%s'", descriptor.Path)
	}
	return assembled, true, nil
}

type digestingReader struct {
	inner     io.Reader
	digest    hash.Hash
	bytesRead uint64
}

func newDigestingReader(inner io.Reader) *digestingReader {
	return &digestingReader{inner: inner, digest: sha256.New()}
}

func (r *digestingReader) finish() (uint64, string) {
	return r.bytesRead, core.LowerHex(r.digest.Sum(nil))
}

func (r *digestingReader) Read(buffer []byte) (int, error) {
	read, err := r.inner.Read(buffer)
	r.bytesRead += uint64(read)
	r.digest.Write(buffer[:read])
	return read, err
}

func parseArchiveManifest(manifestBytes []byte) (core.ArchiveManifest, error) {
	var header struct {
		SchemaVersion uint32 `json:"schema_version"`
		Format        string `json:"format"`
	}
	if err := fetchDecodeRequired(manifestBytes, &header, "schema_version", "format"); err != nil {
		return core.ArchiveManifest{}, fmt.Errorf("parse archive manifest header: %w", err)
	}
	if header.Format != core.ArchiveFormat {
		return core.ArchiveManifest{}, fmt.Errorf("unsupported archive format '%s'", header.Format)
	}
	switch header.SchemaVersion {
	case core.ArchiveSchemaVersion,
		core.ArchiveSchemaVersionSharded,
		core.ArchiveSchemaVersionAttachments,
		core.ArchiveSchemaVersionContext,
		core.ArchiveSchemaVersionCloneRefs,
		core.ArchiveSchemaVersionAgentReports,
		core.ArchiveSchemaVersionCheckoutSubdirectory,
		core.ArchiveSchemaVersionMessages,
		core.ArchiveSchemaVersionReviewRefs,
		core.ArchiveSchemaVersionLinkedWorktrees,
		core.ArchiveSchemaVersionNotificationStack,
		core.ArchiveSchemaVersionMailbox:
	default:
		return core.ArchiveManifest{}, fmt.Errorf("incompatible Mjolnir archive schema %d; this build requires schema %d",
			header.SchemaVersion, core.ArchiveSchemaVersion)
	}
	var manifest core.ArchiveManifest
	if err := json.Unmarshal(manifestBytes, &manifest); err != nil {
		return core.ArchiveManifest{}, fmt.Errorf("parse Mjolnir archive manifest: %w", err)
	}
	if err := validateManifest(&manifest); err != nil {
		return core.ArchiveManifest{}, err
	}
	return manifest, nil
}

// expectedSchemaVersion is the schema an archive must declare for the
// payload layout it carries. Schema 3 adds sharding; schema 4 adds
// relay-owned attachments. Older readers must reject either extension instead
// of silently restoring it wrong.
func expectedSchemaVersion(payloads []core.PayloadDescriptor) uint32 {
	for index := range payloads {
		role := &payloads[index].Role
		if role.Kind == core.PayloadRoleNativeArtifact && archivePathStartsWith(role.RelativePath, core.ArchiveAttachmentDir) {
			return core.ArchiveSchemaVersionAttachments
		}
	}
	for index := range payloads {
		if len(payloads[index].Parts) > 0 {
			return core.ArchiveSchemaVersionSharded
		}
	}
	return core.ArchiveSchemaVersion
}

// carriesAgentReports is whether an archive carries sub-agent report files,
// which only a build that knows to restore them beside the repositories may
// read.
func carriesAgentReports(payloads []core.PayloadDescriptor) bool {
	return slices.ContainsFunc(payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleNativeArtifact &&
			archivePathStartsWith(payload.Role.RelativePath, core.ArchiveReportDir)
	})
}

// carriesCheckoutSubdirectory is whether an archive holds a repository
// captured from a subdirectory of its checkout, which only a build that
// restores from the top level may read.
func carriesCheckoutSubdirectory(repositories []core.RepositoryManifest) bool {
	return slices.ContainsFunc(repositories, func(repository core.RepositoryManifest) bool {
		return repository.Metadata.CheckoutSubdirectory != nil
	})
}

// carriesLinkedWorktrees is whether an archive holds a linked worktree, which
// only a build that restores them may read.
func carriesLinkedWorktrees(repositories []core.RepositoryManifest) bool {
	return slices.ContainsFunc(repositories, func(repository core.RepositoryManifest) bool {
		return len(repository.LinkedWorktrees) > 0
	})
}

func carriesReviewRefs(repositories []core.RepositoryManifest) bool {
	return slices.ContainsFunc(repositories, func(repository core.RepositoryManifest) bool {
		metadata := &repository.Metadata
		if metadata.ReviewRefsMetadataOnly || metadata.ReviewBaselineRef != nil {
			return true
		}
		for name := range metadata.SavedRefs {
			if strings.HasPrefix(name, core.MjolnirRefNamespace) {
				return true
			}
		}
		return false
	})
}

func validateManifest(manifest *core.ArchiveManifest) error {
	var expectedSchema uint32
	switch {
	case carriesLinkedWorktrees(manifest.Repositories):
		expectedSchema = core.ArchiveSchemaVersionLinkedWorktrees
	case carriesReviewRefs(manifest.Repositories):
		expectedSchema = core.ArchiveSchemaVersionReviewRefs
	case carriesCheckoutSubdirectory(manifest.Repositories):
		expectedSchema = core.ArchiveSchemaVersionCheckoutSubdirectory
	case carriesAgentReports(manifest.Payloads):
		expectedSchema = core.ArchiveSchemaVersionAgentReports
	case carriesCloneRefs(manifest.Repositories):
		expectedSchema = core.ArchiveSchemaVersionCloneRefs
	default:
		expectedSchema = expectedSchemaVersion(manifest.Payloads)
	}
	schema := manifest.SchemaVersion
	if !(schema == expectedSchema ||
		schema == core.ArchiveSchemaVersionMailbox ||
		schema == core.ArchiveSchemaVersionNotificationStack ||
		schema == core.ArchiveSchemaVersionLinkedWorktrees ||
		(schema == core.ArchiveSchemaVersionReviewRefs &&
			expectedSchema != core.ArchiveSchemaVersionLinkedWorktrees) ||
		(schema == core.ArchiveSchemaVersionMessages &&
			expectedSchema != core.ArchiveSchemaVersionReviewRefs &&
			expectedSchema != core.ArchiveSchemaVersionLinkedWorktrees) ||
		(expectedSchema != core.ArchiveSchemaVersionLinkedWorktrees &&
			expectedSchema != core.ArchiveSchemaVersionReviewRefs &&
			expectedSchema != core.ArchiveSchemaVersionCloneRefs &&
			expectedSchema != core.ArchiveSchemaVersionAgentReports &&
			expectedSchema != core.ArchiveSchemaVersionCheckoutSubdirectory &&
			schema == core.ArchiveSchemaVersionContext)) {
		return fmt.Errorf("incompatible Mjolnir archive schema %d; this build requires schema %d", schema, expectedSchema)
	}
	if manifest.Format != core.ArchiveFormat {
		return fmt.Errorf("unsupported archive format '%s'", manifest.Format)
	}
	if strings.TrimSpace(manifest.Session.ID) == "" {
		return errors.New("manifest session id is empty")
	}
	if err := validateSecretFreeMap(manifest.Target.Details); err != nil {
		return err
	}
	for index := range manifest.Repositories {
		if reference := manifest.Repositories[index].Metadata.ReviewBaselineRef; reference != nil &&
			!core.IsReviewBaselinePin(*reference) {
			return fmt.Errorf("invalid source review baseline ref: %s", *reference)
		}
	}
	paths := map[string]struct{}{}
	for index := range manifest.Payloads {
		descriptor := &manifest.Payloads[index]
		if err := validateArchiveRelativePath(descriptor.Path); err != nil {
			return err
		}
		if descriptor.Path == manifestPath {
			return errors.New("manifest cannot describe itself as a payload")
		}
		if _, duplicate := paths[descriptor.Path]; duplicate {
			return fmt.Errorf("duplicate manifest payload '%s'", descriptor.Path)
		}
		paths[descriptor.Path] = struct{}{}
		if _, err := normalizedMode(descriptor.Mode); err != nil {
			return err
		}
		if !core.IsLowerHexSha256(descriptor.Sha256) {
			return fmt.Errorf("invalid SHA-256 for payload '%s'", descriptor.Path)
		}
		if err := validatePayloadParts(descriptor, paths); err != nil {
			return err
		}
		if descriptor.Role.Kind == core.PayloadRoleNativeArtifact {
			relativePath := descriptor.Role.RelativePath
			if err := validateArchiveRelativePath(relativePath); err != nil {
				return err
			}
			relative, err := slashPath(relativePath)
			if err != nil {
				return err
			}
			if descriptor.Path != "native/"+relative {
				return errors.New("native artifact path does not match its role")
			}
		}
	}
	canonicalCount := 0
	canonicalAtPath := false
	for index := range manifest.Payloads {
		if manifest.Payloads[index].Role.Kind == core.PayloadRoleCanonicalSession {
			canonicalCount++
			canonicalAtPath = canonicalAtPath || manifest.Payloads[index].Path == canonicalSessionPath
		}
	}
	if canonicalCount != 1 {
		return errors.New("archive must contain exactly one canonical session payload")
	}
	if !canonicalAtPath {
		return fmt.Errorf("canonical session payload must be stored at %s", canonicalSessionPath)
	}

	repositoryIDs := map[string]struct{}{}
	for index := range manifest.Repositories {
		if err := validateManifestRepository(manifest, &manifest.Repositories[index], repositoryIDs); err != nil {
			return err
		}
	}
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		role := &payload.Role
		switch role.Kind {
		case core.PayloadRoleCanonicalSession, core.PayloadRoleNativeArtifact:
			continue
		case core.PayloadRoleGitConflictedWorktreeTar, core.PayloadRoleGitOperationStateTar:
			recorded := slices.ContainsFunc(manifest.Repositories, func(repository core.RepositoryManifest) bool {
				return repository.Metadata.ID == role.RepositoryID && repository.Metadata.GitOperation != nil
			})
			if !recorded {
				return fmt.Errorf("payload '%s' holds a Git operation repository '%s' does not record",
					payload.Path, role.RepositoryID)
			}
		case core.PayloadRoleGitLinkedWorktreeStagedPatch, core.PayloadRoleGitLinkedWorktreeUnstagedPatch,
			core.PayloadRoleGitLinkedWorktreeUntrackedTar, core.PayloadRoleGitLinkedWorktreeConflictedWorktreeTar,
			core.PayloadRoleGitLinkedWorktreeOperationStateTar:
			if err := ensureLinkedWorktreeExists(manifest, payload, role.RepositoryID, role.Worktree); err != nil {
				return err
			}
		}
		if _, ok := repositoryIDs[role.RepositoryID]; !ok {
			return fmt.Errorf("payload '%s' refers to unknown repository '%s'", payload.Path, role.RepositoryID)
		}
	}
	return nil
}

// validateManifestRepository checks one repository's manifest entry and that
// every payload it names is listed with its role.
func validateManifestRepository(manifest *core.ArchiveManifest, repository *core.RepositoryManifest, repositoryIDs map[string]struct{}) error {
	metadata := &repository.Metadata
	if err := ValidateComponent(metadata.ID, "repository id"); err != nil {
		return err
	}
	if _, duplicate := repositoryIDs[metadata.ID]; duplicate {
		return fmt.Errorf("duplicate repository id '%s'", metadata.ID)
	}
	repositoryIDs[metadata.ID] = struct{}{}
	if err := validateArchiveRelativePath(metadata.RelativeDestination); err != nil {
		return err
	}
	if metadata.CheckoutSubdirectory != nil {
		if err := validateArchiveRelativePath(*metadata.CheckoutSubdirectory); err != nil {
			return err
		}
	}
	if originContainsCredentials(metadata.Origin) {
		return fmt.Errorf("repository '%s' origin contains credentials", metadata.ID)
	}
	for _, pushURL := range metadata.PushURLs {
		if originContainsCredentials(pushURL) {
			return fmt.Errorf("repository '%s' push URL contains credentials", metadata.ID)
		}
	}
	if metadata.RemoteWorkspace && metadata.BaseCommit == "" {
		return fmt.Errorf("repository '%s' managed workspace has no base commit", metadata.ID)
	}
	type expectedPayload struct {
		path string
		role core.PayloadRole
	}
	role := func(kind core.PayloadRoleKind) core.PayloadRole {
		return core.PayloadRole{Kind: kind, RepositoryID: metadata.ID}
	}
	expected := []expectedPayload{
		{repository.CommittedBundlePath, role(core.PayloadRoleGitBundle)},
		{repository.StagedPatchPath, role(core.PayloadRoleGitStagedPatch)},
		{repository.UnstagedPatchPath, role(core.PayloadRoleGitUnstagedPatch)},
		{repository.UntrackedTarPath, role(core.PayloadRoleGitUntrackedTar)},
	}
	worktreePath, statePath := repository.ConflictedWorktreeTarPath, repository.OperationStateTarPath
	switch {
	case metadata.GitOperation == nil && worktreePath == nil && statePath == nil:
	case metadata.GitOperation != nil && worktreePath != nil && statePath != nil:
		if err := validateSavedGitOperation(metadata.GitOperation); err != nil {
			return fmt.Errorf("repository '%s': %w", metadata.ID, err)
		}
		expected = append(expected,
			expectedPayload{*worktreePath, role(core.PayloadRoleGitConflictedWorktreeTar)},
			expectedPayload{*statePath, role(core.PayloadRoleGitOperationStateTar)})
	default:
		return fmt.Errorf("repository '%s' Git operation metadata and payloads disagree", metadata.ID)
	}
	worktreePaths := map[string]struct{}{}
	for index := range repository.LinkedWorktrees {
		worktree := &repository.LinkedWorktrees[index]
		if err := validateLinkedWorktreeMetadata(&worktree.Metadata); err != nil {
			return fmt.Errorf("repository '%s': %w", metadata.ID, err)
		}
		key := archivePathKey(worktree.Metadata.Path)
		if _, duplicate := worktreePaths[key]; duplicate {
			return fmt.Errorf("repository '%s' lists linked worktree '%s' twice", metadata.ID, worktree.Metadata.Path)
		}
		worktreePaths[key] = struct{}{}
		linkedRole := func(kind core.PayloadRoleKind) core.PayloadRole {
			return core.PayloadRole{Kind: kind, RepositoryID: metadata.ID, Worktree: uint32(index)}
		}
		expected = append(expected,
			expectedPayload{worktree.StagedPatchPath, linkedRole(core.PayloadRoleGitLinkedWorktreeStagedPatch)},
			expectedPayload{worktree.UnstagedPatchPath, linkedRole(core.PayloadRoleGitLinkedWorktreeUnstagedPatch)},
			expectedPayload{worktree.UntrackedTarPath, linkedRole(core.PayloadRoleGitLinkedWorktreeUntrackedTar)})
		linkedWorktreePath, linkedStatePath := worktree.ConflictedWorktreeTarPath, worktree.OperationStateTarPath
		switch {
		case linkedWorktreePath == nil && linkedStatePath == nil && worktree.Metadata.GitOperation == nil:
		case linkedWorktreePath != nil && linkedStatePath != nil && worktree.Metadata.GitOperation != nil:
			expected = append(expected,
				expectedPayload{*linkedWorktreePath, linkedRole(core.PayloadRoleGitLinkedWorktreeConflictedWorktreeTar)},
				expectedPayload{*linkedStatePath, linkedRole(core.PayloadRoleGitLinkedWorktreeOperationStateTar)})
		default:
			return fmt.Errorf("repository '%s' linked worktree '%s' Git operation metadata and payloads disagree",
				metadata.ID, worktree.Metadata.Path)
		}
	}
	for _, payload := range expected {
		listed := slices.ContainsFunc(manifest.Payloads, func(descriptor core.PayloadDescriptor) bool {
			return descriptor.Path == payload.path && descriptor.Role == payload.role
		})
		if !listed {
			return fmt.Errorf("repository '%s' is missing its %s payload", metadata.ID, payloadRoleDebug(&payload.role))
		}
	}
	return nil
}

func ensureLinkedWorktreeExists(manifest *core.ArchiveManifest, payload *core.PayloadDescriptor, repositoryID string, worktree uint32) error {
	for index := range manifest.Repositories {
		repository := &manifest.Repositories[index]
		if repository.Metadata.ID == repositoryID && uint64(worktree) < uint64(len(repository.LinkedWorktrees)) {
			return nil
		}
	}
	return fmt.Errorf("payload '%s' holds linked worktree %d that repository '%s' does not record",
		payload.Path, worktree, repositoryID)
}

// validateLinkedWorktreeMetadata: a linked worktree's directory is below the
// checkout and names nothing outside it, and its commit and lock reason are
// ones Git can have written.
func validateLinkedWorktreeMetadata(worktree *core.LinkedWorktreeMetadata) error {
	if err := validateWorktreeRelativePath(worktree.Path); err != nil {
		return fmt.Errorf("linked worktree '%s': %w", worktree.Path, err)
	}
	if !core.IsObjectID(worktree.HeadCommit) {
		return fmt.Errorf("linked worktree '%s' has an invalid head commit", worktree.Path)
	}
	if worktree.GitOperation != nil {
		if err := validateSavedGitOperation(worktree.GitOperation); err != nil {
			return fmt.Errorf("linked worktree '%s': %w", worktree.Path, err)
		}
	}
	return nil
}

// validateSavedGitOperation checks the index side of an archived Git
// operation: every path is a safe relative path, and every stage, mode and
// object id is one Git can have written.
func validateSavedGitOperation(operation *core.SavedGitOperation) error {
	unmergedPaths := map[string]struct{}{}
	for index := range operation.Unmerged {
		entry := &operation.Unmerged[index]
		if err := validateWorktreeRelativePath(entry.Path); err != nil {
			return err
		}
		if entry.Stage < 1 || entry.Stage > 3 {
			return fmt.Errorf("unmerged entry '%s' has stage %d", entry.Path, entry.Stage)
		}
		switch entry.Mode {
		case "100644", "100755", "120000", "160000":
		default:
			return fmt.Errorf("unmerged entry '%s' has mode %s", entry.Path, entry.Mode)
		}
		if !core.IsObjectID(entry.Object) {
			return fmt.Errorf("unmerged entry '%s' has an invalid object id", entry.Path)
		}
		unmergedPaths[archivePathKey(entry.Path)] = struct{}{}
	}
	for _, path := range operation.MissingWorktreePaths {
		if _, ok := unmergedPaths[archivePathKey(path)]; !ok {
			return fmt.Errorf("missing working-tree path '%s' is not unmerged", path)
		}
	}
	for _, object := range operation.Objects {
		if !core.IsObjectID(object) {
			return fmt.Errorf("Git operation names an invalid object id %s", archiveDebugString(object))
		}
	}
	return nil
}

// validatePayloadParts checks that a sharded payload can be reassembled
// exactly, and that its part entries occupy names nothing else in the archive
// claims. Every failure here is loud: a missing, extra, renamed, or reordered
// part cannot be read as a silently truncated or scrambled payload.
func validatePayloadParts(descriptor *core.PayloadDescriptor, paths map[string]struct{}) error {
	if len(descriptor.Parts) == 0 {
		return nil
	}
	if len(descriptor.Parts) == 1 {
		return fmt.Errorf("payload '%s' is sharded into a single part", descriptor.Path)
	}
	var covered uint64
	for index := range descriptor.Parts {
		part := &descriptor.Parts[index]
		if err := validateArchiveRelativePath(part.Path); err != nil {
			return err
		}
		if part.Path != payloadPartPath(descriptor.Path, index) {
			return fmt.Errorf("payload '%s' part %d has unexpected path '%s'", descriptor.Path, index, part.Path)
		}
		if _, duplicate := paths[part.Path]; duplicate {
			return fmt.Errorf("duplicate manifest payload '%s'", part.Path)
		}
		paths[part.Path] = struct{}{}
		if part.Size == 0 {
			return fmt.Errorf("payload '%s' part %d is empty", descriptor.Path, index)
		}
		if !core.IsLowerHexSha256(part.Sha256) {
			return fmt.Errorf("invalid SHA-256 for payload part '%s'", part.Path)
		}
		if covered+part.Size < covered {
			return fmt.Errorf("payload '%s' part sizes overflow", descriptor.Path)
		}
		covered += part.Size
	}
	if covered != descriptor.Size {
		return fmt.Errorf("payload '%s' parts cover %d bytes but the payload is %d bytes",
			descriptor.Path, covered, descriptor.Size)
	}
	return nil
}

func normalizedMode(mode uint32) (uint32, error) {
	if mode&^0o7777 != 0 {
		return 0, fmt.Errorf("invalid payload mode %o", mode)
	}
	// Writable archive artifacts never need setuid/setgid/sticky bits.
	return mode & 0o0777, nil
}

func digestBytes(data []byte) string {
	digest := sha256.Sum256(data)
	return core.LowerHex(digest[:])
}

func digestReader(reader io.Reader) (string, error) {
	digest := sha256.New()
	if _, err := io.CopyBuffer(digest, reader, make([]byte, 64*1024)); err != nil {
		return "", fmt.Errorf("hash Mjolnir archive: %w", err)
	}
	return core.LowerHex(digest.Sum(nil)), nil
}

func restrictArchivePermissions(path string) error {
	if runtime.GOOS == "windows" {
		return nil
	}
	if err := os.Chmod(path, 0o600); err != nil {
		return fmt.Errorf("set 0600 permissions on %s: %w", path, err)
	}
	return nil
}

func validateArchiveRelativePath(path string) error {
	if path == "" {
		return errors.New("archive path is empty")
	}
	if filepath.IsAbs(path) {
		return fmt.Errorf("archive path '%s' is absolute", path)
	}
	for _, component := range archivePathComponents(path) {
		if component.kind != archiveComponentNormal {
			return fmt.Errorf("unsafe archive path '%s'", path)
		}
	}
	return nil
}

// validateWorktreeRelativePath checks a path a restore writes or removes in a
// working tree. Besides the archive path rules, it refuses a `.git`
// component in any case, as Git does for every path it tracks: a hook written
// into the Git directory would run on the restored clone's next Git command.
func validateWorktreeRelativePath(path string) error {
	if err := validateArchiveRelativePath(path); err != nil {
		return err
	}
	for _, component := range archivePathComponents(path) {
		if archiveEqualFoldASCII(component.text, ".git") {
			return fmt.Errorf("refusing working-tree path '%s' inside a Git directory", path)
		}
	}
	return nil
}

// slashPath is a validated relative path as /-separated text.
func slashPath(path string) (string, error) {
	if err := validateArchiveRelativePath(path); err != nil {
		return "", err
	}
	components := archivePathComponents(path)
	parts := make([]string, 0, len(components))
	for _, component := range components {
		if !utf8.ValidString(component.text) {
			return "", fmt.Errorf("archive path '%s' is not UTF-8", path)
		}
		parts = append(parts, component.text)
	}
	return strings.Join(parts, "/"), nil
}

func ValidateComponent(value, label string) error {
	if value == "" {
		return fmt.Errorf("%s is empty", label)
	}
	if value == "." || value == ".." {
		return fmt.Errorf("invalid %s '%s'", label, value)
	}
	for index := 0; index < len(value); index++ {
		b := value[index]
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '-' || b == '_' || b == '.') {
			return fmt.Errorf("invalid %s '%s'", label, value)
		}
	}
	return nil
}

func validateSecretFreeMap(values map[string]string) error {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	for _, key := range keys {
		// ASCII lowercasing only, as Rust's to_ascii_lowercase.
		lower := strings.Map(func(r rune) rune {
			if 'A' <= r && r <= 'Z' {
				return r + ('a' - 'A')
			}
			return r
		}, key)
		for _, needle := range []string{"token", "secret", "password", "credential", "private_key"} {
			if strings.Contains(lower, needle) {
				return fmt.Errorf("target provenance key '%s' may contain a secret", key)
			}
		}
	}
	return nil
}

// originContainsCredentials is whether origin is an HTTP(S) URL, as the
// WHATWG URL parser reads it, that carries a user name or password.
func originContainsCredentials(origin string) bool {
	url, ok := core.ParseURL(origin)
	if !ok {
		return false
	}
	_, hasPassword := url.Password()
	return (url.Scheme() == "http" || url.Scheme() == "https") && (url.Username() != "" || hasPassword)
}

// RedactOriginCredentials removes embedded HTTP credentials before storing Git
// provenance.
func RedactOriginCredentials(origin string) (string, error) {
	url, ok := core.ParseURL(origin)
	if !ok {
		return origin, nil
	}
	if url.Scheme() != "http" && url.Scheme() != "https" {
		return origin, nil
	}
	if _, hasPassword := url.Password(); url.Username() == "" && !hasPassword {
		return origin, nil
	}
	redacted, ok := url.WithoutCredentials()
	if !ok {
		return "", errors.New("cannot redact username from Git origin")
	}
	return redacted.String(), nil
}

// MaxSessionFileBytes is the largest file the session file export will
// return. A caller that wants a build artifact wants the bundle, not a
// response the daemon has to hold in memory in full.
const MaxSessionFileBytes uint64 = 16 * 1024 * 1024

// ReadSessionFile reads one file from a session's workspace.
//
// The path is relative and must stay inside root after both ends are
// canonicalized, so neither a `..` component nor a symlink pointing out of
// the workspace can reach the rest of the target's filesystem. A refusal is
// a SessionExportError refusal; any other failure is a SessionExportError
// failure.
func ReadSessionFile(root, relative string) ([]byte, error) {
	if err := core.ValidateRelativeDestination(relative); err != nil {
		return nil, SessionExportErrorRefused(err.Error())
	}
	// The directory that was searched, as the caller spelled it, so a refusal
	// names somewhere the person can look (#1079).
	searched := root
	resolvedRoot, err := archiveCanonicalize(root)
	if err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("resolve session workspace %s: %w", root, err)}
	}
	path, err := archiveCanonicalize(filepath.Join(resolvedRoot, relative))
	if errors.Is(err, fs.ErrNotExist) {
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s is not in the session workspace %s", relative, searched))
	}
	if err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("resolve %s in the session workspace: %w", relative, err)}
	}
	if !archivePathStartsWith(path, resolvedRoot) {
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s leaves the session workspace %s", relative, searched))
	}
	info, err := os.Stat(path)
	if err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("read %s in the session workspace: %w", relative, err)}
	}
	if !info.Mode().IsRegular() {
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s is not a regular file", relative))
	}
	if uint64(info.Size()) > MaxSessionFileBytes {
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s is larger than %d bytes", relative, MaxSessionFileBytes))
	}
	contents, err := os.ReadFile(path)
	if err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("read %s in the session workspace: %w", relative, err)}
	}
	return contents, nil
}

// archiveCanonicalize is fs::canonicalize (realpath): the absolute path with
// every link resolved, where `..` steps out of the directory a link resolved
// to rather than undoing the link's name, and an error when a part of it does
// not exist. filepath.Abs would clean `link/..` away first.
func archiveCanonicalize(path string) (string, error) {
	if !filepath.IsAbs(path) {
		directory, err := os.Getwd()
		if err != nil {
			return "", err
		}
		path = directory + string(filepath.Separator) + path
	}
	return filepath.EvalSymlinks(path)
}

// payloadRoleDebug writes role as Rust's {:?} does, as in
// `GitBundle { repository_id: "hel" }`.
func payloadRoleDebug(role *core.PayloadRole) string {
	var variant strings.Builder
	for _, word := range strings.Split(string(role.Kind), "_") {
		if word != "" {
			variant.WriteString(strings.ToUpper(word[:1]) + word[1:])
		}
	}
	switch {
	case role.Kind == core.PayloadRoleCanonicalSession:
		return variant.String()
	case role.Kind == core.PayloadRoleNativeArtifact:
		return fmt.Sprintf("%s { relative_path: %s }", variant.String(), archiveDebugString(role.RelativePath))
	case strings.HasPrefix(string(role.Kind), "git_linked_worktree_"):
		return fmt.Sprintf("%s { repository_id: %s, worktree: %d }", variant.String(),
			archiveDebugString(role.RepositoryID), role.Worktree)
	}
	return fmt.Sprintf("%s { repository_id: %s }", variant.String(), archiveDebugString(role.RepositoryID))
}

// archiveDebugString renders text as Rust's `{:?}` does for a str.
func archiveDebugString(text string) string {
	var builder strings.Builder
	builder.WriteByte('"')
	for index, r := range text {
		switch {
		case r == '\t':
			builder.WriteString(`\t`)
		case r == '\r':
			builder.WriteString(`\r`)
		case r == '\n':
			builder.WriteString(`\n`)
		case r == '\\':
			builder.WriteString(`\\`)
		case r == '"':
			builder.WriteString(`\"`)
		case r == 0:
			builder.WriteString(`\0`)
		case r == utf8.RuneError && !strings.HasPrefix(text[index:], "�"):
			// Invalid UTF-8 cannot occur in a Rust str; keep the byte visible.
			fmt.Fprintf(&builder, `\x%02x`, text[index])
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			// Rust escapes non-printable and grapheme-extending characters.
			fmt.Fprintf(&builder, `\u{%x}`, r)
		default:
			builder.WriteRune(r)
		}
	}
	builder.WriteByte('"')
	return builder.String()
}

// archiveSerdeJSON is serde_json::to_vec: encoding/json's output without its
// HTML escapes (`<`, `>`, `&`, U+2028 and U+2029 stay as written), so the
// bytes and their digest match the ones Rust writes.
func archiveSerdeJSON(value any) ([]byte, error) {
	data, err := json.Marshal(value)
	if err != nil {
		return nil, err
	}
	if !bytes.Contains(data, []byte(`\u`)) {
		return data, nil
	}
	output := make([]byte, 0, len(data))
	inString := false
	for index := 0; index < len(data); index++ {
		c := data[index]
		if !inString {
			if c == '"' {
				inString = true
			}
			output = append(output, c)
			continue
		}
		switch c {
		case '"':
			inString = false
			output = append(output, c)
		case '\\':
			if index+5 < len(data) && data[index+1] == 'u' {
				replacement := ""
				switch string(data[index+2 : index+6]) {
				case "003c":
					replacement = "<"
				case "003e":
					replacement = ">"
				case "0026":
					replacement = "&"
				case "2028":
					replacement = " "
				case "2029":
					replacement = " "
				}
				if replacement != "" {
					output = append(output, replacement...)
					index += 5
					continue
				}
			}
			output = append(output, c, data[index+1])
			index++
		default:
			output = append(output, c)
		}
	}
	return output, nil
}

// archiveSerdeJSONPretty is serde_json::to_vec_pretty: two-space indentation
// and `": "` separators, as json.Indent writes them.
func archiveSerdeJSONPretty(value any) ([]byte, error) {
	compact, err := archiveSerdeJSON(value)
	if err != nil {
		return nil, err
	}
	var output bytes.Buffer
	if err := json.Indent(&output, compact, "", "  "); err != nil {
		return nil, err
	}
	return output.Bytes(), nil
}

// archiveParallel runs work for every index on up to GOMAXPROCS goroutines,
// as Rust's rayon pool does, and returns each index's error. A panic in work
// is raised again in the caller once every goroutine has stopped, as rayon
// resumes a worker's panic on the calling thread.
func archiveParallel(count int, work func(index int) error) []error {
	outcomes := make([]error, count)
	var next atomic.Int64
	var group sync.WaitGroup
	var panicOnce sync.Once
	var panicked any
	for range min(runtime.GOMAXPROCS(0), count) {
		group.Go(func() {
			defer func() {
				if value := recover(); value != nil {
					panicOnce.Do(func() { panicked = value })
				}
			}()
			for {
				index := int(next.Add(1) - 1)
				if index >= count {
					return
				}
				outcomes[index] = work(index)
			}
		})
	}
	group.Wait()
	if panicked != nil {
		panic(panicked)
	}
	return outcomes
}

// firstError is the error of the lowest index that failed.
func firstError(outcomes []error) error {
	for _, err := range outcomes {
		if err != nil {
			return err
		}
	}
	return nil
}

// archivePathKey is a key under which two paths are equal exactly when
// Rust's Path equality, which compares components, says they are.
func archivePathKey(path string) string {
	var key strings.Builder
	for _, component := range archivePathComponents(path) {
		key.WriteByte(byte('0' + component.kind))
		key.WriteString(component.text)
		key.WriteByte(0)
	}
	return key.String()
}

// zipIsDir is the zip crate's is_dir: the entry name ends in a separator.
func zipIsDir(name string) bool {
	return strings.HasSuffix(name, "/") || strings.HasSuffix(name, `\`)
}

// zipModeOrDefault is an entry's permission bits, 0600 when it records no
// Unix mode.
func zipModeOrDefault(header *zip.FileHeader) uint32 {
	mode, ok := zipUnixMode(header)
	if !ok {
		mode = 0o600
	}
	return mode & 0o7777
}

// archiveComponentKind is the kind of one component of Rust's
// Path::components.
type archiveComponentKind int

const (
	archiveComponentPrefix archiveComponentKind = iota
	archiveComponentRootDir
	archiveComponentCurDir
	archiveComponentParentDir
	archiveComponentNormal
)

// archivePathComponent is one component of a path as Rust's
// Path::components yields it; text is the component as written.
type archivePathComponent struct {
	kind archiveComponentKind
	text string
}

// archivePathComponents splits path as Rust's Path::components does on this
// platform: repeated separators and interior "." components vanish, a
// leading "." is CurDir, and a root or a Windows prefix is its own component.
func archivePathComponents(path string) []archivePathComponent {
	windows := runtime.GOOS == "windows"
	isSeparator := func(c byte) bool { return c == '/' || windows && c == '\\' }
	var components []archivePathComponent
	rest := path
	hasRoot := false
	if windows {
		if prefix := filepath.VolumeName(rest); prefix != "" {
			components = append(components, archivePathComponent{archiveComponentPrefix, prefix})
			rest = rest[len(prefix):]
			// A UNC or device prefix is rooted without a separator of its own.
			hasRoot = len(prefix) > 2
		}
	}
	if rest != "" && isSeparator(rest[0]) {
		components = append(components, archivePathComponent{archiveComponentRootDir, string(filepath.Separator)})
		hasRoot = true
		rest = rest[1:]
	} else if !hasRoot && rest != "" && rest[0] == '.' && (len(rest) == 1 || isSeparator(rest[1])) {
		// Only a leading "." of a path without a root is a component.
		components = append(components, archivePathComponent{archiveComponentCurDir, "."})
		rest = rest[1:]
	}
	for rest != "" {
		end := 0
		for end < len(rest) && !isSeparator(rest[end]) {
			end++
		}
		part := rest[:end]
		rest = rest[end:]
		if rest != "" {
			rest = rest[1:]
		}
		switch part {
		case "", ".":
		case "..":
			components = append(components, archivePathComponent{archiveComponentParentDir, ".."})
		default:
			components = append(components, archivePathComponent{archiveComponentNormal, part})
		}
	}
	return components
}

// archivePathStartsWith is Rust's Path::starts_with: base's components are a
// prefix of path's components.
func archivePathStartsWith(path, base string) bool {
	pathComponents, baseComponents := archivePathComponents(path), archivePathComponents(base)
	if len(baseComponents) > len(pathComponents) {
		return false
	}
	for index, component := range baseComponents {
		if pathComponents[index] != component {
			return false
		}
	}
	return true
}

// archiveEqualFoldASCII is Rust's eq_ignore_ascii_case: equal after
// lowercasing ASCII letters only.
func archiveEqualFoldASCII(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for index := 0; index < len(a); index++ {
		x, y := a[index], b[index]
		if 'A' <= x && x <= 'Z' {
			x += 'a' - 'A'
		}
		if 'A' <= y && y <= 'Z' {
			y += 'a' - 'A'
		}
		if x != y {
			return false
		}
	}
	return true
}

// zipEnclosedName is zip's ZipFile::enclosed_name: the entry name as a path
// when it has no NUL, no root or prefix, and never climbs above its start.
func zipEnclosedName(name string) (string, bool) {
	if strings.Contains(name, "\x00") {
		return "", false
	}
	depth := 0
	for _, component := range archivePathComponents(name) {
		switch component.kind {
		case archiveComponentPrefix, archiveComponentRootDir:
			return "", false
		case archiveComponentParentDir:
			if depth == 0 {
				return "", false
			}
			depth--
		case archiveComponentNormal:
			depth++
		}
	}
	return name, true
}

// zipUnixMode is zip's ZipFile::unix_mode: the Unix mode recorded in the
// entry's external attributes, or one derived from its MS-DOS attributes.
func zipUnixMode(header *zip.FileHeader) (uint32, bool) {
	const (
		systemDOS  = 0
		systemUnix = 3
		modeDir    = 0o040000
		modeFile   = 0o100000
	)
	attributes := header.ExternalAttrs
	if attributes == 0 {
		return 0, false
	}
	switch header.CreatorVersion >> 8 {
	case systemUnix:
		return attributes >> 16, true
	case systemDOS:
		// Interpret the MS-DOS directory bit.
		mode := uint32(modeFile | 0o664)
		if attributes&0x10 != 0 {
			mode = modeDir | 0o775
		}
		if attributes&0x01 != 0 {
			// Read-only bit: strip write permissions.
			mode &= 0o555
		}
		return mode, true
	}
	return 0, false
}
