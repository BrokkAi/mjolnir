// Port of mj-checkpoint/src/checkpoint/restore.rs.

package checkpoint

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"strconv"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// QueueRestorePolicy is whether the destination worker owns dispatch
// immediately, drops the queue, or waits for its controller to admit the
// archived commands after readiness.
type QueueRestorePolicy string

const (
	QueueRestorePolicyRestore QueueRestorePolicy = "restore"
	QueueRestorePolicyDiscard QueueRestorePolicy = "discard"
	QueueRestorePolicyDefer   QueueRestorePolicy = "defer"
)

// UnmarshalJSON accepts only the snake_case variant names, as serde does.
func (p *QueueRestorePolicy) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch policy := QueueRestorePolicy(text); policy {
	case QueueRestorePolicyRestore, QueueRestorePolicyDiscard, QueueRestorePolicyDefer:
		*p = policy
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `restore`, `discard`, `defer`", text)
}

// RelaySeedForRestore is the relay metadata needed to continue a verified
// canonical session. An in-place native Move already has its history and
// attachments on the target.
func RelaySeedForRestore(canonical *core.CanonicalSessionSnapshot, nativeContinuity bool, queuePolicy QueueRestorePolicy) (core.RestoredRelaySeed, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs relay_seed_for_restore")
}

// deserializeQueuePolicy reads the queue policy or, from older controllers,
// the legacy discard_queued_prompts boolean (an untagged enum in Rust).
func deserializeQueuePolicy(data []byte) (QueueRestorePolicy, error) {
	var legacy bool
	if json.Unmarshal(data, &legacy) == nil {
		if legacy {
			return QueueRestorePolicyDiscard, nil
		}
		return QueueRestorePolicyRestore, nil
	}
	var policy QueueRestorePolicy
	if json.Unmarshal(data, &policy) == nil {
		return policy, nil
	}
	return "", errors.New("data did not match any variant of untagged enum Wire")
}

type CheckpointRestoreSpec struct {
	ArchivePath   string `json:"archive_path"`
	WorkspaceRoot string `json:"workspace_root"`
	// ReportRoot is the target-side root for report files; older controllers
	// restore beside repositories.
	ReportRoot          *string `json:"report_root,omitempty"`
	RelayRoot           string  `json:"relay_root"`
	HarnessHome         string  `json:"harness_home"`
	RestoreRepositories bool    `json:"restore_repositories"`
	RestoreNative       bool    `json:"restore_native"`
	// QueuePolicy is also read from the legacy alias
	// "discard_queued_prompts", as a policy or a boolean.
	QueuePolicy QueueRestorePolicy `json:"queue_policy"`
	// PrimaryRepositoryRoot is where the primary repository actually sits,
	// when that is not WorkspaceRoot joined with the archived destination. A
	// resume that moves a session between representations puts the checkout
	// somewhere the archive could not have named, and the restored harness
	// session has to point at the real working directory.
	PrimaryRepositoryRoot *string `json:"primary_repository_root,omitempty"`
}

// UnmarshalJSON refuses unknown fields, reads queue_policy through its alias
// and legacy boolean form, and requires it.
func (s *CheckpointRestoreSpec) UnmarshalJSON(data []byte) error {
	members, err := checkpointObject(data)
	if err != nil {
		return err
	}
	raw, ok := members["queue_policy"]
	if legacy, hasLegacy := members["discard_queued_prompts"]; hasLegacy {
		if ok {
			return errors.New("duplicate field `queue_policy`")
		}
		raw, ok = legacy, true
		delete(members, "discard_queued_prompts")
	}
	if ok {
		policy, err := deserializeQueuePolicy(raw)
		if err != nil {
			return err
		}
		members["queue_policy"] = json.RawMessage(strconv.Quote(string(policy)))
		if data, err = json.Marshal(members); err != nil {
			return err
		}
	}
	type plain CheckpointRestoreSpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

func RestoreCheckpoint(spec *CheckpointRestoreSpec, git GitCommandRunner) error {
	return RestoreCheckpointWithNativeState(spec, git, noNativeCheckpointState{})
}

func RestoreCheckpointWithNativeState(spec *CheckpointRestoreSpec, git GitCommandRunner, nativeState NativeCheckpointState) error {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_checkpoint_with_native_state")
}

// CheckoutRecoverySpec is retryable reconstruction in a private staging
// checkout. Publishing must atomically refuse any directory that appeared at
// the current checkout path.
type CheckoutRecoverySpec struct {
	Version             uint32                        `json:"version"`
	CheckpointSha256    string                        `json:"checkpoint_sha256"`
	Restore             CheckpointRestoreSpec         `json:"restore"`
	StagingRoot         string                        `json:"staging_root"`
	StagedCheckout      string                        `json:"staged_checkout"`
	DestinationCheckout string                        `json:"destination_checkout"`
	AdditionalCheckouts []CheckoutRecoveryPublication `json:"additional_checkouts" serde:"default"`
}

// MarshalJSON writes an empty list as [], as serde does.
func (s CheckoutRecoverySpec) MarshalJSON() ([]byte, error) {
	type plain CheckoutRecoverySpec
	v := plain(s)
	if v.AdditionalCheckouts == nil {
		v.AdditionalCheckouts = []CheckoutRecoveryPublication{}
	}
	return checkpointMarshal(v)
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (s *CheckoutRecoverySpec) UnmarshalJSON(data []byte) error {
	type plain CheckoutRecoverySpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

type CheckoutRecoveryPublication struct {
	StagedCheckout      string `json:"staged_checkout"`
	DestinationCheckout string `json:"destination_checkout"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (p *CheckoutRecoveryPublication) UnmarshalJSON(data []byte) error {
	type plain CheckoutRecoveryPublication
	return checkpointStrictDecode(data, (*plain)(p), true)
}

// ParentRecoveryOutcome: target-side partial recovery never writes checkout
// files, refs or child reports.
type ParentRecoveryOutcome struct {
	NativeRestored bool `json:"native_restored"`
	RelayRestored  bool `json:"relay_restored"`
}

// UnmarshalJSON requires both fields and accepts unknown ones, as serde does
// for this struct.
func (o *ParentRecoveryOutcome) UnmarshalJSON(data []byte) error {
	type plain ParentRecoveryOutcome
	return checkpointStrictDecode(data, (*plain)(o), false)
}

func RestoreMissingParentCheckpoint(spec *CheckpointRestoreSpec, nativeState NativeCheckpointState, currentSeed *core.RestoredRelaySeed) (ParentRecoveryOutcome, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_missing_parent_checkpoint")
}

func parentRelayMissing(root string) (bool, error) {
	state, err := checkpointTryExists(core.Join(root, core.RelayStateFile))
	if err != nil || state {
		return false, err
	}
	journal, err := checkpointTryExists(core.Join(root, core.RelayJournalDir))
	if err != nil {
		return false, err
	}
	return !journal, nil
}

func restoreMissingParentRelay(root string, seed *core.RestoredRelaySeed) (bool, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_missing_parent_relay")
}

// restoreMissingSqliteFamilies collapses a legacy database/WAL family before
// publishing any authoritative file. A retry can then treat a present
// database as complete, with no receipt or partially published sidecars to
// reconcile. It returns whether anything changed and the family paths it
// owns (a Rust BTreeSet<PathBuf>).
func restoreMissingSqliteFamilies(archive *VerifiedArchive, payloadReader *CheckpointPayloadReader, spec *CheckpointRestoreSpec, targetCwd *string) (bool, map[string]struct{}, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_missing_sqlite_families")
}

func restoreNativeArtifacts(archive *VerifiedArchive, payloadReader *CheckpointPayloadReader, spec *CheckpointRestoreSpec, nativeState NativeCheckpointState, missingOnly bool) (bool, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_native_artifacts")
}

// ReadCheckpointSession reads the controller-owned projection without
// restoring target artifacts.
func ReadCheckpointSession(path string) (core.CanonicalSessionSnapshot, error) {
	metadata, err := VerifyArchiveStreaming(path)
	if err != nil {
		return core.CanonicalSessionSnapshot{}, err
	}
	return metadata.CanonicalSession, nil
}

func RestoreRepositories(archivePath string, workspaceRoot string, git GitCommandRunner) error {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_repositories")
}

func restoreRepositoriesFromArchive(archive *VerifiedArchive, workspaceRoot string, git GitCommandRunner, restoreContents bool) error {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_repositories_from_archive")
}

// checkoutTopLevel is the top level of the checkout a repository's session
// directory belongs to.
//
// A session working in a subdirectory of its checkout is described by that
// subdirectory, while its snapshot covers the whole checkout, so the restore
// climbs back out of the subdirectory the snapshot recorded.
func checkoutTopLevel(directory string, metadata *core.RepositoryMetadata) (string, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs checkout_top_level")
}

func archivedRepositorySnapshot(archive *VerifiedArchive, repository *core.RepositoryManifest) (RepositorySnapshot, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs archived_repository_snapshot")
}

// RestoreSingleRepositoryOntoBranch restores a checkpoint's only repository
// into an existing checkout, on a branch the caller names.
//
// A resume that moves a session out of its workspace restores it into a
// worktree of the user's own repository, where the archived branch is
// usually already checked out somewhere else and `git checkout -B` would
// refuse it. Returns the branch the checkpoint recorded (nil when detached).
func RestoreSingleRepositoryOntoBranch(archivePath string, repositoryPath string, branch string, git GitCommandRunner) (*string, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_single_repository_onto_branch")
}

// RestoreSingleRepositoryIntoCheckout restores a retired independent clone
// on the branch saved in its archive.
func RestoreSingleRepositoryIntoCheckout(archivePath string, repositoryPath string, git GitCommandRunner) error {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_single_repository_into_checkout")
}

// restoredNativeRelativePath: native session files use harness-specific
// working-directory keys. Rewrite the archived key to the one the harness
// derives from targetCwd, the directory the worker launches it in.
func restoredNativeRelativePath(harness core.HarnessKind, relativePath string, targetCwd *string) (string, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restored_native_relative_path")
}

func targetPrimaryCwd(primaryRepository string, repositories []core.RepositoryManifest, workspaceRoot string) *string {
	for i := range repositories {
		if repositories[i].Metadata.ID == primaryRepository {
			cwd := core.Join(workspaceRoot, repositories[i].Metadata.RelativeDestination)
			return &cwd
		}
	}
	return nil
}

type nativeRelocation int

const (
	nativeRelocationMuse nativeRelocation = iota
	nativeRelocationDsh
	nativeRelocationAntigravity
	nativeRelocationGrokSummary
	nativeRelocationKimiRegistry
	nativeRelocationKimiIndex
	nativeRelocationKimiState
)

// nativeArtifactRelocation is the single owner of which native formats
// actually rewrite their contents; false when the format does not.
func nativeArtifactRelocation(harness core.HarnessKind, path string) (nativeRelocation, bool) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs native_artifact_relocation")
}

func restoredNativeArtifactBytes(harness core.HarnessKind, relativePath string, data []byte, targetCwd *string, harnessHome string) ([]byte, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restored_native_artifact_bytes")
}

// rewriteDshSessionCwd: a dsh-acp session record names the directory it was
// created in, and `session/load` refuses any other, so a restore at a new
// workspace path records the target's. Additional directories need no
// rewrite: every load replaces them with the ones it is given.
func rewriteDshSessionCwd(data []byte, targetCwd string) ([]byte, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs rewrite_dsh_session_cwd")
}

// restoreSubagentReports puts a checkpoint's sub-agent report files back
// beside the repositories, whether or not the harness's own history is
// restored: the parent reads them with its own tools. A file already there
// is newer than the archive's copy and is kept.
func restoreSubagentReports(archive *VerifiedArchive, workspaceRoot string, reportRoot string) error {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs restore_subagent_reports")
}

// removePrivateFile removes relative from root if it is there, under the
// same rules as writePrivateFile: never through a symlinked directory.
func removePrivateFile(root string, relative string) error {
	if err := validateRelativePath(relative); err != nil {
		return err
	}
	if err := ensureNoSymlinkAncestors(root, relative); err != nil {
		return err
	}
	path := core.Join(root, relative)
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove %s: %w", path, err)
	}
	return nil
}

// writePrivateFile writes relative under root with owner-only permissions,
// refusing to follow a symlink anywhere along the way.
//
// A restore lands in a directory the target user can write, so a planted
// symlink must never redirect the write outside root. The destination is
// inspected without following links and, on Unix, opened with O_NOFOLLOW so
// the check cannot be raced.
func writePrivateFile(root string, relative string, data []byte, mode uint32) error {
	_, err := writePrivateReader(root, relative, bytes.NewReader(data), mode)
	return err
}

func writePrivateReader(root string, relative string, reader io.Reader, mode uint32) (uint64, error) {
	return writePrivateWith(root, relative, mode, func(file *os.File) (uint64, error) {
		written, err := io.Copy(file, reader)
		return uint64(written), err
	})
}

func writeMissingPrivateFile(root string, relative string, data []byte, mode uint32) (bool, error) {
	return writeMissingPrivateWith(root, relative, mode, func(file *os.File) error {
		if _, err := file.Write(data); err != nil {
			return fmt.Errorf("write staged recovery file: %w", err)
		}
		return nil
	})
}

// preparePrivateDestination checks the destination and creates its
// parents, returning the path to write. Both the replacing and the
// missing-only writers start here.
func preparePrivateDestination(root string, relative string) (string, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs prepare_private_destination")
}

func writePrivateWith[T any](root string, relative string, mode uint32, write func(*os.File) (T, error)) (T, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs write_private_with")
}

// writeMissingPrivateWith streams a file into a staged copy and publishes
// it only when the destination is still absent; returns whether it
// published.
func writeMissingPrivateWith(root string, relative string, mode uint32, write func(*os.File) error) (bool, error) {
	path, err := preparePrivateDestination(root, relative)
	if err != nil {
		return false, err
	}
	return publishMissingPrivateFile(path, mode, write)
}

func publishMissingPrivateFile(path string, mode uint32, write func(*os.File) error) (bool, error) {
	panic("unported: mj-checkpoint/src/checkpoint/restore.rs publish_missing_private_file")
}
