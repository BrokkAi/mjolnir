// Port of mj-chat/src/components/scope.rs.

package chat

import (
	"fmt"
	"reflect"
	"slices"
	"time"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

// Focus, event routing, and interaction state shared by the controls.
//
// Events are Bubble Tea messages standing for crossterm's: a key press is a
// tea.KeyPressMsg (crossterm's Repeat kind has Key().IsRepeat set), a
// release a tea.KeyReleaseMsg, a paste a tea.PasteMsg, a resize a
// tea.WindowSizeMsg, focus loss a tea.BlurMsg, and mouse events the tea
// mouse messages (see classifyMouse).

// DoubleClickInterval is the maximum interval between completed clicks on
// the same list item.
const DoubleClickInterval = 500 * time.Millisecond

// ListActivation says how mouse activation follows the purpose of a list.
type ListActivation int

const (
	// ListActivationDoubleClick selects on one click and activates on a
	// second click (the default for pickers).
	ListActivationDoubleClick ListActivation = iota
	// ListActivationSingleClick runs a command on one completed click.
	ListActivationSingleClick
)

// clickTarget identifies what a press landed on, so the release can be
// checked against the same item of the same list contents.
type clickTarget[K comparable] struct {
	id K
	// item is the list row, tab, or popup option under the pointer;
	// hasItem is false when there is none (and item is then 0).
	item     int
	hasItem  bool
	area     uv.Rectangle
	revision uint64
}

// clickTargetsEqual compares two optional click targets as Rust compares
// `Option<ClickTarget>`.
func clickTargetsEqual[K comparable](a, b *clickTarget[K]) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// FieldEdit is a user-visible edit delivered by a [TextField].
type FieldEdit interface {
	isFieldEdit()
}

// FieldEditKey is a key event to pass to the readline editor.
type FieldEditKey struct {
	Key tea.KeyPressMsg
}

// FieldEditPaste is text pasted into the field.
type FieldEditPaste struct {
	Text string
}

// FieldEditCursor is a byte offset selected by clicking in the field.
type FieldEditCursor struct {
	Offset int
}

func (FieldEditKey) isFieldEdit()    {}
func (FieldEditPaste) isFieldEdit()  {}
func (FieldEditCursor) isFieldEdit() {}

// Interaction is the common interaction vocabulary emitted by a [Form]. A
// nil Interaction is no interaction.
type Interaction[K comparable] interface {
	isInteraction(K)
}

// InteractionActivate activates a button or submits a text field.
type InteractionActivate[K comparable] struct {
	ID K
}

// InteractionToggle toggles a checkbox or a multiple-choice row.
type InteractionToggle[K comparable] struct {
	ID K
}

// InteractionSelect selects a row or tab.
type InteractionSelect[K comparable] struct {
	ID    K
	Index int
}

// InteractionComboBoxCommit accepts the currently highlighted combobox
// option.
type InteractionComboBoxCommit[K comparable] struct {
	ID    K
	Index int
}

// InteractionComboBoxDismiss dismisses an expanded combobox without changing
// its value.
type InteractionComboBoxDismiss[K comparable] struct {
	ID K
}

// InteractionComplete requests completions for a path field.
type InteractionComplete[K comparable] struct {
	ID K
}

// InteractionPathCommit accepts the highlighted completion.
type InteractionPathCommit[K comparable] struct {
	ID    K
	Index int
}

// InteractionPathDismiss closes the completion popup without changing the
// value.
type InteractionPathDismiss[K comparable] struct {
	ID K
}

// InteractionEdit edits a text field.
type InteractionEdit[K comparable] struct {
	ID   K
	Edit FieldEdit
}

// InteractionCancel is Escape requesting dismissal of the active form.
type InteractionCancel[K comparable] struct{}

func (InteractionActivate[K]) isInteraction(K)        {}
func (InteractionToggle[K]) isInteraction(K)          {}
func (InteractionSelect[K]) isInteraction(K)          {}
func (InteractionComboBoxCommit[K]) isInteraction(K)  {}
func (InteractionComboBoxDismiss[K]) isInteraction(K) {}
func (InteractionComplete[K]) isInteraction(K)        {}
func (InteractionPathCommit[K]) isInteraction(K)      {}
func (InteractionPathDismiss[K]) isInteraction(K)     {}
func (InteractionEdit[K]) isInteraction(K)            {}
func (InteractionCancel[K]) isInteraction(K)          {}

// EventResult is a small result which preserves both whether the event was
// consumed and a typed action.
//
// The action types used with it (Interaction, ChatAction, ChatEventOutcome,
// DashboardAction) are interfaces, so a nil Action is Rust's `None`.
type EventResult[A any] struct {
	// Consumed says whether this handler took responsibility for the event.
	// An event that was not consumed is offered to the next handler.
	Consumed bool
	// Action is the screen-level action, when one was produced.
	Action A
}

// EventResultIgnored is an event which was not recognized by this form.
func EventResultIgnored[A any]() EventResult[A] {
	return EventResult[A]{}
}

// EventResultHandled is a consumed event which produced no action.
func EventResultHandled[A any]() EventResult[A] {
	return EventResult[A]{Consumed: true}
}

// EventResultWithAction is a consumed event which carries an action for the
// owner to apply.
func EventResultWithAction[A any](action A) EventResult[A] {
	return EventResult[A]{Consumed: true, Action: action}
}

// HasAction reports whether the result carries an action.
func (r EventResult[A]) HasAction() bool {
	return any(r.Action) != nil
}

// MapEventAction replaces the action while preserving whether the event was
// consumed (Rust's `EventResult::map_action`).
func MapEventAction[A, B any](result EventResult[A], mapAction func(A) B) EventResult[B] {
	mapped := EventResult[B]{Consumed: result.Consumed}
	if result.HasAction() {
		mapped.Action = mapAction(result.Action)
	}
	return mapped
}

// ControlKind is the kind of behavior associated with a registered control.
type ControlKind interface {
	isControlKind()
}

// ControlKindButton is a push button.
type ControlKindButton struct{}

// ControlKindTextField is a readline text field.
type ControlKindTextField struct{}

// ControlKindCheckbox is a boolean checkbox.
type ControlKindCheckbox struct{}

// ControlKindChoiceList is a vertically navigable list.
type ControlKindChoiceList struct {
	// Len is the number of rows.
	Len int
	// Selected is the current row.
	Selected int
}

// ControlKindComboBox is a scalar field with a popup list of choices.
type ControlKindComboBox struct {
	// Len is the number of options.
	Len int
	// Selected is the current popup cursor.
	Selected int
	// Expanded says whether the popup is expanded.
	Expanded bool
}

// ControlKindPathField is a path text field with a popup of completions.
type ControlKindPathField struct {
	// Len is the number of candidates in the popup.
	Len int
	// Selected is the current popup cursor.
	Selected int
	// Expanded says whether the popup is open.
	Expanded bool
}

// ControlKindTabs is a horizontally navigable tab strip.
type ControlKindTabs struct {
	// Len is the number of tabs.
	Len int
	// Selected is the current tab.
	Selected int
}

func (ControlKindButton) isControlKind()     {}
func (ControlKindTextField) isControlKind()  {}
func (ControlKindCheckbox) isControlKind()   {}
func (ControlKindChoiceList) isControlKind() {}
func (ControlKindComboBox) isControlKind()   {}
func (ControlKindPathField) isControlKind()  {}
func (ControlKindTabs) isControlKind()       {}

func controlKindIsButton(kind ControlKind) bool {
	_, ok := kind.(ControlKindButton)
	return ok
}

func controlKindIsField(kind ControlKind) bool {
	switch kind.(type) {
	case ControlKindTextField, ControlKindPathField:
		return true
	}
	return false
}

func controlKindIsPathField(kind ControlKind) bool {
	_, ok := kind.(ControlKindPathField)
	return ok
}

func controlKindIsCheckbox(kind ControlKind) bool {
	_, ok := kind.(ControlKindCheckbox)
	return ok
}

func controlKindIsChoiceList(kind ControlKind) bool {
	_, ok := kind.(ControlKindChoiceList)
	return ok
}

func controlKindIsComboBox(kind ControlKind) bool {
	_, ok := kind.(ControlKindComboBox)
	return ok
}

func controlKindIsTabStrip(kind ControlKind) bool {
	_, ok := kind.(ControlKindTabs)
	return ok
}

func controlKindSelected(kind ControlKind) (int, bool) {
	switch kind := kind.(type) {
	case ControlKindChoiceList:
		return kind.Selected, true
	case ControlKindComboBox:
		return kind.Selected, true
	case ControlKindPathField:
		return kind.Selected, true
	case ControlKindTabs:
		return kind.Selected, true
	}
	return 0, false
}

// controlKindIsExpandedPathField matches `ControlKind::PathField { expanded:
// true, .. }`.
func controlKindIsExpandedPathField(kind ControlKind) bool {
	path, ok := kind.(ControlKindPathField)
	return ok && path.Expanded
}

// controlKindIsExpandedComboBox matches `ControlKind::ComboBox { expanded:
// true, .. }`.
func controlKindIsExpandedComboBox(kind ControlKind) bool {
	combo, ok := kind.(ControlKindComboBox)
	return ok && combo.Expanded
}

// sameControlKindVariant compares the variants of two kinds, ignoring their
// data (Rust's `mem::discriminant`).
func sameControlKindVariant(a, b ControlKind) bool {
	return reflect.TypeOf(a) == reflect.TypeOf(b)
}

// cursorStop maps a screen column to a byte offset in a single-line field.
type cursorStop struct {
	x      int
	offset int
}

// multilineCursorStop maps a screen cell to a byte offset in a wrapped field.
type multilineCursorStop struct {
	x      int
	y      int
	offset int
}

// regionSpan maps the half-open screen columns [start, end) to an item.
type regionSpan struct {
	start int
	end   int
	index int
}

// scopeControl is one registered control (Rust's private `Control`).
type scopeControl[K comparable] struct {
	id                 K
	flag               *focusFlag
	area               uv.Rectangle
	enabled            bool
	active             bool
	kind               ControlKind
	usesVerticalKeys   bool
	cursorMap          []cursorStop
	multilineCursorMap []multilineCursorStop
	editorArea         uv.Rectangle
	regionMap          []regionSpan
	listOffset         int
	// rowMap maps display rows to items; nil entries are headings or
	// separators.
	rowMap      []*int
	rowEnabled  []bool
	popupArea   uv.Rectangle
	popupRowMap []*int
	activation  ListActivation
	contents    []string
	identity    string
	revision    uint64
}

type pointerOwnerKind uint8

const (
	pointerOwnerControl pointerOwnerKind = iota
	pointerOwnerDismiss
)

// pointerOwner is what owns a press/release gesture: a control or the
// modal dismissal target.
type pointerOwner[K comparable] struct {
	kind pointerOwnerKind
	id   K
}

type pointerHitKind uint8

const (
	pointerHitControl pointerHitKind = iota
	pointerHitDismiss
)

// pointerHit is what a pointer position lands on.
type pointerHit[K comparable] struct {
	kind pointerHitKind
	id   K
}

func newScopeControl[K comparable](id K, kind ControlKind) scopeControl[K] {
	return scopeControl[K]{
		id:      id,
		flag:    &focusFlag{},
		enabled: true,
		active:  true,
		kind:    kind,
	}
}

// navigable is rat_focus's `HasFocus::navigable` for a control.
func (c *scopeControl[K]) navigable() bool {
	return c.active && c.enabled
}

// rowIsEnabled is `row_enabled.get(row).copied().unwrap_or(true)`.
func (c *scopeControl[K]) rowIsEnabled(row int) bool {
	return row < 0 || row >= len(c.rowEnabled) || c.rowEnabled[row]
}

// rowItem is `row_map.get(row).copied().flatten()`.
func (c *scopeControl[K]) rowItem(row int) (int, bool) {
	if row < 0 || row >= len(c.rowMap) || c.rowMap[row] == nil {
		return 0, false
	}
	return *c.rowMap[row], true
}

// popupItem is `popup_row_map.get(row).copied().flatten()`.
func (c *scopeControl[K]) popupItem(row int) (int, bool) {
	if row < 0 || row >= len(c.popupRowMap) || c.popupRowMap[row] == nil {
		return 0, false
	}
	return *c.popupRowMap[row], true
}

// rowOf is the first display row standing for item.
func (c *scopeControl[K]) rowOf(item int) (int, bool) {
	for row, entry := range c.rowMap {
		if entry != nil && *entry == item {
			return row, true
		}
	}
	return 0, false
}

// focusFlag is the identity of one control in the focus tree (rat_focus's
// FocusFlag): a shared cell, compared by pointer. lost marks the leaf that
// held focus before the latest change, which is what next and prev move
// from.
type focusFlag struct {
	focused bool
	lost    bool
}

// clear drops the focus state held by the flag.
func (f *focusFlag) clear() {
	f.focused = false
	f.lost = false
}

// focusLeaf is one leaf widget registered with the focus builder.
type focusLeaf struct {
	flag      *focusFlag
	area      uv.Rectangle
	navigable bool
}

// focusTree is the part of rat_focus's Focus that Form uses: leaves in
// visual order with their navigability, one of which may hold focus.
type focusTree struct {
	leaves []focusLeaf
}

// buildFocusTree is rat_focus's `FocusBuilder::new(Some(old))` followed by
// one `leaf_widget` per leaf and `build`: flags of controls that left the
// tree lose their focus.
func buildFocusTree(old focusTree, leaves []focusLeaf) focusTree {
	for _, leaf := range old.leaves {
		kept := slices.ContainsFunc(leaves, func(next focusLeaf) bool { return next.flag == leaf.flag })
		if !kept {
			leaf.flag.clear()
		}
	}
	return focusTree{leaves: leaves}
}

// focused returns the flag holding focus, or nil.
func (t *focusTree) focused() *focusFlag {
	for _, leaf := range t.leaves {
		if leaf.flag.focused {
			return leaf.flag
		}
	}
	return nil
}

// focus moves focus to the leaf with the given flag.
func (t *focusTree) focus(flag *focusFlag) {
	index := slices.IndexFunc(t.leaves, func(leaf focusLeaf) bool { return leaf.flag == flag })
	if index < 0 {
		return
	}
	t.startChange()
	t.focusIndex(index)
}

// isValidWidget reports whether the flag belongs to a leaf of this tree.
func (t *focusTree) isValidWidget(flag *focusFlag) bool {
	return slices.ContainsFunc(t.leaves, func(leaf focusLeaf) bool { return leaf.flag == flag })
}

// next focuses the next navigable leaf, reporting whether focus changed
// (rat_focus's Focus::next: with nothing focused it focuses the first
// navigable leaf and reports true; a focused leaf that is not navigable
// keeps focus).
func (t *focusTree) next() bool {
	return t.step(true)
}

// prev focuses the previous navigable leaf, reporting whether focus
// changed.
func (t *focusTree) prev() bool {
	return t.step(false)
}

func (t *focusTree) step(forward bool) bool {
	current := slices.IndexFunc(t.leaves, func(leaf focusLeaf) bool { return leaf.flag.focused })
	if current < 0 {
		t.first()
		return true
	}
	if !t.leaves[current].navigable {
		return false
	}
	t.startChange()
	for index, leaf := range t.leaves {
		if leaf.flag.lost {
			if forward {
				t.focusIndex(t.nextNavigable(index))
			} else {
				t.focusIndex(t.prevNavigable(index))
			}
			return true
		}
	}
	if index, ok := t.firstNavigable(); ok {
		t.focusIndex(index)
		return true
	}
	return false
}

// none removes focus from every leaf.
func (t *focusTree) none() {
	t.startChange()
}

func (t *focusTree) first() {
	if index, ok := t.firstNavigable(); ok {
		t.startChange()
		t.focusIndex(index)
	}
}

// startChange is rat_focus's `__start_change(true)`: the focused leaf is
// marked lost and nothing keeps focus.
func (t *focusTree) startChange() {
	for _, leaf := range t.leaves {
		leaf.flag.lost = leaf.flag.focused
		leaf.flag.focused = false
	}
}

// focusIndex is rat_focus's `__focus(n, true)`.
func (t *focusTree) focusIndex(index int) {
	if index < 0 || index >= len(t.leaves) {
		return
	}
	flag := t.leaves[index].flag
	flag.focused = true
	if flag.lost {
		// The new focus is the old one.
		flag.lost = false
	}
}

func (t *focusTree) firstNavigable() (int, bool) {
	for index, leaf := range t.leaves {
		if leaf.navigable {
			return index, true
		}
	}
	return 0, false
}

func (t *focusTree) nextNavigable(start int) int {
	index := start
	for {
		if index+1 < len(t.leaves) {
			index++
		} else {
			index = 0
		}
		if t.leaves[index].navigable || index == start {
			return index
		}
	}
}

func (t *focusTree) prevNavigable(start int) int {
	index := start
	for {
		if index > 0 {
			index--
		} else {
			index = len(t.leaves) - 1
		}
		if t.leaves[index].navigable || index == start {
			return index
		}
	}
}

// lastClick is a completed click and the time it completed.
type lastClick[K comparable] struct {
	target clickTarget[K]
	at     time.Time
}

// formViewport is the laid-out scrolling body and the content height it was
// computed for.
type formViewport struct {
	viewport      FormViewport
	contentHeight int
}

// Form is a persistent, screen-local collection of interactive controls.
//
// Controls retain their identity when a screen redraws. Call
// [Form.BeginFrame], register controls in visual order while rendering, and
// finish with [Form.EndFrame]. The focus tree is rebuilt from the
// registrations and keeps the focused control when possible.
//
// The zero value is not ready for use; create one with [NewForm]. Copy a
// form with [Form.Clone]: a plain copy shares its controls' focus state.
type Form[K comparable] struct {
	controls          []scopeControl[K]
	order             []K
	lastOrder         []K
	focusTree         focusTree
	pointerOwner      *pointerOwner[K]
	dismissArea       uv.Rectangle
	dismissEnabled    bool
	dismissActive     bool
	pendingFocus      *K
	pressedTarget     *clickTarget[K]
	lastClick         *lastClick[K]
	eventTime         time.Time
	defaultAction     *K
	blockedActivation *K
	viewport          *formViewport
	viewportFocus     *K
	viewportReveal    bool
	viewportActive    bool
}

// NewForm creates an empty form.
func NewForm[K comparable]() Form[K] {
	return Form[K]{
		eventTime:      time.Now(),
		viewportReveal: true,
	}
}

// optionalKey copies an optional key, so the copy does not alias the
// original's storage.
func optionalKey[K comparable](id *K) *K {
	if id == nil {
		return nil
	}
	copied := *id
	return &copied
}

// optionalKeysEqual compares two optional keys as Rust compares
// `Option<K>`.
func optionalKeysEqual[K comparable](a, b *K) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// focusedKey is Focused as an optional key.
func (f *Form[K]) focusedKey() *K {
	if id, ok := f.Focused(); ok {
		return &id
	}
	return nil
}

// BeginUpdate begins a metadata update between frames, preserving the drawn
// hitboxes.
//
// The dismiss control stays active. Only rendering registers it, so
// deactivating it here would leave the title bar's × unclickable for every
// event handled between two frames. Whether it may be used is
// dismissEnabled, which the owning dialog sets during the same update.
func (f *Form[K]) BeginUpdate() {
	f.order = f.order[:0]
	for i := range f.controls {
		f.controls[i].active = false
	}
}

// BeginFrame marks the start of a render pass. Registrations from the
// previous pass are hidden.
func (f *Form[K]) BeginFrame() {
	f.BeginUpdate()
	f.viewportActive = false
	for i := range f.controls {
		control := &f.controls[i]
		control.cursorMap = nil
		control.multilineCursorMap = nil
		control.editorArea = uv.Rectangle{}
	}
}

// Viewport lays out this form's scrolling body. Focus changes reveal the
// entire half-open content row block; ordinary redraws preserve wheel
// scrolling.
func (f *Form[K]) Viewport(area uv.Rectangle, contentHeight int, focusedBlock *RowRange) FormViewport {
	// A newly focused control may not have registered in this render yet.
	focused := optionalKey(f.pendingFocus)
	if focused == nil {
		focused = f.focusedKey()
	}
	reveal := f.viewportReveal ||
		!optionalKeysEqual(f.viewportFocus, focused) ||
		(f.viewport != nil && f.viewport.viewport.areaRect() != area)
	previous := 0
	if f.viewport != nil {
		previous = f.viewport.viewport.Offset()
	}
	var block *RowRange
	if reveal {
		block = focusedBlock
	}
	viewport := NewFormViewport(area, contentHeight, previous, block)
	f.viewport = &formViewport{viewport: viewport, contentHeight: contentHeight}
	f.viewportActive = true
	f.viewportFocus = focused
	f.viewportReveal = false
	return viewport
}

// RevealFocused reveals the focused block on the next layout, even if focus
// did not change. Use after validation adds error or help rows beneath an
// existing field.
func (f *Form[K]) RevealFocused() {
	f.viewportReveal = true
}

// SetFieldUsesVerticalKeys reserves vertical arrows for a field's editor or
// screen-owned history. TextField and PathField register TextInput's
// capability automatically; custom history owners set this after rendering
// and during declaration.
func (f *Form[K]) SetFieldUsesVerticalKeys(id K, usesVerticalKeys bool) {
	if control := f.controlMut(id); control != nil {
		control.usesVerticalKeys = usesVerticalKeys
	}
}

// SetDefaultAction marks the primary action without moving keyboard focus.
func (f *Form[K]) SetDefaultAction(id K) {
	f.defaultAction = &id
}

// IsDefaultAction reports whether id is the primary action.
func (f *Form[K]) IsDefaultAction(id K) bool {
	return f.defaultAction != nil && *f.defaultAction == id
}

// SetListActivation chooses command-menu or selection-list activation for a
// declared control.
func (f *Form[K]) SetListActivation(id K, activation ListActivation) {
	if control := f.controlMut(id); control != nil {
		control.activation = activation
	}
}

// SetListContents identifies list contents independently of selection
// styling and redraws. Changing the items invalidates any gesture begun
// against the old items.
func (f *Form[K]) SetListContents(id K, contents []string) {
	if control := f.controlMut(id); control != nil && !slices.Equal(control.contents, contents) {
		control.revision++
		control.contents = contents
	}
}

// SetListIdentity identifies domain items when distinct items can have
// identical visible labels.
func (f *Form[K]) SetListIdentity(id K, identity string) {
	if control := f.controlMut(id); control != nil && control.identity != identity {
		control.revision++
		control.identity = identity
	}
}

// ControlArea returns the last registered hitbox area for id, if present.
func (f *Form[K]) ControlArea(id K) (uv.Rectangle, bool) {
	if control := f.control(id); control != nil {
		return control.area, true
	}
	return uv.Rectangle{}, false
}

// Register registers a control with its exact drawn hitbox.
func (f *Form[K]) Register(id K, kind ControlKind, area uv.Rectangle, enabled bool) {
	f.registerWithCursorMap(id, kind, area, enabled, nil)
}

// RegisterDismiss registers the non-focusable upper-left dismissal target
// for a modal.
//
// The target is independent of regular controls so a footer Cancel button
// can retain its own hitbox and keyboard focus.
func (f *Form[K]) RegisterDismiss(area uv.Rectangle, enabled bool) {
	f.dismissArea = area
	f.dismissEnabled = enabled
	f.dismissActive = true
}

// registerControl writes a full registration: what every register_* method
// sets, with the maps and popup each one supplies.
func (f *Form[K]) registerControl(id K, kind ControlKind, area uv.Rectangle, enabled bool, apply func(control *scopeControl[K])) {
	index := f.ensureControl(id, kind)
	control := &f.controls[index]
	control.area = area
	control.enabled = enabled
	control.active = true
	control.kind = kind
	control.cursorMap = nil
	control.multilineCursorMap = nil
	control.editorArea = uv.Rectangle{}
	control.regionMap = nil
	control.listOffset = 0
	control.rowMap = nil
	control.rowEnabled = nil
	control.popupArea = uv.Rectangle{}
	control.popupRowMap = nil
	apply(control)
	if !slices.Contains(f.order, id) {
		f.order = append(f.order, id)
	}
}

// registerWithCursorMap registers a control and the screen columns
// corresponding to field cursors.
func (f *Form[K]) registerWithCursorMap(id K, kind ControlKind, area uv.Rectangle, enabled bool, cursorMap []cursorStop) {
	f.registerControl(id, kind, area, enabled, func(control *scopeControl[K]) {
		control.cursorMap = cursorMap
		if controlKindIsField(kind) {
			control.editorArea = area
		}
	})
}

func (f *Form[K]) registerWithRegions(id K, kind ControlKind, area uv.Rectangle, enabled bool, regionMap []regionSpan) {
	f.registerControl(id, kind, area, enabled, func(control *scopeControl[K]) {
		control.regionMap = regionMap
	})
}

func (f *Form[K]) registerWithRows(id K, kind ControlKind, area uv.Rectangle, enabled bool, rowMap []*int, rowEnabled []bool) {
	f.registerControl(id, kind, area, enabled, func(control *scopeControl[K]) {
		control.rowMap = rowMap
		control.rowEnabled = rowEnabled
	})
}

// registerInlineEditor registers the editor within a compound control
// without adding a tab stop.
func (f *Form[K]) registerInlineEditor(id K, area uv.Rectangle, cursorMap []cursorStop) {
	if control := f.controlMut(id); control != nil {
		control.editorArea = area
		control.cursorMap = cursorMap
		control.multilineCursorMap = nil
	}
}

// registerCombobox registers a scalar combobox field and, when expanded, its
// popup hitbox.
func (f *Form[K]) registerCombobox(id K, kind ControlKind, area uv.Rectangle, enabled bool, popupArea uv.Rectangle, popupRowMap []*int) {
	f.registerControl(id, kind, area, enabled, func(control *scopeControl[K]) {
		control.popupArea = popupArea
		control.popupRowMap = popupRowMap
	})
}

// registerPopup registers a popup anchored to a control that has already
// registered itself. Unlike registerCombobox this only adds the popup
// geometry, so a path field keeps the cursor map its text editor just
// registered.
func (f *Form[K]) registerPopup(id K, popupArea uv.Rectangle, popupRowMap []*int) {
	if control := f.controlMut(id); control != nil {
		control.popupArea = popupArea
		control.popupRowMap = popupRowMap
	}
}

// registerWithMultilineCursorMap registers screen cells corresponding to
// cursors in a wrapped field.
func (f *Form[K]) registerWithMultilineCursorMap(id K, kind ControlKind, area uv.Rectangle, enabled bool, cursorMap []multilineCursorStop) {
	f.registerControl(id, kind, area, enabled, func(control *scopeControl[K]) {
		control.multilineCursorMap = cursorMap
		if controlKindIsField(kind) {
			control.editorArea = area
		}
	})
}

// RegisterColumns registers a row of side-by-side columns the caller drew
// inside area, such as the bars of a chart. A click anywhere in area over a
// column's [start, end) screen span selects it, and the left and right
// arrows move the selection, the way they move a tab strip's.
func (f *Form[K]) RegisterColumns(id K, area uv.Rectangle, selected int, columns [][2]int, enabled bool) {
	kind := ControlKindTabs{Len: len(columns), Selected: selected}
	f.DeclareWithEnabled(id, kind, enabled)
	regions := make([]regionSpan, 0, len(columns))
	for index, column := range columns {
		regions = append(regions, regionSpan{start: column[0], end: column[1], index: index})
	}
	f.registerWithRegions(id, kind, area, enabled, regions)
}

// Declare declares a control for event handling before its first render.
//
// The declaration uses a zero geometry; a later render registration should
// replace it.
func (f *Form[K]) Declare(id K, kind ControlKind) {
	f.DeclareWithEnabled(id, kind, true)
}

// DeclareWithEnabled declares a control for event handling with an explicit
// enabled state.
func (f *Form[K]) DeclareWithEnabled(id K, kind ControlKind, enabled bool) {
	index := f.ensureControl(id, kind)
	control := &f.controls[index]
	if !sameControlKindVariant(control.kind, kind) || controlKindLengthChanged(control.kind, kind) {
		control.area = uv.Rectangle{}
		control.rowMap = nil
		control.rowEnabled = nil
		control.cursorMap = nil
		control.multilineCursorMap = nil
		control.editorArea = uv.Rectangle{}
		control.listOffset = 0
		control.popupArea = uv.Rectangle{}
		control.popupRowMap = nil
	}
	control.active = true
	control.enabled = enabled
	control.kind = kind
	if !slices.Contains(f.order, id) {
		f.order = append(f.order, id)
	}
}

// controlKindLengthChanged reports a list, combobox, or path field that
// now has a different number of rows, which reads as a different list.
func controlKindLengthChanged(before, after ControlKind) bool {
	switch before := before.(type) {
	case ControlKindChoiceList:
		after, ok := after.(ControlKindChoiceList)
		return ok && before.Len != after.Len
	case ControlKindComboBox:
		after, ok := after.(ControlKindComboBox)
		return ok && before.Len != after.Len
	case ControlKindPathField:
		after, ok := after.(ControlKindPathField)
		return ok && before.Len != after.Len
	}
	return false
}

// Clear removes all active controls while retaining their stable identities
// for future frames.
func (f *Form[K]) Clear() {
	f.viewport = nil
	f.viewportActive = false
	f.viewportFocus = nil
	f.viewportReveal = true
	f.CancelPointer()
	f.order = f.order[:0]
	f.lastOrder = f.lastOrder[:0]
	f.pendingFocus = nil
	f.pointerOwner = nil
	f.dismissArea = uv.Rectangle{}
	f.dismissEnabled = false
	f.dismissActive = false
	for i := range f.controls {
		control := &f.controls[i]
		control.active = false
		control.flag.clear()
		control.popupArea = uv.Rectangle{}
		control.popupRowMap = nil
	}
	f.focusTree.none()
}

// SetRowEnabled updates choice availability after a background result
// without waiting for another render or invalidating the list's hitboxes.
func (f *Form[K]) SetRowEnabled(id K, enabled []bool) {
	if control := f.controlMut(id); control != nil {
		control.rowEnabled = enabled
	}
}

// ResetGeometry clears hitboxes and cursor maps while retaining
// declarations and focus identities.
func (f *Form[K]) ResetGeometry() {
	f.viewportActive = false
	for i := range f.controls {
		control := &f.controls[i]
		control.area = uv.Rectangle{}
		control.cursorMap = nil
		control.multilineCursorMap = nil
		control.editorArea = uv.Rectangle{}
		control.popupArea = uv.Rectangle{}
		control.popupRowMap = nil
	}
	f.dismissArea = uv.Rectangle{}
}

// activeLeaves is the focus tree's leaves: the active controls in visual
// order.
func (f *Form[K]) activeLeaves() []focusLeaf {
	leaves := make([]focusLeaf, 0, len(f.order))
	for _, id := range f.order {
		if control := f.activeControl(id); control != nil {
			leaves = append(leaves, focusLeaf{flag: control.flag, area: control.area, navigable: control.navigable()})
		}
	}
	return leaves
}

// EndFrame rebuilds the focus tree and repairs focus after controls
// appeared or disappeared.
func (f *Form[K]) EndFrame(initial K) {
	previousFocus := f.focusedKey()
	if candidate := optionalKey(f.pendingFocus); candidate != nil || previousFocus != nil {
		if candidate == nil {
			candidate = previousFocus
		}
		if control := f.activeControl(*candidate); control != nil && !control.enabled && controlKindIsButton(control.kind) {
			f.blockedActivation = candidate
		}
	}
	previousOrder := slices.Clone(f.lastOrder)
	f.focusTree = buildFocusTree(f.focusTree, f.activeLeaves())
	f.lastOrder = slices.Clone(f.order)
	// Hosts clear geometry before rendering. Metadata reconciliation can run
	// in that interval; only removal or disabling cancels a captured press.
	if f.pointerOwner != nil && !f.pointerOwnerIsEligible(*f.pointerOwner) {
		f.pointerOwner = nil
	}

	var preserved *K
	if previousFocus != nil && f.isEligible(*previousFocus) {
		preserved = previousFocus
	}
	pending := f.pendingFocus
	f.pendingFocus = nil
	var wanted *K
	switch {
	case pending != nil && f.isEligible(*pending):
		wanted = pending
	case preserved != nil:
		wanted = preserved
	case previousFocus == nil && f.isEligible(initial):
		wanted = &initial
	default:
		if id, ok := f.fallback(previousFocus, previousOrder); ok {
			wanted = &id
		}
	}
	if wanted != nil && f.isEligible(*wanted) {
		f.Focus(*wanted)
	} else {
		f.focusTree.none()
		f.pendingFocus = nil
	}
}

// Focused returns the focused control, if any.
func (f *Form[K]) Focused() (K, bool) {
	if f.pendingFocus != nil && f.isEligible(*f.pendingFocus) {
		return *f.pendingFocus, true
	}
	var none K
	if flag := f.focusTree.focused(); flag != nil {
		for i := range f.controls {
			if f.controls[i].flag == flag {
				return f.controls[i].id, true
			}
		}
	}
	return none, false
}

// Focus requests focus for a stable control id.
func (f *Form[K]) Focus(id K) {
	if current, ok := f.Focused(); !ok || current != id {
		f.viewportReveal = true
	}
	if control := f.activeControl(id); control != nil && control.enabled {
		if f.focusTree.isValidWidget(control.flag) {
			f.focusTree.focus(control.flag)
			f.pendingFocus = nil
		} else {
			f.pendingFocus = &id
		}
	} else {
		f.pendingFocus = &id
	}
}

// Contains reports whether the given point is in any active control hitbox.
func (f *Form[K]) Contains(x, y int) bool {
	_, ok := f.hit(x, y)
	return ok
}

// Handle handles keyboard and mouse input for the form.
func (f *Form[K]) Handle(event tea.Msg) EventResult[Interaction[K]] {
	return f.HandleAt(event, time.Now())
}

// HandleAt handles input with an explicit monotonic time, also useful for
// deterministic tests.
func (f *Form[K]) HandleAt(event tea.Msg, now time.Time) EventResult[Interaction[K]] {
	f.eventTime = now
	switch event.(type) {
	case tea.WindowSizeMsg, tea.PasteMsg, tea.BlurMsg, tea.MouseWheelMsg:
		f.CancelPointer()
	}
	press, isPress := event.(tea.KeyPressMsg)
	isPress = isPress && !press.IsRepeat
	mouseKind, _, _, _ := classifyMouse(event)
	if isPress && press.Code == tea.KeyEnter && f.blockedActivation != nil {
		id := *f.blockedActivation
		if !f.IsEnabled(id) {
			return EventResultHandled[Interaction[K]]()
		}
		f.Focus(id)
		f.blockedActivation = nil
	} else if isPress || mouseKind == mouseKindDown {
		f.blockedActivation = nil
	}
	return f.handleInner(event)
}

func (f *Form[K]) handleInner(event tea.Msg) EventResult[Interaction[K]] {
	if result, ok := f.handlePointer(event); ok {
		return result
	}
	if press, ok := event.(tea.KeyPressMsg); ok && !press.IsRepeat {
		f.CancelPointer()
	}
	if result, ok := f.handleKey(event); ok {
		return result
	}
	if paste, ok := event.(tea.PasteMsg); ok {
		if id, ok := f.Focused(); ok && controlKindIsField(f.kind(id)) {
			return EventResultWithAction[Interaction[K]](InteractionEdit[K]{ID: id, Edit: FieldEditPaste{Text: paste.Content}})
		}
	}
	return EventResultIgnored[Interaction[K]]()
}

// CapturesPointer reports whether a mouse control owns a press/release
// gesture.
func (f *Form[K]) CapturesPointer() bool {
	return f.pointerOwner != nil
}

// CancelPointer cancels a captured mouse gesture.
func (f *Form[K]) CancelPointer() {
	f.pointerOwner = nil
	f.pressedTarget = nil
	f.lastClick = nil
}

// IsEnabled reports whether the control is declared and accepts keyboard
// focus.
func (f *Form[K]) IsEnabled(id K) bool {
	return f.isEligible(id)
}

// IsFocused reports whether a control currently has input focus.
func (f *Form[K]) IsFocused(id K) bool {
	if !f.isEligible(id) {
		return false
	}
	focused, ok := f.Focused()
	return ok && focused == id
}

// IsArmed reports whether a control currently owns a mouse press/release
// gesture.
func (f *Form[K]) IsArmed(id K) bool {
	return f.pointerOwner != nil && *f.pointerOwner == pointerOwner[K]{kind: pointerOwnerControl, id: id}
}

// DismissIsArmed reports whether the modal dismissal target owns an active
// press.
func (f *Form[K]) DismissIsArmed() bool {
	return f.pointerOwner != nil && f.pointerOwner.kind == pointerOwnerDismiss
}

// Selected returns the current selection metadata for a list, combobox, or
// tab strip.
func (f *Form[K]) Selected(id K) (int, bool) {
	return controlKindSelected(f.kind(id))
}

// ListOffset returns the first display row currently visible for a choice
// list.
func (f *Form[K]) ListOffset(id K) int {
	if control := f.control(id); control != nil {
		return control.listOffset
	}
	return 0
}

// SetSelected updates selection metadata immediately, allowing batched key
// events before redraw.
func (f *Form[K]) SetSelected(id K, selected int) {
	control := f.controlMut(id)
	if control == nil {
		return
	}
	clamp := func(length int) int { return min(selected, max(length-1, 0)) }
	switch kind := control.kind.(type) {
	case ControlKindChoiceList:
		control.kind = ControlKindChoiceList{Len: kind.Len, Selected: clamp(kind.Len)}
	case ControlKindTabs:
		control.kind = ControlKindTabs{Len: kind.Len, Selected: clamp(kind.Len)}
	case ControlKindComboBox:
		control.kind = ControlKindComboBox{Len: kind.Len, Selected: clamp(kind.Len), Expanded: kind.Expanded}
	case ControlKindPathField:
		control.kind = ControlKindPathField{Len: kind.Len, Selected: clamp(kind.Len), Expanded: kind.Expanded}
	}
}

func (f *Form[K]) setListOffset(id K, offset int) {
	if control := f.controlMut(id); control != nil {
		control.listOffset = offset
	}
}

func (f *Form[K]) ensureControl(id K, kind ControlKind) int {
	for index := range f.controls {
		if f.controls[index].id == id {
			return index
		}
	}
	f.controls = append(f.controls, newScopeControl(id, kind))
	return len(f.controls) - 1
}

func (f *Form[K]) control(id K) *scopeControl[K] {
	for index := range f.controls {
		if f.controls[index].id == id {
			return &f.controls[index]
		}
	}
	return nil
}

func (f *Form[K]) controlMut(id K) *scopeControl[K] {
	return f.control(id)
}

func (f *Form[K]) activeControl(id K) *scopeControl[K] {
	if control := f.control(id); control != nil && control.active {
		return control
	}
	return nil
}

func (f *Form[K]) kind(id K) ControlKind {
	if control := f.control(id); control != nil {
		return control.kind
	}
	return ControlKindButton{}
}

func (f *Form[K]) isEligible(id K) bool {
	control := f.activeControl(id)
	return control != nil && control.enabled
}

func (f *Form[K]) dismissIsEligible() bool {
	return f.dismissActive && f.dismissEnabled
}

func (f *Form[K]) pointerOwnerIsEligible(owner pointerOwner[K]) bool {
	if owner.kind == pointerOwnerDismiss {
		return f.dismissIsEligible()
	}
	return f.isEligible(owner.id)
}

func (f *Form[K]) fallback(previousFocus *K, previousOrder []K) (K, bool) {
	skip := 0
	if previousFocus != nil {
		if start := slices.Index(previousOrder, *previousFocus); start >= 0 {
			skip = start
		}
	}
	for _, id := range f.order[min(skip, len(f.order)):] {
		if f.isEligible(id) {
			return id, true
		}
	}
	for _, id := range f.order {
		if f.isEligible(id) {
			return id, true
		}
	}
	var none K
	return none, false
}

// pointIn is ratatui's `Rect::contains`.
func pointIn(x, y int, area uv.Rectangle) bool {
	return uv.Pos(x, y).In(area)
}

func (f *Form[K]) hit(x, y int) (pointerHit[K], bool) {
	if f.dismissActive && pointIn(x, y, f.dismissArea) {
		return pointerHit[K]{kind: pointerHitDismiss}, true
	}
	// An open popup is painted over later controls, so it wins even when its
	// owner registered earlier in tab order.
	for i := len(f.order) - 1; i >= 0; i-- {
		if control := f.activeControl(f.order[i]); control != nil && pointIn(x, y, control.popupArea) {
			return pointerHit[K]{kind: pointerHitControl, id: control.id}, true
		}
	}
	for i := len(f.order) - 1; i >= 0; i-- {
		if control := f.activeControl(f.order[i]); control != nil && pointIn(x, y, control.area) {
			return pointerHit[K]{kind: pointerHitControl, id: control.id}, true
		}
	}
	return pointerHit[K]{}, false
}

// componentKeyMods is a key's modifiers as crossterm reports them: the lock
// states Bubble Tea folds into its modifiers are not modifiers there.
func componentKeyMods(mod tea.KeyMod) tea.KeyMod {
	return mod &^ (tea.ModCapsLock | tea.ModNumLock | tea.ModScrollLock)
}

// componentKeyCode is the key a press names, as crossterm's KeyCode names
// it: a printable key is the character it types, so a shifted letter is the
// upper-case character (crossterm's `Char('G')`).
func componentKeyCode(key tea.Key) rune {
	if character, size := utf8.DecodeRuneInString(key.Text); size > 0 && size == len(key.Text) {
		return character
	}
	return key.Code
}

// isCharKey matches crossterm's `KeyCode::Char(_)`: a printable character,
// which Enter, Tab, Escape and Backspace are not.
func isCharKey(code rune) bool {
	return code >= ' ' && code != tea.KeyBackspace && code < tea.KeyExtended
}

func (f *Form[K]) handleKey(event tea.Msg) (EventResult[Interaction[K]], bool) {
	var key tea.Key
	var press tea.KeyPressMsg
	switch msg := event.(type) {
	case tea.KeyPressMsg:
		press = msg
		key = msg.Key()
	case tea.KeyReleaseMsg:
		if _, ok := f.Focused(); ok {
			return EventResultHandled[Interaction[K]](), true
		}
		return EventResultIgnored[Interaction[K]](), true
	default:
		return EventResult[Interaction[K]]{}, false
	}
	with := func(action Interaction[K]) (EventResult[Interaction[K]], bool) {
		return EventResultWithAction(action), true
	}
	handled := func() (EventResult[Interaction[K]], bool) {
		return EventResultHandled[Interaction[K]](), true
	}
	isPress := !key.IsRepeat
	mods := componentKeyMods(key.Mod)
	code := componentKeyCode(key)
	ordinary := mods&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) == 0
	focusedID, hasFocused := f.Focused()
	hasFocused = hasFocused && f.isEligible(focusedID)
	// Ctrl-Space asks a path field for completions. Some terminals send it
	// as NUL instead of a modified space, so both spellings are accepted;
	// neither survives the modifier gate below.
	if isPress && hasFocused && controlKindIsPathField(f.kind(focusedID)) &&
		(code == 0 || (mods == tea.ModCtrl && code == ' ')) {
		return with(InteractionComplete[K]{ID: focusedID})
	}
	if ordinary && isTab(key) {
		if hasFocused && controlKindIsExpandedPathField(f.kind(focusedID)) {
			// Tab still moves focus; the popup closes behind it.
			f.focusPendingSibling(!isBackTab(key))
			return with(InteractionPathDismiss[K]{ID: focusedID})
		}
		if hasFocused {
			if combo, ok := f.kind(focusedID).(ControlKindComboBox); ok && combo.Expanded {
				return with(InteractionComboBoxCommit[K]{ID: focusedID, Index: combo.Selected})
			}
		}
		f.focusPendingSibling(!isBackTab(key))
		return handled()
	}
	if isPress && ordinary && code == tea.KeyEscape {
		if hasFocused && controlKindIsExpandedComboBox(f.kind(focusedID)) {
			return with(InteractionComboBoxDismiss[K]{ID: focusedID})
		}
		if hasFocused && controlKindIsExpandedPathField(f.kind(focusedID)) {
			return with(InteractionPathDismiss[K]{ID: focusedID})
		}
		return with(InteractionCancel[K]{})
	}

	if !hasFocused {
		return EventResult[Interaction[K]]{}, false
	}
	id := focusedID
	kind := f.kind(id)
	if !isPress && ordinary &&
		(code == tea.KeyEnter || code == tea.KeyEscape || (code == ' ' && !controlKindIsField(kind))) {
		return handled()
	}
	if ordinary && (code == tea.KeyUp || code == tea.KeyDown) {
		control := f.control(id)
		if control == nil {
			return EventResult[Interaction[K]]{}, false
		}
		combo, isCombo := kind.(ControlKindComboBox)
		if controlKindIsButton(kind) ||
			controlKindIsCheckbox(kind) ||
			(isCombo && !combo.Expanded) ||
			(controlKindIsField(kind) && !controlKindIsExpandedPathField(kind) && !control.usesVerticalKeys) {
			f.focusSibling(id, code == tea.KeyDown, false)
			return handled()
		}
	}
	if controlKindIsButton(kind) {
		if isPress && ordinary && (code == tea.KeyEnter || code == ' ') {
			return with(InteractionActivate[K]{ID: id})
		}
		if ordinary && (code == tea.KeyLeft || code == tea.KeyRight) {
			f.focusSibling(id, code == tea.KeyRight, true)
			return handled()
		}
		return EventResult[Interaction[K]]{}, false
	}
	// An open completion popup takes the navigation keys and Enter; every
	// other key, including letters, stays text so typing keeps editing.
	if path, ok := kind.(ControlKindPathField); ordinary && ok && path.Expanded {
		if isPress && code == tea.KeyEnter {
			return with(InteractionPathCommit[K]{ID: id, Index: path.Selected})
		}
		if !isCharKey(code) {
			if next, ok := listSelection(code, mods, path.Selected, path.Len); ok {
				f.SetSelected(id, next)
				return with(InteractionSelect[K]{ID: id, Index: next})
			}
		}
	}
	if controlKindIsField(kind) {
		if isPress && ordinary && code == tea.KeyEnter {
			return with(InteractionActivate[K]{ID: id})
		}
		return with(InteractionEdit[K]{ID: id, Edit: FieldEditKey{Key: press}})
	}
	if isPress && controlKindIsCheckbox(kind) && ordinary && (code == tea.KeyEnter || code == ' ') {
		return with(InteractionToggle[K]{ID: id})
	}
	if combo, ok := kind.(ControlKindComboBox); ordinary && ok {
		if isPress && (code == ' ' || code == tea.KeyEnter) {
			if combo.Expanded {
				return with(InteractionComboBoxCommit[K]{ID: id, Index: combo.Selected})
			}
			return with(InteractionActivate[K]{ID: id})
		}
		if combo.Expanded {
			if next, ok := listSelection(code, mods, combo.Selected, combo.Len); ok {
				f.SetSelected(id, next)
				return with(InteractionSelect[K]{ID: id, Index: next})
			}
		}
	}
	// `ctrl+d` and `ctrl+u` page a list, so they are the one pair allowed
	// past the modifier gate; every other list key is unmodified.
	listPage := mods == tea.ModCtrl && (code == 'd' || code == 'u')
	if list, ok := kind.(ControlKindChoiceList); (ordinary || listPage) && ok {
		if isPress && (code == ' ' || code == tea.KeyEnter) {
			control := f.control(id)
			if control == nil {
				return EventResult[Interaction[K]]{}, false
			}
			allowed := list.Selected < list.Len
			if len(control.rowMap) > 0 {
				row, found := control.rowOf(list.Selected)
				allowed = found && control.rowIsEnabled(row)
			}
			if !allowed {
				return handled()
			}
			if code == tea.KeyEnter {
				return with(InteractionActivate[K]{ID: id})
			}
			return with(InteractionToggle[K]{ID: id})
		}
		next, hasNext := f.listSelection(id, code, mods, list.Selected, list.Len)
		// At either end, arrows leave the list for the surrounding controls.
		// Follow selectable rows so disabled rows and section headers do not
		// trap focus before an unreachable first or last data index.
		if ordinary && (code == tea.KeyUp || code == tea.KeyDown) &&
			(!hasNext || next == list.Selected) &&
			f.focusSibling(id, code == tea.KeyDown, false) {
			return handled()
		}
		if hasNext {
			f.SetSelected(id, next)
			return with(InteractionSelect[K]{ID: id, Index: next})
		}
	}
	if tabs, ok := kind.(ControlKindTabs); ordinary && ok {
		if next, ok := tabSelection(code, tabs.Selected, tabs.Len); ok {
			f.SetSelected(id, next)
			return with(InteractionSelect[K]{ID: id, Index: next})
		}
	}
	if isPress && ordinary && controlKindIsTabStrip(kind) && code == tea.KeyEnter {
		return with(InteractionActivate[K]{ID: id})
	}
	return EventResult[Interaction[K]]{}, false
}

func (f *Form[K]) focusSibling(current K, forward bool, buttonsOnly bool) bool {
	index := slices.Index(f.order, current)
	if index < 0 {
		return false
	}
	var indices []int
	if forward {
		for candidate := index + 1; candidate < len(f.order); candidate++ {
			indices = append(indices, candidate)
		}
		for candidate := range index {
			indices = append(indices, candidate)
		}
	} else {
		for candidate := index - 1; candidate >= 0; candidate-- {
			indices = append(indices, candidate)
		}
		for candidate := len(f.order) - 1; candidate > index; candidate-- {
			indices = append(indices, candidate)
		}
	}
	for _, candidate := range indices {
		id := f.order[candidate]
		if f.isEligible(id) && (!buttonsOnly || controlKindIsButton(f.kind(id))) {
			f.Focus(id)
			return true
		}
	}
	return false
}

func (f *Form[K]) focusPendingSibling(forward bool) bool {
	if f.focusTree.focused() != nil {
		previous := f.focusedKey()
		var changed bool
		if forward {
			changed = f.focusTree.next()
		} else {
			changed = f.focusTree.prev()
		}
		if !optionalKeysEqual(f.focusedKey(), previous) {
			f.viewportReveal = true
		}
		return changed
	}
	currentIndex := -1
	if f.pendingFocus != nil {
		currentIndex = slices.Index(f.order, *f.pendingFocus)
	}
	var candidates []int
	switch {
	case forward && currentIndex >= 0:
		for index := currentIndex + 1; index < len(f.order); index++ {
			candidates = append(candidates, index)
		}
		for index := 0; index <= currentIndex; index++ {
			candidates = append(candidates, index)
		}
	case forward:
		for index := range f.order {
			candidates = append(candidates, index)
		}
	case currentIndex >= 0:
		for index := currentIndex - 1; index >= 0; index-- {
			candidates = append(candidates, index)
		}
		for index := len(f.order) - 1; index >= currentIndex; index-- {
			candidates = append(candidates, index)
		}
	default:
		for index := len(f.order) - 1; index >= 0; index-- {
			candidates = append(candidates, index)
		}
	}
	for _, index := range candidates {
		if f.isEligible(f.order[index]) {
			f.Focus(f.order[index])
			return true
		}
	}
	return false
}

func (f *Form[K]) listSelection(id K, code rune, modifiers tea.KeyMod, selected int, length int) (int, bool) {
	control := f.control(id)
	if control == nil {
		return 0, false
	}
	if len(control.rowMap) == 0 {
		return listSelection(code, modifiers, selected, length)
	}
	rows := len(control.rowMap)
	current, _ := control.rowOf(selected)
	ctrl := modifiers&tea.ModCtrl != 0
	selectable := func(row int) bool { return control.rowIsEnabled(row) && control.rowMap[row] != nil }
	target, ok := 0, false
	switch {
	case code == 'd' && ctrl:
		target, ok = stepRows(control, current, selected, true, halfPage)
	case code == 'u' && ctrl:
		target, ok = stepRows(control, current, selected, false, halfPage)
	case ctrl:
	case code == tea.KeyUp || code == 'k':
		target, ok = stepRows(control, current, selected, false, 1)
	case code == tea.KeyDown || code == 'j':
		target, ok = stepRows(control, current, selected, true, 1)
	case code == tea.KeyHome:
		for row := range rows {
			if selectable(row) {
				target, ok = row, true
				break
			}
		}
	case code == tea.KeyEnd || code == 'G':
		for row := rows - 1; row >= 0; row-- {
			if selectable(row) {
				target, ok = row, true
				break
			}
		}
	case code == tea.KeyPgUp:
		target, ok = pageTarget(control, current, false)
	case code == tea.KeyPgDown:
		target, ok = pageTarget(control, current, true)
	}
	if !ok {
		return 0, false
	}
	return control.rowItem(target)
}

// mouseEventKind is crossterm's MouseEventKind as read from a Bubble Tea
// mouse message.
type mouseEventKind uint8

const (
	mouseKindNone mouseEventKind = iota
	mouseKindDown
	mouseKindUp
	mouseKindDrag
	mouseKindMoved
	mouseKindScrollUp
	mouseKindScrollDown
	mouseKindScrollLeft
	mouseKindScrollRight
)

// classifyMouse reads crossterm's view of a mouse event from a Bubble Tea
// message: Down(button) is a tea.MouseClickMsg, Up(button) a
// tea.MouseReleaseMsg, Drag(button) a tea.MouseMotionMsg with a button held,
// Moved one with none, and the Scroll kinds a tea.MouseWheelMsg. Anything
// else is mouseKindNone.
func classifyMouse(event tea.Msg) (kind mouseEventKind, button tea.MouseButton, x, y int) {
	switch msg := event.(type) {
	case tea.MouseClickMsg:
		return mouseKindDown, msg.Button, msg.X, msg.Y
	case tea.MouseReleaseMsg:
		return mouseKindUp, msg.Button, msg.X, msg.Y
	case tea.MouseMotionMsg:
		if msg.Button == tea.MouseNone {
			return mouseKindMoved, msg.Button, msg.X, msg.Y
		}
		return mouseKindDrag, msg.Button, msg.X, msg.Y
	case tea.MouseWheelMsg:
		switch msg.Button {
		case tea.MouseWheelUp:
			kind = mouseKindScrollUp
		case tea.MouseWheelDown:
			kind = mouseKindScrollDown
		case tea.MouseWheelLeft:
			kind = mouseKindScrollLeft
		case tea.MouseWheelRight:
			kind = mouseKindScrollRight
		}
		return kind, msg.Button, msg.X, msg.Y
	}
	return mouseKindNone, tea.MouseNone, 0, 0
}

func (f *Form[K]) handlePointer(event tea.Msg) (EventResult[Interaction[K]], bool) {
	if _, ok := event.(tea.MouseMsg); !ok {
		return EventResult[Interaction[K]]{}, false
	}
	kind, button, x, y := classifyMouse(event)
	handled := func() (EventResult[Interaction[K]], bool) {
		return EventResultHandled[Interaction[K]](), true
	}
	if f.pointerOwner != nil {
		switch {
		case (kind == mouseKindDrag && button == tea.MouseLeft) || kind == mouseKindMoved:
			owner := *f.pointerOwner
			if !f.pointerOwnerContains(owner, x, y) ||
				(owner.kind == pointerOwnerControl && !clickTargetsEqual(f.pressedTarget, f.clickTarget(owner.id, x, y))) {
				f.CancelPointer()
			}
			return handled()
		case kind == mouseKindUp && button == tea.MouseLeft:
			owner := *f.pointerOwner
			f.pointerOwner = nil
			if !f.pointerOwnerContains(owner, x, y) {
				f.CancelPointer()
				return handled()
			}
			if owner.kind == pointerOwnerControl {
				pressed := f.pressedTarget
				f.pressedTarget = nil
				if !clickTargetsEqual(pressed, f.clickTarget(owner.id, x, y)) {
					f.CancelPointer()
					return handled()
				}
			}
			var interaction Interaction[K]
			if owner.kind == pointerOwnerControl {
				interaction = f.pointerInteraction(owner.id, x, y)
			} else {
				interaction = InteractionCancel[K]{}
			}
			if interaction != nil {
				return EventResultWithAction(interaction), true
			}
			return handled()
		default:
			return EventResult[Interaction[K]]{}, false
		}
	}
	if kind == mouseKindScrollUp || kind == mouseKindScrollDown {
		var target *scopeControl[K]
		length, selected := 0, 0
		if hit, ok := f.hit(x, y); ok && hit.kind == pointerHitControl {
			if control := f.control(hit.id); control != nil && control.enabled {
				switch kind := control.kind.(type) {
				case ControlKindChoiceList:
					target, length, selected = control, kind.Len, kind.Selected
				case ControlKindComboBox:
					if kind.Expanded {
						target, length, selected = control, kind.Len, kind.Selected
					}
				case ControlKindPathField:
					if kind.Expanded {
						target, length, selected = control, kind.Len, kind.Selected
					}
				}
			}
		}
		if target == nil {
			if f.viewport == nil {
				return EventResult[Interaction[K]]{}, false
			}
			viewport, height := f.viewport.viewport, f.viewport.contentHeight
			if !f.viewportActive || !pointIn(x, y, viewport.areaRect()) {
				return EventResult[Interaction[K]]{}, false
			}
			// ChoiceList wheel events move by one row too.
			offset := viewport.Offset() + 1
			if kind == mouseKindScrollUp {
				offset = max(viewport.Offset()-1, 0)
			}
			f.viewport = &formViewport{
				viewport:      NewFormViewport(viewport.areaRect(), height, offset, nil),
				contentHeight: height,
			}
			// Scrolling follows any focus repair in the last frame. Only a
			// subsequent focus change should reveal again.
			f.viewportFocus = optionalKey(f.pendingFocus)
			if f.viewportFocus == nil {
				f.viewportFocus = f.focusedKey()
			}
			f.viewportReveal = false
			return handled()
		}
		id := target.id
		code := tea.KeyDown
		if kind == mouseKindScrollUp {
			code = tea.KeyUp
		}
		f.Focus(id)
		if next, ok := f.listSelection(id, code, 0, selected, length); ok {
			f.SetSelected(id, next)
			return EventResultWithAction[Interaction[K]](InteractionSelect[K]{ID: id, Index: next}), true
		}
		return handled()
	}
	if kind != mouseKindDown || button != tea.MouseLeft {
		return EventResult[Interaction[K]]{}, false
	}
	hit, ok := f.hit(x, y)
	if !ok {
		f.CancelPointer()
		return EventResult[Interaction[K]]{}, false
	}
	if hit.kind == pointerHitDismiss {
		if !f.dismissEnabled {
			return handled()
		}
		f.lastClick = nil
		f.pointerOwner = &pointerOwner[K]{kind: pointerOwnerDismiss}
		return handled()
	}
	id := hit.id
	control := f.control(id)
	if control == nil {
		return EventResult[Interaction[K]]{}, false
	}
	enabled, controlKind := control.enabled, control.kind
	cursor := cursorAt(control, x, y)
	editor := pointIn(x, y, control.editorArea)
	if !enabled {
		f.CancelPointer()
		return handled()
	}
	// A press on another control closes an open completion popup. Focus
	// follows the press, but the press itself only dismisses; the new
	// control answers the next click.
	if previous, ok := f.Focused(); ok && previous != id && controlKindIsExpandedPathField(f.kind(previous)) {
		f.CancelPointer()
		f.Focus(id)
		return EventResultWithAction[Interaction[K]](InteractionPathDismiss[K]{ID: previous}), true
	}
	control = f.control(id)
	inPopup := (controlKindIsComboBox(controlKind) || controlKindIsPathField(controlKind)) && pointIn(x, y, control.popupArea)
	if inPopup {
		if _, ok := control.popupItem(max(y-control.popupArea.Min.Y, 0)); !ok {
			return handled()
		}
	}
	if controlKindIsChoiceList(controlKind) && !editor {
		row := max(y-control.area.Min.Y, 0) + control.listOffset
		_, mapped := control.rowItem(row)
		if !control.rowIsEnabled(row) || (len(control.rowMap) > 0 && !mapped) {
			f.CancelPointer()
			return handled()
		}
	}
	target := f.clickTarget(id, x, y)
	if f.lastClick != nil && (target == nil || f.lastClick.target != *target) {
		f.lastClick = nil
	}
	f.Focus(id)
	// A press inside the popup is a row click, not a place-the-cursor click,
	// so it arms the gesture before the field's editor claims it.
	if inPopup && controlKindIsPathField(controlKind) {
		f.pressedTarget = target
		f.pointerOwner = &pointerOwner[K]{kind: pointerOwnerControl, id: id}
		return handled()
	}
	if controlKindIsField(controlKind) || editor {
		return EventResultWithAction[Interaction[K]](InteractionEdit[K]{ID: id, Edit: FieldEditCursor{Offset: cursor}}), true
	}
	if controlKindIsButton(controlKind) ||
		controlKindIsCheckbox(controlKind) ||
		controlKindIsChoiceList(controlKind) ||
		controlKindIsComboBox(controlKind) ||
		controlKindIsTabStrip(controlKind) {
		f.pressedTarget = target
		f.pointerOwner = &pointerOwner[K]{kind: pointerOwnerControl, id: id}
		return handled()
	}
	return EventResult[Interaction[K]]{}, false
}

func (f *Form[K]) pointerOwnerContains(owner pointerOwner[K], x, y int) bool {
	if owner.kind == pointerOwnerDismiss {
		return f.dismissIsEligible() && pointIn(x, y, f.dismissArea)
	}
	control := f.control(owner.id)
	return control != nil &&
		control.active &&
		control.enabled &&
		(pointIn(x, y, control.area) || pointIn(x, y, control.popupArea))
}

func (f *Form[K]) clickTarget(id K, x, y int) *clickTarget[K] {
	control := f.activeControl(id)
	if control == nil {
		return nil
	}
	target := clickTarget[K]{id: id, area: control.area, revision: control.revision}
	switch {
	case controlKindIsChoiceList(control.kind):
		row := max(y-control.area.Min.Y, 0) + control.listOffset
		if !control.rowIsEnabled(row) {
			return nil
		}
		if len(control.rowMap) == 0 {
			target.item, target.hasItem = row, true
		} else {
			item, ok := control.rowItem(row)
			if !ok {
				return nil
			}
			target.item, target.hasItem = item, true
		}
	case controlKindIsTabStrip(control.kind):
		for _, region := range control.regionMap {
			if x >= region.start && x < region.end {
				target.item, target.hasItem = region.index, true
				break
			}
		}
	case pointIn(x, y, control.popupArea):
		if item, ok := control.popupItem(max(y-control.popupArea.Min.Y, 0)); ok {
			target.item, target.hasItem = item, true
		}
	}
	return &target
}

func (f *Form[K]) pointerInteraction(id K, x, y int) Interaction[K] {
	control := f.control(id)
	if control == nil {
		return nil
	}
	switch kind := control.kind.(type) {
	case ControlKindButton:
		return InteractionActivate[K]{ID: id}
	case ControlKindCheckbox:
		return InteractionToggle[K]{ID: id}
	case ControlKindChoiceList:
		row := max(y-control.area.Min.Y, 0) + control.listOffset
		if (len(control.rowMap) == 0 && row >= kind.Len) || !control.rowIsEnabled(row) {
			return nil
		}
		selected := row
		if len(control.rowMap) > 0 {
			item, ok := control.rowItem(row)
			if !ok {
				return nil
			}
			selected = item
		}
		target := f.clickTarget(id, x, y)
		if target == nil {
			return nil
		}
		activate := control.activation == ListActivationSingleClick
		if !activate && f.lastClick != nil && f.lastClick.target == *target {
			// Instant::saturating_duration_since: a click from the future is
			// no time ago.
			activate = max(f.eventTime.Sub(f.lastClick.at), 0) <= DoubleClickInterval
		}
		if activate {
			f.lastClick = nil
		} else {
			f.lastClick = &lastClick[K]{target: *target, at: f.eventTime}
		}
		f.SetSelected(id, selected)
		if activate {
			return InteractionActivate[K]{ID: id}
		}
		return InteractionSelect[K]{ID: id, Index: selected}
	case ControlKindTabs:
		for _, region := range control.regionMap {
			if x >= region.start && x < region.end {
				if region.index >= kind.Len {
					return nil
				}
				f.SetSelected(id, region.index)
				return InteractionSelect[K]{ID: id, Index: region.index}
			}
		}
		return nil
	case ControlKindComboBox:
		if kind.Expanded && pointIn(x, y, control.popupArea) {
			index, ok := control.popupItem(max(y-control.popupArea.Min.Y, 0))
			if !ok {
				return nil
			}
			f.SetSelected(id, index)
			return InteractionComboBoxCommit[K]{ID: id, Index: index}
		}
		if kind.Expanded {
			return InteractionComboBoxDismiss[K]{ID: id}
		}
		return InteractionActivate[K]{ID: id}
	case ControlKindPathField:
		if kind.Expanded && pointIn(x, y, control.popupArea) {
			index, ok := control.popupItem(max(y-control.popupArea.Min.Y, 0))
			if !ok {
				return nil
			}
			f.SetSelected(id, index)
			return InteractionPathCommit[K]{ID: id, Index: index}
		}
		// A click in the field itself keeps the cursor placement the press
		// already emitted.
		return nil
	}
	return nil
}

// Clone copies the form for a draft: the focused control carries over, a
// physical pointer gesture does not.
func (f *Form[K]) Clone() Form[K] {
	focused := f.focusedKey()
	clone := NewForm[K]()
	clone.order = slices.Clone(f.order)
	clone.lastOrder = slices.Clone(f.lastOrder)
	clone.pendingFocus = focused
	if clone.pendingFocus == nil {
		clone.pendingFocus = optionalKey(f.pendingFocus)
	}
	clone.dismissArea = f.dismissArea
	clone.dismissEnabled = f.dismissEnabled
	clone.dismissActive = f.dismissActive
	clone.defaultAction = optionalKey(f.defaultAction)
	clone.blockedActivation = optionalKey(f.blockedActivation)
	if f.viewport != nil {
		viewport := *f.viewport
		clone.viewport = &viewport
	}
	clone.viewportFocus = optionalKey(f.viewportFocus)
	clone.viewportReveal = f.viewportReveal
	clone.viewportActive = f.viewportActive
	// A physical pointer gesture belongs to the original view, never a draft
	// copy.
	clone.pointerOwner = nil
	clone.controls = make([]scopeControl[K], 0, len(f.controls))
	for i := range f.controls {
		control := &f.controls[i]
		cloned := newScopeControl(control.id, control.kind)
		cloned.area = control.area
		cloned.usesVerticalKeys = control.usesVerticalKeys
		cloned.enabled = control.enabled
		cloned.active = control.active
		cloned.cursorMap = slices.Clone(control.cursorMap)
		cloned.multilineCursorMap = slices.Clone(control.multilineCursorMap)
		cloned.editorArea = control.editorArea
		cloned.regionMap = slices.Clone(control.regionMap)
		cloned.listOffset = control.listOffset
		cloned.rowMap = slices.Clone(control.rowMap)
		cloned.rowEnabled = slices.Clone(control.rowEnabled)
		cloned.popupArea = control.popupArea
		cloned.popupRowMap = slices.Clone(control.popupRowMap)
		cloned.activation = control.activation
		cloned.contents = slices.Clone(control.contents)
		cloned.identity = control.identity
		cloned.revision = control.revision
		clone.controls = append(clone.controls, cloned)
	}
	clone.rebuildWithoutPreviousFocus()
	if focused != nil && clone.isEligible(*focused) {
		clone.Focus(*focused)
	}
	return clone
}

// Equal compares the active controls' identities, areas, enabled states and
// kinds, and the focused control.
func (f *Form[K]) Equal(other *Form[K]) bool {
	type summary struct {
		id      K
		area    uv.Rectangle
		enabled bool
		kind    ControlKind
	}
	active := func(form *Form[K]) []summary {
		var summaries []summary
		for _, control := range form.controls {
			if control.active {
				summaries = append(summaries, summary{control.id, control.area, control.enabled, control.kind})
			}
		}
		return summaries
	}
	return slices.Equal(active(f), active(other)) && optionalKeysEqual(f.focusedKey(), other.focusedKey())
}

// String renders the form's Debug summary.
func (f *Form[K]) String() string {
	_, hasFocus := f.Focused()
	return fmt.Sprintf(
		"Form { control_count: %d, active_count: %d, has_focus: %t, captures_pointer: %t }",
		len(f.controls), len(f.order), hasFocus, f.CapturesPointer(),
	)
}

func (f *Form[K]) rebuildWithoutPreviousFocus() {
	f.focusTree = buildFocusTree(f.focusTree, f.activeLeaves())
}

func isTab(key tea.Key) bool {
	return key.Code == tea.KeyTab
}

// isBackTab is crossterm's BackTab, which Bubble Tea reports as Shift-Tab.
func isBackTab(key tea.Key) bool {
	return key.Code == tea.KeyTab && key.Mod.Contains(tea.ModShift)
}

// halfPage is how far `ctrl+d` and `ctrl+u` move a list, following the vim
// convention of half a screen; the lists this serves are short, so the step
// is a constant.
const halfPage = 8

// listSelection answers the keys every list answers: arrows, Home/End, the
// Page keys, and the vim set (`j`, `k`, `G`, `ctrl+d`, `ctrl+u`). A focused
// text field never reaches here, because fields answer with
// [InteractionEdit] first, so the letters stay text wherever text is being
// typed.
func listSelection(code rune, modifiers tea.KeyMod, selected int, rows int) (int, bool) {
	if rows == 0 {
		return 0, false
	}
	last := rows - 1
	ctrl := modifiers&tea.ModCtrl != 0
	switch {
	case code == 'd' && ctrl:
		return min(selected+halfPage, last), true
	case code == 'u' && ctrl:
		return max(selected-halfPage, 0), true
	case ctrl:
		return 0, false
	case code == tea.KeyUp || code == 'k':
		return max(selected-1, 0), true
	case code == tea.KeyDown || code == 'j':
		return min(selected+1, last), true
	case code == tea.KeyHome:
		return 0, true
	case code == tea.KeyEnd || code == 'G':
		return last, true
	case code == tea.KeyPgUp:
		return max(selected-5, 0), true
	case code == tea.KeyPgDown:
		return min(selected+5, last), true
	}
	return 0, false
}

// rowSelectable reports whether a display row can be selected: it is
// enabled, it stands for an item, and it is not another row of the item
// already selected.
func rowSelectable[K comparable](control *scopeControl[K], row int, selected int) bool {
	enabled := row >= len(control.rowEnabled) || control.rowEnabled[row]
	item := control.rowMap[row]
	return enabled && item != nil && *item != selected
}

// stepRows walks times selectable rows from from, stopping at the end of the
// list and returning the last row reached.
func stepRows[K comparable](control *scopeControl[K], from int, selected int, forward bool, times int) (int, bool) {
	rows := len(control.rowMap)
	current := from
	reached, ok := 0, false
	for range times {
		next := -1
		if forward {
			for row := current + 1; row < rows; row++ {
				if rowSelectable(control, row, selected) {
					next = row
					break
				}
			}
		} else {
			for row := current - 1; row >= 0; row-- {
				if rowSelectable(control, row, selected) {
					next = row
					break
				}
			}
		}
		if next < 0 {
			break
		}
		current = next
		reached, ok = next, true
	}
	return reached, ok
}

func pageTarget[K comparable](control *scopeControl[K], current int, forward bool) (int, bool) {
	rows := len(control.rowMap)
	areaRows := max(control.area.Dy(), 1)
	selectable := func(row int) bool { return control.rowIsEnabled(row) && control.rowMap[row] != nil }
	if forward {
		for row := min(current+areaRows, max(rows-1, 0)); row < rows; row++ {
			if selectable(row) {
				return row, true
			}
		}
		return 0, false
	}
	for row := max(current-areaRows, 0); row >= 0; row-- {
		if selectable(row) {
			return row, true
		}
	}
	return 0, false
}

func tabSelection(code rune, selected int, tabs int) (int, bool) {
	if tabs == 0 {
		return 0, false
	}
	last := tabs - 1
	switch code {
	case tea.KeyLeft:
		return max(selected-1, 0), true
	case tea.KeyRight:
		return min(selected+1, last), true
	case tea.KeyHome:
		return 0, true
	case tea.KeyEnd:
		return last, true
	}
	return 0, false
}

func absDiff(a, b int) int {
	if a > b {
		return a - b
	}
	return b - a
}

func cursorAt[K comparable](control *scopeControl[K], x, y int) int {
	if len(control.multilineCursorMap) > 0 {
		rowDistance := -1
		for _, stop := range control.multilineCursorMap {
			if distance := absDiff(stop.y, y); rowDistance < 0 || distance < rowDistance {
				rowDistance = distance
			}
		}
		columnDistance := -1
		for _, stop := range control.multilineCursorMap {
			if absDiff(stop.y, y) != rowDistance {
				continue
			}
			if distance := absDiff(stop.x, x); columnDistance < 0 || distance < columnDistance {
				columnDistance = distance
			}
		}
		offset := 0
		for _, stop := range control.multilineCursorMap {
			if absDiff(stop.y, y) == rowDistance && absDiff(stop.x, x) == columnDistance {
				offset = max(offset, stop.offset)
			}
		}
		return offset
	}
	best, offset := -1, 0
	for _, stop := range control.cursorMap {
		if distance := absDiff(stop.x, x); best < 0 || distance < best {
			best, offset = distance, stop.offset
		}
	}
	return offset
}

// ApplyFieldEdit applies a field edit to the existing readline editor.
//
// A cursor move counts as a change: callers such as the path fields
// recompute completions from the text under the cursor.
func ApplyFieldEdit(input *TextInput, edit FieldEdit) EditOutcome {
	switch edit := edit.(type) {
	case FieldEditKey:
		cursor := input.Cursor()
		outcome := input.HandleKey(edit.Key)
		if input.Cursor() == cursor {
			return outcome
		}
		return EditOutcomeChanged
	case FieldEditPaste:
		if input.InsertStr(SingleLinePaste(edit.Text)) {
			return EditOutcomeChanged
		}
		return EditOutcomeHandled
	case FieldEditCursor:
		before := input.Cursor()
		input.SetCursor(edit.Offset)
		if before == input.Cursor() {
			return EditOutcomeHandled
		}
		return EditOutcomeChanged
	}
	return EditOutcomeUnhandled
}
