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
	"maps"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

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
	if values == nil {
		return nil
	}
	clones := make([]fieldValue, len(values))
	for i, value := range values {
		switch value := value.(type) {
		case *fieldValueText:
			// TextInput's history is shared copy-on-write; its other
			// pointers are copied so neither copy's edits reach the other.
			input := value.Input
			if input.maxChars != nil {
				limit := *input.maxChars
				input.maxChars = &limit
			}
			if input.preferredColumn != nil {
				column := *input.preferredColumn
				input.preferredColumn = &column
			}
			clones[i] = &fieldValueText{Input: input}
		case *fieldValueSingle:
			clone := &fieldValueSingle{}
			if value.Selected != nil {
				selected := *value.Selected
				clone.Selected = &selected
			}
			clones[i] = clone
		case *fieldValueMulti:
			clones[i] = &fieldValueMulti{Selected: value.Selected.clone()}
		case *fieldValueBoolean:
			clones[i] = &fieldValueBoolean{Value: value.Value}
		}
	}
	return clones
}

// fieldValuesEqual is Vec<FieldValue>'s PartialEq.
func fieldValuesEqual(a, b []fieldValue) bool {
	return slices.EqualFunc(a, b, func(a, b fieldValue) bool {
		switch a := a.(type) {
		case *fieldValueText:
			b, ok := b.(*fieldValueText)
			return ok && a.Input.Equal(&b.Input)
		case *fieldValueSingle:
			b, ok := b.(*fieldValueSingle)
			return ok && optionalIntEqual(a.Selected, b.Selected)
		case *fieldValueMulti:
			b, ok := b.(*fieldValueMulti)
			return ok && slices.Equal(a.Selected, b.Selected)
		case *fieldValueBoolean:
			b, ok := b.(*fieldValueBoolean)
			return ok && a.Value == b.Value
		}
		return false
	})
}

func optionalIntEqual(a, b *int) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// Equal is ElicitationDraft's PartialEq.
func (d *ElicitationDraft) Equal(other *ElicitationDraft) bool {
	viewportEqual := func(a, b elicitationViewport) bool {
		return a.messageScroll == b.messageScroll && anchorEqual(a.messageAnchor, b.messageAnchor) &&
			a.messageAnchorWidth == b.messageAnchorWidth && a.focusScroll == b.focusScroll
	}
	viewportsEqual := d.viewports == nil && other.viewports == nil
	if d.viewports != nil && other.viewports != nil {
		viewportsEqual = viewportEqual(d.viewports.conversation, other.viewports.conversation) &&
			viewportEqual(d.viewports.notification, other.viewports.notification)
	}
	return d.req.Equal(&other.req) &&
		((d.error == nil && other.error == nil) || (d.error != nil && other.error != nil && *d.error == *other.error)) &&
		fieldValuesEqual(d.values, other.values) &&
		slices.Equal(d.optionCursors, other.optionCursors) &&
		slices.Equal(d.activeCustomFields, other.activeCustomFields) &&
		d.surface == other.surface && viewportsEqual &&
		d.focus == other.focus && d.messageScroll == other.messageScroll &&
		anchorEqual(d.messageAnchor, other.messageAnchor) &&
		d.messageAnchorWidth == other.messageAnchorWidth && d.focusScroll == other.focusScroll &&
		optionalIntEqual(d.currentQuestion, other.currentQuestion) &&
		slices.Equal(d.confirmed, other.confirmed)
}

func anchorEqual(a, b *[2]int) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
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
	return d.req.Equal(request)
}

func (d *ElicitationDraft) request() *core.ElicitationRequest { return &d.req }

// clone is ElicitationDraft's Clone: a copy whose answers can be edited
// without changing this draft.
func (d *ElicitationDraft) clone() ElicitationDraft {
	clone := *d
	if d.error != nil {
		message := *d.error
		clone.error = &message
	}
	clone.values = cloneFieldValues(d.values)
	clone.optionCursors = slices.Clone(d.optionCursors)
	clone.activeCustomFields = d.activeCustomFields.clone()
	clone.confirmed = d.confirmed.clone()
	if d.viewports != nil {
		viewports := *d.viewports
		clone.viewports = &viewports
	}
	return clone
}

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
	form := NewForm[elicitationControl]()
	return &elicitationView{form: &form}
}

func newElicitationDialog(request core.ElicitationRequest) *elicitationDialog {
	values := make([]fieldValue, len(request.Fields))
	for i := range request.Fields {
		values[i] = defaultValue(&request.Fields[i])
	}
	optionCursors := make([]int, len(values))
	for i, value := range values {
		switch value := value.(type) {
		case *fieldValueSingle:
			if value.Selected != nil {
				optionCursors[i] = *value.Selected
			}
		case *fieldValueMulti:
			if first, ok := value.Selected.first(); ok {
				optionCursors[i] = first
			}
		}
	}
	fields, activeCustomFields := displayFields(&request, values)
	for _, display := range fields {
		if display.custom == nil {
			continue
		}
		owner := &request.Fields[display.field]
		optionCount, isSelect := selectOptionCount(owner)
		if activeCustomFields.contains(*display.custom) && !display.alongside && isSelect {
			cursor := optionCount
			if display.customOption != nil {
				cursor = *display.customOption
			}
			optionCursors[display.field] = cursor
			// A free-text alternative picks no option; detail tied to an
			// option picks that option.
			if single, ok := values[display.field].(*fieldValueSingle); ok {
				single.Selected = cloneOptionalInt(display.customOption)
			}
		}
	}
	dialog := &elicitationDialog{
		req:                request,
		values:             values,
		optionCursors:      optionCursors,
		displayFields:      fields,
		activeCustomFields: activeCustomFields,
		confirmed:          indexSet{},
		view:               newElicitationView(),
		otherView:          newElicitationView(),
		surface:            ElicitationSurfaceConversation,
	}
	dialog.resetControls()
	return dialog
}

func cloneOptionalInt(value *int) *int {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// currentPage is the page the dialog is on: the question, and whether it is
// the unanswered-questions confirmation.
func (d *elicitationDialog) currentPage() elicitationPage {
	return elicitationPage{question: d.currentQuestion, confirming: d.confirmingUnanswered}
}

// selectSurface makes surface the active presentation. Only the active
// presentation supplies focus. Inactive Forms retain hitboxes and pointer
// capture, and receive that focus when activated.
func (d *elicitationDialog) selectSurface(surface ElicitationSurface) {
	if d.surface == surface {
		return
	}
	focus := d.focus
	d.view, d.otherView = d.otherView, d.view
	d.surface = surface
	// A view still on the current page keeps the registration it last
	// rendered: that is what is on screen, so it is what clicks must hit.
	// Redeclaring with the logical option count would drop the rendered row
	// map of a list whose options wrap or carry descriptions.
	if d.view.page == nil || *d.view.page != d.currentPage() {
		d.view = newElicitationView()
		d.resetControls()
	}
	if focus != nil {
		d.view.form.Focus(*focus)
	}
	d.focus = focus
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
	conversation, notification := d.view, d.otherView
	if d.surface != ElicitationSurfaceConversation {
		conversation, notification = d.otherView, d.view
	}
	var viewports *elicitationViewports
	if notification.page != nil {
		page := d.currentPage()
		viewports = &elicitationViewports{
			conversation: conversation.viewport(page),
			notification: notification.viewport(page),
		}
	}
	question := d.currentQuestion
	var err *string
	if d.error != nil {
		message := *d.error
		err = &message
	}
	return ElicitationDraft{
		req:                d.req,
		error:              err,
		values:             cloneFieldValues(d.values),
		optionCursors:      slices.Clone(d.optionCursors),
		activeCustomFields: d.activeCustomFields.clone(),
		surface:            d.surface,
		viewports:          viewports,
		focus:              d.focusIndex(),
		messageScroll:      d.view.messageScroll,
		messageAnchor:      d.view.messageAnchor,
		messageAnchorWidth: d.view.messageAnchorWidth,
		focusScroll:        d.view.focusScroll,
		currentQuestion:    &question,
		confirmed:          d.confirmed.clone(),
	}
}

// elicitationDialogFromDraft restores a dialog from draft, or nil when the
// draft belongs to another request or shape (ElicitationDialog::from_draft).
func elicitationDialogFromDraft(request core.ElicitationRequest, draft ElicitationDraft) *elicitationDialog {
	if !draft.req.Equal(&request) || len(draft.values) != len(request.Fields) || len(draft.optionCursors) != len(request.Fields) {
		return nil
	}
	viewports := draft.viewports
	surface := draft.surface
	dialog := newElicitationDialog(request)
	dialog.error = draft.error
	// Rust moves the draft in; the dialog takes copies so a draft the caller
	// keeps is never edited through the dialog.
	dialog.values = cloneFieldValues(draft.values)
	dialog.optionCursors = slices.Clone(draft.optionCursors)
	dialog.activeCustomFields = draft.activeCustomFields.clone()
	if dialog.activeCustomFields == nil {
		dialog.activeCustomFields = indexSet{}
	}
	question := draft.focus
	if draft.currentQuestion != nil {
		question = *draft.currentQuestion
	}
	dialog.currentQuestion = min(question, max(len(dialog.displayFields)-1, 0))
	dialog.confirmed = indexSet{}
	for _, index := range draft.confirmed {
		if index < len(dialog.displayFields) {
			dialog.confirmed.insert(index)
		}
	}
	dialog.resetControls()
	dialog.focusControl(min(draft.focus, len(dialog.displayFields)+2))
	dialog.selectSurface(draft.surface)
	dialog.view.messageScroll = draft.messageScroll
	dialog.view.messageAnchor = draft.messageAnchor
	dialog.view.messageAnchorWidth = draft.messageAnchorWidth
	dialog.view.focusScroll = draft.focusScroll
	if viewports != nil {
		dialog.selectSurface(ElicitationSurfaceConversation)
		dialog.view.restoreViewport(viewports.conversation)
		dialog.selectSurface(ElicitationSurfaceNotification)
		dialog.view.restoreViewport(viewports.notification)
		dialog.selectSurface(surface)
	}
	return dialog
}

func (d *elicitationDialog) paste(text string) {
	d.view.lastOptionClick = nil
	text = SingleLinePaste(sanitizeTerminalText(text))
	if text == "" {
		return
	}
	field, custom, ok := d.editableField()
	if !ok {
		return
	}
	if custom {
		d.activeCustomFields.insert(field)
	}
	if field < len(d.values) {
		if value, ok := d.values[field].(*fieldValueText); ok {
			value.Input.InsertStr(text)
			d.confirmed.remove(d.currentQuestion)
			d.error = nil
		}
	}
}

// handleKeyEvent applies a key and returns the response it completes, if
// any.
func (d *elicitationDialog) handleKeyEvent(key tea.KeyMsg) *core.ElicitationResponse {
	response := d.handleKeyInView(key)
	d.finishSurface()
	return response
}

func (d *elicitationDialog) finishSurface() {
	form := d.view.form
	// A compact presentation without an action bar (Cancel is always drawn
	// with the bar) keeps its field fallback local, so a button focused on
	// the other surface stays focused there.
	barDrawn := form.IsEnabled(elicitationControlCancel)
	focusedButton := d.focus != nil && d.focus.kind != elicitationControlKindField
	if barDrawn || !focusedButton {
		if focused, ok := form.Focused(); ok {
			d.focus = &focused
		} else {
			d.focus = nil
		}
	}
}

func (d *elicitationDialog) handleKeyInView(key tea.KeyMsg) *core.ElicitationResponse {
	if _, release := key.(tea.KeyReleaseMsg); release {
		return nil
	}
	pressed := key.Key()
	code, _ := plainKeypadCode(typedKeyCode(pressed))
	code, modifiers := normalizeKey(code, pressed.Mod)
	modifiers = withoutLockModifiers(modifiers)
	pressed.Code, pressed.Mod = code, modifiers
	// A key between two clicks makes them two separate clicks.
	d.view.lastOptionClick = nil
	if !pressed.IsRepeat {
		if d.confirmingUnanswered && code == tea.KeyEscape {
			d.goBack()
			return nil
		}
		if !d.confirmingUnanswered {
			if modifiers == tea.ModAlt && (code == tea.KeyLeft || code == tea.KeyRight) {
				d.navigate(code == tea.KeyRight)
				return nil
			}
			if focused, ok := d.view.form.Focused(); code == tea.KeyEnter && modifiers == 0 &&
				ok && focused.kind == elicitationControlKindField {
				d.pickCursorWhenEmpty()
				return d.answerAndNext()
			}
		}
	}
	switch code {
	case tea.KeyPgUp:
		d.scrollMessage(-d.messagePageStep())
		return nil
	case tea.KeyPgDown:
		d.scrollMessage(d.messagePageStep())
		return nil
	}
	d.error = nil
	// Inline custom answers retain their parent question's focus. Text
	// editing and option navigation are distinct operations on that page.
	if _, custom, ok := d.editableField(); ok && custom &&
		code != tea.KeyTab && code != tea.KeyEnter && code != tea.KeyEscape &&
		((code != tea.KeyUp && code != tea.KeyDown) || modifiers != 0) {
		d.view.form.CancelPointer()
		d.editText(tea.KeyPressMsg(pressed))
		return nil
	}
	result := d.view.form.Handle(tea.KeyPressMsg(pressed))
	if result.Action != nil {
		return d.applyInteraction(result.Action)
	}
	return nil
}

func (d *elicitationDialog) applyInteraction(interaction Interaction[elicitationControl]) *core.ElicitationResponse {
	cancel := &core.ElicitationResponse{Action: core.ElicitationResponseCancel}
	switch interaction := interaction.(type) {
	case InteractionActivate[elicitationControl]:
		switch interaction.ID.kind {
		case elicitationControlKindSubmit:
			if d.confirmingUnanswered {
				return d.accept()
			}
			return d.answerAndNext()
		case elicitationControlKindPrevious:
			if d.confirmingUnanswered {
				d.goBack()
			} else {
				d.navigate(false)
			}
			return nil
		case elicitationControlKindCancel:
			return cancel
		case elicitationControlKindField:
			d.pickCursorWhenEmpty()
			return d.answerAndNext()
		}
	case InteractionCancel[elicitationControl]:
		return cancel
	case InteractionToggle[elicitationControl]:
		if interaction.ID.kind == elicitationControlKindField {
			d.focusControl(interaction.ID.field)
			d.toggleCurrent()
		}
	case InteractionSelect[elicitationControl]:
		if interaction.ID.kind == elicitationControlKindField {
			d.focusControl(interaction.ID.field)
			d.selectOption(interaction.Index)
		}
	case InteractionEdit[elicitationControl]:
		if interaction.ID.kind != elicitationControlKindField {
			return nil
		}
		d.focusControl(interaction.ID.field)
		switch edit := interaction.Edit.(type) {
		case FieldEditKey:
			d.editText(edit.Key)
		case FieldEditPaste:
			d.paste(edit.Text)
		case FieldEditCursor:
			if field, _, ok := d.editableField(); ok {
				if value, ok := d.values[field].(*fieldValueText); ok {
					value.Input.SetCursor(edit.Offset)
				}
			}
		}
	}
	return nil
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
	result := d.view.form.HandleAt(mouse, now)
	event := mouse.Mouse()
	_, release := mouse.(tea.MouseReleaseMsg)
	click := release && event.Button == tea.MouseLeft
	_, wheel := mouse.(tea.MouseWheelMsg)
	if wheel {
		d.view.lastOptionClick = nil
	}
	// The dialog alone decides double-clicks on pages that pick on purpose:
	// a second click on the cell of the last option click, within the
	// interval, is a double-click on the option drawn there then, whatever
	// the first click has since reflowed onto that cell.
	if click && d.view.lastOptionClick != nil {
		first := *d.view.lastOptionClick
		d.view.lastOptionClick = nil
		if first.column == event.X && first.row == event.Y && now.Sub(first.at) <= DoubleClickInterval {
			return d.doubleClickOption(first.field, first.option)
		}
	}
	if result.Action != nil {
		interaction := result.Action
		clickedIndex, clickedRow, clicked := 0, 0, false
		if click {
			switch interaction := interaction.(type) {
			case InteractionSelect[elicitationControl]:
				if interaction.ID.kind == elicitationControlKindField {
					clickedIndex, clickedRow, clicked = interaction.ID.field, interaction.Index, true
				}
			case InteractionActivate[elicitationControl]:
				if interaction.ID.kind == elicitationControlKindField {
					if row, ok := d.view.form.Selected(interaction.ID); ok {
						clickedIndex, clickedRow, clicked = interaction.ID.field, row, true
					}
				}
			}
		}
		if clicked && clickedIndex < len(d.displayFields) && d.picksOnPurpose(d.displayFields[clickedIndex]) {
			return d.clickOptionOnPurpose(clickedIndex, clickedRow, event, now)
		}
		// A click on a multi-select option toggles it, as Space does.
		toggle := false
		if selectInteraction, ok := interaction.(InteractionSelect[elicitationControl]); ok && click &&
			selectInteraction.ID.kind == elicitationControlKindField && selectInteraction.ID.field < len(d.displayFields) {
			_, toggle = d.values[d.displayFields[selectInteraction.ID.field].field].(*fieldValueMulti)
		}
		response := d.applyInteraction(interaction)
		if toggle {
			if custom, isCustom, ok := d.editableField(); ok && isCustom {
				d.activeCustomFields.insert(custom)
			} else {
				d.toggleCurrent()
			}
		}
		return response
	}
	if result.Consumed {
		return nil
	}
	if d.view.messageArea == nil || !uv.Pos(event.X, event.Y).In(*d.view.messageArea) {
		return nil
	}
	if wheel {
		switch event.Button {
		case tea.MouseWheelUp:
			d.scrollMessage(-3)
		case tea.MouseWheelDown:
			d.scrollMessage(3)
		}
	}
	return nil
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
	page := d.currentPage()
	d.view.page = &page
	form := NewForm[elicitationControl]()
	first := elicitationControlSubmit
	switch {
	case d.confirmingUnanswered:
		first = elicitationControlPrevious
	case d.currentQuestion < len(d.displayFields):
		id := elicitationControlField(d.currentQuestion)
		form.Declare(id, displayControlKind(&d.req, d.values, d.optionCursors, d.displayFields[d.currentQuestion]))
		first = id
	}
	for _, action := range d.actions() {
		form.DeclareWithEnabled(action.control, ControlKindButton{}, action.enabled)
	}
	form.EndFrame(first)
	d.view.form = &form
	d.finishSurface()
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
	if d.confirmingUnanswered {
		unanswered := d.unanswered()
		text := fmt.Sprintf("Submit with %d unanswered questions?\n\n", len(unanswered))
		for _, index := range unanswered {
			text += fmt.Sprintf("%d. %s\n", index+1, d.req.Fields[d.displayFields[index].field].Title)
		}
		return text
	}
	if !d.isPlanReview() && d.currentQuestion < len(d.displayFields) {
		field := &d.req.Fields[d.displayFields[d.currentQuestion].field]
		text := field.Title
		if field.Required {
			text += " (required)"
		}
		if field.Description != nil {
			text += "\n" + *field.Description
		}
		return text + "\n\n" + d.req.Message
	}
	return d.req.Message
}

func (d *elicitationDialog) selectOption(selected int) {
	index := d.focusIndex()
	if index >= len(d.displayFields) {
		return
	}
	display := d.displayFields[index]
	optionCount, _ := selectOptionCount(&d.req.Fields[display.field])
	count := optionCount
	if display.custom != nil && display.customOption == nil {
		count++
	}
	if selected >= count {
		return
	}
	d.optionCursors[display.field] = selected
	// Beside its own text field, a pick is a deliberate act (Space, a click,
	// or Enter with nothing picked): moving the cursor, on the way to the
	// text row or back, never picks an option the user did not choose.
	if display.alongside {
		return
	}
	value, ok := d.values[display.field].(*fieldValueSingle)
	if !ok {
		return
	}
	var next *int
	if selected < optionCount {
		next = &selected
	}
	if !optionalIntEqual(value.Selected, next) {
		d.confirmed.remove(d.currentQuestion)
	}
	value.Selected = next
	if display.custom != nil {
		if (display.customOption != nil && *display.customOption == selected) || selected == optionCount {
			d.activeCustomFields.insert(*display.custom)
		} else {
			d.activeCustomFields.remove(*display.custom)
		}
	}
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
	if width == 0 {
		return ""
	}
	var selected []string
	base := 0
	for _, line := range strings.Split(d.promptText(), "\n") {
		rows := wrappedRowCount(line, width)
		lastRow := base + max(rows-1, 0)
		if rows == 0 || base > selection.End.Row || lastRow < selection.Start.Row {
			base += rows
			continue
		}
		first := max(selection.Start.Row, base)
		last := min(selection.End.Row, lastRow)
		whole := first == base && last == lastRow
		for row := first; whole && row <= last; row++ {
			start, end, ok := selection.ColumnsOn(row, width)
			whole = ok && start == 0 && end == width-1
		}
		if whole {
			selected = append(selected, line)
		} else {
			selected = append(selected, partialLineText(line, width, base, [2]int{first, last}, selection))
		}
		base += rows
		if base > selection.End.Row {
			break
		}
	}
	return strings.Join(selected, "\n")
}

func (d *elicitationDialog) messagePageStep() int {
	return max(d.view.messagePageHeight-1, 1)
}

func (d *elicitationDialog) scrollMessage(delta int) {
	current := d.view.messageScroll
	maximum := d.view.messageMaxScroll
	// Rust saturates in both directions: current.saturating_sub(|delta|)
	// upward, and current.saturating_add(delta).min(maximum) downward. A
	// non-negative current plus a negative delta cannot overflow; a positive
	// delta is compared against the remaining room instead of added.
	var next int
	if delta < 0 {
		next = max(current+delta, 0)
	} else if delta > maximum-current {
		next = maximum
	} else {
		next = current + delta
	}
	if next == current {
		return
	}
	d.view.messageScroll = next
	// A user scroll establishes a new logical anchor. Subsequent redraws and
	// resizes retain that source position instead of repeatedly replacing it
	// with whichever wrapped row happens to be at the top.
	if d.view.messageAnchorWidth > 0 {
		line, offset := messagePositionAtRow(d.promptText(), d.view.messageAnchorWidth, next)
		d.view.messageAnchor = &[2]int{line, offset}
	}
}

// clickOptionOnPurpose is a single click on row of a page that picks on
// purpose, the list's own activation included. On an option it toggles the
// option as Space does and records the click, so a second click on the same
// cell is a double-click on this option. On the text row it starts writing.
func (d *elicitationDialog) clickOptionOnPurpose(index, row int, mouse tea.Mouse, now time.Time) *core.ElicitationResponse {
	d.focusControl(index)
	d.selectOption(row)
	display := d.displayFields[index]
	optionCount, _ := selectOptionCount(&d.req.Fields[display.field])
	if row >= optionCount {
		if custom, isCustom, ok := d.editableField(); ok && isCustom {
			d.activeCustomFields.insert(custom)
		}
		return nil
	}
	d.toggleCurrent()
	d.view.lastOptionClick = &optionClick{field: index, option: row, column: mouse.X, row: mouse.Y, at: now}
	return nil
}

// doubleClickOption is a double-click on option of display field index: it
// ends picked and the question is answered, as a double-click does on any
// other list.
func (d *elicitationDialog) doubleClickOption(index, option int) *core.ElicitationResponse {
	d.focusControl(index)
	display := d.displayFields[index]
	d.optionCursors[display.field] = option
	if single, ok := d.values[display.field].(*fieldValueSingle); ok &&
		(single.Selected == nil || *single.Selected != option) {
		single.Selected = &option
		d.confirmed.remove(d.currentQuestion)
	}
	return d.answerAndNext()
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
	index := d.focusIndex()
	if index >= len(d.displayFields) {
		return
	}
	display := d.displayFields[index]
	if !d.picksOnPurpose(display) {
		return
	}
	cursor := d.optionCursors[display.field]
	optionCount, _ := selectOptionCount(&d.req.Fields[display.field])
	textBlank := true
	if display.custom != nil {
		value, ok := d.values[*display.custom].(*fieldValueText)
		textBlank = ok && strings.TrimSpace(value.Input.Value()) == ""
	}
	if single, ok := d.values[display.field].(*fieldValueSingle); ok && cursor < optionCount && textBlank && single.Selected == nil {
		single.Selected = &cursor
		d.confirmed.remove(d.currentQuestion)
	}
}

func (d *elicitationDialog) toggleCurrent() {
	if _, _, ok := d.editableField(); ok {
		d.editText(tea.KeyPressMsg{Code: ' ', Text: " "})
		return
	}
	index := d.focusIndex()
	if index >= len(d.displayFields) {
		return
	}
	display := d.displayFields[index]
	d.confirmed.remove(d.currentQuestion)
	switch value := d.values[display.field].(type) {
	case *fieldValueSingle:
		cursor := d.optionCursors[display.field]
		if display.alongside {
			// Beside its own text field a pick can be taken back, so the
			// text alone can be the whole answer.
			if value.Selected != nil && *value.Selected == cursor {
				value.Selected = nil
			} else {
				value.Selected = &cursor
			}
			return
		}
		value.Selected = &cursor
		if display.custom != nil {
			d.activeCustomFields.remove(*display.custom)
		}
	case *fieldValueMulti:
		index := d.optionCursors[display.field]
		if !value.Selected.remove(index) {
			value.Selected.insert(index)
		}
		if display.custom != nil && !display.alongside {
			d.activeCustomFields.remove(*display.custom)
		}
	case *fieldValueBoolean:
		value.Value = !value.Value
	case *fieldValueText:
		panic("text fields are handled above")
	}
}

func (d *elicitationDialog) editText(key tea.KeyMsg) {
	field, custom, ok := d.editableField()
	if !ok {
		return
	}
	activated := custom && d.activeCustomFields.insert(field)
	if field >= len(d.values) {
		panic("editable fields contain text values")
	}
	value, isText := d.values[field].(*fieldValueText)
	if !isText {
		panic("editable fields contain text values")
	}
	before := value.Input.Value()
	value.Input.HandleKey(tea.KeyPressMsg(key.Key()))
	if activated || value.Input.Value() != before {
		d.confirmed.remove(d.currentQuestion)
	}
}

// questionContent is the answer the question at displayIndex gives, keyed by
// field id, or why it cannot be sent.
func (d *elicitationDialog) questionContent(displayIndex int) (map[string]core.ElicitationValue, error) {
	display := d.displayFields[displayIndex]
	var activeCustom *int
	if display.custom != nil && d.activeCustomFields.contains(*display.custom) {
		activeCustom = display.custom
	}
	picked := true
	switch value := d.values[display.field].(type) {
	case *fieldValueSingle:
		picked = value.Selected != nil
	case *fieldValueMulti:
		picked = len(value.Selected) > 0
	}
	var indices []int
	switch {
	case display.alongside:
		// Text beside a pick is sent with whatever is picked; with nothing
		// picked it is the whole answer.
		if display.custom != nil {
			indices = append(indices, *display.custom)
		}
		if picked {
			indices = append(indices, display.field)
		}
	case activeCustom != nil && display.customOption != nil:
		indices = []int{display.field, *activeCustom}
	case activeCustom != nil:
		indices = []int{*activeCustom}
	default:
		indices = []int{display.field}
	}
	content := map[string]core.ElicitationValue{}
	for _, index := range indices {
		field := &d.req.Fields[index]
		value, err := validatedValue(field, d.values[index])
		if err != nil {
			return nil, err
		}
		if value != nil {
			content[field.ID] = value
		}
	}
	if owner := &d.req.Fields[display.field]; owner.Required && len(content) == 0 {
		return nil, fmt.Errorf("%s is required", owner.Title)
	}
	return content, nil
}

func (d *elicitationDialog) answerAndNext() *core.ElicitationResponse {
	if len(d.displayFields) > 0 {
		content, err := d.questionContent(d.currentQuestion)
		if err != nil {
			message := err.Error()
			d.error = &message
			d.focusControl(d.currentQuestion)
			return nil
		}
		if len(content) == 0 {
			d.confirmed.remove(d.currentQuestion)
		} else {
			d.confirmed.insert(d.currentQuestion)
		}
	}
	if d.currentQuestion+1 < len(d.displayFields) {
		d.navigate(true)
		return nil
	}
	unanswered := d.unanswered()
	for _, index := range unanswered {
		if field := &d.req.Fields[d.displayFields[index].field]; field.Required {
			d.focusControl(index)
			message := fmt.Sprintf("%s is required", field.Title)
			d.error = &message
			return nil
		}
	}
	if len(unanswered) > 0 {
		d.confirmingUnanswered = true
		d.resetPage()
		return nil
	}
	return d.accept()
}

func (d *elicitationDialog) accept() *core.ElicitationResponse {
	content := map[string]core.ElicitationValue{}
	for _, index := range d.confirmed {
		answer, err := d.questionContent(index)
		if err != nil {
			message := err.Error()
			d.error = &message
			return nil
		}
		maps.Copy(content, answer)
	}
	response := core.ElicitationResponse{Action: core.ElicitationResponseAccept, Content: content}
	if err := d.req.ValidateResponse(&response); err != nil {
		message := err.Error()
		d.error = &message
		return nil
	}
	return &response
}

// editableField is the field whose text the keyboard edits, and whether it
// is a custom answer beside a choice.
func (d *elicitationDialog) editableField() (field int, custom bool, ok bool) {
	focused, hasFocus := d.view.form.Focused()
	if !hasFocus || focused.kind != elicitationControlKindField || focused.field >= len(d.displayFields) {
		return 0, false, false
	}
	display := d.displayFields[focused.field]
	if _, text := d.values[display.field].(*fieldValueText); text {
		return display.field, false, true
	}
	if display.custom == nil {
		return 0, false, false
	}
	optionCount, isSelect := selectOptionCount(&d.req.Fields[display.field])
	if !isSelect {
		return 0, false, false
	}
	customCursor := optionCount
	if display.customOption != nil {
		customCursor = *display.customOption
	}
	if d.optionCursors[display.field] != customCursor {
		return 0, false, false
	}
	return *display.custom, true, true
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
	first, last := covered[0], covered[1]
	// Clamping last rather than the height keeps the row the range ends on
	// and the rows rendered for it the same rows, so a capped line takes the
	// full-width branch below instead of cutting a row it never drew.
	last = min(last, first+maximumOffscreenRows-1)
	height := max(last-first, 0) + 1
	if height > math.MaxUint16 {
		return ""
	}
	skip := min(max(first-base, 0), math.MaxUint16)
	area := uv.Rect(0, 0, width, height)
	buffer := uv.NewBuffer(width, height)
	NewParagraph(RawText(line)).WithWrap(Wrap{Trim: true}).WithScroll(skip, 0).Render(area, buffer)
	startColumn := 0
	if first == selection.Start.Row {
		startColumn = selection.Start.Col
	}
	endColumn := width - 1
	if last == selection.End.Row {
		endColumn = selection.End.Col
	}
	cut := SelectionRange{Start: NewContentPos(0, startColumn), End: NewContentPos(height-1, endColumn)}
	frame := SurfaceFrameFixed(SurfaceIdElicitationMessage{}, area)
	var rows []string
	for _, row := range strings.Split(ExtractRows(buffer, &frame, &cut), "\n") {
		if row != "" {
			rows = append(rows, row)
		}
	}
	return strings.Join(rows, " ")
}

func displayFields(request *core.ElicitationRequest, values []fieldValue) ([]displayField, indexSet) {
	type companion struct {
		custom int
		role   core.CompanionRole
	}
	customByOwner := map[int]companion{}
	attachedCustom := indexSet{}
	for owner := range request.Fields {
		if custom, role, ok := request.Companion(&request.Fields[owner]); ok {
			customByOwner[owner] = companion{custom: custom, role: role}
			attachedCustom.insert(custom)
		}
	}
	var fields []displayField
	for field := range request.Fields {
		if attachedCustom.contains(field) {
			continue
		}
		display := displayField{field: field}
		if companion, ok := customByOwner[field]; ok {
			custom := companion.custom
			display.custom = &custom
			display.alongside = companion.role == core.CompanionRoleAlongside
			if companion.role == core.CompanionRoleDetail {
				if value := request.Fields[custom].CustomAnswerOption; value != nil {
					if option, ok := selectOptionIndex(&request.Fields[field], *value); ok {
						display.customOption = &option
					}
				}
			}
		}
		fields = append(fields, display)
	}
	activeCustomFields := indexSet{}
	for _, index := range attachedCustom {
		if value, ok := values[index].(*fieldValueText); ok && !value.Input.IsEmpty() {
			activeCustomFields.insert(index)
		}
	}
	return fields, activeCustomFields
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
	if optionCount, ok := selectOptionCount(&request.Fields[display.field]); ok {
		length := optionCount
		if display.custom != nil && display.customOption == nil {
			length++
		}
		return ControlKindChoiceList{Len: length, Selected: cursors[display.field]}
	}
	if _, boolean := values[display.field].(*fieldValueBoolean); boolean {
		return ControlKindCheckbox{}
	}
	return ControlKindTextField{}
}

func defaultValue(field *core.ElicitationField) fieldValue {
	switch kind := field.Kind.(type) {
	case core.ElicitationFieldKindText:
		text := ""
		if kind.Default != nil {
			text = *kind.Default
		}
		return &fieldValueText{Input: TextInputFromValue(text)}
	case core.ElicitationFieldKindSingleSelect:
		value := &fieldValueSingle{}
		if kind.Default != nil {
			for index, option := range kind.Options {
				if option.Value == *kind.Default {
					value.Selected = &index
					break
				}
			}
		} else if len(kind.Options) > 0 {
			first := 0
			value.Selected = &first
		}
		return value
	case core.ElicitationFieldKindMultiSelect:
		selected := indexSet{}
		for index, option := range kind.Options {
			if slices.Contains(kind.Default, option.Value) {
				selected.insert(index)
			}
		}
		return &fieldValueMulti{Selected: selected}
	case core.ElicitationFieldKindBoolean:
		return &fieldValueBoolean{Value: kind.Default != nil && *kind.Default}
	case core.ElicitationFieldKindInteger:
		text := ""
		if kind.Default != nil {
			text = strconv.FormatInt(*kind.Default, 10)
		}
		return &fieldValueText{Input: TextInputFromValue(text)}
	case core.ElicitationFieldKindNumber:
		text := ""
		if kind.Default != nil {
			text = rustFloatDisplay(*kind.Default)
		}
		return &fieldValueText{Input: TextInputFromValue(text)}
	}
	return &fieldValueText{Input: TextInputFromValue("")}
}

// rustFloatDisplay is Rust's Display of an f64: the shortest text that reads
// back as the same value, never in exponent form (`1e21` prints in full,
// `0.1` as `0.1`), with `inf`, `-inf` and `NaN` for the special values.
func rustFloatDisplay(value float64) string {
	switch {
	case math.IsNaN(value):
		return "NaN"
	case math.IsInf(value, 1):
		return "inf"
	case math.IsInf(value, -1):
		return "-inf"
	}
	return strconv.FormatFloat(value, 'f', -1, 64)
}

// validatedValue is the value to send for field, nil when an optional field
// is left empty, or why it cannot be sent.
func validatedValue(field *core.ElicitationField, value fieldValue) (core.ElicitationValue, error) {
	missing := func() (core.ElicitationValue, error) {
		return nil, fmt.Errorf("%s is required", field.Title)
	}
	empty := func() (core.ElicitationValue, error) {
		if field.Required {
			return missing()
		}
		return nil, nil
	}
	var answered core.ElicitationValue
	switch kind := field.Kind.(type) {
	case core.ElicitationFieldKindText:
		text, ok := value.(*fieldValueText)
		if !ok {
			break
		}
		if strings.TrimSpace(text.Input.Value()) == "" {
			return empty()
		}
		answered = core.ElicitationValueString(text.Input.String())
	case core.ElicitationFieldKindSingleSelect:
		single, ok := value.(*fieldValueSingle)
		if !ok {
			break
		}
		if single.Selected == nil {
			return empty()
		}
		answered = core.ElicitationValueString(kind.Options[*single.Selected].Value)
	case core.ElicitationFieldKindMultiSelect:
		multi, ok := value.(*fieldValueMulti)
		if !ok {
			break
		}
		if len(multi.Selected) == 0 && field.Required {
			return missing()
		}
		values := make(core.ElicitationValueStringArray, 0, len(multi.Selected))
		for _, index := range multi.Selected {
			values = append(values, kind.Options[index].Value)
		}
		// An empty optional multi-select still has to satisfy `minItems`,
		// so the constraints are checked before the answer is dropped.
		if err := core.ValidateFieldValue(field, values); err != nil {
			return nil, err
		}
		if len(multi.Selected) == 0 {
			return nil, nil
		}
		answered = values
	case core.ElicitationFieldKindBoolean:
		boolean, ok := value.(*fieldValueBoolean)
		if !ok {
			break
		}
		answered = core.ElicitationValueBoolean(boolean.Value)
	case core.ElicitationFieldKindInteger:
		text, ok := value.(*fieldValueText)
		if !ok {
			break
		}
		if strings.TrimSpace(text.Input.Value()) == "" {
			return empty()
		}
		parsed, err := parseRustI64(text.Input.String())
		if err != nil {
			return nil, fmt.Errorf("%s must be an integer", field.Title)
		}
		answered = core.ElicitationValueInteger(parsed)
	case core.ElicitationFieldKindNumber:
		text, ok := value.(*fieldValueText)
		if !ok {
			break
		}
		if strings.TrimSpace(text.Input.Value()) == "" {
			return empty()
		}
		parsed, err := parseRustF64(text.Input.String())
		if err != nil {
			return nil, fmt.Errorf("%s must be a number", field.Title)
		}
		answered = core.ElicitationValueNumber(parsed)
	}
	if answered == nil {
		return nil, fmt.Errorf("%s has an incompatible value", field.Title)
	}
	if err := core.ValidateFieldValue(field, answered); err != nil {
		return nil, err
	}
	return answered, nil
}

// parseRustI64 is Rust's `str::parse::<i64>`, which base-10 ParseInt
// matches: an optional sign and decimal digits, no whitespace or
// underscores.
func parseRustI64(text string) (int64, error) {
	return strconv.ParseInt(text, 10, 64)
}

// parseRustF64 is Rust's `str::parse::<f64>`: decimal and exponent forms,
// and `inf`, `infinity` and `nan` in any case, each with an optional sign.
// Go's ParseFloat also reads hexadecimal floats, which Rust refuses, and
// fails on an out-of-range literal, which Rust reads as infinity.
func parseRustF64(text string) (float64, error) {
	unsigned := text
	if unsigned != "" && (unsigned[0] == '+' || unsigned[0] == '-') {
		unsigned = unsigned[1:]
	}
	switch strings.ToLower(unsigned) {
	case "inf", "infinity", "nan":
		return strconv.ParseFloat(text, 64)
	}
	for _, character := range unsigned {
		if !(character >= '0' && character <= '9' || strings.ContainsRune(".eE+-", character)) {
			return 0, fmt.Errorf("invalid float literal")
		}
	}
	value, err := strconv.ParseFloat(text, 64)
	if numError, ok := err.(*strconv.NumError); ok && numError.Err == strconv.ErrRange {
		return value, nil
	}
	return value, err
}

// renderElicitationIn draws an elicitation in the bounds owned by the chat
// surface. This is used by the combined dashboard so the question cannot
// cover its navigator or the other support panes.
func renderElicitationIn(frame *Frame, dialog *elicitationDialog, surfaces *FrameSurfaces, bounds uv.Rectangle, focused bool) {
	renderElicitationAt(frame, dialog, surfaces, bounds, focused)
	dialog.finishSurface()
}

func renderElicitationAt(frame *Frame, dialog *elicitationDialog, surfaces *FrameSurfaces, area uv.Rectangle, focused bool) {
	// The question replaces the session content rectangle. Clear exactly
	// that rectangle so hidden transcript text cannot show through it, while
	// the navigator and every neighboring pane remain untouched.
	frame.RenderWidget(Clear{}, area)
	rendered := area
	dialog.view.renderedArea = &rendered
	var title string
	switch {
	case dialog.confirmingUnanswered:
		title = "Unanswered questions"
	case len(dialog.displayFields) == 0:
		title = "Agent question"
		if dialog.req.Title != nil {
			title = *dialog.req.Title
		}
	default:
		title = fmt.Sprintf("Question %d/%d · %d unanswered",
			dialog.currentQuestion+1, len(dialog.displayFields), len(dialog.unanswered()))
	}
	focus := elicitationFocusContent(dialog)
	focusedField := dialog.currentQuestion
	form := dialog.view.form
	form.BeginFrame()
	titleLine := DismissibleModalTitle(form, area, title, Title(focused), true)
	block := Panel(focused).WithTitle(titleLine)
	// Compact panes reserve their content rows for the labeled control and
	// actions; keep validation failures visible in the bottom border.
	if area.Dy() < 6 && dialog.error != nil {
		block = block.WithTitleBottom(StyledLine(*dialog.error, Style{}.Foreground(ThemePalette().Error)))
	}
	inner := block.Inner(area)
	frame.RenderWidget(block, area)
	renderElicitationBody(frame, dialog, surfaces, inner, focused, focus, focusedField)
}

// rectUnion is ratatui's Rect::union: the smallest rectangle holding both,
// empty ones included.
func rectUnion(a, b uv.Rectangle) uv.Rectangle {
	return uv.Rectangle{
		Min: uv.Pos(min(a.Min.X, b.Min.X), min(a.Min.Y, b.Min.Y)),
		Max: uv.Pos(max(a.Max.X, b.Max.X), max(a.Max.Y, b.Max.Y)),
	}
}

// focusCursorRow is how many rows into the focused line the text cursor
// sits, at width.
func focusCursorRow(focus *focusContent, line, width int) int {
	if focus.textCursor == nil || focus.textCursor.line != line {
		return 0
	}
	text := focus.lines[line].String()
	_, row := InputCursorVisualPosition(text, focusCursorByte(text, focus.textCursor.column), width)
	return row
}

// focusCursorByte is the byte offset of the text cursor in a focus line,
// which starts with the two-cell `> ` prompt.
func focusCursorByte(text string, column int) int {
	graphemes := splitGraphemes(text)
	if index := 2 + column; index < len(graphemes) {
		return graphemes[index].offset
	}
	return len(text)
}

// paragraphLineCount is the rows lines take wrapped at width, without
// trimming.
func paragraphLineCount(lines []Line, width int) int {
	return NewParagraph(TextFrom(lines...)).WithWrap(Wrap{Trim: false}).LineCount(width)
}

func renderElicitationBody(frame *Frame, dialog *elicitationDialog, surfaces *FrameSurfaces, inner uv.Rectangle, focused bool, focus focusContent, focusedField int) {
	naturalFocusHeight := focus.wrappedHeight(inner.Dx())
	prompt := dialog.promptText()
	naturalMessageHeight := max(min(NewParagraph(RawText(prompt)).WithWrap(Wrap{Trim: true}).LineCount(inner.Dx()), math.MaxUint16), 1)
	// Keep a field's title, control, and action row before spending a row
	// on keyboard hints. A five-row bordered pane has only three content
	// rows.
	footerHeight := 0
	if inner.Dy() >= 4 {
		footerHeight = 1
	}
	// In a compact question pane one button row is enough: the second row is
	// more valuable to the message and focused control, which can each
	// scroll their content independently.
	buttonRows := 2
	if inner.Dy() <= 4 {
		buttonRows = 1
	}
	// With only two content rows, preserve the question and answer control.
	// Enter and Esc still use the ordinary dialog handlers without a button
	// bar.
	buttonsHeight := min(max(inner.Dy()-footerHeight, 0), buttonRows)
	if inner.Dy() < 3 && len(dialog.displayFields) > 0 && !dialog.confirmingUnanswered {
		buttonsHeight = 0
	}
	bodyHeight := max(inner.Dy()-(footerHeight+buttonsHeight), 0)
	var messageHeight, focusHeight int
	switch {
	case dialog.confirmingUnanswered || len(dialog.displayFields) == 0:
		messageHeight, focusHeight = bodyHeight, 0
	case dialog.isPlanReview():
		focusHeight = min(naturalFocusHeight, max(bodyHeight-1, 1), bodyHeight)
		messageHeight = max(bodyHeight-focusHeight, 0)
	default:
		minimumAnswer := min(naturalFocusHeight, 2)
		messageHeight = min(naturalMessageHeight, max(max(bodyHeight-minimumAnswer, 0), bodyHeight/2), max(bodyHeight-1, 0))
		focusHeight = max(bodyHeight-messageHeight, 0)
	}
	var chunks []uv.Rectangle
	if inner.Dy() == 1 {
		// A single row still needs both the request and its answer. Put the
		// independently scrolling message beside the control/action row.
		columns := HorizontalLayout(ConstraintFill(1), ConstraintFill(1)).Split(inner)
		empty := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), 0)
		if dialog.confirmingUnanswered || len(dialog.displayFields) == 0 {
			chunks = []uv.Rectangle{columns[0], empty, columns[1], empty}
		} else {
			chunks = []uv.Rectangle{columns[0], columns[1], empty, empty}
		}
	} else {
		chunks = VerticalLayout(
			ConstraintLength(messageHeight),
			ConstraintLength(focusHeight),
			ConstraintLength(buttonsHeight),
			ConstraintLength(footerHeight),
		).Split(inner)
	}
	message := NewParagraph(RawText(prompt)).WithWrap(Wrap{Trim: true})
	totalLines := min(message.LineCount(chunks[0].Dx()), math.MaxUint16)
	maximumScroll := max(totalLines-chunks[0].Dy(), 0)
	messageScroll := dialog.view.messageScroll
	if dialog.view.messageAnchorWidth != chunks[0].Dx() && dialog.view.messageAnchor != nil {
		line, row := dialog.view.messageAnchor[0], dialog.view.messageAnchor[1]
		source := ""
		if lines := strings.Split(prompt, "\n"); line < len(lines) {
			source = lines[line]
		}
		messageScroll = min(wrappedPrefixRows(prompt, chunks[0].Dx(), line)+
			min(wrappedRowForGraphemeOffset(source, chunks[0].Dx(), row), math.MaxUint16), math.MaxUint16)
	}
	messageScroll = min(messageScroll, maximumScroll)
	dialog.view.messageScroll = messageScroll
	if dialog.view.messageAnchor == nil {
		line, offset := messagePositionAtRow(prompt, chunks[0].Dx(), messageScroll)
		dialog.view.messageAnchor = &[2]int{line, offset}
	}
	dialog.view.messageAnchorWidth = chunks[0].Dx()
	dialog.view.messagePageHeight = chunks[0].Dy()
	dialog.view.messageMaxScroll = maximumScroll
	messageArea := chunks[0]
	dialog.view.messageArea = &messageArea
	frame.RenderWidget(message.WithScroll(messageScroll, 0), chunks[0])
	// The message pane scrolls its own rows, so it registers in content
	// space: a drag can reach the plan text above and below the viewport.
	surfaces.Push(SurfaceFrameScrollable(SurfaceIdElicitationMessage{}, chunks[0], messageScroll, totalLines))
	surfaces.Push(SurfaceFrameFixed(SurfaceIdModalBody{}, chunks[1]))
	// The field title stays outside the scrolled field body, so scrolling to
	// a control or a distant option can never hide the question it answers.
	// A single remaining row belongs to the control itself.
	focusArea := chunks[1]
	if focus.title != nil && chunks[1].Dy() >= 2 {
		frame.RenderWidget(NewParagraph(TextFrom(*focus.title)), uv.Rect(chunks[1].Min.X, chunks[1].Min.Y, chunks[1].Dx(), 1))
		focusArea = uv.Rect(chunks[1].Min.X, chunks[1].Min.Y+1, chunks[1].Dx(), max(chunks[1].Dy()-1, 0))
	}
	fieldIndex := min(focusedField, max(len(dialog.displayFields)-1, 0))
	form := dialog.view.form
	if !dialog.confirmingUnanswered && focusedField < len(dialog.displayFields) {
		display := dialog.displayFields[focusedField]
		form.Register(elicitationControlField(focusedField),
			displayControlKind(&dialog.req, dialog.values, dialog.optionCursors, display), focusArea, true)
	}

	focusRows := max(min(paragraphLineCount(focus.lines, focusArea.Dx()), math.MaxUint16), 1)
	focusMaxScroll := max(focusRows-focusArea.Dy(), 0)
	targetRow := 0
	if focus.focusedRow != nil {
		line := *focus.focusedRow
		targetRow = paragraphLineCount(focus.lines[:line], focusArea.Dx()) + focusCursorRow(&focus, line, focusArea.Dx())
	}
	focusScroll := min(dialog.view.focusScroll, focusMaxScroll)
	targetRow = min(targetRow, math.MaxUint16)
	if targetRow < focusScroll {
		focusScroll = targetRow
	} else if targetRow >= focusScroll+focusArea.Dy() {
		focusScroll = min(max(targetRow-max(focusArea.Dy()-1, 0), 0), focusMaxScroll)
	}
	dialog.view.focusScroll = focusScroll
	if !dialog.confirmingUnanswered && focusedField < len(dialog.displayFields) {
		display := dialog.displayFields[focusedField]
		id := elicitationControlField(fieldIndex)
		_, isSelect := selectOptionCount(&dialog.req.Fields[display.field])
		if isSelect {
			ChoiceListRenderWrapped(frame, focusArea, focus.lines, focus.optionRows,
				dialog.optionCursors[display.field], focusScroll, form, id)
		} else {
			renderFocus(frame, focusArea, &focus, focusScroll, focused && form.IsFocused(id))
		}
		if focus.textCursor != nil {
			prefix := paragraphLineCount(focus.lines[:focus.textCursor.line], focusArea.Dx())
			// The editor is a single row; scrolling belongs to the field,
			// while the surrounding option descriptions keep their wrapping.
			row := max(prefix-focusScroll, 0)
			if row < focusArea.Dy() {
				field := display.field
				if _, text := dialog.values[display.field].(*fieldValueText); !text {
					if display.custom == nil {
						panic("inline text belongs to a custom answer")
					}
					field = *display.custom
				}
				if input, ok := dialog.values[field].(*fieldValueText); ok {
					area := uv.Rect(focusArea.Min.X+2, focusArea.Min.Y+row, max(focusArea.Dx()-2, 0), 1)
					if !isSelect {
						form.Register(id, ControlKindTextField{}, area, true)
					}
					TextFieldRenderInline(frame, area, &input.Input, dialog.req.Fields[field].Secret, focused, form, id)
				}
			}
		} else if _, boolean := dialog.values[display.field].(*fieldValueBoolean); boolean {
			// The title and description label this boolean too: clicking any
			// of them toggles the same control as clicking its mark.
			area := rectUnion(chunks[0], chunks[1])
			if dialog.isPlanReview() {
				area = chunks[1]
			}
			form.Register(id, ControlKindCheckbox{}, area, true)
		}
	} else {
		renderFocus(frame, focusArea, &focus, focusScroll, focused)
	}
	// A compact form without an action bar must not leave invisible buttons
	// in the Tab order; Enter and Esc answer or cancel directly.
	if buttonsHeight > 0 {
		actions := dialog.actions()
		buttons := make([]ButtonSpec[elicitationControl], len(actions))
		for i, action := range actions {
			buttons[i] = ButtonSpec[elicitationControl]{ID: action.control, Label: action.label, Enabled: action.enabled}
		}
		ButtonRowRender(frame, chunks[2], buttons, form)
	}
	if dialog.confirmingUnanswered {
		form.EndFrame(elicitationControlPrevious)
	} else {
		form.EndFrame(elicitationControlField(dialog.currentQuestion))
	}
	// Where a pick is deliberate, the arrows only move and Space picks.
	picksOnPurpose := !dialog.confirmingUnanswered && dialog.currentQuestion < len(dialog.displayFields) &&
		dialog.picksOnPurpose(dialog.displayFields[dialog.currentQuestion])
	spaceHelp := "Space toggle"
	if picksOnPurpose {
		spaceHelp = "Space pick"
	}
	var footer string
	switch {
	case dialog.error != nil:
		footer = *dialog.error
	case maximumScroll > 0:
		start := messageScroll + 1
		end := min(messageScroll+chunks[0].Dy(), totalLines)
		footer = fmt.Sprintf("Text %d–%d/%d · PgUp/PgDn or wheel scroll · %s · Enter answer · Alt+←/→ questions",
			start, end, totalLines, spaceHelp)
	case dialog.confirmingUnanswered:
		footer = "Enter confirm · Esc go back"
	case picksOnPurpose:
		footer = "Tab controls · ↑/↓ move · Space pick · Enter answer · Alt+←/→ questions"
	default:
		footer = "Tab controls · ↑/↓ choose · Space toggle · Enter answer · Alt+←/→ questions"
	}
	shown := "Click to answer"
	if focused || dialog.error != nil {
		shown = footer
	}
	footerColor := ThemePalette().Muted
	if dialog.error != nil && focused {
		footerColor = ThemePalette().Error
	}
	frame.RenderWidget(NewParagraph(RawText(shown)).WithStyle(Style{}.Foreground(footerColor)), chunks[3])
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
	rows := 0
	lines := strings.Split(message, "\n")
	for index, line := range lines {
		lineRows := max(wrappedRowCount(line, width), 1)
		if row < rows+lineRows {
			return index, graphemeOffsetForWrappedRow(line, width, max(row-rows, 0))
		}
		rows += lineRows
	}
	return max(len(lines)-1, 0), 0
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
	focus := dialog.currentQuestion
	if dialog.confirmingUnanswered || focus >= len(dialog.displayFields) {
		label := "Submit these answers"
		if dialog.confirmingUnanswered {
			label = "Answer & Next submits; Previous returns to unanswered questions"
		}
		return focusContent{lines: []Line{RawLine(label)}, centered: true}
	}
	display := dialog.displayFields[focus]
	field := &dialog.req.Fields[display.field]
	required := ""
	if field.Required {
		required = " (required)"
	}
	title := LineFrom(
		StyledSpan(fmt.Sprintf("%d/%d  ", focus+1, len(dialog.displayFields)), Style{}.Foreground(ThemePalette().Muted)),
		StyledSpan(field.Title+required, Style{}.Add(ModifierBold)),
	)
	var lines []Line
	if dialog.isPlanReview() && field.Description != nil {
		lines = append(lines, StyledLine(*field.Description, Style{}.Foreground(ThemePalette().Text)))
	}
	var optionRows []*int
	var cursor *textCursor
	var focusedRow *int
	// padRows extends optionRows to the lines so far (Vec::resize with None).
	padRows := func() {
		for len(optionRows) < len(lines) {
			optionRows = append(optionRows, nil)
		}
	}
	optionStyle := func(cursor bool) Style {
		if cursor {
			return Style{}.Foreground(ThemePalette().Accent).Add(ModifierBold)
		}
		return Style{}
	}
	switch value := dialog.values[display.field].(type) {
	case *fieldValueText:
		shown := value.Input.String()
		if field.Secret {
			shown = strings.Repeat("•", utf8.RuneCountInString(value.Input.Value()))
		}
		lines = append(lines, RawLine(""))
		inputLine := len(lines)
		focusedRow = &inputLine
		lines = append(lines, StyledLine("> "+shown, Style{}.Foreground(ThemePalette().Accent)))
		cursor = &textCursor{line: inputLine, column: len(splitGraphemes(value.Input.Value()[:value.Input.Cursor()]))}
	case *fieldValueSingle:
		kind, ok := field.Kind.(core.ElicitationFieldKindSingleSelect)
		if !ok {
			break
		}
		// Only a free-text alternative takes the pick's place.
		customActive := !display.alongside && display.custom != nil && dialog.activeCustomFields.contains(*display.custom)
		for index, option := range kind.Options {
			atCursor := dialog.optionCursors[display.field] == index
			customReplacesSelection := display.customOption == nil
			marker := "○"
			if (!customActive || !customReplacesSelection) && value.Selected != nil && *value.Selected == index {
				marker = "●"
			}
			if atCursor {
				row := len(lines)
				focusedRow = &row
			}
			padRows()
			optionIndex := index
			optionRows = append(optionRows, &optionIndex)
			lines = append(lines, StyledLine(marker+" "+option.Title, optionStyle(atCursor)))
			if atCursor {
				if option.Description != nil {
					lines = append(lines, StyledLine("    "+*option.Description, Style{}.Foreground(ThemePalette().Text)))
				}
				if option.Preview != nil {
					lines = append(lines, StyledLine("    "+*option.Preview, Style{}.Foreground(ThemePalette().Muted)))
				}
				if display.customOption != nil && *display.customOption == index && display.custom != nil {
					lines, cursor = renderCustomText(lines, cursor, dialog, *display.custom)
				}
			}
		}
		if display.custom != nil && display.customOption == nil {
			padRows()
			customRow := len(kind.Options)
			optionRows = append(optionRows, &customRow)
		}
		lines, cursor = renderCustomAnswer(lines, cursor, dialog, display, len(kind.Options), "○", "●")
	case *fieldValueMulti:
		kind, ok := field.Kind.(core.ElicitationFieldKindMultiSelect)
		if !ok {
			break
		}
		customActive := !display.alongside && display.custom != nil && dialog.activeCustomFields.contains(*display.custom)
		for index, option := range kind.Options {
			atCursor := dialog.optionCursors[display.field] == index
			if atCursor {
				row := len(lines)
				focusedRow = &row
			}
			marker := CheckboxMarker(!customActive && value.Selected.contains(index))
			padRows()
			optionIndex := index
			optionRows = append(optionRows, &optionIndex)
			lines = append(lines, StyledLine(marker+" "+option.Title, optionStyle(atCursor)))
		}
		if display.custom != nil && display.customOption == nil {
			padRows()
			customRow := len(kind.Options)
			optionRows = append(optionRows, &customRow)
		}
		lines, cursor = renderCustomAnswer(lines, cursor, dialog, display, len(kind.Options), CheckboxMarker(false), CheckboxMarker(true))
	case *fieldValueBoolean:
		if _, ok := field.Kind.(core.ElicitationFieldKindBoolean); !ok {
			break
		}
		row := len(lines)
		focusedRow = &row
		answer := "No"
		if value.Value {
			answer = "Yes"
		}
		lines = append(lines, StyledLine(CheckboxMarker(value.Value)+" "+answer, Style{}.Foreground(ThemePalette().Accent)))
	}
	if focusedRow == nil && cursor != nil {
		row := cursor.line
		focusedRow = &row
	}
	content := focusContent{lines: lines, textCursor: cursor, focusedRow: focusedRow, optionRows: optionRows}
	if dialog.isPlanReview() {
		content.title = &title
	}
	return content
}

func renderFocus(frame *Frame, area uv.Rectangle, content *focusContent, scroll int, focused bool) {
	alignment := AlignmentLeft
	if content.centered {
		alignment = AlignmentCenter
	}
	paragraph := NewParagraph(TextFrom(content.lines...)).WithWrap(Wrap{Trim: false}).WithScroll(scroll, 0).WithAlignment(alignment)
	frame.RenderWidget(paragraph, area)
	if !focused || content.textCursor == nil || area.Dx() <= 2 || area.Dy() <= 0 {
		return
	}
	line := content.textCursor.line
	prefixRows := paragraphLineCount(content.lines[:line], area.Dx())
	text := content.lines[line].String()
	column, cursorRow := InputCursorVisualPosition(text, focusCursorByte(text, content.textCursor.column), area.Dx())
	row := max(prefixRows+cursorRow-scroll, 0)
	if row < area.Dy() {
		frame.SetCursorPosition(uv.Pos(area.Min.X+min(column, area.Dx()-1), area.Min.Y+row))
	}
}

func renderCustomAnswer(lines []Line, cursor *textCursor, dialog *elicitationDialog, display displayField, optionCount int, unselectedMarker, selectedMarker string) ([]Line, *textCursor) {
	if display.custom == nil || display.customOption != nil {
		return lines, cursor
	}
	customIndex := *display.custom
	custom := &dialog.req.Fields[customIndex]
	focused := dialog.optionCursors[display.field] == optionCount
	active := dialog.activeCustomFields.contains(customIndex)
	// Text beside the pick is a field of its own, not one of the choices.
	marker := unselectedMarker
	switch {
	case display.alongside:
		marker = "✎"
	case active:
		marker = selectedMarker
	}
	style := Style{}
	if focused {
		style = Style{}.Foreground(ThemePalette().Accent).Add(ModifierBold)
	}
	lines = append(lines, StyledLine(marker+" "+custom.Title, style))
	if focused {
		return renderCustomText(lines, cursor, dialog, customIndex)
	}
	// Text beside the pick is sent with it, so it stays in view while the
	// cursor is on the choices.
	if value, ok := dialog.values[customIndex].(*fieldValueText); ok && display.alongside &&
		strings.TrimSpace(value.Input.Value()) != "" {
		shown := value.Input.String()
		if custom.Secret {
			shown = strings.Repeat("•", utf8.RuneCountInString(value.Input.Value()))
		}
		lines = append(lines, StyledLine("    "+shown, Style{}.Foreground(ThemePalette().Muted)))
	}
	return lines, cursor
}

func renderCustomText(lines []Line, cursor *textCursor, dialog *elicitationDialog, customIndex int) ([]Line, *textCursor) {
	custom := &dialog.req.Fields[customIndex]
	value, ok := dialog.values[customIndex].(*fieldValueText)
	if !ok {
		panic("custom answer fields contain text values")
	}
	if custom.Description != nil {
		lines = append(lines, StyledLine("    "+*custom.Description, Style{}.Foreground(ThemePalette().Text)))
	}
	shown := value.Input.String()
	if custom.Secret {
		shown = strings.Repeat("•", utf8.RuneCountInString(value.Input.Value()))
	}
	inputLine := len(lines)
	lines = append(lines, StyledLine("> "+shown, Style{}.Foreground(ThemePalette().Accent)))
	return lines, &textCursor{line: inputLine, column: len(splitGraphemes(value.Input.Value()[:value.Input.Cursor()]))}
}
