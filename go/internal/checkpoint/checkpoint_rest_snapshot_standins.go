// Stand-ins for the items of mj-checkpoint's snapshot.rs (Rust 3dde0c11b)
// that the checkpoint-rest sync calls: the snapshot types and entry points
// capture.rs and restore.rs use, and the snapshot helpers the snapshot
// inspection in checkpoint/submodules.rs uses.
//
// They exist only so this package compiles before the checkpoint-snapshot
// sync is merged. That sync ports snapshot.rs in snapshot.go under these
// names and signatures; when both are merged, delete this whole file. The
// three small helpers at the end (snapshotNonEmptyFields, snapshotIsAbsolute
// and snapshotAbsolute) are copies of that port so the archive and
// submodule code works here; every other stand-in fails with
// errSnapshotNotMerged.

package checkpoint

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

var errSnapshotNotMerged = errors.New("origin-relative workspace snapshots need the checkpoint-snapshot port (snapshot.go)")

// Re-exports of mj-core's consent and snapshot metadata types (Rust `pub
// use` in snapshot.rs).
type (
	CaptureConsent            = core.CaptureConsent
	SnapshotGitlink           = core.SnapshotGitlink
	SnapshotHead              = core.SnapshotHead
	SnapshotStashEntry        = core.SnapshotStashEntry
	SnapshotSubmodule         = core.SnapshotSubmodule
	WorkspaceSnapshotMetadata = core.WorkspaceSnapshotMetadata
)

// WorkspaceSnapshot is the metadata of one captured checkout and its pack.
type WorkspaceSnapshot struct {
	Metadata WorkspaceSnapshotMetadata `json:"metadata"`
	// Pack is a non-thin Git pack, if origin does not supply every object.
	Pack *string `json:"pack"`
}

// CheckoutOwnership is who owns a checkout: Mjolnir (managed), or the user
// (raw), with the session's launch record when one was written.
type CheckoutOwnership struct {
	Raw    bool
	Launch *LaunchRecord
}

func CheckoutOwnershipManaged() CheckoutOwnership { return CheckoutOwnership{} }

func CheckoutOwnershipRaw(launch *LaunchRecord) CheckoutOwnership {
	return CheckoutOwnership{Raw: true, Launch: launch}
}

// LaunchRecord is the raw checkout's state when its session launched.
type LaunchRecord struct {
	Version   uint32               `json:"version"`
	Refs      map[string]string    `json:"refs"`
	Stash     []SnapshotStashEntry `json:"stash"`
	Worktrees []string             `json:"worktrees"`
}

func ReadLaunchRecord(GitCommandRunner, string, string) (*LaunchRecord, error) {
	return nil, errSnapshotNotMerged
}

func RecordLaunchIfAbsent(GitCommandRunner, string, string) error {
	return errSnapshotNotMerged
}

type ExclusionSourceKind int

const (
	ExclusionSourceOrigin ExclusionSourceKind = iota
	ExclusionSourceRemote
	ExclusionSourceCommits
)

// ExclusionSource is what a capture's pack is relative to.
type ExclusionSource struct {
	Kind    ExclusionSourceKind
	Remote  string
	Commits []string
}

type RepositoryInput struct {
	ID             string            `json:"id"`
	Checkout       string            `json:"checkout"`
	Ownership      CheckoutOwnership `json:"ownership"`
	OriginOverride *string           `json:"origin_override"`
	PackPath       string            `json:"pack_path"`
	Exclusions     ExclusionSource   `json:"exclusions"`
	KeepDir        *string           `json:"keep_dir"`
}

type SessionCaptureConsent struct {
	Repositories map[string]CaptureConsent `json:"repositories"`
}

type SessionSnapshot struct {
	Repositories map[string]WorkspaceSnapshot `json:"repositories"`
}

func CaptureSessionSnapshot(GitCommandRunner, []RepositoryInput, SessionCaptureConsent) (SessionSnapshot, error) {
	return SessionSnapshot{}, errSnapshotNotMerged
}

func RestoreWorkspaceSnapshot(GitCommandRunner, WorkspaceSnapshot, string) ([]string, error) {
	return nil, errSnapshotNotMerged
}

func RepairWorkspaceSnapshot(GitCommandRunner, WorkspaceSnapshot, string) error {
	return errSnapshotNotMerged
}

type UnrepresentedWorktree struct {
	Locked        bool   `json:"locked"`
	CanMoveInside bool   `json:"can_move_inside"`
	GitDir        string `json:"git_dir"`
	Path          string `json:"path"`
}

// uninitializedOrigin is the configured local origin route of an
// uninitialized submodule.
type uninitializedOrigin struct {
	owner     string
	configKey string
	route     string
}

// snapshotTempFile is a temporary file a guard removes.
type snapshotTempFile struct {
	path string
}

func (t snapshotTempFile) remove() { os.Remove(t.path) }

func intentToAddPaths(GitCommandRunner, string) ([]string, error) {
	return nil, errSnapshotNotMerged
}

func privateIndex(GitCommandRunner, string) (snapshotTempFile, error) {
	return snapshotTempFile{}, errSnapshotNotMerged
}

func resolveSubmoduleURL(GitCommandRunner, string, string, *string) (string, error) {
	return "", errSnapshotNotMerged
}

func submoduleSelectionHasDelta(GitCommandRunner, string, []string) (bool, bool, error) {
	return false, false, errSnapshotNotMerged
}

func localOriginRoute(string, string) (string, bool, error) {
	return "", false, errSnapshotNotMerged
}

func gitPresencePaths(GitCommandRunner, string) ([]string, error) {
	return nil, errSnapshotNotMerged
}

// snapshotNonEmptyFields splits data at separator and drops empty fields.
func snapshotNonEmptyFields(data []byte, separator byte) [][]byte {
	var fields [][]byte
	for _, field := range bytes.Split(data, []byte{separator}) {
		if len(field) > 0 {
			fields = append(fields, field)
		}
	}
	return fields
}

func snapshotIsAbsolute(path string) bool { return filepath.IsAbs(path) }

// snapshotAbsolute is Rust's std::path::absolute. On Unix a relative path is
// joined to the current directory, `.` components and repeated separators
// are dropped, `..` components and a trailing separator are kept, and the
// filesystem is not consulted. Windows resolves the path as
// GetFullPathNameW does.
func snapshotAbsolute(path string) (string, error) {
	if path == "" {
		return "", errors.New("cannot make an empty path absolute")
	}
	if runtime.GOOS == "windows" {
		return filepath.Abs(path)
	}
	var absolute string
	rest := path
	switch {
	case strings.HasPrefix(path, "//") && !strings.HasPrefix(path, "///"):
		absolute = "//"
		rest = path[2:]
	case strings.HasPrefix(path, "/"):
		absolute = "/"
	default:
		cwd, err := os.Getwd()
		if err != nil {
			return "", err
		}
		absolute = cwd
	}
	for _, part := range strings.Split(rest, "/") {
		if part == "" || part == "." {
			continue
		}
		absolute = gitJoin(absolute, part)
	}
	if strings.HasSuffix(path, "/") && !strings.HasSuffix(absolute, "/") {
		absolute += "/"
	}
	return absolute, nil
}
