// Stand-in ports of the mj-core items that mj-checkpoint's snapshot.rs and
// move_stream.rs need, at Rust 3dde0c11b: suspend_consent.rs (the capture
// consent types), session_keep.rs (MoveKeep and normalize_keep_path) and the
// snapshot metadata types in archive.rs.
//
// The checkpoint-snapshot sync ported them here so its area compiles on its
// own. The core sync areas port the same items into suspend_consent.go,
// session_keep.go and archive.go. When both are merged, keep those ports,
// delete the duplicates from this file, and adjust mj-checkpoint's callers if
// a Go name differs.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"unicode/utf8"
)

// ---- suspend_consent.rs ----

// CheckoutKey names one checkout of a session repository: the repository's
// ID and the checkout's path relative to the repository root.
type CheckoutKey struct {
	RepositoryID string `json:"repository_id"`
	CheckoutPath string `json:"checkout_path"`
}

// DefaultCheckoutKey is Rust's `CheckoutKey::default()`.
func DefaultCheckoutKey() CheckoutKey {
	return CheckoutKey{RepositoryID: "workspace", CheckoutPath: ""}
}

// FileIdentity is an opaque hex identity of one file observation (serde
// transparent).
type FileIdentity string

func NewFileIdentity(hex string) FileIdentity { return FileIdentity(hex) }

func (f FileIdentity) AsStr() string { return string(f) }

// UntrackedItem is one discarded item that consent can name, externally
// tagged as serde writes it. One of UntrackedItemPath,
// UntrackedItemNestedRepository, UntrackedItemStash,
// UntrackedItemKeepDirFile; every variant is comparable with ==.
type UntrackedItem interface {
	isUntrackedItem()
}

type UntrackedItemPath struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Identity FileIdentity `json:"identity"`
}

type UntrackedItemNestedRepository struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Identity FileIdentity `json:"identity"`
}

type UntrackedItemStash struct {
	Checkout CheckoutKey `json:"checkout"`
	Commit   string      `json:"commit"`
	Message  string      `json:"message"`
}

type UntrackedItemKeepDirFile struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Identity FileIdentity `json:"identity"`
}

func (UntrackedItemPath) isUntrackedItem()             {}
func (UntrackedItemNestedRepository) isUntrackedItem() {}
func (UntrackedItemStash) isUntrackedItem()            {}
func (UntrackedItemKeepDirFile) isUntrackedItem()      {}

func (i UntrackedItemPath) MarshalJSON() ([]byte, error) {
	type plain UntrackedItemPath
	return externallyTagged("Path", plain(i))
}

func (i UntrackedItemNestedRepository) MarshalJSON() ([]byte, error) {
	type plain UntrackedItemNestedRepository
	return externallyTagged("NestedRepository", plain(i))
}

func (i UntrackedItemStash) MarshalJSON() ([]byte, error) {
	type plain UntrackedItemStash
	return externallyTagged("Stash", plain(i))
}

func (i UntrackedItemKeepDirFile) MarshalJSON() ([]byte, error) {
	type plain UntrackedItemKeepDirFile
	return externallyTagged("KeepDirFile", plain(i))
}

func externallyTagged(tag string, body any) ([]byte, error) {
	encoded, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	name, err := json.Marshal(tag)
	if err != nil {
		return nil, err
	}
	return []byte("{" + string(name) + ":" + string(encoded) + "}"), nil
}

// externalTag splits an externally tagged enum value: a bare string is a
// unit variant, an object with one member is the variant and its body.
func externalTag(data []byte) (string, json.RawMessage, error) {
	var unit string
	if err := json.Unmarshal(data, &unit); err == nil {
		return unit, nil, nil
	}
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return "", nil, err
	}
	if len(members) != 1 {
		return "", nil, errors.New("invalid type: map, expected externally tagged enum with one variant")
	}
	for tag, body := range members {
		return tag, body, nil
	}
	return "", nil, nil
}

// DecodeUntrackedItem decodes one serde-encoded UntrackedItem.
func DecodeUntrackedItem(data []byte) (UntrackedItem, error) {
	tag, body, err := externalTag(data)
	if err != nil {
		return nil, err
	}
	switch tag {
	case "Path":
		var item UntrackedItemPath
		type plain UntrackedItemPath
		err := transcriptStrictDecode(body, (*plain)(&item), false)
		return item, err
	case "NestedRepository":
		var item UntrackedItemNestedRepository
		type plain UntrackedItemNestedRepository
		err := transcriptStrictDecode(body, (*plain)(&item), false)
		return item, err
	case "Stash":
		var item UntrackedItemStash
		type plain UntrackedItemStash
		err := transcriptStrictDecode(body, (*plain)(&item), false)
		return item, err
	case "KeepDirFile":
		var item UntrackedItemKeepDirFile
		type plain UntrackedItemKeepDirFile
		err := transcriptStrictDecode(body, (*plain)(&item), false)
		return item, err
	}
	return nil, transcriptUnknownVariant(tag, "Path", "NestedRepository", "Stash", "KeepDirFile")
}

// DecodeUntrackedItems decodes a serde-encoded Vec<UntrackedItem>.
func DecodeUntrackedItems(data []byte) ([]UntrackedItem, error) {
	var raw []json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil {
		return nil, err
	}
	items := make([]UntrackedItem, 0, len(raw))
	for _, entry := range raw {
		item, err := DecodeUntrackedItem(entry)
		if err != nil {
			return nil, err
		}
		items = append(items, item)
	}
	return items, nil
}

// UntrackedConsentKind is the variant of UntrackedConsent; the zero value is
// None, Rust's default.
type UntrackedConsentKind int

const (
	UntrackedConsentNone UntrackedConsentKind = iota
	UntrackedConsentAny
	UntrackedConsentShown
)

// UntrackedConsent is consent to discard untracked items: none, any, or
// exactly the shown items.
type UntrackedConsent struct {
	Kind UntrackedConsentKind
	// Items are the shown items, for UntrackedConsentShown only.
	Items []UntrackedItem
}

func (c UntrackedConsent) IsNone() bool { return c.Kind == UntrackedConsentNone }

func (c UntrackedConsent) Covers(item UntrackedItem) bool {
	switch c.Kind {
	case UntrackedConsentAny:
		return true
	case UntrackedConsentShown:
		return slices.Contains(c.Items, item)
	}
	return false
}

func (c UntrackedConsent) MarshalJSON() ([]byte, error) {
	switch c.Kind {
	case UntrackedConsentAny:
		return []byte(`"Any"`), nil
	case UntrackedConsentShown:
		return externallyTagged("Shown", storageNonNilSlice(c.Items))
	}
	return []byte(`"None"`), nil
}

func (c *UntrackedConsent) UnmarshalJSON(data []byte) error {
	tag, body, err := externalTag(data)
	if err != nil {
		return err
	}
	switch tag {
	case "None":
		*c = UntrackedConsent{}
	case "Any":
		*c = UntrackedConsent{Kind: UntrackedConsentAny}
	case "Shown":
		items, err := DecodeUntrackedItems(body)
		if err != nil {
			return err
		}
		*c = UntrackedConsent{Kind: UntrackedConsentShown, Items: items}
	default:
		return transcriptUnknownVariant(tag, "None", "Any", "Shown")
	}
	return nil
}

// GitOperationKind is an in-progress Git operation. Its value is the serde
// variant name; String is Rust's Display.
type GitOperationKind string

const (
	GitOperationKindMerge      GitOperationKind = "Merge"
	GitOperationKindRebase     GitOperationKind = "Rebase"
	GitOperationKindCherryPick GitOperationKind = "CherryPick"
	GitOperationKindRevert     GitOperationKind = "Revert"
	GitOperationKindAm         GitOperationKind = "Am"
	GitOperationKindBisect     GitOperationKind = "Bisect"
)

func (k GitOperationKind) String() string {
	switch k {
	case GitOperationKindMerge:
		return "merge"
	case GitOperationKindRebase:
		return "rebase"
	case GitOperationKindCherryPick:
		return "cherry-pick"
	case GitOperationKindRevert:
		return "revert"
	case GitOperationKindAm:
		return "am"
	case GitOperationKindBisect:
		return "bisect"
	}
	return string(k)
}

func (k *GitOperationKind) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch kind := GitOperationKind(text); kind {
	case GitOperationKindMerge, GitOperationKindRebase, GitOperationKindCherryPick,
		GitOperationKindRevert, GitOperationKindAm, GitOperationKindBisect:
		*k = kind
		return nil
	}
	return transcriptUnknownVariant(text, "Merge", "Rebase", "CherryPick", "Revert", "Am", "Bisect")
}

// GitOperation is an operation in progress in one checkout.
type GitOperation struct {
	Checkout CheckoutKey      `json:"checkout"`
	Kind     GitOperationKind `json:"kind"`
}

func (o *GitOperation) UnmarshalJSON(data []byte) error {
	type plain GitOperation
	return transcriptStrictDecode(data, (*plain)(o), false)
}

// ShownOperation is an operation as a preflight showed it.
type ShownOperation = GitOperation

// OperationConsentKind is the variant of OperationConsent; the zero value is
// None, Rust's default.
type OperationConsentKind int

const (
	OperationConsentNone OperationConsentKind = iota
	OperationConsentAny
	OperationConsentShown
)

// OperationConsent is consent to abort in-progress operations.
type OperationConsent struct {
	Kind OperationConsentKind
	// Operations are the shown operations, for OperationConsentShown only.
	Operations []ShownOperation
}

func (c OperationConsent) IsNone() bool { return c.Kind == OperationConsentNone }

func (c OperationConsent) Covers(operation ShownOperation) bool {
	switch c.Kind {
	case OperationConsentAny:
		return true
	case OperationConsentShown:
		return slices.Contains(c.Operations, operation)
	}
	return false
}

func (c OperationConsent) MarshalJSON() ([]byte, error) {
	switch c.Kind {
	case OperationConsentAny:
		return []byte(`"Any"`), nil
	case OperationConsentShown:
		return externallyTagged("Shown", storageNonNilSlice(c.Operations))
	}
	return []byte(`"None"`), nil
}

func (c *OperationConsent) UnmarshalJSON(data []byte) error {
	tag, body, err := externalTag(data)
	if err != nil {
		return err
	}
	switch tag {
	case "None":
		*c = OperationConsent{}
	case "Any":
		*c = OperationConsent{Kind: OperationConsentAny}
	case "Shown":
		var operations []ShownOperation
		if err := json.Unmarshal(body, &operations); err != nil {
			return err
		}
		*c = OperationConsent{Kind: OperationConsentShown, Operations: storageNonNilSlice(operations)}
	default:
		return transcriptUnknownVariant(tag, "None", "Any", "Shown")
	}
	return nil
}

// CaptureConsent is what a capture may discard or abort.
type CaptureConsent struct {
	Untracked  UntrackedConsent `json:"untracked" serde:"default"`
	Operations OperationConsent `json:"operations" serde:"default"`
	// MoveKeep: older workers ignore this field and keep requiring consent
	// for these files. They cannot silently discard them because of the new
	// exemption.
	MoveKeep *MoveKeep `json:"move_keep,omitempty"`
}

// UntrackedPath is an untracked file or nested repository with its shown
// size and identity.
type UntrackedPath struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Bytes    uint64       `json:"bytes"`
	Identity FileIdentity `json:"identity"`
}

func (p *UntrackedPath) UnmarshalJSON(data []byte) error {
	type plain UntrackedPath
	return transcriptStrictDecode(data, (*plain)(p), false)
}

// KeepDirEntry is one file in a session keep directory.
type KeepDirEntry struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Bytes    uint64       `json:"bytes"`
	Identity FileIdentity `json:"identity"`
}

func (e *KeepDirEntry) UnmarshalJSON(data []byte) error {
	type plain KeepDirEntry
	return transcriptStrictDecode(data, (*plain)(e), true)
}

// FormatDroppedUntracked reports a sample of what the final checkpoint
// inventory discarded.
func FormatDroppedUntracked(items []UntrackedItem) string {
	// A summary samples paths; its count covers the entire inventory.
	sample := make([]string, 0, min(len(items), 5))
	for _, item := range items[:min(len(items), 5)] {
		switch item := item.(type) {
		case UntrackedItemPath:
			sample = append(sample, RustDebugString(item.Path))
		case UntrackedItemNestedRepository:
			sample = append(sample, RustDebugString(item.Path))
		case UntrackedItemKeepDirFile:
			sample = append(sample, RustDebugString(rustPathPush(".mj/keep", item.Path)))
		case UntrackedItemStash:
			sample = append(sample, "stash "+item.Commit[:min(len(item.Commit), 8)])
		}
	}
	return fmt.Sprintf("%d (%s)", len(items), strings.Join(sample, ", "))
}

// ---- session_keep.rs ----

// MoveKeep is the source directory and path exclusions for a cross-target
// Move.
type MoveKeep struct {
	directory string
	// skipped is sorted and deduplicated by path, as a BTreeSet<PathBuf>.
	skipped []string
}

func NewMoveKeep(directory string, skip []string) (MoveKeep, error) {
	skipped := make([]string, 0, len(skip))
	for _, path := range skip {
		normalized, err := NormalizeKeepPath(path)
		if err != nil {
			return MoveKeep{}, err
		}
		skipped = append(skipped, normalized)
	}
	slices.SortFunc(skipped, keepComparePaths)
	skipped = slices.CompactFunc(skipped, func(a, b string) bool { return keepComparePaths(a, b) == 0 })
	return MoveKeep{directory: directory, skipped: skipped}, nil
}

func (k MoveKeep) Directory() string { return k.directory }

func (k MoveKeep) SkippedPaths() []string { return slices.Clone(k.skipped) }

// RelativePath matches a path under the checkout-wide keep directory against
// the one session directory the transport sends.
func (k MoveKeep) RelativePath(keepRoot string, path string) (string, bool) {
	prefix, ok := keepStripPrefix(k.directory, keepRoot)
	if !ok {
		return "", false
	}
	return keepStripPrefix(path, prefix)
}

func (k MoveKeep) Carries(keepRoot string, path string) bool {
	relative, ok := k.RelativePath(keepRoot, path)
	return ok && !k.Skips(relative)
}

func (k MoveKeep) Skips(relative string) bool {
	for _, skip := range k.skipped {
		if _, ok := keepStripPrefix(relative, skip); ok {
			return true
		}
	}
	return false
}

func (k MoveKeep) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		Directory string   `json:"directory"`
		Skipped   []string `json:"skipped"`
	}{k.directory, storageNonNilSlice(k.skipped)})
}

func (k *MoveKeep) UnmarshalJSON(data []byte) error {
	var plain struct {
		Directory string   `json:"directory"`
		Skipped   []string `json:"skipped"`
	}
	if err := transcriptStrictDecode(data, &plain, false); err != nil {
		return err
	}
	skipped := slices.Clone(plain.Skipped)
	slices.SortFunc(skipped, keepComparePaths)
	skipped = slices.CompactFunc(skipped, func(a, b string) bool { return keepComparePaths(a, b) == 0 })
	*k = MoveKeep{directory: plain.Directory, skipped: skipped}
	return nil
}

// NormalizeKeepPath keeps a relative path's normal components and refuses
// any parent, root or prefix component.
func NormalizeKeepPath(path string) (string, error) {
	out := ""
	for _, component := range pathComponents(path) {
		switch component.kind {
		case pathComponentNormal:
			out = rustPathPush(out, component.text)
		case pathComponentCurDir:
		default:
			return "", fmt.Errorf("invalid relative path in keep stream: %s", path)
		}
	}
	if out == "" {
		return "", errors.New("empty relative path in keep stream")
	}
	return out, nil
}

// keepStripPrefix is Rust's Path::strip_prefix.
func keepStripPrefix(path string, base string) (string, bool) {
	components, prefix := pathComponents(path), pathComponents(base)
	if len(prefix) > len(components) {
		return "", false
	}
	for index := range prefix {
		if components[index].kind != prefix[index].kind || components[index].text != prefix[index].text {
			return "", false
		}
	}
	rest := ""
	for _, component := range components[len(prefix):] {
		rest = rustPathPush(rest, component.text)
	}
	return rest, true
}

// keepComparePaths is Rust's Ord for paths: by component kind, then text.
func keepComparePaths(left string, right string) int {
	a, b := pathComponents(left), pathComponents(right)
	for index := 0; index < len(a) && index < len(b); index++ {
		if a[index].kind != b[index].kind {
			return int(a[index].kind) - int(b[index].kind)
		}
		if order := strings.Compare(a[index].text, b[index].text); order != 0 {
			return order
		}
	}
	return len(a) - len(b)
}

// ---- archive.rs snapshot metadata ----

// SnapshotHead is a saved HEAD: a branch (Name is its full ref) or, with
// Detached, a commit. Serialized as serde's externally tagged
// `{"Branch":{"name":..,"commit":..}}` or `{"Detached":".."}`.
type SnapshotHead struct {
	Detached bool
	Name     string
	Commit   string
}

func NewSnapshotHeadBranch(name string, commit string) SnapshotHead {
	return SnapshotHead{Name: name, Commit: commit}
}

func NewSnapshotHeadDetached(commit string) SnapshotHead {
	return SnapshotHead{Detached: true, Commit: commit}
}

func (h SnapshotHead) MarshalJSON() ([]byte, error) {
	if h.Detached {
		return externallyTagged("Detached", h.Commit)
	}
	return externallyTagged("Branch", struct {
		Name   string `json:"name"`
		Commit string `json:"commit"`
	}{h.Name, h.Commit})
}

func (h *SnapshotHead) UnmarshalJSON(data []byte) error {
	tag, body, err := externalTag(data)
	if err != nil {
		return err
	}
	switch tag {
	case "Branch":
		var branch struct {
			Name   string `json:"name"`
			Commit string `json:"commit"`
		}
		if err := transcriptStrictDecode(body, &branch, false); err != nil {
			return err
		}
		*h = NewSnapshotHeadBranch(branch.Name, branch.Commit)
	case "Detached":
		var commit string
		if err := json.Unmarshal(body, &commit); err != nil {
			return err
		}
		*h = NewSnapshotHeadDetached(commit)
	default:
		return transcriptUnknownVariant(tag, "Branch", "Detached")
	}
	return nil
}

type SnapshotStashEntry struct {
	Commit  string `json:"commit"`
	Message string `json:"message"`
}

func (e *SnapshotStashEntry) UnmarshalJSON(data []byte) error {
	type plain SnapshotStashEntry
	return transcriptStrictDecode(data, (*plain)(e), true)
}

// snapshotPathText is snapshot_path_serde's serialized form: archive path
// text, which must already be slash-separated when read back.
func snapshotPathText(path string) (string, error) {
	return ArchivePathText(path)
}

func snapshotPathDecode(raw json.RawMessage) (string, error) {
	var text string
	if err := json.Unmarshal(raw, &text); err != nil {
		return "", err
	}
	normalized, err := ArchivePathText(text)
	if err != nil {
		return "", err
	}
	if normalized != text {
		return "", errors.New("snapshot path is not slash-separated")
	}
	return text, nil
}

type SnapshotSubmodule struct {
	Path   string `json:"path"`
	Commit string `json:"commit"`
}

func (s SnapshotSubmodule) MarshalJSON() ([]byte, error) {
	path, err := snapshotPathText(s.Path)
	if err != nil {
		return nil, err
	}
	return json.Marshal(struct {
		Path   string `json:"path"`
		Commit string `json:"commit"`
	}{path, s.Commit})
}

func (s *SnapshotSubmodule) UnmarshalJSON(data []byte) error {
	var plain struct {
		Path   json.RawMessage `json:"path"`
		Commit string          `json:"commit"`
	}
	if err := transcriptStrictDecode(data, &plain, true); err != nil {
		return err
	}
	path, err := snapshotPathDecode(plain.Path)
	if err != nil {
		return err
	}
	*s = SnapshotSubmodule{Path: path, Commit: plain.Commit}
	return nil
}

type SnapshotGitlink struct {
	Path   string `json:"path"`
	Commit string `json:"commit"`
}

func (s SnapshotGitlink) MarshalJSON() ([]byte, error) {
	path, err := snapshotPathText(s.Path)
	if err != nil {
		return nil, err
	}
	return json.Marshal(struct {
		Path   string `json:"path"`
		Commit string `json:"commit"`
	}{path, s.Commit})
}

func (s *SnapshotGitlink) UnmarshalJSON(data []byte) error {
	var plain struct {
		Path   json.RawMessage `json:"path"`
		Commit string          `json:"commit"`
	}
	if err := transcriptStrictDecode(data, &plain, true); err != nil {
		return err
	}
	path, err := snapshotPathDecode(plain.Path)
	if err != nil {
		return err
	}
	*s = SnapshotGitlink{Path: path, Commit: plain.Commit}
	return nil
}

type SnapshotWorktree struct {
	HasUncommittedState   bool                `json:"has_uncommitted_state"`
	RelativePath          string              `json:"relative_path"`
	Head                  SnapshotHead        `json:"head"`
	WorkingState          *string             `json:"working_state"`
	WorkingStateSkipped   bool                `json:"working_state_skipped"`
	InitializedSubmodules []SnapshotSubmodule `json:"initialized_submodules"`
	// IndexGitlinks are only this checkout's index; module indexes do not
	// travel.
	IndexGitlinks []SnapshotGitlink `json:"index_gitlinks"`
}

func (w SnapshotWorktree) MarshalJSON() ([]byte, error) {
	path, err := snapshotPathText(w.RelativePath)
	if err != nil {
		return nil, err
	}
	type plain SnapshotWorktree
	p := plain(w)
	p.RelativePath = path
	p.InitializedSubmodules = storageNonNilSlice(p.InitializedSubmodules)
	p.IndexGitlinks = storageNonNilSlice(p.IndexGitlinks)
	return json.Marshal(p)
}

func (w *SnapshotWorktree) UnmarshalJSON(data []byte) error {
	var plain struct {
		HasUncommittedState   bool                `json:"has_uncommitted_state"`
		RelativePath          json.RawMessage     `json:"relative_path"`
		Head                  SnapshotHead        `json:"head"`
		WorkingState          *string             `json:"working_state"`
		WorkingStateSkipped   bool                `json:"working_state_skipped"`
		InitializedSubmodules []SnapshotSubmodule `json:"initialized_submodules"`
		IndexGitlinks         []SnapshotGitlink   `json:"index_gitlinks"`
	}
	if err := transcriptStrictDecode(data, &plain, true); err != nil {
		return err
	}
	path, err := snapshotPathDecode(plain.RelativePath)
	if err != nil {
		return err
	}
	*w = SnapshotWorktree{
		HasUncommittedState:   plain.HasUncommittedState,
		RelativePath:          path,
		Head:                  plain.Head,
		WorkingState:          plain.WorkingState,
		WorkingStateSkipped:   plain.WorkingStateSkipped,
		InitializedSubmodules: storageNonNilSlice(plain.InitializedSubmodules),
		IndexGitlinks:         storageNonNilSlice(plain.IndexGitlinks),
	}
	return nil
}

type BranchTracking struct {
	Remote *string  `json:"remote"`
	Merge  []string `json:"merge"`
}

func (b BranchTracking) MarshalJSON() ([]byte, error) {
	type plain BranchTracking
	p := plain(b)
	p.Merge = storageNonNilSlice(p.Merge)
	return json.Marshal(p)
}

func (b *BranchTracking) UnmarshalJSON(data []byte) error {
	type plain BranchTracking
	return transcriptStrictDecode(data, (*plain)(b), false)
}

type WorkspaceSnapshotMetadata struct {
	// OriginURL is the configured origin URL, redacted. Nil is an
	// in-place-only raw snapshot.
	OriginURL           *string                   `json:"origin_url"`
	ObjectFormat        string                    `json:"object_format"`
	Notices             []string                  `json:"notices"`
	Head                SnapshotHead              `json:"head"`
	Refs                map[string]string         `json:"refs"`
	BranchTracking      map[string]BranchTracking `json:"branch_tracking"`
	Stash               []SnapshotStashEntry      `json:"stash"`
	WorkingState        *string                   `json:"working_state"`
	WorkingStateSkipped bool                      `json:"working_state_skipped"`
	HasUncommittedState bool                      `json:"has_uncommitted_state"`
	Worktrees           []SnapshotWorktree        `json:"worktrees"`
	// InitializedSubmodules and IndexGitlinks are only this checkout's
	// index; module indexes do not travel.
	InitializedSubmodules []SnapshotSubmodule `json:"initialized_submodules"`
	IndexGitlinks         []SnapshotGitlink   `json:"index_gitlinks"`
	RemoteRefsStale       bool                `json:"remote_refs_stale"`
	// Prerequisites are boundary commits and saved tips omitted from the
	// origin-relative pack.
	Prerequisites []string `json:"prerequisites"`
	// HistoryCutoff is true when captured relative to explicit
	// history-cutoff commits rather than an origin remote.
	HistoryCutoff bool `json:"history_cutoff,omitempty"`
}

func (m WorkspaceSnapshotMetadata) MarshalJSON() ([]byte, error) {
	type plain WorkspaceSnapshotMetadata
	p := plain(m)
	p.Notices = storageNonNilSlice(p.Notices)
	p.Refs = storageNonNilMap(p.Refs)
	p.BranchTracking = storageNonNilMap(p.BranchTracking)
	p.Stash = storageNonNilSlice(p.Stash)
	p.Worktrees = storageNonNilSlice(p.Worktrees)
	p.InitializedSubmodules = storageNonNilSlice(p.InitializedSubmodules)
	p.IndexGitlinks = storageNonNilSlice(p.IndexGitlinks)
	p.Prerequisites = storageNonNilSlice(p.Prerequisites)
	return json.Marshal(p)
}

func (m *WorkspaceSnapshotMetadata) UnmarshalJSON(data []byte) error {
	type plain WorkspaceSnapshotMetadata
	return transcriptStrictDecode(data, (*plain)(m), true)
}

// ArchivePathText is a relative path's normal components joined with `/`;
// it refuses an empty, absolute, non-normal or non-UTF-8 path.
func ArchivePathText(path string) (string, error) {
	if path == "" {
		return "", errors.New("archive path is empty")
	}
	if filepath.IsAbs(path) {
		return "", fmt.Errorf("archive path '%s' is absolute", path)
	}
	components := pathComponents(path)
	parts := make([]string, 0, len(components))
	for _, component := range components {
		if component.kind != pathComponentNormal {
			return "", fmt.Errorf("unsafe archive path '%s'", path)
		}
		if !utf8.ValidString(component.text) {
			return "", fmt.Errorf("archive path '%s' is not UTF-8", path)
		}
		parts = append(parts, component.text)
	}
	return strings.Join(parts, "/"), nil
}

// ---- url::Url::set_password(None) ----

// WithoutPassword is the URL after `set_password(None)`. ok is false where
// the url crate refuses it: a URL without a host, with an empty host, or
// with the file scheme.
func (u URL) WithoutPassword() (URL, bool) {
	if !u.parsed.hasHost || u.parsed.host == "" || u.parsed.scheme == "file" {
		return URL{}, false
	}
	redacted := *u.parsed
	redacted.password = nil
	return URL{parsed: &redacted}, true
}
