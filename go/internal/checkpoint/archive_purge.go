// Port of mj-checkpoint/src/archive/purge.rs.
//
// Rewrite a legacy checkpoint archive so its untracked-file payloads are
// empty.
//
// The old reader still requires every `untracked_tar_path` entry and checks
// its digest, so a purge replaces those bodies with a valid empty tar and
// updates the manifest. Every other entry, including saved operation state,
// is copied through unchanged, and the schema version stays the one the
// archive already declared.

package checkpoint

import (
	"archive/tar"
	"archive/zip"
	"bufio"
	"bytes"
	"compress/zlib"
	"crypto/sha1"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// maxInspectedCommitBytes: commits larger than this are not stash metadata.
// Skip them instead of allocating the size a corrupt pack header claims.
const maxInspectedCommitBytes = 16 * 1024 * 1024

// StashUntrackedNote is a stash commit whose third parent still holds
// untracked files.
//
// Purge reports these and leaves the bundle alone. Restoring the stash would
// bring those files back; deleting them would rewrite history the old reader
// still verifies.
type StashUntrackedNote struct {
	RepositoryID string
	Commit       string
	Message      string
}

// ArchiveUntrackedPurgeKind is what PurgeUntrackedFromArchive did with the
// destination. One of ArchiveUntrackedPurgeKindAlreadyNewFormat,
// ArchiveUntrackedPurgeKindAlreadyClean, ArchiveUntrackedPurgeKindDryRun and
// ArchiveUntrackedPurgeKindRewritten.
type ArchiveUntrackedPurgeKind interface {
	isArchiveUntrackedPurgeKind()
}

// ArchiveUntrackedPurgeKindAlreadyNewFormat: every repository is an
// origin-delta snapshot and no legacy untracked body is large enough to
// replace.
type ArchiveUntrackedPurgeKindAlreadyNewFormat struct{}

// ArchiveUntrackedPurgeKindAlreadyClean: the archive is old, and every
// untracked body is already empty.
type ArchiveUntrackedPurgeKindAlreadyClean struct{}

// ArchiveUntrackedPurgeKindDryRun: untracked bytes would be reclaimed. The
// destination was not written.
type ArchiveUntrackedPurgeKindDryRun struct{}

// ArchiveUntrackedPurgeKindRewritten: the destination is a verified archive
// with empty untracked payloads.
type ArchiveUntrackedPurgeKindRewritten struct {
	ArchiveSha256 string
}

func (ArchiveUntrackedPurgeKindAlreadyNewFormat) isArchiveUntrackedPurgeKind() {}
func (ArchiveUntrackedPurgeKindAlreadyClean) isArchiveUntrackedPurgeKind()     {}
func (ArchiveUntrackedPurgeKindDryRun) isArchiveUntrackedPurgeKind()           {}
func (ArchiveUntrackedPurgeKindRewritten) isArchiveUntrackedPurgeKind()        {}

// ArchiveUntrackedPurgeReport is the result of inspecting, and maybe
// rewriting, one archive.
type ArchiveUntrackedPurgeReport struct {
	Kind ArchiveUntrackedPurgeKind
	// BytesReclaimed is the on-disk bytes a live purge removed: the old
	// archive's file size minus the rewritten archive's file size. A dry run
	// estimates the same saving from the ZIP central directory: the
	// compressed size of each removed untracked entry, including shards,
	// minus the compressed size of the empty tar that would replace it.
	// Neither number is the uncompressed tar size.
	BytesReclaimed      uint64
	StashUntrackedNotes []StashUntrackedNote
	// Warnings are non-fatal warnings (for example when stash bundle
	// inspection could not determine whether stash entries still hold
	// untracked files).
	Warnings      []string
	SchemaVersion uint32
}

// PreparedUntrackedPurge is a purge that stopped before installing a
// destination. One of PreparedUntrackedPurgeFinished,
// PreparedUntrackedPurgeStaged and PreparedUntrackedPurgeCancelled.
type PreparedUntrackedPurge interface {
	isPreparedUntrackedPurge()
}

// PreparedUntrackedPurgeFinished: nothing was written. The report is the
// whole result.
type PreparedUntrackedPurgeFinished struct {
	Report ArchiveUntrackedPurgeReport
}

// PreparedUntrackedPurgeStaged: a verified temporary archive. Discarding it
// (StagedUntrackedPurge.Discard, Rust's drop) removes that file.
type PreparedUntrackedPurgeStaged struct {
	Staged *StagedUntrackedPurge
}

// PreparedUntrackedPurgeCancelled: the cancellation probe asked the purge to
// stop. Temporary files are already gone.
type PreparedUntrackedPurgeCancelled struct{}

func (PreparedUntrackedPurgeFinished) isPreparedUntrackedPurge()  {}
func (PreparedUntrackedPurgeStaged) isPreparedUntrackedPurge()    {}
func (PreparedUntrackedPurgeCancelled) isPreparedUntrackedPurge() {}

// StagedUntrackedPurge is a rewritten archive that has been fsynced and
// verified, but not installed.
//
// The temporary file lives in the destination's directory. Install is the
// atomic publication onto the destination. Discard deletes the temporary
// file and does not touch the destination; Rust does that when the value is
// dropped, so every path that does not install must discard.
type StagedUntrackedPurge struct {
	temporary           string
	destination         string
	ArchiveSha256       string
	BytesReclaimed      uint64
	StashUntrackedNotes []StashUntrackedNote
	Warnings            []string
	schemaVersion       uint32
}

// Destination is the path the staged bytes will occupy after Install.
func (s *StagedUntrackedPurge) Destination() string {
	return s.destination
}

// Discard removes the staged temporary archive (Rust's drop of the staged
// value). It is a no-op once the archive is installed or discarded.
func (s *StagedUntrackedPurge) Discard() {
	if s.temporary != "" {
		os.Remove(s.temporary)
		s.temporary = ""
	}
}

// Install publishes the staged bytes at the destination without replacing
// a file there (tempfile's persist_noclobber).
//
// A failure after the publication removes that installed file. A file that
// already occupied the destination is left in place, because the
// publication refuses to replace it. The temporary file is gone afterwards
// either way.
func (s *StagedUntrackedPurge) Install() (ArchiveUntrackedPurgeReport, error) {
	temporary := s.temporary
	s.temporary = ""
	defer os.Remove(temporary)
	destination := s.destination
	parent := filepath.Dir(destination)
	if err := os.Link(temporary, destination); err != nil {
		return ArchiveUntrackedPurgeReport{}, fmt.Errorf("install purged archive %s: %w", destination, err)
	}
	if err := restrictArchivePermissions(destination); err != nil {
		return ArchiveUntrackedPurgeReport{}, removeInstalled(destination, err)
	}
	if err := core.SyncDirectory(parent); err != nil {
		return ArchiveUntrackedPurgeReport{}, removeInstalled(destination, err)
	}
	return ArchiveUntrackedPurgeReport{
		Kind:                ArchiveUntrackedPurgeKindRewritten{ArchiveSha256: s.ArchiveSha256},
		BytesReclaimed:      s.BytesReclaimed,
		StashUntrackedNotes: s.StashUntrackedNotes,
		Warnings:            s.Warnings,
		SchemaVersion:       s.schemaVersion,
	}, nil
}

func removeInstalled(destination string, err error) error {
	if removeErr := os.Remove(destination); removeErr != nil {
		return fmt.Errorf("also failed to remove incomplete archive %s: %v: %w", destination, core.IOErrorDisplay(removeErr), err)
	}
	return err
}

// ArchivePurgeNotCancelled is a probe that never asks a purge to stop.
func ArchivePurgeNotCancelled() ArchivePurgeCancel {
	return func() bool { return false }
}

// PurgeWasCancelled is whether a purge stopped because its cancellation
// probe fired.
func PurgeWasCancelled(err error) bool {
	return ArchiveVerificationCancelled(err) || errors.Is(err, purgeCancelled{})
}

type purgeCancelled struct{}

func (purgeCancelled) Error() string { return "archive purge cancelled" }

func stopIfCancelled(cancelled ArchivePurgeCancel) error {
	if cancelled() {
		return purgeCancelled{}
	}
	return nil
}

// PurgeUntrackedFromArchive replaces every non-empty legacy untracked tar
// with an empty one.
//
// destinationArchive is written only when dryRun is false and some
// untracked body is larger than an empty tar. The write installs without
// replacing a file after the rewritten bytes have been verified. Only a
// destination this call actually installed is removed if a later step fails.
// A file that was already at destinationArchive is left in place.
//
// This entry never cancels. Call PrepareUntrackedPurge to stop when a probe
// returns true, and install the staged file separately.
func PurgeUntrackedFromArchive(sourceArchive, destinationArchive string, dryRun bool) (ArchiveUntrackedPurgeReport, error) {
	prepared, err := PrepareUntrackedPurge(sourceArchive, destinationArchive, dryRun, ArchivePurgeNotCancelled())
	if err != nil {
		return ArchiveUntrackedPurgeReport{}, err
	}
	switch prepared := prepared.(type) {
	case PreparedUntrackedPurgeFinished:
		return prepared.Report, nil
	case PreparedUntrackedPurgeStaged:
		return prepared.Staged.Install()
	default:
		return ArchiveUntrackedPurgeReport{}, errors.New("archive purge cancelled")
	}
}

// PrepareUntrackedPurge verifies sourceArchive and, when a live purge has
// something to remove, rewrites it into a temporary file in the destination
// directory.
//
// The destination is not installed; StagedUntrackedPurge.Install does that.
// When cancelled returns true, this removes any temporary file and returns
// PreparedUntrackedPurgeCancelled instead of an archive error.
func PrepareUntrackedPurge(sourceArchive, destinationArchive string, dryRun bool, cancelled ArchivePurgeCancel) (PreparedUntrackedPurge, error) {
	if cancelled() {
		return PreparedUntrackedPurgeCancelled{}, nil
	}
	verified, err := VerifyArchiveStreamingUnless(sourceArchive, cancelled)
	if err != nil {
		if PurgeWasCancelled(err) || cancelled() {
			return PreparedUntrackedPurgeCancelled{}, nil
		}
		return nil, fmt.Errorf("verify archive %s before purging untracked files: %w", sourceArchive, err)
	}
	manifest := verified.Manifest
	schemaVersion := manifest.SchemaVersion
	empty, err := emptyUntrackedTar()
	if err != nil {
		return nil, err
	}
	emptyLen := uint64(len(empty))
	// Eligibility stays on the uncompressed body. The reported number is the
	// on-disk delta, which compression can make much smaller.
	var uncompressedReclaim uint64
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		if isUntracked(&payload.Role) && payload.Size > emptyLen {
			uncompressedReclaim = saturatingAdd(uncompressedReclaim, payload.Size-emptyLen)
		}
	}
	originDelta := len(manifest.Repositories) > 0 &&
		!slices.ContainsFunc(manifest.Repositories, func(repository core.RepositoryManifest) bool {
			return repository.Metadata.OriginDelta == nil
		})
	if originDelta && uncompressedReclaim == 0 {
		return PreparedUntrackedPurgeFinished{Report: ArchiveUntrackedPurgeReport{
			Kind:                ArchiveUntrackedPurgeKindAlreadyNewFormat{},
			StashUntrackedNotes: []StashUntrackedNote{},
			Warnings:            []string{},
			SchemaVersion:       schemaVersion,
		}}, nil
	}
	notes, warnings, err := stashUntrackedNotes(sourceArchive, &manifest, cancelled)
	if err != nil {
		if PurgeWasCancelled(err) || cancelled() {
			return PreparedUntrackedPurgeCancelled{}, nil
		}
		return nil, err
	}
	if uncompressedReclaim == 0 {
		return PreparedUntrackedPurgeFinished{Report: ArchiveUntrackedPurgeReport{
			Kind:                ArchiveUntrackedPurgeKindAlreadyClean{},
			StashUntrackedNotes: notes,
			Warnings:            warnings,
			SchemaVersion:       schemaVersion,
		}}, nil
	}
	if dryRun {
		bytesReclaimed, err := compressedReclaimEstimate(sourceArchive, &manifest, empty, cancelled)
		if err != nil {
			if PurgeWasCancelled(err) || cancelled() {
				return PreparedUntrackedPurgeCancelled{}, nil
			}
			return nil, err
		}
		return PreparedUntrackedPurgeFinished{Report: ArchiveUntrackedPurgeReport{
			Kind:                ArchiveUntrackedPurgeKindDryRun{},
			BytesReclaimed:      bytesReclaimed,
			StashUntrackedNotes: notes,
			Warnings:            warnings,
			SchemaVersion:       schemaVersion,
		}}, nil
	}
	if cancelled() {
		return PreparedUntrackedPurgeCancelled{}, nil
	}
	temporary, err := stageRewrittenArchive(sourceArchive, destinationArchive, &manifest, empty, cancelled)
	if err != nil {
		if PurgeWasCancelled(err) || cancelled() {
			return PreparedUntrackedPurgeCancelled{}, nil
		}
		return nil, fmt.Errorf("purge untracked files from %s into %s: %w", sourceArchive, destinationArchive, err)
	}
	staged := &StagedUntrackedPurge{
		temporary:           temporary,
		destination:         destinationArchive,
		StashUntrackedNotes: notes,
		Warnings:            warnings,
		schemaVersion:       schemaVersion,
	}
	rewritten, err := VerifyArchiveStreamingUnless(temporary, cancelled)
	if err != nil {
		staged.Discard()
		if PurgeWasCancelled(err) || cancelled() {
			return PreparedUntrackedPurgeCancelled{}, nil
		}
		return nil, fmt.Errorf("purged archive failed verification and was removed: %w", err)
	}
	staged.ArchiveSha256 = rewritten.ArchiveSha256
	// One checkpoint archive is one file. Its untracked shards are ZIP
	// entries in that file, so the file-size delta is the disk space every
	// rewritten shard released.
	oldBytes, err := fileLen(sourceArchive)
	if err != nil {
		staged.Discard()
		return nil, err
	}
	newBytes, err := fileLen(temporary)
	if err != nil {
		staged.Discard()
		return nil, err
	}
	staged.BytesReclaimed = saturatingSub(oldBytes, newBytes)
	return PreparedUntrackedPurgeStaged{Staged: staged}, nil
}

func fileLen(path string) (uint64, error) {
	info, err := os.Stat(path)
	if err != nil {
		return 0, fmt.Errorf("read the size of %s: %w", path, err)
	}
	return uint64(info.Size()), nil
}

func saturatingAdd(left, right uint64) uint64 {
	if left+right < left {
		return ^uint64(0)
	}
	return left + right
}

func saturatingSub(left, right uint64) uint64 {
	if right > left {
		return 0
	}
	return left - right
}

func isUntracked(role *core.PayloadRole) bool {
	return role.Kind == core.PayloadRoleGitUntrackedTar || role.Kind == core.PayloadRoleGitLinkedWorktreeUntrackedTar
}

func emptyUntrackedTar() ([]byte, error) {
	var body bytes.Buffer
	if err := tar.NewWriter(&body).Close(); err != nil {
		return nil, fmt.Errorf("finish empty untracked tar: %w", err)
	}
	if body.Len() != 1024 || slices.ContainsFunc(body.Bytes(), func(b byte) bool { return b != 0 }) {
		return nil, fmt.Errorf("empty untracked tar must be 1024 zero bytes, got %d bytes", body.Len())
	}
	if err := validateUntrackedTar(body.Bytes()); err != nil {
		return nil, fmt.Errorf("empty untracked tar failed validation: %w", err)
	}
	return body.Bytes(), nil
}

type plannedReplacement struct {
	oldNames []string
	mode     uint32
	entries  []replacementEntry
}

type replacementEntry struct {
	name string
	data []byte
}

func planReplacements(manifest *core.ArchiveManifest, empty []byte) []plannedReplacement {
	emptyLen := uint64(len(empty))
	replaced := map[string]struct{}{}
	oldNames := map[string][]string{}
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		if isUntracked(&payload.Role) && payload.Size > emptyLen {
			replaced[payload.Path] = struct{}{}
			oldNames[payload.Path] = zipNames(payload)
		}
	}
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		if _, ok := replaced[payload.Path]; ok {
			payload.Sha256 = digestBytes(empty)
			payload.Size = emptyLen
			payload.Parts = nil
		}
	}
	// Schema 3 is valid only while some payload is sharded. Clearing the
	// parts of the one payload that forced that schema would make the same
	// version fail the reader, so the empty tar is split into two parts
	// instead.
	keepSharded := manifest.SchemaVersion == core.ArchiveSchemaVersionSharded &&
		!slices.ContainsFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool {
			return len(payload.Parts) > 0
		})
	if keepSharded {
		for index := range manifest.Payloads {
			payload := &manifest.Payloads[index]
			if _, ok := replaced[payload.Path]; !ok {
				continue
			}
			mid := len(empty) / 2
			for partIndex, chunk := range [][]byte{empty[:mid], empty[mid:]} {
				payload.Parts = append(payload.Parts, core.PayloadPartDescriptor{
					Path:   payloadPartPath(payload.Path, partIndex),
					Sha256: digestBytes(chunk),
					Size:   uint64(len(chunk)),
				})
			}
			break
		}
	}
	var planned []plannedReplacement
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		if _, ok := replaced[payload.Path]; !ok {
			continue
		}
		names, ok := oldNames[payload.Path]
		if !ok {
			panic("replaced untracked payload keeps its old entry names")
		}
		delete(oldNames, payload.Path)
		planned = append(planned, plannedReplacement{
			oldNames: names,
			mode:     payload.Mode,
			entries:  replacementEntries(payload, empty),
		})
	}
	return planned
}

func zipNames(payload *core.PayloadDescriptor) []string {
	if len(payload.Parts) == 0 {
		return []string{payload.Path}
	}
	names := make([]string, 0, len(payload.Parts))
	for index := range payload.Parts {
		names = append(names, payload.Parts[index].Path)
	}
	return names
}

func replacementEntries(payload *core.PayloadDescriptor, empty []byte) []replacementEntry {
	if len(payload.Parts) == 0 {
		return []replacementEntry{{payload.Path, slices.Clone(empty)}}
	}
	entries := make([]replacementEntry, 0, len(payload.Parts))
	offset := 0
	for index := range payload.Parts {
		part := &payload.Parts[index]
		start := offset
		offset += int(part.Size)
		entries = append(entries, replacementEntry{part.Path, slices.Clone(empty[start:offset])})
	}
	return entries
}

// stageRewrittenArchive writes the rewritten archive into a temporary file
// beside the destination and returns its path. The temporary file is the
// only thing this function creates, and it is removed on every failure.
func stageRewrittenArchive(sourceArchive, destinationArchive string, manifest *core.ArchiveManifest, empty []byte, cancelled ArchivePurgeCancel) (string, error) {
	if err := stopIfCancelled(cancelled); err != nil {
		return "", err
	}
	if _, err := os.Stat(destinationArchive); err == nil {
		return "", fmt.Errorf("refusing to replace existing archive %s", destinationArchive)
	}
	parent := filepath.Dir(destinationArchive)
	if info, err := os.Stat(parent); err != nil || !info.IsDir() {
		return "", fmt.Errorf("archive directory %s does not exist", parent)
	}
	rewritten := cloneArchiveManifest(manifest)
	planned := planReplacements(&rewritten, empty)
	if len(planned) == 0 {
		return "", errors.New("purge planned no untracked replacement")
	}
	manifestBytes, err := archiveSerdeJSONPretty(&rewritten)
	if err != nil {
		return "", fmt.Errorf("serialize purged archive manifest: %w", err)
	}
	// The destination is installed later, and only then.
	temporary, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return "", fmt.Errorf("create temporary archive in %s: %w", parent, err)
	}
	keep := false
	defer func() {
		if !keep {
			temporary.Close()
			os.Remove(temporary.Name())
		}
	}()
	if err := restrictArchivePermissions(temporary.Name()); err != nil {
		return "", err
	}
	writer := newArchiveZipWriter(temporary)
	if err := streamRewrittenEntries(writer, sourceArchive, manifestBytes, planned, cancelled); err != nil {
		return "", err
	}
	if err := writer.Close(); err != nil {
		return "", fmt.Errorf("finish purged archive ZIP: %w", err)
	}
	if err := temporary.Sync(); err != nil {
		return "", fmt.Errorf("fsync purged archive: %w", err)
	}
	if err := temporary.Close(); err != nil {
		return "", fmt.Errorf("fsync purged archive: %w", err)
	}
	keep = true
	return temporary.Name(), nil
}

// cloneArchiveManifest is a manifest whose payload list the caller can edit
// without changing the original (Rust's Clone).
func cloneArchiveManifest(manifest *core.ArchiveManifest) core.ArchiveManifest {
	clone := *manifest
	clone.Payloads = make([]core.PayloadDescriptor, len(manifest.Payloads))
	for index, payload := range manifest.Payloads {
		payload.Parts = slices.Clone(payload.Parts)
		clone.Payloads[index] = payload
	}
	return clone
}

func streamRewrittenEntries(writer *zip.Writer, sourceArchive string, manifestBytes []byte, planned []plannedReplacement, cancelled ArchivePurgeCancel) error {
	skip := map[string]struct{}{}
	emitAt := map[string]int{}
	for index := range planned {
		replacement := &planned[index]
		if len(replacement.oldNames) == 0 {
			return errors.New("untracked payload has no archive entry to replace")
		}
		first := replacement.oldNames[0]
		if _, duplicate := emitAt[first]; duplicate {
			return fmt.Errorf("two untracked payloads share archive entry '%s'", first)
		}
		emitAt[first] = index
		for _, name := range replacement.oldNames {
			skip[name] = struct{}{}
		}
	}
	archive, err := openArchiveProbed(sourceArchive, cancelled)
	if err != nil {
		return fmt.Errorf("read %s: %w", sourceArchive, err)
	}
	defer archive.close()
	manifestDigest := digestBytes(manifestBytes)
	sawManifest := false
	emitted := map[int]struct{}{}
	for _, entry := range archive.files {
		if err := stopIfCancelled(cancelled); err != nil {
			return err
		}
		name := entry.Name
		if name == manifestPath {
			sawManifest = true
			if err := writeStreamEntry(writer, manifestPath, 0o600, zip.Deflate, uint64(len(manifestBytes)),
				bytes.NewReader(manifestBytes), manifestDigest); err != nil {
				return err
			}
			continue
		}
		if slot, ok := emitAt[name]; ok {
			if err := writeReplacement(writer, &planned[slot]); err != nil {
				return err
			}
			emitted[slot] = struct{}{}
			continue
		}
		if _, ok := skip[name]; ok {
			continue
		}
		if err := writer.Copy(entry); err != nil {
			return fmt.Errorf("copy ZIP entry '%s': %w", name, err)
		}
	}
	if !sawManifest {
		return fmt.Errorf("archive is missing %s", manifestPath)
	}
	if len(emitted) != len(planned) {
		return errors.New("archive is missing an untracked payload that the manifest names")
	}
	return nil
}

func writeReplacement(writer *zip.Writer, replacement *plannedReplacement) error {
	for _, entry := range replacement.entries {
		if err := writeStreamEntry(writer, entry.name, replacement.mode, zipMethodZstd, uint64(len(entry.data)),
			bytes.NewReader(entry.data), digestBytes(entry.data)); err != nil {
			return fmt.Errorf("write empty untracked payload '%s': %w", entry.name, err)
		}
	}
	return nil
}

// compressedReclaimEstimate is the dry-run estimate of the on-disk saving.
//
// Each removed untracked entry, including every shard, contributes its ZIP
// central-directory compressed size. The empty tar that would replace those
// entries is compressed the same way a live rewrite compresses it, and that
// overhead is subtracted. The archive is not rewritten.
func compressedReclaimEstimate(sourceArchive string, manifest *core.ArchiveManifest, empty []byte, cancelled ArchivePurgeCancel) (uint64, error) {
	if err := stopIfCancelled(cancelled); err != nil {
		return 0, err
	}
	plannedManifest := cloneArchiveManifest(manifest)
	planned := planReplacements(&plannedManifest, empty)
	if len(planned) == 0 {
		return 0, errors.New("purge planned no untracked replacement")
	}
	archive, err := openArchiveProbed(sourceArchive, cancelled)
	if err != nil {
		return 0, fmt.Errorf("read %s: %w", sourceArchive, err)
	}
	defer archive.close()
	var removed uint64
	for index := range planned {
		for _, name := range planned[index].oldNames {
			if err := stopIfCancelled(cancelled); err != nil {
				return 0, err
			}
			entry, err := archive.byName(name)
			if err != nil {
				return 0, fmt.Errorf("read compressed size of untracked entry '%s': %w", name, err)
			}
			removed = saturatingAdd(removed, entry.CompressedSize64)
		}
	}
	overhead, err := replacementCompressedOverhead(planned)
	if err != nil {
		return 0, err
	}
	return saturatingSub(removed, overhead), nil
}

// replacementCompressedOverhead is the compressed size of the empty-tar
// entries a live rewrite would store.
func replacementCompressedOverhead(planned []plannedReplacement) (uint64, error) {
	var buffer bytes.Buffer
	writer := newArchiveZipWriter(&buffer)
	for index := range planned {
		replacement := &planned[index]
		for _, entry := range replacement.entries {
			output, err := writer.CreateHeader(archiveFileHeader(entry.name, zipMethodZstd, fs.FileMode(replacement.mode)))
			if err != nil {
				return 0, fmt.Errorf("measure empty untracked payload '%s': %w", entry.name, err)
			}
			if _, err := output.Write(entry.data); err != nil {
				return 0, fmt.Errorf("measure empty untracked payload '%s': %w", entry.name, err)
			}
		}
	}
	if err := writer.Close(); err != nil {
		return 0, fmt.Errorf("finish empty-tar size probe: %w", err)
	}
	probe, err := zip.NewReader(bytes.NewReader(buffer.Bytes()), int64(buffer.Len()))
	if err != nil {
		return 0, fmt.Errorf("read empty-tar size probe: %w", err)
	}
	var overhead uint64
	for _, entry := range probe.File {
		overhead = saturatingAdd(overhead, entry.CompressedSize64)
	}
	return overhead, nil
}

func stashUntrackedNotes(sourceArchive string, manifest *core.ArchiveManifest, cancelled ArchivePurgeCancel) ([]StashUntrackedNote, []string, error) {
	notes := []StashUntrackedNote{}
	warnings := []string{}
	for index := range manifest.Repositories {
		repository := &manifest.Repositories[index]
		if err := stopIfCancelled(cancelled); err != nil {
			return nil, nil, err
		}
		if repository.Metadata.OriginDelta != nil ||
			len(repository.Metadata.StashStack) == 0 ||
			repository.CommittedBundlePath == "" {
			continue
		}
		repositoryNotes, repositoryWarnings, err := notesForRepository(sourceArchive, manifest, repository, cancelled)
		if err != nil {
			if PurgeWasCancelled(err) {
				return nil, nil, err
			}
			slog.Warn("could not inspect stash entries for untracked files; the purge continues",
				"repository_id", repository.Metadata.ID, "error", err.Error())
			warnings = append(warnings, fmt.Sprintf(
				"repository %s: could not inspect stash entries for untracked files (%s); stash entries may still contain untracked files",
				repository.Metadata.ID, err))
			continue
		}
		notes = append(notes, repositoryNotes...)
		warnings = append(warnings, repositoryWarnings...)
	}
	return notes, warnings, nil
}

func notesForRepository(sourceArchive string, manifest *core.ArchiveManifest, repository *core.RepositoryManifest, cancelled ArchivePurgeCancel) ([]StashUntrackedNote, []string, error) {
	if err := stopIfCancelled(cancelled); err != nil {
		return nil, nil, err
	}
	var descriptor *core.PayloadDescriptor
	for index := range manifest.Payloads {
		if manifest.Payloads[index].Path == repository.CommittedBundlePath {
			descriptor = &manifest.Payloads[index]
			break
		}
	}
	if descriptor == nil {
		slog.Warn("stash inspection skipped a repository with no bundle payload",
			"repository_id", repository.Metadata.ID)
		return nil, []string{fmt.Sprintf(
			"repository %s: could not inspect stash entries (bundle payload missing); stash entries may still contain untracked files",
			repository.Metadata.ID)}, nil
	}
	if descriptor.Size == 0 {
		return nil, nil, nil
	}
	// Shards are concatenated in manifest order onto a temporary file. The
	// pack walker streams that file and keeps only commits, so a repository
	// pack is never buffered in memory.
	spool, err := spoolBundle(sourceArchive, descriptor, cancelled)
	if err != nil {
		if PurgeWasCancelled(err) {
			return nil, nil, err
		}
		slog.Warn("could not read the bundle while inspecting stash entries; the purge continues",
			"repository_id", repository.Metadata.ID, "error", err.Error())
		return nil, []string{fmt.Sprintf(
			"repository %s: could not read bundle to inspect stash entries (%s); stash entries may still contain untracked files",
			repository.Metadata.ID, err)}, nil
	}
	defer func() {
		spool.Close()
		os.Remove(spool.Name())
	}()
	reader := bufio.NewReader(spool)
	isGit, err := bundlePrefixIsGit(reader)
	if err != nil {
		slog.Warn("could not read the bundle while inspecting stash entries; the purge continues",
			"repository_id", repository.Metadata.ID, "error", err.Error())
		return nil, []string{fmt.Sprintf(
			"repository %s: could not read bundle prefix to inspect stash entries (%s); stash entries may still contain untracked files",
			repository.Metadata.ID, err)}, nil
	}
	if !isGit {
		return nil, nil, nil
	}
	notes, warnings, err := inspectBundleStashesReader(reader, repository)
	if err != nil {
		slog.Warn("could not inspect stash entries for untracked files; the purge continues",
			"repository_id", repository.Metadata.ID, "error", err.Error())
		return nil, []string{fmt.Sprintf(
			"repository %s: could not inspect stash entries for untracked files (%s); stash entries may still contain untracked files",
			repository.Metadata.ID, err)}, nil
	}
	return notes, warnings, nil
}

func bundlePrefixIsGit(reader *bufio.Reader) (bool, error) {
	const magicLen = 16
	prefix, err := reader.Peek(magicLen)
	if err != nil {
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
			return false, nil
		}
		return false, fmt.Errorf("read bundle prefix: %w", err)
	}
	return bytes.HasPrefix(prefix, []byte("# v2 git bundle\n")) || bytes.HasPrefix(prefix, []byte("# v3 git bundle\n")), nil
}

// spoolBundle copies every shard of one bundle, in manifest order, to a
// temporary file.
//
// Hashing happens while the bytes are copied. The returned file is rewound.
// Disk use follows the bundle; the caller does not hold its bytes. The
// caller closes and removes the file.
func spoolBundle(sourceArchive string, descriptor *core.PayloadDescriptor, cancelled ArchivePurgeCancel) (*os.File, error) {
	archive, err := openArchiveProbed(sourceArchive, cancelled)
	if err != nil {
		return nil, err
	}
	defer archive.close()
	spool, err := os.CreateTemp("", ".tmp")
	if err != nil {
		return nil, fmt.Errorf("create a temporary git bundle spool: %w", err)
	}
	keep := false
	defer func() {
		if !keep {
			spool.Close()
			os.Remove(spool.Name())
		}
	}()
	whole := sha256.New()
	if len(descriptor.Parts) == 0 {
		if _, err := copyBundleEntry(archive, descriptor.Path, descriptor.Size, descriptor.Mode, spool, whole); err != nil {
			return nil, err
		}
	} else {
		var covered uint64
		for index := range descriptor.Parts {
			part := &descriptor.Parts[index]
			digest, err := copyBundleEntry(archive, part.Path, part.Size, descriptor.Mode, spool, whole)
			if err != nil {
				return nil, err
			}
			if digest != part.Sha256 {
				return nil, fmt.Errorf("SHA-256 mismatch for payload part '%s'", part.Path)
			}
			if covered+part.Size < covered {
				return nil, errors.New("payload part sizes overflow")
			}
			covered += part.Size
		}
		if covered != descriptor.Size {
			return nil, fmt.Errorf("payload parts do not cover '%s'", descriptor.Path)
		}
	}
	if core.LowerHex(whole.Sum(nil)) != descriptor.Sha256 {
		return nil, fmt.Errorf("SHA-256 mismatch for payload '%s'", descriptor.Path)
	}
	if _, err := spool.Seek(0, io.SeekStart); err != nil {
		return nil, fmt.Errorf("rewind git bundle spool: %w", err)
	}
	keep = true
	return spool, nil
}

func copyBundleEntry(archive *archiveZip, path string, expectedSize uint64, expectedMode uint32, output io.Writer, whole io.Writer) (string, error) {
	entry, err := archive.byName(path)
	if err != nil {
		return "", fmt.Errorf("archive is missing payload '%s': %w", path, err)
	}
	if zipIsDir(entry.Name) {
		return "", fmt.Errorf("archive payload '%s' is a directory", path)
	}
	enclosed, ok := zipEnclosedName(entry.Name)
	if !ok {
		return "", fmt.Errorf("unsafe ZIP entry path '%s'", entry.Name)
	}
	slashed, err := slashPath(enclosed)
	if err != nil {
		return "", err
	}
	if slashed != path {
		return "", fmt.Errorf("archive payload path does not match '%s'", path)
	}
	if entry.UncompressedSize64 != expectedSize {
		return "", fmt.Errorf("size mismatch for payload '%s'", path)
	}
	if zipModeOrDefault(&entry.FileHeader) != expectedMode {
		return "", fmt.Errorf("mode mismatch for payload '%s'", path)
	}
	reader, err := entry.Open()
	if err != nil {
		return "", fmt.Errorf("read ZIP entry '%s': %w", path, err)
	}
	defer reader.Close()
	part := sha256.New()
	buffer := make([]byte, 8*1024)
	var copied uint64
	for {
		read, err := reader.Read(buffer)
		if read > 0 {
			chunk := buffer[:read]
			if _, err := output.Write(chunk); err != nil {
				return "", fmt.Errorf("write git bundle spool: %w", err)
			}
			part.Write(chunk)
			whole.Write(chunk)
			copied = saturatingAdd(copied, uint64(read))
			if copied > expectedSize {
				return "", fmt.Errorf("payload '%s' exceeds its declared size", path)
			}
		}
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return "", fmt.Errorf("read ZIP entry '%s': %w", path, err)
		}
	}
	if copied != expectedSize {
		return "", fmt.Errorf("size mismatch for payload '%s'", path)
	}
	return core.LowerHex(part.Sum(nil)), nil
}

// inspectBundleStashes is the test entry of inspectBundleStashesReader.
func inspectBundleStashes(data []byte, repository *core.RepositoryManifest) ([]StashUntrackedNote, []string, error) {
	return inspectBundleStashesReader(bufio.NewReader(bytes.NewReader(data)), repository)
}

func inspectBundleStashesReader(reader *bufio.Reader, repository *core.RepositoryManifest) ([]StashUntrackedNote, []string, error) {
	// Clone bundles are thin: `git bundle unbundle` refuses to run until the
	// prerequisite commits exist, and purge does not have that checkout.
	// Stash parents are stored on the commit object, so read the pack
	// directly. A delta whose base is the missing prerequisite is skipped;
	// that still must not fail the purge, but is reported as a warning so
	// the caller knows that stash commit could not be verified.
	parents, err := commitParentCountsReader(reader)
	if err != nil {
		return nil, nil, fmt.Errorf("read stash commits from the bundle pack: %w", err)
	}
	notes := []StashUntrackedNote{}
	warnings := []string{}
	for _, entry := range repository.Metadata.StashStack {
		parentCount, ok := parents[strings.ToLower(entry.Commit)]
		if !ok {
			slog.Warn("stash commit is not in the archived bundle",
				"repository_id", repository.Metadata.ID, "commit", entry.Commit)
			warnings = append(warnings, fmt.Sprintf(
				"repository %s: could not inspect stash commit %s (%s) in the archived bundle; it may still contain untracked files",
				repository.Metadata.ID, entry.Commit, entry.Message))
			continue
		}
		// `git rev-list --parents` prints the commit and then each parent.
		// More than three words means a third parent, which is the untracked
		// tree.
		if parentCount >= 3 {
			notes = append(notes, StashUntrackedNote{
				RepositoryID: repository.Metadata.ID,
				Commit:       entry.Commit,
				Message:      entry.Message,
			})
		}
	}
	return notes, warnings, nil
}

const maxBundleHeaderBytes = 16 * 1024 * 1024

// commitParentCounts is the test entry of commitParentCountsReader.
func commitParentCounts(data []byte) (map[string]int, error) {
	return commitParentCountsReader(bufio.NewReader(bytes.NewReader(data)))
}

func commitParentCountsReader(reader *bufio.Reader) (map[string]int, error) {
	sha256Format, err := readBundleHeader(reader)
	if err != nil {
		return nil, err
	}
	hashLen := 20
	if sha256Format {
		hashLen = 32
	}
	// Position counts bytes consumed from the pack, not the bundle header.
	pack := &countingReader{inner: reader}
	walked, err := readPackCommits(pack, hashLen)
	if err != nil {
		return nil, err
	}
	return resolveCommitParents(walked, sha256Format)
}

func readBundleHeader(reader *bufio.Reader) (bool, error) {
	sha256Format := false
	total := 0
	first := true
	for {
		line, err := readLimitedLine(reader, maxBundleHeaderBytes-total)
		if err != nil {
			return false, err
		}
		if len(line) == 0 {
			return false, errors.New("git bundle has no header terminator")
		}
		total += len(line)
		if first {
			if !bytes.HasPrefix(line, []byte("# v2 git bundle\n")) && !bytes.HasPrefix(line, []byte("# v3 git bundle\n")) {
				return false, errors.New("git bundle has an unsupported header")
			}
			first = false
		}
		if bytes.Equal(line, []byte("\n")) {
			return sha256Format, nil
		}
		body := bytes.TrimSuffix(line, []byte("\n"))
		if bytes.Equal(body, []byte("@object-format=sha256")) {
			sha256Format = true
		}
	}
}

func readLimitedLine(reader *bufio.Reader, remaining int) ([]byte, error) {
	var line []byte
	for {
		chunk, err := reader.ReadSlice('\n')
		if len(chunk) > 0 {
			if len(line)+len(chunk) > remaining {
				return nil, fmt.Errorf("git bundle header exceeds %d bytes", maxBundleHeaderBytes)
			}
			line = append(line, chunk...)
		}
		switch {
		case err == nil, errors.Is(err, io.EOF):
			return line, nil
		case errors.Is(err, bufio.ErrBufferFull):
			continue
		default:
			return nil, fmt.Errorf("read git bundle header: %w", err)
		}
	}
}

// countingReader counts bytes consumed from a pack. It is a byte reader, so
// a zlib decoder reading through it consumes exactly its own stream and the
// position stays exact.
type countingReader struct {
	inner *bufio.Reader
	pos   int
}

func (r *countingReader) Read(buffer []byte) (int, error) {
	read, err := r.inner.Read(buffer)
	r.pos += read
	return read, err
}

func (r *countingReader) ReadByte() (byte, error) {
	b, err := r.inner.ReadByte()
	if err == nil {
		r.pos++
	}
	return b, err
}

func (r *countingReader) position() int {
	return r.pos
}

type packWalk struct {
	objects []packedObject
	// nonCommits are the offsets of trees, blobs, and tags. A delta based on
	// one of these is not a commit.
	nonCommits map[int]struct{}
}

// packedObject is a commit (body set, delta nil) or a delta.
type packedObject struct {
	offset int
	body   []byte
	delta  *packedDelta
}

// packedDelta has a base by offset (baseOID empty) or by object id.
type packedDelta struct {
	baseOffset int
	baseOID    string
	delta      []byte
}

func readPackCommits(pack *countingReader, hashLen int) (packWalk, error) {
	var header [12]byte
	if _, err := io.ReadFull(pack, header[:]); err != nil {
		return packWalk{}, fmt.Errorf("git pack is truncated: %w", err)
	}
	if !bytes.HasPrefix(header[:], []byte("PACK")) {
		return packWalk{}, errors.New("git pack has a bad signature")
	}
	version := binary.BigEndian.Uint32(header[4:8])
	if version != 2 && version != 3 {
		return packWalk{}, fmt.Errorf("unsupported git pack version %d", version)
	}
	count := int(binary.BigEndian.Uint32(header[8:12]))
	walk := packWalk{nonCommits: map[int]struct{}{}}
	for range count {
		headerAt := pack.position()
		kind, size, err := readPackTypeAndSize(pack)
		if err != nil {
			return packWalk{}, err
		}
		switch {
		case kind == 1:
			if size > maxInspectedCommitBytes {
				slog.Warn("skipping an oversized commit while inspecting stash entries", "size", size)
				if err := discardZlib(pack); err != nil {
					return packWalk{}, err
				}
				walk.nonCommits[headerAt] = struct{}{}
				continue
			}
			body, err := inflateExact(pack, size)
			if err != nil {
				return packWalk{}, err
			}
			walk.objects = append(walk.objects, packedObject{offset: headerAt, body: body})
		case kind >= 2 && kind <= 4:
			if err := discardZlib(pack); err != nil {
				return packWalk{}, err
			}
			walk.nonCommits[headerAt] = struct{}{}
		case kind == 6 || kind == 7:
			delta := &packedDelta{}
			if kind == 6 {
				ofs, err := readOfsDelta(pack)
				if err != nil {
					return packWalk{}, err
				}
				if ofs == 0 || ofs > uint64(headerAt) {
					return packWalk{}, errors.New("ofs delta points outside the pack")
				}
				delta.baseOffset = headerAt - int(ofs)
			} else {
				oid := make([]byte, hashLen)
				if _, err := io.ReadFull(pack, oid); err != nil {
					return packWalk{}, fmt.Errorf("ref delta object id is truncated: %w", err)
				}
				delta.baseOID = core.LowerHex(oid)
			}
			// An ofs-delta of a blob or tree is the same type. Drop it now so
			// a deltified pack does not keep those bodies.
			_, knownNonCommit := walk.nonCommits[delta.baseOffset]
			knownNonCommit = knownNonCommit && delta.baseOID == ""
			if knownNonCommit || size > maxInspectedCommitBytes {
				if err := discardZlib(pack); err != nil {
					return packWalk{}, err
				}
				walk.nonCommits[headerAt] = struct{}{}
				continue
			}
			body, err := inflateExact(pack, size)
			if err != nil {
				return packWalk{}, err
			}
			delta.delta = body
			walk.objects = append(walk.objects, packedObject{offset: headerAt, delta: delta})
		default:
			return packWalk{}, fmt.Errorf("git pack contains object type %d", kind)
		}
	}
	trailer := make([]byte, hashLen)
	if _, err := io.ReadFull(pack, trailer); err != nil {
		return packWalk{}, fmt.Errorf("git pack trailer is truncated: %w", err)
	}
	var extra [1]byte
	extraLen, err := pack.Read(extra[:])
	if err != nil && !errors.Is(err, io.EOF) {
		return packWalk{}, fmt.Errorf("read git pack trailer: %w", err)
	}
	if extraLen != 0 {
		return packWalk{}, fmt.Errorf("git pack trailer is longer than %d bytes", hashLen)
	}
	return walk, nil
}

func resolveCommitParents(walk packWalk, sha256Format bool) (map[string]int, error) {
	objects, nonCommits := walk.objects, walk.nonCommits
	byOffset := map[int][]byte{}
	byOID := map[string][]byte{}
	var pending []int
	for index := range objects {
		object := &objects[index]
		if object.delta == nil {
			oid := gitObjectID("commit", object.body, sha256Format)
			byOID[oid] = object.body
			byOffset[object.offset] = object.body
			continue
		}
		pending = append(pending, index)
	}

	passes := 0
	for len(pending) > 0 && passes <= len(objects) {
		passes++
		pendingOffsets := map[int]struct{}{}
		for _, index := range pending {
			pendingOffsets[objects[index].offset] = struct{}{}
		}
		var next []int
		progress := false
		for _, index := range pending {
			object := &objects[index]
			delta := object.delta
			if delta.baseOID == "" {
				if _, ok := nonCommits[delta.baseOffset]; ok {
					nonCommits[object.offset] = struct{}{}
					progress = true
					continue
				}
			}
			var base []byte
			var found bool
			if delta.baseOID == "" {
				base, found = byOffset[delta.baseOffset]
			} else {
				base, found = byOID[delta.baseOID]
			}
			if !found {
				waiting := true
				if delta.baseOID == "" {
					_, waiting = pendingOffsets[delta.baseOffset]
				}
				// The base commit may be a later delta whose id is not known
				// yet.
				if waiting {
					next = append(next, index)
				}
				continue
			}
			body, err := applyDelta(base, delta.delta)
			if err != nil {
				slog.Warn("could not apply a pack delta while inspecting stash commits", "error", err.Error())
				nonCommits[object.offset] = struct{}{}
				progress = true
				continue
			}
			oid := gitObjectID("commit", body, sha256Format)
			byOID[oid] = body
			byOffset[object.offset] = body
			progress = true
		}
		if !progress {
			break
		}
		pending = next
	}

	parents := make(map[string]int, len(byOID))
	for oid, body := range byOID {
		parents[oid] = commitParentCount(body)
	}
	return parents, nil
}

func commitParentCount(body []byte) int {
	count := 0
	for line := range bytes.SplitSeq(body, []byte("\n")) {
		if len(line) == 0 {
			break
		}
		if bytes.HasPrefix(line, []byte("parent ")) {
			count++
		}
	}
	return count
}

// gitObjectID is the object id Git gives an object of kind with body.
func gitObjectID(kind string, body []byte, sha256Format bool) string {
	encoded := append(fmt.Appendf(nil, "%s %d", kind, len(body)), 0)
	encoded = append(encoded, body...)
	if sha256Format {
		digest := sha256.Sum256(encoded)
		return core.LowerHex(digest[:])
	}
	digest := sha1.Sum(encoded)
	return core.LowerHex(digest[:])
}

func readU8(pack *countingReader) (byte, error) {
	b, err := pack.ReadByte()
	if err != nil {
		return 0, fmt.Errorf("truncated pack: %w", err)
	}
	return b, nil
}

func readPackTypeAndSize(pack *countingReader) (byte, uint64, error) {
	b, err := readU8(pack)
	if err != nil {
		return 0, 0, fmt.Errorf("truncated pack object header: %w", err)
	}
	kind := (b >> 4) & 7
	size := uint64(b & 0x0f)
	shift := 4
	for b&0x80 != 0 {
		if shift > 60 {
			return 0, 0, errors.New("pack object size is too large")
		}
		b, err = readU8(pack)
		if err != nil {
			return 0, 0, fmt.Errorf("truncated pack object size: %w", err)
		}
		size |= uint64(b&0x7f) << shift
		shift += 7
	}
	return kind, size, nil
}

func readOfsDelta(pack *countingReader) (uint64, error) {
	b, err := readU8(pack)
	if err != nil {
		return 0, fmt.Errorf("truncated ofs delta: %w", err)
	}
	offset := uint64(b & 0x7f)
	for b&0x80 != 0 {
		b, err = readU8(pack)
		if err != nil {
			return 0, fmt.Errorf("truncated ofs delta: %w", err)
		}
		// Rust's checked_shl checks only the shift amount, so the shift
		// itself wraps; the two additions are checked.
		if offset == ^uint64(0) {
			return 0, errors.New("ofs delta offset overflows")
		}
		shifted := (offset + 1) << 7
		if shifted+uint64(b&0x7f) < shifted {
			return 0, errors.New("ofs delta offset overflows")
		}
		offset = shifted + uint64(b&0x7f)
	}
	return offset, nil
}

func inflateExact(pack *countingReader, size uint64) ([]byte, error) {
	decoder, err := zlib.NewReader(pack)
	if err != nil {
		return nil, fmt.Errorf("inflate pack object: %w", err)
	}
	body := make([]byte, size)
	if _, err := io.ReadFull(decoder, body); err != nil {
		return nil, fmt.Errorf("inflate pack object: %w", err)
	}
	var extra [1]byte
	extraLen, err := decoder.Read(extra[:])
	if err != nil && !errors.Is(err, io.EOF) {
		return nil, fmt.Errorf("finish pack object: %w", err)
	}
	// A zlib reader never answers a non-empty read with no bytes and no
	// error, so no extra byte means its stream, checksum included, ended.
	if extraLen != 0 {
		return nil, errors.New("pack object is longer than its header declares")
	}
	return body, nil
}

func discardZlib(pack *countingReader) error {
	decoder, err := zlib.NewReader(pack)
	if err != nil {
		return fmt.Errorf("skip pack object: %w", err)
	}
	buffer := make([]byte, 8*1024)
	for {
		read, err := decoder.Read(buffer)
		if read > 0 {
			noteDiscardedRead(read)
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return fmt.Errorf("skip pack object: %w", err)
		}
	}
}

// discardReads is the peak read and total uncompressed bytes skipped while
// inspecting a bundle. Rust keeps these only in test builds; recording them
// here costs one lock per 8 KiB discarded.
var discardReads struct {
	sync.Mutex
	peak  int
	total uint64
}

func noteDiscardedRead(read int) {
	discardReads.Lock()
	defer discardReads.Unlock()
	discardReads.peak = max(discardReads.peak, read)
	discardReads.total = saturatingAdd(discardReads.total, uint64(read))
}

// resetBundleDiscardStats and bundleDiscardStats read the peak read and
// total uncompressed bytes skipped while inspecting a bundle. The peak is the
// largest single discard read. A blob buffered whole would exceed the 8 KiB
// discard buffer.
func resetBundleDiscardStats() {
	discardReads.Lock()
	defer discardReads.Unlock()
	discardReads.peak, discardReads.total = 0, 0
}

func bundleDiscardStats() (int, uint64) {
	discardReads.Lock()
	defer discardReads.Unlock()
	return discardReads.peak, discardReads.total
}

func applyDelta(base, delta []byte) ([]byte, error) {
	sourceSize, pos, err := readDeltaSize(delta, 0)
	if err != nil {
		return nil, err
	}
	targetSize, pos, err := readDeltaSize(delta, pos)
	if err != nil {
		return nil, err
	}
	if sourceSize != len(base) {
		return nil, fmt.Errorf("delta base is %d bytes, not %d", len(base), sourceSize)
	}
	if targetSize > maxInspectedCommitBytes {
		return nil, fmt.Errorf("delta target is %d bytes", targetSize)
	}
	out := make([]byte, 0, targetSize)
	for pos < len(delta) {
		command := delta[pos]
		pos++
		switch {
		case command == 0:
			return nil, errors.New("delta contains an empty command")
		case command&0x80 != 0:
			offset, size := 0, 0
			for bit := range 4 {
				if command&(1<<bit) != 0 {
					if pos >= len(delta) {
						return nil, errors.New("truncated delta copy offset")
					}
					offset |= int(delta[pos]) << (8 * bit)
					pos++
				}
			}
			for bit := range 3 {
				if command&(1<<(4+bit)) != 0 {
					if pos >= len(delta) {
						return nil, errors.New("truncated delta copy length")
					}
					size |= int(delta[pos]) << (8 * bit)
					pos++
				}
			}
			if size == 0 {
				size = 0x10000
			}
			end := offset + size
			if end < offset {
				return nil, errors.New("delta copy overflows")
			}
			if end > len(base) {
				return nil, errors.New("delta copy exceeds its base")
			}
			out = append(out, base[offset:end]...)
		default:
			length := int(command)
			if pos+length > len(delta) {
				return nil, errors.New("truncated delta insert")
			}
			out = append(out, delta[pos:pos+length]...)
			pos += length
		}
		if len(out) > targetSize {
			return nil, errors.New("delta expanded past its target size")
		}
	}
	if len(out) != targetSize {
		return nil, fmt.Errorf("delta expanded to %d bytes, not %d", len(out), targetSize)
	}
	return out, nil
}

func readDeltaSize(delta []byte, pos int) (int, int, error) {
	size := 0
	shift := 0
	for {
		if pos >= len(delta) {
			return 0, 0, errors.New("truncated delta size")
		}
		if shift > 60 {
			return 0, 0, errors.New("delta size is too large")
		}
		b := delta[pos]
		pos++
		size |= int(b&0x7f) << shift
		if b&0x80 == 0 {
			break
		}
		shift += 7
	}
	return size, pos, nil
}
