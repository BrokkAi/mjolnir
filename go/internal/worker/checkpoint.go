// Port of mj-worker/src/checkpoint.rs.

// Target checkpoint command entrypoints.

package worker

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"slices"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// checkpointNativeState is the worker's native checkpoint state: Codex goal
// accounting, which Codex stores outside its rollouts.
type checkpointNativeState struct{}

var _ checkpoint.NativeCheckpointState = checkpointNativeState{}

func (checkpointNativeState) Collect(session *core.SessionManifest, home string) ([]core.NativeArtifact, error) {
	if session.HarnessKind != core.HarnessKindCodex {
		return []core.NativeArtifact{}, nil
	}
	artifact, ok, err := codexGoalCollect(home, session.NativeSessionID)
	if err != nil {
		return nil, err
	}
	if !ok {
		return []core.NativeArtifact{}, nil
	}
	return []core.NativeArtifact{artifact}, nil
}

func (checkpointNativeState) RestoreMissing(session *core.SessionManifest, home string, path string, data []byte) (changed bool, handled bool, err error) {
	if !codexGoalIsArtifact(path) {
		return false, false, nil
	}
	if session.HarnessKind != core.HarnessKindCodex {
		return false, false, errors.New("goal artifact belongs to a non-Codex session")
	}
	changed, err = codexGoalRestoreMissing(home, session.NativeSessionID, path, data)
	if err != nil {
		return false, false, err
	}
	return changed, true, nil
}

func (checkpointNativeState) Restore(session *core.SessionManifest, home string, path string, data []byte) (bool, error) {
	if !codexGoalIsArtifact(path) {
		return false, nil
	}
	if session.HarnessKind != core.HarnessKindCodex {
		return false, errors.New("goal artifact belongs to a non-Codex session")
	}
	if err := codexGoalRestore(home, session.NativeSessionID, path, data); err != nil {
		return false, err
	}
	return true, nil
}

func exportCheckpoint(spec *checkpoint.CheckpointExportSpec) (checkpoint.TargetCheckpoint, error) {
	return checkpoint.ExportCheckpointWithNativeState(spec, checkpoint.SystemGit{}, checkpointNativeState{})
}

// ExportFromSpecFile is the hidden target CLI entry point:
// `mj worker export-checkpoint --spec PATH|-`.
func ExportFromSpecFile(path string) (checkpoint.TargetCheckpoint, error) {
	if path == checkpoint.ExportSpecStdin {
		return ExportFromSpecReader(os.Stdin)
	}
	spec, err := checkpoint.CheckpointExportSpecRead(path)
	if err != nil {
		return checkpoint.TargetCheckpoint{}, err
	}
	return exportCheckpoint(&spec)
}

func ExportFromSpecReader(reader io.Reader) (checkpoint.TargetCheckpoint, error) {
	spec, err := checkpoint.CheckpointExportSpecReadFrom(reader)
	if err != nil {
		return checkpoint.TargetCheckpoint{}, err
	}
	return exportCheckpoint(&spec)
}

// SuspendPreflightFromSpecFile is the hidden target CLI entry point:
// `mj worker suspend-preflight --spec PATH|-`.
func SuspendPreflightFromSpecFile(path string) (core.WorkerSuspendPreflightResult, error) {
	var spec core.WorkerSuspendPreflightSpec
	if path == checkpoint.ExportSpecStdin {
		read, err := checkpoint.ReadJSONFrom[core.WorkerSuspendPreflightSpec](os.Stdin, "suspend preflight spec")
		if err != nil {
			return core.WorkerSuspendPreflightResult{}, err
		}
		spec = read
	} else {
		body, err := os.ReadFile(path)
		if err != nil {
			return core.WorkerSuspendPreflightResult{}, fmt.Errorf("read suspend preflight spec %s: %w", path, err)
		}
		if err := json.Unmarshal(body, &spec); err != nil {
			return core.WorkerSuspendPreflightResult{}, fmt.Errorf("decode suspend preflight spec: %w", err)
		}
	}
	return RunSuspendPreflight(&spec, checkpoint.SystemGit{})
}

// RunSuspendPreflight inventories each repository of spec read-only: its
// untracked files, nested repositories, untracked stashes, operation in
// progress, blockers and `.mj/keep/` entries, and, for a dry run, the Git
// delta a suspend would transfer (or why it cannot be estimated).
func RunSuspendPreflight(spec *core.WorkerSuspendPreflightSpec, git checkpoint.GitCommandRunner) (core.WorkerSuspendPreflightResult, error) {
	var result core.WorkerSuspendPreflightResult
	var gitDeltaTotal *uint64
	if spec.DryRun {
		gitDeltaTotal = new(uint64)
	}
	var gitDeltaUnavailable *string

	for _, repo := range spec.Repositories {
		var input checkpoint.RepositoryInput
		var err error
		switch {
		case repo.Raw:
			input = checkpoint.RawSnapshotInput(repo.ID, repo.Path, nil, nil, "")
		case repo.ExcludeCommits != nil:
			input, err = checkpoint.CutoffSnapshotInput(repo.ID, repo.Path, &spec.SessionID, slices.Clone(*repo.ExcludeCommits), "")
		default:
			input, err = checkpoint.ManagedCloneSnapshotInput(repo.ID, repo.Path, &spec.SessionID, "", repo.OriginOverride)
		}
		if err != nil {
			return core.WorkerSuspendPreflightResult{}, err
		}
		input.KeepDir = repo.KeepDir

		inventories, err := checkpoint.SessionInventory(git, []checkpoint.RepositoryInput{input})
		if err != nil {
			return core.WorkerSuspendPreflightResult{}, err
		}
		inventory, ok := inventories[input.ID]
		if !ok {
			return core.WorkerSuspendPreflightResult{}, errors.New("session inventory returned no repository")
		}

		if input.KeepDir != nil {
			entries, size, err := checkpoint.ScanKeepDir(inventory.Checkout, *input.KeepDir)
			if err != nil {
				return core.WorkerSuspendPreflightResult{}, err
			}
			result.KeepDirEntries = append(result.KeepDirEntries, entries...)
			result.KeepDirBytes = checkpointSaturatingAdd(result.KeepDirBytes, size)
		}

		if spec.DryRun && gitDeltaUnavailable == nil {
			if inventory.Operation != nil {
				gitDeltaTotal = nil
				reason := fmt.Sprintf("Git size estimate unavailable during %s", inventory.Operation.Kind)
				gitDeltaUnavailable = &reason
			} else if estimate, err := checkpoint.EstimateRepositoryDelta(git, input); err != nil {
				gitDeltaTotal = nil
				// anyhow's `{err}`: the outermost message only.
				reason := "Git size estimate unavailable: " + acpErrorChain(err)[0]
				gitDeltaUnavailable = &reason
			} else if gitDeltaTotal != nil && estimate.TotalBytes != nil {
				total := checkpointSaturatingAdd(*gitDeltaTotal, *estimate.TotalBytes)
				gitDeltaTotal = &total
			} else {
				gitDeltaTotal = nil
			}
		}

		result.Untracked = append(result.Untracked, inventory.Untracked...)
		result.NestedRepositories = append(result.NestedRepositories, inventory.NestedRepositories...)
		for _, stash := range inventory.UntrackedStashes {
			result.UntrackedStashes = append(result.UntrackedStashes, core.SuspendUntrackedStash{
				Checkout: stash.Checkout,
				Commit:   stash.Value.Commit,
				Message:  stash.Value.Message,
			})
		}
		if inventory.Operation != nil {
			result.Operations = append(result.Operations, *inventory.Operation)
		}
		result.Raw = result.Raw || inventory.Raw
		for _, blocker := range inventory.Blockers {
			paths := make([]string, 0, len(blocker.Paths))
			for _, path := range blocker.Paths {
				paths = append(paths, path.Value)
			}
			result.Blockers = append(result.Blockers, core.SuspendBlocker{
				Kind:   checkpoint.SnapshotRefusedDebug(blocker.Kind),
				Paths:  paths,
				Remedy: blocker.Remedy,
			})
		}
	}

	if spec.DryRun {
		result.DryRun = &core.DryRunEstimate{
			GitDeltaBytes:             gitDeltaTotal,
			GitDeltaUnavailableReason: gitDeltaUnavailable,
			SessionStateBytes:         nil,
			KeepDirBytes:              result.KeepDirBytes,
			TransferRoute:             nil,
		}
	}
	return result, nil
}

// checkpointSaturatingAdd is u64::saturating_add.
func checkpointSaturatingAdd(a, b uint64) uint64 {
	if a > math.MaxUint64-b {
		return math.MaxUint64
	}
	return a + b
}

func CaptureFromSpecReader(reader io.Reader) (checkpoint.CapturedCheckpoint, error) {
	spec, err := checkpoint.ReadJSONFrom[checkpoint.CheckpointCaptureSpec](reader, "checkpoint capture spec")
	if err != nil {
		return checkpoint.CapturedCheckpoint{}, err
	}
	return checkpoint.CaptureCheckpointWithNativeState(&spec, checkpoint.SystemGit{}, checkpointNativeState{})
}

func PackFromSpecReader(reader io.Reader) (checkpoint.TargetCheckpoint, error) {
	spec, err := checkpoint.ReadJSONFrom[checkpoint.CheckpointPackSpec](reader, "checkpoint pack spec")
	if err != nil {
		return checkpoint.TargetCheckpoint{}, err
	}
	return checkpoint.PackCheckpoint(&spec)
}

// DiscardQueuedPromptsBeforeStart discards the queue of a stopped worker.
// The root lock excludes a live worker; the relay's preservation mode never
// promotes accepted work while this command journals its queue disposition.
func DiscardQueuedPromptsBeforeStart(root, sessionID, commandID string) error {
	owner, err := WorkerRootOwnerAcquire(root)
	if err != nil {
		return err
	}
	defer owner.Release()
	return durableRelayDiscardQueuedPromptsBeforeStart(root, sessionID, commandID)
}

// RestoreCheckoutFromSpecFile reconstructs checkouts in private staging and
// publishes each one only where no directory has appeared since, then prints
// `{"published": <bool>}`.
func RestoreCheckoutFromSpecFile(path string) error {
	body, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	var spec checkpoint.CheckoutRecoverySpec
	if err := json.Unmarshal(body, &spec); err != nil {
		return err
	}
	if spec.Version != 1 || !spec.Restore.RestoreRepositories || spec.Restore.RestoreNative {
		return errors.New("unsupported checkout recovery specification")
	}
	for _, field := range []*string{
		&spec.Restore.ArchivePath, &spec.Restore.WorkspaceRoot,
		&spec.StagedCheckout, &spec.DestinationCheckout, &spec.StagingRoot,
	} {
		if *field, err = checkpoint.ResolveTargetPath(*field); err != nil {
			return err
		}
	}
	publications := []checkpoint.CheckoutRecoveryPublication{{
		StagedCheckout:      spec.StagedCheckout,
		DestinationCheckout: spec.DestinationCheckout,
	}}
	for _, publication := range spec.AdditionalCheckouts {
		if publication.StagedCheckout, err = checkpoint.ResolveTargetPath(publication.StagedCheckout); err != nil {
			return err
		}
		if publication.DestinationCheckout, err = checkpoint.ResolveTargetPath(publication.DestinationCheckout); err != nil {
			return err
		}
		publications = append(publications, publication)
	}
	missing := false
	for _, publication := range publications {
		exists, err := pathTryExists(publication.DestinationCheckout)
		if err != nil {
			return err
		}
		if !exists {
			if info, err := os.Stat(publication.StagedCheckout); err != nil || !info.IsDir() {
				return errors.New("staged checkout is missing")
			}
			missing = true
		}
	}
	ready := filepath.Join(spec.StagingRoot, ".mj-checkouts-restored")
	readyExists, err := pathTryExists(ready)
	if err != nil {
		return err
	}
	if missing && !readyExists {
		if err := checkpoint.RestoreRepositories(spec.Restore.ArchivePath, spec.Restore.WorkspaceRoot, checkpoint.SystemGit{}); err != nil {
			return err
		}
		if err := core.AtomicWrite(ready, []byte(spec.CheckpointSha256)); err != nil {
			return err
		}
	}
	if readyExists, err = pathTryExists(ready); err != nil {
		return err
	}
	if readyExists {
		recorded, err := os.ReadFile(ready)
		if err != nil {
			return err
		}
		if !utf8.Valid(recorded) {
			return errors.New("stream did not contain valid UTF-8")
		}
		if string(recorded) != spec.CheckpointSha256 {
			return errors.New("staged checkout recovery identity changed")
		}
	}
	published := false
	for _, publication := range publications {
		exists, err := pathTryExists(publication.DestinationCheckout)
		if err != nil {
			return err
		}
		if !exists {
			if err := os.MkdirAll(filepath.Dir(publication.DestinationCheckout), 0o777); err != nil {
				return err
			}
			renamed, err := renameDirectoryWithoutReplacement(publication.StagedCheckout, publication.DestinationCheckout)
			if err != nil {
				return err
			}
			published = published || renamed
		}
		if info, err := os.Stat(filepath.Join(publication.DestinationCheckout, ".git")); err == nil && info.Mode().IsRegular() {
			output, err := checkpoint.SystemGit{}.Run(publication.DestinationCheckout, &checkpoint.GitCommand{
				Arguments: []string{"worktree", "repair", publication.DestinationCheckout},
			})
			if err != nil {
				return err
			}
			if output.Status != 0 {
				return fmt.Errorf("repair published worktree: %s", fromUTF8Lossy(output.Stderr))
			}
		}
	}
	fmt.Printf("{\"published\":%t}\n", published)
	return nil
}

// RestoreMissingFromSpecFile restores the native state and relay of a parent
// checkpoint where they are missing, then prints the outcome as JSON. seedPath
// names the current relay seed, when there is one.
func RestoreMissingFromSpecFile(path string, seedPath *string) error {
	spec, err := readRestoreSpec(path)
	if err != nil {
		return err
	}
	owner, err := WorkerRootOwnerAcquire(spec.RelayRoot)
	if err != nil {
		return err
	}
	defer owner.Release()
	var seed *core.RestoredRelaySeed
	if seedPath != nil {
		body, err := os.ReadFile(*seedPath)
		if err != nil {
			return err
		}
		var decoded core.RestoredRelaySeed
		if err := json.Unmarshal(body, &decoded); err != nil {
			return err
		}
		seed = &decoded
	}
	outcome, err := checkpoint.RestoreMissingParentCheckpoint(&spec, checkpointNativeState{}, seed)
	if err != nil {
		return err
	}
	encoded, err := json.Marshal(outcome)
	if err != nil {
		return err
	}
	fmt.Println(string(encoded))
	return nil
}

func RestoreFromSpecFile(path string) error {
	spec, err := readRestoreSpec(path)
	if err != nil {
		return err
	}
	return checkpoint.RestoreCheckpointWithNativeState(&spec, checkpoint.SystemGit{}, checkpointNativeState{})
}

func readRestoreSpec(path string) (checkpoint.CheckpointRestoreSpec, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return checkpoint.CheckpointRestoreSpec{}, fmt.Errorf("read checkpoint restore spec %s: %w", path, err)
	}
	var spec checkpoint.CheckpointRestoreSpec
	if err := json.Unmarshal(body, &spec); err != nil {
		return checkpoint.CheckpointRestoreSpec{}, fmt.Errorf("parse checkpoint restore spec %s: %w", path, err)
	}
	for _, field := range []*string{&spec.ArchivePath, &spec.WorkspaceRoot, &spec.RelayRoot, &spec.HarnessHome} {
		if *field, err = checkpoint.ResolveTargetPath(*field); err != nil {
			return checkpoint.CheckpointRestoreSpec{}, err
		}
	}
	return spec, nil
}
