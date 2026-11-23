// Port of mj-chat/src/selection.rs.

// Content-space text selection engine shared by Hel's terminal surfaces.
//
// The engine owns one global selection over a registry of selectable surfaces
// that the render pass rebuilds every frame. Selections are stored in
// *content* coordinates (a scroll-independent row plus a column inside the
// surface), so a surface that scrolls or streams new rows under an active
// drag keeps the same selection.
//
// The engine deliberately knows nothing about Hel's views. It maps screen
// cells to content positions, highlights the visible part of a selection in
// a buffer, and reports the selected range on release. Turning a range into
// text is the caller's job for scrollable surfaces, which own the wrapped-row
// cache; ExtractRows covers surfaces whose content is fully on screen.

package chat

import (
	uv "github.com/charmbracelet/ultraviolet"
)

// SelectableText is plain text using the transcript's wrapping and
// smart-copy rules. The row cache is shared by rendering and extraction,
// including off-screen rows.
type SelectableText struct {
	source string
	width  int
	rows   []Line
	copies []rowCopy
}

// NewSelectableText builds the same rows and soft-wrap boundaries as
// transcript text.
func NewSelectableText(source string, width int) SelectableText {
	panic("unported: mj-chat/src/selection.rs NewSelectableText")
}

// Matches reports whether a cached row space still describes this text and
// width.
func (t *SelectableText) Matches(source string, width int) bool {
	return t.source == source && t.width == width
}

// Rows is the wrapped rows for rendering and scroll extent.
func (t *SelectableText) Rows() []Line {
	return t.rows
}

// SelectionText extracts with the transcript's word/line boundary and
// Unicode rules.
func (t *SelectableText) SelectionText(r *SelectionRange) string {
	panic("unported: mj-chat/src/selection.rs SelectionText")
}

// SurfaceId identifies a surface that can own a selection. Variants are
// comparable, so a SurfaceId works with == and as a map key; nil means no
// surface.
type SurfaceId interface {
	isSurfaceId()
}

// SurfaceIdTranscript is the chat transcript rows.
type SurfaceIdTranscript struct{}

// SurfaceIdNotificationText is a notification body; a new card or row layout
// gets a new identity.
type SurfaceIdNotificationText uint64

// SurfaceIdNotificationFormBody is the rendered answer body of the
// notification's chat-owned form.
type SurfaceIdNotificationFormBody uint64

// SurfaceIdElicitationMessage is the scrollable message body of an
// elicitation dialog.
type SurfaceIdElicitationMessage struct{}

// SurfaceIdPromptInput is the prompt editor.
type SurfaceIdPromptInput struct{}

// SurfaceIdDashboardPane is a dashboard pane, numbered in render order.
type SurfaceIdDashboardPane uint8

// SurfaceIdFailurePane is the error text of a failed session, keyed by the
// raw id of the tiled pane showing it: several panes can each show a failure
// at once.
type SurfaceIdFailurePane uint32

// SurfaceIdResumeList is the session list inside the resume dialog.
type SurfaceIdResumeList struct{}

// SurfaceIdResumePreview is the scrollable transcript preview under the
// resume dialog's list.
type SurfaceIdResumePreview struct{}

// SurfaceIdAutocompletePopup is the autocomplete popup rows.
type SurfaceIdAutocompletePopup struct{}

// SurfaceIdModalBody is the body of a modal dialog or wizard step.
type SurfaceIdModalBody struct{}

func (SurfaceIdTranscript) isSurfaceId()           {}
func (SurfaceIdNotificationText) isSurfaceId()     {}
func (SurfaceIdNotificationFormBody) isSurfaceId() {}
func (SurfaceIdElicitationMessage) isSurfaceId()   {}
func (SurfaceIdPromptInput) isSurfaceId()          {}
func (SurfaceIdDashboardPane) isSurfaceId()        {}
func (SurfaceIdFailurePane) isSurfaceId()          {}
func (SurfaceIdResumeList) isSurfaceId()           {}
func (SurfaceIdResumePreview) isSurfaceId()        {}
func (SurfaceIdAutocompletePopup) isSurfaceId()    {}
func (SurfaceIdModalBody) isSurfaceId()            {}

// SurfaceFrame is where a selectable surface sits on screen for one frame.
//
// Rect is the content area *inside* any border. TopRow is the content row
// currently drawn at Rect's top, and TotalRows is how many content rows the
// surface holds in total. A surface whose content is fully on screen
// registers with TopRow = 0 and TotalRows = Rect's height.
type SurfaceFrame struct {
	// ID is which surface this frame belongs to.
	ID SurfaceId
	// Rect is the content area inside the surface's border.
	Rect uv.Rectangle
	// TopRow is the content row drawn at Rect's top.
	TopRow int
	// TotalRows is the total content rows the surface holds.
	TotalRows int
}

// SurfaceFrameScrollable registers a scrollable surface showing totalRows
// rows starting at topRow.
func SurfaceFrameScrollable(id SurfaceId, rect uv.Rectangle, topRow int, totalRows int) SurfaceFrame {
	return SurfaceFrame{ID: id, Rect: rect, TopRow: topRow, TotalRows: totalRows}
}

// SurfaceFrameFixed registers a surface whose content is entirely on screen.
func SurfaceFrameFixed(id SurfaceId, rect uv.Rectangle) SurfaceFrame {
	return SurfaceFrame{ID: id, Rect: rect, TopRow: 0, TotalRows: rect.Dy()}
}

// ContentPos maps a screen cell to a content position, clamped to this
// surface.
//
// Columns clamp to Rect's horizontal span; rows clamp to the visible band and
// then to [0, TotalRows), so a pointer dragged above or below the rect lands
// on its first or last visible row.
func (f *SurfaceFrame) ContentPos(column int, row int) ContentPos {
	panic("unported: mj-chat/src/selection.rs ContentPos")
}

// ScreenRow is the screen row showing contentRow, or false when it is
// scrolled out.
func (f *SurfaceFrame) ScreenRow(contentRow int) (int, bool) {
	offset := contentRow - f.TopRow
	if offset < 0 || offset >= f.Rect.Dy() {
		return 0, false
	}
	return f.Rect.Min.Y + offset, true
}

// FrameSurfaces is the surfaces registered during the current frame, in
// render order.
type FrameSurfaces struct {
	surfaces []SurfaceFrame
}

// NewFrameSurfaces creates an empty registry.
func NewFrameSurfaces() FrameSurfaces {
	return FrameSurfaces{}
}

// Clear drops every registration; call once at the start of each frame.
func (s *FrameSurfaces) Clear() {
	s.surfaces = s.surfaces[:0]
}

// Push registers a surface. Later registrations sit above earlier ones.
func (s *FrameSurfaces) Push(surface SurfaceFrame) {
	s.surfaces = append(s.surfaces, surface)
}

// Append appends every surface other registered, preserving render order.
//
// One frame can be drawn by more than one renderer; this is how the second
// renderer's hitboxes join the first's without either owning the other's
// registry.
func (s *FrameSurfaces) Append(other *FrameSurfaces) {
	s.surfaces = append(s.surfaces, other.surfaces...)
}

// IsEmpty returns true when no surface is registered.
func (s *FrameSurfaces) IsEmpty() bool {
	return len(s.surfaces) == 0
}

// SurfaceAt is the topmost surface containing the screen cell, or nil.
//
// Render order is z-order, so the last registration wins where surfaces
// overlap and an overlay takes the hit from whatever it covers.
func (s *FrameSurfaces) SurfaceAt(column int, row int) *SurfaceFrame {
	position := uv.Pos(column, row)
	for index := len(s.surfaces) - 1; index >= 0; index-- {
		if position.In(s.surfaces[index].Rect) {
			return &s.surfaces[index]
		}
	}
	return nil
}

// Surface is the most recent registration for id, or nil when it is not on
// screen.
func (s *FrameSurfaces) Surface(id SurfaceId) *SurfaceFrame {
	for index := len(s.surfaces) - 1; index >= 0; index-- {
		if s.surfaces[index].ID == id {
			return &s.surfaces[index]
		}
	}
	return nil
}

// ContentPos is a position inside a surface's content: a content row and a
// column measured from the rect's left edge. Ordered row-major by Less.
type ContentPos struct {
	// Row is the content row, independent of scrolling.
	Row int
	// Col is the column offset from the surface's left edge.
	Col int
}

// NewContentPos creates a content position.
func NewContentPos(row int, col int) ContentPos {
	return ContentPos{Row: row, Col: col}
}

// Less is Rust's derived Ord: row first, then column.
func (p ContentPos) Less(other ContentPos) bool {
	if p.Row != other.Row {
		return p.Row < other.Row
	}
	return p.Col < other.Col
}

// SelectionRange is a normalized selection over one surface, inclusive of
// both endpoints.
//
// Semantics are linear (stream-like), not rectangular: a single-row
// selection covers Start.Col..=End.Col; a multi-row selection covers the
// first row from Start.Col to the surface's right edge, every middle row in
// full, and the last row from column 0 to End.Col.
type SelectionRange struct {
	// Start is the earlier endpoint in row-major order.
	Start ContentPos
	// End is the later endpoint in row-major order.
	End ContentPos
}

// SelectionVisualState is the portion of SelectionState that affects
// rendered highlighting.
//
// A pressed click candidate is intentionally absent: it has not acquired a
// visible highlight yet. Callers can compare this small value with == before
// and after routing a pointer event to decide whether the frame needs
// repainting.
type SelectionVisualState struct {
	// Surface contains the visible highlight, if any (nil when none).
	Surface SurfaceId
	// Range is the visible selection range when HasRange is set: a drag is
	// in progress or has completed.
	Range    SelectionRange
	HasRange bool
}

// NewSelectionRange builds a range from an anchor and a cursor, ordering
// them row-major.
func NewSelectionRange(anchor ContentPos, cursor ContentPos) SelectionRange {
	if cursor.Less(anchor) {
		return SelectionRange{Start: cursor, End: anchor}
	}
	return SelectionRange{Start: anchor, End: cursor}
}

// ColumnsOn is the inclusive column span selected on row for a surface width
// wide.
//
// Returns false when row lies outside the range or the surface has no width.
func (r *SelectionRange) ColumnsOn(row int, width int) (int, int, bool) {
	panic("unported: mj-chat/src/selection.rs ColumnsOn")
}

// SelectionAction is what the caller should do after a button release.
type SelectionAction interface {
	isSelectionAction()
}

// SelectionActionNone: nothing to do.
type SelectionActionNone struct{}

// SelectionActionClick: the button was released without ever dragging;
// forward it to the view's click handling.
type SelectionActionClick struct {
	// Column is the screen column of the release.
	Column int
	// Row is the screen row of the release.
	Row int
}

// SelectionActionCopyRequested: a drag finished. The caller extracts the
// text for Range and copies it.
type SelectionActionCopyRequested struct {
	// Surface is the surface the selection belongs to.
	Surface SurfaceId
	// Range is the normalized selected range.
	Range SelectionRange
}

func (SelectionActionNone) isSelectionAction()          {}
func (SelectionActionClick) isSelectionAction()         {}
func (SelectionActionCopyRequested) isSelectionAction() {}

// selectionPhase is Rust's private `Phase`.
type selectionPhase int

const (
	// selectionPhaseIdle: no button held and no selection to show.
	selectionPhaseIdle selectionPhase = iota
	// selectionPhasePressed: button down, no movement yet: still a click
	// candidate.
	selectionPhasePressed
	// selectionPhaseDragging: button down and moved: a selection is being
	// dragged.
	selectionPhaseDragging
	// selectionPhaseCompleted: button released after dragging; the highlight
	// persists.
	selectionPhaseCompleted
)

// SelectionState is the single global selection state machine.
//
// Wheel events never reach the engine; the caller routes them to scrolling.
type SelectionState struct {
	phase          selectionPhase
	surface        SurfaceId
	anchor         ContentPos
	cursor         ContentPos
	pressCell      uv.Position
	pointer        uv.Position
	visualRevision uint64
}

// NewSelectionState creates an idle state.
func NewSelectionState() SelectionState {
	return SelectionState{}
}

// Clear drops any selection and returns to idle.
func (s *SelectionState) Clear() {
	before := s.VisualState()
	s.phase = selectionPhaseIdle
	s.surface = nil
	s.anchor = ContentPos{}
	s.cursor = ContentPos{}
	s.pressCell = uv.Position{}
	s.pointer = uv.Position{}
	s.bumpVisualRevision(before)
}

// VisualState returns the small state that determines whether this engine
// paints a highlight, and which cells it paints.
func (s *SelectionState) VisualState() SelectionVisualState {
	r, ok := s.Range()
	if !ok {
		return SelectionVisualState{}
	}
	return SelectionVisualState{Surface: s.ActiveSurface(), Range: r, HasRange: true}
}

// VisualRevision returns a cheap revision which advances whenever the
// visible highlight changes. Comparing VisualState is preferable when a
// caller needs to know what changed; this revision is useful when it only
// needs to detect that a repaint may be needed.
func (s *SelectionState) VisualRevision() uint64 {
	return s.visualRevision
}

// ActiveSurface is the surface owning the in-flight or completed gesture, if
// any.
func (s *SelectionState) ActiveSurface() SurfaceId {
	if s.phase == selectionPhaseIdle {
		return nil
	}
	return s.surface
}

// OwnsPointer reports that a pressed/dragging selection captures the pointer
// until release.
func (s *SelectionState) OwnsPointer() bool {
	return s.phase == selectionPhasePressed || s.phase == selectionPhaseDragging
}

// ReconcileSurfaces: a surface identity belongs to one content space.
// Removing that surface invalidates its selection before highlighting,
// routing or copying.
func (s *SelectionState) ReconcileSurfaces(surfaces *FrameSurfaces) {
	if s.surface != nil && surfaces.Surface(s.surface) == nil {
		s.Clear()
	}
}

// Range is the normalized range while dragging or after a completed drag.
func (s *SelectionState) Range() (SelectionRange, bool) {
	if s.phase != selectionPhaseDragging && s.phase != selectionPhaseCompleted {
		return SelectionRange{}, false
	}
	return NewSelectionRange(s.anchor, s.cursor), true
}

// OnMouseDown starts a click candidate on the surface under the pointer.
//
// Any completed selection is dropped. A press that hits no surface leaves
// the engine idle.
func (s *SelectionState) OnMouseDown(column int, row int, surfaces *FrameSurfaces) {
	panic("unported: mj-chat/src/selection.rs OnMouseDown")
}

// OnMouseDrag extends the selection while the button is held.
//
// The first movement to a different cell promotes the candidate to a drag.
// The cursor is resolved against the surface's *current* frame, so a drag
// that runs alongside auto-scrolling tracks the rows now under the pointer.
func (s *SelectionState) OnMouseDrag(column int, row int, surfaces *FrameSurfaces) {
	if !s.OwnsPointer() {
		return
	}
	s.track(column, row, surfaces)
}

// OnMouseUp finishes the gesture.
//
// A release that never moved is reported as a SelectionActionClick for the
// view's own handlers; a release after movement yields
// SelectionActionCopyRequested and leaves the highlight in place until the
// next press or Clear.
func (s *SelectionState) OnMouseUp(column int, row int, surfaces *FrameSurfaces) SelectionAction {
	panic("unported: mj-chat/src/selection.rs OnMouseUp")
}

// Retrack re-resolves the held pointer against the current frame, extending
// the selection after the caller auto-scrolled the surface under it.
//
// A stationary pointer at a scroll edge produces no new drag events, so the
// auto-scroll tick must call this after re-rendering to pick up the rows that
// moved under the pointer.
func (s *SelectionState) Retrack(surfaces *FrameSurfaces) {
	if s.phase == selectionPhaseDragging {
		s.track(s.pointer.X, s.pointer.Y, surfaces)
	}
}

// AutoscrollRequest is the direction to auto-scroll the dragged surface, if
// the pointer sits at or beyond one of its edges.
//
// -1 scrolls up, +1 scrolls down. Surfaces with nothing scrolled out never
// request a scroll.
func (s *SelectionState) AutoscrollRequest(surfaces *FrameSurfaces) (SurfaceId, int8, bool) {
	panic("unported: mj-chat/src/selection.rs AutoscrollRequest")
}

func (s *SelectionState) track(column int, row int, surfaces *FrameSurfaces) {
	panic("unported: mj-chat/src/selection.rs track")
}

func (s *SelectionState) bumpVisualRevision(before SelectionVisualState) {
	if s.VisualState() != before {
		// The revision is diagnostic/convenience state only. Wrapping is
		// preferable to making pointer handling fail after an impossible
		// number of visible selection changes.
		s.visualRevision++
	}
}

// Highlight reverse-videos the visible part of r inside frame.
//
// Cells outside frame's rect or outside the buffer are left alone, so the
// pass is safe to run after any widget has drawn.
func Highlight(buffer *uv.Buffer, frame *SurfaceFrame, r *SelectionRange) {
	for _, span := range visibleSpans(frame, r) {
		for x := span.firstX; x <= span.lastX; x++ {
			if cell := buffer.CellAt(x, span.screenY); cell != nil {
				cell.Style.Attrs |= uv.AttrReverse
			}
		}
	}
}

// ExtractRows reads the selected text out of the frame buffer.
//
// This is for surfaces whose content is entirely on screen. Rows are trimmed
// of trailing whitespace and joined with "\n". Scrollable surfaces extract
// from their own row cache instead; the engine only reports the range.
func ExtractRows(buffer *uv.Buffer, frame *SurfaceFrame, r *SelectionRange) string {
	panic("unported: mj-chat/src/selection.rs ExtractRows")
}

// visibleSpan is one visible selected row, in screen coordinates.
type visibleSpan struct {
	screenY int
	firstX  int
	lastX   int
}

func visibleSpans(frame *SurfaceFrame, r *SelectionRange) []visibleSpan {
	panic("unported: mj-chat/src/selection.rs visibleSpans")
}

// extractSpan reads one row span, emitting each grapheme once.
//
// The buffer stores a wide grapheme in its leading cell and resets the cells
// it covers. Scanning from the surface's left edge keeps the
// wide/continuation alignment right even when the span starts mid-row, and
// continuation cells are skipped instead of emitted as spurious spaces.
func extractSpan(buffer *uv.Buffer, rowStartX int, span *visibleSpan) string {
	panic("unported: mj-chat/src/selection.rs extractSpan")
}
