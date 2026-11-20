// Port of mj-checkpoint/src/checkpoint/capture.rs.

package checkpoint

import (
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"io"
	"io/fs"
	"maps"
	"math"
	"math/bits"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// captureCheckpointStageManifest is the stage's manifest file name (Rust
// CHECKPOINT_STAGE_MANIFEST; prefixed because the stage manifest type owns
// the plain name).
const captureCheckpointStageManifest = "stage.json"

// CheckpointCaptureTemporaryPrefix is the name a capture gives the directory
// it fills before sealing it as the stage, beside the stage. A capture that
// is killed leaves it behind.
const CheckpointCaptureTemporaryPrefix = ".checkpoint-capture-"

// CaptureCheckpoint captures mutable target-owned state into a sealed,
// uncompressed generation. The caller holds the relay barrier only for this
// operation; packaging the generation is intentionally a separate command.
func CaptureCheckpoint(spec *CheckpointCaptureSpec, git GitCommandRunner) (CapturedCheckpoint, error) {
	return CaptureCheckpointWithNativeState(spec, git, noNativeCheckpointState{})
}

func CaptureCheckpointWithNativeState(spec *CheckpointCaptureSpec, git GitCommandRunner, nativeState NativeCheckpointState) (CapturedCheckpoint, error) {
	if spec.ProtocolVersion != CheckpointStagingProtocolVersion {
		return CapturedCheckpoint{}, fmt.Errorf("unsupported checkpoint staging protocol version %d; worker supports %d",
			spec.ProtocolVersion, CheckpointStagingProtocolVersion)
	}
	resolved := *spec
	var err error
	if resolved.RelayRoot, err = ResolveTargetPath(resolved.RelayRoot); err != nil {
		return CapturedCheckpoint{}, err
	}
	requestedHome, err := ResolveTargetPath(resolved.HarnessHome)
	if err != nil {
		return CapturedCheckpoint{}, err
	}
	if resolved.HarnessHome, err = CheckpointHarnessHome(resolved.RelayRoot, requestedHome); err != nil {
		return CapturedCheckpoint{}, err
	}
	if resolved.WorkspaceRoot, err = ResolveTargetPath(resolved.WorkspaceRoot); err != nil {
		return CapturedCheckpoint{}, err
	}
	if resolved.StagePath, err = ResolveTargetPath(resolved.StagePath); err != nil {
		return CapturedCheckpoint{}, err
	}
	if err := validateCaptureSpec(&resolved); err != nil {
		return CapturedCheckpoint{}, err
	}

	sourceFingerprintBefore, err := checkpointSourceFingerprint(resolved.HarnessHome, resolved.WorkspaceRoot, resolved.ReportDirectories)
	if err != nil {
		return CapturedCheckpoint{}, err
	}
	if resolved.RefreshExisting && checkpointIsDir(resolved.StagePath) {
		captured, err := refreshCheckpointStage(&resolved, sourceFingerprintBefore, git)
		if err != nil {
			return CapturedCheckpoint{}, err
		}
		if captured != nil {
			return *captured, nil
		}
		if err := os.RemoveAll(resolved.StagePath); err != nil {
			return CapturedCheckpoint{}, fmt.Errorf("remove stale checkpoint prestage %s: %w", resolved.StagePath, err)
		}
	}

	nativeArtifacts, omittedReports, err := collectCheckpointNativeArtifactsWithReports(
		&resolved.Session,
		resolved.RelayRoot,
		resolved.HarnessHome,
		resolved.WorkspaceRoot,
		resolved.ReportDirectories,
		resolved.AllowEmptyNative,
		nativeState,
	)
	if err != nil {
		return CapturedCheckpoint{}, err
	}

	parent, ok := checkpointParent(resolved.StagePath)
	if !ok {
		return CapturedCheckpoint{}, errors.New("checkpoint stage has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return CapturedCheckpoint{}, fmt.Errorf("create checkpoint stage parent %s: %w", parent, err)
	}
	if checkpointExists(resolved.StagePath) {
		return CapturedCheckpoint{}, fmt.Errorf("checkpoint stage already exists at %s", resolved.StagePath)
	}
	temporary, err := os.MkdirTemp(parent, CheckpointCaptureTemporaryPrefix+"*")
	if err != nil {
		return CapturedCheckpoint{}, fmt.Errorf("create temporary checkpoint stage in %s: %w", parent, err)
	}
	// Rust's TempDir removes the unsealed stage when the capture fails.
	sealed := false
	defer func() {
		if !sealed {
			os.RemoveAll(temporary)
		}
	}()

	stagedNative := make([]stagedNativeArtifact, 0, len(nativeArtifacts))
	var nativeBytes uint64
	for index := range nativeArtifacts {
		artifact := &nativeArtifacts[index]
		bodyPath := fmt.Sprintf("native/%08d", index)
		reader, err := artifact.Reader()
		if err != nil {
			return CapturedCheckpoint{}, err
		}
		size, err := writePrivateReader(temporary, bodyPath, reader, 0o600)
		reader.Close()
		if err != nil {
			return CapturedCheckpoint{}, err
		}
		total, carry := bits.Add64(nativeBytes, size, 0)
		if carry != 0 {
			return CapturedCheckpoint{}, errors.New("native checkpoint size overflow")
		}
		nativeBytes = total
		stagedNative = append(stagedNative, stagedNativeArtifact{
			RelativePath: artifact.RelativePath,
			Mode:         artifact.Mode,
			Size:         size,
			BodyPath:     bodyPath,
		})
	}
	// Native bytes are copied before repository collection, which can take
	// long enough for the harness to append more history; the stage must hold
	// the native state selected above, as it did when bodies were read eagerly.
	repositories, err := collectCheckpointRepositories(resolved.WorkspaceRoot, resolved.Repositories, git)
	if err != nil {
		return CapturedCheckpoint{}, err
	}
	repositoryBytes, err := checkpointRepositoryBytes(repositories)
	if err != nil {
		return CapturedCheckpoint{}, err
	}
	stagedRepositories, err := writeStagedRepositories(temporary, repositories)
	if err != nil {
		return CapturedCheckpoint{}, err
	}
	sourceFingerprintAfter, err := checkpointSourceFingerprint(resolved.HarnessHome, resolved.WorkspaceRoot, resolved.ReportDirectories)
	if err != nil {
		return CapturedCheckpoint{}, err
	}
	manifest := checkpointStageManifest{
		OmittedReports:  omittedReports,
		ProtocolVersion: CheckpointStagingProtocolVersion,
		Session:         resolved.Session,
		Target:          resolved.Target,
		Bundle:          resolved.Bundle,
		NativeArtifacts: stagedNative,
		Repositories:    stagedRepositories,
	}
	if sourceFingerprintBefore == sourceFingerprintAfter {
		manifest.SourceFingerprint = &sourceFingerprintAfter
	}
	body, err := checkpointMarshal(manifest)
	if err != nil {
		return CapturedCheckpoint{}, fmt.Errorf("serialize checkpoint stage manifest: %w", err)
	}
	if err := writePrivateFile(temporary, captureCheckpointStageManifest, body, 0o600); err != nil {
		return CapturedCheckpoint{}, err
	}
	if err := os.Rename(temporary, resolved.StagePath); err != nil {
		return CapturedCheckpoint{}, fmt.Errorf("seal checkpoint stage %s: %w", resolved.StagePath, err)
	}
	sealed = true

	return CapturedCheckpoint{
		OmittedReports:  omittedReports,
		StagePath:       resolved.StagePath,
		NativeBytes:     nativeBytes,
		RepositoryBytes: repositoryBytes,
		ReusedNative:    false,
	}, nil
}

// refreshCheckpointStage reuses a sealed prestage whose native sources are
// unchanged; nil when the prestage is stale.
func refreshCheckpointStage(spec *CheckpointCaptureSpec, sourceFingerprintBefore string, git GitCommandRunner) (*CapturedCheckpoint, error) {
	manifestBody, err := readStagedFile(spec.StagePath, captureCheckpointStageManifest, 8*1024*1024)
	if err != nil {
		return nil, err
	}
	var manifest checkpointStageManifest
	if err := json.Unmarshal(manifestBody, &manifest); err != nil {
		return nil, fmt.Errorf("parse checkpoint stage manifest: %w", err)
	}
	if manifest.ProtocolVersion != CheckpointStagingProtocolVersion {
		return nil, fmt.Errorf("unsupported sealed checkpoint stage version %d", manifest.ProtocolVersion)
	}
	if manifest.Session != spec.Session ||
		!checkpointSameTarget(&manifest.Target, &spec.Target) ||
		manifest.Bundle != spec.Bundle ||
		manifest.SourceFingerprint == nil || *manifest.SourceFingerprint != sourceFingerprintBefore {
		return nil, nil
	}

	repositories, err := collectCheckpointRepositories(spec.WorkspaceRoot, spec.Repositories, git)
	if err != nil {
		return nil, err
	}
	repositoryBytes, err := checkpointRepositoryBytes(repositories)
	if err != nil {
		return nil, err
	}
	sourceFingerprintAfter, err := checkpointSourceFingerprint(spec.HarnessHome, spec.WorkspaceRoot, spec.ReportDirectories)
	if err != nil {
		return nil, err
	}
	if sourceFingerprintAfter != sourceFingerprintBefore {
		return nil, nil
	}
	repositoriesRoot := core.Join(spec.StagePath, "repositories")
	if checkpointExists(repositoriesRoot) {
		if err := os.RemoveAll(repositoriesRoot); err != nil {
			return nil, fmt.Errorf("replace prestaged repository state %s: %w", repositoriesRoot, err)
		}
	}
	if manifest.Repositories, err = writeStagedRepositories(spec.StagePath, repositories); err != nil {
		return nil, err
	}
	manifest.SourceFingerprint = &sourceFingerprintAfter
	body, err := checkpointMarshal(manifest)
	if err != nil {
		return nil, fmt.Errorf("serialize refreshed checkpoint stage manifest: %w", err)
	}
	if err := writePrivateFile(spec.StagePath, captureCheckpointStageManifest, body, 0o600); err != nil {
		return nil, err
	}
	var nativeBytes uint64
	for _, artifact := range manifest.NativeArtifacts {
		total, carry := bits.Add64(nativeBytes, artifact.Size, 0)
		if carry != 0 {
			return nil, errors.New("native checkpoint size overflow")
		}
		nativeBytes = total
	}
	return &CapturedCheckpoint{
		OmittedReports:  manifest.OmittedReports,
		StagePath:       spec.StagePath,
		NativeBytes:     nativeBytes,
		RepositoryBytes: repositoryBytes,
		ReusedNative:    true,
	}, nil
}

// checkpointSameTarget is TargetManifest's derived equality: a missing and
// an empty details map are the same BTreeMap to Rust.
func checkpointSameTarget(left, right *core.TargetManifest) bool {
	return left.TemplateID == right.TemplateID &&
		left.TargetKind == right.TargetKind &&
		maps.Equal(left.Details, right.Details)
}

func checkpointRepositoryBytes(repositories []RepositorySnapshot) (uint64, error) {
	var total uint64
	add := func(payloads ...[]byte) bool {
		for _, payload := range payloads {
			sum, carry := bits.Add64(total, uint64(len(payload)), 0)
			if carry != 0 {
				return false
			}
			total = sum
		}
		return true
	}
	overflow := errors.New("repository checkpoint size overflow")
	for i := range repositories {
		repository := &repositories[i]
		if !add(repository.CommittedBundle, repository.StagedPatch, repository.UnstagedPatch, repository.UntrackedTar) {
			return 0, overflow
		}
		if payloads := repository.OperationPayloads; payloads != nil &&
			!add(payloads.ConflictedWorktreeTar, payloads.StateTar) {
			return 0, overflow
		}
		for j := range repository.LinkedWorktrees {
			worktree := &repository.LinkedWorktrees[j]
			if !add(worktree.StagedPatch, worktree.UnstagedPatch, worktree.UntrackedTar) {
				return 0, overflow
			}
			if payloads := worktree.OperationPayloads; payloads != nil &&
				!add(payloads.ConflictedWorktreeTar, payloads.StateTar) {
				return 0, overflow
			}
		}
	}
	return total, nil
}

// writeStagedOperationPayloads writes a Git operation's two payloads under
// root and returns their paths, or nils when there is no operation.
func writeStagedOperationPayloads(stageRoot string, root string, payloads *GitOperationPayloads) (*string, *string, error) {
	if payloads == nil {
		return nil, nil, nil
	}
	worktreePath := core.Join(root, "conflicted-worktree.tar")
	statePath := core.Join(root, "operation-state.tar")
	if err := writePrivateFile(stageRoot, worktreePath, payloads.ConflictedWorktreeTar, 0o600); err != nil {
		return nil, nil, err
	}
	if err := writePrivateFile(stageRoot, statePath, payloads.StateTar, 0o600); err != nil {
		return nil, nil, err
	}
	return &worktreePath, &statePath, nil
}

func writeStagedRepositories(stageRoot string, repositories []RepositorySnapshot) ([]stagedRepository, error) {
	staged := make([]stagedRepository, 0, len(repositories))
	for index := range repositories {
		repository := &repositories[index]
		root := fmt.Sprintf("repositories/%08d", index)
		committedBundlePath := core.Join(root, "committed.bundle")
		stagedPatchPath := core.Join(root, "staged.patch")
		unstagedPatchPath := core.Join(root, "unstaged.patch")
		untrackedTarPath := core.Join(root, "untracked.tar")
		for _, file := range []struct {
			path string
			data []byte
		}{
			{committedBundlePath, repository.CommittedBundle},
			{stagedPatchPath, repository.StagedPatch},
			{unstagedPatchPath, repository.UnstagedPatch},
			{untrackedTarPath, repository.UntrackedTar},
		} {
			if err := writePrivateFile(stageRoot, file.path, file.data, 0o600); err != nil {
				return nil, err
			}
		}
		conflictedWorktreeTarPath, operationStateTarPath, err := writeStagedOperationPayloads(stageRoot, root, repository.OperationPayloads)
		if err != nil {
			return nil, err
		}
		linkedWorktrees := make([]stagedLinkedWorktree, 0, len(repository.LinkedWorktrees))
		for worktreeIndex := range repository.LinkedWorktrees {
			worktree := &repository.LinkedWorktrees[worktreeIndex]
			worktreeRoot := core.Join(root, fmt.Sprintf("worktrees/%08d", worktreeIndex))
			stagedPatchPath := core.Join(worktreeRoot, "staged.patch")
			unstagedPatchPath := core.Join(worktreeRoot, "unstaged.patch")
			untrackedTarPath := core.Join(worktreeRoot, "untracked.tar")
			for _, file := range []struct {
				path string
				data []byte
			}{
				{stagedPatchPath, worktree.StagedPatch},
				{unstagedPatchPath, worktree.UnstagedPatch},
				{untrackedTarPath, worktree.UntrackedTar},
			} {
				if err := writePrivateFile(stageRoot, file.path, file.data, 0o600); err != nil {
					return nil, err
				}
			}
			conflictedWorktreeTarPath, operationStateTarPath, err := writeStagedOperationPayloads(stageRoot, worktreeRoot, worktree.OperationPayloads)
			if err != nil {
				return nil, err
			}
			linkedWorktrees = append(linkedWorktrees, stagedLinkedWorktree{
				Metadata:                  worktree.Metadata,
				StagedPatchPath:           stagedPatchPath,
				UnstagedPatchPath:         unstagedPatchPath,
				UntrackedTarPath:          untrackedTarPath,
				ConflictedWorktreeTarPath: conflictedWorktreeTarPath,
				OperationStateTarPath:     operationStateTarPath,
			})
		}
		staged = append(staged, stagedRepository{
			Metadata:                  repository.Metadata,
			CommittedBundlePath:       committedBundlePath,
			StagedPatchPath:           stagedPatchPath,
			UnstagedPatchPath:         unstagedPatchPath,
			UntrackedTarPath:          untrackedTarPath,
			ConflictedWorktreeTarPath: conflictedWorktreeTarPath,
			OperationStateTarPath:     operationStateTarPath,
			LinkedWorktrees:           linkedWorktrees,
		})
	}
	return staged, nil
}

func nativeSourceFingerprint(root string) (string, error) {
	digest := sha256.New()
	if err := fingerprintTree(root, root, digest); err != nil {
		return "", err
	}
	return core.LowerHex(digest.Sum(nil)), nil
}

// checkpointSourceFingerprint is what a prestaged generation's native
// artifacts were read from: the harness home and, when there is one, the
// sub-agent report directory. A report written after the prestage makes the
// prestage stale, like a harness file. Without a report directory it is the
// harness home's own fingerprint.
func checkpointSourceFingerprint(harnessHome string, workspaceRoot string, reportDirectories *[]string) (string, error) {
	roots, err := checkpointReportSources(workspaceRoot, reportDirectories)
	if err != nil {
		return "", err
	}
	if len(roots) == 0 {
		return nativeSourceFingerprint(harnessHome)
	}
	digest := sha256.New()
	if err := fingerprintTree(harnessHome, harnessHome, digest); err != nil {
		return "", err
	}
	for _, source := range roots {
		digest.Write([]byte("\x00sub-agent reports\x00"))
		digest.Write([]byte(checkpointLossy([]byte(source.prefix))))
		if err := fingerprintTree(source.root, source.root, digest); err != nil {
			return "", err
		}
	}
	return core.LowerHex(digest.Sum(nil)), nil
}

// checkpointReportSource is one report root and the prefix its files take
// in the archive (a Rust (PathBuf, PathBuf) pair, ordered root first).
type checkpointReportSource struct {
	root   string
	prefix string
}

// checkpointReportSources: both collection and invalidation use this exact
// ownership selection.
func checkpointReportSources(workspaceRoot string, directories *[]string) ([]checkpointReportSource, error) {
	var roots []checkpointReportSource
	if directories == nil {
		roots = append(roots, checkpointReportSource{root: core.Join(workspaceRoot, core.ReportRootDir), prefix: ""})
	} else {
		children := map[string]bool{}
		for _, directory := range *directories {
			directory, err := ResolveTargetPath(directory)
			if err != nil {
				return nil, err
			}
			child, ok := checkpointFileName(directory)
			if !ok {
				return nil, errors.New("report directory has no child id")
			}
			if err := ValidateComponent(checkpointLossy([]byte(child)), "report child id"); err != nil {
				return nil, err
			}
			if children[child] {
				return nil, errors.New("duplicate child report directory")
			}
			children[child] = true
			roots = append(roots, checkpointReportSource{root: directory, prefix: child})
		}
	}
	roots = slices.DeleteFunc(roots, func(source checkpointReportSource) bool {
		info, err := os.Lstat(source.root)
		return err != nil || !info.IsDir()
	})
	slices.SortStableFunc(roots, func(left, right checkpointReportSource) int {
		if order := checkpointPathCompare(left.root, right.root); order != 0 {
			return order
		}
		return checkpointPathCompare(left.prefix, right.prefix)
	})
	return roots, nil
}

// checkpointSaturatingAdd is u64::saturating_add.
func checkpointSaturatingAdd(left, right uint64) uint64 {
	sum, carry := bits.Add64(left, right, 0)
	if carry != 0 {
		return math.MaxUint64
	}
	return sum
}

// collectSubagentReports returns the newest regular report files that fit,
// plus metadata about the files left out.
func collectSubagentReports(workspaceRoot string, reportDirectories *[]string) ([]core.NativeArtifact, CheckpointReportOmissions, error) {
	roots, err := checkpointReportSources(workspaceRoot, reportDirectories)
	if err != nil {
		return nil, CheckpointReportOmissions{}, err
	}
	type reportFile struct {
		modified time.Time
		size     uint64
		path     string
		relative string
	}
	type pendingDirectory struct {
		directory string
		root      string
		prefix    string
	}
	var files []reportFile
	directories := map[string]uint64{}
	pending := make([]pendingDirectory, 0, len(roots))
	for _, source := range roots {
		pending = append(pending, pendingDirectory{directory: source.root, root: source.root, prefix: source.prefix})
	}
	for len(pending) > 0 {
		next := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		entries, err := os.ReadDir(next.directory)
		if err != nil {
			return nil, CheckpointReportOmissions{}, fmt.Errorf("scan sub-agent reports %s: %w", next.directory, err)
		}
		for _, entry := range entries {
			path := core.Join(next.directory, entry.Name())
			info, err := os.Lstat(path)
			if err != nil {
				return nil, CheckpointReportOmissions{}, err
			}
			if info.IsDir() {
				pending = append(pending, pendingDirectory{directory: path, root: next.root, prefix: next.prefix})
			} else if info.Mode().IsRegular() {
				stripped, ok := checkpointStripPrefix(path, next.root)
				if !ok {
					return nil, CheckpointReportOmissions{}, fmt.Errorf("report escaped its root: %w", errCheckpointPrefixNotFound)
				}
				relative := core.Join(next.prefix, stripped)
				if parent, ok := checkpointParent(relative); ok && parent != "" {
					child := checkpointLossy([]byte(checkpointComponents(relative)[0].text))
					directories[child] = checkpointSaturatingAdd(directories[child], uint64(info.Size()))
				}
				files = append(files, reportFile{modified: info.ModTime(), size: uint64(info.Size()), path: path, relative: relative})
			}
		}
	}
	slices.SortStableFunc(files, func(left, right reportFile) int {
		if order := right.modified.Compare(left.modified); order != 0 {
			return order
		}
		return checkpointPathCompare(left.path, right.path)
	})
	var total uint64
	artifacts := []core.NativeArtifact{}
	var omitted CheckpointReportOmissions
	for _, report := range files {
		if checkpointSaturatingAdd(total, report.size) > core.MaxCheckpointReportBytes {
			omitted.Files++
			omitted.Bytes = checkpointSaturatingAdd(omitted.Bytes, report.size)
			continue
		}
		data, grown, err := readSubagentReport(report.path, core.MaxCheckpointReportBytes-total+1, total)
		if err != nil {
			return nil, CheckpointReportOmissions{}, err
		}
		// Files can grow while prestaging without a barrier. Apply the bound
		// to the actual bytes too, rather than sealing an oversized
		// generation.
		if grown != nil {
			omitted.Files++
			omitted.Bytes = checkpointSaturatingAdd(omitted.Bytes, *grown)
			continue
		}
		total += uint64(len(data))
		artifacts = append(artifacts, core.NativeArtifact{
			RelativePath: core.Join(core.ArchiveReportDir, report.relative),
			Data:         core.NativeArtifactDataBytes(data),
			Mode:         0o600,
		})
	}
	if omitted.Files > 0 {
		omitted.LargestDirectories = make([]CheckpointReportDirectory, 0, len(directories))
		for childID, bytes := range directories {
			omitted.LargestDirectories = append(omitted.LargestDirectories, CheckpointReportDirectory{ChildID: childID, Bytes: bytes})
		}
		slices.SortStableFunc(omitted.LargestDirectories, func(a, b CheckpointReportDirectory) int {
			if a.Bytes != b.Bytes {
				if b.Bytes < a.Bytes {
					return -1
				}
				return 1
			}
			return strings.Compare(a.ChildID, b.ChildID)
		})
		if len(omitted.LargestDirectories) > 3 {
			omitted.LargestDirectories = omitted.LargestDirectories[:3]
		}
	}
	return artifacts, omitted, nil
}

// readSubagentReport reads at most limit bytes of the report at path. When
// the bytes read no longer fit after total, it returns the file's current
// size as grown instead of the data.
func readSubagentReport(path string, limit uint64, total uint64) (data []byte, grown *uint64, err error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, nil, fmt.Errorf("open sub-agent report %s: %w", path, err)
	}
	defer file.Close()
	reader := io.Reader(file)
	if limit <= math.MaxInt64 {
		reader = io.LimitReader(file, int64(limit))
	}
	data, err = io.ReadAll(reader)
	if err != nil {
		return nil, nil, fmt.Errorf("read sub-agent report %s: %w", path, err)
	}
	if checkpointSaturatingAdd(total, uint64(len(data))) > core.MaxCheckpointReportBytes {
		info, err := file.Stat()
		if err != nil {
			return nil, nil, fmt.Errorf("inspect growing report %s: %w", path, err)
		}
		size := uint64(info.Size())
		return nil, &size, nil
	}
	return data, nil, nil
}

func fingerprintTree(root string, path string, digest hash.Hash) error {
	entries, err := os.ReadDir(path)
	if err != nil {
		return fmt.Errorf("scan checkpoint source %s: %w", path, err)
	}
	// os.ReadDir sorts by name, as Rust sorts by DirEntry::file_name.
	for _, entry := range entries {
		path := core.Join(path, entry.Name())
		relative, ok := checkpointStripPrefix(path, root)
		if !ok {
			return fmt.Errorf("checkpoint source escaped its root: %w", errCheckpointPrefixNotFound)
		}
		info, err := os.Lstat(path)
		if err != nil {
			return fmt.Errorf("inspect checkpoint source %s: %w", path, err)
		}
		digest.Write([]byte(checkpointLossy([]byte(relative))))
		digest.Write([]byte{0})
		digest.Write(binary.LittleEndian.AppendUint64(nil, uint64(info.Size())))
		digest.Write(checkpointUnixNanosLE(info.ModTime()))
		if mode, ok := checkpointRawMode(info); ok {
			digest.Write(binary.LittleEndian.AppendUint32(nil, mode))
		}
		switch {
		case info.IsDir():
			digest.Write([]byte("directory"))
			if err := fingerprintTree(root, path, digest); err != nil {
				return err
			}
		case info.Mode().IsRegular():
			digest.Write([]byte("file"))
		default:
			digest.Write([]byte("other"))
		}
	}
	return nil
}

// checkpointUnixNanosLE is a modification time as Rust hashes it: the u128
// nanoseconds since the Unix epoch in little-endian order, zero before it.
func checkpointUnixNanosLE(modified time.Time) []byte {
	encoded := make([]byte, 16)
	seconds := modified.Unix()
	if seconds < 0 {
		return encoded
	}
	high, low := bits.Mul64(uint64(seconds), uint64(time.Second))
	low, carry := bits.Add64(low, uint64(modified.Nanosecond()), 0)
	high += carry
	binary.LittleEndian.PutUint64(encoded[:8], low)
	binary.LittleEndian.PutUint64(encoded[8:], high)
	return encoded
}

// PackCheckpoint packages a previously sealed target generation with the
// controller's canonical projection. This operation runs after ACP dispatch
// resumes.
func PackCheckpoint(spec *CheckpointPackSpec) (TargetCheckpoint, error) {
	started := time.Now()
	if spec.ProtocolVersion != CheckpointStagingProtocolVersion {
		return TargetCheckpoint{}, fmt.Errorf("unsupported checkpoint staging protocol version %d; worker supports %d",
			spec.ProtocolVersion, CheckpointStagingProtocolVersion)
	}
	if err := spec.CanonicalSession.Validate(); err != nil {
		return TargetCheckpoint{}, err
	}
	relayRoot, err := ResolveTargetPath(spec.RelayRoot)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	stagePath, err := ResolveTargetPath(spec.StagePath)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	outputPath, err := ResolveTargetPath(spec.OutputPath)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	if err := validateStagePath(relayRoot, stagePath, "checkpoint stage"); err != nil {
		return TargetCheckpoint{}, err
	}
	if err := validateStagePath(relayRoot, outputPath, "checkpoint archive"); err != nil {
		return TargetCheckpoint{}, err
	}
	if !checkpointIsDir(stagePath) {
		return TargetCheckpoint{}, errors.New("checkpoint stage is missing")
	}

	result, resultErr := packStagedCheckpoint(spec, stagePath, outputPath, started)
	if cleanupErr := os.RemoveAll(stagePath); cleanupErr != nil && !errors.Is(cleanupErr, fs.ErrNotExist) {
		if resultErr == nil {
			return TargetCheckpoint{}, fmt.Errorf("remove consumed checkpoint stage %s: %w", stagePath, cleanupErr)
		}
		return TargetCheckpoint{}, fmt.Errorf("also failed to remove checkpoint stage %s: %v: %w", stagePath, cleanupErr, resultErr)
	}
	return result, resultErr
}

// readStagedOperationPayloads reads a Git operation's two staged payloads,
// which are present together or not at all; what names the owner in the
// error.
func readStagedOperationPayloads(stagePath string, worktreePath, statePath *string, what string) (*GitOperationPayloads, error) {
	switch {
	case worktreePath == nil && statePath == nil:
		return nil, nil
	case worktreePath != nil && statePath != nil:
		conflicted, err := readStagedFile(stagePath, *worktreePath, math.MaxUint64)
		if err != nil {
			return nil, err
		}
		state, err := readStagedFile(stagePath, *statePath, math.MaxUint64)
		if err != nil {
			return nil, err
		}
		return &GitOperationPayloads{ConflictedWorktreeTar: conflicted, StateTar: state}, nil
	}
	return nil, fmt.Errorf("%s has only one of its Git operation payloads", what)
}

// packStagedCheckpoint is the part of PackCheckpoint after which the stage
// is removed whatever the outcome.
func packStagedCheckpoint(spec *CheckpointPackSpec, stagePath, outputPath string, started time.Time) (TargetCheckpoint, error) {
	manifestBody, err := readStagedFile(stagePath, captureCheckpointStageManifest, 8*1024*1024)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	var manifest checkpointStageManifest
	if err := json.Unmarshal(manifestBody, &manifest); err != nil {
		return TargetCheckpoint{}, fmt.Errorf("parse checkpoint stage manifest: %w", err)
	}
	if manifest.ProtocolVersion != CheckpointStagingProtocolVersion {
		return TargetCheckpoint{}, fmt.Errorf("unsupported sealed checkpoint stage version %d", manifest.ProtocolVersion)
	}
	nativeStarted := time.Now()
	nativeArtifacts := make([]core.NativeArtifact, 0, len(manifest.NativeArtifacts))
	for _, artifact := range manifest.NativeArtifacts {
		if err := validateRelativePath(artifact.BodyPath); err != nil {
			return TargetCheckpoint{}, err
		}
		path := core.Join(stagePath, artifact.BodyPath)
		if err := ensureNoSymlinkAncestors(stagePath, artifact.BodyPath); err != nil {
			return TargetCheckpoint{}, err
		}
		info, err := os.Lstat(path)
		if err != nil {
			return TargetCheckpoint{}, err
		}
		if !info.Mode().IsRegular() {
			return TargetCheckpoint{}, errors.New("staged native artifact is not a file")
		}
		if uint64(info.Size()) != artifact.Size {
			return TargetCheckpoint{}, errors.New("staged native artifact size changed")
		}
		nativeArtifacts = append(nativeArtifacts, core.NativeArtifact{
			RelativePath: artifact.RelativePath,
			Data:         core.NativeArtifactDataFile(path),
			Mode:         artifact.Mode,
		})
	}
	nativeMs := uint64(time.Since(nativeStarted).Milliseconds())
	repositoriesStarted := time.Now()
	repositories := make([]RepositorySnapshot, 0, len(manifest.Repositories))
	for _, repository := range manifest.Repositories {
		snapshot := RepositorySnapshot{Metadata: repository.Metadata}
		for _, file := range []struct {
			path string
			into *[]byte
		}{
			{repository.CommittedBundlePath, &snapshot.CommittedBundle},
			{repository.StagedPatchPath, &snapshot.StagedPatch},
			{repository.UnstagedPatchPath, &snapshot.UnstagedPatch},
			{repository.UntrackedTarPath, &snapshot.UntrackedTar},
		} {
			if *file.into, err = readStagedFile(stagePath, file.path, math.MaxUint64); err != nil {
				return TargetCheckpoint{}, err
			}
		}
		if snapshot.OperationPayloads, err = readStagedOperationPayloads(stagePath,
			repository.ConflictedWorktreeTarPath, repository.OperationStateTarPath, "staged repository"); err != nil {
			return TargetCheckpoint{}, err
		}
		snapshot.LinkedWorktrees = make([]LinkedWorktreeSnapshot, 0, len(repository.LinkedWorktrees))
		for _, worktree := range repository.LinkedWorktrees {
			linked := LinkedWorktreeSnapshot{Metadata: worktree.Metadata}
			for _, file := range []struct {
				path string
				into *[]byte
			}{
				{worktree.StagedPatchPath, &linked.StagedPatch},
				{worktree.UnstagedPatchPath, &linked.UnstagedPatch},
				{worktree.UntrackedTarPath, &linked.UntrackedTar},
			} {
				if *file.into, err = readStagedFile(stagePath, file.path, math.MaxUint64); err != nil {
					return TargetCheckpoint{}, err
				}
			}
			if linked.OperationPayloads, err = readStagedOperationPayloads(stagePath,
				worktree.ConflictedWorktreeTarPath, worktree.OperationStateTarPath, "staged linked worktree"); err != nil {
				return TargetCheckpoint{}, err
			}
			snapshot.LinkedWorktrees = append(snapshot.LinkedWorktrees, linked)
		}
		repositories = append(repositories, snapshot)
	}
	repositoriesMs := uint64(time.Since(repositoriesStarted).Milliseconds())
	eventFrontier := spec.CanonicalSession.EventFrontier
	eventFrontierDigest := spec.CanonicalSession.EventFrontierDigest
	archiveStarted := time.Now()
	sha, err := WriteArchiveHashed(outputPath, &ArchiveInput{
		Session:          manifest.Session,
		Target:           manifest.Target,
		Bundle:           manifest.Bundle,
		CanonicalSession: spec.CanonicalSession,
		NativeArtifacts:  nativeArtifacts,
		Repositories:     repositories,
	})
	if err != nil {
		return TargetCheckpoint{}, err
	}
	archiveMs := uint64(time.Since(archiveStarted).Milliseconds())
	return TargetCheckpoint{
		Path:                outputPath,
		Sha256:              sha,
		EventFrontier:       eventFrontier,
		EventFrontierDigest: eventFrontierDigest,
		Timings: &CheckpointExportTimings{
			NativeMs:       nativeMs,
			RepositoriesMs: repositoriesMs,
			ArchiveMs:      archiveMs,
			TotalMs:        uint64(time.Since(started).Milliseconds()),
		},
	}, nil
}

func validateStagePath(relayRoot string, path string, name string) error {
	parent, ok := checkpointParent(path)
	if !ok {
		parent = "."
	}
	if !checkpointPathStartsWith(parent, relayRoot) {
		return fmt.Errorf("%s must be beneath relay root", name)
	}
	return nil
}

func readStagedFile(root string, relative string, maximum uint64) ([]byte, error) {
	if err := validateRelativePath(relative); err != nil {
		return nil, err
	}
	if err := ensureNoSymlinkAncestors(root, relative); err != nil {
		return nil, err
	}
	path := core.Join(root, relative)
	info, err := os.Lstat(path)
	if err != nil {
		return nil, fmt.Errorf("inspect staged checkpoint file %s: %w", path, err)
	}
	if !info.Mode().IsRegular() {
		return nil, errors.New("staged checkpoint path is not a file")
	}
	if uint64(info.Size()) > maximum {
		return nil, errors.New("staged checkpoint file is too large")
	}
	file, err := os.OpenFile(path, os.O_RDONLY|checkpointNoFollow, 0)
	if err != nil {
		return nil, fmt.Errorf("open staged checkpoint file %s: %w", path, err)
	}
	defer file.Close()
	body := make([]byte, 0, info.Size())
	buffer := bytesWriter{body: body}
	if _, err := io.Copy(&buffer, file); err != nil {
		return nil, fmt.Errorf("read staged checkpoint file %s: %w", path, err)
	}
	if int64(len(buffer.body)) != info.Size() {
		return nil, errors.New("staged checkpoint file size changed")
	}
	return buffer.body, nil
}

// bytesWriter appends to a slice whose capacity the caller sized, as Rust's
// read_to_end into Vec::with_capacity does.
type bytesWriter struct{ body []byte }

func (w *bytesWriter) Write(data []byte) (int, error) {
	w.body = append(w.body, data...)
	return len(data), nil
}

func ExportCheckpoint(spec *CheckpointExportSpec) (TargetCheckpoint, error) {
	return ExportCheckpointWithGit(spec, &SystemGit{})
}

func ExportCheckpointWithGit(spec *CheckpointExportSpec, git GitCommandRunner) (TargetCheckpoint, error) {
	return ExportCheckpointWithNativeState(spec, git, noNativeCheckpointState{})
}

func ExportCheckpointWithNativeState(spec *CheckpointExportSpec, git GitCommandRunner, nativeState NativeCheckpointState) (TargetCheckpoint, error) {
	started := time.Now()
	if spec.ProtocolVersion != CheckpointExportProtocolVersion {
		return TargetCheckpoint{}, fmt.Errorf("unsupported checkpoint export protocol version %d; worker supports %d",
			spec.ProtocolVersion, CheckpointExportProtocolVersion)
	}
	relayRoot, err := ResolveTargetPath(spec.RelayRoot)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	requestedHome, err := ResolveTargetPath(spec.HarnessHome)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	harnessHome, err := CheckpointHarnessHome(relayRoot, requestedHome)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	workspaceRoot, err := ResolveTargetPath(spec.WorkspaceRoot)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	outputPath, err := ResolveTargetPath(spec.OutputPath)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	if err := spec.CanonicalSession.Validate(); err != nil {
		return TargetCheckpoint{}, err
	}
	if err := validateCheckpointSource(&spec.Session, relayRoot, harnessHome, workspaceRoot, spec.Repositories); err != nil {
		return TargetCheckpoint{}, err
	}
	if err := validateStagePath(relayRoot, outputPath, "checkpoint archive"); err != nil {
		return TargetCheckpoint{}, err
	}
	eventFrontier := spec.CanonicalSession.EventFrontier
	eventFrontierDigest := spec.CanonicalSession.EventFrontierDigest
	// A native session that never received a prompt legitimately has no
	// harness artifacts yet; requiring them would make an unused session, or
	// one just replaced by `/clear`, impossible to close cleanly (R4-5).
	prompted := CurrentNativeSessionReceivedPrompt(&spec.CanonicalSession)
	nativeStarted := time.Now()
	nativeArtifacts, err := collectCheckpointNativeArtifacts(
		&spec.Session,
		relayRoot,
		harnessHome,
		workspaceRoot,
		spec.ReportDirectories,
		!prompted,
		nativeState,
	)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	// The barrier stops dispatch, but a harness can still finish an async
	// native write. Own the captured bytes before repository collection and
	// ZIP planning, just as the staged capture path does.
	parent, ok := checkpointParent(outputPath)
	if !ok {
		return TargetCheckpoint{}, errors.New("checkpoint archive has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return TargetCheckpoint{}, err
	}
	nativeSnapshot, err := os.MkdirTemp(parent, CheckpointCaptureTemporaryPrefix+"*")
	if err != nil {
		return TargetCheckpoint{}, err
	}
	defer os.RemoveAll(nativeSnapshot)
	for index := range nativeArtifacts {
		artifact := &nativeArtifacts[index]
		if _, ok := artifact.Data.(core.NativeArtifactDataFile); !ok {
			continue
		}
		relative := strconv.Itoa(index)
		reader, err := artifact.Reader()
		if err != nil {
			return TargetCheckpoint{}, err
		}
		_, err = writePrivateReader(nativeSnapshot, relative, reader, 0o600)
		reader.Close()
		if err != nil {
			return TargetCheckpoint{}, err
		}
		artifact.Data = core.NativeArtifactDataFile(core.Join(nativeSnapshot, relative))
	}
	nativeMs := uint64(time.Since(nativeStarted).Milliseconds())
	repositoriesStarted := time.Now()
	untrackedFiles := UntrackedFilesInclude
	if spec.IgnoreNestedRepositories {
		untrackedFiles = UntrackedFilesIgnoreNestedRepositories
	}
	repositories, err := collectCheckpointRepositoriesWithPolicy(workspaceRoot, spec.Repositories, git, untrackedFiles)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	repositoriesMs := uint64(time.Since(repositoriesStarted).Milliseconds())
	// The export runs while the relay's barrier freezes ACP dispatch, so it
	// hashes the archive it just wrote instead of structurally re-reading it.
	// `CheckpointTransfer::execute` checks the controller's copy against this
	// digest; resume/import performs full structural verification when read.
	archiveStarted := time.Now()
	sha, err := WriteArchiveHashedBorrowed(
		outputPath,
		&spec.Session,
		&spec.Target,
		&spec.Bundle,
		&spec.CanonicalSession,
		nativeArtifacts,
		repositories,
	)
	if err != nil {
		return TargetCheckpoint{}, err
	}
	archiveMs := uint64(time.Since(archiveStarted).Milliseconds())
	return TargetCheckpoint{
		Path:                outputPath,
		Sha256:              sha,
		EventFrontier:       eventFrontier,
		EventFrontierDigest: eventFrontierDigest,
		Timings: &CheckpointExportTimings{
			NativeMs:       nativeMs,
			RepositoriesMs: repositoriesMs,
			ArchiveMs:      archiveMs,
			TotalMs:        uint64(time.Since(started).Milliseconds()),
		},
	}, nil
}

func collectCheckpointRepositories(workspaceRoot string, specifications []CheckpointRepositorySpec, git GitCommandRunner) ([]RepositorySnapshot, error) {
	return collectCheckpointRepositoriesWithPolicy(workspaceRoot, specifications, git, UntrackedFilesInclude)
}

// collectCheckpointRepositoriesWithPolicy collects the repositories in
// parallel; the result keeps the spec order, which in turn keeps the
// manifest and ZIP entry order deterministic.
func collectCheckpointRepositoriesWithPolicy(workspaceRoot string, specifications []CheckpointRepositorySpec, git GitCommandRunner, untrackedFiles UntrackedFiles) ([]RepositorySnapshot, error) {
	snapshots := make([]RepositorySnapshot, len(specifications))
	failures := make([]error, len(specifications))
	var group sync.WaitGroup
	for index := range specifications {
		group.Go(func() {
			snapshots[index], failures[index] = collectCheckpointRepository(workspaceRoot, &specifications[index], git, untrackedFiles)
		})
	}
	group.Wait()
	for _, err := range failures {
		if err != nil {
			return nil, err
		}
	}
	return snapshots, nil
}

// collectCheckpointRepository is one repository of
// collectCheckpointRepositoriesWithPolicy.
func collectCheckpointRepository(workspaceRoot string, repository *CheckpointRepositorySpec, git GitCommandRunner, untrackedFiles UntrackedFiles) (RepositorySnapshot, error) {
	inRepository := func(err error) error {
		return fmt.Errorf("repository '%s': %w", repository.ID, err)
	}
	path := core.Join(workspaceRoot, repository.RelativeDestination)
	if !checkpointIsDir(path) {
		return RepositorySnapshot{}, fmt.Errorf("repository %s is missing", path)
	}
	var history GitHistoryMode
	var remoteBase *string
	switch repository.Capture.Mode {
	case CheckpointRepositoryCapturePlainDirectory:
		snapshot, err := CollectPlainDirectorySnapshot(&GitCollectionSpec{
			ID:                  repository.ID,
			RelativeDestination: repository.RelativeDestination,
			History:             GitHistoryModeNoBundle{},
		})
		if err != nil {
			return RepositorySnapshot{}, inRepository(err)
		}
		return snapshot, nil
	case CheckpointRepositoryCaptureMetadataOnly:
		snapshot, err := CollectGitMetadataSnapshot(git, path, &GitCollectionSpec{
			ID:                  repository.ID,
			RelativeDestination: repository.RelativeDestination,
			History:             GitHistoryModeNoBundle{},
			OriginOverride:      repository.OriginOverride,
		})
		if err != nil {
			return RepositorySnapshot{}, inRepository(err)
		}
		return snapshot, nil
	case CheckpointRepositoryCaptureSessionDelta:
		if err := RepairOriginRefs(git, path, repository.ID); err != nil {
			return RepositorySnapshot{}, err
		}
		history = GitHistoryModeSessionDelta{}
	case CheckpointRepositoryCaptureManagedClone:
		history = GitHistoryModeCloneFrom(repository.Capture.BaseCommit)
	case CheckpointRepositoryCaptureDeltaFrom:
		history = GitHistoryModeDeltaFrom(repository.Capture.BaseCommit)
	case CheckpointRepositoryCaptureRemoteWorkspace:
		base, err := RemoteWorkspaceBase(git, path)
		if err != nil {
			return RepositorySnapshot{}, inRepository(err)
		}
		if base == nil {
			return RepositorySnapshot{}, fmt.Errorf("repository '%s' is not marked as a managed remote workspace", repository.ID)
		}
		history = GitHistoryModeCloneFrom(*base)
		remoteBase = base
	default:
		return RepositorySnapshot{}, fmt.Errorf("unknown checkpoint repository capture mode %q", string(repository.Capture.Mode))
	}
	if err := rejectDirtySubmodules(git, path); err != nil {
		return RepositorySnapshot{}, inRepository(err)
	}
	worktrees, err := linkedWorktreeDirectories(git, path)
	if err != nil {
		return RepositorySnapshot{}, inRepository(err)
	}
	for _, worktree := range worktrees {
		if err := rejectDirtySubmodules(git, worktree); err != nil {
			return RepositorySnapshot{}, fmt.Errorf("repository '%s' linked worktree %s: %w", repository.ID, worktree, err)
		}
	}
	snapshot, err := CollectGitSnapshotWithProgress(
		git,
		path,
		&GitCollectionSpec{
			ID:                  repository.ID,
			RelativeDestination: repository.RelativeDestination,
			History:             history,
			OriginOverride:      repository.OriginOverride,
		},
		untrackedFiles,
		func(GitSnapshotProgress) error { return nil },
	)
	if err != nil {
		return RepositorySnapshot{}, inRepository(err)
	}
	if remoteBase != nil {
		// select_git_history records the merge base. Preserve the immutable
		// launch value exactly so a split/fork checkpoint can restore the
		// workspace marker after origin moved.
		if snapshot.Metadata.Origin == "" {
			return RepositorySnapshot{}, fmt.Errorf("repository '%s' managed workspace has no Git origin", repository.ID)
		}
		if err := core.ValidateNetworkURL(snapshot.Metadata.Origin); err != nil {
			return RepositorySnapshot{}, err
		}
		for _, url := range snapshot.Metadata.PushURLs {
			if err := core.ValidateNetworkURL(url); err != nil {
				return RepositorySnapshot{}, err
			}
		}
		snapshot.Metadata.BaseCommit = *remoteBase
		snapshot.Metadata.RemoteWorkspace = true
	}
	return snapshot, nil
}

// CheckpointHarnessHome uses the installed worker's source paths while
// retaining the controller's requirement that the export belongs to the
// same physical harness home.
func CheckpointHarnessHome(relayRoot string, requested string) (string, error) {
	installed, ok, err := core.InstalledHarnessHome(core.Join(relayRoot, "launch.json"))
	if err != nil {
		return "", err
	}
	if !ok {
		// Earlier launch formats without a home use the checkpoint
		// protocol's explicit source, as do standalone exports without a
		// live worker.
		return requested, nil
	}
	if installed, err = resolveHomeRelativeTargetPath(installed); err != nil {
		return "", err
	}
	requestedReal, err := checkpointCanonicalize(requested)
	if err != nil {
		return "", fmt.Errorf("resolve checkpoint harness home: %w", err)
	}
	installedReal, err := checkpointCanonicalize(installed)
	if err != nil {
		return "", fmt.Errorf("resolve installed worker harness home: %w", err)
	}
	if requestedReal != installedReal {
		return "", errors.New("checkpoint harness home does not match the installed worker")
	}
	// An upgraded controller names <worker root>/profile, which can still be
	// a link to an older worker's original home. Its launch file owns the
	// paths used by both native history and the project-memory replica.
	return installed, nil
}

// collectCheckpointNativeArtifacts: Codex exports carry the relay-root scan
// cache so that a long-lived session probes each unrelated rollout at most
// once.
func collectCheckpointNativeArtifacts(session *core.SessionManifest, relayRoot string, harnessHome string, workspaceRoot string, reportDirectories *[]string, allowEmpty bool, nativeState NativeCheckpointState) ([]core.NativeArtifact, error) {
	artifacts, _, err := collectCheckpointNativeArtifactsWithReports(session, relayRoot, harnessHome, workspaceRoot, reportDirectories, allowEmpty, nativeState)
	return artifacts, err
}

func collectCheckpointNativeArtifactsWithReports(session *core.SessionManifest, relayRoot string, harnessHome string, workspaceRoot string, reportDirectories *[]string, allowEmpty bool, nativeState NativeCheckpointState) ([]core.NativeArtifact, CheckpointReportOmissions, error) {
	sessionID := session.NativeSessionID
	var artifacts []core.NativeArtifact
	var err error
	if session.HarnessKind != core.HarnessKindCodex {
		artifacts, err = CollectNativeArtifacts(session.HarnessKind, harnessHome, sessionID, allowEmpty)
		if err != nil {
			return nil, CheckpointReportOmissions{}, err
		}
	} else {
		cache := loadCodexScanCache(relayRoot, sessionID)
		known := len(cache.NotOurs)
		artifacts, err = collectNativeArtifactsCached(core.HarnessKindCodex, harnessHome, sessionID, allowEmpty, &cache)
		if err != nil {
			return nil, CheckpointReportOmissions{}, err
		}
		if len(cache.NotOurs) != known {
			if err := saveCodexScanCache(relayRoot, &cache); err != nil {
				return nil, CheckpointReportOmissions{}, err
			}
		}
	}
	state, err := nativeState.Collect(session, harnessHome)
	if err != nil {
		return nil, CheckpointReportOmissions{}, err
	}
	artifacts = append(artifacts, state...)
	attachments := core.AttachmentStoreWorker(relayRoot)
	attached, err := attachments.ArchiveArtifacts()
	if err != nil {
		return nil, CheckpointReportOmissions{}, err
	}
	artifacts = append(artifacts, attached...)
	launchPath := core.Join(relayRoot, "launch.json")
	launch, launchErr := readProjectMemoryCheckpointEndpoint(launchPath)
	// Children share the parent's workspace; only the parent's checkpoint
	// carries that report tree. The stable launch flag owns this
	// distinction.
	var omittedReports CheckpointReportOmissions
	if launchErr != nil || !launch.HandbackTool {
		var reports []core.NativeArtifact
		reports, omittedReports, err = collectSubagentReports(workspaceRoot, reportDirectories)
		if err != nil {
			return nil, CheckpointReportOmissions{}, err
		}
		artifacts = append(artifacts, reports...)
	}
	switch {
	case launchErr == nil:
		if launch.ProjectMemory != nil {
			root, err := resolveHomeRelativeTargetPath(launch.ProjectMemory.Root)
			if err != nil {
				return nil, CheckpointReportOmissions{}, err
			}
			if !checkpointPathStartsWith(root, harnessHome) {
				return nil, CheckpointReportOmissions{}, errors.New("project memory replica is outside the harness home")
			}
			if checkpointIsDir(root) {
				if err := collectClaudeMemoryTree(harnessHome, root, &artifacts); err != nil {
					return nil, CheckpointReportOmissions{}, err
				}
			}
		}
	case !checkpointExists(launchPath):
	default:
		return nil, CheckpointReportOmissions{}, fmt.Errorf("read project memory checkpoint endpoint: %w", launchErr)
	}
	checkpointSortArtifacts(artifacts)
	artifacts = slices.CompactFunc(artifacts, func(left, right core.NativeArtifact) bool {
		return checkpointPathEqual(left.RelativePath, right.RelativePath)
	})
	return artifacts, omittedReports, nil
}

// projectMemoryCheckpointLaunch: checkpoint exporters can be refreshed
// independently of the live worker, so read only the stable launch fields
// needed for collection. Fully parsing the current worker config would
// reject launch files written by older versions.
type projectMemoryCheckpointLaunch struct {
	HandbackTool  bool                             `json:"handback_tool" serde:"default"`
	ProjectMemory *projectMemoryCheckpointEndpoint `json:"project_memory" serde:"default"`
}

// UnmarshalJSON accepts unknown fields, as serde does for this struct.
func (l *projectMemoryCheckpointLaunch) UnmarshalJSON(data []byte) error {
	type plain projectMemoryCheckpointLaunch
	return checkpointStrictDecode(data, (*plain)(l), false)
}

type projectMemoryCheckpointEndpoint struct {
	Root string `json:"root"`
}

// UnmarshalJSON requires root and accepts unknown fields, as serde does for
// this struct.
func (e *projectMemoryCheckpointEndpoint) UnmarshalJSON(data []byte) error {
	type plain projectMemoryCheckpointEndpoint
	return checkpointStrictDecode(data, (*plain)(e), false)
}

func readProjectMemoryCheckpointEndpoint(path string) (projectMemoryCheckpointLaunch, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return projectMemoryCheckpointLaunch{}, fmt.Errorf("read worker launch config %s: %w", path, err)
	}
	var launch projectMemoryCheckpointLaunch
	if err := json.Unmarshal(body, &launch); err != nil {
		return projectMemoryCheckpointLaunch{}, fmt.Errorf("parse worker launch config %s: %w", path, err)
	}
	return launch, nil
}

func validateCaptureSpec(spec *CheckpointCaptureSpec) error {
	if err := validateCheckpointSource(&spec.Session, spec.RelayRoot, spec.HarnessHome, spec.WorkspaceRoot, spec.Repositories); err != nil {
		return err
	}
	return validateStagePath(spec.RelayRoot, spec.StagePath, "checkpoint stage")
}

func validateCheckpointSource(session *core.SessionManifest, relayRoot string, harnessHome string, workspaceRoot string, repositories []CheckpointRepositorySpec) error {
	if err := ValidateComponent(session.ID, "session ID"); err != nil {
		return err
	}
	if err := ValidateComponent(session.NativeSessionID, "native session ID"); err != nil {
		return err
	}
	if !checkpointIsDir(relayRoot) {
		return errors.New("relay root is missing")
	}
	if !checkpointIsDir(harnessHome) {
		return errors.New("harness home is missing")
	}
	if !checkpointIsDir(workspaceRoot) {
		return errors.New("workspace root is missing")
	}
	if len(repositories) == 0 {
		return errors.New("checkpoint has no repositories")
	}
	ids := map[string]bool{}
	destinations := map[string]bool{}
	for i := range repositories {
		repository := &repositories[i]
		if err := ValidateComponent(repository.ID, "repository ID"); err != nil {
			return err
		}
		if err := validateRelativePath(repository.RelativeDestination); err != nil {
			return err
		}
		if repository.Capture.Mode == CheckpointRepositoryCaptureDeltaFrom && strings.TrimSpace(repository.Capture.BaseCommit) == "" {
			return errors.New("base commit is empty")
		}
		if ids[repository.ID] {
			return errors.New("duplicate repository ID")
		}
		ids[repository.ID] = true
		destination := checkpointNormalize(repository.RelativeDestination)
		if destinations[destination] {
			return errors.New("duplicate destination")
		}
		destinations[destination] = true
	}
	return nil
}
