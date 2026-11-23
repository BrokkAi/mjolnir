// Port of mj-tui/src/setup.rs.

// In-memory setup form. Discovery and persistence run in supervised workers.
//
// The Rust module is split into the setup_*.go files of this package, one per
// submodule (setup/draft.rs is setup_draft.go, and so on). A settings draft is
// a serde_json::Value held the way package client holds it: nil, bool,
// string, int64, uint64, float64, []any and jsonvalue.Object (see
// client/settings.go). The setupValue* helpers at the end of this file read
// and change such values the way serde_json's Value API does.

package tui

import (
	"encoding/json"
	"fmt"
	"maps"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DetectScope is Rust's `pub use mj_client::discovery::DetectScope`, which
// lib.rs re-exports from this module.
type DetectScope = client.DetectScope

const (
	DetectScopeProfiles = client.DetectScopeProfiles
	DetectScopeRuntimes = client.DetectScopeRuntimes
)

// SetupDetection is Rust's `pub use mj_client::discovery::SetupDetection`.
type SetupDetection = client.SetupDetection

// RejectedRuntime is Rust's `pub use mj_client::settings::RejectedRuntime`.
type RejectedRuntime = client.RejectedRuntime

// setupControlKind is which SetupControl variant a control is.
type setupControlKind uint8

const (
	setupControlKindRow setupControlKind = iota
	setupControlKindField
	setupControlKindSearch
	setupControlKindResults
	setupControlKindOpenSearch
	setupControlKindBack
	setupControlKindAdd
	setupControlKindRemove
	setupControlKindApply
	setupControlKindDetectProfiles
	setupControlKindDetectRuntimes
	setupControlKindDetectionResults
	setupControlKindOpenMachines
	setupControlKindManagedSetup
	setupControlKindManagedContinue
	setupControlKindSignIn
	setupControlKindInstallMbx
	setupControlKindSave
	setupControlKindCancel
)

// SetupControl identifies one control of the setup form. Rust's enum has one
// data variant, Row(u64); the rest carry nothing, so the Go form is one
// comparable struct (it keys chat.Form). Build a row with SetupControlRow
// and compare with the SetupControl* values.
type SetupControl struct {
	kind setupControlKind
	row  uint64
}

// SetupControlRow is SetupControl::Row(id): one setting row of a page.
func SetupControlRow(id uint64) SetupControl {
	return SetupControl{kind: setupControlKindRow, row: id}
}

var (
	SetupControlField   = SetupControl{kind: setupControlKindField}
	SetupControlSearch  = SetupControl{kind: setupControlKindSearch}
	SetupControlResults = SetupControl{kind: setupControlKindResults}
	// SetupControlOpenSearch is not a drawn control: the key that opens the
	// search, routed through the same interaction path as the dialog's other
	// shortcuts.
	SetupControlOpenSearch       = SetupControl{kind: setupControlKindOpenSearch}
	SetupControlBack             = SetupControl{kind: setupControlKindBack}
	SetupControlAdd              = SetupControl{kind: setupControlKindAdd}
	SetupControlRemove           = SetupControl{kind: setupControlKindRemove}
	SetupControlApply            = SetupControl{kind: setupControlKindApply}
	SetupControlDetectProfiles   = SetupControl{kind: setupControlKindDetectProfiles}
	SetupControlDetectRuntimes   = SetupControl{kind: setupControlKindDetectRuntimes}
	SetupControlDetectionResults = SetupControl{kind: setupControlKindDetectionResults}
	// SetupControlOpenMachines goes from Runtimes to the Machines page, where
	// SSH hosts are added.
	SetupControlOpenMachines    = SetupControl{kind: setupControlKindOpenMachines}
	SetupControlManagedSetup    = SetupControl{kind: setupControlKindManagedSetup}
	SetupControlManagedContinue = SetupControl{kind: setupControlKindManagedContinue}
	SetupControlSignIn          = SetupControl{kind: setupControlKindSignIn}
	SetupControlInstallMbx      = SetupControl{kind: setupControlKindInstallMbx}
	SetupControlSave            = SetupControl{kind: setupControlKindSave}
	SetupControlCancel          = SetupControl{kind: setupControlKindCancel}
)

// IsRow reports whether the control is a Row, and its id.
func (c SetupControl) IsRow() (uint64, bool) {
	return c.row, c.kind == setupControlKindRow
}

// EditorInput is Rust's `enum EditorInput { Text(TextInput), Path(PathInput) }`.
// A Text input is held as a PathInput whose completion is never used; isPath
// says which variant it is. text is Rust's Deref/DerefMut to the TextInput.
type EditorInput struct {
	isPath bool
	input  chat.PathInput
}

// editorInputText is EditorInput::Text(input).
func editorInputText(input chat.TextInput) EditorInput {
	return EditorInput{input: chat.PathInput{TextInput: input}}
}

// editorInputPath is EditorInput::Path(input).
func editorInputPath(input chat.PathInput) EditorInput {
	return EditorInput{isPath: true, input: input}
}

// text is the editor both variants deref to.
func (e *EditorInput) text() *chat.TextInput { return &e.input.TextInput }

// pathInput is the Path variant's input, or nil for a Text input.
func (e *EditorInput) pathInput() *chat.PathInput {
	if !e.isPath {
		return nil
	}
	return &e.input
}

// String is Rust's Display: the text.
func (e EditorInput) String() string { return e.input.TextInput.String() }

// Editor is the name typed for a new entry of a collection page.
type Editor struct {
	path   []string
	input  EditorInput
	adding bool
}

// SettingControl is one descriptor that owns declaration, layout and
// interaction-to-path lookup.
type SettingControl struct {
	id   SetupControl
	path []string
	kind SettingKind
}

// SettingKind is how a setting row behaves (sealed: the SettingKind*
// variants).
type SettingKind interface {
	isSettingKind()
}

type SettingKindPage struct{}

type SettingKindRemoveEntry struct{}

type SettingKindCheckbox struct{}

type SettingKindChoices struct{ values []any }

type SettingKindField struct{}

type SettingKindReadOnly struct{}

type SettingKindManagedHarness struct{ index int }

func (SettingKindPage) isSettingKind()           {}
func (SettingKindRemoveEntry) isSettingKind()    {}
func (SettingKindCheckbox) isSettingKind()       {}
func (SettingKindChoices) isSettingKind()        {}
func (SettingKindField) isSettingKind()          {}
func (SettingKindReadOnly) isSettingKind()       {}
func (SettingKindManagedHarness) isSettingKind() {}

// settingKindEqual is SettingKind's derived PartialEq.
func settingKindEqual(a, b SettingKind) bool {
	switch a := a.(type) {
	case SettingKindChoices:
		other, ok := b.(SettingKindChoices)
		return ok && slices.EqualFunc(a.values, other.values, setupValueEqual)
	}
	return a == b
}

// sameSettingKindVariant compares the variants of two kinds, ignoring their
// data (Rust's `mem::discriminant`).
func sameSettingKindVariant(a, b SettingKind) bool {
	return fmt.Sprintf("%T", a) == fmt.Sprintf("%T", b)
}

// SubagentCategories are the pages under Subagents, one list per category.
var SubagentCategories = [4]string{"primary", "straightforward", "subtle", "oracle"}

func isSubagentCategoryPage(path []string) bool {
	return len(path) == 2 && path[0] == "subagents" && slices.Contains(SubagentCategories[:], path[1])
}

func setupPageForPath(path []string) []string {
	if len(path) >= 2 && isSubagentCategoryPage(path[:2]) {
		return slices.Clone(path[:2])
	}
	return slices.Clone(client.RowPage(path))
}

func subagentRowIndex(path []string) (int, bool) {
	if len(path) < 3 || !isSubagentCategoryPage(path[:2]) {
		return 0, false
	}
	index, err := strconv.ParseUint(path[2], 10, 64)
	if err != nil {
		return 0, false
	}
	return int(index), true
}

// PendingField is a field's typing buffer: what is typed, the text last
// applied, the draft value it was read from, and why it was refused.
type PendingField struct {
	input   EditorInput
	applied string
	source  any
	err     *string
}

// setupFields is Rust's BTreeMap<Vec<String>, PendingField>: pending field
// buffers keyed by setupPathKey of their draft path. Iterate with paths(),
// which yields them in the BTreeMap's order.
type setupFields map[string]*PendingField

// setupPathKey is the key of path in setupFields. Segments are joined with
// NUL, which sorts below every other byte, so the keys sort exactly as Rust
// orders Vec<String>.
func setupPathKey(path []string) string { return strings.Join(path, "\x00") }

// setupKeyPath is the path setupPathKey made key from.
func setupKeyPath(key string) []string { return strings.Split(key, "\x00") }

func (f setupFields) get(path []string) (*PendingField, bool) {
	field, ok := f[setupPathKey(path)]
	return field, ok
}

func (f setupFields) remove(path []string) (*PendingField, bool) {
	key := setupPathKey(path)
	field, ok := f[key]
	delete(f, key)
	return field, ok
}

// paths is every buffered path in Rust's BTreeMap order.
func (f setupFields) paths() [][]string {
	keys := slices.Sorted(maps.Keys(f))
	paths := make([][]string, len(keys))
	for i, key := range keys {
		paths[i] = setupKeyPath(key)
	}
	return paths
}

// PendingSave is where Save stands: it may await target-machine path
// expansion before continuing validation (sealed: the PendingSave* variants).
type PendingSave interface {
	isPendingSave()
}

type PendingSaveIdle struct{}

type PendingSaveResolving struct {
	path  []string
	value string
	draft any
}

type PendingSaveReady struct{}

func (PendingSaveIdle) isPendingSave()      {}
func (PendingSaveResolving) isPendingSave() {}
func (PendingSaveReady) isPendingSave()     {}

// SearchState is the open search: the query, the index it filters, and the
// row the arrows are on. It replaces the page body while it is up; the page
// underneath keeps its own path so closing the search returns to it.
type SearchState struct {
	input   chat.TextInput
	entries []SearchEntry
	// matches are indices into entries, in the order they are drawn.
	matches  []int
	selected int
}

func newSearchState(draft any) SearchState {
	entries := setupSearchIndex(draft)
	return SearchState{
		input:    chat.NewTextInput(),
		entries:  entries,
		matches:  setupSearchMatches(entries, ""),
		selected: 0,
	}
}

func (s *SearchState) refilter() {
	s.matches = setupSearchMatches(s.entries, s.input.Value())
	s.selected = min(s.selected, max(len(s.matches)-1, 0))
}

func (s *SearchState) selectedEntry() (*SearchEntry, bool) {
	if s.selected < 0 || s.selected >= len(s.matches) {
		return nil, false
	}
	index := s.matches[s.selected]
	if index < 0 || index >= len(s.entries) {
		return nil, false
	}
	return &s.entries[index], true
}

// Jump is what activating a search result leaves for the caller to do,
// because Code Review is opened through its own dialog rather than as a page
// of values.
type Jump int

const (
	JumpDone Jump = iota
	JumpOpenReview
)

// SetupSize is the dialog size the draft's widest and tallest page needs.
type SetupSize struct {
	width  int
	height int
}

var managedSetupHarnesses = [6]core.HarnessKind{
	core.HarnessKindCodex,
	core.HarnessKindClaude,
	core.HarnessKindKimi,
	core.HarnessKindGrok,
	core.HarnessKindMuse,
	core.HarnessKindOpenCode,
}

// obviouslyNoInteractiveLogin: a profile whose authentication obviously needs
// no interactive login, using only configuration already in memory. Codex
// custom providers are decided by the background status read because their
// provider file is I/O.
func obviouslyNoInteractiveLogin(profile *core.HarnessProfile) bool {
	if _, ok := profile.Kind.APIKeyVariable(); ok {
		return true
	}
	if profile.Kind != core.HarnessKindClaude {
		return false
	}
	value, ok := profile.Environment.Get("CLAUDE_CODE_USE_BEDROCK")
	return ok && (value == "1" || value == "true" || value == "TRUE")
}

// SetupDialog is the Settings dialog.
type SetupDialog struct {
	generation  uint64
	original    string
	draft       any
	path        []string
	descriptors []SettingControl
	nextControl uint64
	fields      setupFields
	combo       chat.ComboBoxState[SetupControl]
	pendingSave PendingSave
	editor      *Editor
	search      *SearchState
	// managedSetup is the highlighted harness of the managed chooser, when
	// it is open.
	managedSetup     *int
	pendingLogin     *string
	reviewEditor     *ReviewSettingsDialog
	reviewValidation *ReviewSettingsValidation
	// form is Rust's RefCell<Dialog<SetupControl>>.
	form        chat.Dialog[SetupControl]
	saving      bool
	discovering bool
	// savedProfiles are the profiles that already exist in the saved
	// configuration. A profile added in this draft has no entry on disk for
	// `mj login --profile` to find.
	savedProfiles map[string]struct{}
	// savedProfileValues is the saved, expanded value of each profile. Sign
	// in targets the file on disk, so it is only offered while the draft
	// still matches that shape.
	savedProfileValues map[string]any
	// loginStatuses are saved-profile statuses, used to keep Sign in off
	// API-key profiles.
	loginStatuses map[string]core.LoginStatus
	notice        *string
	// noticeAnchor is where the notice was first shown. A status line
	// belongs to the page or prompt that set it and goes when the dialog is
	// no longer there.
	noticeAnchor *NoticeAnchor
	// detectionRejected are the runtimes the last runtimes detection turned
	// down. Stays until Settings closes so the Detection results view can be
	// read again without re-probing every machine.
	detectionRejected []RejectedRuntime
	// report is the open Detection results view and its scroll offset.
	report *ReportState
	// buildCachePreviews is one owner for each machine's cache resolution,
	// retained across navigation. Each result is guarded by the settings it
	// was requested for.
	buildCachePreviews map[string]BuildCachePreviewState
	// archiveSpacePreview is what the SessionWiki page's archive window
	// would reclaim, keyed by the number of days it was measured for.
	archiveSpacePreview *ArchiveSpacePreviewState
	// mbxInstall is the machine-wide mbx install in flight, if settings
	// initiated one.
	mbxInstall      *MbxInstallState
	preferredWidth  int
	preferredHeight int
}

// setupNoticeView is the view a notice belongs to: the page path, and
// whether a value editor or the search is open.
type setupNoticeView struct {
	path      []string
	editing   bool
	searching bool
}

func (v setupNoticeView) equal(other setupNoticeView) bool {
	return slices.Equal(v.path, other.path) && v.editing == other.editing && v.searching == other.searching
}

// NoticeAnchor is a notice and the view it was first shown in.
type NoticeAnchor struct {
	text string
	view setupNoticeView
}

// BuildCachePreviewState is one machine's build cache resolution.
type BuildCachePreviewState struct {
	// key is the target and global settings the preview was resolved from.
	// A draft edit that changes them starts a new resolution.
	key                 any
	result              BuildCachePreviewResult
	installMbxAvailable bool
}

// MbxInstallState is an mbx install settings started.
type MbxInstallState struct {
	key       any
	machineID string
	upgrading bool
}

// MbxInstallOffer is install eligibility and the exact machine draft a
// confirmation will use.
type MbxInstallOffer struct {
	machineID      string
	key            any
	upgrading      bool
	profileFile    string
	profileWarning *string
	manualPathLine *string
	machine        core.Machine
}

// ArchiveSpacePreviewState is what an archive window would reclaim.
type ArchiveSpacePreviewState struct {
	// key is the `archive_after_days` value the answer is about; nil is the
	// "Never" case, which still reports the space sessions use today.
	key    *uint32
	result ArchiveSpacePreviewResult
}

// ArchiveSpacePreviewResult is how far an archive space measurement got
// (sealed: the ArchiveSpacePreviewResult* variants).
type ArchiveSpacePreviewResult interface {
	isArchiveSpacePreviewResult()
}

type ArchiveSpacePreviewResultResolving struct{}

type ArchiveSpacePreviewResultReady struct{ value core.ArchiveSpacePreview }

type ArchiveSpacePreviewResultFailed struct{ value string }

func (ArchiveSpacePreviewResultResolving) isArchiveSpacePreviewResult() {}
func (ArchiveSpacePreviewResultReady) isArchiveSpacePreviewResult()     {}
func (ArchiveSpacePreviewResultFailed) isArchiveSpacePreviewResult()    {}

// BuildCachePreviewResult is how far a build cache resolution got (sealed:
// the BuildCachePreviewResult* variants).
type BuildCachePreviewResult interface {
	isBuildCachePreviewResult()
}

type BuildCachePreviewResultResolving struct{}

// BuildCachePreviewResultReady holds nil when the target kind cannot share a
// cache.
type BuildCachePreviewResultReady struct {
	value *core.BuildCachePreview
}

type BuildCachePreviewResultFailed struct{ value string }

func (BuildCachePreviewResultResolving) isBuildCachePreviewResult() {}
func (BuildCachePreviewResultReady) isBuildCachePreviewResult()     {}
func (BuildCachePreviewResultFailed) isBuildCachePreviewResult()    {}

// setupNextGeneration numbers each Settings dialog from 1, so a background
// reply can tell whether the dialog that asked is still the one open.
var setupNextGeneration atomic.Uint64

func newSetupDialog(config *core.Config) *SetupDialog {
	original, draft := client.DraftOpen(config)
	savedProfileValues := map[string]any{}
	if profiles, ok := setupValueObject(setupValueGet(draft, "profiles")); ok {
		for _, member := range profiles {
			savedProfileValues[member.Key] = setupValueClone(member.Value)
		}
	}
	// Start with the profiles that have no interactive login at all, so
	// Sign in is not offered before the background status read lands.
	loginStatuses := map[string]core.LoginStatus{}
	for id, profile := range config.Profiles {
		if obviouslyNoInteractiveLogin(&profile) {
			loginStatuses[id] = core.LoginStatusNotRequired{Detail: "no interactive login"}
		}
	}
	savedProfiles := make(map[string]struct{}, len(config.Profiles))
	for id := range config.Profiles {
		savedProfiles[id] = struct{}{}
	}
	preferred := preferredSize(draft)
	dialog := &SetupDialog{
		generation:         setupNextGeneration.Add(1),
		original:           setupValueDisplay(original),
		draft:              draft,
		path:               []string{},
		fields:             setupFields{},
		pendingSave:        PendingSaveIdle{},
		form:               chat.NewDialog[SetupControl](),
		savedProfiles:      savedProfiles,
		savedProfileValues: savedProfileValues,
		loginStatuses:      loginStatuses,
		buildCachePreviews: map[string]BuildCachePreviewState{},
		preferredWidth:     preferred.width,
		preferredHeight:    preferred.height,
	}
	dialog.prepare()
	return dialog
}

// page is the open page's path (Rust test support).
func (d *SetupDialog) page() []string { return d.path }

// The helpers below are serde_json's Value API over the draft's form, so
// the setup files translate line for line. They mirror the private ones in
// client/settings.go.

// setupValueGet is serde_json's `value[key]`: the member, or null when value
// is not an object or has no such member.
func setupValueGet(value any, key string) any {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil
	}
	member, _ := object.Get(key)
	return member
}

// setupValuePath is `value[k1][k2]...` with object keys.
func setupValuePath(value any, keys ...string) any {
	for _, key := range keys {
		value = setupValueGet(value, key)
	}
	return value
}

// setupValueHas is serde_json's `value.get(key).is_some()` on an object.
func setupValueHas(value any, key string) bool {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return false
	}
	_, found := object.Get(key)
	return found
}

func setupValueObject(value any) (jsonvalue.Object, bool) {
	object, ok := value.(jsonvalue.Object)
	return object, ok && object != nil
}

func setupValueArray(value any) ([]any, bool) {
	array, ok := value.([]any)
	return array, ok && array != nil
}

func setupValueString(value any) (string, bool) {
	text, ok := value.(string)
	return text, ok
}

func setupValueBool(value any) (bool, bool) {
	flag, ok := value.(bool)
	return flag, ok
}

// setupValueU64 is serde_json's as_u64: a non-negative integer.
func setupValueU64(value any) (uint64, bool) {
	switch number := value.(type) {
	case uint64:
		return number, true
	case int64:
		if number >= 0 {
			return uint64(number), true
		}
	case int:
		if number >= 0 {
			return uint64(number), true
		}
	}
	return 0, false
}

func setupValueIsObject(value any) bool { _, ok := setupValueObject(value); return ok }

func setupValueIsArray(value any) bool { _, ok := setupValueArray(value); return ok }

// setupValueEqual is serde_json's Value equality: objects compare members
// whatever their order, numbers by their class and value.
func setupValueEqual(a, b any) bool {
	switch left := a.(type) {
	case jsonvalue.Object:
		right, ok := b.(jsonvalue.Object)
		if !ok || (left == nil) != (right == nil) {
			return jsonvalue.Equal(a, b)
		}
		leftKeys, rightKeys := left.Keys(), right.Keys()
		if len(leftKeys) != len(rightKeys) {
			return false
		}
		for _, key := range leftKeys {
			leftValue, _ := left.Get(key)
			rightValue, found := right.Get(key)
			if !found || !setupValueEqual(leftValue, rightValue) {
				return false
			}
		}
		return true
	case []any:
		right, ok := b.([]any)
		if !ok || (left == nil) != (right == nil) {
			return jsonvalue.Equal(a, b)
		}
		return slices.EqualFunc(left, right, setupValueEqual)
	}
	return jsonvalue.Equal(a, b)
}

// setupValueOptionEqual is `Option<&Value> == Option<&Value>`.
func setupValueOptionEqual(a any, aOK bool, b any, bOK bool) bool {
	return aOK == bOK && (!aOK || setupValueEqual(a, b))
}

// setupValueDisplay is serde_json's Display (Value::to_string): compact
// JSON.
func setupValueDisplay(value any) string {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		panic(fmt.Sprintf("a settings value serializes: %v", err))
	}
	return string(encoded)
}

// setupValueTo is serde_json::to_value.
func setupValueTo(v any) (any, error) {
	encoded, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Parse(encoded)
}

// setupValueFrom is serde_json::from_value into target.
func setupValueFrom(value any, target any) error {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(encoded, target)
}

// setupValueMachine is serde_json::from_value::<Machine>.
func setupValueMachine(value any) (core.Machine, error) {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return nil, err
	}
	return core.UnmarshalMachineJSON(encoded)
}

// setupValueClone is Value::clone: nested objects and arrays are copied.
func setupValueClone(value any) any {
	switch typed := value.(type) {
	case jsonvalue.Object:
		return typed.Clone()
	case []any:
		if typed == nil {
			return typed
		}
		cloned := make([]any, len(typed))
		for i, item := range typed {
			cloned[i] = setupValueClone(item)
		}
		return cloned
	}
	return value
}

// setupLookup is `draft.pointer(&pointer(path))`: the value a page path
// addresses in the draft.
func setupLookup(draft any, path []string) (any, bool) {
	var value any = draft
	pointer := client.Pointer(path)
	if pointer == "" {
		return value, true
	}
	for _, token := range strings.Split(pointer[1:], "/") {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch typed := value.(type) {
		case jsonvalue.Object:
			member, found := typed.Get(token)
			if !found {
				return nil, false
			}
			value = member
		case []any:
			if strings.HasPrefix(token, "+") || (strings.HasPrefix(token, "0") && len(token) != 1) {
				return nil, false
			}
			index, err := strconv.ParseUint(token, 10, 63)
			if err != nil || index >= uint64(len(typed)) {
				return nil, false
			}
			value = typed[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// setupValueInsert is `root[k1]...[kn].as_object_mut().unwrap().insert(key,
// value)`: the object at keys gets the member, kept in place when it exists
// and appended otherwise. It panics where serde_json's unwrap would.
func setupValueInsert(root *any, keys []string, key string, value any) {
	if len(keys) == 0 {
		object, ok := (*root).(jsonvalue.Object)
		if !ok {
			panic(fmt.Sprintf("called `Option::unwrap()` on a `None` value: %s is not an object", setupValueDisplay(*root)))
		}
		object.Set(key, value)
		*root = object
		return
	}
	object, ok := (*root).(jsonvalue.Object)
	if !ok {
		if *root != nil {
			panic(fmt.Sprintf("cannot access key %q in JSON %s", keys[0], setupValueDisplay(*root)))
		}
		object = jsonvalue.Object{}
	}
	child, _ := object.Get(keys[0])
	setupValueInsert(&child, keys[1:], key, value)
	object.Set(keys[0], child)
	*root = object
}
