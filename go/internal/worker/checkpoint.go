// Port of mj-worker/src/checkpoint.rs.

// Target checkpoint command entrypoints.

package worker

import (
	"errors"
	"io"
	"os"

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
	panic("unported: mj-worker/src/checkpoint.rs restore_checkout_from_spec_file")
}

// RestoreMissingFromSpecFile restores the native state and relay of a parent
// checkpoint where they are missing, then prints the outcome as JSON. seedPath
// names the current relay seed, when there is one.
func RestoreMissingFromSpecFile(path string, seedPath *string) error {
	panic("unported: mj-worker/src/checkpoint.rs restore_missing_from_spec_file")
}

func RestoreFromSpecFile(path string) error {
	spec, err := readRestoreSpec(path)
	if err != nil {
		return err
	}
	return checkpoint.RestoreCheckpointWithNativeState(&spec, checkpoint.SystemGit{}, checkpointNativeState{})
}

func readRestoreSpec(path string) (checkpoint.CheckpointRestoreSpec, error) {
	panic("unported: mj-worker/src/checkpoint.rs read_restore_spec")
}
