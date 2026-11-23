// Port of mj-chat/src/components/scope.rs.

package chat

import (
	"fmt"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

// Focus, event routing, and interaction state shared by the controls.

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
	// hasItem is false when there is none.
	item     int
	hasItem  bool
	area     uv.Rectangle
	revision uint64
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

// focusFlag is the identity of one control in the focus tree (rat_focus's
// FocusFlag): a shared cell, compared by pointer.
type focusFlag struct {
	focused bool
}

// clear drops the focus state held by the flag.
func (f *focusFlag) clear() {
	f.focused = false
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
	panic("unported: mj-chat/src/components/scope.rs buildFocusTree")
}

// focused returns the flag holding focus, or nil.
func (t *focusTree) focused() *focusFlag {
	panic("unported: mj-chat/src/components/scope.rs focusTree.focused")
}

// focus moves focus to the leaf with the given flag.
func (t *focusTree) focus(flag *focusFlag) {
	panic("unported: mj-chat/src/components/scope.rs focusTree.focus")
}

// isValidWidget reports whether the flag belongs to a leaf of this tree.
func (t *focusTree) isValidWidget(flag *focusFlag) bool {
	panic("unported: mj-chat/src/components/scope.rs focusTree.isValidWidget")
}

// next focuses the next navigable leaf, reporting whether focus changed.
func (t *focusTree) next() bool {
	panic("unported: mj-chat/src/components/scope.rs focusTree.next")
}

// prev focuses the previous navigable leaf, reporting whether focus
// changed.
func (t *focusTree) prev() bool {
	panic("unported: mj-chat/src/components/scope.rs focusTree.prev")
}

// none removes focus from every leaf.
func (t *focusTree) none() {
	panic("unported: mj-chat/src/components/scope.rs focusTree.none")
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
// The zero value is not ready for use; create one with [NewForm].
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
	panic("unported: mj-chat/src/components/scope.rs Form.Viewport")
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
	panic("unported: mj-chat/src/components/scope.rs Form.SetListContents")
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

// registerWithCursorMap registers a control and the screen columns
// corresponding to field cursors.
func (f *Form[K]) registerWithCursorMap(id K, kind ControlKind, area uv.Rectangle, enabled bool, cursorMap []cursorStop) {
	panic("unported: mj-chat/src/components/scope.rs Form.registerWithCursorMap")
}

func (f *Form[K]) registerWithRegions(id K, kind ControlKind, area uv.Rectangle, enabled bool, regionMap []regionSpan) {
	panic("unported: mj-chat/src/components/scope.rs Form.registerWithRegions")
}

func (f *Form[K]) registerWithRows(id K, kind ControlKind, area uv.Rectangle, enabled bool, rowMap []*int, rowEnabled []bool) {
	panic("unported: mj-chat/src/components/scope.rs Form.registerWithRows")
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
	panic("unported: mj-chat/src/components/scope.rs Form.registerCombobox")
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
	panic("unported: mj-chat/src/components/scope.rs Form.registerWithMultilineCursorMap")
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
	panic("unported: mj-chat/src/components/scope.rs Form.DeclareWithEnabled")
}

// Clear removes all active controls while retaining their stable identities
// for future frames.
func (f *Form[K]) Clear() {
	panic("unported: mj-chat/src/components/scope.rs Form.Clear")
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

// EndFrame rebuilds the focus tree and repairs focus after controls
// appeared or disappeared.
func (f *Form[K]) EndFrame(initial K) {
	panic("unported: mj-chat/src/components/scope.rs Form.EndFrame")
}

// Focused returns the focused control, if any.
func (f *Form[K]) Focused() (K, bool) {
	panic("unported: mj-chat/src/components/scope.rs Form.Focused")
}

// Focus requests focus for a stable control id.
func (f *Form[K]) Focus(id K) {
	panic("unported: mj-chat/src/components/scope.rs Form.Focus")
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
	panic("unported: mj-chat/src/components/scope.rs Form.HandleAt")
}

func (f *Form[K]) handleInner(event tea.Msg) EventResult[Interaction[K]] {
	panic("unported: mj-chat/src/components/scope.rs Form.handleInner")
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
	panic("unported: mj-chat/src/components/scope.rs Form.SetSelected")
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
	panic("unported: mj-chat/src/components/scope.rs Form.fallback")
}

func (f *Form[K]) hit(x, y int) (pointerHit[K], bool) {
	panic("unported: mj-chat/src/components/scope.rs Form.hit")
}

func (f *Form[K]) handleKey(event tea.Msg) (EventResult[Interaction[K]], bool) {
	panic("unported: mj-chat/src/components/scope.rs Form.handleKey")
}

func (f *Form[K]) focusSibling(current K, forward bool, buttonsOnly bool) bool {
	panic("unported: mj-chat/src/components/scope.rs Form.focusSibling")
}

func (f *Form[K]) focusPendingSibling(forward bool) bool {
	panic("unported: mj-chat/src/components/scope.rs Form.focusPendingSibling")
}

func (f *Form[K]) listSelection(id K, code rune, modifiers tea.KeyMod, selected int, length int) (int, bool) {
	panic("unported: mj-chat/src/components/scope.rs Form.listSelection")
}

func (f *Form[K]) handlePointer(event tea.Msg) (EventResult[Interaction[K]], bool) {
	panic("unported: mj-chat/src/components/scope.rs Form.handlePointer")
}

func (f *Form[K]) pointerOwnerContains(owner pointerOwner[K], x, y int) bool {
	panic("unported: mj-chat/src/components/scope.rs Form.pointerOwnerContains")
}

func (f *Form[K]) clickTarget(id K, x, y int) *clickTarget[K] {
	panic("unported: mj-chat/src/components/scope.rs Form.clickTarget")
}

func (f *Form[K]) pointerInteraction(id K, x, y int) Interaction[K] {
	panic("unported: mj-chat/src/components/scope.rs Form.pointerInteraction")
}

// Clone copies the form for a draft: the focused control carries over, a
// physical pointer gesture does not.
func (f *Form[K]) Clone() Form[K] {
	panic("unported: mj-chat/src/components/scope.rs Form.Clone")
}

// Equal compares the active controls' identities, areas, enabled states and
// kinds, and the focused control.
func (f *Form[K]) Equal(other *Form[K]) bool {
	panic("unported: mj-chat/src/components/scope.rs Form.Equal")
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
	panic("unported: mj-chat/src/components/scope.rs Form.rebuildWithoutPreviousFocus")
}

func isTab(key tea.Key) bool {
	panic("unported: mj-chat/src/components/scope.rs isTab")
}

func isBackTab(key tea.Key) bool {
	panic("unported: mj-chat/src/components/scope.rs isBackTab")
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
	panic("unported: mj-chat/src/components/scope.rs listSelection")
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
	panic("unported: mj-chat/src/components/scope.rs stepRows")
}

func pageTarget[K comparable](control *scopeControl[K], current int, forward bool) (int, bool) {
	panic("unported: mj-chat/src/components/scope.rs pageTarget")
}

func tabSelection(code rune, selected int, tabs int) (int, bool) {
	panic("unported: mj-chat/src/components/scope.rs tabSelection")
}

func cursorAt[K comparable](control *scopeControl[K], x, y int) int {
	panic("unported: mj-chat/src/components/scope.rs cursorAt")
}

// ApplyFieldEdit applies a field edit to the existing readline editor.
//
// A cursor move counts as a change: callers such as the path fields
// recompute completions from the text under the cursor.
func ApplyFieldEdit(input *TextInput, edit FieldEdit) EditOutcome {
	panic("unported: mj-chat/src/components/scope.rs ApplyFieldEdit")
}
