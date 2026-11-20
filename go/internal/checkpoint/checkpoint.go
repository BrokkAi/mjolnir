// Port of mj-checkpoint/src/checkpoint.rs.
//
// Target-side checkpoint collection and controller-side verified transfer.
//
// Targets own the Git worktrees and native harness history, so they build the
// archive. The controller downloads into a same-directory temporary file and
// only returns a teardown gate after reopening and verifying the installed
// archive.

package checkpoint

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NativeCheckpointState is native state whose storage format is owned by the
// target runtime. Shared archive algorithms never open a provider's database
// themselves. Implementations must be safe for concurrent use (Rust: Sync).
//
// Embed NativeCheckpointStateDefaults for the trait's default RestoreMissing.
type NativeCheckpointState interface {
	Collect(session *core.SessionManifest, home string) ([]core.NativeArtifact, error)
	Restore(session *core.SessionManifest, home string, path string, data []byte) (bool, error)
	// RestoreMissing handles target-owned artifacts without replacing
	// existing state. handled false means this is an ordinary file;
	// otherwise changed reports whether data changed (Rust Option<bool>).
	RestoreMissing(session *core.SessionManifest, home string, path string, data []byte) (changed bool, handled bool, err error)
}

// NativeCheckpointStateDefaults supplies the Rust trait's default methods to
// an implementation that embeds it.
type NativeCheckpointStateDefaults struct{}

// RestoreMissing is the default: every artifact is an ordinary file.
func (NativeCheckpointStateDefaults) RestoreMissing(_ *core.SessionManifest, _ string, _ string, _ []byte) (bool, bool, error) {
	return false, false, nil
}

type noNativeCheckpointState struct {
	NativeCheckpointStateDefaults
}

func (noNativeCheckpointState) Collect(_ *core.SessionManifest, _ string) ([]core.NativeArtifact, error) {
	return []core.NativeArtifact{}, nil
}

func (noNativeCheckpointState) Restore(_ *core.SessionManifest, _ string, _ string, _ []byte) (bool, error) {
	return false, nil
}

// CheckpointExportProtocolVersion is the version of the
// controller-to-exporter checkpoint specification contract.
const CheckpointExportProtocolVersion uint32 = 2

// CheckpointStagingProtocolVersion is the version of the two-phase
// capture/pack contract used by ordinary checkpoints.
const CheckpointStagingProtocolVersion uint32 = 2

type CheckpointRepositorySpec struct {
	ID                  string                      `json:"id"`
	RelativeDestination string                      `json:"relative_destination"`
	Capture             CheckpointRepositoryCapture `json:"capture"`
	OriginOverride      *string                     `json:"origin_override"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (s *CheckpointRepositorySpec) UnmarshalJSON(data []byte) error {
	type plain CheckpointRepositorySpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

// CheckpointRepositoryCaptureMode is the "mode" tag of a
// CheckpointRepositoryCapture.
type CheckpointRepositoryCaptureMode string

const (
	// CheckpointRepositoryCaptureSessionDelta bundles commits reachable from
	// HEAD but from no origin ref.
	CheckpointRepositoryCaptureSessionDelta CheckpointRepositoryCaptureMode = "session_delta"
	// CheckpointRepositoryCaptureManagedClone saves all clone-owned refs and
	// stashes beyond the source launch commit (BaseCommit).
	CheckpointRepositoryCaptureManagedClone CheckpointRepositoryCaptureMode = "managed_clone"
	// CheckpointRepositoryCaptureDeltaFrom bundles the repository relative to
	// an explicit commit (BaseCommit).
	CheckpointRepositoryCaptureDeltaFrom CheckpointRepositoryCaptureMode = "delta_from"
	// CheckpointRepositoryCaptureRemoteWorkspace bundles a managed network
	// workspace relative to its immutable launch base recorded in
	// `mj.baseCommit`, even after the workspace has pushed and moved its
	// origin-tracking refs.
	CheckpointRepositoryCaptureRemoteWorkspace CheckpointRepositoryCaptureMode = "remote_workspace"
	// CheckpointRepositoryCaptureMetadataOnly preserves Git provenance and
	// durable review refs. The existing worktree remains the source for
	// working files and ordinary Git history.
	CheckpointRepositoryCaptureMetadataOnly CheckpointRepositoryCaptureMode = "metadata_only"
	// CheckpointRepositoryCapturePlainDirectory: the directory has no
	// committed Git checkout. Its files belong to the user's own directory on
	// a bare local target, exactly as a raw checkout's do, so the snapshot
	// records no Git state, runs no Git command, and never rewrites a working
	// file.
	CheckpointRepositoryCapturePlainDirectory CheckpointRepositoryCaptureMode = "plain_directory"
)

// CheckpointRepositoryCapture is the required repository capture semantics
// for the checkpoint protocol floor. It is Rust's internally tagged enum
// (tag "mode", snake_case variants) as one struct: BaseCommit is set exactly
// for ManagedClone and DeltaFrom.
type CheckpointRepositoryCapture struct {
	Mode       CheckpointRepositoryCaptureMode
	BaseCommit string
}

var checkpointRepositoryCaptureModes = []CheckpointRepositoryCaptureMode{
	CheckpointRepositoryCaptureSessionDelta,
	CheckpointRepositoryCaptureManagedClone,
	CheckpointRepositoryCaptureDeltaFrom,
	CheckpointRepositoryCaptureRemoteWorkspace,
	CheckpointRepositoryCaptureMetadataOnly,
	CheckpointRepositoryCapturePlainDirectory,
}

// MarshalJSON writes the variant as serde does: the "mode" tag first, then
// the variant's fields.
func (c CheckpointRepositoryCapture) MarshalJSON() ([]byte, error) {
	switch c.Mode {
	case CheckpointRepositoryCaptureManagedClone, CheckpointRepositoryCaptureDeltaFrom:
		return checkpointMarshal(struct {
			Mode       CheckpointRepositoryCaptureMode `json:"mode"`
			BaseCommit string                          `json:"base_commit"`
		}{c.Mode, c.BaseCommit})
	case CheckpointRepositoryCaptureSessionDelta, CheckpointRepositoryCaptureRemoteWorkspace,
		CheckpointRepositoryCaptureMetadataOnly, CheckpointRepositoryCapturePlainDirectory:
		return checkpointMarshal(struct {
			Mode CheckpointRepositoryCaptureMode `json:"mode"`
		}{c.Mode})
	}
	return nil, fmt.Errorf("unknown checkpoint repository capture mode %q", string(c.Mode))
}

// UnmarshalJSON reads the internally tagged enum. Struct variants refuse
// unknown fields (deny_unknown_fields); unit variants ignore other members,
// as serde's internally tagged unit visitor does.
func (c *CheckpointRepositoryCapture) UnmarshalJSON(data []byte) error {
	members, err := checkpointObject(data)
	if err != nil {
		return err
	}
	rawMode, ok := members["mode"]
	if !ok {
		return errors.New("missing field `mode`")
	}
	var mode string
	if err := json.Unmarshal(rawMode, &mode); err != nil {
		return err
	}
	switch CheckpointRepositoryCaptureMode(mode) {
	case CheckpointRepositoryCaptureManagedClone, CheckpointRepositoryCaptureDeltaFrom:
		var fields struct {
			BaseCommit string `json:"base_commit"`
		}
		delete(members, "mode")
		rest, err := json.Marshal(members)
		if err != nil {
			return err
		}
		if err := checkpointStrictDecode(rest, &fields, true); err != nil {
			return err
		}
		*c = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureMode(mode), BaseCommit: fields.BaseCommit}
		return nil
	case CheckpointRepositoryCaptureSessionDelta, CheckpointRepositoryCaptureRemoteWorkspace,
		CheckpointRepositoryCaptureMetadataOnly, CheckpointRepositoryCapturePlainDirectory:
		*c = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureMode(mode)}
		return nil
	}
	names := make([]string, len(checkpointRepositoryCaptureModes))
	for i, known := range checkpointRepositoryCaptureModes {
		names[i] = string(known)
	}
	return fmt.Errorf("unknown variant `%s`, %s", mode, checkpointExpected(names))
}

// CheckpointExportSpec is uploaded target-side input. It contains
// provenance and paths, never secrets.
type CheckpointExportSpec struct {
	ProtocolVersion uint32 `json:"protocol_version"`
	// IgnoreNestedRepositories is one suspension's explicit consent;
	// ordinary checkpoints remain strict.
	IgnoreNestedRepositories bool                       `json:"ignore_nested_repositories,omitempty" serde:"default"`
	Session                  core.SessionManifest       `json:"session"`
	Target                   core.TargetManifest        `json:"target"`
	Bundle                   core.BundleManifest        `json:"bundle"`
	RelayRoot                string                     `json:"relay_root"`
	HarnessHome              string                     `json:"harness_home"`
	WorkspaceRoot            string                     `json:"workspace_root"`
	Repositories             []CheckpointRepositorySpec `json:"repositories"`
	// ReportDirectories are the exact child directories owned by this
	// session. Absent on older controllers.
	ReportDirectories *[]string `json:"report_directories,omitempty"`
	// CanonicalSession is the controller projection latched at the relay's
	// checkpoint barrier.
	CanonicalSession core.CanonicalSessionSnapshot `json:"canonical_session"`
	OutputPath       string                        `json:"output_path"`
}

// MarshalJSON writes empty lists as [], as serde does.
func (s CheckpointExportSpec) MarshalJSON() ([]byte, error) {
	type plain CheckpointExportSpec
	v := plain(s)
	if v.Repositories == nil {
		v.Repositories = []CheckpointRepositorySpec{}
	}
	v.ReportDirectories = checkpointNonNilPaths(v.ReportDirectories)
	return checkpointMarshal(v)
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (s *CheckpointExportSpec) UnmarshalJSON(data []byte) error {
	type plain CheckpointExportSpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

// CheckpointCaptureSpec is small target-side input used while the relay
// checkpoint barrier is held. Canonical controller history is deliberately
// absent: it is streamed only after the captured target state has been
// sealed and ACP dispatch resumes.
type CheckpointCaptureSpec struct {
	ProtocolVersion uint32                     `json:"protocol_version"`
	Session         core.SessionManifest       `json:"session"`
	Target          core.TargetManifest        `json:"target"`
	Bundle          core.BundleManifest        `json:"bundle"`
	RelayRoot       string                     `json:"relay_root"`
	HarnessHome     string                     `json:"harness_home"`
	WorkspaceRoot   string                     `json:"workspace_root"`
	Repositories    []CheckpointRepositorySpec `json:"repositories"`
	// ReportDirectories are the exact child directories owned by this
	// session. Absent on older controllers.
	ReportDirectories *[]string `json:"report_directories,omitempty"`
	AllowEmptyNative  bool      `json:"allow_empty_native"`
	StagePath         string    `json:"stage_path"`
	// RefreshExisting refreshes a prestaged generation when its native
	// source tree is unchanged.
	RefreshExisting bool `json:"refresh_existing" serde:"default"`
}

// MarshalJSON writes empty lists as [], as serde does.
func (s CheckpointCaptureSpec) MarshalJSON() ([]byte, error) {
	type plain CheckpointCaptureSpec
	v := plain(s)
	if v.Repositories == nil {
		v.Repositories = []CheckpointRepositorySpec{}
	}
	v.ReportDirectories = checkpointNonNilPaths(v.ReportDirectories)
	return checkpointMarshal(v)
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (s *CheckpointCaptureSpec) UnmarshalJSON(data []byte) error {
	type plain CheckpointCaptureSpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

type CheckpointPackSpec struct {
	ProtocolVersion  uint32                        `json:"protocol_version"`
	RelayRoot        string                        `json:"relay_root"`
	StagePath        string                        `json:"stage_path"`
	CanonicalSession core.CanonicalSessionSnapshot `json:"canonical_session"`
	OutputPath       string                        `json:"output_path"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (s *CheckpointPackSpec) UnmarshalJSON(data []byte) error {
	type plain CheckpointPackSpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

// CheckpointPackSpecRef is CheckpointPackSpec borrowing the canonical
// session, so a sender can serialize a large snapshot without first copying
// it. Its JSON is identical to the owned spec's. Serialization only.
type CheckpointPackSpecRef struct {
	ProtocolVersion  uint32                         `json:"protocol_version"`
	RelayRoot        string                         `json:"relay_root"`
	StagePath        string                         `json:"stage_path"`
	CanonicalSession *core.CanonicalSessionSnapshot `json:"canonical_session"`
	OutputPath       string                         `json:"output_path"`
}

// CheckpointReportOmissions are report files omitted from a sealed
// generation, retained when it is refreshed.
type CheckpointReportOmissions struct {
	Files              uint64                      `json:"files"`
	Bytes              uint64                      `json:"bytes"`
	LargestDirectories []CheckpointReportDirectory `json:"largest_directories"`
}

// MarshalJSON writes an empty list as [], as serde does.
func (o CheckpointReportOmissions) MarshalJSON() ([]byte, error) {
	type plain CheckpointReportOmissions
	v := plain(o)
	if v.LargestDirectories == nil {
		v.LargestDirectories = []CheckpointReportDirectory{}
	}
	return checkpointMarshal(v)
}

// UnmarshalJSON requires every field and accepts unknown ones, as serde does
// for this struct.
func (o *CheckpointReportOmissions) UnmarshalJSON(data []byte) error {
	type plain CheckpointReportOmissions
	return checkpointStrictDecode(data, (*plain)(o), false)
}

type CheckpointReportDirectory struct {
	ChildID string `json:"child_id"`
	Bytes   uint64 `json:"bytes"`
}

// UnmarshalJSON requires every field and accepts unknown ones, as serde does
// for this struct.
func (d *CheckpointReportDirectory) UnmarshalJSON(data []byte) error {
	type plain CheckpointReportDirectory
	return checkpointStrictDecode(data, (*plain)(d), false)
}

type CapturedCheckpoint struct {
	OmittedReports  CheckpointReportOmissions `json:"omitted_reports" serde:"default"`
	StagePath       string                    `json:"stage_path"`
	NativeBytes     uint64                    `json:"native_bytes"`
	RepositoryBytes uint64                    `json:"repository_bytes"`
	ReusedNative    bool                      `json:"reused_native"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (c *CapturedCheckpoint) UnmarshalJSON(data []byte) error {
	type plain CapturedCheckpoint
	return checkpointStrictDecode(data, (*plain)(c), true)
}

// CheckpointExportSpecRead reads an export spec file (Rust
// CheckpointExportSpec::read).
func CheckpointExportSpecRead(path string) (CheckpointExportSpec, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return CheckpointExportSpec{}, fmt.Errorf("read checkpoint export spec %s: %w", path, err)
	}
	var spec CheckpointExportSpec
	if err := json.Unmarshal(body, &spec); err != nil {
		return CheckpointExportSpec{}, fmt.Errorf("parse checkpoint export spec %s: %w", path, err)
	}
	return spec, nil
}

// CheckpointExportSpecReadFrom reads an export spec from standard input
// (Rust CheckpointExportSpec::read_from).
func CheckpointExportSpecReadFrom(reader io.Reader) (CheckpointExportSpec, error) {
	body, err := io.ReadAll(reader)
	if err != nil {
		return CheckpointExportSpec{}, fmt.Errorf("read checkpoint export spec from standard input: %w", err)
	}
	var spec CheckpointExportSpec
	if err := json.Unmarshal(body, &spec); err != nil {
		return CheckpointExportSpec{}, fmt.Errorf("parse checkpoint export spec from standard input: %w", err)
	}
	return spec, nil
}

// Write writes the spec as pretty JSON readable only by its owner.
func (s *CheckpointExportSpec) Write(path string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		return err
	}
	body, err := checkpointMarshalPretty(s)
	if err != nil {
		return err
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	if _, err := file.Write(body); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return RestrictPermissions(path)
}

type TargetCheckpoint struct {
	Path                string `json:"path"`
	Sha256              string `json:"sha256"`
	EventFrontier       uint64 `json:"event_frontier"`
	EventFrontierDigest string `json:"event_frontier_digest"`
	// Timings are phase timings measured on the target. A worker that
	// predates this field simply omits it, so the controller must treat it
	// as optional.
	Timings *CheckpointExportTimings `json:"timings,omitempty"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (t *TargetCheckpoint) UnmarshalJSON(data []byte) error {
	type plain TargetCheckpoint
	return checkpointStrictDecode(data, (*plain)(t), true)
}

// CheckpointExportTimings is the wall-clock cost of each target-side
// checkpoint phase, in milliseconds.
//
// The worker runs as a child process over ssh or `podman exec`, so its own
// tracing output never reaches the daemon log. These numbers ride back in
// the JSON result instead, which is the only channel the controller reads.
type CheckpointExportTimings struct {
	// NativeMs is collecting native harness artifacts from the relay and
	// harness home.
	NativeMs uint64 `json:"native_ms"`
	// RepositoriesMs is collecting Git bundles and patches for every
	// workspace repository.
	RepositoriesMs uint64 `json:"repositories_ms"`
	// ArchiveMs is writing and hashing the archive.
	ArchiveMs uint64 `json:"archive_ms"`
	// TotalMs is the whole target-side operation, including validation and
	// path resolution.
	TotalMs uint64 `json:"total_ms"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (t *CheckpointExportTimings) UnmarshalJSON(data []byte) error {
	type plain CheckpointExportTimings
	return checkpointStrictDecode(data, (*plain)(t), true)
}

// ExportSpecStdin is the spec argument that means "read the export spec
// from standard input".
//
// Streaming the spec saves one round trip to the target, which is time the
// relay spends with ACP dispatch frozen behind the checkpoint barrier.
const ExportSpecStdin = "-"

// ReadJSONFrom reads one JSON value of type T from standard input.
func ReadJSONFrom[T any](reader io.Reader, description string) (T, error) {
	var value T
	body, err := io.ReadAll(reader)
	if err != nil {
		return value, fmt.Errorf("read %s from standard input: %w", description, err)
	}
	if err := json.Unmarshal(body, &value); err != nil {
		return value, fmt.Errorf("parse %s from standard input: %w", description, err)
	}
	return value, nil
}

type checkpointStageManifest struct {
	OmittedReports    CheckpointReportOmissions `json:"omitted_reports" serde:"default"`
	ProtocolVersion   uint32                    `json:"protocol_version"`
	Session           core.SessionManifest      `json:"session"`
	Target            core.TargetManifest       `json:"target"`
	Bundle            core.BundleManifest       `json:"bundle"`
	SourceFingerprint *string                   `json:"source_fingerprint"`
	NativeArtifacts   []stagedNativeArtifact    `json:"native_artifacts"`
	Repositories      []stagedRepository        `json:"repositories"`
}

// MarshalJSON writes empty lists as [], as serde does.
func (m checkpointStageManifest) MarshalJSON() ([]byte, error) {
	type plain checkpointStageManifest
	v := plain(m)
	if v.NativeArtifacts == nil {
		v.NativeArtifacts = []stagedNativeArtifact{}
	}
	if v.Repositories == nil {
		v.Repositories = []stagedRepository{}
	}
	return checkpointMarshal(v)
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (m *checkpointStageManifest) UnmarshalJSON(data []byte) error {
	type plain checkpointStageManifest
	return checkpointStrictDecode(data, (*plain)(m), true)
}

type stagedNativeArtifact struct {
	RelativePath string `json:"relative_path"`
	Mode         uint32 `json:"mode"`
	Size         uint64 `json:"size"`
	BodyPath     string `json:"body_path"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (a *stagedNativeArtifact) UnmarshalJSON(data []byte) error {
	type plain stagedNativeArtifact
	return checkpointStrictDecode(data, (*plain)(a), true)
}

type stagedRepository struct {
	Metadata            core.RepositoryMetadata `json:"metadata"`
	CommittedBundlePath string                  `json:"committed_bundle_path"`
	StagedPatchPath     string                  `json:"staged_patch_path"`
	UnstagedPatchPath   string                  `json:"unstaged_patch_path"`
	UntrackedTarPath    string                  `json:"untracked_tar_path"`
	// ConflictedWorktreeTarPath and OperationStateTarPath are the Git
	// operation's two payloads, present exactly when
	// Metadata.GitOperation is.
	ConflictedWorktreeTarPath *string                `json:"conflicted_worktree_tar_path,omitempty"`
	OperationStateTarPath     *string                `json:"operation_state_tar_path,omitempty"`
	LinkedWorktrees           []stagedLinkedWorktree `json:"linked_worktrees,omitempty" serde:"default"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (r *stagedRepository) UnmarshalJSON(data []byte) error {
	type plain stagedRepository
	return checkpointStrictDecode(data, (*plain)(r), true)
}

type stagedLinkedWorktree struct {
	Metadata          core.LinkedWorktreeMetadata `json:"metadata"`
	StagedPatchPath   string                      `json:"staged_patch_path"`
	UnstagedPatchPath string                      `json:"unstaged_patch_path"`
	UntrackedTarPath  string                      `json:"untracked_tar_path"`
	// Present exactly when Metadata.GitOperation is.
	ConflictedWorktreeTarPath *string `json:"conflicted_worktree_tar_path,omitempty"`
	OperationStateTarPath     *string `json:"operation_state_tar_path,omitempty"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (w *stagedLinkedWorktree) UnmarshalJSON(data []byte) error {
	type plain stagedLinkedWorktree
	return checkpointStrictDecode(data, (*plain)(w), true)
}

// RepairOriginRefs: a session delta is measured against every origin ref, so
// a repository that lost its remote-tracking refs would silently have
// nothing to exclude. Try one repair fetch, then fail the checkpoint instead
// of bundling full history.
//
// This is the one network call in an export. It cannot stop on a prompt:
// SystemGit runs every child with NonInteractiveGitEnv.
//
// Public because the same rule applies wherever a session delta is bundled,
// including the raw-checkout conversion in the controller.
func RepairOriginRefs(git GitCommandRunner, path string, id string) error {
	listed := func() (bool, error) {
		ok, err := HasOriginRefs(git, path)
		if err != nil {
			return false, fmt.Errorf("repository '%s': %w", id, err)
		}
		return ok, nil
	}
	if ok, err := listed(); err != nil || ok {
		return err
	}
	fetch, err := git.Run(path, &GitCommand{Arguments: []string{"fetch", "origin"}})
	if err != nil {
		return err
	}
	if ok, err := listed(); err != nil || ok {
		return err
	}
	outcome := "repair fetch produced no origin refs"
	if fetch.Status != 0 {
		outcome = fmt.Sprintf("repair fetch failed with status %d: %s", fetch.Status, strings.TrimSpace(checkpointLossy(fetch.Stderr)))
	}
	return fmt.Errorf("repository '%s' has no origin refs to delta against; refusing to bundle full history (%s)", id, outcome)
}

// CurrentNativeSessionReceivedPrompt is whether the native session the
// checkpoint continues ever received a prompt. `/clear` replaces the native
// session, so only the conversation after the newest context boundary
// belongs to the current one. Codex writes a rollout, and Claude Code a
// transcript, only when a turn runs, so a session with no prompt here has no
// native history to lose.
func CurrentNativeSessionReceivedPrompt(snapshot *core.CanonicalSessionSnapshot) bool {
	start := snapshot.CurrentContextStart()
	for i := range snapshot.Transcript {
		item := &snapshot.Transcript[i]
		if item.Position < start {
			continue
		}
		_, user := item.Body.(core.CanonicalTranscriptBodyUser)
		// The shipped canonical format rejects unknown fields. Keep adoption
		// conservative until its native-use proof can be carried by the
		// planned archive schema revision.
		if user || item.StableID == core.AdoptionHistoryUnavailableItemID {
			return true
		}
	}
	return false
}

func validateRelativePath(path string) error {
	if path == "" || filepath.IsAbs(path) {
		return errors.New("invalid relative path")
	}
	for _, component := range checkpointComponents(path) {
		if component.kind != checkpointComponentNormal {
			return errors.New("relative path traversal")
		}
	}
	return nil
}

// ResolveTargetPath resolves an absolute or `~`-relative target path.
func ResolveTargetPath(path string) (string, error) {
	components := checkpointComponents(path)
	for _, component := range components {
		if component.kind == checkpointComponentParentDir {
			return "", errors.New("target path traverses a parent")
		}
	}
	if filepath.IsAbs(path) {
		return path, nil
	}
	if len(components) > 0 && components[0].text == "~" {
		home, ok := os.LookupEnv("HOME")
		if !ok {
			return "", errors.New("HOME is required to expand target path")
		}
		return core.ExpandHome(path, &home)
	}
	return "", errors.New("target path must be absolute or start with '~'")
}

// resolveHomeRelativeTargetPath: SSH and EC2 worker launch files use
// login-home-relative paths, matching the working directory of their remote
// commands. Checkpoint collection runs in the same account but resolves
// paths explicitly instead of relying on cwd.
func resolveHomeRelativeTargetPath(path string) (string, error) {
	components := checkpointComponents(path)
	if filepath.IsAbs(path) || (len(components) > 0 && components[0].text == "~") {
		return ResolveTargetPath(path)
	}
	return ResolveTargetPath(core.Join("~", path))
}

// fileMode is the permission bits of a file; 0o600 off Unix.
func fileMode(info fs.FileInfo) uint32 {
	if runtime.GOOS == "windows" {
		return 0o600
	}
	return uint32(info.Mode().Perm())
}

// RestrictPermissions makes a file readable and writable only by its owner
// on Unix; elsewhere it does nothing.
func RestrictPermissions(path string) error {
	if runtime.GOOS == "windows" {
		return nil
	}
	return os.Chmod(path, 0o600)
}

// SyncDirectory is core.SyncDirectory (Rust re-exports
// mj_core::config::sync_directory here).
func SyncDirectory(path string) error {
	return core.SyncDirectory(path)
}

// RemoveFailedCheckpointInstall removes an incomplete install after err,
// adding any removal failure other than absence to err.
func RemoveFailedCheckpointInstall(path string, err error) error {
	removeErr := os.Remove(path)
	if removeErr == nil || errors.Is(removeErr, fs.ErrNotExist) {
		return err
	}
	return fmt.Errorf("also failed to remove incomplete checkpoint install %s: %v: %w", path, removeErr, err)
}

// CheckpointSha256 is the lower-hex SHA-256 of a file.
func CheckpointSha256(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open %s: %w", path, err)
	}
	defer file.Close()
	digest := sha256.New()
	if _, err := io.CopyBuffer(digest, file, make([]byte, 1024*1024)); err != nil {
		return "", fmt.Errorf("read %s: %w", path, err)
	}
	return core.LowerHex(digest.Sum(nil)), nil
}

// The helpers below stand in for Rust's serde and std::path behaviour in this
// module. They are kept local to the checkpoint module (prefixed) until a
// shared copy is chosen in the final cleanup.

// checkpointMarshal encodes v as serde_json::to_vec does: compact, without
// HTML escaping.
func checkpointMarshal(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// checkpointMarshalPretty encodes v as serde_json::to_vec_pretty does.
func checkpointMarshalPretty(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// checkpointNonNilPaths writes a present empty path list as [] rather than
// null.
func checkpointNonNilPaths(paths *[]string) *[]string {
	if paths != nil && *paths == nil {
		empty := []string{}
		return &empty
	}
	return paths
}

// checkpointObject reads a JSON object's members.
func checkpointObject(data []byte) (map[string]json.RawMessage, error) {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return nil, err
	}
	if members == nil {
		return nil, errors.New("invalid type: null, expected a map")
	}
	return members, nil
}

// checkpointExpected is serde's list of expected names in an error.
func checkpointExpected(names []string) string {
	quoted := make([]string, len(names))
	for i, name := range names {
		quoted[i] = "`" + name + "`"
	}
	switch len(quoted) {
	case 0:
		return "there are no fields"
	case 1:
		return "expected " + quoted[0]
	case 2:
		return "expected " + quoted[0] + " or " + quoted[1]
	}
	return "expected one of " + strings.Join(quoted, ", ")
}

// checkpointFields lists a struct type's JSON member names and the required
// ones: a member is optional when its field is a pointer, has omitempty, or
// is tagged serde:"default".
func checkpointFields(t reflect.Type) (known, required []string) {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	for i := 0; i < t.NumField(); i++ {
		field := t.Field(i)
		tag := field.Tag.Get("json")
		if tag == "-" || !field.IsExported() {
			continue
		}
		name, options, _ := strings.Cut(tag, ",")
		if name == "" {
			name = field.Name
		}
		known = append(known, name)
		optional := field.Type.Kind() == reflect.Pointer ||
			strings.Contains(options, "omitempty") ||
			field.Tag.Get("serde") == "default"
		if !optional {
			required = append(required, name)
		}
	}
	return known, required
}

// checkpointStrictDecode decodes data into v (a pointer to a struct without
// its own UnmarshalJSON), rejecting a missing required member and, with
// deny, any member that is not a field of v (serde deny_unknown_fields).
func checkpointStrictDecode(data []byte, v any, deny bool) error {
	members, err := checkpointObject(data)
	if err != nil {
		return err
	}
	known, required := checkpointFields(reflect.TypeOf(v))
	if deny {
		allowed := make(map[string]bool, len(known))
		for _, name := range known {
			allowed[name] = true
		}
		names := make([]string, 0, len(members))
		for name := range members {
			names = append(names, name)
		}
		sort.Strings(names)
		for _, name := range names {
			if !allowed[name] {
				return fmt.Errorf("unknown field `%s`, %s", name, checkpointExpected(known))
			}
		}
	}
	for _, name := range required {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return json.Unmarshal(data, v)
}

// checkpointComponentKind is Rust's std::path::Component variant, in its
// derived order.
type checkpointComponentKind int

const (
	checkpointComponentPrefix checkpointComponentKind = iota
	checkpointComponentRootDir
	checkpointComponentCurDir
	checkpointComponentParentDir
	checkpointComponentNormal
)

type checkpointComponent struct {
	kind checkpointComponentKind
	text string
}

func checkpointIsSeparator(b byte) bool {
	return b == '/' || (runtime.GOOS == "windows" && b == '\\')
}

// checkpointComponents is Rust's Path::components: repeated separators and
// `.` after the first component are dropped, a leading `.` of a relative
// path is kept as CurDir, and `..` is ParentDir. On Windows a drive or UNC
// prefix comes first and both separators count.
func checkpointComponents(path string) []checkpointComponent {
	var components []checkpointComponent
	rest := path
	if runtime.GOOS == "windows" {
		if volume := filepath.VolumeName(path); volume != "" {
			components = append(components, checkpointComponent{checkpointComponentPrefix, volume})
			rest = path[len(volume):]
		}
	}
	if rest != "" && checkpointIsSeparator(rest[0]) {
		components = append(components, checkpointComponent{checkpointComponentRootDir, rest[:1]})
		rest = rest[1:]
	}
	parts := strings.FieldsFunc(rest, func(r rune) bool { return r < 0x80 && checkpointIsSeparator(byte(r)) })
	leading := rest != "" && !checkpointIsSeparator(rest[0])
	for i, part := range parts {
		switch {
		case part == ".":
			if i == 0 && leading && len(components) == 0 {
				components = append(components, checkpointComponent{checkpointComponentCurDir, part})
			}
		case part == "..":
			components = append(components, checkpointComponent{checkpointComponentParentDir, part})
		default:
			components = append(components, checkpointComponent{checkpointComponentNormal, part})
		}
	}
	return components
}

// checkpointJoinComponents rebuilds a path from components, as collecting
// them into a PathBuf does.
func checkpointJoinComponents(components []checkpointComponent) string {
	var path strings.Builder
	for i, component := range components {
		if component.kind != checkpointComponentPrefix && component.kind != checkpointComponentRootDir &&
			i > 0 && components[i-1].kind != checkpointComponentPrefix && components[i-1].kind != checkpointComponentRootDir {
			path.WriteRune(filepath.Separator)
		}
		path.WriteString(component.text)
	}
	return path.String()
}

// checkpointNormalize is `path.components().collect::<PathBuf>()`.
func checkpointNormalize(path string) string {
	return checkpointJoinComponents(checkpointComponents(path))
}

// checkpointPathCompare is Rust's Ord for paths: components compared in
// order, by kind and then by bytes.
func checkpointPathCompare(left, right string) int {
	a, b := checkpointComponents(left), checkpointComponents(right)
	for i := 0; i < len(a) && i < len(b); i++ {
		if a[i].kind != b[i].kind {
			return int(a[i].kind) - int(b[i].kind)
		}
		if c := strings.Compare(a[i].text, b[i].text); c != 0 {
			return c
		}
	}
	return len(a) - len(b)
}

// checkpointPathEqual is Rust's == for paths.
func checkpointPathEqual(left, right string) bool {
	return checkpointPathCompare(left, right) == 0
}

// checkpointStripPrefix is Path::strip_prefix: the rest of path after the
// components of base, when base's components lead path's.
func checkpointStripPrefix(path, base string) (string, bool) {
	components, prefix := checkpointComponents(path), checkpointComponents(base)
	if len(prefix) > len(components) {
		return "", false
	}
	for i := range prefix {
		if components[i] != prefix[i] {
			return "", false
		}
	}
	return checkpointJoinComponents(components[len(prefix):]), true
}

// checkpointPathStartsWith is Path::starts_with.
func checkpointPathStartsWith(path, base string) bool {
	_, ok := checkpointStripPrefix(path, base)
	return ok
}

// checkpointPathEndsWith is Path::ends_with.
func checkpointPathEndsWith(path, child string) bool {
	components, suffix := checkpointComponents(path), checkpointComponents(child)
	if len(suffix) > len(components) {
		return false
	}
	offset := len(components) - len(suffix)
	for i := range suffix {
		if components[offset+i] != suffix[i] {
			return false
		}
	}
	return true
}

// checkpointFileName is Path::file_name: the last component when it is a
// normal name.
func checkpointFileName(path string) (string, bool) {
	components := checkpointComponents(path)
	if len(components) == 0 || components[len(components)-1].kind != checkpointComponentNormal {
		return "", false
	}
	return components[len(components)-1].text, true
}

// checkpointParent is Path::parent.
func checkpointParent(path string) (string, bool) {
	components := checkpointComponents(path)
	if len(components) == 0 {
		return "", false
	}
	switch components[len(components)-1].kind {
	case checkpointComponentNormal, checkpointComponentCurDir, checkpointComponentParentDir:
		return checkpointJoinComponents(components[:len(components)-1]), true
	}
	return "", false
}

// checkpointExtension is Path::extension.
func checkpointExtension(path string) (string, bool) {
	name, ok := checkpointFileName(path)
	if !ok || name == ".." {
		return "", false
	}
	dot := strings.LastIndexByte(name, '.')
	if dot <= 0 {
		return "", false
	}
	return name[dot+1:], true
}

// checkpointTryExists is Path::try_exists: false only for a path that is
// not there, an error for any other failure to inspect it.
func checkpointTryExists(path string) (bool, error) {
	_, err := os.Stat(path)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return false, err
}

// checkpointLossy is String::from_utf8_lossy.
func checkpointLossy(data []byte) string {
	return strings.ToValidUTF8(string(data), "\uFFFD")
}

// checkpointCanonicalize is fs::canonicalize: the absolute path with every
// symlink resolved; an error when it does not exist.
func checkpointCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// errCheckpointPrefixNotFound is Rust's StripPrefixError, which a failed
// Path::strip_prefix leaves on the error chain.
var errCheckpointPrefixNotFound = errors.New("prefix not found")

// checkpointIsDir is Path::is_dir: it follows symlinks and is false when the
// path cannot be inspected.
func checkpointIsDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

// checkpointExists is Path::exists: it follows symlinks and is false when the
// path cannot be inspected.
func checkpointExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// checkpointIsSymlink reports a symbolic link in Lstat information.
func checkpointIsSymlink(info fs.FileInfo) bool {
	return info.Mode()&fs.ModeSymlink != 0
}

// checkpointDebugString renders text as Rust's `{:?}` does for a str.
func checkpointDebugString(text string) string {
	var b strings.Builder
	b.WriteByte('"')
	for i, r := range text {
		switch {
		case r == '\t':
			b.WriteString(`\t`)
		case r == '\r':
			b.WriteString(`\r`)
		case r == '\n':
			b.WriteString(`\n`)
		case r == '\\':
			b.WriteString(`\\`)
		case r == '"':
			b.WriteString(`\"`)
		case r == 0:
			b.WriteString(`\0`)
		case r == utf8.RuneError && !strings.HasPrefix(text[i:], "�"):
			// Invalid UTF-8 cannot occur in a Rust str; keep the byte visible.
			fmt.Fprintf(&b, `\u{%x}`, text[i])
		case !unicode.IsGraphic(r) || unicode.In(r, unicode.Mn, unicode.Me):
			// Rust escapes non-printable and grapheme-extending characters.
			fmt.Fprintf(&b, `\u{%x}`, r)
		default:
			b.WriteRune(r)
		}
	}
	b.WriteByte('"')
	return b.String()
}

// checkpointSortArtifacts sorts artifacts by relative path as Rust's stable
// `sort_by` on PathBuf does.
func checkpointSortArtifacts(artifacts []core.NativeArtifact) {
	slices.SortStableFunc(artifacts, func(left, right core.NativeArtifact) int {
		return checkpointPathCompare(left.RelativePath, right.RelativePath)
	})
}
