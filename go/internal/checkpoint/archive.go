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
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync/atomic"
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
	panic("unported: mj-checkpoint/src/archive.rs checkpoint_bundle_prerequisites")
}

func bundleHeaderObjectID(line []byte, kind string) (string, error) {
	panic("unported: mj-checkpoint/src/archive.rs bundle_header_object_id")
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
	panic("unported: mj-checkpoint/src/archive.rs write_archive_installed_view")
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
	panic("unported: mj-checkpoint/src/archive.rs copy_payload_from_archive")
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
	panic("unported: mj-checkpoint/src/archive.rs verify_repository_bundles_streaming")
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
	panic("unported: mj-checkpoint/src/archive.rs read_checkpoint_repository_bundles")
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
	panic("unported: mj-checkpoint/src/archive.rs read_verified_checkpoint_payload")
}

func readCheckpointManifestFrom(archive *archiveZip) (core.ArchiveManifest, error) {
	panic("unported: mj-checkpoint/src/archive.rs read_checkpoint_manifest_from")
}

func readCheckpointPayload(archive *archiveZip, descriptor *core.PayloadDescriptor) ([]byte, error) {
	panic("unported: mj-checkpoint/src/archive.rs read_checkpoint_payload")
}

func readCheckpointEntry(archive *archiveZip, path string, expectedSize uint64, expectedMode uint32) ([]byte, error) {
	panic("unported: mj-checkpoint/src/archive.rs read_checkpoint_entry")
}

func prepareArchiveViewWithPartSize(input archiveInputView, partBytes int) (core.ArchiveManifest, []pendingPayload, error) {
	panic("unported: mj-checkpoint/src/archive.rs prepare_archive_view_with_part_size")
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
	panic("unported: mj-checkpoint/src/archive.rs plan_payload_parts")
}

// newArchiveZipWriter is a ZIP writer whose DEFLATE and Zstandard entries
// use the archive's compression levels.
func newArchiveZipWriter(output io.Writer) *zip.Writer {
	writer := zip.NewWriter(output)
	writer.RegisterCompressor(zip.Deflate, func(out io.Writer) (io.WriteCloser, error) {
		return flate.NewWriter(out, deflateLevel)
	})
	writer.RegisterCompressor(zipMethodZstd, func(out io.Writer) (io.WriteCloser, error) {
		return zstd.NewWriter(out, zstd.WithEncoderLevel(zstd.EncoderLevelFromZstd(zstdLevel)))
	})
	return writer
}

func writeZip(output *os.File, spoolParent string, manifest *core.ArchiveManifest, payloads []pendingPayload) error {
	panic("unported: mj-checkpoint/src/archive.rs write_zip")
}

// appendStreamEntries: assembly owns payload completion. Workers may finish
// parts out of order, so only this ordered pass can check EOF after every
// part has completed. compressed[i] is the private single-entry ZIP holding
// entries[i] already compressed, or nil for a stored entry.
func appendStreamEntries(writer *zip.Writer, entries []streamEntry, compressed []*os.File) error {
	panic("unported: mj-checkpoint/src/archive.rs append_stream_entries")
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
	panic("unported: mj-checkpoint/src/archive.rs compress_stream_entry")
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
	panic("unported: mj-checkpoint/src/archive.rs write_stream_entry")
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
}

func openArchive(path string) (*archiveZip, error) {
	return openArchiveCancellable(path, nil)
}

func openArchiveCancellable(path string, cancelled *atomic.Bool) (*archiveZip, error) {
	panic("unported: mj-checkpoint/src/archive.rs open_archive_cancellable")
}

// byName is the entry named name (zip's ZipArchive::by_name).
func (a *archiveZip) byName(name string) (*zip.File, error) {
	panic("unported: mj-checkpoint/src/archive.rs ZipArchive::by_name")
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
	panic("unported: mj-checkpoint/src/archive.rs read_verified_zip_cancellable")
}

// readVerifiedEntry reads one ZIP entry, verifies it against the manifest,
// and returns its body (with true) when the caller needs it.
func readVerifiedEntry(archive *archiveZip, meta *zipEntryMeta, expectation entryExpectation, retention payloadRetention) ([]byte, bool, error) {
	panic("unported: mj-checkpoint/src/archive.rs read_verified_entry")
}

// reassemblePayload turns verified entry bodies back into one payload body
// (with true when there is one). Restore and import therefore never see part
// entries, whatever the archive layout is.
func reassemblePayload(descriptor *core.PayloadDescriptor, bodies map[string][]byte, retention payloadRetention) ([]byte, bool, error) {
	panic("unported: mj-checkpoint/src/archive.rs reassemble_payload")
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
	panic("unported: mj-checkpoint/src/archive.rs parse_archive_manifest")
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
	panic("unported: mj-checkpoint/src/archive.rs validate_manifest")
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
	panic("unported: mj-checkpoint/src/archive.rs validate_saved_git_operation")
}

// validatePayloadParts checks that a sharded payload can be reassembled
// exactly, and that its part entries occupy names nothing else in the archive
// claims. Every failure here is loud: a missing, extra, renamed, or reordered
// part cannot be read as a silently truncated or scrambled payload.
func validatePayloadParts(descriptor *core.PayloadDescriptor, paths map[string]struct{}) error {
	panic("unported: mj-checkpoint/src/archive.rs validate_payload_parts")
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
	panic("unported: mj-checkpoint/src/archive.rs origin_contains_credentials")
}

// RedactOriginCredentials removes embedded HTTP credentials before storing Git
// provenance.
func RedactOriginCredentials(origin string) (string, error) {
	panic("unported: mj-checkpoint/src/archive.rs redact_origin_credentials")
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
	panic("unported: mj-checkpoint/src/archive.rs read_session_file")
}

// payloadRoleDebug writes role as Rust's {:?} does, as in
// `GitBundle { repository_id: "hel" }`.
func payloadRoleDebug(role *core.PayloadRole) string {
	panic("unported: mj-checkpoint/src/archive.rs PayloadRole Debug")
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
	panic("unported: mj-checkpoint/src/archive.rs Path::components")
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
	panic("unported: mj-checkpoint/src/archive.rs ZipFile::enclosed_name")
}

// zipUnixMode is zip's ZipFile::unix_mode: the Unix mode recorded in the
// entry's external attributes, or one derived from its MS-DOS attributes.
func zipUnixMode(header *zip.FileHeader) (uint32, bool) {
	panic("unported: mj-checkpoint/src/archive.rs ZipFile::unix_mode")
}
