// Port of mj-checkpoint/src/checkpoint/capture.rs.

package checkpoint

import (
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"os"

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
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs capture_checkpoint_with_native_state")
}

// refreshCheckpointStage reuses a sealed prestage whose native sources are
// unchanged; nil when the prestage is stale.
func refreshCheckpointStage(spec *CheckpointCaptureSpec, sourceFingerprintBefore string, git GitCommandRunner) (*CapturedCheckpoint, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs refresh_checkpoint_stage")
}

func checkpointRepositoryBytes(repositories []RepositorySnapshot) (uint64, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs checkpoint_repository_bytes")
}

func writeStagedRepositories(stageRoot string, repositories []RepositorySnapshot) ([]stagedRepository, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs write_staged_repositories")
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
		digest.Write([]byte(source.prefix))
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
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs checkpoint_report_sources")
}

// collectSubagentReports returns the newest regular report files that fit,
// plus metadata about the files left out.
func collectSubagentReports(workspaceRoot string, reportDirectories *[]string) ([]core.NativeArtifact, CheckpointReportOmissions, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs collect_subagent_reports")
}

func fingerprintTree(root string, path string, digest hash.Hash) error {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs fingerprint_tree")
}

// PackCheckpoint packages a previously sealed target generation with the
// controller's canonical projection. This operation runs after ACP dispatch
// resumes.
func PackCheckpoint(spec *CheckpointPackSpec) (TargetCheckpoint, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs pack_checkpoint")
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
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs read_staged_file")
}

func ExportCheckpoint(spec *CheckpointExportSpec) (TargetCheckpoint, error) {
	return ExportCheckpointWithGit(spec, &SystemGit{})
}

func ExportCheckpointWithGit(spec *CheckpointExportSpec, git GitCommandRunner) (TargetCheckpoint, error) {
	return ExportCheckpointWithNativeState(spec, git, noNativeCheckpointState{})
}

func ExportCheckpointWithNativeState(spec *CheckpointExportSpec, git GitCommandRunner, nativeState NativeCheckpointState) (TargetCheckpoint, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs export_checkpoint_with_native_state")
}

func collectCheckpointRepositories(workspaceRoot string, specifications []CheckpointRepositorySpec, git GitCommandRunner) ([]RepositorySnapshot, error) {
	return collectCheckpointRepositoriesWithPolicy(workspaceRoot, specifications, git, UntrackedFilesInclude)
}

// collectCheckpointRepositoriesWithPolicy collects the repositories in
// parallel; the result keeps the spec order, which in turn keeps the
// manifest and ZIP entry order deterministic.
func collectCheckpointRepositoriesWithPolicy(workspaceRoot string, specifications []CheckpointRepositorySpec, git GitCommandRunner, untrackedFiles UntrackedFiles) ([]RepositorySnapshot, error) {
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs collect_checkpoint_repositories_with_policy")
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
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs collect_checkpoint_native_artifacts_with_reports")
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
	panic("unported: mj-checkpoint/src/checkpoint/capture.rs validate_checkpoint_source")
}
