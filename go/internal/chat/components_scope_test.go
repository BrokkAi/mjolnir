// Port of mj-chat/src/components/scope/tests.rs.

package chat

import (
	"fmt"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func scopeForm() Form[uint8] {
	form := NewForm[uint8]()
	form.Register(1, ControlKindTextField{}, uv.Rect(0, 0, 5, 1), true)
	form.Register(2, ControlKindButton{}, uv.Rect(0, 1, 5, 1), true)
	form.EndFrame(1)
	return form
}

func listForm() Form[uint8] {
	form := NewForm[uint8]()
	form.Register(1, ControlKindChoiceList{Len: 3, Selected: 0}, uv.Rect(0, 0, 10, 3), true)
	form.SetListContents(1, []string{"Alpha", "Beta", "Gamma"})
	form.EndFrame(1)
	return form
}

func clickAt(form *Form[uint8], row int, at time.Time) Interaction[uint8] {
	form.HandleAt(formMouseDown(1, row), at)
	return form.HandleAt(formMouseUp(1, row), at).Action
}

func requireSelected[K comparable](t *testing.T, form *Form[K], id K, want int) {
	t.Helper()
	if got, ok := form.Selected(id); !ok || got != want {
		t.Fatalf("selected = %d (%t), want %d", got, ok, want)
	}
}

func requireNoFocus[K comparable](t *testing.T, form *Form[K]) {
	t.Helper()
	if got, ok := form.Focused(); ok {
		t.Fatalf("focused = %v, want none", got)
	}
}

func TestDifferentRowsExpiredClicksAndNavigationDoNotActivate(t *testing.T) {
	form := listForm()
	now := time.Now()
	clickAt(&form, 0, now)
	requireAction[uint8](t, clickAt(&form, 1, now), InteractionSelect[uint8]{ID: 1, Index: 1})
	requireAction[uint8](t, clickAt(&form, 1, now.Add(501*time.Millisecond)), InteractionSelect[uint8]{ID: 1, Index: 1})
	form.HandleAt(key(tea.KeyDown), now.Add(502*time.Millisecond))
	requireAction[uint8](t, clickAt(&form, 1, now.Add(503*time.Millisecond)), InteractionSelect[uint8]{ID: 1, Index: 1})
}

func TestIdenticalLabelsWithNewIdentitiesDoNotActivateTheReplacement(t *testing.T) {
	form := listForm()
	now := time.Now()
	form.SetListIdentity(1, "old ids")
	clickAt(&form, 1, now)
	form.SetListIdentity(1, "replacement ids")
	requireAction[uint8](t, clickAt(&form, 1, now), InteractionSelect[uint8]{ID: 1, Index: 1})
}

func TestADisabledRowBetweenClicksBreaksThePair(t *testing.T) {
	form := listForm()
	form.controls[0].rowEnabled = []bool{true, false, true}
	now := time.Now()
	clickAt(&form, 0, now)
	requireAction(t, clickAt(&form, 1, now), nil)
	requireAction[uint8](t, clickAt(&form, 0, now), InteractionSelect[uint8]{ID: 1, Index: 0})
}

func TestChangedContentsAndResizeInvalidateDoubleClicks(t *testing.T) {
	form := listForm()
	now := time.Now()
	clickAt(&form, 1, now)
	form.SetListContents(1, []string{"Alpha", "Different item", "Gamma"})
	requireAction[uint8](t, clickAt(&form, 1, now), InteractionSelect[uint8]{ID: 1, Index: 1})
	form.HandleAt(tea.WindowSizeMsg{Width: 80, Height: 24}, now)
	requireAction[uint8](t, clickAt(&form, 1, now), InteractionSelect[uint8]{ID: 1, Index: 1})
}

func TestMovingBetweenRowsDuringAPressDoesNotSelectOrActivate(t *testing.T) {
	form := listForm()
	form.Handle(formMouseDown(1, 0))
	form.Handle(formMouseDrag(1, 1))
	requireAction(t, form.Handle(formMouseUp(1, 1)).Action, nil)
	requireSelected(t, &form, 1, 0)
}

func TestOrdinaryRedrawPreservesDoubleClickButACloneDoesNot(t *testing.T) {
	form := listForm()
	now := time.Now()
	clickAt(&form, 1, now)
	clone := form.Clone()
	requireAction[uint8](t, clickAt(&clone, 1, now), InteractionSelect[uint8]{ID: 1, Index: 1})
	form.ResetGeometry()
	form.BeginFrame()
	form.Register(1, ControlKindChoiceList{Len: 3, Selected: 1}, uv.Rect(0, 0, 10, 3), true)
	form.EndFrame(1)
	requireAction[uint8](t, clickAt(&form, 1, now.Add(DoubleClickInterval)), InteractionActivate[uint8]{ID: 1})
}

func TestFieldSpaceIsEditingAndUnicodeCursorIsAppliedByEditor(t *testing.T) {
	form := scopeForm()
	space := key(' ')
	requireAction[uint8](t, form.Handle(space).Action, InteractionEdit[uint8]{ID: 1, Edit: FieldEditKey{Key: key(' ')}})
	input := TextInputFromValue("a👩‍💻b")
	if got := ApplyFieldEdit(&input, FieldEditCursor{Offset: 2}); got != EditOutcomeChanged {
		t.Fatalf("cursor edit outcome = %v, want Changed", got)
	}
	if input.Cursor() != 1 {
		t.Fatalf("cursor = %d, want 1", input.Cursor())
	}
}

func TestFocusRepairsAfterRemovalAndDisablingEveryControl(t *testing.T) {
	form := scopeForm()
	form.Focus(2)
	form.BeginFrame()
	form.Register(1, ControlKindButton{}, uv.Rectangle{}, true)
	form.Register(3, ControlKindButton{}, uv.Rectangle{}, true)
	form.EndFrame(1)
	requireFocused(t, &form, 3)
	form.BeginFrame()
	form.Register(1, ControlKindButton{}, uv.Rectangle{}, false)
	form.Register(3, ControlKindButton{}, uv.Rectangle{}, false)
	form.EndFrame(1)
	requireNoFocus(t, &form)
	requireAction(t, form.Handle(key(tea.KeyEnter)).Action, nil)
	form.BeginFrame()
	form.Register(1, ControlKindButton{}, uv.Rectangle{}, true)
	form.EndFrame(1)
	requireFocused(t, &form, 1)
}

func TestPointerReleaseOutsideOrAfterDisappearanceNeverActivates(t *testing.T) {
	form := scopeForm()
	down := formMouseDown(1, 1)
	up := formMouseUp(1, 1)
	if !form.Handle(down).Consumed {
		t.Fatal("press on a button is not consumed")
	}
	requireFocused(t, &form, 2)
	if !form.CapturesPointer() {
		t.Fatal("press does not capture the pointer")
	}
	// A matching release activates the captured button once.
	requireAction[uint8](t, form.Handle(up).Action, InteractionActivate[uint8]{ID: 2})
	if form.CapturesPointer() {
		t.Fatal("release keeps the pointer captured")
	}
	requireAction(t, form.Handle(up).Action, nil)

	form.Handle(down)
	outside := form.Handle(formMouseUp(20, 20))
	if !outside.Consumed {
		t.Fatal("release outside is not consumed")
	}
	requireAction(t, outside.Action, nil)
	if form.CapturesPointer() {
		t.Fatal("release outside keeps the pointer captured")
	}
	requireAction(t, form.Handle(up).Action, nil)
	form.Handle(down)
	form.BeginFrame()
	form.Register(1, ControlKindTextField{}, uv.Rectangle{}, true)
	form.EndFrame(1)
	if form.CapturesPointer() {
		t.Fatal("a vanished button keeps the pointer captured")
	}
	requireAction(t, form.Handle(up).Action, nil)
}

func TestMetadataUpdatesPreserveCursorMapsAndPressedControls(t *testing.T) {
	form := scopeForm()
	form.registerWithCursorMap(1, ControlKindTextField{}, uv.Rect(0, 0, 5, 1), true, []cursorStop{{x: 0, offset: 0}, {x: 3, offset: 4}})
	form.Handle(formMouseDown(1, 1))
	form.BeginUpdate()
	form.Declare(1, ControlKindTextField{})
	form.Declare(2, ControlKindButton{})
	form.EndFrame(1)
	requireAction[uint8](t, form.Handle(formMouseUp(1, 1)).Action, InteractionActivate[uint8]{ID: 2})
	requireAction[uint8](t, form.Handle(formMouseDown(3, 0)).Action, InteractionEdit[uint8]{ID: 1, Edit: FieldEditCursor{Offset: 4}})
}

func TestCaptureSurvivesMetadataReconciliationBeforeRedraw(t *testing.T) {
	form := scopeForm()
	form.Handle(formMouseDown(1, 1))
	form.ResetGeometry()
	form.BeginUpdate()
	form.Declare(1, ControlKindTextField{})
	form.Declare(2, ControlKindButton{})
	form.EndFrame(1)
	if !form.CapturesPointer() {
		t.Fatal("metadata reconciliation drops the capture")
	}
	form.BeginFrame()
	form.Register(1, ControlKindTextField{}, uv.Rect(0, 0, 5, 1), true)
	form.Register(2, ControlKindButton{}, uv.Rect(0, 1, 5, 1), true)
	form.EndFrame(1)
	requireAction[uint8](t, form.Handle(formMouseUp(1, 1)).Action, InteractionActivate[uint8]{ID: 2})
}

func TestChangedListMetadataDropsObsoleteRowMappings(t *testing.T) {
	form := NewForm[uint8]()
	form.registerWithRows(
		1,
		ControlKindChoiceList{Len: 2, Selected: 0},
		uv.Rect(0, 0, 4, 2),
		true,
		[]*int{intPtr(0), intPtr(1)},
		[]bool{true, true},
	)
	form.EndFrame(1)
	form.BeginUpdate()
	form.Declare(1, ControlKindChoiceList{Len: 4, Selected: 0})
	form.EndFrame(1)
	requireAction[uint8](t, form.Handle(key(tea.KeyEnd)).Action, InteractionSelect[uint8]{ID: 1, Index: 3})
	if form.Contains(1, 1) {
		t.Fatal("the obsolete list hitbox survives")
	}
}

func TestActivationIgnoresRepeatReleaseAndModifiedSpace(t *testing.T) {
	form := scopeForm()
	form.Focus(2)
	for _, event := range []tea.Msg{
		tea.KeyPressMsg{Code: tea.KeyEnter, IsRepeat: true},
		tea.KeyReleaseMsg{Code: tea.KeyEnter},
	} {
		requireAction(t, form.Handle(event).Action, nil)
	}
	requireAction(t, form.Handle(chord(' ', tea.ModCtrl)).Action, nil)
	requireAction[uint8](t, form.Handle(key(' ')).Action, InteractionActivate[uint8]{ID: 2})
}

func TestDismissDragOrMoveOutsideDisarmsWithoutCancel(t *testing.T) {
	form := NewForm[uint8]()
	form.RegisterDismiss(uv.Rect(1, 0, 3, 1), true)
	form.EndFrame(1)
	form.Handle(formMouseDown(2, 0))
	if !form.DismissIsArmed() {
		t.Fatal("press does not arm the dismiss control")
	}
	requireAction(t, form.Handle(formMouseMoved(20, 20)).Action, nil)
	if form.DismissIsArmed() {
		t.Fatal("moving away keeps the dismiss control armed")
	}
	if form.CapturesPointer() {
		t.Fatal("moving away keeps the pointer captured")
	}
	requireAction(t, form.Handle(formMouseUp(2, 0)).Action, nil)
}

func TestDismissGeometryDoesNotLeaveAStaleCaptureAcrossFrames(t *testing.T) {
	form := NewForm[uint8]()
	form.RegisterDismiss(uv.Rect(1, 0, 3, 1), true)
	form.EndFrame(1)
	form.Handle(formMouseDown(2, 0))
	form.ResetGeometry()
	if form.Contains(2, 0) {
		t.Fatal("cleared geometry still contains the dismiss control")
	}
	requireAction(t, form.Handle(formMouseUp(2, 0)).Action, nil)
	form.BeginFrame()
	form.EndFrame(1)
	if form.Contains(2, 0) {
		t.Fatal("a later frame restores the dismiss geometry")
	}
	if form.CapturesPointer() {
		t.Fatal("a later frame keeps a stale capture")
	}
}

// pathForm is a path field with a following button, so focus has somewhere
// to go.
func pathForm(length int, expanded bool) Form[uint8] {
	form := NewForm[uint8]()
	form.Register(1, ControlKindPathField{Len: length, Selected: 0, Expanded: expanded}, uv.Rect(0, 0, 10, 1), true)
	form.Register(2, ControlKindButton{}, uv.Rect(0, 8, 6, 1), true)
	form.EndFrame(1)
	return form
}

func pathFieldGoldenBuffer(input *PathInput, form *Form[uint8], width, height int, focused uint8) *componentTerminal {
	terminal := newComponentTerminal(width, height)
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		PathFieldRender(frame, uv.Rect(0, 1, 12, 1), input, form, 1)
		ButtonRender(frame, uv.Rect(0, 8, 10, 1), "Continue", true, form, 2)
		form.EndFrame(focused)
	})
	return terminal
}

func appendPathGoldenState(output *strings.Builder, label string, width, height int, terminal *componentTerminal, details []string) {
	if output.Len() > 0 {
		output.WriteString("\n")
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(BufferLines(terminal.buffer), "\n"))
	output.WriteString("\n")
	for _, detail := range details {
		output.WriteString(detail + "\n")
	}
}

func debugOptionalString(value string, ok bool) string {
	return debugOption(ok, rustDebugString(value))
}

func debugOptionalFocus(form *Form[uint8]) string {
	id, ok := form.Focused()
	return debugOption(ok, fmt.Sprint(id))
}

func twoPathCandidates() core.PathCompletion {
	return core.PathCompletion{
		Candidates: []string{"~/projects/", "~/provision/"},
		Insert:     nil,
		Truncated:  false,
	}
}

func completingPath() PathInput {
	input := PathInputFromValue("~/p")
	input.RequestCompletion()
	input.ApplyCompletion("~/p", twoPathCandidates())
	return input
}

func TestGoldenPathFieldCompletion(t *testing.T) {
	var output strings.Builder
	input := PathInputFromValue("~/p")
	form := pathForm(0, false)
	appendPathGoldenState(&output, "collapsed path field", 40, 14, pathFieldGoldenBuffer(&input, &form, 40, 14, 1), nil)

	completion := form.Handle(chord(' ', tea.ModCtrl))
	requested, requestedOK := input.RequestCompletion()
	appendPathGoldenState(&output, "ctrl space requests completion", 40, 14, pathFieldGoldenBuffer(&input, &form, 40, 14, 1), []string{
		"action: " + debugInteraction(completion.Action),
		"request prefix: " + debugOptionalString(requested, requestedOK),
	})
	// KeyCode::Null: some terminals send Ctrl-Space as NUL.
	nul := form.Handle(tea.KeyPressMsg{})
	duplicate, duplicateOK := input.RequestCompletion()
	appendPathGoldenState(&output, "terminal NUL completion key", 40, 14, pathFieldGoldenBuffer(&input, &form, 40, 14, 1), []string{
		"action: " + debugInteraction(nul.Action),
		"duplicate request: " + debugOptionalString(duplicate, duplicateOK),
	})

	input.ApplyCompletion("~/p", twoPathCandidates())
	appendPathGoldenState(&output, "completion candidates", 40, 14, pathFieldGoldenBuffer(&input, &form, 40, 14, 1), nil)
	selected := form.Handle(key(tea.KeyDown))
	if action, ok := selected.Action.(InteractionSelect[uint8]); ok {
		input.SelectCompletion(action.Index)
	}
	appendPathGoldenState(&output, "keyboard previews second candidate", 40, 14, pathFieldGoldenBuffer(&input, &form, 40, 14, 1), []string{
		"action: " + debugInteraction(selected.Action),
	})
	accepted := form.Handle(key(tea.KeyEnter))
	committed := input.AcceptCompletion()
	appendPathGoldenState(&output, "enter commits candidate", 40, 14, pathFieldGoldenBuffer(&input, &form, 40, 14, 1), []string{
		"action: " + debugInteraction(accepted.Action),
		fmt.Sprintf("accepted: %t", committed),
	})

	edited := PathInputFromValue("")
	editForm := pathForm(0, false)
	edit := editForm.Handle(key('a'))
	if action, ok := edit.Action.(InteractionEdit[uint8]); ok {
		PathFieldApply(&edited, action.Edit)
	}
	appendPathGoldenState(&output, "collapsed path edits as text", 40, 14, pathFieldGoldenBuffer(&edited, &editForm, 40, 14, 1), []string{
		"action: " + debugInteraction(edit.Action),
	})
	submit := editForm.Handle(key(tea.KeyEnter))
	appendPathGoldenState(&output, "collapsed path enter activates", 40, 14, pathFieldGoldenBuffer(&edited, &editForm, 40, 14, 1), []string{
		"action: " + debugInteraction(submit.Action),
	})

	escaped := completingPath()
	escapeForm := pathForm(2, true)
	dismissal := escapeForm.Handle(key(tea.KeyEscape))
	escaped.DismissCompletion()
	appendPathGoldenState(&output, "escape dismisses candidates", 40, 14, pathFieldGoldenBuffer(&escaped, &escapeForm, 40, 14, 1), []string{
		"action: " + debugInteraction(dismissal.Action),
	})

	tabbed := completingPath()
	tabForm := pathForm(2, true)
	dismissal = tabForm.Handle(key(tea.KeyTab))
	tabbed.DismissCompletion()
	appendPathGoldenState(&output, "tab dismisses and focuses next control", 40, 14, pathFieldGoldenBuffer(&tabbed, &tabForm, 40, 14, 2), []string{
		"action: " + debugInteraction(dismissal.Action),
		"focused control: " + debugOptionalFocus(&tabForm),
	})

	clicked := completingPath()
	clickForm := pathForm(2, true)
	pathFieldGoldenBuffer(&clicked, &clickForm, 40, 14, 1)
	control := clickForm.control(1)
	if control == nil {
		t.Fatal("path field registration")
	}
	popup := control.popupArea
	x, y := popup.Min.X+1, popup.Min.Y+1
	clickForm.Handle(formMouseDown(x, y))
	click := clickForm.Handle(formMouseUp(x, y)).Action
	if action, ok := click.(InteractionPathCommit[uint8]); ok {
		clicked.SelectCompletion(action.Index)
		clicked.AcceptCompletion()
	}
	appendPathGoldenState(&output, "mouse click commits candidate", 40, 14, pathFieldGoldenBuffer(&clicked, &clickForm, 40, 14, 1), []string{
		"action: " + debugInteraction(click),
	})

	outside := completingPath()
	outsideForm := pathForm(2, true)
	dismissal = outsideForm.Handle(formMouseDown(1, 8))
	outside.DismissCompletion()
	appendPathGoldenState(&output, "outside click dismisses candidates", 40, 14, pathFieldGoldenBuffer(&outside, &outsideForm, 40, 14, 2), []string{
		"action: " + debugInteraction(dismissal.Action),
	})

	core.AssertGolden(t, "testdata", "path-field-completion", output.String())
}

// Hard-won: 20b5749a: an open popup let a click activate the control it covered.
func TestOpenPopupTakesClicksOverALaterControlItCovers(t *testing.T) {
	form := NewForm[uint8]()
	form.registerCombobox(
		1,
		ControlKindComboBox{Len: 3, Selected: 0, Expanded: true},
		uv.Rect(0, 0, 8, 1),
		true,
		uv.Rect(0, 1, 10, 5),
		[]*int{nil, intPtr(0), intPtr(1), intPtr(2), nil},
	)
	form.Register(2, ControlKindButton{}, uv.Rect(0, 3, 8, 1), true)
	form.EndFrame(1)
	form.Handle(formMouseDown(1, 3))
	requireAction[uint8](t, form.Handle(formMouseUp(1, 3)).Action, InteractionComboBoxCommit[uint8]{ID: 1, Index: 1})
}

func TestVerticalArrowsTraverseControlsInOrderSkipDisabledAndWrap(t *testing.T) {
	form := NewForm[uint8]()
	for _, control := range []struct {
		id      uint8
		kind    ControlKind
		enabled bool
	}{
		{1, ControlKindCheckbox{}, true},
		{2, ControlKindComboBox{Len: 3, Selected: 0, Expanded: false}, true},
		{3, ControlKindTextField{}, true},
		{4, ControlKindCheckbox{}, false},
		{5, ControlKindButton{}, true},
		{6, ControlKindPathField{Len: 0, Selected: 0, Expanded: false}, true},
	} {
		form.DeclareWithEnabled(control.id, control.kind, control.enabled)
	}
	form.EndFrame(1)
	for _, id := range []uint8{2, 3, 5, 6, 1} {
		result := form.Handle(key(tea.KeyDown))
		if !result.Consumed {
			t.Fatal("Down is not consumed")
		}
		requireAction(t, result.Action, nil)
		requireFocused(t, &form, id)
	}
	for _, id := range []uint8{6, 5, 3, 2, 1} {
		form.Handle(key(tea.KeyUp))
		requireFocused(t, &form, id)
	}
	form.Focus(2)
	form.Declare(2, ControlKindComboBox{Len: 3, Selected: 1, Expanded: true})
	requireAction[uint8](t, form.Handle(key(tea.KeyDown)).Action, InteractionSelect[uint8]{ID: 2, Index: 2})
	requireAction[uint8](t, form.Handle(key(tea.KeyUp)).Action, InteractionSelect[uint8]{ID: 2, Index: 1})
	requireFocused(t, &form, 2)
}

func TestRenderedHistoryAndMultilineFieldsKeepVerticalKeys(t *testing.T) {
	terminal := newComponentTerminal(30, 8)
	form := NewForm[uint8]()
	history := TextInputFromValue("draft")
	history.SetHistory([]string{"previous"})
	multiline := TextInputMultiline()
	multiline.SetValue("first\nsecond")
	terminal.draw(func(frame *Frame) {
		form.BeginFrame()
		TextFieldRender(frame, uv.Rect(0, 0, 20, 1), &history, &form, 1)
		TextFieldRenderMultiline(frame, uv.Rect(0, 2, 20, 3), &multiline, true, true, &form, 2)
		form.Declare(3, ControlKindButton{})
		form.EndFrame(1)
	})
	edit, ok := form.Handle(key(tea.KeyUp)).Action.(InteractionEdit[uint8])
	if !ok || edit.ID != 1 {
		t.Fatal("history must own Up")
	}
	TextFieldApply(&history, edit.Edit)
	if history.Value() != "previous" {
		t.Fatalf("history value = %q", history.Value())
	}
	requireFocused(t, &form, 1)
	form.Focus(2)
	edit, ok = form.Handle(key(tea.KeyUp)).Action.(InteractionEdit[uint8])
	if !ok || edit.ID != 2 {
		t.Fatal("multiline editor must own Up")
	}
	TextFieldApply(&multiline, edit.Edit)
	if multiline.Cursor() >= len("first\n") {
		t.Fatalf("multiline cursor = %d", multiline.Cursor())
	}
	requireFocused(t, &form, 2)
}

func rowRange(start, end int) *RowRange {
	return &RowRange{Start: start, End: end}
}

func requireOffset(t *testing.T, viewport FormViewport, want int) {
	t.Helper()
	if viewport.Offset() != want {
		t.Fatalf("viewport offset = %d, want %d", viewport.Offset(), want)
	}
}

func TestViewportWheelPreservesFocusAndRedrawThenFocusRevealsBlock(t *testing.T) {
	form := scopeForm()
	area := uv.Rect(0, 0, 20, 4)
	requireOffset(t, form.Viewport(area, 20, rowRange(0, 2)), 0)
	for range 6 {
		if !form.Handle(formScrollDown(12, 2)).Consumed {
			t.Fatal("wheel over the body is not consumed")
		}
	}
	requireFocused(t, &form, 1)
	// A complete redraw must not undo scrolling back to the focused field.
	form.BeginFrame()
	form.Declare(1, ControlKindTextField{})
	form.Declare(2, ControlKindButton{})
	requireOffset(t, form.Viewport(area, 20, rowRange(0, 2)), 6)
	form.EndFrame(1)
	requireOffset(t, form.Viewport(area, 20, rowRange(0, 2)), 6)
	form.Handle(key(tea.KeyDown))
	viewport := form.Viewport(area, 20, rowRange(12, 15))
	requireFocused(t, &form, 2)
	if got := viewport.Row(12, 3).Dy(); got != 3 {
		t.Fatalf("row height = %d, want 3", got)
	}
	requireOffset(t, viewport, 11)
	form.Focus(1)
	viewport = form.Viewport(area, 20, rowRange(2, 9))
	requireOffset(t, viewport, 2)
	if got := viewport.Row(2, 7); got != area {
		t.Fatalf("row = %v, want %v", got, area)
	}
	// Bounds shrink independently of focus and always clamp retained offsets.
	requireOffset(t, form.Viewport(area, 3, rowRange(2, 3)), 0)
	if form.Handle(formScrollDown(25, 2)).Consumed {
		t.Fatal("wheel outside the body is consumed")
	}

	// Programmatic focus can target a newly added, not-yet-drawn control.
	form.Declare(3, ControlKindCheckbox{})
	form.Focus(3)
	form.BeginFrame()
	form.Viewport(area, 20, rowRange(8, 10))
	form.Declare(3, ControlKindCheckbox{})
	form.EndFrame(3)
	form.Handle(formScrollDown(12, 2))
	requireOffset(t, form.Viewport(area, 20, rowRange(8, 10)), 7)
	// The first frame may establish focus after laying out the viewport.
	fresh := NewForm[uint8]()
	fresh.BeginFrame()
	fresh.Viewport(area, 20, rowRange(0, 1))
	fresh.Declare(1, ControlKindCheckbox{})
	fresh.EndFrame(1)
	fresh.Handle(formScrollDown(12, 2))
	requireOffset(t, fresh.Viewport(area, 20, rowRange(0, 1)), 1)
}

func TestViewportWheelYieldsToInnerListsAndExpandedPopups(t *testing.T) {
	for _, kind := range []ControlKind{
		ControlKindChoiceList{Len: 3, Selected: 0},
		ControlKindComboBox{Len: 3, Selected: 0, Expanded: true},
		ControlKindPathField{Len: 3, Selected: 0, Expanded: true},
	} {
		form := NewForm[uint8]()
		form.Register(1, ControlKindButton{}, uv.Rect(0, 0, 4, 1), true)
		form.Register(2, kind, uv.Rect(0, 1, 10, 3), true)
		form.EndFrame(1)
		area := uv.Rect(0, 0, 20, 5)
		form.Viewport(area, 20, rowRange(0, 1))
		requireAction[uint8](t, form.Handle(formScrollDown(1, 2)).Action, InteractionSelect[uint8]{ID: 2, Index: 1})
		requireOffset(t, form.Viewport(area, 20, rowRange(1, 4)), 0)
	}
}

func TestValidationCanRevealAGrowingBlockWithoutChangingFocus(t *testing.T) {
	form := scopeForm()
	area := uv.Rect(0, 0, 20, 4)
	requireOffset(t, form.Viewport(area, 20, rowRange(3, 4)), 0)
	// Validation adds two rows below the field; ordinary redraw preserves
	// scroll.
	requireOffset(t, form.Viewport(area, 20, rowRange(3, 6)), 0)
	form.RevealFocused()
	requireOffset(t, form.Viewport(area, 20, rowRange(3, 6)), 2)
	requireFocused(t, &form, 1)
	form.Handle(formScrollDown(12, 2))
	requireOffset(t, form.Viewport(area, 20, rowRange(3, 6)), 3)
}
