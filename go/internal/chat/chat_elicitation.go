// Port of mj-chat/src/chat/elicitation.rs.
//
// Modal editor for ACP form elicitations.
//
// Rust's Cell and RefCell fields become plain fields mutated through the
// dialog's pointer. Key and mouse events are Bubble Tea's (tea.KeyMsg,
// tea.MouseMsg); see chat_keys.go.
package chat

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"slices"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// maximumOffscreenRows is how many wrapped rows of one logical line the
// extractor is willing to render off screen when a selection cuts it. A plan
// line long enough to wrap past this is pathological; the rows beyond it are
// dropped rather than allowed to allocate an unbounded buffer.
const maximumOffscreenRows = 4_096

// indexSet is a BTreeSet<usize>: sorted, without repeats, and written as a
// JSON array (`[]` when empty).
type indexSet []int

func (s indexSet) contains(index int) bool {
	_, found := slices.BinarySearch(s, index)
	return found
}

// insert adds index, reporting whether it was new.
func (s *indexSet) insert(index int) bool {
	at, found := slices.BinarySearch(*s, index)
	if found {
		return false
	}
	*s = slices.Insert(*s, at, index)
	return true
}

// remove drops index, reporting whether it was present.
func (s *indexSet) remove(index int) bool {
	at, found := slices.BinarySearch(*s, index)
	if !found {
		return false
	}
	*s = slices.Delete(*s, at, at+1)
	return true
}

func (s indexSet) first() (int, bool) {
	if len(s) == 0 {
		return 0, false
	}
	return s[0], true
}

func (s indexSet) clone() indexSet { return slices.Clone(s) }

func (s indexSet) MarshalJSON() ([]byte, error) {
	if s == nil {
		return []byte("[]"), nil
	}
	return json.Marshal([]int(s))
}

func (s *indexSet) UnmarshalJSON(data []byte) error {
	var values []uint
	if err := json.Unmarshal(data, &values); err != nil {
		return err
	}
	set := indexSet{}
	for _, value := range values {
		set.insert(int(value))
	}
	*s = set
	return nil
}

// fieldValue is the local answer to one field. Each variant is held by
// pointer so it can be edited in place.
type fieldValue interface{ isFieldValue() }

// fieldValueText carries user-authored form text; across a binary upgrade
// only its value is kept, not readline's internal history, undo, and cursor
// state.
type fieldValueText struct{ Input TextInput }

// fieldValueSingle is the picked option, nil for none.
type fieldValueSingle struct{ Selected *int }

type fieldValueMulti struct{ Selected indexSet }

type fieldValueBoolean struct{ Value bool }

func (*fieldValueText) isFieldValue()    {}
func (*fieldValueSingle) isFieldValue()  {}
func (*fieldValueMulti) isFieldValue()   {}
func (*fieldValueBoolean) isFieldValue() {}

// marshalFieldValue writes value externally tagged, as serde does:
// `{"Text":"..."}`, `{"Single":null}`, `{"Multi":[0,2]}`, `{"Boolean":true}`.
func marshalFieldValue(value fieldValue) ([]byte, error) {
	var tagged map[string]any
	switch value := value.(type) {
	case *fieldValueText:
		tagged = map[string]any{"Text": value.Input.Value()}
	case *fieldValueSingle:
		tagged = map[string]any{"Single": value.Selected}
	case *fieldValueMulti:
		tagged = map[string]any{"Multi": value.Selected}
	case *fieldValueBoolean:
		tagged = map[string]any{"Boolean": value.Value}
	default:
		return nil, fmt.Errorf("unknown field value %T", value)
	}
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(tagged); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(encoded.Bytes(), []byte("\n")), nil
}

// unmarshalFieldValue reads an externally tagged field value.
func unmarshalFieldValue(data []byte) (fieldValue, error) {
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return nil, err
	}
	if len(tagged) != 1 {
		return nil, fmt.Errorf("invalid type: map with %d entries, expected enum FieldValue", len(tagged))
	}
	for variant, content := range tagged {
		switch variant {
		case "Text":
			var text string
			if err := json.Unmarshal(content, &text); err != nil {
				return nil, err
			}
			return &fieldValueText{Input: TextInputFromValue(text)}, nil
		case "Single":
			var selected *uint
			if err := json.Unmarshal(content, &selected); err != nil {
				return nil, err
			}
			if selected == nil {
				return &fieldValueSingle{}, nil
			}
			index := int(*selected)
			return &fieldValueSingle{Selected: &index}, nil
		case "Multi":
			var selected indexSet
			if err := json.Unmarshal(content, &selected); err != nil {
				return nil, err
			}
			return &fieldValueMulti{Selected: selected}, nil
		case "Boolean":
			var value bool
			if err := json.Unmarshal(content, &value); err != nil {
				return nil, err
			}
			return &fieldValueBoolean{Value: value}, nil
		default:
			return nil, fmt.Errorf("unknown variant `%s`, expected one of `Text`, `Single`, `Multi`, `Boolean`", variant)
		}
	}
	panic("unreachable")
}

// cloneFieldValues is Vec<FieldValue>::clone: every value copied, so edits
// to one copy never show in the other.
func cloneFieldValues(values []fieldValue) []fieldValue {
	panic("unported: mj-chat/src/chat/elicitation.rs FieldValue Clone (needs TextInput's deep clone)")
}

type displayField struct {
	field        int
	custom       *int
	customOption *int
	// alongside: the companion goes beside any pick (claude-agent-acp's
	// "Other") instead of replacing it (core.CompanionRoleAlongside).
	alongside bool
}

type elicitationControlKind uint8

const (
	elicitationControlKindField elicitationControlKind = iota
	elicitationControlKindSubmit
	elicitationControlKindCancel
	elicitationControlKindPrevious
)

// elicitationControl is a control of the dialog's form: the field of display
// index Field, or one of the action buttons. It is comparable, so it can key
// a Form.
type elicitationControl struct {
	kind  elicitationControlKind
	field int
}

func elicitationControlField(index int) elicitationControl {
	return elicitationControl{kind: elicitationControlKindField, field: index}
}

var (
	elicitationControlSubmit   = elicitationControl{kind: elicitationControlKindSubmit}
	elicitationControlCancel   = elicitationControl{kind: elicitationControlKindCancel}
	elicitationControlPrevious = elicitationControl{kind: elicitationControlKindPrevious}
)

// ElicitationDraft is a process-local copy of an unanswered form. The request
// is retained in the snapshot deliberately: an id can be reused by a harness
// for a different form, and local answers must never be applied to that new
// form.
type ElicitationDraft struct {
	// req is Rust's field `request`, renamed because the method takes the
	// name.
	req                core.ElicitationRequest
	error              *string
	values             []fieldValue
	optionCursors      []int
	activeCustomFields indexSet
	surface            ElicitationSurface
	viewports          *elicitationViewports
	focus              int
	messageScroll      int
	// messageAnchor is `(source line, grapheme offset)` at the top of the
	// viewport. Both components are logical source coordinates; the visual
	// row is recomputed by the renderer for the current width.
	messageAnchor      *[2]int
	messageAnchorWidth int
	focusScroll        int
	currentQuestion    *int
	confirmed          indexSet
}

// elicitationDraftWire is ElicitationDraft's serialized form, field for
// field in serde's order.
type elicitationDraftWire struct {
	Request            core.ElicitationRequest `json:"request"`
	Error              *string                 `json:"error,omitempty"`
	Values             []json.RawMessage       `json:"values"`
	OptionCursors      []uint                  `json:"option_cursors"`
	ActiveCustomFields indexSet                `json:"active_custom_fields"`
	Surface            *ElicitationSurface     `json:"surface,omitempty"`
	Viewports          *elicitationViewports   `json:"viewports,omitempty"`
	Focus              uint                    `json:"focus"`
	MessageScroll      uint16                  `json:"message_scroll"`
	MessageAnchor      *[2]uint                `json:"message_anchor"`
	MessageAnchorWidth uint16                  `json:"message_anchor_width"`
	FocusScroll        uint16                  `json:"focus_scroll"`
	CurrentQuestion    *uint                   `json:"current_question"`
	Confirmed          indexSet                `json:"confirmed"`
}

func (d ElicitationDraft) MarshalJSON() ([]byte, error) {
	wire := elicitationDraftWire{
		Request:            d.req,
		Error:              d.error,
		Values:             make([]json.RawMessage, len(d.values)),
		OptionCursors:      make([]uint, len(d.optionCursors)),
		ActiveCustomFields: d.activeCustomFields,
		Viewports:          d.viewports,
		Focus:              uint(d.focus),
		MessageScroll:      uint16(d.messageScroll),
		MessageAnchor:      anchorToWire(d.messageAnchor),
		MessageAnchorWidth: uint16(d.messageAnchorWidth),
		FocusScroll:        uint16(d.focusScroll),
		Confirmed:          d.confirmed,
	}
	for i, value := range d.values {
		encoded, err := marshalFieldValue(value)
		if err != nil {
			return nil, err
		}
		wire.Values[i] = encoded
	}
	for i, cursor := range d.optionCursors {
		wire.OptionCursors[i] = uint(cursor)
	}
	if !d.surface.isConversation() {
		wire.Surface = &d.surface
	}
	if d.currentQuestion != nil {
		question := uint(*d.currentQuestion)
		wire.CurrentQuestion = &question
	}
	return json.Marshal(wire)
}

func (d *ElicitationDraft) UnmarshalJSON(data []byte) error {
	if err := requireDraftFields(data, "request", "values", "option_cursors", "active_custom_fields",
		"focus", "message_scroll", "message_anchor_width", "focus_scroll"); err != nil {
		return err
	}
	var wire elicitationDraftWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	draft := ElicitationDraft{
		req:                wire.Request,
		error:              wire.Error,
		values:             make([]fieldValue, len(wire.Values)),
		optionCursors:      make([]int, len(wire.OptionCursors)),
		activeCustomFields: wire.ActiveCustomFields,
		viewports:          wire.Viewports,
		focus:              int(wire.Focus),
		messageScroll:      int(wire.MessageScroll),
		messageAnchor:      anchorFromWire(wire.MessageAnchor),
		messageAnchorWidth: int(wire.MessageAnchorWidth),
		focusScroll:        int(wire.FocusScroll),
		confirmed:          wire.Confirmed,
	}
	for i, raw := range wire.Values {
		value, err := unmarshalFieldValue(raw)
		if err != nil {
			return err
		}
		draft.values[i] = value
	}
	for i, cursor := range wire.OptionCursors {
		draft.optionCursors[i] = int(cursor)
	}
	if wire.Surface != nil {
		draft.surface = *wire.Surface
	}
	if wire.CurrentQuestion != nil {
		question := int(*wire.CurrentQuestion)
		draft.currentQuestion = &question
	}
	*d = draft
	return nil
}

// requireDraftFields fails as serde does when a required field is missing.
func requireDraftFields(data []byte, names ...string) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	for _, name := range names {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

func anchorToWire(pair *[2]int) *[2]uint {
	if pair == nil {
		return nil
	}
	return &[2]uint{uint(pair[0]), uint(pair[1])}
}

func anchorFromWire(pair *[2]uint) *[2]int {
	if pair == nil {
		return nil
	}
	return &[2]int{int(pair[0]), int(pair[1])}
}

type elicitationViewport struct {
	messageScroll      int
	messageAnchor      *[2]int
	messageAnchorWidth int
	focusScroll        int
}

type elicitationViewportWire struct {
	MessageScroll      uint16   `json:"message_scroll"`
	MessageAnchor      *[2]uint `json:"message_anchor"`
	MessageAnchorWidth uint16   `json:"message_anchor_width"`
	FocusScroll        uint16   `json:"focus_scroll"`
}

func (v elicitationViewport) MarshalJSON() ([]byte, error) {
	return json.Marshal(elicitationViewportWire{
		MessageScroll:      uint16(v.messageScroll),
		MessageAnchor:      anchorToWire(v.messageAnchor),
		MessageAnchorWidth: uint16(v.messageAnchorWidth),
		FocusScroll:        uint16(v.focusScroll),
	})
}

func (v *elicitationViewport) UnmarshalJSON(data []byte) error {
	if err := requireDraftFields(data, "message_scroll", "message_anchor_width", "focus_scroll"); err != nil {
		return err
	}
	var wire elicitationViewportWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*v = elicitationViewport{
		messageScroll:      int(wire.MessageScroll),
		messageAnchor:      anchorFromWire(wire.MessageAnchor),
		messageAnchorWidth: int(wire.MessageAnchorWidth),
		focusScroll:        int(wire.FocusScroll),
	}
	return nil
}

type elicitationViewports struct {
	conversation elicitationViewport
	notification elicitationViewport
}

type elicitationViewportsWire struct {
	Conversation elicitationViewport `json:"conversation"`
	Notification elicitationViewport `json:"notification"`
}

func (v elicitationViewports) MarshalJSON() ([]byte, error) {
	return json.Marshal(elicitationViewportsWire{Conversation: v.conversation, Notification: v.notification})
}

func (v *elicitationViewports) UnmarshalJSON(data []byte) error {
	if err := requireDraftFields(data, "conversation", "notification"); err != nil {
		return err
	}
	var wire elicitationViewportsWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*v = elicitationViewports{conversation: wire.Conversation, notification: wire.Notification}
	return nil
}

// Matches reports whether this snapshot belongs to exactly this pending
// request.
func (d *ElicitationDraft) Matches(request *core.ElicitationRequest) bool {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDraft::matches (needs ElicitationRequest equality)")
}

func (d *ElicitationDraft) request() *core.ElicitationRequest { return &d.req }

func (d *ElicitationDraft) setResponseError(err string) { d.error = &err }

// elicitationPage is the page a view last laid out: the question and whether
// it was the unanswered-questions confirmation.
type elicitationPage struct {
	question   int
	confirming bool
}

type elicitationDialog struct {
	// req is Rust's field `request`, renamed because the method takes the
	// name.
	req                core.ElicitationRequest
	values             []fieldValue
	optionCursors      []int
	displayFields      []displayField
	activeCustomFields indexSet
	error              *string
	// currentQuestion: page navigation is independent of control focus; only
	// an explicit answer action confirms a page, and edits invalidate that
	// confirmation.
	currentQuestion      int
	confirmed            indexSet
	confirmingUnanswered bool
	focus                *elicitationControl
	view                 *elicitationView
	otherView            *elicitationView
	surface              ElicitationSurface
}

// ElicitationSurface is one of the two hosts of one logical form, each with
// independent presentation geometry. Written in snake_case; the zero value
// is the default, Conversation.
type ElicitationSurface int

const (
	ElicitationSurfaceConversation ElicitationSurface = iota
	ElicitationSurfaceNotification
)

func (s ElicitationSurface) isConversation() bool { return s == ElicitationSurfaceConversation }

func (s ElicitationSurface) MarshalText() ([]byte, error) {
	switch s {
	case ElicitationSurfaceConversation:
		return []byte("conversation"), nil
	case ElicitationSurfaceNotification:
		return []byte("notification"), nil
	}
	return nil, fmt.Errorf("unknown elicitation surface %d", int(s))
}

func (s *ElicitationSurface) UnmarshalText(text []byte) error {
	switch string(text) {
	case "conversation":
		*s = ElicitationSurfaceConversation
	case "notification":
		*s = ElicitationSurfaceNotification
	default:
		return fmt.Errorf("unknown variant `%s`, expected `conversation` or `notification`", text)
	}
	return nil
}

type elicitationView struct {
	form              *Form[elicitationControl]
	page              *elicitationPage
	messageScroll     int
	messagePageHeight int
	messageMaxScroll  int
	messageArea       *uv.Rectangle
	// messageAnchor is the source line and grapheme offset at the top of the
	// last rendered message viewport. Keeping this anchor lets a resize
	// recompute wrapped rows at the new width without moving the reader to
	// an unrelated part of a long source line.
	messageAnchor      *[2]int
	messageAnchorWidth int
	// renderedArea is the complete question panel bounds from the most
	// recent frame. This lets mouse routing distinguish its chrome from the
	// transcript above without making the message pane a component hitbox
	// (message text is still selectable).
	renderedArea *uv.Rectangle
	// focusScroll is the wrapped rows to skip in the form body when a long
	// option list is taller than the space left above the answer controls.
	focusScroll int
	// lastOptionClick is the last click on an option of a page that picks on
	// purpose. The dialog decides double-clicks there by screen cell,
	// because the first click moves the cursor and reflows the rows below
	// it: the second click lands on the same cell but often on a different
	// row.
	lastOptionClick *optionClick
}

// optionClick is a click on an option: the screen cell it landed on, the
// option drawn there at the time, and when.
type optionClick struct {
	field  int
	option int
	column int
	row    int
	at     time.Time
}

func (v *elicitationView) viewport(page elicitationPage) elicitationViewport {
	// An inactive presentation can still describe the previous page. Its
	// geometry would be reset on selection; its saved viewport must follow
	// that same rule rather than transfer the old page's scroll.
	if v.page == nil || *v.page != page {
		return elicitationViewport{}
	}
	return elicitationViewport{
		messageScroll:      v.messageScroll,
		messageAnchor:      v.messageAnchor,
		messageAnchorWidth: v.messageAnchorWidth,
		focusScroll:        v.focusScroll,
	}
}

func (v *elicitationView) restoreViewport(viewport elicitationViewport) {
	v.messageScroll = viewport.messageScroll
	v.messageAnchor = viewport.messageAnchor
	v.messageAnchorWidth = viewport.messageAnchorWidth
	v.focusScroll = viewport.focusScroll
}

func newElicitationView() *elicitationView {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationView::new")
}

func newElicitationDialog(request core.ElicitationRequest) *elicitationDialog {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::new")
}

// selectSurface makes surface the active presentation. Only the active
// presentation supplies focus. Inactive Forms retain hitboxes and pointer
// capture, and receive that focus when activated.
func (d *elicitationDialog) selectSurface(surface ElicitationSurface) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::select_surface")
}

func (d *elicitationDialog) setResponseError(err string) { d.error = &err }

func (d *elicitationDialog) isNotificationSurface() bool {
	return d.surface == ElicitationSurfaceNotification
}

// viewFor is the presentation drawn on surface.
func (d *elicitationDialog) viewFor(surface ElicitationSurface) *elicitationView {
	if d.surface == surface {
		return d.view
	}
	return d.otherView
}

func (d *elicitationDialog) pointerSurfaceIsRendered(surface ElicitationSurface) bool {
	if surface == ElicitationSurfaceNotification {
		return d.wasRenderedIn(surface)
	}
	area := d.viewFor(surface).renderedArea
	return area != nil && area.Dx() > 0 && area.Dy() > 0
}

func (d *elicitationDialog) pointerSurface() (ElicitationSurface, bool) {
	other := ElicitationSurfaceConversation
	if d.surface == ElicitationSurfaceConversation {
		other = ElicitationSurfaceNotification
	}
	switch {
	case d.pointerSurfaceIsRendered(d.surface) && d.view.form.CapturesPointer():
		return d.surface, true
	case d.pointerSurfaceIsRendered(other) && d.otherView.form.CapturesPointer():
		return other, true
	}
	return 0, false
}

func (d *elicitationDialog) request() *core.ElicitationRequest { return &d.req }

// minimumHeight is the natural height of the current question page,
// including its panel border, message, focused answer content, action row,
// and footer.
//
// Prompts use their complete wrapped height, capped by the caller to the
// available pane. Overflow scrolls independently of the controls.
func (d *elicitationDialog) minimumHeight(width int) int {
	// Match renderElicitationBody: two borders, footer, two button rows, one
	// readable message row, and the focused label/control.
	return 2 + 1 + 2 + 1 + min(elicitationFocusContent(d).wrappedHeight(max(width-2, 1)), 2)
}

func (d *elicitationDialog) naturalHeight(width int) int {
	contentWidth := max(width-2, 1)
	focusRows := elicitationFocusContent(d).wrappedHeight(contentWidth)
	messageRows := max(min(NewParagraph(RawText(d.promptText())).WithWrap(Wrap{Trim: true}).LineCount(contentWidth), math.MaxUint16), 1)
	// Two border rows, one footer row, and the two action rows match the
	// full-size renderer. The renderer collapses actions to one row only
	// when the capped pane itself is exceptionally small.
	// Rust adds in u16, saturating at its maximum.
	return min(2+1+2+messageRows+focusRows, math.MaxUint16)
}

func (d *elicitationDialog) draft() ElicitationDraft {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::draft")
}

// elicitationDialogFromDraft restores a dialog from draft, or nil when the
// draft belongs to another request or shape (ElicitationDialog::from_draft).
func elicitationDialogFromDraft(request core.ElicitationRequest, draft ElicitationDraft) *elicitationDialog {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::from_draft")
}

func (d *elicitationDialog) paste(text string) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::paste")
}

// handleKeyEvent applies a key and returns the response it completes, if
// any.
func (d *elicitationDialog) handleKeyEvent(key tea.KeyMsg) *core.ElicitationResponse {
	response := d.handleKeyInView(key)
	d.finishSurface()
	return response
}

func (d *elicitationDialog) finishSurface() {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::finish_surface")
}

func (d *elicitationDialog) handleKeyInView(key tea.KeyMsg) *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::handle_key_in_view")
}

func (d *elicitationDialog) applyInteraction(interaction Interaction[elicitationControl]) *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::apply_interaction")
}

func (d *elicitationDialog) handleMouse(mouse tea.MouseMsg) *core.ElicitationResponse {
	return d.handleMouseAt(mouse, time.Now())
}

// handleMouseAt handles a mouse event at an explicit monotonic time, so the
// double-click interval is deterministic in tests.
func (d *elicitationDialog) handleMouseAt(mouse tea.MouseMsg, now time.Time) *core.ElicitationResponse {
	response := d.handleMouseInView(mouse, now)
	d.finishSurface()
	return response
}

func (d *elicitationDialog) handleMouseInView(mouse tea.MouseMsg, now time.Time) *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::handle_mouse_in_view")
}

func (d *elicitationDialog) wasRenderedIn(surface ElicitationSurface) bool {
	view := d.viewFor(surface)
	// Answer controls always share the rendered body with the request.
	return view.renderedArea != nil && view.renderedArea.Dx() > 2 && view.renderedArea.Dy() > 2 &&
		view.messageArea != nil && view.messageArea.Dx() > 0 && view.messageArea.Dy() > 0
}

func (d *elicitationDialog) componentHandlesMouseIn(surface ElicitationSurface, column, row int) bool {
	form := d.viewFor(surface).form
	return form.CapturesPointer() || form.Contains(column, row)
}

func (d *elicitationDialog) componentHandlesMouseAt(column, row int) bool {
	return d.view.form.CapturesPointer() || d.view.form.Contains(column, row)
}

func (d *elicitationDialog) renderedAreaContains(column, row int) bool {
	return d.view.renderedArea != nil && uv.Pos(column, row).In(*d.view.renderedArea)
}

func (d *elicitationDialog) cancelPointerIn(surface ElicitationSurface) {
	view := d.viewFor(surface)
	view.form.CancelPointer()
	view.lastOptionClick = nil
}

func (d *elicitationDialog) resetComponentGeometry() {
	for _, surface := range []ElicitationSurface{ElicitationSurfaceConversation, ElicitationSurfaceNotification} {
		view := d.viewFor(surface)
		if !d.pointerSurfaceIsRendered(surface) {
			view.form.CancelPointer()
		}
		view.form.ResetGeometry()
		view.renderedArea = nil
		view.lastOptionClick = nil
	}
}

func (d *elicitationDialog) focusIndex() int {
	if d.focus == nil {
		return d.currentQuestion
	}
	switch d.focus.kind {
	case elicitationControlKindField:
		return d.focus.field
	case elicitationControlKindSubmit:
		return len(d.displayFields)
	case elicitationControlKindCancel:
		return len(d.displayFields) + 2
	}
	return d.currentQuestion
}

func (d *elicitationDialog) focusControl(index int) {
	fieldCount := len(d.displayFields)
	var control elicitationControl
	switch {
	case index < fieldCount:
		control = elicitationControlField(index)
	case index == fieldCount:
		control = elicitationControlSubmit
	default:
		// Older upgrade drafts used fieldCount + 1 for the removed Skip action.
		control = elicitationControlCancel
	}
	if index < fieldCount && index != d.currentQuestion {
		d.currentQuestion = index
		d.resetPage()
	}
	d.focus = &control
	d.view.form.Focus(control)
}

// elicitationAction is one button of the dialog's action row: its control,
// label, and whether it is enabled (Rust's `(ElicitationControl, &str,
// bool)`).
type elicitationAction struct {
	control elicitationControl
	label   string
	enabled bool
}

func (d *elicitationDialog) actions() [3]elicitationAction {
	return [3]elicitationAction{
		{elicitationControlSubmit, "Answer & Next", true},
		{elicitationControlPrevious, "Previous", d.confirmingUnanswered || d.currentQuestion > 0},
		{elicitationControlCancel, "Cancel", true},
	}
}

func (d *elicitationDialog) resetControls() {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::reset_controls")
}

func (d *elicitationDialog) resetPage() {
	d.view.lastOptionClick = nil
	d.view.messageScroll = 0
	d.view.messageAnchor = nil
	d.view.messageAnchorWidth = 0
	d.view.focusScroll = 0
	d.resetControls()
}

func (d *elicitationDialog) navigate(forward bool) {
	next := max(d.currentQuestion-1, 0)
	if forward {
		next = min(d.currentQuestion+1, max(len(d.displayFields)-1, 0))
	}
	if next != d.currentQuestion {
		d.currentQuestion = next
		d.error = nil
		d.resetPage()
	}
}

// unanswered is the display indexes of the questions not yet confirmed, in
// order.
func (d *elicitationDialog) unanswered() []int {
	var indexes []int
	for index := range d.displayFields {
		if !d.confirmed.contains(index) {
			indexes = append(indexes, index)
		}
	}
	return indexes
}

func (d *elicitationDialog) goBack() {
	unanswered := d.unanswered()
	d.confirmingUnanswered = false
	if len(unanswered) > 0 {
		d.currentQuestion = unanswered[0]
	}
	d.resetPage()
}

func (d *elicitationDialog) promptText() string {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::prompt_text")
}

func (d *elicitationDialog) selectOption(selected int) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::select_option")
}

func (d *elicitationDialog) isPlanReview() bool { return core.IsPlanReviewID(d.req.ID) }

// messageArea is the message pane's content area, recorded by the last
// frame.
func (d *elicitationDialog) messageArea() (uv.Rectangle, bool) {
	if d.view.messageArea == nil {
		return uv.Rectangle{}, false
	}
	return *d.view.messageArea, true
}

// selectionText is the message text a selection covers, reconstructed as
// source lines.
//
// Wrapping is a rendering artifact, so a logical line the range covers whole
// comes back exactly as the message wrote it, without the newlines word wrap
// introduced. Only the range's partial endpoints are cut, and those go back
// through the same Paragraph pipeline the pane renders with, so wide
// characters are sliced on the cells they actually occupy.
func (d *elicitationDialog) selectionText(selection *SelectionRange, width int) string {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::selection_text")
}

func (d *elicitationDialog) messagePageStep() int {
	return max(d.view.messagePageHeight-1, 1)
}

func (d *elicitationDialog) scrollMessage(delta int) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::scroll_message")
}

// clickOptionOnPurpose is a single click on row of a page that picks on
// purpose, the list's own activation included. On an option it toggles the
// option as Space does and records the click, so a second click on the same
// cell is a double-click on this option. On the text row it starts writing.
func (d *elicitationDialog) clickOptionOnPurpose(index, row int, mouse tea.Mouse, now time.Time) *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::click_option_on_purpose")
}

// doubleClickOption is a double-click on option of display field index: it
// ends picked and the question is answered, as a double-click does on any
// other list.
func (d *elicitationDialog) doubleClickOption(index, option int) *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::double_click_option")
}

// picksOnPurpose reports whether display is a single-select beside its own
// text field, where a pick is deliberate: ↑/↓ move only the cursor, Space or
// a click toggles the option under it, and Enter answers with what is there.
func (d *elicitationDialog) picksOnPurpose(display displayField) bool {
	_, single := d.values[display.field].(*fieldValueSingle)
	return display.alongside && single
}

// pickCursorWhenEmpty: Enter on a page that picks on purpose answers with
// the pick and the text already there. Only when that answer would be
// empty, with nothing picked and the text blank, does it take the option
// under the cursor, as Enter does on any other list.
func (d *elicitationDialog) pickCursorWhenEmpty() {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::pick_cursor_when_empty")
}

func (d *elicitationDialog) toggleCurrent() {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::toggle_current")
}

func (d *elicitationDialog) editText(key tea.KeyMsg) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::edit_text")
}

// questionContent is the answer the question at displayIndex gives, keyed by
// field id, or why it cannot be sent.
func (d *elicitationDialog) questionContent(displayIndex int) (map[string]core.ElicitationValue, error) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::question_content")
}

func (d *elicitationDialog) answerAndNext() *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::answer_and_next")
}

func (d *elicitationDialog) accept() *core.ElicitationResponse {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::accept")
}

// editableField is the field whose text the keyboard edits, and whether it
// is a custom answer beside a choice.
func (d *elicitationDialog) editableField() (field int, custom bool, ok bool) {
	panic("unported: mj-chat/src/chat/elicitation.rs ElicitationDialog::editable_field")
}

// wrappedRowCount is the rows one source line takes when the message pane
// wraps it.
//
// Each input line wraps on its own, so these counts compose: their prefix
// sums are the visual rows of the whole message.
func wrappedRowCount(line string, width int) int {
	return NewParagraph(RawText(line)).WithWrap(Wrap{Trim: true}).LineCount(width)
}

// partialLineText is the part of one logical line a range covers, cut on
// cell boundaries.
//
// The line is re-rendered alone, scrolled to the first covered row, so the
// engine can slice the same cells the pane drew. Word wrap consumed the
// spaces it broke on, so the covered rows rejoin with one space.
func partialLineText(line string, width, base int, covered [2]int, selection *SelectionRange) string {
	panic("unported: mj-chat/src/chat/elicitation.rs partial_line_text")
}

func displayFields(request *core.ElicitationRequest, values []fieldValue) ([]displayField, indexSet) {
	panic("unported: mj-chat/src/chat/elicitation.rs display_fields")
}

func selectOptionCount(field *core.ElicitationField) (int, bool) {
	switch kind := field.Kind.(type) {
	case core.ElicitationFieldKindSingleSelect:
		return len(kind.Options), true
	case core.ElicitationFieldKindMultiSelect:
		return len(kind.Options), true
	}
	return 0, false
}

func selectOptionIndex(field *core.ElicitationField, value string) (int, bool) {
	kind, ok := field.Kind.(core.ElicitationFieldKindSingleSelect)
	if !ok {
		return 0, false
	}
	for index, option := range kind.Options {
		if option.Value == value {
			return index, true
		}
	}
	return 0, false
}

func displayControlKind(request *core.ElicitationRequest, values []fieldValue, cursors []int, display displayField) ControlKind {
	panic("unported: mj-chat/src/chat/elicitation.rs display_control_kind")
}

func defaultValue(field *core.ElicitationField) fieldValue {
	panic("unported: mj-chat/src/chat/elicitation.rs default_value")
}

// validatedValue is the value to send for field, nil when an optional field
// is left empty, or why it cannot be sent.
func validatedValue(field *core.ElicitationField, value fieldValue) (core.ElicitationValue, error) {
	panic("unported: mj-chat/src/chat/elicitation.rs validated_value")
}

// renderElicitationIn draws an elicitation in the bounds owned by the chat
// surface. This is used by the combined dashboard so the question cannot
// cover its navigator or the other support panes.
func renderElicitationIn(frame *Frame, dialog *elicitationDialog, surfaces *FrameSurfaces, bounds uv.Rectangle, focused bool) {
	renderElicitationAt(frame, dialog, surfaces, bounds, focused)
	dialog.finishSurface()
}

func renderElicitationAt(frame *Frame, dialog *elicitationDialog, surfaces *FrameSurfaces, area uv.Rectangle, focused bool) {
	panic("unported: mj-chat/src/chat/elicitation.rs render_elicitation_at")
}

func renderElicitationBody(frame *Frame, dialog *elicitationDialog, surfaces *FrameSurfaces, inner uv.Rectangle, focused bool, focus focusContent, focusedField int) {
	panic("unported: mj-chat/src/chat/elicitation.rs render_elicitation_body")
}

func wrappedPrefixRows(message string, width, lineCount int) int {
	rows := 0
	for index, line := range strings.Split(message, "\n") {
		if index >= lineCount {
			break
		}
		rows += wrappedRowCount(line, width)
	}
	return min(rows, math.MaxUint16)
}

func messagePositionAtRow(message string, width, row int) (int, int) {
	panic("unported: mj-chat/src/chat/elicitation.rs message_position_at_row")
}

// textCursor is where the text cursor sits in a focus body: the body line
// and the grapheme offset in its field (Rust's `(u16, usize)`).
type textCursor struct {
	line   int
	column int
}

type focusContent struct {
	// title is the field position and title. It is pinned above the
	// scrolling field body so a control can never be shown without the
	// question it answers.
	title      *Line
	lines      []Line
	textCursor *textCursor
	focusedRow *int
	centered   bool
	optionRows []*int
}

// wrappedHeight is the rows the pinned title and the field body occupy at
// width.
func (c focusContent) wrappedHeight(width int) int {
	body := max(min(NewParagraph(TextFrom(c.lines...)).WithWrap(Wrap{Trim: false}).LineCount(width), math.MaxUint16), 1)
	if c.title != nil {
		return min(body+1, math.MaxUint16)
	}
	return body
}

// elicitationFocusContent is what the focused question's answer area shows
// (Rust's focus_content; renamed because the type focusContent takes the
// name).
func elicitationFocusContent(dialog *elicitationDialog) focusContent {
	panic("unported: mj-chat/src/chat/elicitation.rs focus_content")
}

func renderFocus(frame *Frame, area uv.Rectangle, content *focusContent, scroll int, focused bool) {
	panic("unported: mj-chat/src/chat/elicitation.rs render_focus")
}

func renderCustomAnswer(lines []Line, cursor *textCursor, dialog *elicitationDialog, display displayField, optionCount int, unselectedMarker, selectedMarker string) ([]Line, *textCursor) {
	panic("unported: mj-chat/src/chat/elicitation.rs render_custom_answer")
}

func renderCustomText(lines []Line, cursor *textCursor, dialog *elicitationDialog, customIndex int) ([]Line, *textCursor) {
	panic("unported: mj-chat/src/chat/elicitation.rs render_custom_text")
}
