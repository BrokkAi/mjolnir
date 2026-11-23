// Port of mj-checkpoint/src/snapshot.rs.
//
// Origin-relative Git snapshots, packing pinned objects without temporary
// refs. Filesystem paths belong to the machine running the supplied Git
// runner.

package checkpoint

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"maps"
	"math/big"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"time"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"lukechampine.com/blake3"
)

const rawNotice = "raw checkout is kept; teardown audits are skipped; untracked files and unsupported Git state do not travel with the snapshot"

// Re-exports of mj-core's consent and snapshot metadata types (Rust `pub
// use`).
type (
	CaptureConsent            = core.CaptureConsent
	CheckoutKey               = core.CheckoutKey
	GitOperation              = core.GitOperation
	GitOperationKind          = core.GitOperationKind
	OperationConsent          = core.OperationConsent
	ShownOperation            = core.ShownOperation
	UntrackedConsent          = core.UntrackedConsent
	UntrackedItem             = core.UntrackedItem
	UntrackedPath             = core.UntrackedPath
	BranchTracking            = core.BranchTracking
	SnapshotGitlink           = core.SnapshotGitlink
	SnapshotHead              = core.SnapshotHead
	SnapshotStashEntry        = core.SnapshotStashEntry
	SnapshotSubmodule         = core.SnapshotSubmodule
	SnapshotWorktree          = core.SnapshotWorktree
	WorkspaceSnapshotMetadata = core.WorkspaceSnapshotMetadata
)

// Scoped is a value inside one checkout of a session repository.
type Scoped[T any] struct {
	Checkout CheckoutKey `json:"checkout"`
	Value    T           `json:"value"`
}

func (s *Scoped[T]) UnmarshalJSON(data []byte) error {
	members, err := checkpointObject(data)
	if err != nil {
		return err
	}
	for _, name := range []string{"checkout", "value"} {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	var scopedValue Scoped[T]
	if err := json.Unmarshal(members["checkout"], &scopedValue.Checkout); err != nil {
		return err
	}
	if err := json.Unmarshal(members["value"], &scopedValue.Value); err != nil {
		return err
	}
	*s = scopedValue
	return nil
}

func scoped[T any](checkout CheckoutKey, value T) Scoped[T] {
	return Scoped[T]{Checkout: checkout, Value: value}
}

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

// CheckoutOwnershipManaged is Rust's `CheckoutOwnership::Managed`.
func CheckoutOwnershipManaged() CheckoutOwnership { return CheckoutOwnership{} }

// CheckoutOwnershipRaw is Rust's `CheckoutOwnership::Raw { launch }`.
func CheckoutOwnershipRaw(launch *LaunchRecord) CheckoutOwnership {
	return CheckoutOwnership{Raw: true, Launch: launch}
}

func (o CheckoutOwnership) MarshalJSON() ([]byte, error) {
	if !o.Raw {
		return []byte(`"Managed"`), nil
	}
	body, err := json.Marshal(struct {
		Launch *LaunchRecord `json:"launch"`
	}{o.Launch})
	if err != nil {
		return nil, err
	}
	return []byte(`{"Raw":` + string(body) + `}`), nil
}

func (o *CheckoutOwnership) UnmarshalJSON(data []byte) error {
	tag, body, err := snapshotExternalTag(data)
	if err != nil {
		return err
	}
	switch tag {
	case "Managed":
		*o = CheckoutOwnershipManaged()
	case "Raw":
		members, err := checkpointObject(body)
		if err != nil {
			return err
		}
		var launch *LaunchRecord
		if raw, ok := members["launch"]; ok {
			if err := json.Unmarshal(raw, &launch); err != nil {
				return err
			}
		}
		*o = CheckoutOwnershipRaw(launch)
	default:
		return fmt.Errorf("unknown variant `%s`, expected `Managed` or `Raw`", tag)
	}
	return nil
}

// LaunchRecord is the raw checkout's state when its session launched.
// Version 1 is a UTF-8 JSON object followed by a newline. Paths use serde's
// platform PathBuf representation. Stashes are newest first; refs are full
// names.
type LaunchRecord struct {
	Version   uint32               `json:"version"`
	Refs      map[string]string    `json:"refs"`
	Stash     []SnapshotStashEntry `json:"stash"`
	Worktrees []string             `json:"worktrees"`
}

func (r LaunchRecord) MarshalJSON() ([]byte, error) {
	type plain LaunchRecord
	p := plain(r)
	p.Refs = snapshotNonNilMap(p.Refs)
	p.Stash = snapshotNonNil(p.Stash)
	p.Worktrees = snapshotNonNil(p.Worktrees)
	return snapshotMarshal(p)
}

func (r *LaunchRecord) UnmarshalJSON(data []byte) error {
	type plain LaunchRecord
	return checkpointStrictDecode(data, (*plain)(r), false)
}

// FileIdentityOf is the identity of one metadata observation (Rust's
// `file_identity`; renamed in Go because the type has the same name). Use
// the same observation for this identity and the displayed size. The
// controller can use this for .mj/keep entries as well as untracked files.
func FileIdentityOf(metadata fs.FileInfo) (core.FileIdentity, error) {
	identity := blake3.New(32, nil)
	if err := hashFileMetadata(identity, metadata); err != nil {
		return "", err
	}
	return core.NewFileIdentity(hex.EncodeToString(identity.Sum(nil))), nil
}

// ScanKeepDir scans `.mj/keep/` (or a session's keep directory) for regular
// files and symlinks, returning each entry with its byte size and file
// identity plus the total size.
func ScanKeepDir(checkout CheckoutKey, keepDir string) ([]core.KeepDirEntry, uint64, error) {
	if !checkpointExists(keepDir) {
		return []core.KeepDirEntry{}, 0, nil
	}
	entries := []core.KeepDirEntry{}
	var totalBytes uint64
	stack := []string{keepDir}
	for len(stack) > 0 {
		dir := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		children, err := os.ReadDir(dir)
		if err != nil {
			return nil, 0, fmt.Errorf("read keep directory %s: %w", dir, err)
		}
		// os.ReadDir sorts by name, as sorting the child paths does.
		for _, child := range children {
			path := gitJoin(dir, child.Name())
			metadata, err := os.Lstat(path)
			if err != nil {
				return nil, 0, fmt.Errorf("stat keep entry %s: %w", path, err)
			}
			if metadata.IsDir() {
				stack = append(stack, path)
				continue
			}
			relative, ok := checkpointStripPrefix(path, keepDir)
			if !ok {
				relative = path
			}
			size := uint64(metadata.Size())
			if totalBytes+size < totalBytes {
				totalBytes = ^uint64(0)
			} else {
				totalBytes += size
			}
			identity, err := FileIdentityOf(metadata)
			if err != nil {
				return nil, 0, err
			}
			entries = append(entries, core.KeepDirEntry{
				Checkout: checkout,
				Path:     relative,
				Bytes:    size,
				Identity: identity,
			})
		}
	}
	slices.SortStableFunc(entries, func(a, b core.KeepDirEntry) int { return checkpointPathCompare(a.Path, b.Path) })
	return entries, totalBytes, nil
}

// CheckKeepDirConsent verifies that every file currently inside `.mj/keep/`
// is covered by consent. Called immediately before capture under the
// barrier so scratch files created after preflight are never silently
// discarded.
func CheckKeepDirConsent(checkout CheckoutKey, keepDir string, consent UntrackedConsent, moveKeep *core.MoveKeep) error {
	entries, _, err := ScanKeepDir(checkout, keepDir)
	if err != nil {
		return err
	}
	if len(entries) == 0 {
		return nil
	}
	var uncovered []UntrackedItem
	for _, entry := range entries {
		if moveKeep != nil && moveKeep.Carries(keepDir, entry.Path) {
			continue
		}
		item := core.UntrackedItem{Kind: core.UntrackedItemKeepDirFile, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity}
		if !consent.Covers(&item) {
			uncovered = append(uncovered, item)
		}
	}
	if len(uncovered) == 0 {
		return nil
	}
	return SnapshotRefusedUntracked(uncovered)
}

type UnrepresentedWorktree struct {
	Locked        bool   `json:"locked"`
	CanMoveInside bool   `json:"can_move_inside"`
	GitDir        string `json:"git_dir"`
	Path          string `json:"path"`
}

func (w *UnrepresentedWorktree) UnmarshalJSON(data []byte) error {
	type plain UnrepresentedWorktree
	return checkpointStrictDecode(data, (*plain)(w), false)
}

// uninitializedOrigin is the configured local origin route of an
// uninitialized submodule.
type uninitializedOrigin struct {
	owner     string
	configKey string
	route     string
}

type OriginDependency struct {
	ModuleURLKey    *string `json:"module_url_key,omitempty"`
	ObjectProvider  *string `json:"object_provider,omitempty"`
	Route           string  `json:"route"`
	CanonicalPath   string  `json:"canonical_path"`
	TargetIsRemoved bool    `json:"target_is_removed"`
}

func (d *OriginDependency) UnmarshalJSON(data []byte) error {
	type plain OriginDependency
	return checkpointStrictDecode(data, (*plain)(d), false)
}

// InventoryBlocker is a refusal with the paths it names and its remedy; the
// refusal's `kind` and `detail` are flattened into the object.
type InventoryBlocker struct {
	Kind   SnapshotRefused
	Paths  []Scoped[string]
	Remedy string
}

func (b InventoryBlocker) MarshalJSON() ([]byte, error) {
	kind, err := json.Marshal(b.Kind)
	if err != nil {
		return nil, err
	}
	rest, err := snapshotMarshal(struct {
		Paths  []Scoped[string] `json:"paths"`
		Remedy string           `json:"remedy"`
	}{snapshotNonNil(b.Paths), b.Remedy})
	if err != nil {
		return nil, err
	}
	return snapshotJoinObjects(kind, rest), nil
}

func (b *InventoryBlocker) UnmarshalJSON(data []byte) error {
	members, err := checkpointObject(data)
	if err != nil {
		return err
	}
	kind, err := DecodeSnapshotRefused(data)
	if err != nil {
		return err
	}
	for _, name := range []string{"paths", "remedy"} {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	var blocker InventoryBlocker
	blocker.Kind = kind
	if err := json.Unmarshal(members["paths"], &blocker.Paths); err != nil {
		return err
	}
	if err := json.Unmarshal(members["remedy"], &blocker.Remedy); err != nil {
		return err
	}
	blocker.Paths = snapshotNonNil(blocker.Paths)
	*b = blocker
	return nil
}

// SnapshotRefused is a capture refusal, adjacently tagged by `kind` (snake
// case) with its `detail`. Every variant is an error whose text is the
// Rust Display.
type SnapshotRefused interface {
	error
	refusedKind() string
	refusedDetail() any
}

type (
	SnapshotRefusedUntracked                      []UntrackedItem
	SnapshotRefusedOperationInProgress            ShownOperation
	SnapshotRefusedDirtySubmodule                 Scoped[string]
	SnapshotRefusedMissingManagedOrigin           CheckoutKey
	SnapshotRefusedOriginRemovedByTeardown        []Scoped[OriginDependency]
	SnapshotRefusedHiddenTrackedEdits             []Scoped[string]
	SnapshotRefusedIntentToAdd                    []Scoped[string]
	SnapshotRefusedUntrackedSubmoduleRegistration []Scoped[string]
)

type SnapshotRefusedUnrepresentedGitState struct {
	Directories []Scoped[string]                `json:"directories"`
	Worktrees   []Scoped[UnrepresentedWorktree] `json:"worktrees"`
}

type SnapshotRefusedTrackedKeepFiles struct {
	KeepDir string           `json:"keep_dir"`
	Paths   []Scoped[string] `json:"paths"`
}

func (SnapshotRefusedUntracked) refusedKind() string           { return "untracked" }
func (SnapshotRefusedOperationInProgress) refusedKind() string { return "operation_in_progress" }
func (SnapshotRefusedDirtySubmodule) refusedKind() string      { return "dirty_submodule" }
func (SnapshotRefusedMissingManagedOrigin) refusedKind() string {
	return "missing_managed_origin"
}
func (SnapshotRefusedOriginRemovedByTeardown) refusedKind() string {
	return "origin_removed_by_teardown"
}
func (SnapshotRefusedUnrepresentedGitState) refusedKind() string { return "unrepresented_git_state" }
func (SnapshotRefusedHiddenTrackedEdits) refusedKind() string    { return "hidden_tracked_edits" }
func (SnapshotRefusedIntentToAdd) refusedKind() string           { return "intent_to_add" }
func (SnapshotRefusedUntrackedSubmoduleRegistration) refusedKind() string {
	return "untracked_submodule_registration"
}
func (SnapshotRefusedTrackedKeepFiles) refusedKind() string { return "tracked_keep_files" }

// SnapshotRefusedDebug is Rust's derived Debug of a refusal (`{:?}`), the
// form suspend preflight reports a blocker's kind in.
func SnapshotRefusedDebug(refused SnapshotRefused) string {
	quote := core.RustDebugString
	checkout := func(key CheckoutKey) string {
		return "CheckoutKey { repository_id: " + quote(key.RepositoryID) + ", checkout_path: " + quote(key.CheckoutPath) + " }"
	}
	scoped := func(key CheckoutKey, value string) string {
		return "Scoped { checkout: " + checkout(key) + ", value: " + value + " }"
	}
	scopedPaths := func(paths []Scoped[string]) string {
		return rustDebugList(paths, func(path Scoped[string]) string { return scoped(path.Checkout, quote(path.Value)) })
	}
	option := func(value *string) string {
		if value == nil {
			return "None"
		}
		return "Some(" + quote(*value) + ")"
	}
	switch r := refused.(type) {
	case SnapshotRefusedUntracked:
		return "Untracked(" + rustDebugList([]UntrackedItem(r), func(item UntrackedItem) string {
			if item.Kind == core.UntrackedItemStash {
				return "Stash { checkout: " + checkout(item.Checkout) + ", commit: " + quote(item.Commit) +
					", message: " + quote(item.Message) + " }"
			}
			return string(item.Kind) + " { checkout: " + checkout(item.Checkout) + ", path: " + quote(item.Path) +
				", identity: FileIdentity(" + quote(item.Identity.AsStr()) + ") }"
		}) + ")"
	case SnapshotRefusedOperationInProgress:
		return "OperationInProgress(GitOperation { checkout: " + checkout(r.Checkout) + ", kind: " + string(r.Kind) + " })"
	case SnapshotRefusedDirtySubmodule:
		return "DirtySubmodule(" + scoped(r.Checkout, quote(r.Value)) + ")"
	case SnapshotRefusedMissingManagedOrigin:
		return "MissingManagedOrigin(" + checkout(CheckoutKey(r)) + ")"
	case SnapshotRefusedOriginRemovedByTeardown:
		return "OriginRemovedByTeardown(" + rustDebugList([]Scoped[OriginDependency](r), func(origin Scoped[OriginDependency]) string {
			value := origin.Value
			return scoped(origin.Checkout, "OriginDependency { module_url_key: "+option(value.ModuleURLKey)+
				", object_provider: "+option(value.ObjectProvider)+", route: "+quote(value.Route)+
				", canonical_path: "+quote(value.CanonicalPath)+", target_is_removed: "+strconv.FormatBool(value.TargetIsRemoved)+" }")
		}) + ")"
	case SnapshotRefusedUnrepresentedGitState:
		return "UnrepresentedGitState { directories: " + scopedPaths(r.Directories) + ", worktrees: " +
			rustDebugList(r.Worktrees, func(worktree Scoped[UnrepresentedWorktree]) string {
				value := worktree.Value
				return scoped(worktree.Checkout, "UnrepresentedWorktree { locked: "+strconv.FormatBool(value.Locked)+
					", can_move_inside: "+strconv.FormatBool(value.CanMoveInside)+", git_dir: "+quote(value.GitDir)+
					", path: "+quote(value.Path)+" }")
			}) + " }"
	case SnapshotRefusedHiddenTrackedEdits:
		return "HiddenTrackedEdits(" + scopedPaths(r) + ")"
	case SnapshotRefusedIntentToAdd:
		return "IntentToAdd(" + scopedPaths(r) + ")"
	case SnapshotRefusedUntrackedSubmoduleRegistration:
		return "UntrackedSubmoduleRegistration(" + scopedPaths(r) + ")"
	case SnapshotRefusedTrackedKeepFiles:
		return "TrackedKeepFiles { keep_dir: " + quote(r.KeepDir) + ", paths: " + scopedPaths(r.Paths) + " }"
	}
	return fmt.Sprintf("%#v", refused)
}

// rustDebugList is Rust's Debug of a Vec: `[a, b]`.
func rustDebugList[T any](items []T, debug func(T) string) string {
	parts := make([]string, len(items))
	for index, item := range items {
		parts[index] = debug(item)
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func (r SnapshotRefusedUntracked) refusedDetail() any {
	return snapshotNonNil([]UntrackedItem(r))
}
func (r SnapshotRefusedOperationInProgress) refusedDetail() any { return ShownOperation(r) }
func (r SnapshotRefusedDirtySubmodule) refusedDetail() any      { return Scoped[string](r) }
func (r SnapshotRefusedMissingManagedOrigin) refusedDetail() any {
	return CheckoutKey(r)
}
func (r SnapshotRefusedOriginRemovedByTeardown) refusedDetail() any {
	return snapshotNonNil([]Scoped[OriginDependency](r))
}
func (r SnapshotRefusedUnrepresentedGitState) refusedDetail() any {
	return SnapshotRefusedUnrepresentedGitState{snapshotNonNil(r.Directories), snapshotNonNil(r.Worktrees)}
}
func (r SnapshotRefusedHiddenTrackedEdits) refusedDetail() any {
	return snapshotNonNil([]Scoped[string](r))
}
func (r SnapshotRefusedIntentToAdd) refusedDetail() any { return snapshotNonNil([]Scoped[string](r)) }
func (r SnapshotRefusedUntrackedSubmoduleRegistration) refusedDetail() any {
	return snapshotNonNil([]Scoped[string](r))
}
func (r SnapshotRefusedTrackedKeepFiles) refusedDetail() any {
	return SnapshotRefusedTrackedKeepFiles{r.KeepDir, snapshotNonNil(r.Paths)}
}

func snapshotRefusedJSON(r SnapshotRefused) ([]byte, error) {
	return snapshotMarshal(struct {
		Kind   string `json:"kind"`
		Detail any    `json:"detail"`
	}{r.refusedKind(), r.refusedDetail()})
}

func (r SnapshotRefusedUntracked) MarshalJSON() ([]byte, error) { return snapshotRefusedJSON(r) }
func (r SnapshotRefusedOperationInProgress) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}
func (r SnapshotRefusedDirtySubmodule) MarshalJSON() ([]byte, error) { return snapshotRefusedJSON(r) }
func (r SnapshotRefusedMissingManagedOrigin) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}
func (r SnapshotRefusedOriginRemovedByTeardown) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}
func (r SnapshotRefusedUnrepresentedGitState) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}
func (r SnapshotRefusedHiddenTrackedEdits) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}
func (r SnapshotRefusedIntentToAdd) MarshalJSON() ([]byte, error) { return snapshotRefusedJSON(r) }
func (r SnapshotRefusedUntrackedSubmoduleRegistration) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}
func (r SnapshotRefusedTrackedKeepFiles) MarshalJSON() ([]byte, error) {
	return snapshotRefusedJSON(r)
}

// DecodeSnapshotRefused decodes the `kind` and `detail` members of a JSON
// object as a SnapshotRefused.
func DecodeSnapshotRefused(data []byte) (SnapshotRefused, error) {
	members, err := checkpointObject(data)
	if err != nil {
		return nil, err
	}
	rawKind, ok := members["kind"]
	if !ok {
		return nil, errors.New("missing field `kind`")
	}
	var kind string
	if err := json.Unmarshal(rawKind, &kind); err != nil {
		return nil, err
	}
	detail, ok := members["detail"]
	if !ok {
		return nil, errors.New("missing field `detail`")
	}
	decodeScopedPaths := func() ([]Scoped[string], error) {
		var paths []Scoped[string]
		err := json.Unmarshal(detail, &paths)
		return snapshotNonNil(paths), err
	}
	switch kind {
	case "untracked":
		var items []UntrackedItem
		err := json.Unmarshal(detail, &items)
		return SnapshotRefusedUntracked(items), err
	case "operation_in_progress":
		var operation ShownOperation
		err := json.Unmarshal(detail, &operation)
		return SnapshotRefusedOperationInProgress(operation), err
	case "dirty_submodule":
		var path Scoped[string]
		err := json.Unmarshal(detail, &path)
		return SnapshotRefusedDirtySubmodule(path), err
	case "missing_managed_origin":
		var checkout CheckoutKey
		err := json.Unmarshal(detail, &checkout)
		return SnapshotRefusedMissingManagedOrigin(checkout), err
	case "origin_removed_by_teardown":
		var origins []Scoped[OriginDependency]
		err := json.Unmarshal(detail, &origins)
		return SnapshotRefusedOriginRemovedByTeardown(snapshotNonNil(origins)), err
	case "unrepresented_git_state":
		var state SnapshotRefusedUnrepresentedGitState
		if err := checkpointStrictDecode(detail, &state, false); err != nil {
			return nil, err
		}
		state.Directories, state.Worktrees = snapshotNonNil(state.Directories), snapshotNonNil(state.Worktrees)
		return state, nil
	case "hidden_tracked_edits":
		paths, err := decodeScopedPaths()
		return SnapshotRefusedHiddenTrackedEdits(paths), err
	case "intent_to_add":
		paths, err := decodeScopedPaths()
		return SnapshotRefusedIntentToAdd(paths), err
	case "untracked_submodule_registration":
		paths, err := decodeScopedPaths()
		return SnapshotRefusedUntrackedSubmoduleRegistration(paths), err
	case "tracked_keep_files":
		var keep SnapshotRefusedTrackedKeepFiles
		if err := checkpointStrictDecode(detail, &keep, false); err != nil {
			return nil, err
		}
		keep.Paths = snapshotNonNil(keep.Paths)
		return keep, nil
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `untracked`, `operation_in_progress`, `dirty_submodule`, `missing_managed_origin`, `origin_removed_by_teardown`, `unrepresented_git_state`, `hidden_tracked_edits`, `intent_to_add`, `untracked_submodule_registration`, `tracked_keep_files`", kind)
}

// snapshotCheckoutDirectory is ` -C <checkout>` for a nested checkout, or
// nothing for a repository root.
func snapshotCheckoutDirectory(checkout CheckoutKey) string {
	if checkout.CheckoutPath == "" {
		return ""
	}
	return " -C " + core.PosixQuote(checkout.CheckoutPath)
}

func (r SnapshotRefusedMissingManagedOrigin) Error() string {
	return fmt.Sprintf("managed checkout %s:%s has no origin remote or source override; restore its origin with `git remote add origin <url>`",
		r.RepositoryID, r.CheckoutPath)
}

func (r SnapshotRefusedOriginRemovedByTeardown) Error() string {
	var text strings.Builder
	for index, origin := range r {
		if index != 0 {
			text.WriteString("; ")
		}
		if origin.Value.ObjectProvider != nil {
			path := core.PosixQuote(origin.Value.CanonicalPath)
			fmt.Fprintf(&text, "local origin %s relies on provider %s inside the managed teardown; make the origin an independent repository outside that set; to dissociate borrowed objects run `git -C %s repack -a -d`, then remove its objects/info/alternates, and suspend again",
				origin.Value.Route, *origin.Value.ObjectProvider, path)
			continue
		}
		directory := snapshotCheckoutDirectory(origin.Checkout)
		fmt.Fprintf(&text, "origin route %s for %s:%s depends on the managed teardown; ", origin.Value.Route, origin.Checkout.RepositoryID, origin.Checkout.CheckoutPath)
		setting := "remote set-url origin"
		if origin.Value.ModuleURLKey != nil {
			setting = "config -f .gitmodules " + core.PosixQuote(*origin.Value.ModuleURLKey)
		}
		if origin.Value.TargetIsRemoved {
			fmt.Fprintf(&text, "move the mirror %s outside the clone and run `git%s %s <new path>`", origin.Value.CanonicalPath, directory, setting)
		} else {
			fmt.Fprintf(&text, "run `git%s %s %s`", directory, setting, core.PosixQuote(origin.Value.CanonicalPath))
		}
		text.WriteString(", then suspend again")
	}
	return text.String()
}

func (r SnapshotRefusedTrackedKeepFiles) Error() string {
	return fmt.Sprintf("%d tracked keep files would enter the snapshot; run git rm -r --cached %s and commit any removal from HEAD before suspending",
		len(r.Paths), r.KeepDir)
}

func (r SnapshotRefusedUntrackedSubmoduleRegistration) Error() string {
	items := make([]string, 0, len(r))
	for _, entry := range r {
		items = append(items, fmt.Sprintf("submodule registration %s:%s:%s is untracked and would not survive discard; run `git%s add %s` to preserve it before suspending",
			entry.Checkout.RepositoryID, entry.Checkout.CheckoutPath, entry.Value, snapshotCheckoutDirectory(entry.Checkout), core.PosixQuote(entry.Value)))
	}
	return strings.Join(items, "; ")
}

func (r SnapshotRefusedIntentToAdd) Error() string {
	items := make([]string, 0, len(r))
	for _, entry := range r {
		directory := snapshotCheckoutDirectory(entry.Checkout)
		path := core.PosixQuote(entry.Value)
		items = append(items, fmt.Sprintf("intent-to-add %s:%s:%s cannot be captured; run `git%s add %s` to stage its contents, or `git%s reset -- %s` to leave it untracked",
			entry.Checkout.RepositoryID, entry.Checkout.CheckoutPath, entry.Value, directory, path, directory, path))
	}
	return strings.Join(items, "; ")
}

func (r SnapshotRefusedHiddenTrackedEdits) Error() string {
	items := make([]string, 0, len(r))
	for _, path := range r {
		items = append(items, fmt.Sprintf("%s:%s:%s", path.Checkout.RepositoryID, path.Checkout.CheckoutPath, path.Value))
	}
	return fmt.Sprintf("%s have skip-worktree or assume-unchanged flags; clear those flags before suspending or moving so all tracked edits can be verified",
		strings.Join(items, ", "))
}

func (r SnapshotRefusedUnrepresentedGitState) Error() string {
	items := make([]string, 0, len(r.Directories)+len(r.Worktrees))
	for _, item := range r.Directories {
		items = append(items, fmt.Sprintf("unsupported Git state at %s; finish or remove it through Git before suspending", item.Value))
	}
	for _, item := range r.Worktrees {
		repo := core.PosixQuote(item.Value.GitDir)
		path := core.PosixQuote(item.Value.Path)
		alternative := ""
		if item.Value.CanMoveInside && !item.Value.Locked {
			alternative = " or move it inside the clone"
		}
		locked := ""
		if item.Value.Locked {
			locked = fmt.Sprintf("; this worktree is locked: once it is available and its contents are safe, deliberately run `git --git-dir %s --work-tree %s worktree unlock %s` first", repo, repo, path)
		}
		items = append(items, fmt.Sprintf("unrepresented worktree %s%s; push or copy out anything wanted, then run `git --git-dir %s --work-tree %s worktree remove --force %s`%s before suspending",
			item.Value.Path, locked, repo, repo, path, alternative))
	}
	return strings.Join(items, "; ")
}

func (r SnapshotRefusedDirtySubmodule) Error() string {
	return fmt.Sprintf("submodule %s:%s:%s has tracked changes or an origin-relative delta; commit and push the work (including wanted refs and stashes)",
		r.Checkout.RepositoryID, r.Checkout.CheckoutPath, r.Value)
}

func (r SnapshotRefusedOperationInProgress) Error() string {
	arguments := abortOperationArguments(r.Kind)
	return fmt.Sprintf("%s in progress at %s:%s; finish it or run `git %s %s` there before suspending",
		string(r.Kind), r.Checkout.RepositoryID, r.Checkout.CheckoutPath, arguments[0], arguments[1])
}

func (r SnapshotRefusedUntracked) Error() string {
	var text strings.Builder
	fmt.Fprintf(&text, "%d untracked items require discard consent:", len(r))
	for _, item := range r {
		switch item.Kind {
		case core.UntrackedItemPath:
			fmt.Fprintf(&text, " %s:%s:%s", item.Checkout.RepositoryID, item.Checkout.CheckoutPath, item.Path)
		case core.UntrackedItemNestedRepository:
			fmt.Fprintf(&text, " %s:%s nested repository %s", item.Checkout.RepositoryID, item.Checkout.CheckoutPath, item.Path)
		case core.UntrackedItemStash:
			fmt.Fprintf(&text, " %s:%s stash %s (%s)", item.Checkout.RepositoryID, item.Checkout.CheckoutPath, item.Commit, item.Message)
		case core.UntrackedItemKeepDirFile:
			fmt.Fprintf(&text, " %s:%s .mj/keep/%s", item.Checkout.RepositoryID, item.Checkout.CheckoutPath, item.Path)
		}
	}
	text.WriteString(". Commit wanted files, move them outside the clone, or remove them before suspending. Preserve wanted stash contents outside the clone before dropping those stashes.")
	return text.String()
}

// NestedInventory is a nested linked worktree's inventory, serialized as a
// `[path, inventory]` pair.
type NestedInventory struct {
	Path      string
	Inventory WorkspaceInventory
}

func (n NestedInventory) MarshalJSON() ([]byte, error) {
	return snapshotMarshal([]any{n.Path, n.Inventory})
}

func (n *NestedInventory) UnmarshalJSON(data []byte) error {
	var pair []json.RawMessage
	if err := json.Unmarshal(data, &pair); err != nil {
		return err
	}
	if len(pair) != 2 {
		return fmt.Errorf("invalid length %d, expected a tuple of size 2", len(pair))
	}
	var nested NestedInventory
	if err := json.Unmarshal(pair[0], &nested.Path); err != nil {
		return err
	}
	if err := json.Unmarshal(pair[1], &nested.Inventory); err != nil {
		return err
	}
	*n = nested
	return nil
}

// WorkspaceInventory is everything about one checkout that consent, refusal
// or capture decides on.
type WorkspaceInventory struct {
	Notices                        []string                        `json:"notices"`
	Checkout                       CheckoutKey                     `json:"checkout"`
	Untracked                      []UntrackedPath                 `json:"untracked"`
	UntrackedStashes               []Scoped[SnapshotStashEntry]    `json:"untracked_stashes"`
	NestedRepositories             []UntrackedPath                 `json:"nested_repositories"`
	TrackedKeepFiles               []Scoped[string]                `json:"tracked_keep_files"`
	KeepDir                        *string                         `json:"keep_dir"`
	DirtySubmodules                []Scoped[string]                `json:"dirty_submodules"`
	UntrackedSubmoduleRegistration []Scoped[string]                `json:"untracked_submodule_registration"`
	UnrepresentedWorktrees         []Scoped[UnrepresentedWorktree] `json:"unrepresented_worktrees"`
	Presence                       []Scoped[string]                `json:"presence"`
	OriginsRemovedByTeardown       []Scoped[OriginDependency]      `json:"origins_removed_by_teardown"`
	// uninitializedOrigins is not serialized (serde skip).
	uninitializedOrigins  []uninitializedOrigin
	Blockers              []InventoryBlocker          `json:"blockers"`
	InPlaceOnly           bool                        `json:"in_place_only"`
	OriginMissing         bool                        `json:"origin_missing"`
	HiddenTrackedEdits    []Scoped[string]            `json:"hidden_tracked_edits"`
	IntentToAdd           []Scoped[string]            `json:"intent_to_add"`
	InitializedSubmodules []Scoped[SnapshotSubmodule] `json:"initialized_submodules"`
	IndexGitlinks         []Scoped[SnapshotGitlink]   `json:"index_gitlinks"`
	Operation             *GitOperation               `json:"operation"`
	Raw                   bool                        `json:"raw"`
	RemoteRefsStale       bool                        `json:"remote_refs_stale"`
	Worktrees             []NestedInventory           `json:"worktrees"`
}

func (w WorkspaceInventory) MarshalJSON() ([]byte, error) {
	type plain WorkspaceInventory
	p := plain(w)
	p.Notices = snapshotNonNil(p.Notices)
	p.Untracked = snapshotNonNil(p.Untracked)
	p.UntrackedStashes = snapshotNonNil(p.UntrackedStashes)
	p.NestedRepositories = snapshotNonNil(p.NestedRepositories)
	p.TrackedKeepFiles = snapshotNonNil(p.TrackedKeepFiles)
	p.DirtySubmodules = snapshotNonNil(p.DirtySubmodules)
	p.UntrackedSubmoduleRegistration = snapshotNonNil(p.UntrackedSubmoduleRegistration)
	p.UnrepresentedWorktrees = snapshotNonNil(p.UnrepresentedWorktrees)
	p.Presence = snapshotNonNil(p.Presence)
	p.OriginsRemovedByTeardown = snapshotNonNil(p.OriginsRemovedByTeardown)
	p.Blockers = snapshotNonNil(p.Blockers)
	p.HiddenTrackedEdits = snapshotNonNil(p.HiddenTrackedEdits)
	p.IntentToAdd = snapshotNonNil(p.IntentToAdd)
	p.InitializedSubmodules = snapshotNonNil(p.InitializedSubmodules)
	p.IndexGitlinks = snapshotNonNil(p.IndexGitlinks)
	p.Worktrees = snapshotNonNil(p.Worktrees)
	return snapshotMarshal(p)
}

func (w *WorkspaceInventory) UnmarshalJSON(data []byte) error {
	type plain WorkspaceInventory
	return checkpointStrictDecode(data, (*plain)(w), false)
}

// captureGit disables source maintenance, including lazy promisor fetch
// children.
type captureGit struct {
	inner GitCommandRunner
}

func newCaptureGit(inner GitCommandRunner) captureGit { return captureGit{inner: inner} }

func (g captureGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	prepared, err := g.prepareCommand(repository, command)
	if err != nil {
		return GitOutput{}, err
	}
	return g.inner.Run(repository, &prepared)
}

func (g captureGit) RunToFile(repository string, command *GitCommand, output *os.File) (GitOutput, error) {
	prepared, err := g.prepareCommand(repository, command)
	if err != nil {
		return GitOutput{}, err
	}
	return runToFile(g.inner, repository, &prepared, output)
}

func (g captureGit) prepareCommand(repository string, command *GitCommand) (GitCommand, error) {
	prepared := GitCommand{
		Arguments: append([]string{"-c", "gc.auto=0", "-c", "maintenance.auto=false"}, command.Arguments...),
		Stdin:     command.Stdin,
		Env:       slices.Clone(command.Env),
	}
	if err := honorRepositorySSH(g.inner, repository, &prepared); err != nil {
		return GitCommand{}, err
	}
	// Snapshot packs must hydrate promised objects they carry. The -c
	// maintenance settings above also reach Git's lazy-fetch children.
	prepared.Env = append(prepared.Env, [2]string{"GIT_NO_LAZY_FETCH", "0"})
	// Inspection must not refresh or lock a user's real index.
	prepared.Env = append(prepared.Env, [2]string{"GIT_OPTIONAL_LOCKS", "0"})
	return prepared, nil
}

// honorRepositorySSH preserves a user-selected identity or proxy before the
// runner supplies its default SSH command. The explicit process environment
// still wins.
func honorRepositorySSH(runner GitCommandRunner, repository string, command *GitCommand) error {
	if _, set := os.LookupEnv("GIT_SSH_COMMAND"); set {
		return nil
	}
	if slices.ContainsFunc(command.Env, func(pair [2]string) bool { return pair[0] == "GIT_SSH_COMMAND" }) {
		return nil
	}
	configured, err := runner.Run(repository, &GitCommand{
		Arguments: []string{"config", "--get", "core.sshCommand"},
		Env:       slices.Clone(command.Env),
	})
	if err != nil {
		return err
	}
	if configured.Status != 0 && configured.Status != 1 {
		return gitFailure("read source SSH command", &configured)
	}
	if configured.Status == 0 {
		value, err := core.FromGitBytes(bytes.TrimSuffix(configured.Stdout, []byte("\n")))
		if err != nil {
			return err
		}
		command.Env = append(command.Env, [2]string{"GIT_SSH_COMMAND", value})
	}
	return nil
}

func gitPath(runner GitCommandRunner, checkout string, name string) (string, error) {
	output, err := gitBytes(runner, checkout, []string{"rev-parse", "--git-path", name}, nil, "resolve Git path")
	if err != nil {
		return "", err
	}
	// Strip only Git's terminating newline: spaces in paths are significant.
	return absoluteGitPath(checkout, output)
}

func launchPath(runner GitCommandRunner, checkout string, sessionID string) (string, error) {
	valid := sessionID != ""
	for _, character := range sessionID {
		if !(character == '-' || character < 0x80 && (character >= '0' && character <= '9' || character >= 'a' && character <= 'z' || character >= 'A' && character <= 'Z')) {
			valid = false
		}
	}
	if !valid {
		return "", errors.New("invalid session id")
	}
	_, common, err := gitDirectories(runner, checkout)
	if err != nil {
		return "", err
	}
	return gitJoin(gitJoin(gitJoin(common, "mj/sessions"), sessionID), "launch"), nil
}

// ReadLaunchRecord is Rust's `LaunchRecord::read`: the session's launch
// record, or nil when none was written.
func ReadLaunchRecord(runner GitCommandRunner, checkout string, sessionID string) (*LaunchRecord, error) {
	path, err := launchPath(runner, checkout, sessionID)
	if err != nil {
		return nil, err
	}
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	var record LaunchRecord
	if err := json.Unmarshal(data, &record); err != nil {
		return nil, fmt.Errorf("decode launch record: %w", err)
	}
	if record.Version != 1 {
		return nil, fmt.Errorf("unsupported launch record version %d", record.Version)
	}
	return &record, nil
}

func buildLaunchRecord(runner GitCommandRunner, topLevel string) (LaunchRecord, error) {
	refs, err := readRefs(runner, topLevel)
	if err != nil {
		return LaunchRecord{}, err
	}
	stash, err := readStash(runner, topLevel)
	if err != nil {
		return LaunchRecord{}, err
	}
	trees, err := discoverLinkedWorktrees(runner, topLevel)
	if err != nil {
		return LaunchRecord{}, err
	}
	worktrees := make([]string, 0, len(trees))
	for _, tree := range trees {
		worktrees = append(worktrees, tree.relative)
	}
	return LaunchRecord{Version: 1, Refs: refs, Stash: stash, Worktrees: worktrees}, nil
}

// writeLaunchRecordTemp writes record to a synced temporary file in parent
// and returns its path.
func writeLaunchRecordTemp(parent string, record LaunchRecord) (string, error) {
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return "", err
	}
	data, err := snapshotMarshal(record)
	if err != nil {
		return "", err
	}
	data = append(data, '\n')
	file, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return "", err
	}
	if _, err := file.Write(data); err != nil {
		file.Close()
		os.Remove(file.Name())
		return "", err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		os.Remove(file.Name())
		return "", err
	}
	if err := file.Close(); err != nil {
		os.Remove(file.Name())
		return "", err
	}
	return file.Name(), nil
}

// RecordLaunch writes the checkout's launch record for sessionID, replacing
// any earlier one.
func RecordLaunch(runner GitCommandRunner, checkout string, sessionID string) error {
	location, err := locateCheckout(runner, checkout)
	if err != nil {
		return err
	}
	topLevel := location.topLevel
	record, err := buildLaunchRecord(runner, topLevel)
	if err != nil {
		return err
	}
	path, err := launchPath(runner, topLevel, sessionID)
	if err != nil {
		return err
	}
	parent, ok := checkpointParent(path)
	if !ok {
		return errors.New("launch path has no parent")
	}
	temporary, err := writeLaunchRecordTemp(parent, record)
	if err != nil {
		return err
	}
	if err := os.Rename(temporary, path); err != nil {
		os.Remove(temporary)
		return fmt.Errorf("write %s: %w", path, err)
	}
	return nil
}

// RecordLaunchIfAbsent writes the checkout's launch record for sessionID
// unless one already exists.
func RecordLaunchIfAbsent(runner GitCommandRunner, checkout string, sessionID string) error {
	location, err := locateCheckout(runner, checkout)
	if err != nil {
		return err
	}
	topLevel := location.topLevel
	path, err := launchPath(runner, topLevel, sessionID)
	if err != nil {
		return err
	}
	if checkpointExists(path) {
		return nil
	}
	record, err := buildLaunchRecord(runner, topLevel)
	if err != nil {
		return err
	}
	parent, ok := checkpointParent(path)
	if !ok {
		return errors.New("launch path has no parent")
	}
	temporary, err := writeLaunchRecordTemp(parent, record)
	if err != nil {
		return err
	}
	defer os.Remove(temporary)
	// Persist without replacing a record a concurrent launch wrote first.
	if err := os.Link(temporary, path); err != nil {
		if errors.Is(err, fs.ErrExist) {
			return nil
		}
		return fmt.Errorf("write %s: %w", path, err)
	}
	return nil
}

// RemoveLaunchRecord deletes the session's launch record and, when empty,
// its directory.
func RemoveLaunchRecord(runner GitCommandRunner, checkout string, sessionID string) error {
	path, err := launchPath(runner, checkout, sessionID)
	if err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove %s: %w", path, err)
	}
	if parent, ok := checkpointParent(path); ok {
		// A missing or nonempty directory, or any other failure, is left.
		_ = os.Remove(parent)
	}
	return nil
}

func localSnapshotRef(name string) bool {
	return strings.HasPrefix(name, "refs/heads/") ||
		strings.HasPrefix(name, "refs/tags/") ||
		strings.HasPrefix(name, "refs/notes/") ||
		(strings.HasPrefix(name, "refs/mj/") &&
			!strings.HasPrefix(name, "refs/mj/snapshot/") &&
			!strings.HasPrefix(name, "refs/mj/restore/") &&
			!core.IsReviewBaselinePin(name))
}

func readAllRefs(runner GitCommandRunner, checkout string) (map[string]string, error) {
	text, err := gitText(runner, checkout, []string{
		"for-each-ref",
		"--format=%(refname)%00%(objectname)%00%(symref)",
		"refs/heads",
		"refs/tags",
		"refs/notes",
		"refs/mj/",
		"refs/remotes/",
	})
	if err != nil {
		return nil, err
	}
	refs := map[string]string{}
	for _, line := range gitLines(text) {
		fields := strings.Split(line, "\x00")
		if len(fields) < 1 {
			return nil, errors.New("missing Git ref name")
		}
		if len(fields) < 2 {
			return nil, errors.New("missing Git ref object")
		}
		if len(fields) < 3 {
			return nil, errors.New("missing Git symbolic ref field")
		}
		name, oid, symbolic := fields[0], fields[1], fields[2]
		// Keep local symbolic aliases as ordinary refs at their resolved OID.
		if localSnapshotRef(name) || (symbolic == "" && strings.HasPrefix(name, "refs/remotes/")) {
			refs[name] = oid
		}
	}
	return refs, nil
}

func readRefs(runner GitCommandRunner, checkout string) (map[string]string, error) {
	refs, err := readAllRefs(runner, checkout)
	if err != nil {
		return nil, err
	}
	maps.DeleteFunc(refs, func(name, _ string) bool { return !localSnapshotRef(name) })
	return refs, nil
}

func selectedStash(runner GitCommandRunner, checkout string, ownership CheckoutOwnership) ([]SnapshotStashEntry, error) {
	if !ownership.Raw {
		owns, err := ownsGitDirectory(runner, checkout, ownership)
		if err != nil {
			return nil, err
		}
		if !owns {
			return []SnapshotStashEntry{}, nil
		}
	}
	stash, err := readStash(runner, checkout)
	if err != nil {
		return nil, err
	}
	if !ownership.Raw || ownership.Launch == nil {
		return stash, nil
	}
	old := map[SnapshotStashEntry]int{}
	for _, entry := range ownership.Launch.Stash {
		old[entry]++
	}
	// Oldest-first matching leaves newer repeated entries in the snapshot.
	added := []SnapshotStashEntry{}
	for index := len(stash) - 1; index >= 0; index-- {
		entry := stash[index]
		if old[entry] > 0 {
			old[entry]--
		} else {
			added = append(added, entry)
		}
	}
	slices.Reverse(added)
	return added, nil
}

func stashHasUntracked(runner GitCommandRunner, checkout string, oid string) (bool, error) {
	parents, err := gitText(runner, checkout, []string{"rev-list", "--parents", "-n", "1", oid})
	if err != nil {
		return false, err
	}
	return len(strings.Fields(parents)) > 3, nil
}

// trackedStash rebuilds a stash whose third parent contains untracked files
// from only its tracked tree and index parents, using the shared internal
// identity and original message.
func trackedStash(runner GitCommandRunner, checkout string, entry SnapshotStashEntry) (SnapshotStashEntry, error) {
	untracked, err := stashHasUntracked(runner, checkout, entry.Commit)
	if err != nil {
		return SnapshotStashEntry{}, err
	}
	if !untracked {
		return entry, nil
	}
	raw, err := gitBytes(runner, checkout, []string{"cat-file", "commit", entry.Commit}, nil, "read stash message")
	if err != nil {
		return SnapshotStashEntry{}, err
	}
	separator := bytes.Index(raw, []byte("\n\n"))
	if separator < 0 {
		return SnapshotStashEntry{}, errors.New("stash commit has no message separator")
	}
	message := slices.Clone(raw[separator+2:])
	if !bytes.HasSuffix(message, []byte("\n")) {
		message = append(message, '\n')
	}
	// Distinct -u stashes can have identical tracked parents and messages.
	// Keep their identities distinct without retaining the discarded parent.
	message = append(message, fmt.Sprintf("\nMjolnir-Source-Stash: %s\n", entry.Commit)...)
	output, err := runner.Run(checkout, &GitCommand{
		Arguments: []string{
			"commit-tree",
			entry.Commit + "^{tree}",
			"-p",
			entry.Commit + "^1",
			"-p",
			entry.Commit + "^2",
		},
		Stdin: message,
		Env:   syntheticGitCommitIdentity(),
	})
	if err != nil {
		return SnapshotStashEntry{}, err
	}
	if output.Status != 0 {
		return SnapshotStashEntry{}, gitFailure("strip stash untracked parent", &output)
	}
	commit, err := gitDecodeUTF8(output.Stdout)
	if err != nil {
		return SnapshotStashEntry{}, err
	}
	return SnapshotStashEntry{Commit: strings.TrimSpace(commit), Message: entry.Message}, nil
}

func readStash(runner GitCommandRunner, checkout string) ([]SnapshotStashEntry, error) {
	named, err := namedStash(runner, checkout)
	if err != nil {
		return nil, err
	}
	entries := make([]SnapshotStashEntry, 0, len(named))
	for _, entry := range named {
		entries = append(entries, entry.entry)
	}
	return entries, nil
}

type namedStashEntry struct {
	name  string
	entry SnapshotStashEntry
}

func namedStash(runner GitCommandRunner, checkout string) ([]namedStashEntry, error) {
	raw, err := gitBytes(runner, checkout, []string{"stash", "list", "--format=%gd%x00%H%x00%gs"}, nil, "read stash list")
	if err != nil {
		return nil, err
	}
	text, err := gitDecodeUTF8(raw)
	if err != nil {
		return nil, err
	}
	entries := []namedStashEntry{}
	for _, line := range gitLines(text) {
		name, entry, ok := strings.Cut(line, "\x00")
		if !ok {
			return nil, errors.New("missing stash selector")
		}
		commit, message, ok := strings.Cut(entry, "\x00")
		if !ok {
			return nil, errors.New("malformed stash entry")
		}
		entries = append(entries, namedStashEntry{name, SnapshotStashEntry{Commit: commit, Message: message}})
	}
	current, err := runGit(runner, checkout, []string{"rev-parse", "--verify", "--quiet", "refs/stash"}, nil)
	if err != nil {
		return nil, err
	}
	if current.Status != 0 && current.Status != 1 {
		return nil, gitFailure("read current stash ref", &current)
	}
	if current.Status == 0 {
		currentText, err := gitDecodeUTF8(current.Stdout)
		if err != nil {
			return nil, err
		}
		commit := strings.TrimSpace(currentText)
		if len(entries) == 0 || entries[0].entry.Commit != commit {
			message, err := gitText(runner, checkout, []string{"show", "-s", "--format=%s", commit})
			if err != nil {
				return nil, err
			}
			entries = slices.Insert(entries, 0, namedStashEntry{"refs/stash", SnapshotStashEntry{Commit: commit, Message: message}})
		}
	}
	return entries, nil
}

func readHead(runner GitCommandRunner, checkout string) (SnapshotHead, error) {
	symbolic, err := runGit(runner, checkout, []string{"symbolic-ref", "-q", "HEAD"}, nil)
	if err != nil {
		return SnapshotHead{}, err
	}
	switch symbolic.Status {
	case 0:
		name, err := gitDecodeUTF8(symbolic.Stdout)
		if err != nil {
			return SnapshotHead{}, err
		}
		name = strings.TrimSuffix(name, "\n")
		commit, err := gitText(runner, checkout, []string{"rev-parse", "HEAD"})
		if err != nil {
			return SnapshotHead{}, err
		}
		return SnapshotHead{Kind: core.SnapshotHeadBranch, Name: name, Commit: commit}, nil
	case 1:
		commit, err := gitText(runner, checkout, []string{"rev-parse", "HEAD"})
		if err != nil {
			return SnapshotHead{}, err
		}
		return SnapshotHead{Kind: core.SnapshotHeadDetached, Commit: commit}, nil
	}
	return SnapshotHead{}, gitFailure("read symbolic HEAD", &symbolic)
}

// operation reports the Git operation in progress in checkout, if any.
func operation(runner GitCommandRunner, checkout string) (GitOperationKind, bool, error) {
	for _, candidate := range []struct {
		name string
		kind GitOperationKind
	}{
		{"rebase-merge", core.GitOperationKindRebase},
		{"rebase-apply", core.GitOperationKindRebase},
		{"MERGE_HEAD", core.GitOperationKindMerge},
		{"CHERRY_PICK_HEAD", core.GitOperationKindCherryPick},
		{"REVERT_HEAD", core.GitOperationKindRevert},
		{"sequencer", core.GitOperationKindCherryPick},
		{"BISECT_LOG", core.GitOperationKindBisect},
		{"BISECT_START", core.GitOperationKindBisect},
	} {
		path, err := gitPath(runner, checkout, candidate.name)
		if err != nil {
			return "", false, err
		}
		exists, err := checkpointTryExists(path)
		if err != nil {
			return "", false, err
		}
		if !exists {
			continue
		}
		if candidate.name == "rebase-apply" {
			applying, err := checkpointTryExists(gitJoin(path, "applying"))
			if err != nil {
				return "", false, err
			}
			if applying {
				return core.GitOperationKindAm, true, nil
			}
		}
		if candidate.name == "sequencer" {
			todo, err := os.ReadFile(gitJoin(path, "todo"))
			if err != nil {
				return "", false, err
			}
			todoText, err := gitDecodeUTF8(todo)
			if err != nil {
				return "", false, err
			}
			for _, line := range gitLines(todoText) {
				if strings.HasPrefix(line, "revert ") {
					return core.GitOperationKindRevert, true, nil
				}
			}
			return candidate.kind, true, nil
		}
		return candidate.kind, true, nil
	}
	return "", false, nil
}

func hashFileMetadata(identity io.Writer, metadata fs.FileInfo) error {
	if err := hashFileIdentity(identity, metadata); err != nil {
		return err
	}
	// Shown discard consent includes the displayed length, even when a
	// writer preserves the inode and restores its previous modification
	// timestamp.
	identity.Write(snapshotBigEndian64(uint64(metadata.Size())))
	epoch := time.Unix(0, 0)
	modified := metadata.ModTime()
	if !modified.Before(epoch) {
		identity.Write([]byte{0})
		identity.Write(snapshotNanos128(modified.Unix(), int64(modified.Nanosecond())))
	} else {
		identity.Write([]byte{1})
		// The duration before the epoch, as SystemTimeError::duration has it.
		seconds := -modified.Unix()
		nanos := -int64(modified.Nanosecond())
		if nanos < 0 {
			seconds--
			nanos += 1_000_000_000
		}
		identity.Write(snapshotNanos128(seconds, nanos))
	}
	return nil
}

func snapshotBigEndian64(value uint64) []byte {
	encoded := make([]byte, 8)
	for index := 7; index >= 0; index-- {
		encoded[index] = byte(value)
		value >>= 8
	}
	return encoded
}

// snapshotNanos128 is Duration::as_nanos().to_be_bytes(): seconds and
// nanoseconds as a big-endian 128-bit nanosecond count.
func snapshotNanos128(seconds int64, nanos int64) []byte {
	total := new(big.Int).Mul(big.NewInt(seconds), big.NewInt(1_000_000_000))
	total.Add(total, big.NewInt(nanos))
	encoded := make([]byte, 16)
	total.FillBytes(encoded)
	return encoded
}

func operationRecord(runner GitCommandRunner, checkout string, key CheckoutKey) (*GitOperation, error) {
	kind, ok, err := operation(runner, checkout)
	if err != nil || !ok {
		return nil, err
	}
	return &GitOperation{Checkout: key, Kind: kind}, nil
}

func abortOperationArguments(kind GitOperationKind) [2]string {
	switch kind {
	case core.GitOperationKindMerge:
		return [2]string{"merge", "--abort"}
	case core.GitOperationKindRebase:
		return [2]string{"rebase", "--abort"}
	case core.GitOperationKindCherryPick:
		return [2]string{"cherry-pick", "--abort"}
	case core.GitOperationKindRevert:
		return [2]string{"revert", "--abort"}
	case core.GitOperationKindAm:
		return [2]string{"am", "--abort"}
	}
	return [2]string{"bisect", "reset"}
}

// prepareOperation aborts a managed checkout's operation. Managed callers
// must stop every writer and hold exclusive checkout ownership from
// inventory/consent through capture completion. External Git writers are
// outside this contract; raw checkouts are never aborted.
func prepareOperation(runner GitCommandRunner, checkout string, ownership CheckoutOwnership, kind *GitOperationKind) (bool, error) {
	if kind != nil {
		if ownership.Raw {
			return true, nil
		}
		arguments := abortOperationArguments(*kind)
		if _, err := gitText(runner, checkout, arguments[:]); err != nil {
			return false, err
		}
	}
	return false, nil
}

func submoduleUncommittedState(runner GitCommandRunner, checkout string, inventory *WorkspaceInventory) (bool, error) {
	if len(inventory.DirtySubmodules) > 0 {
		return true, nil
	}
	for _, module := range inventory.InitializedSubmodules {
		directory := gitJoin(checkout, module.Value.Path)
		var pin *string
		if index := slices.IndexFunc(inventory.IndexGitlinks, func(link Scoped[SnapshotGitlink]) bool {
			return checkpointPathEqual(link.Value.Path, module.Value.Path)
		}); index >= 0 {
			pin = &inventory.IndexGitlinks[index].Value.Commit
		} else {
			parent, ok := checkpointParent(directory)
			if !ok {
				return false, errors.New("module has no parent")
			}
			leaf, ok := checkpointFileName(directory)
			if !ok {
				return false, errors.New("module has no name")
			}
			links, err := snapshotIndexGitlinks(runner, parent)
			if err != nil {
				return false, err
			}
			for _, link := range links {
				if checkpointPathEqual(link.Path, leaf) {
					pin = &link.Commit
					break
				}
			}
		}
		if pin == nil || *pin != module.Value.Commit {
			return true, nil
		}
		staged, err := stagedGitlinkChanges(runner, directory)
		if err != nil {
			return false, err
		}
		if staged {
			return true, nil
		}
	}
	return false, nil
}

func stagedGitlinkChanges(runner GitCommandRunner, checkout string) (bool, error) {
	raw, err := gitBytes(runner, checkout, []string{
		"diff", "--cached", "--ignore-submodules=none", "--raw", "--no-renames", "--no-ext-diff", "-z",
	}, nil, "inspect staged gitlinks")
	if err != nil {
		return false, err
	}
	fields := snapshotNonEmptyFields(raw, 0)
	if len(fields)%2 != 0 {
		return false, errors.New("malformed staged Git diff")
	}
	for index := 0; index < len(fields); index += 2 {
		header := fields[index]
		parts := bytes.Split(header, []byte(" "))
		if bytes.HasPrefix(header, []byte(":160000 ")) || (len(parts) > 1 && string(parts[1]) == "160000") {
			return true, nil
		}
	}
	return false, nil
}

func intentToAddPaths(runner GitCommandRunner, checkout string) ([]string, error) {
	changes := func(visibility string) (map[string]string, error) {
		raw, err := gitBytes(runner, checkout, []string{
			"diff", "--cached", "--raw", "--no-abbrev", "--no-renames", "--no-ext-diff",
			"--ignore-submodules=all", visibility, "-z",
		}, nil, "inspect intent-to-add index entries")
		if err != nil {
			return nil, err
		}
		fields := snapshotNonEmptyFields(raw, 0)
		if len(fields)%2 != 0 {
			return nil, errors.New("malformed intent-to-add index diff")
		}
		entries := map[string]string{}
		for index := 0; index < len(fields); index += 2 {
			entries[string(fields[index+1])] = string(fields[index])
		}
		return entries, nil
	}
	invisible, err := changes("--ita-invisible-in-index")
	if err != nil {
		return nil, err
	}
	visible, err := changes("--ita-visible-in-index")
	if err != nil {
		return nil, err
	}
	// Compare complete entry identities in both directions. For a HEAD path,
	// ITA can be a modification when visible and a deletion when invisible;
	// an empty HEAD blob may disappear from the visible diff altogether.
	differing := map[string]bool{}
	for _, entries := range []map[string]string{invisible, visible} {
		for path := range entries {
			left, inLeft := invisible[path]
			right, inRight := visible[path]
			if inLeft != inRight || left != right {
				differing[path] = true
			}
		}
	}
	paths := []string{}
	for _, raw := range slices.Sorted(maps.Keys(differing)) {
		path, err := core.FromGitBytes([]byte(raw))
		if err != nil {
			return nil, err
		}
		paths = append(paths, path)
	}
	return paths, nil
}

// snapshotTempFile is a temporary file a guard removes.
type snapshotTempFile struct {
	path string
}

func (t snapshotTempFile) remove() { os.Remove(t.path) }

// privateIndex is a private index beside the real one, which preserves
// split-index lookup while letting snapshot inspection remove gitlinks or
// create stashes safely. The caller removes it.
func privateIndex(runner GitCommandRunner, checkout string) (snapshotTempFile, error) {
	index, err := gitPath(runner, checkout, "index")
	if err != nil {
		return snapshotTempFile{}, err
	}
	parent, ok := checkpointParent(index)
	if !ok {
		return snapshotTempFile{}, errors.New("index has no parent")
	}
	file, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return snapshotTempFile{}, err
	}
	temporary := snapshotTempFile{path: file.Name()}
	file.Close()
	exists, err := checkpointTryExists(index)
	if err != nil {
		temporary.remove()
		return snapshotTempFile{}, err
	}
	if exists {
		if err := snapshotCopyFile(index, temporary.path); err != nil {
			temporary.remove()
			return snapshotTempFile{}, err
		}
	} else if err := os.Remove(temporary.path); err != nil {
		return snapshotTempFile{}, err
	}
	return temporary, nil
}

func workingState(runner GitCommandRunner, checkout string, skipped bool) (*string, error) {
	if skipped {
		return nil, nil
	}
	// stash create writes its index even with optional locks disabled.
	temporary, err := privateIndex(runner, checkout)
	if err != nil {
		return nil, err
	}
	defer temporary.remove()
	env := append(syntheticGitCommitIdentity(), [2]string{"GIT_INDEX_FILE", temporary.path})
	// Editing files needs no user identity; synthetic stash commits use ours.
	output, err := runner.Run(checkout, &GitCommand{Arguments: []string{"stash", "create"}, Env: env})
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, gitFailure("capture tracked working state", &output)
	}
	text, err := gitDecodeUTF8(output.Stdout)
	if err != nil {
		return nil, err
	}
	state := strings.TrimSpace(text)
	if state == "" {
		return nil, nil
	}
	return &state, nil
}

func relativeLocalRemote(remote string) bool {
	// A colon before the first slash denotes Git's scp or transport syntax.
	// Native drive letters are handled first by Path::is_absolute.
	if snapshotIsAbsolute(remote) {
		return false
	}
	colon := strings.IndexByte(remote, ':')
	if colon < 0 {
		return true
	}
	slash := strings.IndexByte(remote, '/')
	return !(slash < 0 || colon < slash)
}

// transportURL splits Git's explicit helper syntax, which wraps an
// underlying URL (https::https://...), into the helper prefix and address.
func transportURL(value string) (string, string) {
	address := value
	for {
		helper, inner, ok := strings.Cut(address, "::")
		if !ok || helper == "" || !snapshotHelperName(helper) {
			break
		}
		address = inner
	}
	return value[:len(value)-len(address)], address
}

func snapshotHelperName(helper string) bool {
	for index := 0; index < len(helper); index++ {
		character := helper[index]
		if !(character >= '0' && character <= '9' || character >= 'a' && character <= 'z' ||
			character >= 'A' && character <= 'Z' || strings.IndexByte("+.-_", character) >= 0) {
			return false
		}
	}
	return true
}

// redactSnapshotURL strips credentials even from incomplete rewrite
// prefixes, which URL parsers reject (for example https://user:token@
// without a host).
func redactSnapshotURL(value string) (string, error) {
	transport, address := transportURL(value)
	if transport != "" {
		redacted, err := redactSnapshotURL(address)
		if err != nil {
			return "", err
		}
		return transport + redacted, nil
	}
	if url, ok := core.ParseURL(value); ok {
		_, hasPassword := url.Password()
		if url.Scheme() == "http" || url.Scheme() == "https" {
			if url.Username() == "" && !hasPassword {
				return value, nil
			}
			redacted, ok := url.WithoutCredentials()
			if !ok {
				return "", errors.New("cannot redact Git URL username")
			}
			return redacted.String(), nil
		}
		if hasPassword {
			redacted, ok := url.WithoutPassword()
			if !ok {
				return "", errors.New("cannot redact Git URL password")
			}
			return redacted.String(), nil
		}
	}
	if scheme := strings.Index(value, "://"); scheme >= 0 {
		authority := scheme + 3
		end := len(value)
		if offset := strings.IndexAny(value[authority:], "/?#"); offset >= 0 {
			end = authority + offset
		}
		if at := strings.LastIndexByte(value[authority:end], '@'); at >= 0 {
			userinfo := value[authority : authority+at]
			if strings.EqualFold(value[:scheme], "http") || strings.EqualFold(value[:scheme], "https") {
				return value[:authority] + value[authority+at+1:], nil
			}
			if username, _, ok := strings.Cut(userinfo, ":"); ok {
				return value[:authority] + username + "@" + value[authority+at+1:], nil
			}
		}
	} else if !snapshotIsAbsolute(value) {
		if at := strings.IndexByte(value, '@'); at >= 0 && strings.Contains(value[at+1:], ":") {
			if username, _, ok := strings.Cut(value[:at], ":"); ok {
				return username + "@" + value[at+1:], nil
			}
		}
	}
	return value, nil
}

func resolveRemotePath(checkout string, remote string) (string, error) {
	if !snapshotIsAbsolute(remote) && !relativeLocalRemote(remote) {
		return remote, nil
	}
	absolute, err := snapshotAbsolute(gitJoin(checkout, remote))
	if err != nil {
		return "", err
	}
	canonical, err := archiveCanonicalize(absolute)
	switch {
	case err == nil:
		absolute = canonical
	case errors.Is(err, fs.ErrNotExist):
	default:
		return "", fmt.Errorf("resolve Git remote path: %w", err)
	}
	if !snapshotValidUTF8(absolute) {
		return "", errors.New("Git remote path is not UTF-8")
	}
	return absolute, nil
}

func absoluteConfigURL(checkout string, value string) (string, error) {
	if value == "" || !relativeLocalRemote(value) {
		return value, nil
	}
	// Preserve URL text for prefix rewrites; filesystem canonicalization can
	// move a symlinked destination outside its configured pushInsteadOf
	// prefix.
	anchor := checkout
	if !snapshotIsAbsolute(checkout) {
		absolute, err := snapshotAbsolute(checkout)
		if err != nil {
			return "", err
		}
		anchor = absolute
	}
	stored := gitJoin(anchor, value)
	if !snapshotValidUTF8(stored) {
		return "", errors.New("Git config path is not UTF-8")
	}
	if strings.HasSuffix(value, "/") && !strings.HasSuffix(stored, "/") {
		stored += "/"
	}
	return stored, nil
}

// resolveSubmoduleURL resolves .gitmodules URLs against the only remote
// restore recreates: origin.
func resolveSubmoduleURL(runner GitCommandRunner, checkout string, url string, originOverride *string) (string, error) {
	if !strings.HasPrefix(url, "./") && !strings.HasPrefix(url, "../") {
		return absoluteConfigURL(checkout, url)
	}
	base, err := configValues(runner, checkout, "remote.origin.url")
	if err != nil {
		return "", err
	}
	switch {
	case originOverride != nil:
		return relativeSubmoduleURL(*originOverride, url, checkout)
	case len(base) > 0:
		return relativeSubmoduleURL(base[len(base)-1], url, checkout)
	}
	return resolveRemotePath(checkout, url)
}

// relativeSubmoduleURL resolves ../sub.git as Git does, treating the
// superproject URL as a directory.
func relativeSubmoduleURL(base string, relative string, checkout string) (string, error) {
	isURL := strings.Contains(base, "://")
	scpColon := -1
	if !isURL && !snapshotIsAbsolute(base) {
		scpColon = strings.IndexByte(base, ':')
	}
	if !isURL && scpColon < 0 {
		absolute, err := absoluteConfigURL(checkout, base)
		if err != nil {
			return "", err
		}
		base = absolute
		if runtime.GOOS == "windows" {
			base = strings.ReplaceAll(base, `\`, "/")
		}
	}
	minimum := 0
	if scheme, address, ok := strings.Cut(base, "://"); ok {
		slash := strings.IndexByte(address, '/')
		if slash < 0 {
			slash = len(address)
		}
		minimum = len(scheme) + 3 + slash
	} else if scpColon >= 0 {
		minimum = scpColon + 1
	}
	base = base[:len(strings.TrimRight(base, "/"))]
	suffix := relative
	// Git consumes only the leading ./ and ../ components. Preserve the
	// remaining suffix literally: an internal x/.. may cross a symlink.
	for {
		if rest, ok := strings.CutPrefix(suffix, "./"); ok {
			suffix = rest
		} else if rest, ok := strings.CutPrefix(suffix, "../"); ok {
			split := strings.LastIndexByte(base, '/')
			if split < minimum {
				split = -1
			}
			if split < 0 && scpColon >= 0 && len(base) > minimum {
				split = scpColon + 1
			}
			if split < 0 {
				return "", errors.New("relative submodule URL escapes its remote")
			}
			base = base[:split]
			suffix = rest
		} else {
			break
		}
	}
	separator := "/"
	if scpColon >= 0 && strings.HasSuffix(base, ":") {
		separator = ""
	}
	return base + separator + suffix, nil
}

func checkStashConsent(runner GitCommandRunner, checkout string, stash []SnapshotStashEntry, consent UntrackedConsent, key CheckoutKey) error {
	var missing []UntrackedItem
	for _, entry := range stash {
		untracked, err := stashHasUntracked(runner, checkout, entry.Commit)
		if err != nil {
			return err
		}
		if untracked {
			item := core.UntrackedItem{Kind: core.UntrackedItemStash, Checkout: key, Commit: entry.Commit, Message: entry.Message}
			if !consent.Covers(&item) {
				missing = append(missing, item)
			}
		}
	}
	if len(missing) > 0 {
		return SnapshotRefusedUntracked(missing)
	}
	return nil
}

type PinnedWorktree struct {
	RelativePath string       `json:"relative_path"`
	Head         SnapshotHead `json:"head"`
}

type PinnedWorkspaceCut struct {
	Refs      map[string]string `json:"refs"`
	Head      SnapshotHead      `json:"head"`
	Worktrees []PinnedWorktree  `json:"worktrees"`
}

func (c PinnedWorkspaceCut) MarshalJSON() ([]byte, error) {
	type plain PinnedWorkspaceCut
	p := plain(c)
	p.Refs = snapshotNonNilMap(p.Refs)
	p.Worktrees = snapshotNonNil(p.Worktrees)
	return snapshotMarshal(p)
}

// PinWorkspaceCut reads refs and HEADs without fetching, aborting
// operations, creating stashes or changing anything. Branch commit IDs come
// from the one ref enumeration.
func PinWorkspaceCut(runner GitCommandRunner, checkout string, _ CheckoutOwnership) (PinnedWorkspaceCut, error) {
	refs, err := readAllRefs(runner, checkout)
	if err != nil {
		return PinnedWorkspaceCut{}, err
	}
	read := func(path string) (SnapshotHead, error) {
		head, err := readHead(runner, path)
		if err != nil {
			return SnapshotHead{}, err
		}
		if head.Kind != core.SnapshotHeadDetached {
			if value, ok := refs[head.Name]; ok {
				head.Commit = value
			} else {
				head = SnapshotHead{Kind: core.SnapshotHeadDetached, Commit: head.Commit}
			}
		}
		return head, nil
	}
	head, err := read(checkout)
	if err != nil {
		return PinnedWorkspaceCut{}, err
	}
	trees, err := discoverLinkedWorktrees(runner, checkout)
	if err != nil {
		return PinnedWorkspaceCut{}, err
	}
	worktrees := make([]PinnedWorktree, 0, len(trees))
	for _, tree := range trees {
		treeHead, err := read(tree.directory)
		if err != nil {
			return PinnedWorkspaceCut{}, err
		}
		worktrees = append(worktrees, PinnedWorktree{RelativePath: tree.relative, Head: treeHead})
	}
	return PinnedWorkspaceCut{Refs: refs, Head: head, Worktrees: worktrees}, nil
}

type consentInputs struct {
	untracked  []UntrackedItem
	operations []ShownOperation
}

// inventoryTrees is the inventory followed by its nested worktrees'.
func inventoryTrees(inventory *WorkspaceInventory) []*WorkspaceInventory {
	trees := []*WorkspaceInventory{inventory}
	for index := range inventory.Worktrees {
		trees = append(trees, &inventory.Worktrees[index].Inventory)
	}
	return trees
}

func checkUnconditionalBlockers(inventory *WorkspaceInventory) (consentInputs, error) {
	if len(inventory.Blockers) > 0 {
		return consentInputs{}, inventory.Blockers[0].Kind
	}
	var inputs consentInputs
	for _, tree := range inventoryTrees(inventory) {
		for _, entry := range tree.Untracked {
			inputs.untracked = append(inputs.untracked, core.UntrackedItem{Kind: core.UntrackedItemPath, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
		}
		for _, entry := range tree.NestedRepositories {
			inputs.untracked = append(inputs.untracked, core.UntrackedItem{Kind: core.UntrackedItemNestedRepository, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
		}
		for _, entry := range tree.UntrackedStashes {
			inputs.untracked = append(inputs.untracked, core.UntrackedItem{Kind: core.UntrackedItemStash, Checkout: entry.Checkout, Commit: entry.Value.Commit, Message: entry.Value.Message})
		}
		if tree.Operation != nil {
			inputs.operations = append(inputs.operations, *tree.Operation)
		}
	}
	return inputs, nil
}

func inventoryBlockers(inventory *WorkspaceInventory) ([]InventoryBlocker, error) {
	if inventory.InPlaceOnly {
		return []InventoryBlocker{}, nil
	}
	var reasons []SnapshotRefused
	if !inventory.Raw && inventory.OriginMissing {
		reasons = append(reasons, SnapshotRefusedMissingManagedOrigin(inventory.Checkout))
	}
	if !inventory.Raw && len(inventory.OriginsRemovedByTeardown) > 0 {
		reasons = append(reasons, SnapshotRefusedOriginRemovedByTeardown(slices.Clone(inventory.OriginsRemovedByTeardown)))
	}
	var directories []Scoped[string]
	var worktrees []Scoped[UnrepresentedWorktree]
	for _, tree := range inventoryTrees(inventory) {
		if len(tree.TrackedKeepFiles) > 0 {
			if tree.KeepDir == nil {
				return nil, errors.New("tracked keep inventory has no keep directory")
			}
			reasons = append(reasons, SnapshotRefusedTrackedKeepFiles{KeepDir: *tree.KeepDir, Paths: slices.Clone(tree.TrackedKeepFiles)})
		}
		if !(tree.Raw && tree.Operation != nil) {
			if len(tree.UntrackedSubmoduleRegistration) > 0 {
				reasons = append(reasons, SnapshotRefusedUntrackedSubmoduleRegistration(slices.Clone(tree.UntrackedSubmoduleRegistration)))
			}
			if len(tree.IntentToAdd) > 0 {
				reasons = append(reasons, SnapshotRefusedIntentToAdd(slices.Clone(tree.IntentToAdd)))
			}
			if len(tree.HiddenTrackedEdits) > 0 {
				reasons = append(reasons, SnapshotRefusedHiddenTrackedEdits(slices.Clone(tree.HiddenTrackedEdits)))
			}
		}
		if !tree.Raw {
			directories = append(directories, tree.Presence...)
			worktrees = append(worktrees, tree.UnrepresentedWorktrees...)
		}
		if !tree.Raw {
			for _, path := range tree.DirtySubmodules {
				reasons = append(reasons, SnapshotRefusedDirtySubmodule(path))
			}
		}
	}
	if len(directories) > 0 || len(worktrees) > 0 {
		reasons = append(reasons, SnapshotRefusedUnrepresentedGitState{Directories: directories, Worktrees: worktrees})
	}
	blockers := make([]InventoryBlocker, 0, len(reasons))
	for _, kind := range reasons {
		var paths []Scoped[string]
		switch kind := kind.(type) {
		case SnapshotRefusedTrackedKeepFiles:
			paths = slices.Clone(kind.Paths)
		case SnapshotRefusedIntentToAdd:
			paths = slices.Clone(kind)
		case SnapshotRefusedHiddenTrackedEdits:
			paths = slices.Clone(kind)
		case SnapshotRefusedUntrackedSubmoduleRegistration:
			paths = slices.Clone(kind)
		case SnapshotRefusedOriginRemovedByTeardown:
			for _, origin := range kind {
				paths = append(paths, scoped(origin.Checkout, origin.Value.Route))
			}
		case SnapshotRefusedDirtySubmodule:
			paths = []Scoped[string]{Scoped[string](kind)}
		case SnapshotRefusedMissingManagedOrigin:
			paths = []Scoped[string]{scoped(CheckoutKey(kind), "")}
		case SnapshotRefusedUnrepresentedGitState:
			paths = slices.Clone(kind.Directories)
			for _, item := range kind.Worktrees {
				paths = append(paths, scoped(item.Checkout, item.Value.Path))
			}
		}
		blockers = append(blockers, InventoryBlocker{Remedy: kind.Error(), Paths: snapshotNonNil(paths), Kind: kind})
	}
	return blockers, nil
}

func checkConsentInputs(raw bool, inputs consentInputs, consent CaptureConsent) error {
	if raw {
		return nil
	}
	var untracked []UntrackedItem
	for _, entry := range inputs.untracked {
		if !consent.Untracked.Covers(&entry) {
			untracked = append(untracked, entry)
		}
	}
	if len(untracked) > 0 {
		return SnapshotRefusedUntracked(untracked)
	}
	for _, operation := range inputs.operations {
		if !consent.Operations.Covers(&operation) {
			return SnapshotRefusedOperationInProgress(operation)
		}
	}
	return nil
}

type originExclusions struct {
	objects []string
	stale   bool
	notice  *string
}

func refspecSuffix(pattern string, reference string) (string, bool) {
	if prefix, suffix, ok := strings.Cut(pattern, "*"); ok {
		rest, ok := strings.CutPrefix(reference, prefix)
		if !ok {
			return "", false
		}
		return strings.CutSuffix(rest, suffix)
	}
	return "", pattern == reference
}

// mappedFetchSources are all effective source refs that could have
// populated a destination. Origin shares the same provenance rule as other
// remotes; one eligible mapping is not proof when another mapping can put
// pull/review-only history there.
func mappedFetchSources(specs []string, destination string) []string {
	sources := map[string]bool{}
	for _, spec := range specs {
		if strings.HasPrefix(spec, "^") {
			continue
		}
		source, pattern, ok := strings.Cut(strings.TrimLeft(spec, "+"), ":")
		if !ok {
			continue
		}
		suffix, ok := refspecSuffix(pattern, destination)
		if !ok {
			continue
		}
		source = strings.Replace(source, "*", suffix, 1)
		negated := false
		for _, negative := range specs {
			if pattern, ok := strings.CutPrefix(negative, "^"); ok {
				if _, matches := refspecSuffix(pattern, source); matches {
					negated = true
					break
				}
			}
		}
		if !negated {
			sources[source] = true
		}
	}
	return slices.Sorted(maps.Keys(sources))
}

type originTracking struct {
	objects   []string
	ambiguous bool
}

func originTrackingObjects(runner GitCommandRunner, checkout string, refs map[string]string) (originTracking, error) {
	specs, err := configValues(runner, checkout, "remote.origin.fetch")
	if err != nil {
		return originTracking{}, err
	}
	remotes, err := gitText(runner, checkout, []string{"remote"})
	if err != nil {
		return originTracking{}, err
	}
	var otherMappings [][]string
	for _, remote := range gitLines(remotes) {
		if remote == "origin" {
			continue
		}
		values, err := configValues(runner, checkout, "remote."+remote+".fetch")
		if err != nil {
			return originTracking{}, err
		}
		otherMappings = append(otherMappings, values)
	}
	eligible := func(source string) bool {
		return strings.HasPrefix(source, "refs/heads/") || strings.HasPrefix(source, "refs/tags/")
	}
	var tracking originTracking
	for _, name := range slices.Sorted(maps.Keys(refs)) {
		sources := mappedFetchSources(specs, name)
		if !strings.HasPrefix(name, "refs/remotes/") || !slices.ContainsFunc(sources, eligible) {
			continue
		}
		ambiguous := slices.ContainsFunc(sources, func(source string) bool { return !eligible(source) })
		for _, other := range otherMappings {
			if len(mappedFetchSources(other, name)) > 0 {
				ambiguous = true
			}
		}
		if ambiguous {
			tracking.ambiguous = true
		} else {
			tracking.objects = append(tracking.objects, refs[name])
		}
	}
	slices.Sort(tracking.objects)
	tracking.objects = slices.Compact(tracking.objects)
	return tracking, nil
}

// sharedSourceSkip is the ref names that must not be excluded when remote
// shares checkout's common Git directory.
type sharedSourceSkip struct {
	sharesCommonDir bool
	refs            map[string]bool
}

// sharedSourceSkipRefs lists the ref names that must not be excluded when
// remote shares checkout's common Git directory (for example an originless
// legacy linked worktree falling back to source_repository via
// origin_override). Otherwise ls-remote would advertise the session's own
// mutable branch back to capture and drop the session's commits from the
// pack.
func sharedSourceSkipRefs(runner GitCommandRunner, checkout string, remote string, selectedRefs map[string]string) (sharedSourceSkip, error) {
	none := sharedSourceSkip{}
	if remote == "" {
		return none, nil
	}
	effective, err := runGit(runner, checkout, []string{"ls-remote", "--get-url", remote}, nil)
	if err != nil {
		return none, err
	}
	if effective.Status != 0 {
		return none, nil
	}
	text, err := gitDecodeUTF8(bytes.TrimSuffix(effective.Stdout, []byte("\n")))
	if err != nil {
		return none, err
	}
	localRemote, ok, err := localOriginRoute(checkout, text)
	if err != nil {
		return none, err
	}
	if !ok || !checkpointExists(localRemote) {
		return none, nil
	}
	checkoutCommonOut, err := runGit(runner, checkout, []string{"rev-parse", "--git-common-dir"}, nil)
	if err != nil {
		return none, err
	}
	remoteCommonOut, err := runGit(runner, localRemote, []string{"rev-parse", "--git-common-dir"}, nil)
	if err != nil {
		return none, err
	}
	if checkoutCommonOut.Status != 0 || remoteCommonOut.Status != 0 {
		return none, nil
	}
	checkoutCommon, err := absoluteGitPath(checkout, checkoutCommonOut.Stdout)
	if err != nil {
		return none, err
	}
	remoteCommon, err := absoluteGitPath(localRemote, remoteCommonOut.Stdout)
	if err != nil {
		return none, err
	}
	if canonical, err := archiveCanonicalize(checkoutCommon); err == nil {
		checkoutCommon = canonical
	}
	if canonical, err := archiveCanonicalize(remoteCommon); err == nil {
		remoteCommon = canonical
	}
	if !checkpointPathEqual(checkoutCommon, remoteCommon) {
		return none, nil
	}
	skip := sharedSourceSkip{sharesCommonDir: true, refs: map[string]bool{}}
	for name := range selectedRefs {
		skip.refs[name] = true
	}
	return skip, nil
}

// originExclusionsFor shares origin queries and objects-only downloads
// between capture and dry-run (Rust's origin_exclusions).
func originExclusionsFor(runner GitCommandRunner, checkout string, remote string, refs map[string]string, skip sharedSourceSkip) (originExclusions, error) {
	fallback, err := originTrackingObjects(runner, checkout, refs)
	if err != nil {
		return originExclusions{}, err
	}
	result, err := remoteObjects(runner, checkout, remote, fallback.objects, skip)
	if err != nil {
		return originExclusions{}, err
	}
	if result.stale && len(result.objects) == 0 {
		notice := "snapshot carries full history because origin was unreachable and no offline refs were available"
		if fallback.ambiguous {
			notice = "snapshot carries full history because origin was unreachable and its offline refs were ambiguous"
		}
		result.notice = &notice
		result.stale = false
	}
	return result, nil
}

func remoteObjects(runner GitCommandRunner, checkout string, remote string, fallback []string, skip sharedSourceSkip) (originExclusions, error) {
	advertised, advertisedErr := runGit(runner, checkout, []string{"ls-remote", remote}, nil)
	objects := []string{}
	stale := false
	if advertisedErr == nil && advertised.Status == 0 {
		text, err := gitDecodeUTF8(advertised.Stdout)
		if err != nil {
			return originExclusions{}, err
		}
		var tips []string
		for _, line := range gitLines(text) {
			oid, name, ok := strings.Cut(line, "\t")
			if !ok {
				return originExclusions{}, errors.New("malformed origin advertisement")
			}
			if skip.sharesCommonDir && (skip.refs[name] || strings.HasPrefix(name, "refs/heads/mj/")) {
				continue
			}
			if strings.HasPrefix(name, "refs/heads/") || strings.HasPrefix(name, "refs/tags/") {
				tips = append(tips, oid)
			}
		}
		slices.Sort(tips)
		tips = slices.Compact(tips)
		missing, err := missingTips(runner, checkout, tips)
		if err != nil {
			return originExclusions{}, err
		}
		if len(missing) > 0 {
			output, err := runner.Run(checkout, &GitCommand{
				Arguments: []string{
					"fetch",
					"--no-write-fetch-head",
					"--no-tags",
					"--no-prune",
					"--no-prune-tags",
					"--refmap=",
					"--no-recurse-submodules",
					"--stdin",
					remote,
				},
				Stdin: []byte(strings.Join(missing, "\n") + "\n"),
			})
			switch {
			case err != nil:
				slog.Warn("could not download advertised origin tips; packing locally missing history", "error", err.Error())
			case output.Status != 0:
				slog.Warn("could not download advertised origin tips; packing locally missing history", "status", output.Status)
			}
		}
		stillMissing, err := missingTips(runner, checkout, tips)
		if err != nil {
			return originExclusions{}, err
		}
		unavailable := map[string]bool{}
		for _, tip := range stillMissing {
			unavailable[tip] = true
		}
		// A successful advertisement is authoritative even if downloading a
		// missing tip fails. Stale refs may cover force-pushed-away work.
		for _, tip := range tips {
			if !unavailable[tip] {
				objects = append(objects, tip)
			}
		}
	} else {
		// Deliberate policy even after an offline origin URL change: callers
		// warn on stale refs, and restore verifies all missing dependencies.
		objects = append(objects, fallback...)
		stale = true
	}
	slices.Sort(objects)
	objects = slices.Compact(objects)
	return originExclusions{objects: objects, stale: stale}, nil
}

func submoduleSelectionHasDelta(runner GitCommandRunner, checkout string, tips []string) (bool, bool, error) {
	refs, err := readAllRefs(runner, checkout)
	if err != nil {
		return false, false, err
	}
	for _, name := range slices.Sorted(maps.Keys(refs)) {
		if localSnapshotRef(name) {
			tips = append(tips, refs[name])
		}
	}
	stash, err := readStash(runner, checkout)
	if err != nil {
		return false, false, err
	}
	for _, entry := range stash {
		tips = append(tips, entry.Commit)
	}
	exclusions, err := originExclusionsFor(runner, checkout, "origin", refs, sharedSourceSkip{})
	if err != nil {
		return false, false, err
	}
	slices.Sort(tips)
	tips = slices.Compact(tips)
	missing, err := missingTips(runner, checkout, tips)
	if err != nil {
		return false, false, err
	}
	if len(missing) > 0 {
		return true, exclusions.stale, nil
	}
	revisions := strings.Join(tips, "\n") + "\n--not\n" + strings.Join(exclusions.objects, "\n") + "\n"
	objects, err := gitBytes(runner, checkout, []string{"rev-list", "--objects", "--no-object-names", "--stdin"}, []byte(revisions), "inspect submodule delta")
	if err != nil {
		return false, false, err
	}
	return len(objects) != 0, exclusions.stale, nil
}

func snapshotExclusions(runner GitCommandRunner, checkout string, remote string, refs map[string]string, selectedRefs map[string]string, source ExclusionSource) (originExclusions, error) {
	switch source.Kind {
	case ExclusionSourceRemote:
		skip, err := sharedSourceSkipRefs(runner, checkout, remote, selectedRefs)
		if err != nil {
			return originExclusions{}, err
		}
		result, err := remoteObjects(runner, checkout, remote, nil, skip)
		if err != nil {
			return originExclusions{}, err
		}
		if result.stale && len(result.objects) == 0 {
			notice := "snapshot carries full history because the selected remote was unreachable"
			result.notice = &notice
			result.stale = false
		}
		return result, nil
	case ExclusionSourceCommits:
		objects := []string{}
		for _, commit := range source.Commits {
			object, err := gitText(runner, checkout, []string{"rev-parse", "--verify", "--end-of-options", commit + "^{commit}"})
			if err != nil {
				return originExclusions{}, fmt.Errorf("cutoff exclusion commit %s is unavailable: %w", commit, err)
			}
			objects = append(objects, object)
		}
		slices.Sort(objects)
		return originExclusions{objects: slices.Compact(objects)}, nil
	}
	skip, err := sharedSourceSkipRefs(runner, checkout, remote, selectedRefs)
	if err != nil {
		return originExclusions{}, err
	}
	return originExclusionsFor(runner, checkout, remote, refs, skip)
}

func configValues(runner GitCommandRunner, checkout string, key string) ([]string, error) {
	output, err := runGit(runner, checkout, []string{"config", "--null", "--get-all", key}, nil)
	if err != nil {
		return nil, err
	}
	if output.Status == 1 {
		return []string{}, nil
	}
	if output.Status != 0 {
		return nil, gitFailure("read branch tracking", &output)
	}
	values := []string{}
	for _, value := range bytes.Split(bytes.TrimSuffix(output.Stdout, []byte{0}), []byte{0}) {
		text, err := gitDecodeUTF8(value)
		if err != nil {
			return nil, err
		}
		values = append(values, text)
	}
	return values, nil
}

func branchTracking(runner GitCommandRunner, checkout string, refs map[string]string, cut PinnedWorkspaceCut) (map[string]BranchTracking, error) {
	tracking := map[string]BranchTracking{}
	for _, reference := range slices.Sorted(maps.Keys(refs)) {
		name, ok := strings.CutPrefix(reference, "refs/heads/")
		if !ok {
			continue
		}
		isHead := func(head SnapshotHead) bool { return head.Kind != core.SnapshotHeadDetached && head.Name == reference }
		path := ""
		if !isHead(cut.Head) {
			for _, tree := range cut.Worktrees {
				if isHead(tree.Head) {
					path = tree.RelativePath
					break
				}
			}
		}
		owner := gitJoin(checkout, path)
		remotes, err := configValues(runner, owner, "branch."+name+".remote")
		if err != nil {
			return nil, err
		}
		merge, err := configValues(runner, owner, "branch."+name+".merge")
		if err != nil {
			return nil, err
		}
		if len(remotes) > 0 && remotes[len(remotes)-1] == "origin" {
			remote := remotes[len(remotes)-1]
			tracking[name] = BranchTracking{Remote: &remote, Merge: merge}
		}
	}
	return tracking, nil
}

// selectedRefs chooses once per cut: primary first, then nested checkout
// discovery order.
func selectedRefs(runner GitCommandRunner, checkout string, cut PinnedWorkspaceCut, ownership CheckoutOwnership) (map[string]string, error) {
	legacy := false
	if !ownership.Raw {
		owns, err := ownsGitDirectory(runner, checkout, ownership)
		if err != nil {
			return nil, err
		}
		legacy = !owns
	}
	branches := map[string]bool{}
	for _, head := range append([]SnapshotHead{cut.Head}, worktreeHeads(cut)...) {
		if head.Kind != core.SnapshotHeadDetached {
			branches[head.Name] = true
		}
	}
	selected := map[string]string{}
	for name, oid := range cut.Refs {
		if !localSnapshotRef(name) {
			continue
		}
		keep := branches[name]
		if !keep {
			switch {
			case !ownership.Raw:
				keep = !legacy
			case ownership.Launch != nil:
				launched, ok := ownership.Launch.Refs[name]
				keep = !ok || launched != oid
			default:
				keep = true
			}
		}
		if keep {
			selected[name] = oid
		}
	}
	return selected, nil
}

func worktreeHeads(cut PinnedWorkspaceCut) []SnapshotHead {
	heads := make([]SnapshotHead, 0, len(cut.Worktrees))
	for _, tree := range cut.Worktrees {
		heads = append(heads, tree.Head)
	}
	return heads
}

// writeSnapshotPack packs tips not reachable from exclusions into a new
// temporary file beside base. It returns the pack's path, or "" for an empty
// pack; the caller removes a returned pack it does not keep.
func writeSnapshotPack(runner GitCommandRunner, checkout string, tips []string, exclusions []string, base string) (string, error) {
	tips = slices.Clone(tips)
	slices.Sort(tips)
	tips = slices.Compact(tips)
	exclusions = slices.Clone(exclusions)
	slices.Sort(exclusions)
	exclusions = slices.Compact(exclusions)
	var revisions strings.Builder
	revisions.WriteString(strings.Join(tips, "\n"))
	revisions.WriteString("\n--not\n")
	for _, oid := range exclusions {
		revisions.WriteString(oid)
		revisions.WriteByte('\n')
	}
	base, err := snapshotAbsolute(base)
	if err != nil {
		return "", err
	}
	parent, ok := checkpointParent(base)
	if !ok {
		return "", errors.New("pack has no parent")
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return "", err
	}
	name, ok := checkpointFileName(base)
	if !ok {
		return "", errors.New("pack base has no filename")
	}
	// Every in-flight capture owns a distinct temp file in the destination
	// directory, removed if packing fails or produces an empty pack.
	// Streaming via `--stdout` avoids Git creating a temp pack in the
	// repository's object store and renaming it across filesystems (#1635).
	published, err := os.CreateTemp(parent, name+"*.pack")
	if err != nil {
		return "", err
	}
	keep := false
	defer func() {
		published.Close()
		if !keep {
			os.Remove(published.Name())
		}
	}()
	// A snapshot has one pack artifact; ignore repository-configured
	// splitting.
	output, err := runToFile(runner, checkout, &GitCommand{
		Arguments: []string{"-c", "pack.packSizeLimit=0", "pack-objects", "--revs", "--stdout", "--quiet"},
		Stdin:     []byte(revisions.String()),
	}, published)
	if err != nil {
		return "", err
	}
	if output.Status != 0 {
		return "", gitFailure("pack snapshot objects", &output)
	}
	info, err := published.Stat()
	if err != nil {
		return "", err
	}
	if info.Size() < 32 {
		return "", errors.New("Git returned an invalid snapshot pack")
	}
	header := make([]byte, 12)
	if _, err := published.ReadAt(header, 0); err != nil {
		return "", err
	}
	if !bytes.HasPrefix(header, []byte("PACK")) {
		return "", errors.New("Git returned an invalid snapshot pack")
	}
	if bytes.Equal(header[8:12], []byte{0, 0, 0, 0}) {
		return "", nil
	}
	keep = true
	return published.Name(), nil
}

type DeltaEstimate struct {
	CommittedBytes uint64 `json:"committed_bytes"`
	// WorkingStateBytes is additional working-state pack bytes; unknown
	// during an operation.
	WorkingStateBytes *uint64 `json:"working_state_bytes"`
	// TotalBytes is combined pack bytes; unknown when tracked working state
	// cannot be read.
	TotalBytes      *uint64 `json:"total_bytes"`
	RemoteRefsStale bool    `json:"remote_refs_stale"`
}

// EstimateDelta is a dry run: it changes no ref, index, working file or
// stash list, and may add unreferenced objects that garbage collection
// removes. It never aborts operations.
func EstimateDelta(runner GitCommandRunner, checkout string, ownership CheckoutOwnership, originOverride *string) (DeltaEstimate, error) {
	return estimateDeltaWithSource(runner, checkout, ownership, originOverride, ExclusionSource{})
}

func EstimateRepositoryDelta(runner GitCommandRunner, repository RepositoryInput) (DeltaEstimate, error) {
	return estimateDeltaWithSource(runner, repository.Checkout, repository.Ownership, repository.OriginOverride, repository.Exclusions)
}

func estimateDeltaWithSource(inner GitCommandRunner, checkout string, ownership CheckoutOwnership, originOverride *string, source ExclusionSource) (DeltaEstimate, error) {
	var runner GitCommandRunner = newCaptureGit(inner)
	historyCutoff := source.Kind == ExclusionSourceCommits
	var remote string
	var inventoryOrigin *string
	switch source.Kind {
	case ExclusionSourceCommits:
	case ExclusionSourceRemote:
		absolute, err := absoluteConfigURL(checkout, source.Remote)
		if err != nil {
			return DeltaEstimate{}, err
		}
		url := source.Remote
		remote, inventoryOrigin = absolute, &url
	default:
		configured, err := runGit(runner, checkout, []string{"remote", "get-url", "origin"}, nil)
		if err != nil {
			return DeltaEstimate{}, err
		}
		var selected *string
		if configured.Status == 0 {
			origin := "origin"
			selected = &origin
		} else {
			selected = originOverride
		}
		if selected == nil {
			if !ownership.Raw {
				return DeltaEstimate{}, errors.New("managed snapshot requires an origin remote or source override")
			}
			return DeltaEstimate{}, nil
		}
		resolved := *selected
		if configured.Status != 0 {
			absolute, err := absoluteConfigURL(checkout, *selected)
			if err != nil {
				return DeltaEstimate{}, err
			}
			resolved = absolute
		}
		remote, inventoryOrigin = resolved, selected
	}
	inventory, err := workspaceInventoryScoped(runner, checkout, ownership, core.DefaultCheckoutKey(), inventoryOrigin, historyCutoff)
	if err != nil {
		return DeltaEstimate{}, err
	}
	cut, err := PinWorkspaceCut(runner, checkout, ownership)
	if err != nil {
		return DeltaEstimate{}, err
	}
	refs, err := selectedRefs(runner, checkout, cut, ownership)
	if err != nil {
		return DeltaEstimate{}, err
	}
	exclusions, err := snapshotExclusions(runner, checkout, remote, cut.Refs, refs, source)
	if err != nil {
		return DeltaEstimate{}, err
	}
	committed := slices.Collect(maps.Values(refs))
	committed = append(committed, cut.Head.Commit)
	for _, tree := range cut.Worktrees {
		committed = append(committed, tree.Head.Commit)
	}
	stash, err := selectedStash(runner, checkout, ownership)
	if err != nil {
		return DeltaEstimate{}, err
	}
	for _, entry := range stash {
		tracked, err := trackedStash(runner, checkout, entry)
		if err != nil {
			return DeltaEstimate{}, err
		}
		committed = append(committed, tracked.Commit)
	}
	slices.Sort(committed)
	committed = slices.Compact(committed)
	temp, err := os.MkdirTemp("", ".tmp")
	if err != nil {
		return DeltaEstimate{}, fmt.Errorf("create Git delta estimate directory: %w", err)
	}
	defer os.RemoveAll(temp)
	packBytes := func(tips []string, exclusions []string, name string) (uint64, error) {
		pack, err := writeSnapshotPack(runner, checkout, tips, exclusions, gitJoin(temp, name))
		if err != nil || pack == "" {
			return 0, err
		}
		defer os.Remove(pack)
		info, err := os.Stat(pack)
		if err != nil {
			return 0, err
		}
		return uint64(info.Size()), nil
	}
	committedBytes, err := packBytes(committed, exclusions.objects, "committed")
	if err != nil {
		return DeltaEstimate{}, err
	}
	stale := exclusions.stale || inventory.RemoteRefsStale
	operationInProgress := inventory.Operation != nil
	for _, nested := range inventory.Worktrees {
		stale = stale || nested.Inventory.RemoteRefsStale
		operationInProgress = operationInProgress || nested.Inventory.Operation != nil
	}
	if operationInProgress {
		return DeltaEstimate{CommittedBytes: committedBytes, RemoteRefsStale: stale}, nil
	}
	var working []string
	state, err := workingState(runner, checkout, false)
	if err != nil {
		return DeltaEstimate{}, err
	}
	if state != nil {
		working = append(working, *state)
	}
	for _, tree := range cut.Worktrees {
		state, err := workingState(runner, gitJoin(checkout, tree.RelativePath), false)
		if err != nil {
			return DeltaEstimate{}, err
		}
		if state != nil {
			working = append(working, *state)
		}
	}
	workingExclusions := append(slices.Clone(exclusions.objects), committed...)
	var workingStateBytes uint64
	if len(working) > 0 {
		workingStateBytes, err = packBytes(working, workingExclusions, "working")
		if err != nil {
			return DeltaEstimate{}, err
		}
	}
	combined := append(slices.Clone(committed), working...)
	totalBytes, err := packBytes(combined, exclusions.objects, "combined")
	if err != nil {
		return DeltaEstimate{}, err
	}
	return DeltaEstimate{
		CommittedBytes:    committedBytes,
		WorkingStateBytes: &workingStateBytes,
		TotalBytes:        &totalBytes,
		RemoteRefsStale:   stale,
	}, nil
}

// ExclusionSourceKind is the variant of ExclusionSource; the zero value is
// Origin, Rust's default.
type ExclusionSourceKind int

const (
	ExclusionSourceOrigin ExclusionSourceKind = iota
	// ExclusionSourceRemote excludes objects reachable from an explicit
	// remote URL even when `remote.origin.url` points elsewhere.
	ExclusionSourceRemote
	ExclusionSourceCommits
)

// ExclusionSource is what a capture's pack is relative to.
type ExclusionSource struct {
	Kind    ExclusionSourceKind
	Remote  string
	Commits []string
}

func (s ExclusionSource) MarshalJSON() ([]byte, error) {
	switch s.Kind {
	case ExclusionSourceRemote:
		return snapshotMarshal(map[string]string{"Remote": s.Remote})
	case ExclusionSourceCommits:
		return snapshotMarshal(map[string][]string{"Commits": snapshotNonNil(s.Commits)})
	}
	return []byte(`"Origin"`), nil
}

func (s *ExclusionSource) UnmarshalJSON(data []byte) error {
	tag, body, err := snapshotExternalTag(data)
	if err != nil {
		return err
	}
	switch tag {
	case "Origin":
		*s = ExclusionSource{}
	case "Remote":
		var remote string
		if err := json.Unmarshal(body, &remote); err != nil {
			return err
		}
		*s = ExclusionSource{Kind: ExclusionSourceRemote, Remote: remote}
	case "Commits":
		var commits []string
		if err := json.Unmarshal(body, &commits); err != nil {
			return err
		}
		*s = ExclusionSource{Kind: ExclusionSourceCommits, Commits: snapshotNonNil(commits)}
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `Origin`, `Remote`, `Commits`", tag)
	}
	return nil
}

type RepositoryInput struct {
	ID             string            `json:"id"`
	Checkout       string            `json:"checkout"`
	Ownership      CheckoutOwnership `json:"ownership"`
	OriginOverride *string           `json:"origin_override"`
	PackPath       string            `json:"pack_path"`
	Exclusions     ExclusionSource   `json:"exclusions" serde:"default"`
	KeepDir        *string           `json:"keep_dir"`
}

func (r *RepositoryInput) UnmarshalJSON(data []byte) error {
	type plain RepositoryInput
	return checkpointStrictDecode(data, (*plain)(r), false)
}

type SessionCaptureConsent struct {
	// Repositories scopes consent by repository ID, then by the path or item
	// inside it.
	Repositories map[string]CaptureConsent `json:"repositories"`
}

func (c SessionCaptureConsent) MarshalJSON() ([]byte, error) {
	type plain SessionCaptureConsent
	return snapshotMarshal(plain{snapshotNonNilMap(c.Repositories)})
}

func (c *SessionCaptureConsent) UnmarshalJSON(data []byte) error {
	type plain SessionCaptureConsent
	return checkpointStrictDecode(data, (*plain)(c), false)
}

type SessionSnapshot struct {
	Repositories map[string]WorkspaceSnapshot `json:"repositories"`
}

func (s SessionSnapshot) MarshalJSON() ([]byte, error) {
	type plain SessionSnapshot
	return snapshotMarshal(plain{snapshotNonNilMap(s.Repositories)})
}

func (s *SessionSnapshot) UnmarshalJSON(data []byte) error {
	type plain SessionSnapshot
	return checkpointStrictDecode(data, (*plain)(s), false)
}

func sessionError(id string, err error) error {
	var refused SnapshotRefused
	if errors.As(err, &refused) {
		return err
	}
	return fmt.Errorf("snapshot repository %s: %w", id, err)
}

func repositoryKey(id string) CheckoutKey {
	return CheckoutKey{RepositoryID: id, CheckoutPath: ""}
}

func inPlaceOnlyMode(runner GitCommandRunner, checkout string, ownership CheckoutOwnership, originOverride *string) (bool, error) {
	if !ownership.Raw || originOverride != nil {
		return false, nil
	}
	output, err := runGit(runner, checkout, []string{"remote", "get-url", "origin"}, nil)
	if err != nil {
		return false, err
	}
	return output.Status != 0, nil
}

func inPlaceOnlyInventory(runner GitCommandRunner, checkout string, key CheckoutKey) (WorkspaceInventory, error) {
	operation, err := operationRecord(runner, checkout, key)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	hidden, err := HiddenTrackedPaths(runner, checkout)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	hiddenEdits := make([]Scoped[string], 0, len(hidden))
	for _, path := range hidden {
		hiddenEdits = append(hiddenEdits, scoped(key, path))
	}
	return WorkspaceInventory{
		Notices:            []string{rawNotice},
		Checkout:           key,
		InPlaceOnly:        true,
		OriginMissing:      true,
		Operation:          operation,
		Raw:                true,
		HiddenTrackedEdits: hiddenEdits,
	}, nil
}

// captureMode is a repository's capture plan: in place only (nil
// inventory), or a transfer of its inventory.
type captureMode struct {
	inventory *WorkspaceInventory
}

// CaptureSessionSnapshot inventories every repository and decides consent
// for the complete session before the first abort or other mutation. Paths
// inside consent remain repository-relative. The caller must have stopped
// all writers to managed checkouts and hold their exclusive ownership until
// capture returns. Raw checkouts are never aborted.
func CaptureSessionSnapshot(inner GitCommandRunner, repositories []RepositoryInput, consent SessionCaptureConsent) (SessionSnapshot, error) {
	var runner GitCommandRunner = newCaptureGit(inner)
	none := CaptureConsent{}
	consentFor := func(id string) CaptureConsent {
		if found, ok := consent.Repositories[id]; ok {
			return found
		}
		return none
	}
	inventories := map[string]*captureMode{}
	for _, repository := range repositories {
		if _, ok := inventories[repository.ID]; ok {
			return SessionSnapshot{}, fmt.Errorf("duplicate snapshot repository ID %s", repository.ID)
		}
		historyCutoff := repository.Exclusions.Kind == ExclusionSourceCommits
		inPlace := false
		if !historyCutoff {
			mode, err := inPlaceOnlyMode(runner, repository.Checkout, repository.Ownership, repository.OriginOverride)
			if err != nil {
				return SessionSnapshot{}, err
			}
			inPlace = mode
		}
		mode := &captureMode{}
		if !inPlace {
			inventory, err := repositoryInventory(runner, repository)
			if err != nil {
				return SessionSnapshot{}, sessionError(repository.ID, err)
			}
			mode.inventory = &inventory
		}
		inventories[repository.ID] = mode
	}
	deletionRoots, err := sessionTeardownRoots(runner, repositories)
	if err != nil {
		return SessionSnapshot{}, err
	}
	for _, repository := range repositories {
		mode, ok := inventories[repository.ID]
		if !ok {
			return SessionSnapshot{}, errors.New("missing capture inventory")
		}
		if mode.inventory == nil {
			continue
		}
		if repository.Exclusions.Kind != ExclusionSourceCommits {
			if err := inventoryOriginDependencies(runner, repository.Checkout, mode.inventory, repository.OriginOverride, deletionRoots); err != nil {
				return SessionSnapshot{}, err
			}
		}
		blockers, err := inventoryBlockers(mode.inventory)
		if err != nil {
			return SessionSnapshot{}, err
		}
		mode.inventory.Blockers = blockers
	}
	validate := func() error {
		type pendingCheck struct {
			repository RepositoryInput
			raw        bool
			inputs     consentInputs
		}
		var pending []pendingCheck
		for _, repository := range repositories {
			if inventory := inventories[repository.ID].inventory; inventory != nil {
				inputs, err := checkUnconditionalBlockers(inventory)
				if err != nil {
					return sessionError(repository.ID, err)
				}
				pending = append(pending, pendingCheck{repository, inventory.Raw, inputs})
			}
		}
		for _, check := range pending {
			repoConsent := consentFor(check.repository.ID)
			if err := checkConsentInputs(check.raw, check.inputs, repoConsent); err != nil {
				return sessionError(check.repository.ID, err)
			}
			if !check.raw && check.repository.KeepDir != nil {
				if err := CheckKeepDirConsent(repositoryKey(check.repository.ID), *check.repository.KeepDir, repoConsent.Untracked, repoConsent.MoveKeep); err != nil {
					return sessionError(check.repository.ID, err)
				}
			}
		}
		return nil
	}
	if err := validate(); err != nil {
		return SessionSnapshot{}, err
	}
	aborted := false
	for _, repository := range repositories {
		inventory := inventories[repository.ID].inventory
		if inventory == nil || inventory.Raw {
			continue
		}
		for _, entry := range snapshotTreesWithPaths(inventory) {
			if entry.tree.Operation != nil {
				kind := entry.tree.Operation.Kind
				if _, err := prepareOperation(runner, gitJoin(repository.Checkout, entry.path), repository.Ownership, &kind); err != nil {
					return SessionSnapshot{}, err
				}
				aborted = true
			}
		}
	}
	if aborted {
		// One session owner aborts first, then inventories the final state
		// against every teardown root. No abort prediction or stability
		// retry.
		for _, repository := range repositories {
			mode, ok := inventories[repository.ID]
			if !ok {
				return SessionSnapshot{}, errors.New("missing capture inventory")
			}
			if mode.inventory == nil {
				continue
			}
			current, err := repositoryInventory(runner, repository)
			if err != nil {
				return SessionSnapshot{}, sessionError(repository.ID, err)
			}
			if repository.Exclusions.Kind != ExclusionSourceCommits {
				if err := inventoryOriginDependencies(runner, repository.Checkout, &current, repository.OriginOverride, deletionRoots); err != nil {
					return SessionSnapshot{}, err
				}
			}
			blockers, err := inventoryBlockers(&current)
			if err != nil {
				return SessionSnapshot{}, err
			}
			current.Blockers = blockers
			mode.inventory = &current
		}
		if err := validate(); err != nil {
			return SessionSnapshot{}, err
		}
	}
	snapshots := map[string]WorkspaceSnapshot{}
	var pendingPacks []string
	succeeded := false
	defer func() {
		if !succeeded {
			for _, pack := range pendingPacks {
				os.Remove(pack)
			}
		}
	}()
	for _, repository := range repositories {
		mode, ok := inventories[repository.ID]
		if !ok {
			return SessionSnapshot{}, errors.New("missing prepared inventory")
		}
		delete(inventories, repository.ID)
		if mode.inventory == nil {
			snapshot, err := captureInPlaceMetadata(runner, repository)
			if err != nil {
				return SessionSnapshot{}, err
			}
			snapshots[repository.ID] = snapshot
			continue
		}
		snapshot, err := capturePreparedWorkspace(runner, repository, consentFor(repository.ID), repositoryKey(repository.ID), &pendingPacks, *mode.inventory)
		if err != nil {
			return SessionSnapshot{}, sessionError(repository.ID, err)
		}
		snapshots[repository.ID] = snapshot
	}
	succeeded = true
	return SessionSnapshot{Repositories: snapshots}, nil
}

// CaptureWorkspaceSnapshot captures a one-repository session, retaining
// direct typed workspace refusals. Writers in managed clones must be stopped
// by the caller. Raw capture uses one ref enumeration and never rewrites
// refs, index, working files or stashes. Raw callers load their session's
// launch record with ReadLaunchRecord.
func CaptureWorkspaceSnapshot(runner GitCommandRunner, checkout string, ownership CheckoutOwnership, packPath string, consent CaptureConsent, originOverride *string) (WorkspaceSnapshot, error) {
	id := "workspace"
	repository := RepositoryInput{
		ID:             id,
		Checkout:       checkout,
		Ownership:      ownership,
		OriginOverride: originOverride,
		PackPath:       packPath,
	}
	session, err := CaptureSessionSnapshot(runner, []RepositoryInput{repository}, SessionCaptureConsent{
		Repositories: map[string]CaptureConsent{id: consent},
	})
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	snapshot, ok := session.Repositories[id]
	if !ok {
		return WorkspaceSnapshot{}, errors.New("single-repository snapshot was not captured")
	}
	return snapshot, nil
}

func ownsGitDirectory(runner GitCommandRunner, checkout string, ownership CheckoutOwnership) (bool, error) {
	if ownership.Raw {
		return false, nil
	}
	_, common, err := gitDirectories(runner, checkout)
	if err != nil {
		return false, err
	}
	commonCanonical, err := archiveCanonicalize(common)
	if err != nil {
		return false, err
	}
	checkoutCanonical, err := archiveCanonicalize(checkout)
	if err != nil {
		return false, err
	}
	return checkpointPathStartsWith(commonCanonical, checkoutCanonical), nil
}

func captureInPlaceMetadata(runner GitCommandRunner, repository RepositoryInput) (WorkspaceSnapshot, error) {
	checkout := repository.Checkout
	inventory, err := inPlaceOnlyInventory(runner, checkout, repositoryKey(repository.ID))
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	skipped := inventory.Operation != nil
	notices := inventory.Notices
	status, statusErr := runGit(runner, checkout, []string{
		"status", "--porcelain", "--untracked-files=no", "--ignore-submodules=none",
	}, nil)
	var dirty bool
	if statusErr == nil && status.Status == 0 {
		dirty = len(status.Stdout) != 0
	} else {
		slog.Warn("could not inspect raw in-place tracked state; checkout is kept", "checkout", checkout)
		notices = append(notices, "tracked state could not be inspected; the raw checkout is kept")
		dirty = true
	}
	hidden := inventory.HiddenTrackedEdits
	objectFormat, err := gitText(runner, checkout, []string{"rev-parse", "--show-object-format"})
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	head, err := readHead(runner, checkout)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	return WorkspaceSnapshot{
		Metadata: WorkspaceSnapshotMetadata{
			ObjectFormat:          objectFormat,
			Notices:               notices,
			Head:                  head,
			Refs:                  map[string]string{},
			BranchTracking:        map[string]BranchTracking{},
			Stash:                 []SnapshotStashEntry{},
			WorkingStateSkipped:   skipped,
			HasUncommittedState:   skipped || dirty || len(hidden) > 0,
			Worktrees:             []SnapshotWorktree{},
			InitializedSubmodules: []SnapshotSubmodule{},
			IndexGitlinks:         []SnapshotGitlink{},
			Prerequisites:         []string{},
		},
	}, nil
}

// DroppedUntrackedNoticePrefix starts the notice recorded with the final
// inventory of an origin-delta capture.
const DroppedUntrackedNoticePrefix = "Dropped untracked items: "

func capturePreparedWorkspace(runner GitCommandRunner, repository RepositoryInput, consent CaptureConsent, key CheckoutKey, pendingPacks *[]string, inventory WorkspaceInventory) (WorkspaceSnapshot, error) {
	checkout := repository.Checkout
	ownership := repository.Ownership
	historyCutoff := repository.Exclusions.Kind == ExclusionSourceCommits
	var originURL *string
	var remote string
	switch repository.Exclusions.Kind {
	case ExclusionSourceCommits:
	case ExclusionSourceRemote:
		absolute, err := absoluteConfigURL(checkout, repository.Exclusions.Remote)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		redacted, err := redactSnapshotURL(repository.Exclusions.Remote)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		originURL, remote = &redacted, absolute
	default:
		configuredValues, err := configValues(runner, checkout, "remote.origin.url")
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		var origin string
		switch {
		case len(configuredValues) > 0:
			origin = configuredValues[0]
		case repository.OriginOverride != nil:
			origin = *repository.OriginOverride
		default:
			return captureInPlaceMetadata(runner, repository)
		}
		remote = "origin"
		if len(configuredValues) == 0 {
			absolute, err := absoluteConfigURL(checkout, origin)
			if err != nil {
				return WorkspaceSnapshot{}, err
			}
			remote = absolute
		}
		redacted, err := redactSnapshotURL(origin)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		originURL = &redacted
	}
	cut, err := PinWorkspaceCut(runner, checkout, ownership)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	refs, err := selectedRefs(runner, checkout, cut, ownership)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	exclusions, err := snapshotExclusions(runner, checkout, remote, cut.Refs, refs, repository.Exclusions)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	notices := slices.Clone(inventory.Notices)
	if !inventory.Raw && consent.Untracked.Kind == core.ConsentAny {
		inputs, err := checkUnconditionalBlockers(&inventory)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		dropped := inputs.untracked
		if repository.KeepDir != nil {
			entries, _, err := ScanKeepDir(key, *repository.KeepDir)
			if err != nil {
				return WorkspaceSnapshot{}, err
			}
			for _, entry := range entries {
				dropped = append(dropped, core.UntrackedItem{Kind: core.UntrackedItemKeepDirFile, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
			}
		}
		if len(dropped) > 0 {
			// Keep the durable summary bounded. Capture can see an
			// arbitrarily large working tree, and the manifest has its own
			// size limit.
			notices = append(notices, DroppedUntrackedNoticePrefix+core.FormatDroppedUntracked(dropped))
		}
	}
	if exclusions.notice != nil {
		notices = append(notices, *exclusions.notice)
	}
	originalStash, err := selectedStash(runner, checkout, ownership)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	if !inventory.Raw {
		if err := checkStashConsent(runner, checkout, originalStash, consent.Untracked, key); err != nil {
			return WorkspaceSnapshot{}, err
		}
	}
	stash := make([]SnapshotStashEntry, 0, len(originalStash))
	for _, entry := range originalStash {
		tracked, err := trackedStash(runner, checkout, entry)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		stash = append(stash, tracked)
	}
	skipped := inventory.Raw && inventory.Operation != nil
	working, err := workingState(runner, checkout, skipped)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	head, err := headForWorkingState(runner, checkout, cut.Head, working)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	tips := []string{head.Commit}
	for _, name := range slices.Sorted(maps.Keys(refs)) {
		tips = append(tips, refs[name])
	}
	for _, entry := range stash {
		tips = append(tips, entry.Commit)
	}
	if working != nil {
		tips = append(tips, *working)
	}
	worktrees := []SnapshotWorktree{}
	for _, tree := range cut.Worktrees {
		directory := gitJoin(checkout, tree.RelativePath)
		index := slices.IndexFunc(inventory.Worktrees, func(nested NestedInventory) bool {
			return checkpointPathEqual(nested.Path, tree.RelativePath)
		})
		if index < 0 {
			return WorkspaceSnapshot{}, errors.New("missing checkout inventory")
		}
		own := &inventory.Worktrees[index].Inventory
		treeSkipped := inventory.Raw && own.Operation != nil
		state, err := workingState(runner, directory, treeSkipped)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		treeHead, err := headForWorkingState(runner, directory, tree.Head, state)
		if err != nil {
			return WorkspaceSnapshot{}, err
		}
		tips = append(tips, treeHead.Commit)
		if state != nil {
			tips = append(tips, *state)
		}
		uncommitted := state != nil || treeSkipped
		if !uncommitted {
			if uncommitted, err = stagedGitlinkChanges(runner, directory); err != nil {
				return WorkspaceSnapshot{}, err
			}
		}
		if !uncommitted {
			if uncommitted, err = submoduleUncommittedState(runner, directory, own); err != nil {
				return WorkspaceSnapshot{}, err
			}
		}
		worktrees = append(worktrees, SnapshotWorktree{
			HasUncommittedState:   uncommitted,
			RelativePath:          tree.RelativePath,
			Head:                  treeHead,
			WorkingState:          state,
			WorkingStateSkipped:   treeSkipped,
			InitializedSubmodules: scopedValues(own.InitializedSubmodules),
			IndexGitlinks:         scopedValues(own.IndexGitlinks),
		})
	}
	prerequisites, err := packPrerequisites(runner, checkout, tips, exclusions.objects)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	// Pack against the recorded prerequisites, not every exclusion. An
	// exclusion that is not an ancestor of the saved state can share blobs
	// with it; restore never checks for such a tip, so those objects must
	// travel in the pack.
	owned, err := writeSnapshotPack(runner, checkout, tips, prerequisites, repository.PackPath)
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	var pack *string
	if owned != "" {
		*pendingPacks = append(*pendingPacks, owned)
		pack = &owned
	}
	objectFormat, err := gitText(runner, checkout, []string{"rev-parse", "--show-object-format"})
	if err != nil {
		return WorkspaceSnapshot{}, err
	}
	tracking := map[string]BranchTracking{}
	if !historyCutoff {
		if tracking, err = branchTracking(runner, checkout, refs, cut); err != nil {
			return WorkspaceSnapshot{}, err
		}
	}
	uncommitted := working != nil || skipped
	if !uncommitted {
		if uncommitted, err = stagedGitlinkChanges(runner, checkout); err != nil {
			return WorkspaceSnapshot{}, err
		}
	}
	if !uncommitted {
		if uncommitted, err = submoduleUncommittedState(runner, checkout, &inventory); err != nil {
			return WorkspaceSnapshot{}, err
		}
	}
	stale := exclusions.stale || inventory.RemoteRefsStale
	for _, nested := range inventory.Worktrees {
		stale = stale || nested.Inventory.RemoteRefsStale
	}
	return WorkspaceSnapshot{
		Metadata: WorkspaceSnapshotMetadata{
			ObjectFormat:          objectFormat,
			OriginURL:             originURL,
			Notices:               notices,
			Head:                  head,
			Refs:                  maps.Clone(refs),
			BranchTracking:        tracking,
			Stash:                 stash,
			HasUncommittedState:   uncommitted,
			WorkingState:          working,
			WorkingStateSkipped:   skipped,
			Worktrees:             worktrees,
			InitializedSubmodules: scopedValues(inventory.InitializedSubmodules),
			IndexGitlinks:         scopedValues(inventory.IndexGitlinks),
			RemoteRefsStale:       stale,
			Prerequisites:         prerequisites,
			HistoryCutoff:         historyCutoff,
		},
		Pack: pack,
	}, nil
}

func scopedValues[T any](entries []Scoped[T]) []T {
	values := make([]T, 0, len(entries))
	for _, entry := range entries {
		values = append(values, entry.Value)
	}
	return values
}

func headForWorkingState(runner GitCommandRunner, checkout string, head SnapshotHead, state *string) (SnapshotHead, error) {
	if state != nil {
		parent, err := gitText(runner, checkout, []string{"rev-parse", *state + "^1"})
		if err != nil {
			return SnapshotHead{}, err
		}
		if parent != head.Commit {
			return SnapshotHead{Kind: core.SnapshotHeadDetached, Commit: parent}, nil
		}
	}
	return head, nil
}

func packPrerequisites(runner GitCommandRunner, checkout string, tips []string, exclusions []string) ([]string, error) {
	omitted := map[string]bool{}
	for _, tip := range tips {
		omitted[tip] = true
	}
	input := strings.Join(tips, "\n") + "\n--not\n" + strings.Join(exclusions, "\n") + "\n"
	output, err := gitBytes(runner, checkout, []string{"rev-list", "--objects", "--no-object-names", "--boundary", "--stdin"}, []byte(input), "record pack prerequisites")
	if err != nil {
		return nil, err
	}
	text, err := gitDecodeUTF8(output)
	if err != nil {
		return nil, err
	}
	boundary := map[string]bool{}
	for _, line := range gitLines(text) {
		if oid, ok := strings.CutPrefix(line, "-"); ok {
			boundary[oid] = true
		} else {
			delete(omitted, line)
		}
	}
	for oid := range omitted {
		boundary[oid] = true
	}
	return slices.Sorted(maps.Keys(boundary)), nil
}

func nestedRepositoryInventory(root string) (uint64, core.FileIdentity, error) {
	type pendingPath struct {
		path     string
		relative string
	}
	pending := []pendingPath{{root, ""}}
	identity := blake3.New(32, nil)
	var size uint64
	for len(pending) > 0 {
		next := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		metadata, err := os.Lstat(next.path)
		if err != nil {
			return 0, "", fmt.Errorf("inspect nested repository %s: %w", next.path, err)
		}
		encoded := []byte(next.relative)
		identity.Write(snapshotBigEndian64(uint64(len(encoded))))
		identity.Write(encoded)
		fileIdentity, err := FileIdentityOf(metadata)
		if err != nil {
			return 0, "", err
		}
		identity.Write([]byte(fileIdentity.AsStr()))
		if metadata.IsDir() {
			children, err := os.ReadDir(next.path)
			if err != nil {
				return 0, "", err
			}
			for index := len(children) - 1; index >= 0; index-- {
				name := children[index].Name()
				pending = append(pending, pendingPath{gitJoin(next.path, name), gitJoin(next.relative, name)})
			}
		} else {
			if size+uint64(metadata.Size()) < size {
				return 0, "", errors.New("nested repository byte size overflow")
			}
			size += uint64(metadata.Size())
		}
	}
	return size, core.NewFileIdentity(hex.EncodeToString(identity.Sum(nil))), nil
}

func keepRelative(repository RepositoryInput) (string, bool, error) {
	if repository.KeepDir == nil {
		return "", false, nil
	}
	root, err := snapshotAbsolute(repository.Checkout)
	if err != nil {
		return "", false, err
	}
	keep, err := snapshotAbsolute(gitJoin(root, *repository.KeepDir))
	if err != nil {
		return "", false, err
	}
	relative, ok := checkpointStripPrefix(keep, root)
	if !ok {
		return "", false, errors.New("keep directory is outside its repository")
	}
	normal := relative != ""
	for _, component := range checkpointComponents(relative) {
		if component.kind != checkpointComponentNormal {
			normal = false
		}
	}
	if !normal {
		return "", false, errors.New("keep directory must be a nonempty path inside its repository")
	}
	return relative, true, nil
}

func trackedKeepPaths(runner GitCommandRunner, checkout string, key CheckoutKey, keep string, state *string) ([]Scoped[string], error) {
	var listed []byte
	var err error
	if state != nil {
		listed, err = gitBytes(runner, checkout, []string{"ls-tree", "-r", "-z", "--name-only", *state + "^{tree}"}, nil, "inspect keep files in working state")
	} else {
		listed, err = gitBytes(runner, checkout, []string{"ls-files", "-z"}, nil, "inspect tracked keep files")
	}
	if err != nil {
		return nil, err
	}
	paths := []Scoped[string]{}
	for _, raw := range snapshotNonEmptyFields(listed, 0) {
		path, err := core.FromGitBytes(raw)
		if err != nil {
			return nil, err
		}
		if checkpointPathStartsWith(gitJoin(key.CheckoutPath, path), keep) {
			paths = append(paths, scoped(key, path))
		}
	}
	slices.SortStableFunc(paths, func(left, right Scoped[string]) int { return checkpointPathCompare(left.Value, right.Value) })
	return slices.CompactFunc(paths, scopedPathEqual), nil
}

func scopedPathEqual(left, right Scoped[string]) bool {
	return left.Checkout == right.Checkout && checkpointPathEqual(left.Value, right.Value)
}

func stripKeepPathsFromUntracked(inventory *WorkspaceInventory, keep string) {
	localKeep, ok := checkpointStripPrefix(keep, inventory.Checkout.CheckoutPath)
	switch {
	case ok && localKeep != "":
	case checkpointPathStartsWith(inventory.Checkout.CheckoutPath, keep):
		localKeep = "."
	default:
		return
	}
	inventory.Untracked = slices.DeleteFunc(inventory.Untracked, func(entry UntrackedPath) bool {
		if localKeep == "." {
			return true
		}
		return checkpointPathStartsWith(entry.Path, localKeep)
	})
}

func sortScopedPaths(paths []Scoped[string]) []Scoped[string] {
	slices.SortStableFunc(paths, func(left, right Scoped[string]) int { return checkpointPathCompare(left.Value, right.Value) })
	return slices.CompactFunc(paths, scopedPathEqual)
}

func repositoryInventory(runner GitCommandRunner, repository RepositoryInput) (WorkspaceInventory, error) {
	historyCutoff := repository.Exclusions.Kind == ExclusionSourceCommits
	originOverride := repository.OriginOverride
	if historyCutoff {
		originOverride = nil
	}
	inventory, err := workspaceInventoryScoped(runner, repository.Checkout, repository.Ownership, repositoryKey(repository.ID), originOverride, historyCutoff)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	if inventory.InPlaceOnly {
		return inventory, nil
	}
	if !inventory.Raw {
		keep, ok, err := keepRelative(repository)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		if ok {
			inventory.KeepDir = repository.KeepDir
			// `.mj/keep/<session-id>/` files are owned exclusively by
			// ScanKeepDir / keep_dir_entries / CheckKeepDirConsent so
			// preflight and dialogs never double-count them in both
			// `untracked` and `keep_dir_entries`.
			stripKeepPathsFromUntracked(&inventory, keep)
			head := "HEAD"
			tracked, err := trackedKeepPaths(runner, repository.Checkout, inventory.Checkout, keep, nil)
			if err != nil {
				return WorkspaceInventory{}, err
			}
			committed, err := trackedKeepPaths(runner, repository.Checkout, inventory.Checkout, keep, &head)
			if err != nil {
				return WorkspaceInventory{}, err
			}
			inventory.TrackedKeepFiles = sortScopedPaths(append(tracked, committed...))
			for index := range inventory.Worktrees {
				nested := &inventory.Worktrees[index]
				directory := gitJoin(repository.Checkout, nested.Path)
				nested.Inventory.KeepDir = repository.KeepDir
				stripKeepPathsFromUntracked(&nested.Inventory, keep)
				tracked, err := trackedKeepPaths(runner, directory, nested.Inventory.Checkout, keep, nil)
				if err != nil {
					return WorkspaceInventory{}, err
				}
				committed, err := trackedKeepPaths(runner, directory, nested.Inventory.Checkout, keep, &head)
				if err != nil {
					return WorkspaceInventory{}, err
				}
				nested.Inventory.TrackedKeepFiles = sortScopedPaths(append(tracked, committed...))
			}
		}
	}
	blockers, err := inventoryBlockers(&inventory)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	inventory.Blockers = blockers
	return inventory, nil
}

func WorkspaceInventoryOf(inner GitCommandRunner, checkout string, ownership CheckoutOwnership) (WorkspaceInventory, error) {
	return workspaceInventoryScoped(newCaptureGit(inner), checkout, ownership, core.DefaultCheckoutKey(), nil, false)
}

func SessionInventory(inner GitCommandRunner, repositories []RepositoryInput) (map[string]WorkspaceInventory, error) {
	var runner GitCommandRunner = newCaptureGit(inner)
	result := map[string]*WorkspaceInventory{}
	for _, repository := range repositories {
		if _, ok := result[repository.ID]; ok {
			return nil, fmt.Errorf("duplicate snapshot repository ID %s", repository.ID)
		}
		inventory, err := repositoryInventory(runner, repository)
		if err != nil {
			return nil, err
		}
		result[repository.ID] = &inventory
	}
	deletionRoots, err := sessionTeardownRoots(runner, repositories)
	if err != nil {
		return nil, err
	}
	for _, repository := range repositories {
		inventory, ok := result[repository.ID]
		if !ok {
			return nil, errors.New("missing session inventory")
		}
		if repository.Exclusions.Kind != ExclusionSourceCommits {
			if err := inventoryOriginDependencies(runner, repository.Checkout, inventory, repository.OriginOverride, deletionRoots); err != nil {
				return nil, err
			}
		}
		blockers, err := inventoryBlockers(inventory)
		if err != nil {
			return nil, err
		}
		inventory.Blockers = blockers
	}
	inventories := make(map[string]WorkspaceInventory, len(result))
	for id, inventory := range result {
		inventories[id] = *inventory
	}
	return inventories, nil
}

// RequireManagedSnapshotWithoutConsent is a read-only suspend preflight.
// Final capture independently evaluates the same policy against the
// quiesced checkout.
func RequireManagedSnapshotWithoutConsent(runner GitCommandRunner, repository RepositoryInput) error {
	inventories, err := SessionInventory(runner, []RepositoryInput{repository})
	if err != nil {
		return err
	}
	inventory, ok := inventories[repository.ID]
	if !ok {
		return errors.New("snapshot inventory is missing")
	}
	inputs, err := checkUnconditionalBlockers(&inventory)
	if err != nil {
		return err
	}
	return checkConsentInputs(false, inputs, CaptureConsent{})
}

func teardownRoots(runner GitCommandRunner, checkout string) ([]string, error) {
	checkoutRoot, err := archiveCanonicalize(checkout)
	if err != nil {
		return nil, err
	}
	gitDir, _, err := gitDirectories(runner, checkout)
	if err != nil {
		return nil, err
	}
	gitRoot, err := archiveCanonicalize(gitDir)
	if err != nil {
		return nil, err
	}
	return []string{checkoutRoot, gitRoot}, nil
}

func sessionTeardownRoots(runner GitCommandRunner, repositories []RepositoryInput) ([]string, error) {
	roots := []string{}
	for _, repository := range repositories {
		if !repository.Ownership.Raw {
			repositoryRoots, err := teardownRoots(runner, repository.Checkout)
			if err != nil {
				return nil, err
			}
			roots = append(roots, repositoryRoots...)
		}
	}
	return roots, nil
}

func localFileOriginRoute(address string) (string, error) {
	// Git decodes the entire file URL path literally, including dot
	// segments, question marks and backslashes on Unix. Url::parse
	// normalizes those.
	if runtime.GOOS == "windows" {
		address = strings.ReplaceAll(address, `\`, "/")
	}
	_, afterScheme, ok := strings.Cut(address, "://")
	if !ok {
		return "", errors.New("invalid local file origin")
	}
	authority, path, ok := strings.Cut(afterScheme, "/")
	if !ok {
		return "", errors.New("local file origin has no path")
	}
	decoded := snapshotPercentDecode("/" + path)
	if bytes.IndexByte(decoded, 0) >= 0 {
		return "", errors.New("file origin contains an invalid NUL path byte")
	}
	if runtime.GOOS != "windows" {
		return core.FromGitBytes(decoded)
	}
	text, err := gitDecodeUTF8(decoded)
	if err != nil {
		return "", fmt.Errorf("local file origin path is not UTF-8: %w", err)
	}
	drive := func(value string) bool {
		return len(value) >= 2 && (value[0] >= 'a' && value[0] <= 'z' || value[0] >= 'A' && value[0] <= 'Z') && value[1] == ':'
	}
	switch {
	case drive(authority):
		return authority + text, nil
	case strings.HasPrefix(text, "/") && drive(text[1:]):
		return text[1:], nil
	case authority != "" && !strings.EqualFold(authority, "localhost"):
		return "//" + authority + text, nil
	}
	return text, nil
}

func originRouteIsRemoved(route string, roots []string, inspected map[string]bool) (bool, error) {
	route, err := snapshotAbsolute(route)
	if err != nil {
		return false, err
	}
	key := gitComponentKey(route)
	if inspected[key] {
		return false, nil
	}
	inspected[key] = true
	prefix := ""
	for _, component := range checkpointComponents(route) {
		prefix = snapshotPushComponent(prefix, component)
		canonical, err := archiveCanonicalize(prefix)
		switch {
		case err == nil:
			if slices.ContainsFunc(roots, func(root string) bool { return checkpointPathStartsWith(canonical, root) }) {
				return true, nil
			}
		case errors.Is(err, fs.ErrNotExist):
		default:
			return false, fmt.Errorf("resolve origin route component: %w", err)
		}
		metadata, err := os.Lstat(prefix)
		switch {
		case err == nil:
			if checkpointIsSymlink(metadata) {
				target, err := os.Readlink(prefix)
				if err != nil {
					return false, err
				}
				parent, ok := checkpointParent(prefix)
				if !ok {
					return false, errors.New("origin symlink has no parent")
				}
				removed, err := originRouteIsRemoved(gitJoin(parent, target), roots, inspected)
				if err != nil {
					return false, err
				}
				if removed {
					return true, nil
				}
			}
		case errors.Is(err, fs.ErrNotExist):
		default:
			return false, fmt.Errorf("inspect origin route component: %w", err)
		}
	}
	return false, nil
}

// localOriginRoute is the filesystem path a local origin URL names, if it
// names one.
func localOriginRoute(repository string, text string) (string, bool, error) {
	transport, address := transportURL(text)
	if transport != "" {
		for _, part := range strings.Split(transport, "::") {
			if part != "" && part != "file" {
				return "", false, nil
			}
		}
	}
	if scheme, _, ok := strings.Cut(address, "://"); ok && strings.EqualFold(scheme, "file") {
		route, err := localFileOriginRoute(address)
		if err != nil {
			return "", false, err
		}
		return route, true, nil
	}
	if snapshotIsAbsolute(address) || relativeLocalRemote(address) {
		return gitJoin(repository, address), true, nil
	}
	return "", false, nil
}

// teardownPathDependency is the canonical path absolute names, and whether
// that target is itself removed, when absolute depends on a teardown root.
func teardownPathDependency(absolute string, roots []string) (string, bool, bool, error) {
	cursor := absolute
	var missing []string
	var canonical string
	for {
		resolved, err := archiveCanonicalize(cursor)
		if err == nil {
			canonical = resolved
			break
		}
		if !errors.Is(err, fs.ErrNotExist) {
			return "", false, false, fmt.Errorf("resolve local origin dependency: %w", err)
		}
		components := checkpointComponents(cursor)
		if len(components) == 0 {
			return "", false, false, errors.New("local origin has no component")
		}
		missing = append(missing, components[len(components)-1].text)
		parent, ok := checkpointParent(cursor)
		if !ok {
			return "", false, false, errors.New("local origin has no existing ancestor")
		}
		cursor = parent
	}
	for index := len(missing) - 1; index >= 0; index-- {
		switch component := missing[index]; component {
		case "..":
			if parent, ok := checkpointParent(canonical); ok {
				canonical = parent
			}
		case ".":
		default:
			canonical = gitJoin(canonical, component)
		}
	}
	targetIsRemoved := slices.ContainsFunc(roots, func(root string) bool { return checkpointPathStartsWith(canonical, root) })
	if !targetIsRemoved {
		removed, err := originRouteIsRemoved(absolute, roots, map[string]bool{})
		if err != nil {
			return "", false, false, err
		}
		if !removed {
			return "", false, false, nil
		}
	}
	if !snapshotValidUTF8(canonical) {
		return "", false, false, errors.New("origin dependency must be UTF-8; rename it before suspending")
	}
	return canonical, targetIsRemoved, true, nil
}

// localOriginProviders is a local origin's object providers, or why they
// are unavailable. Native Git expands recursive object providers. Retain
// literal alternates routes too: canonical provider names alone hide
// symlinks through teardown.
type localOriginProviders struct {
	unavailable *string
	providers   []string
	shallow     *string
}

func localOriginObjectProviders(runner GitCommandRunner, context string, origin string) (localOriginProviders, error) {
	marker := origin
	if checkpointIsDir(origin) {
		exists, err := checkpointTryExists(gitJoin(origin, ".git"))
		if err != nil {
			return localOriginProviders{}, err
		}
		if exists {
			marker = gitJoin(origin, ".git")
		}
	}
	output, err := runner.Run(context, &GitCommand{Arguments: []string{"rev-parse", "--resolve-git-dir", marker}})
	if err != nil {
		return localOriginProviders{}, err
	}
	if output.Status != 0 {
		reason := gitFailure("resolve local origin Git directory", &output).Error()
		return localOriginProviders{unavailable: &reason}, nil
	}
	private, err := absoluteGitPath(context, output.Stdout)
	if err != nil {
		return localOriginProviders{}, err
	}
	query := func(arguments ...string) ([]byte, error) {
		output, err := runner.Run(context, &GitCommand{Arguments: append([]string{"--git-dir", private}, arguments...)})
		if err != nil {
			return nil, err
		}
		if output.Status != 0 {
			return nil, gitFailure("inspect local origin object providers", &output)
		}
		return output.Stdout, nil
	}
	var shallow *string
	isShallow, err := query("rev-parse", "--is-shallow-repository")
	if err != nil {
		return localOriginProviders{}, err
	}
	if string(isShallow) == "true\n" {
		location, err := query("rev-parse", "--git-path", "shallow")
		if err != nil {
			return localOriginProviders{}, err
		}
		path, err := absoluteGitPath(context, location)
		if err != nil {
			return localOriginProviders{}, err
		}
		shallow = &path
	}
	commonOutput, err := query("rev-parse", "--git-common-dir")
	if err != nil {
		return localOriginProviders{}, err
	}
	common, err := absoluteGitPath(context, commonOutput)
	if err != nil {
		return localOriginProviders{}, err
	}
	objectsOutput, err := query("rev-parse", "--git-path", "objects")
	if err != nil {
		return localOriginProviders{}, err
	}
	objects, err := absoluteGitPath(context, objectsOutput)
	if err != nil {
		return localOriginProviders{}, err
	}
	listing, err := query("count-objects", "-v")
	if err != nil {
		return localOriginProviders{}, err
	}
	objectDirectories := []string{objects}
	for _, line := range bytes.Split(listing, []byte("\n")) {
		if path, ok := bytes.CutPrefix(line, []byte("alternate: ")); ok {
			unquoted, err := unquoteGitPath(path)
			if err != nil {
				return localOriginProviders{}, err
			}
			snapshotPathSetInsert(&objectDirectories, gitJoin(context, unquoted))
		}
	}
	slices.SortStableFunc(objectDirectories, checkpointPathCompare)
	objectDirectories = slices.CompactFunc(objectDirectories, checkpointPathEqual)
	var providers []string
	for _, path := range []string{marker, private, common, objects} {
		snapshotPathSetInsert(&providers, path)
	}
	commondir := gitJoin(private, "commondir")
	exists, err := checkpointTryExists(commondir)
	if err != nil {
		return localOriginProviders{}, err
	}
	if exists {
		snapshotPathSetInsert(&providers, commondir)
	}
	for _, directory := range objectDirectories {
		snapshotPathSetInsert(&providers, directory)
		alternates := gitJoin(directory, "info/alternates")
		entries, err := os.ReadFile(alternates)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return localOriginProviders{}, fmt.Errorf("read local origin alternates routes: %w", err)
		}
		snapshotPathSetInsert(&providers, alternates)
		for _, path := range snapshotNonEmptyFields(entries, '\n') {
			unquoted, err := unquoteGitPath(path)
			if err != nil {
				return localOriginProviders{}, fmt.Errorf("invalid alternate route in %s: %w", alternates, err)
			}
			snapshotPathSetInsert(&providers, gitJoin(directory, unquoted))
		}
	}
	return localOriginProviders{providers: providers, shallow: shallow}, nil
}

type originRepository struct {
	path           string
	key            CheckoutKey
	originOverride *string
}

type originCandidate struct {
	key          CheckoutKey
	moduleURLKey *string
	route        string
}

// snapshotTreeEntry is an inventory or a nested worktree's, with its path
// below the checkout ("" for the checkout itself).
type snapshotTreeEntry struct {
	path string
	tree *WorkspaceInventory
}

func snapshotTreesWithPaths(inventory *WorkspaceInventory) []snapshotTreeEntry {
	entries := []snapshotTreeEntry{{"", inventory}}
	for index := range inventory.Worktrees {
		entries = append(entries, snapshotTreeEntry{inventory.Worktrees[index].Path, &inventory.Worktrees[index].Inventory})
	}
	return entries
}

func inventoryOriginDependencies(runner GitCommandRunner, checkout string, inventory *WorkspaceInventory, originOverride *string, roots []string) error {
	inventory.OriginsRemovedByTeardown = nil
	if inventory.Raw {
		return nil
	}
	repositories := []originRepository{{checkout, inventory.Checkout, originOverride}}
	for _, entry := range snapshotTreesWithPaths(inventory) {
		for _, module := range entry.tree.InitializedSubmodules {
			key := CheckoutKey{
				RepositoryID: entry.tree.Checkout.RepositoryID,
				CheckoutPath: gitJoin(entry.tree.Checkout.CheckoutPath, module.Value.Path),
			}
			repositories = append(repositories, originRepository{gitJoin(gitJoin(checkout, entry.path), module.Value.Path), key, nil})
		}
	}
	var candidates []originCandidate
	for _, entry := range snapshotTreesWithPaths(inventory) {
		for _, origin := range entry.tree.uninitializedOrigins {
			configKey := origin.configKey
			candidates = append(candidates, originCandidate{
				key: CheckoutKey{
					RepositoryID: entry.tree.Checkout.RepositoryID,
					CheckoutPath: gitJoin(entry.tree.Checkout.CheckoutPath, origin.owner),
				},
				moduleURLKey: &configKey,
				route:        origin.route,
			})
		}
	}
	for _, repository := range repositories {
		output, err := runGit(runner, repository.path, []string{"remote", "get-url", "origin"}, nil)
		if err != nil {
			return err
		}
		if output.Status != 0 {
			if repository.originOverride == nil {
				continue
			}
			literalOverride, err := absoluteConfigURL(repository.path, *repository.originOverride)
			if err != nil {
				return err
			}
			output, err = runGit(runner, repository.path, []string{"ls-remote", "--get-url", literalOverride}, nil)
			if err != nil {
				return err
			}
		}
		if output.Status != 0 {
			return gitFailure("resolve effective origin", &output)
		}
		text, err := gitDecodeUTF8(bytes.TrimSuffix(output.Stdout, []byte("\n")))
		if err != nil {
			return err
		}
		local, ok, err := localOriginRoute(repository.path, text)
		if err != nil {
			return err
		}
		if ok {
			candidates = append(candidates, originCandidate{key: repository.key, route: local})
		}
	}
	providerCache := map[string][]string{}
	for _, candidate := range candidates {
		absolute, err := snapshotAbsolute(candidate.route)
		if err != nil {
			return err
		}
		canonical, targetIsRemoved, depends, err := teardownPathDependency(absolute, roots)
		if err != nil {
			return err
		}
		if depends {
			inventory.OriginsRemovedByTeardown = append(inventory.OriginsRemovedByTeardown, scoped(candidate.key, OriginDependency{
				ModuleURLKey:    candidate.moduleURLKey,
				Route:           absolute,
				CanonicalPath:   canonical,
				TargetIsRemoved: targetIsRemoved,
			}))
			continue
		}
		exists, err := checkpointTryExists(absolute)
		if err != nil {
			return err
		}
		if !exists {
			continue
		}
		cacheKey := gitComponentKey(absolute)
		providers, cached := providerCache[cacheKey]
		if !cached {
			found, err := localOriginObjectProviders(runner, checkout, absolute)
			if err != nil {
				return fmt.Errorf("cannot inspect local origin object storage at %s; repair its objects/info/alternates, or make it independent with `git -C %s repack -a -d` and remove its alternates file, then retry: %w",
					absolute, core.PosixQuote(absolute), err)
			}
			if found.unavailable != nil {
				// Only an absent/unresolvable repository uses the existing
				// offline policy. Errors after resolution must never
				// masquerade as an empty provider set.
				inventory.Notices = append(inventory.Notices, fmt.Sprintf("could not inspect local origin object storage at %s: %s", absolute, *found.unavailable))
				providers = []string{}
			} else {
				// A local shallow server advertises tips without promising
				// their parents. Never exclude using our fuller source
				// graph. Hosted origins retain the agreed complete-origin
				// assumption.
				if found.shallow != nil {
					inventory.Presence = append(inventory.Presence, scoped(candidate.key, *found.shallow))
				}
				providers = found.providers
			}
			providerCache[cacheKey] = providers
		}
		for _, provider := range providers {
			_, _, depends, err := teardownPathDependency(provider, roots)
			if err != nil {
				return err
			}
			if !depends {
				continue
			}
			canonicalOrigin, err := archiveCanonicalize(absolute)
			if err != nil {
				return err
			}
			objectProvider := provider
			inventory.OriginsRemovedByTeardown = append(inventory.OriginsRemovedByTeardown, scoped(candidate.key, OriginDependency{
				ModuleURLKey:    candidate.moduleURLKey,
				ObjectProvider:  &objectProvider,
				Route:           absolute,
				CanonicalPath:   canonicalOrigin,
				TargetIsRemoved: false,
			}))
		}
	}
	return nil
}

// inventoryCollect inventories one checkout (the inner `collect` of
// Rust's workspace_inventory_scoped).
func inventoryCollect(runner GitCommandRunner, checkout string, raw bool, nested []string, key CheckoutKey, originOverride *string, historyCutoff bool) (WorkspaceInventory, error) {
	listed, err := gitBytes(runner, checkout, []string{"ls-files", "--others", "--exclude-standard", "-z"}, nil, "list untracked files")
	if err != nil {
		return WorkspaceInventory{}, err
	}
	submodules, err := inspectSnapshotSubmodules(runner, checkout, originOverride, !raw && !historyCutoff)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	var modulePaths []string
	for _, link := range submodules.indexGitlinks {
		modulePaths = append(modulePaths, link.Path)
	}
	for _, module := range submodules.initialized {
		modulePaths = append(modulePaths, module.Path)
	}
	for _, path := range modulePaths {
		if _, err := slashPath(path); err != nil {
			return WorkspaceInventory{}, fmt.Errorf("snapshot paths must be UTF-8; rename %s before suspending: %w", path, err)
		}
	}
	var paths []string
	for _, raw := range snapshotNonEmptyFields(listed, 0) {
		path, err := core.FromGitBytes(raw)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		paths = append(paths, path)
	}
	paths = append(paths, submodules.untracked...)
	slices.SortStableFunc(paths, checkpointPathCompare)
	paths = slices.CompactFunc(paths, checkpointPathEqual)
	insideNested := func(path string) bool {
		return slices.ContainsFunc(nested, func(nested string) bool { return checkpointPathStartsWith(path, nested) })
	}
	untracked := []UntrackedPath{}
	nestedRepositories := []UntrackedPath{}
	for _, path := range submodules.unregistered {
		if insideNested(path) {
			continue
		}
		directory := gitJoin(checkout, path)
		exists, err := checkpointTryExists(directory)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		if exists {
			size, identity, err := nestedRepositoryInventory(directory)
			if err != nil {
				return WorkspaceInventory{}, err
			}
			nestedRepositories = append(nestedRepositories, UntrackedPath{Checkout: key, Path: path, Bytes: size, Identity: identity})
		}
	}
	intentPaths, err := intentToAddPaths(runner, checkout)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	intent := []Scoped[string]{}
	for _, path := range intentPaths {
		intent = append(intent, scoped(key, path))
	}
	for _, entry := range submodules.intentToAdd {
		intent = append(intent, Scoped[string]{
			Checkout: CheckoutKey{RepositoryID: key.RepositoryID, CheckoutPath: gitJoin(key.CheckoutPath, entry[0])},
			Value:    entry[1],
		})
	}
	hidden, err := HiddenTrackedPaths(runner, checkout)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	hidden = append(hidden, submodules.hiddenTrackedEdits...)
	for _, path := range paths {
		if insideNested(path) || slices.ContainsFunc(nestedRepositories, func(repository UntrackedPath) bool {
			return checkpointPathStartsWith(path, repository.Path)
		}) {
			continue
		}
		metadata, err := os.Lstat(gitJoin(checkout, path))
		if err != nil {
			return WorkspaceInventory{}, fmt.Errorf("inspect %s: %w", path, err)
		}
		var size uint64
		var identity core.FileIdentity
		if metadata.IsDir() {
			// Git collapses an embedded untracked repository to one path.
			// Consent must cover its files, not just the directory inode.
			size, identity, err = nestedRepositoryInventory(gitJoin(checkout, path))
		} else {
			size = uint64(metadata.Size())
			identity, err = FileIdentityOf(metadata)
		}
		if err != nil {
			return WorkspaceInventory{}, err
		}
		untracked = append(untracked, UntrackedPath{Checkout: key, Identity: identity, Path: path, Bytes: size})
	}
	registration := []Scoped[string]{}
	for _, path := range submodules.untrackedRegistration {
		registration = append(registration, scoped(key, path))
	}
	dirty := []Scoped[string]{}
	for _, path := range submodules.dirty {
		dirty = append(dirty, scoped(key, path))
	}
	if historyCutoff && !raw {
		for _, module := range submodules.initialized {
			entry := scoped(key, module.Path)
			if !slices.ContainsFunc(dirty, func(existing Scoped[string]) bool { return scopedPathEqual(existing, entry) }) {
				dirty = append(dirty, entry)
			}
		}
	}
	initialized := []Scoped[SnapshotSubmodule]{}
	for _, module := range submodules.initialized {
		initialized = append(initialized, scoped(key, module))
	}
	gitlinkEntries := []Scoped[SnapshotGitlink]{}
	for _, link := range submodules.indexGitlinks {
		gitlinkEntries = append(gitlinkEntries, scoped(key, link))
	}
	originMissing := false
	if !historyCutoff && originOverride == nil {
		output, err := runGit(runner, checkout, []string{"remote", "get-url", "origin"}, nil)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		originMissing = output.Status != 0
	}
	hiddenEdits := []Scoped[string]{}
	for _, path := range hidden {
		hiddenEdits = append(hiddenEdits, scoped(key, path))
	}
	operation, err := operationRecord(runner, checkout, key)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	return WorkspaceInventory{
		Notices:                        []string{},
		Checkout:                       key,
		Untracked:                      untracked,
		NestedRepositories:             nestedRepositories,
		UntrackedSubmoduleRegistration: registration,
		DirtySubmodules:                dirty,
		InitializedSubmodules:          initialized,
		IndexGitlinks:                  gitlinkEntries,
		uninitializedOrigins:           submodules.uninitializedOrigins,
		OriginMissing:                  originMissing,
		IntentToAdd:                    intent,
		HiddenTrackedEdits:             hiddenEdits,
		Operation:                      operation,
		Raw:                            raw,
		RemoteRefsStale:                submodules.remoteRefsStale,
	}, nil
}

func workspaceInventoryScoped(runner GitCommandRunner, checkout string, ownership CheckoutOwnership, key CheckoutKey, originOverride *string, historyCutoff bool) (WorkspaceInventory, error) {
	if !historyCutoff {
		inPlace, err := inPlaceOnlyMode(runner, checkout, ownership, originOverride)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		if inPlace {
			return inPlaceOnlyInventory(runner, checkout, key)
		}
	}
	// Restore shares the primary checkout's anchored origin with its linked
	// worktrees, even when their source config contains a relative URL.
	var restoreOrigin *string
	if !historyCutoff {
		primaryOrigin, err := configValues(runner, checkout, "remote.origin.url")
		if err != nil {
			return WorkspaceInventory{}, err
		}
		var origin *string
		if len(primaryOrigin) > 0 {
			origin = &primaryOrigin[len(primaryOrigin)-1]
		} else {
			origin = originOverride
		}
		if origin != nil {
			absolute, err := absoluteConfigURL(checkout, *origin)
			if err != nil {
				return WorkspaceInventory{}, err
			}
			restoreOrigin = &absolute
		}
	}
	originOverride = restoreOrigin
	nested, err := discoverLinkedWorktrees(runner, checkout)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	paths := make([]string, 0, len(nested))
	for _, tree := range nested {
		paths = append(paths, tree.relative)
	}
	for _, path := range paths {
		if _, err := slashPath(path); err != nil {
			return WorkspaceInventory{}, fmt.Errorf("snapshot paths must be UTF-8; rename %s before suspending: %w", path, err)
		}
	}
	raw := ownership.Raw
	result, err := inventoryCollect(runner, checkout, raw, paths, key, originOverride, historyCutoff)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	stash, err := selectedStash(runner, checkout, ownership)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	for _, entry := range stash {
		untracked, err := stashHasUntracked(runner, checkout, entry.Commit)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		if untracked {
			result.UntrackedStashes = append(result.UntrackedStashes, scoped(key, entry))
		}
	}
	for _, tree := range nested {
		var children []string
		for _, path := range paths {
			if child, ok := checkpointStripPrefix(path, tree.relative); ok && child != "" {
				children = append(children, child)
			}
		}
		nestedKey := CheckoutKey{RepositoryID: key.RepositoryID, CheckoutPath: gitJoin(key.CheckoutPath, tree.relative)}
		inventory, err := inventoryCollect(runner, tree.directory, raw, children, nestedKey, originOverride, historyCutoff)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		result.Worktrees = append(result.Worktrees, NestedInventory{Path: tree.relative, Inventory: inventory})
	}
	roots := []string{checkout}
	var modules []string
	for _, module := range result.InitializedSubmodules {
		modules = append(modules, gitJoin(checkout, module.Value.Path))
	}
	for _, nested := range result.Worktrees {
		root := gitJoin(checkout, nested.Path)
		roots = append(roots, root)
		for _, module := range nested.Inventory.InitializedSubmodules {
			modules = append(modules, gitJoin(root, module.Value.Path))
		}
	}
	if !raw && !historyCutoff {
		teardown, err := teardownRoots(runner, checkout)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		if err := inventoryOriginDependencies(runner, checkout, &result, originOverride, teardown); err != nil {
			return WorkspaceInventory{}, err
		}
	}
	if raw {
		result.Notices = append(result.Notices, rawNotice)
	} else {
		structure, err := inspectSnapshotGitStructure(runner, checkout, roots, modules)
		if err != nil {
			return WorkspaceInventory{}, err
		}
		for _, path := range structure.directories {
			result.Presence = append(result.Presence, scoped(key, path))
		}
		result.UnrepresentedWorktrees = nil
		for _, worktree := range structure.worktrees {
			result.UnrepresentedWorktrees = append(result.UnrepresentedWorktrees, scoped(key, worktree))
		}
	}
	blockers, err := inventoryBlockers(&result)
	if err != nil {
		return WorkspaceInventory{}, err
	}
	result.Blockers = blockers
	return result, nil
}

func requireObject(runner GitCommandRunner, destination string, name string, oid string) error {
	output, err := runGit(runner, destination, []string{"cat-file", "-e", oid + "^{object}"}, nil)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("snapshot object %s for %s is missing; origin may have rewritten history", oid, name)
	}
	return nil
}

func checkoutHead(runner GitCommandRunner, destination string, head SnapshotHead) error {
	oid := head.Commit
	if err := requireObject(runner, destination, "HEAD", oid); err != nil {
		return err
	}
	if head.Kind != core.SnapshotHeadDetached {
		if !strings.HasPrefix(head.Name, "refs/heads/") {
			return errors.New("saved HEAD is not a branch")
		}
		if _, err := gitText(runner, destination, []string{"symbolic-ref", "HEAD", head.Name}); err != nil {
			return err
		}
	} else if _, err := gitText(runner, destination, []string{"update-ref", "--no-deref", "HEAD", oid}); err != nil {
		return err
	}
	_, err := gitText(runner, destination, []string{"reset", "--hard", "--no-recurse-submodules", oid})
	return err
}

// gitPresencePaths is Git state outside the snapshot grammar, a
// presence-only teardown audit.
func gitPresencePaths(runner GitCommandRunner, checkout string) ([]string, error) {
	var paths []string
	replaced, err := gitText(runner, checkout, []string{"replace", "--list"})
	if err != nil {
		return nil, err
	}
	if replaced != "" {
		path, err := gitPath(runner, checkout, "refs/replace")
		if err != nil {
			return nil, err
		}
		paths = append(paths, path)
	}
	for _, name := range []string{"info/grafts", "shallow"} {
		path, err := gitPath(runner, checkout, name)
		if err != nil {
			return nil, err
		}
		exists, err := checkpointTryExists(path)
		if err != nil {
			return nil, err
		}
		if exists {
			paths = append(paths, path)
		}
	}
	return paths, nil
}

func restoreSubmodules(runner GitCommandRunner, checkout string, modules []SnapshotSubmodule, links *[]SnapshotGitlink, notices *[]string) error {
	if links != nil {
		// This is the superproject's own index, never flattened child
		// indexes.
		desired := *links
		current, err := snapshotIndexGitlinks(runner, checkout)
		if err != nil {
			return err
		}
		var input []byte
		for _, link := range current {
			if !slices.ContainsFunc(desired, func(saved SnapshotGitlink) bool { return checkpointPathEqual(saved.Path, link.Path) }) {
				path, err := slashPath(link.Path)
				if err != nil {
					return err
				}
				input = append(input, fmt.Sprintf("0 %s\t%s\x00", link.Commit, path)...)
			}
		}
		for _, link := range desired {
			if !slices.ContainsFunc(current, func(present SnapshotGitlink) bool {
				return checkpointPathEqual(present.Path, link.Path) && present.Commit == link.Commit
			}) {
				path, err := slashPath(link.Path)
				if err != nil {
					return err
				}
				input = append(input, fmt.Sprintf("160000 %s\t%s\x00", link.Commit, path)...)
			}
		}
		if len(input) > 0 {
			if _, err := gitBytes(runner, checkout, []string{"update-index", "-z", "--index-info"}, input, "restore saved gitlinks"); err != nil {
				return err
			}
		}
	}
	ordered := slices.Clone(modules)
	slices.SortStableFunc(ordered, func(left, right SnapshotSubmodule) int {
		return len(checkpointComponents(left.Path)) - len(checkpointComponents(right.Path))
	})
	for _, module := range ordered {
		if err := validateWorktreeRelativePath(module.Path); err != nil {
			return err
		}
		parent := ""
		depth := -1
		for _, candidate := range ordered {
			if !checkpointPathEqual(module.Path, candidate.Path) && checkpointPathStartsWith(module.Path, candidate.Path) {
				if count := len(checkpointComponents(candidate.Path)); count >= depth {
					parent, depth = candidate.Path, count
				}
			}
		}
		relative, ok := checkpointStripPrefix(module.Path, parent)
		if !ok {
			return errCheckpointPrefixNotFound
		}
		parentDirectory := gitJoin(checkout, parent)
		command := GitCommand{Arguments: []string{"--literal-pathspecs", "submodule", "update", "--init", "--", relative}}
		if err := honorRepositorySSH(runner, parentDirectory, &command); err != nil {
			return err
		}
		output, err := runner.Run(parentDirectory, &command)
		switch {
		case err != nil:
			*notices = append(*notices, fmt.Sprintf("submodule %s was not initialized: %v", module.Path, err))
		case output.Status == 0:
			directory := gitJoin(checkout, module.Path)
			restored := fetchPrerequisites(runner, directory, []string{module.Commit})
			if restored == nil {
				_, restored = gitText(runner, directory, []string{"switch", "--detach", "--", module.Commit})
			}
			if restored != nil {
				*notices = append(*notices, fmt.Sprintf("submodule %s was not restored at %s: %v", module.Path, module.Commit, restored))
			}
		default:
			*notices = append(*notices, fmt.Sprintf("submodule %s was not initialized: %v", module.Path, gitFailure("git submodule update --init", &output)))
		}
	}
	return nil
}

func fetchPrerequisites(runner GitCommandRunner, checkout string, prerequisites []string) error {
	missing, err := missingTips(runner, checkout, prerequisites)
	if err != nil {
		return err
	}
	failure := ""
	if len(missing) > 0 {
		command := GitCommand{
			Arguments: []string{
				"fetch",
				"--no-write-fetch-head",
				"--no-tags",
				"--no-prune",
				"--no-prune-tags",
				"--refmap=",
				"--no-recurse-submodules",
				"--stdin",
				"origin",
			},
			Stdin: []byte(strings.Join(missing, "\n") + "\n"),
		}
		if err := honorRepositorySSH(runner, checkout, &command); err != nil {
			return err
		}
		output, err := runner.Run(checkout, &command)
		if err != nil {
			return err
		}
		if output.Status != 0 {
			failure = gitFailure("fetch snapshot prerequisites", &output).Error()
		}
	}
	missing, err = missingTips(runner, checkout, prerequisites)
	if err != nil {
		return err
	}
	if len(missing) > 0 {
		return fmt.Errorf("snapshot prerequisites are missing: %s; origin may have rewritten history; %s", strings.Join(missing, ", "), failure)
	}
	return nil
}

// RestoreWorkspaceSnapshot applies a snapshot once to the fresh clone
// create_managed_clone made for this resume. The provisioner owns
// initialization, baseline history and publication. On failure it discards
// this staging clone; restore never reconciles old state.
func RestoreWorkspaceSnapshot(runner GitCommandRunner, snapshot WorkspaceSnapshot, destination string) ([]string, error) {
	metadata := snapshot.Metadata
	if metadata.HistoryCutoff {
		if metadata.OriginURL != nil {
			return nil, errors.New("history-cutoff snapshot must not record an origin")
		}
	} else {
		if metadata.OriginURL == nil {
			return nil, errors.New("snapshot is in-place-only; resume its raw checkout without restoring Git state")
		}
		origin := *metadata.OriginURL
		// Capture saved origin with credentials redacted; the seed keeps its
		// URL as configured. Compare both redacted, so an unchanged origin
		// keeps the seed's credentialed URL and its push settings.
		seeded, err := runGit(runner, destination, []string{"config", "--get", "remote.origin.url"}, nil)
		if err != nil {
			return nil, err
		}
		originChanged := seeded.Status != 0
		if !originChanged {
			redacted, err := redactSnapshotURL(strings.TrimRightFunc(checkpointLossy(seeded.Stdout), unicode.IsSpace))
			if err != nil {
				return nil, err
			}
			originChanged = redacted != origin
		}
		if originChanged {
			// The seed's push URL and tracking refs describe the seed's
			// remote. Keep neither under the session's origin: a push must
			// not reach the old destination, and upstreams must not resolve
			// to its commits.
			unset, err := runGit(runner, destination, []string{"config", "--local", "--unset-all", "remote.origin.pushurl"}, nil)
			if err != nil {
				return nil, err
			}
			// Exit 5: there was no push URL to remove.
			if unset.Status != 0 && unset.Status != 5 {
				return nil, fmt.Errorf("remove the seed's origin push URL: %s", strings.TrimSpace(checkpointLossy(unset.Stderr)))
			}
			// Git's own listing goes back to Git byte for byte: a ref name
			// may end in characters that text trimming would remove.
			stale, err := gitBytes(runner, destination, []string{"for-each-ref", "--format=delete %(refname)", "refs/remotes/origin/"}, nil, "list the seed's origin tracking refs")
			if err != nil {
				return nil, err
			}
			if len(stale) > 0 {
				if _, err := gitBytes(runner, destination, []string{"update-ref", "--no-deref", "--stdin"}, stale, "remove the seed's origin tracking refs"); err != nil {
					return nil, err
				}
			}
			if _, err := gitText(runner, destination, []string{"config", "--local", "--replace-all", "remote.origin.url", origin}); err != nil {
				return nil, err
			}
		}
	}
	// Tracking refs for the restored origin arrive with the next fetch;
	// until then, upstreams show as gone rather than as the seed's commits.
	notices := []string{}
	if snapshot.Pack != nil {
		if err := installSnapshotPack(runner, metadata.ObjectFormat, *snapshot.Pack, destination); err != nil {
			return nil, err
		}
	}
	if metadata.HistoryCutoff {
		missing, err := missingTips(runner, destination, metadata.Prerequisites)
		if err != nil {
			return nil, err
		}
		if len(missing) > 0 {
			return nil, fmt.Errorf("history-cutoff snapshot prerequisites are missing: %s; refusing to fetch later history", strings.Join(missing, ", "))
		}
	} else if err := fetchPrerequisites(runner, destination, metadata.Prerequisites); err != nil {
		origin := ""
		if metadata.OriginURL != nil {
			origin = *metadata.OriginURL
		}
		return nil, fmt.Errorf("restore snapshot prerequisites from origin %s: %w", core.RustDebugString(origin), err)
	}
	for _, name := range slices.Sorted(maps.Keys(metadata.Refs)) {
		if !localSnapshotRef(name) {
			return nil, fmt.Errorf("snapshot ref %s is outside its local namespaces", name)
		}
		if _, err := gitText(runner, destination, []string{"update-ref", "--no-deref", name, metadata.Refs[name]}); err != nil {
			return nil, err
		}
	}
	if !metadata.HistoryCutoff {
		for _, name := range slices.Sorted(maps.Keys(metadata.BranchTracking)) {
			tracking := metadata.BranchTracking[name]
			if _, err := gitText(runner, destination, []string{"config", "--local", "--replace-all", "branch." + name + ".remote", "origin"}); err != nil {
				return nil, err
			}
			mergeKey := "branch." + name + ".merge"
			// Replace the provisioner's default upstream; --add would
			// duplicate it. Exit 5 is Git config's normal result when the
			// key does not exist.
			cleared, err := runGit(runner, destination, []string{"config", "--local", "--unset-all", mergeKey}, nil)
			if err != nil {
				return nil, err
			}
			if cleared.Status != 0 && cleared.Status != 5 {
				return nil, gitFailure("set saved origin upstream", &cleared)
			}
			for _, merge := range tracking.Merge {
				if _, err := gitText(runner, destination, []string{"config", "--local", "--add", mergeKey, merge}); err != nil {
					return nil, err
				}
			}
		}
	}
	for index := len(metadata.Stash) - 1; index >= 0; index-- {
		if err := storeSnapshotStash(runner, destination, metadata.Stash[index]); err != nil {
			return nil, err
		}
	}
	if err := checkoutHead(runner, destination, metadata.Head); err != nil {
		return nil, err
	}
	if metadata.WorkingState != nil {
		if _, err := gitText(runner, destination, []string{"stash", "apply", "--index", *metadata.WorkingState}); err != nil {
			return nil, err
		}
	}
	gitlinksFor := func(skipped bool, links []SnapshotGitlink) *[]SnapshotGitlink {
		if skipped {
			return nil
		}
		return &links
	}
	if metadata.HistoryCutoff {
		if len(metadata.InitializedSubmodules) > 0 {
			return nil, errors.New("history-cutoff snapshot cannot restore initialized submodules without fetching later history")
		}
		if err := restoreSubmodules(runner, destination, nil, gitlinksFor(metadata.WorkingStateSkipped, metadata.IndexGitlinks), &notices); err != nil {
			return nil, err
		}
	} else if err := restoreSubmodules(runner, destination, metadata.InitializedSubmodules, gitlinksFor(metadata.WorkingStateSkipped, metadata.IndexGitlinks), &notices); err != nil {
		return nil, err
	}
	for _, tree := range metadata.Worktrees {
		if err := validateWorktreeRelativePath(tree.RelativePath); err != nil {
			return nil, err
		}
		path := gitJoin(destination, tree.RelativePath)
		output, err := runner.Run(destination, &GitCommand{Arguments: []string{"worktree", "add", "--detach", "--", path, tree.Head.Commit}})
		if err != nil {
			return nil, err
		}
		if output.Status != 0 {
			return nil, gitFailure("create saved nested worktree", &output)
		}
		if err := checkoutHead(runner, path, tree.Head); err != nil {
			return nil, err
		}
		if tree.WorkingState != nil {
			if _, err := gitText(runner, path, []string{"stash", "apply", "--index", *tree.WorkingState}); err != nil {
				return nil, err
			}
		}
		if metadata.HistoryCutoff {
			if len(tree.InitializedSubmodules) > 0 {
				return nil, errors.New("history-cutoff snapshot cannot restore initialized submodules without fetching later history")
			}
			if err := restoreSubmodules(runner, path, nil, gitlinksFor(tree.WorkingStateSkipped, tree.IndexGitlinks), &notices); err != nil {
				return nil, err
			}
		} else if err := restoreSubmodules(runner, path, tree.InitializedSubmodules, gitlinksFor(tree.WorkingStateSkipped, tree.IndexGitlinks), &notices); err != nil {
			return nil, err
		}
	}
	return notices, nil
}

// installSnapshotPack copies the pack into the destination's object store
// under its trailer hash and indexes it.
func installSnapshotPack(runner GitCommandRunner, objectFormat string, pack string, destination string) error {
	directory, err := gitPath(runner, destination, "objects/pack")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return err
	}
	source, err := os.Open(pack)
	if err != nil {
		return err
	}
	defer source.Close()
	hashBytes := int64(20)
	if objectFormat == "sha256" {
		hashBytes = 32
	}
	if _, err := source.Seek(-hashBytes, io.SeekEnd); err != nil {
		return err
	}
	trailer := make([]byte, hashBytes)
	if _, err := io.ReadFull(source, trailer); err != nil {
		return err
	}
	installed := gitJoin(directory, "pack-"+hex.EncodeToString(trailer)+".pack")
	if err := snapshotCopyFile(pack, installed); err != nil {
		return err
	}
	output, err := runner.Run(destination, &GitCommand{Arguments: []string{"index-pack", installed}})
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return gitFailure("import snapshot pack", &output)
	}
	return nil
}

// RepairWorkspaceSnapshot is called by the provisioner after the staging
// directory is renamed into place.
func RepairWorkspaceSnapshot(runner GitCommandRunner, snapshot WorkspaceSnapshot, destination string) error {
	if len(snapshot.Metadata.Worktrees) == 0 {
		return nil
	}
	arguments := []string{"worktree", "repair"}
	for _, tree := range snapshot.Metadata.Worktrees {
		if err := validateWorktreeRelativePath(tree.RelativePath); err != nil {
			return err
		}
		arguments = append(arguments, gitJoin(destination, tree.RelativePath))
	}
	output, err := runner.Run(destination, &GitCommand{Arguments: arguments})
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return gitFailure("repair published nested worktrees", &output)
	}
	return nil
}

func storeSnapshotStash(runner GitCommandRunner, checkout string, entry SnapshotStashEntry) error {
	// stash store elides an adjacent identical OID. A temporary parent
	// transition and removal of its reflog row preserve repeated entries and
	// their messages.
	current, err := runGit(runner, checkout, []string{"rev-parse", "--verify", "--quiet", "refs/stash"}, nil)
	if err != nil {
		return err
	}
	repeated := false
	if current.Status == 0 {
		text, err := gitDecodeUTF8(current.Stdout)
		if err != nil {
			return err
		}
		repeated = strings.TrimSpace(text) == entry.Commit
	}
	if repeated {
		parent, err := gitText(runner, checkout, []string{"rev-parse", entry.Commit + "^1"})
		if err != nil {
			return err
		}
		if _, err := gitText(runner, checkout, []string{"update-ref", "refs/stash", parent}); err != nil {
			return err
		}
	}
	output, err := runner.Run(checkout, &GitCommand{
		Arguments: []string{"stash", "store", "-m", entry.Message, entry.Commit},
		Env:       syntheticGitIdentity(),
	})
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return gitFailure("restore saved stash", &output)
	}
	if repeated {
		if _, err := gitText(runner, checkout, []string{"reflog", "delete", "--rewrite", "refs/stash@{1}"}); err != nil {
			return err
		}
	}
	return nil
}

func missingTips(runner GitCommandRunner, checkout string, tips []string) ([]string, error) {
	if len(tips) == 0 {
		return []string{}, nil
	}
	output, err := runner.Run(checkout, &GitCommand{
		Arguments: []string{"cat-file", "--batch-check=%(objectname) %(objecttype)"},
		Stdin:     []byte(strings.Join(tips, "\n") + "\n"),
	})
	if err != nil {
		return nil, err
	}
	if output.Status != 0 {
		return nil, gitFailure("check saved snapshot tips", &output)
	}
	text, err := gitDecodeUTF8(output.Stdout)
	if err != nil {
		return nil, err
	}
	missing := []string{}
	for _, line := range gitLines(text) {
		if oid, ok := strings.CutSuffix(line, " missing"); ok {
			missing = append(missing, oid)
		}
	}
	return missing, nil
}

// ---- helpers ----

func snapshotNonNil[T any](items []T) []T {
	if items == nil {
		return []T{}
	}
	return items
}

func snapshotNonNilMap[K comparable, V any](entries map[K]V) map[K]V {
	if entries == nil {
		return map[K]V{}
	}
	return entries
}

// snapshotMarshal encodes as serde_json does: no HTML escaping.
func snapshotMarshal(value any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// snapshotJoinObjects joins two encoded JSON objects into one, members in
// order, as serde writes a struct with a flattened field.
func snapshotJoinObjects(first []byte, second []byte) []byte {
	first = bytes.TrimSuffix(bytes.TrimSpace(first), []byte("}"))
	second = bytes.TrimPrefix(bytes.TrimSpace(second), []byte("{"))
	if bytes.HasSuffix(first, []byte("{")) {
		return append(first, second...)
	}
	if bytes.HasPrefix(second, []byte("}")) {
		return append(first, second...)
	}
	return append(append(first, ','), second...)
}

// snapshotExternalTag splits an externally tagged enum value: a bare string
// is a unit variant, an object with one member is the variant and its body.
func snapshotExternalTag(data []byte) (string, json.RawMessage, error) {
	var unit string
	if err := json.Unmarshal(data, &unit); err == nil {
		return unit, nil, nil
	}
	members, err := checkpointObject(data)
	if err != nil {
		return "", nil, err
	}
	if len(members) != 1 {
		return "", nil, errors.New("invalid type: map, expected an externally tagged enum with one variant")
	}
	for tag, body := range members {
		return tag, body, nil
	}
	return "", nil, nil
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

func snapshotValidUTF8(text string) bool {
	_, err := gitDecodeUTF8([]byte(text))
	return err == nil
}

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

// snapshotPushComponent is PathBuf::push of one component's text.
func snapshotPushComponent(prefix string, component checkpointComponent) string {
	if component.kind == checkpointComponentRootDir {
		return prefix + component.text
	}
	return gitJoin(prefix, component.text)
}

// snapshotPercentDecode is percent_encoding::percent_decode_str: `%XX`
// becomes its byte, and anything else, including a malformed escape, is
// kept as is.
func snapshotPercentDecode(text string) []byte {
	decoded := make([]byte, 0, len(text))
	for index := 0; index < len(text); index++ {
		if text[index] == '%' && index+2 < len(text) {
			high, highOK := snapshotHexValue(text[index+1])
			low, lowOK := snapshotHexValue(text[index+2])
			if highOK && lowOK {
				decoded = append(decoded, high<<4|low)
				index += 2
				continue
			}
		}
		decoded = append(decoded, text[index])
	}
	return decoded
}

func snapshotHexValue(character byte) (byte, bool) {
	switch {
	case character >= '0' && character <= '9':
		return character - '0', true
	case character >= 'a' && character <= 'f':
		return character - 'a' + 10, true
	case character >= 'A' && character <= 'F':
		return character - 'A' + 10, true
	}
	return 0, false
}

// snapshotCopyFile is fs::copy: the contents and permissions of source,
// written over destination.
func snapshotCopyFile(source string, destination string) error {
	input, err := os.Open(source)
	if err != nil {
		return err
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return err
	}
	output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, input); err != nil {
		output.Close()
		return err
	}
	if err := output.Close(); err != nil {
		return err
	}
	return os.Chmod(destination, info.Mode().Perm())
}
