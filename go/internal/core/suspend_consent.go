// Port of mj-core/src/suspend_consent.rs.

// Typed suspend/move consent and preflight contracts shared across the
// checkpoint engine, daemon wire protocol, store, CLI, TUI, and web viewer.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
)

type CheckoutKey struct {
	RepositoryID string `json:"repository_id"`
	CheckoutPath string `json:"checkout_path"`
}

// DefaultCheckoutKey is Rust's Default: the `workspace` repository with an
// empty checkout path.
func DefaultCheckoutKey() CheckoutKey { return CheckoutKey{RepositoryID: "workspace"} }

func (k *CheckoutKey) UnmarshalJSON(data []byte) error {
	type plain CheckoutKey
	if err := agentRequireFields(data, "repository_id", "checkout_path"); err != nil {
		return err
	}
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*k = CheckoutKey(decoded)
	return nil
}

func (k CheckoutKey) equal(other CheckoutKey) bool {
	return k.RepositoryID == other.RepositoryID && comparePaths(k.CheckoutPath, other.CheckoutPath) == 0
}

// FileIdentity is a file's content identity, as hex (serde transparent).
type FileIdentity string

func NewFileIdentity(hex string) FileIdentity { return FileIdentity(hex) }

func (i FileIdentity) AsStr() string { return string(i) }

// UntrackedItemKind names an UntrackedItem variant; the values are the wire
// tags.
type UntrackedItemKind string

const (
	UntrackedItemPath             UntrackedItemKind = "Path"
	UntrackedItemNestedRepository UntrackedItemKind = "NestedRepository"
	UntrackedItemStash            UntrackedItemKind = "Stash"
	UntrackedItemKeepDirFile      UntrackedItemKind = "KeepDirFile"
)

// UntrackedItem is one thing a checkpoint would not keep. Path and Identity
// belong to Path, NestedRepository and KeepDirFile; Commit and Message to
// Stash. Serialized externally tagged, as serde writes the Rust enum.
type UntrackedItem struct {
	Kind     UntrackedItemKind
	Checkout CheckoutKey
	Path     string
	Identity FileIdentity
	Commit   string
	Message  string
}

type untrackedItemFile struct {
	Checkout *CheckoutKey  `json:"checkout"`
	Path     *string       `json:"path"`
	Identity *FileIdentity `json:"identity"`
}

type untrackedItemStash struct {
	Checkout *CheckoutKey `json:"checkout"`
	Commit   *string      `json:"commit"`
	Message  *string      `json:"message"`
}

func (i UntrackedItem) MarshalJSON() ([]byte, error) {
	var body any
	switch i.Kind {
	case UntrackedItemPath, UntrackedItemNestedRepository, UntrackedItemKeepDirFile:
		body = untrackedItemFile{Checkout: &i.Checkout, Path: &i.Path, Identity: &i.Identity}
	case UntrackedItemStash:
		body = untrackedItemStash{Checkout: &i.Checkout, Commit: &i.Commit, Message: &i.Message}
	default:
		return nil, fmt.Errorf("invalid untracked item kind %q", i.Kind)
	}
	return json.Marshal(map[string]any{string(i.Kind): body})
}

func (i *UntrackedItem) UnmarshalJSON(data []byte) error {
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return err
	}
	if len(tagged) != 1 {
		return errors.New("invalid type: expected an UntrackedItem variant")
	}
	for tag, body := range tagged {
		switch kind := UntrackedItemKind(tag); kind {
		case UntrackedItemPath, UntrackedItemNestedRepository, UntrackedItemKeepDirFile:
			var file untrackedItemFile
			if err := json.Unmarshal(body, &file); err != nil {
				return err
			}
			switch {
			case file.Checkout == nil:
				return errors.New("missing field `checkout`")
			case file.Path == nil:
				return errors.New("missing field `path`")
			case file.Identity == nil:
				return errors.New("missing field `identity`")
			}
			*i = UntrackedItem{Kind: kind, Checkout: *file.Checkout, Path: *file.Path, Identity: *file.Identity}
		case UntrackedItemStash:
			var stash untrackedItemStash
			if err := json.Unmarshal(body, &stash); err != nil {
				return err
			}
			switch {
			case stash.Checkout == nil:
				return errors.New("missing field `checkout`")
			case stash.Commit == nil:
				return errors.New("missing field `commit`")
			case stash.Message == nil:
				return errors.New("missing field `message`")
			}
			*i = UntrackedItem{Kind: kind, Checkout: *stash.Checkout, Commit: *stash.Commit, Message: *stash.Message}
		default:
			return fmt.Errorf("unknown variant `%s`, expected one of `Path`, `NestedRepository`, `Stash`, `KeepDirFile`", tag)
		}
	}
	return nil
}

// Equal is Rust's PartialEq: paths compare by component.
func (i UntrackedItem) Equal(other UntrackedItem) bool {
	return i.Kind == other.Kind && i.Checkout.equal(other.Checkout) &&
		comparePaths(i.Path, other.Path) == 0 && i.Identity == other.Identity &&
		i.Commit == other.Commit && i.Message == other.Message
}

// ConsentKind names a consent variant: none, any, or exactly what was shown.
type ConsentKind int

const (
	ConsentNone ConsentKind = iota
	ConsentAny
	ConsentShown
)

func marshalConsent[T any](kind ConsentKind, shown []T) ([]byte, error) {
	switch kind {
	case ConsentNone:
		return []byte(`"None"`), nil
	case ConsentAny:
		return []byte(`"Any"`), nil
	case ConsentShown:
		if shown == nil {
			shown = []T{}
		}
		return json.Marshal(map[string][]T{"Shown": shown})
	}
	return nil, fmt.Errorf("invalid consent kind %d", kind)
}

func unmarshalConsent[T any](data []byte) (ConsentKind, []T, error) {
	var unit string
	if err := json.Unmarshal(data, &unit); err == nil {
		switch unit {
		case "None":
			return ConsentNone, nil, nil
		case "Any":
			return ConsentAny, nil, nil
		case "Shown":
			return 0, nil, errors.New("invalid type: unit variant, expected newtype variant")
		}
		return 0, nil, fmt.Errorf("unknown variant `%s`, expected one of `None`, `Any`, `Shown`", unit)
	}
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return 0, nil, err
	}
	body, ok := tagged["Shown"]
	if len(tagged) != 1 || !ok {
		return 0, nil, errors.New("unknown variant, expected one of `None`, `Any`, `Shown`")
	}
	var shown []T
	if err := json.Unmarshal(body, &shown); err != nil {
		return 0, nil, err
	}
	if shown == nil {
		return 0, nil, errors.New("invalid type: null, expected a sequence")
	}
	return ConsentShown, shown, nil
}

// UntrackedConsent is what untracked state the person agreed to lose. The
// zero value is ConsentNone.
type UntrackedConsent struct {
	Kind  ConsentKind
	Shown []UntrackedItem
}

func (c UntrackedConsent) MarshalJSON() ([]byte, error) { return marshalConsent(c.Kind, c.Shown) }

func (c *UntrackedConsent) UnmarshalJSON(data []byte) error {
	kind, shown, err := unmarshalConsent[UntrackedItem](data)
	if err != nil {
		return err
	}
	*c = UntrackedConsent{Kind: kind, Shown: shown}
	return nil
}

func (c *UntrackedConsent) IsNone() bool { return c.Kind == ConsentNone }

func (c *UntrackedConsent) Covers(item *UntrackedItem) bool {
	switch c.Kind {
	case ConsentAny:
		return true
	case ConsentShown:
		for _, shown := range c.Shown {
			if shown.Equal(*item) {
				return true
			}
		}
	}
	return false
}

type GitOperationKind string

const (
	GitOperationKindMerge      GitOperationKind = "Merge"
	GitOperationKindRebase     GitOperationKind = "Rebase"
	GitOperationKindCherryPick GitOperationKind = "CherryPick"
	GitOperationKindRevert     GitOperationKind = "Revert"
	GitOperationKindAm         GitOperationKind = "Am"
	GitOperationKindBisect     GitOperationKind = "Bisect"
)

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
	return fmt.Errorf("unknown variant `%s`, expected one of `Merge`, `Rebase`, `CherryPick`, `Revert`, `Am`, `Bisect`", text)
}

// String is Rust's Display: the Git command's name.
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

type GitOperation struct {
	Checkout CheckoutKey      `json:"checkout"`
	Kind     GitOperationKind `json:"kind"`
}

func (o *GitOperation) UnmarshalJSON(data []byte) error {
	type plain GitOperation
	if err := agentRequireFields(data, "checkout", "kind"); err != nil {
		return err
	}
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*o = GitOperation(decoded)
	return nil
}

// ShownOperation is an in-progress Git operation the person was shown.
type ShownOperation = GitOperation

// OperationConsent is which in-progress Git operations the person agreed to
// abort. The zero value is ConsentNone.
type OperationConsent struct {
	Kind  ConsentKind
	Shown []ShownOperation
}

func (c OperationConsent) MarshalJSON() ([]byte, error) { return marshalConsent(c.Kind, c.Shown) }

func (c *OperationConsent) UnmarshalJSON(data []byte) error {
	kind, shown, err := unmarshalConsent[ShownOperation](data)
	if err != nil {
		return err
	}
	*c = OperationConsent{Kind: kind, Shown: shown}
	return nil
}

func (c *OperationConsent) IsNone() bool { return c.Kind == ConsentNone }

func (c *OperationConsent) Covers(operation *ShownOperation) bool {
	switch c.Kind {
	case ConsentAny:
		return true
	case ConsentShown:
		for _, shown := range c.Shown {
			if shown.Kind == operation.Kind && shown.Checkout.equal(operation.Checkout) {
				return true
			}
		}
	}
	return false
}

type CaptureConsent struct {
	Untracked  UntrackedConsent `json:"untracked"`
	Operations OperationConsent `json:"operations"`
	// MoveKeep: older workers ignore this field and keep requiring consent
	// for these files. They cannot silently discard them because of the new
	// exemption.
	MoveKeep *MoveKeep `json:"move_keep,omitempty"`
}

// SuspendConsent is the consent persisted on `sessions.closing_consent`
// when a suspend is accepted, and sent over the daemon wire and HTTP API.
// Serialized with deny_unknown_fields.
type SuspendConsent struct {
	DiscardUntracked    UntrackedConsent `json:"discard_untracked"`
	AbortOperations     OperationConsent `json:"abort_operations"`
	WithoutCheckpoint   bool             `json:"without_checkpoint"`
	ConfirmNoCheckpoint bool             `json:"confirm_no_checkpoint,omitempty"`
}

func (c *SuspendConsent) UnmarshalJSON(data []byte) error {
	type plain SuspendConsent
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded); err != nil {
		return err
	}
	*c = SuspendConsent(decoded)
	return nil
}

func (c *SuspendConsent) CaptureConsent() CaptureConsent {
	return CaptureConsent{
		Untracked:  UntrackedConsent{Kind: c.DiscardUntracked.Kind, Shown: append([]UntrackedItem(nil), c.DiscardUntracked.Shown...)},
		Operations: OperationConsent{Kind: c.AbortOperations.Kind, Shown: append([]ShownOperation(nil), c.AbortOperations.Shown...)},
	}
}

type UntrackedPath struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Bytes    uint64       `json:"bytes"`
	Identity FileIdentity `json:"identity"`
}

func (p *UntrackedPath) UnmarshalJSON(data []byte) error {
	type plain UntrackedPath
	if err := agentRequireFields(data, "checkout", "path", "bytes", "identity"); err != nil {
		return err
	}
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*p = UntrackedPath(decoded)
	return nil
}

// SuspendUntrackedStash is serialized with deny_unknown_fields.
type SuspendUntrackedStash struct {
	Checkout CheckoutKey `json:"checkout"`
	Commit   string      `json:"commit"`
	Message  string      `json:"message"`
}

func (s *SuspendUntrackedStash) UnmarshalJSON(data []byte) error {
	type plain SuspendUntrackedStash
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "checkout", "commit", "message"); err != nil {
		return err
	}
	*s = SuspendUntrackedStash(decoded)
	return nil
}

// KeepDirEntry is serialized with deny_unknown_fields.
type KeepDirEntry struct {
	Checkout CheckoutKey  `json:"checkout"`
	Path     string       `json:"path"`
	Bytes    uint64       `json:"bytes"`
	Identity FileIdentity `json:"identity"`
}

func (e *KeepDirEntry) UnmarshalJSON(data []byte) error {
	type plain KeepDirEntry
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "checkout", "path", "bytes", "identity"); err != nil {
		return err
	}
	*e = KeepDirEntry(decoded)
	return nil
}

// SuspendBlocker is serialized with deny_unknown_fields.
type SuspendBlocker struct {
	Kind   string   `json:"kind"`
	Paths  []string `json:"paths"`
	Remedy string   `json:"remedy"`
}

func (b SuspendBlocker) MarshalJSON() ([]byte, error) {
	type plain SuspendBlocker
	b.Paths = workerLaunchNonNilSlice(b.Paths)
	return json.Marshal(plain(b))
}

func (b *SuspendBlocker) UnmarshalJSON(data []byte) error {
	type plain SuspendBlocker
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "kind", "paths", "remedy"); err != nil {
		return err
	}
	*b = SuspendBlocker(decoded)
	return nil
}

// ExistingCheckpointSummary is serialized with deny_unknown_fields.
type ExistingCheckpointSummary struct {
	Verified   bool    `json:"verified"`
	CreatedAt  *string `json:"created_at,omitempty"`
	AgeSeconds *uint64 `json:"age_seconds,omitempty"`
}

func (s *ExistingCheckpointSummary) UnmarshalJSON(data []byte) error {
	type plain ExistingCheckpointSummary
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "verified"); err != nil {
		return err
	}
	*s = ExistingCheckpointSummary(decoded)
	return nil
}

// DryRunEstimate is serialized with deny_unknown_fields.
type DryRunEstimate struct {
	GitDeltaBytes             *uint64 `json:"git_delta_bytes,omitempty"`
	GitDeltaUnavailableReason *string `json:"git_delta_unavailable_reason,omitempty"`
	SessionStateBytes         *uint64 `json:"session_state_bytes,omitempty"`
	KeepDirBytes              uint64  `json:"keep_dir_bytes"`
	TransferRoute             *string `json:"transfer_route,omitempty"`
}

func (e *DryRunEstimate) UnmarshalJSON(data []byte) error {
	type plain DryRunEstimate
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "keep_dir_bytes"); err != nil {
		return err
	}
	*e = DryRunEstimate(decoded)
	return nil
}

// WorkerPreflightRepo is target-side input for `mj-worker suspend-preflight
// --spec -`. Serialized with deny_unknown_fields.
type WorkerPreflightRepo struct {
	ID             string    `json:"id"`
	Path           string    `json:"path"`
	Raw            bool      `json:"raw"`
	ExcludeCommits *[]string `json:"exclude_commits,omitempty"`
	KeepDir        *string   `json:"keep_dir,omitempty"`
	OriginOverride *string   `json:"origin_override,omitempty"`
}

func (r *WorkerPreflightRepo) UnmarshalJSON(data []byte) error {
	type plain WorkerPreflightRepo
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "id", "path"); err != nil {
		return err
	}
	*r = WorkerPreflightRepo(decoded)
	return nil
}

// WorkerSuspendPreflightSpec is serialized with deny_unknown_fields.
type WorkerSuspendPreflightSpec struct {
	SessionID    string                `json:"session_id"`
	Repositories []WorkerPreflightRepo `json:"repositories"`
	DryRun       bool                  `json:"dry_run"`
}

func (s WorkerSuspendPreflightSpec) MarshalJSON() ([]byte, error) {
	type plain WorkerSuspendPreflightSpec
	s.Repositories = workerLaunchNonNilSlice(s.Repositories)
	return json.Marshal(plain(s))
}

func (s *WorkerSuspendPreflightSpec) UnmarshalJSON(data []byte) error {
	type plain WorkerSuspendPreflightSpec
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "session_id", "repositories"); err != nil {
		return err
	}
	*s = WorkerSuspendPreflightSpec(decoded)
	return nil
}

// WorkerSuspendPreflightResult is serialized with deny_unknown_fields.
type WorkerSuspendPreflightResult struct {
	Untracked          []UntrackedPath         `json:"untracked"`
	NestedRepositories []UntrackedPath         `json:"nested_repositories"`
	UntrackedStashes   []SuspendUntrackedStash `json:"untracked_stashes"`
	KeepDirEntries     []KeepDirEntry          `json:"keep_dir_entries"`
	KeepDirBytes       uint64                  `json:"keep_dir_bytes"`
	Operations         []ShownOperation        `json:"operations"`
	Raw                bool                    `json:"raw"`
	Blockers           []SuspendBlocker        `json:"blockers"`
	DryRun             *DryRunEstimate         `json:"dry_run,omitempty"`
}

func (r WorkerSuspendPreflightResult) MarshalJSON() ([]byte, error) {
	type plain WorkerSuspendPreflightResult
	r.Untracked = workerLaunchNonNilSlice(r.Untracked)
	r.NestedRepositories = workerLaunchNonNilSlice(r.NestedRepositories)
	r.UntrackedStashes = workerLaunchNonNilSlice(r.UntrackedStashes)
	r.KeepDirEntries = workerLaunchNonNilSlice(r.KeepDirEntries)
	r.Operations = workerLaunchNonNilSlice(r.Operations)
	r.Blockers = workerLaunchNonNilSlice(r.Blockers)
	return json.Marshal(plain(r))
}

func (r *WorkerSuspendPreflightResult) UnmarshalJSON(data []byte) error {
	type plain WorkerSuspendPreflightResult
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded); err != nil {
		return err
	}
	*r = WorkerSuspendPreflightResult(decoded)
	return nil
}

// SuspendPreflight is serialized with deny_unknown_fields.
type SuspendPreflight struct {
	SessionID               string                     `json:"session_id"`
	Untracked               []UntrackedPath            `json:"untracked"`
	NestedRepositories      []UntrackedPath            `json:"nested_repositories"`
	UntrackedStashes        []SuspendUntrackedStash    `json:"untracked_stashes"`
	KeepDirEntries          []KeepDirEntry             `json:"keep_dir_entries"`
	KeepDirBytes            uint64                     `json:"keep_dir_bytes"`
	Operations              []ShownOperation           `json:"operations"`
	Raw                     bool                       `json:"raw"`
	Interrupting            bool                       `json:"interrupting"`
	ChildrenNotHandedBack   []string                   `json:"children_not_handed_back"`
	OwningChildrenToSuspend uint64                     `json:"owning_children_to_suspend,omitempty"`
	Blockers                []SuspendBlocker           `json:"blockers"`
	ExistingCheckpoint      *ExistingCheckpointSummary `json:"existing_checkpoint,omitempty"`
	DryRun                  *DryRunEstimate            `json:"dry_run,omitempty"`
}

func (p SuspendPreflight) MarshalJSON() ([]byte, error) {
	type plain SuspendPreflight
	p.Untracked = workerLaunchNonNilSlice(p.Untracked)
	p.NestedRepositories = workerLaunchNonNilSlice(p.NestedRepositories)
	p.UntrackedStashes = workerLaunchNonNilSlice(p.UntrackedStashes)
	p.KeepDirEntries = workerLaunchNonNilSlice(p.KeepDirEntries)
	p.Operations = workerLaunchNonNilSlice(p.Operations)
	p.ChildrenNotHandedBack = workerLaunchNonNilSlice(p.ChildrenNotHandedBack)
	p.Blockers = workerLaunchNonNilSlice(p.Blockers)
	return json.Marshal(plain(p))
}

func (p *SuspendPreflight) UnmarshalJSON(data []byte) error {
	type plain SuspendPreflight
	var decoded plain
	if err := workerLaunchDecodeStrict(data, &decoded, "session_id"); err != nil {
		return err
	}
	*p = SuspendPreflight(decoded)
	return nil
}

func OwningChildrenSuspendWarning(count uint64) (string, bool) {
	switch count {
	case 0:
		return "", false
	case 1:
		return "Suspending also suspends one child with its own environment and drops that child's untracked files.", true
	}
	return fmt.Sprintf("Suspending also suspends %d children with their own environments and drops their untracked files.", count), true
}

func (p *SuspendPreflight) HasBlockers() bool { return len(p.Blockers) > 0 }

func (p *SuspendPreflight) UntrackedCount() int {
	return len(p.Untracked) + len(p.NestedRepositories) + len(p.UntrackedStashes) + len(p.KeepDirEntries)
}

func (p *SuspendPreflight) HasWarnings() bool {
	return p.UntrackedCount() > 0 ||
		len(p.Operations) > 0 ||
		p.Raw ||
		p.Interrupting ||
		len(p.ChildrenNotHandedBack) > 0 ||
		p.OwningChildrenToSuspend > 0
}

func (p *SuspendPreflight) ShownUntrackedConsent() UntrackedConsent {
	items := make([]UntrackedItem, 0, p.UntrackedCount())
	for _, entry := range p.Untracked {
		items = append(items, UntrackedItem{Kind: UntrackedItemPath, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
	}
	for _, entry := range p.NestedRepositories {
		items = append(items, UntrackedItem{Kind: UntrackedItemNestedRepository, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
	}
	for _, entry := range p.UntrackedStashes {
		items = append(items, UntrackedItem{Kind: UntrackedItemStash, Checkout: entry.Checkout, Commit: entry.Commit, Message: entry.Message})
	}
	for _, entry := range p.KeepDirEntries {
		items = append(items, UntrackedItem{Kind: UntrackedItemKeepDirFile, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
	}
	if len(items) == 0 {
		return UntrackedConsent{}
	}
	return UntrackedConsent{Kind: ConsentShown, Shown: items}
}

func (p *SuspendPreflight) ShownOperationConsent() OperationConsent {
	if len(p.Operations) == 0 {
		return OperationConsent{}
	}
	return OperationConsent{Kind: ConsentShown, Shown: append([]ShownOperation(nil), p.Operations...)}
}

func (p *SuspendPreflight) ShownConsent() SuspendConsent {
	return SuspendConsent{
		DiscardUntracked: p.ShownUntrackedConsent(),
		AbortOperations:  p.ShownOperationConsent(),
	}
}

// suspendShortCommit is the first eight bytes of commit, as Rust's
// `&commit[..commit.len().min(8)]`.
func suspendShortCommit(commit string) string {
	return commit[:min(len(commit), 8)]
}

// UntrackedDisplayPaths is the displayable list of all untracked/discarded
// paths for UIs and CLI refusals.
func (p *SuspendPreflight) UntrackedDisplayPaths() []string {
	paths := make([]string, 0, p.UntrackedCount())
	for _, entry := range p.Untracked {
		paths = append(paths, entry.Path)
	}
	for _, entry := range p.NestedRepositories {
		paths = append(paths, entry.Path+" (nested repository)")
	}
	for _, entry := range p.UntrackedStashes {
		paths = append(paths, fmt.Sprintf("stash %s (%s)", suspendShortCommit(entry.Commit), entry.Message))
	}
	for _, entry := range p.KeepDirEntries {
		paths = append(paths, ".mj/keep/"+entry.Path)
	}
	return paths
}

// FormatDroppedUntracked reports a sample of what the final checkpoint
// inventory discarded.
func FormatDroppedUntracked(items []UntrackedItem) string {
	// A summary samples paths; its count covers the entire inventory.
	var sample []string
	for _, item := range items[:min(len(items), 5)] {
		switch item.Kind {
		case UntrackedItemPath, UntrackedItemNestedRepository:
			sample = append(sample, debugString(item.Path))
		case UntrackedItemKeepDirFile:
			sample = append(sample, debugString(rustPathPush(".mj/keep", item.Path)))
		case UntrackedItemStash:
			sample = append(sample, "stash "+suspendShortCommit(item.Commit))
		}
	}
	return fmt.Sprintf("%d (%s)", len(items), strings.Join(sample, ", "))
}

// FormatUntrackedRefusal formats the canonical agent/CLI refusal message for
// untracked files.
func FormatUntrackedRefusal(count int, firstPaths []string) string {
	return FormatMoveUntrackedRefusal(count, firstPaths) + " Ignored files are never kept."
}

func FormatMoveUntrackedRefusal(count int, firstPaths []string) string {
	preview := firstPaths[:min(len(firstPaths), 5)]
	suffix := ""
	if count > len(preview) {
		suffix = "…"
	}
	return fmt.Sprintf("%d untracked files will not be kept: %s%s `git add` (or commit) anything you want to keep, or pass `--discard-untracked`.",
		count, strings.Join(preview, ", "), suffix)
}
