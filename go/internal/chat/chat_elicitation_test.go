// Port of mj-chat/src/chat/elicitation/tests.rs.

package chat

import (
	"encoding/json"
	"fmt"
	"maps"
	"os"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func elText(value string) *string { return &value }

func elTextKind() core.ElicitationFieldKindText { return core.ElicitationFieldKindText{} }

func elRequest(kind core.ElicitationFieldKind, required bool) core.ElicitationRequest {
	return core.ElicitationRequest{
		ID:      "ask-1",
		Message: "Choose an architecture",
		Fields: []core.ElicitationField{{
			ID:       "question_0",
			Title:    "Architecture",
			Required: required,
			Kind:     kind,
		}},
	}
}

func elPairedRequest(questionCount int, multiSelect bool) core.ElicitationRequest {
	var fields []core.ElicitationField
	for index := range questionCount {
		id := fmt.Sprintf("question_%d", index)
		options := []core.ElicitationOption{
			{Value: "alpha", Title: "Alpha", Description: elText("Choose alpha")},
			{Value: "beta", Title: "Beta", Description: elText("Choose beta")},
		}
		var kind core.ElicitationFieldKind = core.ElicitationFieldKindSingleSelect{Options: options}
		if multiSelect {
			kind = core.ElicitationFieldKindMultiSelect{Options: options, Default: []string{}}
		}
		fields = append(fields, core.ElicitationField{
			ID:          id,
			Title:       fmt.Sprintf("Question %d", index+1),
			Description: elText(fmt.Sprintf("Prompt %d", index+1)),
			Kind:        kind,
		})
		fields = append(fields, core.ElicitationField{
			ID:              id + "__other",
			Title:           "Other",
			Description:     elText("Type your own answer instead of choosing an option above."),
			CustomAnswerFor: elText(id),
			Kind:            elTextKind(),
		})
	}
	return core.ElicitationRequest{ID: "ask-paired", Message: "Input requested", Fields: fields}
}

// elKey presses code with modifiers, carrying a character's text as a
// terminal reports it (Rust's test-only ElicitationDialog::handle_key).
func elKey(dialog *elicitationDialog, code rune, modifiers tea.KeyMod) *core.ElicitationResponse {
	press := tea.KeyPressMsg{Code: code, Mod: modifiers}
	if isCharacterKey(code) && unicode.IsPrint(code) && modifiers&^tea.ModShift == 0 {
		press.Text = string(code)
	}
	return dialog.handleKeyEvent(press)
}

// elBackTab is crossterm's KeyCode::BackTab with Shift.
func elBackTab(dialog *elicitationDialog) *core.ElicitationResponse {
	return elKey(dialog, tea.KeyTab, tea.ModShift)
}

func elRendered(dialog *elicitationDialog) string {
	surfaces := NewFrameSurfaces()
	return elRenderedWithSurfaces(dialog, &surfaces)
}

func elRenderedInPane(dialog *elicitationDialog, width, height int) *uv.Buffer {
	buffer := uv.NewBuffer(width, height)
	frame := NewFrame(buffer)
	surfaces := NewFrameSurfaces()
	renderElicitationIn(frame, dialog, &surfaces, frame.Area(), true)
	return buffer
}

// elCellSymbol is ratatui's Cell::symbol: a wide glyph's continuation cell
// reads as the blank ratatui resets it to.
func elCellSymbol(buffer *uv.Buffer, x, y int) string {
	cell := buffer.CellAt(x, y)
	if cell == nil || cell.Content == "" {
		return " "
	}
	return cell.Content
}

func elBufferRow(buffer *uv.Buffer, row, start, end int) string {
	var text strings.Builder
	for column := start; column < end; column++ {
		text.WriteString(elCellSymbol(buffer, column, row))
	}
	return text.String()
}

func elBufferText(buffer *uv.Buffer) string {
	return strings.Join(BufferLines(buffer), "\n")
}

func elRenderedWithSurfaces(dialog *elicitationDialog, surfaces *FrameSurfaces) string {
	buffer := uv.NewBuffer(100, 30)
	frame := NewFrame(buffer)
	renderElicitationIn(frame, dialog, surfaces, frame.Area(), true)
	rows := make([]string, buffer.Height())
	for y := range rows {
		rows[y] = elBufferRow(buffer, y, 0, buffer.Width())
	}
	return strings.Join(rows, "\n")
}

// elFind is the screen column where label starts in row text: Rust's
// `line[..line.find(label)].chars().count()`.
func elFind(line, label string) (int, bool) {
	byteOffset := strings.Index(line, label)
	if byteOffset < 0 {
		return 0, false
	}
	return utf8.RuneCountInString(line[:byteOffset]), true
}

func elPlanReview(lineCount int) *elicitationDialog {
	lines := make([]string, lineCount)
	for line := range lines {
		lines[line] = fmt.Sprintf("plan-line-%02d", line)
	}
	return elPlanReviewMessage(strings.Join(lines, "\n"))
}

func elPlanReviewMessage(message string) *elicitationDialog {
	request := elRequest(core.ElicitationFieldKindSingleSelect{
		Options: []core.ElicitationOption{
			{Value: "implement", Title: "Implement", Description: elText("Approve and continue")},
			{Value: "revise", Title: "Revise"},
		},
		Default: elText("implement"),
	}, true)
	request.ID = "plan-review-test"
	request.Title = elText("Plan review")
	request.Fields = append(request.Fields, core.ElicitationField{
		ID:                 "feedback",
		Title:              "Revision feedback",
		Description:        elText("Describe what the agent should change"),
		CustomAnswerFor:    elText("question_0"),
		CustomAnswerOption: elText("revise"),
		Kind:               elTextKind(),
	})
	request.Message = message
	return newElicitationDialog(request)
}

// elMessagePane is the pane a plan-review dialog registered on its last
// frame.
func elMessagePane(t *testing.T, dialog *elicitationDialog) SurfaceFrame {
	t.Helper()
	surfaces := NewFrameSurfaces()
	elRenderedWithSurfaces(dialog, &surfaces)
	pane := surfaces.Surface(SurfaceIdElicitationMessage{})
	if pane == nil {
		t.Fatal("the message pane is registered")
	}
	return *pane
}

func elRange(start, end [2]int) SelectionRange {
	return SelectionRange{Start: NewContentPos(start[0], start[1]), End: NewContentPos(end[0], end[1])}
}

func elAccept(content map[string]core.ElicitationValue) *core.ElicitationResponse {
	return &core.ElicitationResponse{Action: core.ElicitationResponseAccept, Content: content}
}

// elAssertResponse compares two optional responses as Rust's PartialEq does.
func elAssertResponse(t *testing.T, got, want *core.ElicitationResponse) {
	t.Helper()
	if (got == nil) != (want == nil) || (got != nil && !got.Equal(*want)) {
		t.Fatalf("response = %s, want %s", elOptionResponseDebug(got), elOptionResponseDebug(want))
	}
}

// elValueDebug is Rust's Debug of an ElicitationValue.
func elValueDebug(value core.ElicitationValue) string {
	switch value := value.(type) {
	case core.ElicitationValueString:
		return "String(" + core.RustDebugString(string(value)) + ")"
	case core.ElicitationValueStringArray:
		items := make([]string, len(value))
		for i, item := range value {
			items[i] = core.RustDebugString(item)
		}
		return "StringArray([" + strings.Join(items, ", ") + "])"
	case core.ElicitationValueInteger:
		return fmt.Sprintf("Integer(%d)", int64(value))
	case core.ElicitationValueNumber:
		text := strconv.FormatFloat(float64(value), 'f', -1, 64)
		if !strings.ContainsAny(text, ".eE") {
			text += ".0"
		}
		return "Number(" + text + ")"
	case core.ElicitationValueBoolean:
		return fmt.Sprintf("Boolean(%t)", bool(value))
	}
	return fmt.Sprintf("%v", value)
}

// elContentDebug is Rust's Debug of a BTreeMap<String, ElicitationValue>.
func elContentDebug(content map[string]core.ElicitationValue) string {
	keys := slices.Sorted(maps.Keys(content))
	entries := make([]string, len(keys))
	for i, key := range keys {
		entries[i] = core.RustDebugString(key) + ": " + elValueDebug(content[key])
	}
	return "{" + strings.Join(entries, ", ") + "}"
}

// elResponseDebug is Rust's Debug of an ElicitationResponse.
func elResponseDebug(response core.ElicitationResponse) string {
	switch response.Action {
	case core.ElicitationResponseAccept:
		return "Accept { content: " + elContentDebug(response.Content) + " }"
	case core.ElicitationResponseDecline:
		return "Decline"
	}
	return "Cancel"
}

func elOptionResponseDebug(response *core.ElicitationResponse) string {
	if response == nil {
		return "None"
	}
	return "Some(" + elResponseDebug(*response) + ")"
}

func elResultResponseDebug(response core.ElicitationResponse, err error) string {
	if err != nil {
		return "Err(" + core.RustDebugString(err.Error()) + ")"
	}
	return "Ok(" + elResponseDebug(response) + ")"
}

func elResultContentDebug(content map[string]core.ElicitationValue, err error) string {
	if err != nil {
		return "Err(" + core.RustDebugString(err.Error()) + ")"
	}
	return "Ok(" + elContentDebug(content) + ")"
}

func elOptionIntDebug(value *int) string {
	if value == nil {
		return "None"
	}
	return fmt.Sprintf("Some(%d)", *value)
}

// elTextInputDebug is Rust's derived Debug of TextInput.
func elTextInputDebug(input *TextInput) string {
	filter := map[InputFilter]string{InputFilterAny: "Any"}[input.filter]
	if filter == "" {
		filter = fmt.Sprintf("%d", int(input.filter))
	}
	history := "None"
	if input.history != nil {
		entries := make([]string, len(input.history.entries))
		for i, entry := range input.history.entries {
			entries[i] = core.RustDebugString(entry)
		}
		history = fmt.Sprintf("Some(InputHistory { entries: [%s], index: %s, draft: %s })",
			strings.Join(entries, ", "), elOptionIntDebug(input.history.index), core.RustDebugString(input.history.draft))
	}
	return fmt.Sprintf("TextInput { value: %s, cursor: %d, kill_buffer: %s, chain_kill: %t, max_chars: %s, filter: %s, multiline: %t, preferred_column: %s, history: %s }",
		core.RustDebugString(input.value), input.cursor, core.RustDebugString(input.killBuffer), input.chainKill,
		elOptionIntDebug(input.maxChars), filter, input.multiline, elOptionIntDebug(input.preferredColumn), history)
}

// elFieldValueDebug is Rust's derived Debug of FieldValue.
func elFieldValueDebug(value fieldValue) string {
	switch value := value.(type) {
	case *fieldValueText:
		return "Text(" + elTextInputDebug(&value.Input) + ")"
	case *fieldValueSingle:
		return "Single(" + elOptionIntDebug(value.Selected) + ")"
	case *fieldValueMulti:
		items := make([]string, len(value.Selected))
		for i, item := range value.Selected {
			items[i] = strconv.Itoa(item)
		}
		return "Multi({" + strings.Join(items, ", ") + "})"
	case *fieldValueBoolean:
		return fmt.Sprintf("Boolean(%t)", value.Value)
	}
	return "?"
}

func elFieldValuesDebug(values []fieldValue) string {
	items := make([]string, len(values))
	for i, value := range values {
		items[i] = elFieldValueDebug(value)
	}
	return "[" + strings.Join(items, ", ") + "]"
}

func elFixture(t *testing.T, name string) map[string]any {
	t.Helper()
	data, err := os.ReadFile("testdata/chat/elicitation/" + name)
	if err != nil {
		t.Fatal(err)
	}
	var params map[string]any
	if err := json.Unmarshal(data, &params); err != nil {
		t.Fatal(err)
	}
	return params
}

// elFromParams builds a request from ACP params, as serde_json::Value hands
// them over: object keys in sorted order.
func elFromParams(t *testing.T, id string, params any) core.ElicitationRequest {
	t.Helper()
	data, err := json.Marshal(params)
	if err != nil {
		t.Fatal(err)
	}
	request, err := core.ElicitationRequestFromACPParams(id, data)
	if err != nil {
		t.Fatal(err)
	}
	return request
}

// elPath walks nested JSON objects and arrays by key or index.
func elPath(value any, path ...any) any {
	for _, step := range path {
		switch step := step.(type) {
		case string:
			value = value.(map[string]any)[step]
		case int:
			value = value.([]any)[step]
		}
	}
	return value
}

func TestAnUpgradePreservesUnsubmittedFormAnswers(t *testing.T) {
	request := elRequest(elTextKind(), true)
	dialog := newElicitationDialog(request)
	text := strings.Repeat("Keep this answer 🦀 ", 8_000)
	dialog.paste(text)
	encoded, err := json.Marshal(dialog.draft())
	if err != nil {
		t.Fatal(err)
	}
	if len(encoded) <= 64*1024 {
		t.Fatalf("draft is only %d bytes", len(encoded))
	}
	var draft ElicitationDraft
	if err := json.Unmarshal(encoded, &draft); err != nil {
		t.Fatal(err)
	}
	restored := elicitationDialogFromDraft(request, draft)
	if restored == nil {
		t.Fatal("the draft does not restore")
	}
	restored.focusControl(1)
	elAssertResponse(t, elKey(restored, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0": core.ElicitationValueString(text)}))
}

func TestPaneResizePreservesTheVisibleWordInsideAnIndentedParagraph(t *testing.T) {
	for _, c := range []struct {
		indent             string
		oldWidth, newWidth int
	}{
		{"", 42, 62},
		{strings.Repeat(" ", 13), 33, 57},
		{strings.Repeat(" ", 23), 57, 37},
		{strings.Repeat(" ", 80), 42, 62},
	} {
		words := make([]string, 200)
		for n := range words {
			words[n] = fmt.Sprintf("word%03d", n)
		}
		dialog := elPlanReviewMessage(c.indent + strings.Join(words, " "))
		elRenderedInPane(dialog, c.oldWidth, 18)
		dialog.scrollMessage(8)
		before := elRenderedInPane(dialog, c.oldWidth, 18)
		area := *dialog.view.messageArea
		beforeRow := elBufferRow(before, area.Min.Y, area.Min.X, area.Max.X)
		fields := strings.Fields(beforeRow)
		if len(fields) == 0 {
			t.Fatalf("no visible word in %q", beforeRow)
		}
		anchor := fields[0]
		after := elRenderedInPane(dialog, c.newWidth, 18)
		area = *dialog.view.messageArea
		afterRow := elBufferRow(after, area.Min.Y, area.Min.X, area.Max.X)
		if !strings.Contains(afterRow, anchor) {
			t.Fatalf("anchor %q disappeared from top row %q, indent length %d", anchor, afterRow, len(c.indent))
		}
	}

	words := make([]string, 400)
	for index := range words {
		words[index] = fmt.Sprintf("word-%03d", index)
	}
	dialog := elPlanReviewMessage(strings.Join(words, " "))
	elRenderedInPane(dialog, 42, 18)
	dialog.scrollMessage(8)
	if dialog.view.messageAnchor == nil {
		t.Fatal("scroll anchor")
	}
	anchor := *dialog.view.messageAnchor
	for _, width := range []int{34, 62, 42} {
		elRenderedInPane(dialog, width, 18)
		if dialog.view.messageAnchor == nil || *dialog.view.messageAnchor != anchor {
			t.Fatalf("anchor moved to %v at width %d, want %v", dialog.view.messageAnchor, width, anchor)
		}
	}

	var boundary strings.Builder
	for n := range 400 {
		fmt.Fprintf(&boundary, "w%04d", n)
	}
	dialog = elPlanReviewMessage(boundary.String())
	elRenderedInPane(dialog, 42, 18)
	dialog.scrollMessage(8)
	before := elRenderedInPane(dialog, 42, 18)
	area := *dialog.view.messageArea
	beforeRow := elBufferRow(before, area.Min.Y, area.Min.X, area.Max.X)
	after := elRenderedInPane(dialog, 34, 18)
	area = *dialog.view.messageArea
	afterRow := elBufferRow(after, area.Min.Y, area.Min.X, area.Max.X)
	if !strings.HasPrefix(afterRow, beforeRow[:8]) {
		t.Fatalf("reading position moved from %q to %q", beforeRow, afterRow)
	}
}

func TestSixRowPlanPaneKeepsTextAndPageDownActionable(t *testing.T) {
	words := make([]string, 80)
	for index := range words {
		words[index] = fmt.Sprintf("plan-%03d", index)
	}
	dialog := elPlanReviewMessage(strings.Join(words, " "))
	screen := func(buffer *uv.Buffer) string {
		rows := make([]string, 6)
		for row := range rows {
			rows[row] = elBufferRow(buffer, row, 0, 60)
		}
		return strings.Join(rows, "\n")
	}
	beforeText := screen(elRenderedInPane(dialog, 60, 6))
	for _, want := range []string{"plan-000", "Answer & Next", "PgUp/PgDn"} {
		if !strings.Contains(beforeText, want) {
			t.Fatalf("%q is hidden: %s", want, beforeText)
		}
	}
	elKey(dialog, tea.KeyPgDown, 0)
	afterText := screen(elRenderedInPane(dialog, 60, 6))
	if strings.Contains(afterText, "plan-000") {
		t.Fatalf("PageDown did not scroll: %s", afterText)
	}
	if !strings.Contains(afterText, "plan-") {
		t.Fatalf("scrolled plan text is hidden: %s", afterText)
	}
}

func TestPlanReviewPreservesContentOutsideScopedBounds(t *testing.T) {
	const width, height = 100, 30
	dialog := elPlanReview(20)
	questionBounds := uv.Rect(10, 5, 80, 20)
	buffer := uv.NewBuffer(width, height)
	frame := NewFrame(buffer)
	background := make([]Line, height)
	for i := range background {
		background[i] = RawLine(strings.Repeat("X", width))
	}
	frame.RenderWidget(NewParagraph(TextFrom(background...)), frame.Area())
	surfaces := NewFrameSurfaces()
	renderElicitationIn(frame, dialog, &surfaces, questionBounds, true)
	for y := range height {
		for x := range width {
			if !uv.Pos(x, y).In(questionBounds) && elCellSymbol(buffer, x, y) != "X" {
				t.Fatalf("scoped question changed content outside (%d, %d)", x, y)
			}
		}
	}
	if got := elCellSymbol(buffer, questionBounds.Min.X, questionBounds.Min.Y); got != "╭" {
		t.Fatalf("top-left corner = %q", got)
	}
	if got := elCellSymbol(buffer, questionBounds.Max.X-1, questionBounds.Min.Y); got != "╮" {
		t.Fatalf("top-right corner = %q", got)
	}
}

// The point of the feature: a range over whole logical lines comes back as
// the plan wrote them, without the newlines word wrap introduced, even
// though most of those rows were scrolled out of the pane.
func TestCopyingAPlanRangePastTheViewportReturnsTheUnwrappedSourceLines(t *testing.T) {
	paragraph := "This step is long enough that the plan pane wraps it over several rows, which is exactly the fugliness copying is meant to undo."
	steps := make([]string, 12)
	for step := range steps {
		steps[step] = fmt.Sprintf("Step %d: %s", step, paragraph)
	}
	message := strings.Join(steps, "\n\n")
	dialog := elPlanReviewMessage(message)
	pane := elMessagePane(t, dialog)
	if pane.TotalRows <= pane.Rect.Dy() {
		t.Fatal("the fixture has to outgrow the pane")
	}
	selection := elRange([2]int{0, 0}, [2]int{pane.TotalRows - 1, pane.Rect.Dx() - 1})
	if got := dialog.selectionText(&selection, pane.Rect.Dx()); got != message {
		t.Fatalf("copied %q", got)
	}
}

func TestPartialFirstAndLastPlanLinesAreCutAtTheSelectedColumns(t *testing.T) {
	dialog := elPlanReviewMessage("alpha beta gamma\n世界 wide row\nomega")
	// At this width the pane draws "alpha beta" / "gamma" / "世界 wide" /
	// "row" / "omega"; the wide graphemes take two cells each.
	selection := elRange([2]int{0, 6}, [2]int{3, 2})
	if got := dialog.selectionText(&selection, 10); got != "beta gamma\n世界 wide row" {
		t.Fatalf("copied %q", got)
	}
}

func TestARangeInsideOneWrappedLineRejoinsTheRowsWordWrapSplit(t *testing.T) {
	dialog := elPlanReviewMessage("alpha beta gamma\n世界 wide row\nomega")
	selection := elRange([2]int{0, 6}, [2]int{1, 2})
	if got := dialog.selectionText(&selection, 10); got != "beta gam" {
		t.Fatalf("copied %q", got)
	}
}

func TestMouseWheelScrollsThePlanWithoutMovingTheDecision(t *testing.T) {
	dialog := elPlanReview(80)
	elRendered(dialog)
	area := *dialog.view.messageArea
	dialog.handleMouse(tea.MouseWheelMsg{X: area.Min.X, Y: area.Min.Y, Button: tea.MouseWheelDown})
	if dialog.view.messageScroll != 3 {
		t.Fatalf("message scroll = %d", dialog.view.messageScroll)
	}
	if dialog.focusIndex() != 0 {
		t.Fatalf("focus index = %d", dialog.focusIndex())
	}
	if !strings.Contains(elRendered(dialog), "plan-line-03") {
		t.Fatal("the scrolled plan is not drawn")
	}
}

func TestLeavingReviseKeepsItsDraftOutOfTheAnswer(t *testing.T) {
	dialog := elPlanReview(4)
	elKey(dialog, tea.KeyDown, 0)
	dialog.paste("stale revision")
	elKey(dialog, tea.KeyUp, 0)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0": core.ElicitationValueString("implement")}))
}

func elAppendGoldenState(output *strings.Builder, label string, buffer *uv.Buffer, details []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, buffer.Width(), buffer.Height())
	output.WriteString(elBufferText(buffer))
	output.WriteByte('\n')
	for _, detail := range details {
		output.WriteString(detail + "\n")
	}
}

// Hard-won: #1265: async Codex questions must expose both suggested and custom answers.
// Hard-won: #1312: Claude's Other dropped the picks beside it and a required Codex question's own field.
func TestGoldenElicitationDialog(t *testing.T) {
	var output strings.Builder

	// Recorded ACP form from the Codex bridge's async-user-input regression.
	billing := newElicitationDialog(elFromParams(t, "async-billing-question", elFixture(t, "codex-async-user-input.json")))
	elAppendGoldenState(&output, "Codex async question with a suggested answer", elRenderedInPane(billing, 100, 20), nil)
	elKey(billing, tea.KeyDown, 0)
	elKey(billing, tea.KeyDown, 0)
	billing.paste("Use a third AWS billing account")
	elAppendGoldenState(&output, "Codex async question with a custom answer", elRenderedInPane(billing, 100, 20), nil)
	elKey(billing, tea.KeyEnter, 0)
	elKey(billing, tea.KeyDown, 0)
	elKey(billing, tea.KeyEnter, 0)
	billing.paste("BedrockCI")
	answer := elKey(billing, tea.KeyEnter, 0)
	if answer == nil {
		t.Fatal("the last answer submits the form")
	}
	harness, harnessErr := billing.req.HarnessResponse(*answer)
	elAppendGoldenState(&output, "Codex answers as submitted and as the harness receives them",
		elRenderedInPane(billing, 100, 20), []string{
			"submitted: " + elResponseDebug(*answer),
			"harness: " + elResultResponseDebug(harness, harnessErr),
		})

	// Recorded ACP form from claude-agent-acp 0.90.0 for an Air client
	// (askUserQuestionsToCreateRequest): a single-select and a multi-select
	// question, each with its "Other" companion.
	claude := newElicitationDialog(elFromParams(t, "claude-questions", elFixture(t, "claude-ask-user-question.json")))
	elAppendGoldenState(&output, "Claude question starts with its first option picked", elRenderedInPane(claude, 100, 20), nil)
	// Keep the default pick while writing: arrows to the text box under the
	// choices pass Memcached without picking it, and the pick stays Redis.
	elKey(claude, tea.KeyDown, 0)
	elKey(claude, tea.KeyDown, 0)
	claude.paste("Keep TTLs short")
	elAppendGoldenState(&output, "Claude pick kept while writing Other", elRenderedInPane(claude, 100, 20), nil)
	elKey(claude, tea.KeyEnter, 0)
	elKey(claude, ' ', 0)
	elKey(claude, tea.KeyDown, 0)
	elKey(claude, ' ', 0)
	elKey(claude, tea.KeyEnd, 0)
	claude.paste("freebsd")
	elAppendGoldenState(&output, "Claude multi-select picks with Other text", elRenderedInPane(claude, 100, 20), nil)
	answer = elKey(claude, tea.KeyEnter, 0)
	if answer == nil {
		t.Fatal("the last answer submits the form")
	}
	harness, harnessErr = claude.req.HarnessResponse(*answer)
	elAppendGoldenState(&output, "Claude answers as submitted and as the harness receives them",
		elRenderedInPane(claude, 100, 20), []string{
			"submitted: " + elResponseDebug(*answer),
			"harness: " + elResultResponseDebug(harness, harnessErr),
		})
	// Take back the default pick so the text alone is the whole answer, also
	// when the arrows pass every option on the way to it.
	own := newElicitationDialog(claude.req)
	elKey(own, ' ', 0)
	elKey(own, tea.KeyDown, 0)
	elKey(own, tea.KeyDown, 0)
	for _, character := range "Valkey" {
		elKey(own, character, 0)
	}
	buffer := elRenderedInPane(own, 100, 20)
	elKey(own, tea.KeyEnter, 0)
	elKey(own, tea.KeyEnter, 0)
	if !own.confirmingUnanswered {
		t.Fatal("the targets question is unanswered")
	}
	elKey(own, tea.KeyTab, 0)
	answer = elKey(own, ' ', 0)
	if answer == nil {
		t.Fatal("submitting with an unanswered question still answers")
	}
	elAppendGoldenState(&output, "Claude default taken back, arrows to Other pick nothing on the way",
		buffer, []string{"submitted: " + elResponseDebug(*answer)})

	// A click or Space toggles an option: it picks it, replacing any other
	// pick, and takes back the option already picked. A mouse-only answer
	// can therefore still end with nothing picked.
	pageAnswer := func(dialog *elicitationDialog) string {
		return elResultContentDebug(dialog.questionContent(0))
	}
	// Clicks on a fixed clock, each well past the double-click interval.
	start := time.Now()
	later := func(clicks int) time.Time { return start.Add(DoubleClickInterval * 3 * time.Duration(clicks)) }
	toggles := newElicitationDialog(claude.req)
	elKey(toggles, ' ', 0)
	steps := []string{"Space takes back default Redis: " + pageAnswer(toggles)}
	for click, step := range []struct{ label, target string }{
		{"click Redis", "Redis"},
		{"click Memcached", "Memcached"},
		{"click Memcached again, later", "Memcached"},
	} {
		elClickQuestionButtonAt(t, toggles, step.target, later(click))
		steps = append(steps, step.label+": "+pageAnswer(toggles))
	}
	elKey(toggles, tea.KeyUp, 0)
	elKey(toggles, ' ', 0)
	steps = append(steps, "Space on Redis: "+pageAnswer(toggles))
	elKey(toggles, ' ', 0)
	steps = append(steps, "Space on Redis again: "+pageAnswer(toggles))
	elClickQuestionButtonAt(t, toggles, "Other", later(3))
	toggles.paste("Valkey")
	steps = append(steps, "click Other, write Valkey: "+pageAnswer(toggles))
	elAppendGoldenState(&output, "Claude clicks and Space toggle the pick", elRenderedInPane(toggles, 100, 20), steps)

	// A double-click on an option is an activation, as on any other list:
	// the option ends picked and the question is answered. A real
	// double-click lands twice on one screen cell, and the first click moves
	// the cursor, which reflows the rows below it, so the second click lands
	// on whatever row now occupies that cell. Starting from the default
	// cursor on Redis, that is Other on the recorded form and Dragonfly once
	// a third option follows Memcached.
	withDragonfly := elFixture(t, "claude-ask-user-question.json")
	question := elPath(withDragonfly, "requestedSchema", "properties", "question_0").(map[string]any)
	question["oneOf"] = append(question["oneOf"].([]any), map[string]any{
		"const":       "Dragonfly",
		"title":       "Dragonfly",
		"description": "Redis-compatible, multi-threaded",
	})
	steps = nil
	var double *elicitationDialog
	for _, c := range []struct {
		label   string
		request core.ElicitationRequest
	}{
		{"recorded form", claude.req},
		{"with Dragonfly after Memcached", elFromParams(t, "claude-questions", withDragonfly)},
	} {
		double = newElicitationDialog(c.request)
		column, row := elLabelCell(t, double, "Memcached")
		first := start.Add(DoubleClickInterval * 3)
		elClickCellAt(double, column, row, first)
		redrawn := elRenderedInPane(double, 110, 22)
		underCell := strings.TrimFunc(elBufferRow(redrawn, row, 0, redrawn.Width()), func(character rune) bool {
			return character == '│' || unicode.IsSpace(character)
		})
		response := elClickCellAt(double, column, row, first.Add(DoubleClickInterval/2))
		steps = append(steps, fmt.Sprintf("%s: second click lands on %s; %s, answered=%t, now on question %d, response=%s",
			c.label, core.RustDebugString(underCell), pageAnswer(double), double.confirmed.contains(0),
			double.currentQuestion+1, elOptionResponseDebug(response)))
	}
	elAppendGoldenState(&output, "Claude double-click picks the option and answers", elRenderedInPane(double, 100, 20), steps)

	// Enter answers with what is already there. It takes the option under
	// the cursor only when the question would otherwise be empty. Explicitly
	// take back Redis before exercising Enter on an empty answer.
	empty := newElicitationDialog(claude.req)
	elKey(empty, ' ', 0)
	elKey(empty, tea.KeyDown, 0)
	elKey(empty, tea.KeyEnter, 0)
	written := newElicitationDialog(claude.req)
	elKey(written, ' ', 0)
	elKey(written, tea.KeyDown, 0)
	elKey(written, tea.KeyDown, 0)
	written.paste("Valkey")
	elKey(written, tea.KeyUp, 0)
	buffer = elRenderedInPane(written, 100, 20)
	elKey(written, tea.KeyEnter, 0)
	elAppendGoldenState(&output, "Claude Enter on an option answers with what is there", buffer, []string{
		"Enter on Memcached, nothing picked, Other blank: " + pageAnswer(empty),
		"Enter on Memcached, nothing picked, Other holds Valkey: " + pageAnswer(written),
	})

	// Tool permissions share the three form actions; rejection is chosen
	// from their permission options. Preserve the harness response
	// semantics for one-time rejection and persistent network rules.
	for _, c := range []struct {
		label, id, message string
		options            []any
		skip               bool
	}{
		{
			"Claude permission form uses the same three actions",
			"tool-permission-1",
			"Claude Code requests permission:\nRun cargo test",
			[]any{
				map[string]any{"const": "allow-once", "title": "Yes"},
				map[string]any{"const": "allow-with-updates", "title": "Yes, and don't ask again for `cargo test` commands"},
				map[string]any{"const": "reject", "title": "No"},
			},
			true,
		},
		{
			"persistent permission form uses the same three actions",
			"tool-permission-2",
			"Codex requests permission:\nReach crates.io",
			[]any{
				map[string]any{"const": "network_policy_amendment:0", "title": "Yes, and allow this host in the future"},
				map[string]any{"const": "network_policy_amendment:1", "title": "No, and block this host in the future"},
			},
			false,
		},
	} {
		request := elFromParams(t, c.id, map[string]any{
			"mode": "form", "sessionId": "session-1",
			"message": c.message,
			"requestedSchema": map[string]any{"type": "object", "required": []any{"choice"}, "properties": map[string]any{
				"choice": map[string]any{"type": "string", "title": "Permission", "oneOf": c.options},
			}},
		})
		request.ToolPermission = &core.ToolPermissionForm{Skip: c.skip}
		dialog := newElicitationDialog(request)
		skip, skipErr := dialog.req.HarnessResponse(core.ElicitationResponse{Action: core.ElicitationResponseDecline})
		elAppendGoldenState(&output, c.label, elRenderedInPane(dialog, 100, 14),
			[]string{"skip: " + elResultResponseDebug(skip, skipErr)})
	}

	smallest := newElicitationDialog(elPairedRequest(1, true))
	elKey(smallest, tea.KeyEnd, 0)
	smallest.paste("custom draft")
	buffer = elRenderedInPane(smallest, 60, 6)
	elAppendGoldenState(&output, "small question pane keeps the focused custom answer and actions", buffer,
		[]string{"focused value=" + elFieldValuesDebug(smallest.values)})

	plan := elPlanReview(80)
	before := elRenderedInPane(plan, 100, 30)
	elAppendGoldenState(&output, "plan review before scrolling", before,
		[]string{fmt.Sprintf("focus index=%d", plan.focusIndex())})
	for range 10 {
		elKey(plan, tea.KeyPgDown, 0)
		elRenderedInPane(plan, 100, 30)
	}
	after := elRenderedInPane(plan, 100, 30)
	elAppendGoldenState(&output, "plan review after paging to the end", after,
		[]string{fmt.Sprintf("focus index=%d", plan.focusIndex())})

	for _, c := range []struct {
		label      string
		clickTitle bool
	}{{"boolean checkbox click", false}, {"boolean field title click", true}} {
		defaultFalse := false
		dialog := newElicitationDialog(elRequest(core.ElicitationFieldKindBoolean{Default: &defaultFalse}, false))
		initial := elRenderedInPane(dialog, 100, 30)
		column, row, found := 0, 0, false
		if c.clickTitle {
			title := dialog.req.Fields[0].Title
			for y := 0; y < initial.Height() && !found; y++ {
				column, found = elFind(elBufferRow(initial, y, 0, initial.Width()), title)
				row = y
			}
		} else {
			for x := 0; x < initial.Width() && !found; x++ {
				for y := 0; y < initial.Height() && !found; y++ {
					if elCellSymbol(initial, x, y) == "☐" {
						column, row, found = x, y, true
					}
				}
			}
		}
		if !found {
			t.Fatalf("%s: the target is not rendered", c.label)
		}
		dialog.handleMouse(tea.MouseClickMsg{X: column, Y: row, Button: tea.MouseLeft})
		dialog.handleMouse(tea.MouseReleaseMsg{X: column, Y: row, Button: tea.MouseLeft})
		elAppendGoldenState(&output, c.label, elRenderedInPane(dialog, 100, 30),
			[]string{"field value=" + elFieldValueDebug(dialog.values[0])})
	}

	revise := elPlanReview(4)
	elAppendGoldenState(&output, "plan review implement option selected", elRenderedInPane(revise, 100, 30), nil)
	elKey(revise, tea.KeyDown, 0)
	elAppendGoldenState(&output, "revise option exposes inline feedback", elRenderedInPane(revise, 100, 30), nil)
	revise.paste("add a regression test")
	action := elKey(revise, tea.KeyEnter, 0)
	elAppendGoldenState(&output, "revise submits its feedback with the selected action", elRenderedInPane(revise, 100, 30),
		[]string{"action: " + elOptionResponseDebug(action)})

	paired := newElicitationDialog(elPairedRequest(3, false))
	elAppendGoldenState(&output, "paired custom answer starts on question one of three", elRenderedInPane(paired, 100, 30),
		[]string{fmt.Sprintf("display fields=%d", len(paired.displayFields))})
	elKey(paired, tea.KeyEnter, 0)
	elAppendGoldenState(&output, "paired custom answer advances to question two of three", elRenderedInPane(paired, 100, 30),
		[]string{fmt.Sprintf("display fields=%d", len(paired.displayFields))})

	elKey(paired, tea.KeyEnter, 0)
	elAppendGoldenState(&output, "last question keeps the same three actions", elRenderedInPane(paired, 100, 20), nil)
	elClickQuestionButton(t, paired, "Previous")
	elAppendGoldenState(&output, "previous returns to the confirmed answer", elRenderedInPane(paired, 100, 20), nil)

	partial := newElicitationDialog(elPairedRequest(1, true))
	elKey(partial, tea.KeyEnter, 0)
	elAppendGoldenState(&output, "unanswered warning keeps the same three actions", elRenderedInPane(partial, 100, 20), nil)
	response := elClickQuestionButton(t, partial, "Cancel")
	elAppendGoldenState(&output, "cancel remains available from the unanswered warning", elRenderedInPane(partial, 100, 20),
		[]string{"action: " + elOptionResponseDebug(response)})

	core.AssertGolden(t, "testdata", "elicitation-dialog", output.String())
}

func TestAClaudeQuestionStartsWithItsRecommendedOptionPicked(t *testing.T) {
	params := elFixture(t, "claude-ask-user-question.json")
	elPath(params, "requestedSchema", "properties", "question_0", "oneOf", 1).(map[string]any)["title"] = "Memcached (Recommended)"
	dialog := newElicitationDialog(elFromParams(t, "claude-recommended", params))
	if dialog.optionCursors[0] != 1 {
		t.Fatalf("the cursor starts on the pick: %d", dialog.optionCursors[0])
	}
	content, err := dialog.questionContent(0)
	if got, want := elResultContentDebug(content, err), `Ok({"question_0": String("Memcached")})`; got != want {
		t.Fatalf("question content = %s, want %s", got, want)
	}
}

func TestClaudeQuestionsStartWithTheirRecommendedOrFirstOptionPicked(t *testing.T) {
	params := elFixture(t, "claude-ask-user-question.json")
	elPath(params, "requestedSchema", "properties", "question_0", "oneOf", 1).(map[string]any)["title"] = "Memcached (Recommended)"
	question := elPath(params, "requestedSchema", "properties", "question_1").(map[string]any)
	question["type"] = "string"
	items := question["items"].(map[string]any)
	delete(question, "items")
	question["oneOf"] = items["anyOf"]
	dialog := newElicitationDialog(elFromParams(t, "claude-mixed-recommendations", params))
	if dialog.optionCursors[0] != 1 {
		t.Fatalf("Q1 starts on its recommendation: %d", dialog.optionCursors[0])
	}
	if dialog.optionCursors[1] != 0 {
		t.Fatalf("Q2 starts on its first option: %d", dialog.optionCursors[1])
	}
	for index, value := range []string{"Memcached", "linux"} {
		content, err := dialog.questionContent(index)
		want := fmt.Sprintf(`Ok({"question_%d": String(%q)})`, index, value)
		if got := elResultContentDebug(content, err); got != want {
			t.Fatalf("question %d content = %s, want %s", index, got, want)
		}
	}
}

func TestCustomAnswerUsesTheAdapterFieldInsteadOfTheStaleSelection(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(1, false))
	elKey(dialog, tea.KeyDown, 0)
	elKey(dialog, tea.KeyDown, 0)
	for _, character := range "custom answer" {
		elKey(dialog, character, 0)
	}
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0__other": core.ElicitationValueString("custom answer")}))
}

func TestChoosingAnOptionAfterTypingOtherOmitsTheCustomDraft(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(1, false))
	elKey(dialog, tea.KeyEnd, 0)
	dialog.paste("custom draft")
	elKey(dialog, tea.KeyUp, 0)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0": core.ElicitationValueString("beta")}))
}

func TestTogglingAMultiSelectOptionDeactivatesOther(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(1, true))
	elKey(dialog, tea.KeyEnd, 0)
	dialog.paste("custom draft")
	elKey(dialog, tea.KeyUp, 0)
	elKey(dialog, ' ', 0)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0": core.ElicitationValueStringArray{"beta"}}))
}

func TestDanglingCustomMetadataRemainsAStandalonePage(t *testing.T) {
	request := elPairedRequest(1, false)
	request.Fields[1].CustomAnswerFor = elText("missing")
	dialog := newElicitationDialog(request)
	if len(dialog.displayFields) != 2 {
		t.Fatalf("display fields = %d", len(dialog.displayFields))
	}
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	screen := elRendered(dialog)
	if !strings.Contains(screen, "2/2") || !strings.Contains(screen, "Other") {
		t.Fatalf("the second page is not drawn:\n%s", screen)
	}
}

func elComponentForm() core.ElicitationRequest {
	defaultFalse := false
	return core.ElicitationRequest{
		ID:      "ask-component",
		Message: "Component form: edit the label and choose the options.",
		Fields: []core.ElicitationField{
			{ID: "label", Title: "Label", Kind: elTextKind()},
			{ID: "enabled", Title: "Enabled", Kind: core.ElicitationFieldKindBoolean{Default: &defaultFalse}},
			{ID: "choice", Title: "Choice", Kind: core.ElicitationFieldKindSingleSelect{Options: []core.ElicitationOption{
				{Value: "one", Title: "One"},
				{Value: "two", Title: "Two"},
			}}},
		},
	}
}

// Hard-won: 9651f8c: compact forms scrolled the focused field title away from its control
func TestCompactQuestionPaneShowsTheFocusedFieldTitleWithItsControl(t *testing.T) {
	dialog := newElicitationDialog(elComponentForm())
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	if dialog.focusIndex() != 1 {
		t.Fatalf("focus index = %d", dialog.focusIndex())
	}
	const width = 78
	for _, height := range []int{dialog.naturalHeight(width), 10, 8, 6, 5} {
		text := elBufferText(elRenderedInPane(dialog, width, height))
		if !strings.Contains(text, "Enabled") {
			t.Fatalf("field title missing at height %d:\n%s", height, text)
		}
		if !strings.Contains(text, "☐ No") {
			t.Fatalf("boolean control missing at height %d:\n%s", height, text)
		}
		if !strings.Contains(text, "Answer & Next") {
			t.Fatalf("actions missing:\n%s", text)
		}
	}
}

func TestCompactQuestionKeepsValidationErrorsAndTheLabeledControlVisible(t *testing.T) {
	dialog := newElicitationDialog(elRequest(elTextKind(), true))
	dialog.focusControl(1)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	text := elBufferText(elRenderedInPane(dialog, 78, 5))
	if !strings.Contains(text, "Architecture") || !strings.Contains(text, "1/1") {
		t.Fatalf("field title missing:\n%s", text)
	}
	if !strings.Contains(text, "Architecture is required") {
		t.Fatalf("error missing:\n%s", text)
	}
	if !strings.Contains(text, "Answer & Next") {
		t.Fatalf("actions missing:\n%s", text)
	}
}

// Hard-won: 9651f8c: scrolling an option list hid the title identifying its field
func TestAScrolledOptionListKeepsShowingTheFieldItAnswers(t *testing.T) {
	options := make([]core.ElicitationOption, 12)
	for index := range options {
		options[index] = core.ElicitationOption{Value: fmt.Sprintf("option-%d", index), Title: fmt.Sprintf("Option %d", index)}
	}
	dialog := newElicitationDialog(elRequest(core.ElicitationFieldKindSingleSelect{Options: options}, false))
	for range 11 {
		elKey(dialog, tea.KeyDown, 0)
	}
	text := elBufferText(elRenderedInPane(dialog, 78, 12))
	if !strings.Contains(text, "Architecture") {
		t.Fatalf("field title missing while the last option is focused:\n%s", text)
	}
	if !strings.Contains(text, "Option 11") {
		t.Fatalf("focused option missing:\n%s", text)
	}
}

func TestAnsweringAdvancesOneQuestionAndOnlySendsConfirmedAnswers(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(3, false))
	if len(dialog.unanswered()) != 3 {
		t.Fatalf("unanswered = %v", dialog.unanswered())
	}
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if dialog.currentQuestion != 1 || len(dialog.unanswered()) != 2 {
		t.Fatalf("question %d, unanswered %v", dialog.currentQuestion, dialog.unanswered())
	}
	elKey(dialog, tea.KeyDown, 0)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if dialog.currentQuestion != 2 {
		t.Fatalf("question %d", dialog.currentQuestion)
	}
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), elAccept(map[string]core.ElicitationValue{
		"question_0": core.ElicitationValueString("alpha"),
		"question_1": core.ElicitationValueString("beta"),
		"question_2": core.ElicitationValueString("alpha"),
	}))
}

func TestNavigatingPastDefaultsRequiresASeparateDeliberatePartialSubmission(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(3, false))
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	warning := elRendered(dialog)
	for _, want := range []string{"2 unanswered", "Question 1", "Question 2"} {
		if !strings.Contains(warning, want) {
			t.Fatalf("%q missing from the warning:\n%s", want, warning)
		}
	}
	// Enter on the default warning action goes back, never submits.
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if dialog.currentQuestion != 0 || dialog.confirmingUnanswered {
		t.Fatalf("question %d, confirming %t", dialog.currentQuestion, dialog.confirmingUnanswered)
	}
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	elBackTab(dialog)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_2": core.ElicitationValueString("alpha")}))
}

func TestEscapeFromTheWarningReturnsToTheFirstUnansweredQuestion(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(2, false))
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elKey(dialog, tea.KeyEnter, 0)
	elAssertResponse(t, elKey(dialog, tea.KeyEscape, 0), nil)
	if dialog.currentQuestion != 0 || dialog.confirmingUnanswered {
		t.Fatalf("question %d, confirming %t", dialog.currentQuestion, dialog.confirmingUnanswered)
	}
}

func TestEditingAConfirmedAnswerRequiresConfirmationAgain(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(2, false))
	elKey(dialog, tea.KeyEnter, 0)
	elKey(dialog, tea.KeyLeft, tea.ModAlt)
	if len(dialog.unanswered()) != 1 {
		t.Fatalf("unanswered = %v", dialog.unanswered())
	}
	elKey(dialog, tea.KeyDown, 0)
	if len(dialog.unanswered()) != 2 {
		t.Fatalf("unanswered = %v", dialog.unanswered())
	}
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if !dialog.confirmingUnanswered {
		t.Fatal("the warning is not shown")
	}
}

func TestRequiredUnconfirmedDefaultsReturnToTheRequiredQuestion(t *testing.T) {
	request := elPairedRequest(2, false)
	request.Fields[0].Required = true
	dialog := newElicitationDialog(request)
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if dialog.currentQuestion != 0 {
		t.Fatalf("question %d", dialog.currentQuestion)
	}
	if dialog.error == nil || *dialog.error != "Question 1 is required" {
		t.Fatalf("error = %v", dialog.error)
	}
	if dialog.confirmingUnanswered {
		t.Fatal("the warning is shown")
	}
}

func TestFalseIsAnAnswerButOptionalWhitespaceIsNot(t *testing.T) {
	dialog := newElicitationDialog(elRequest(core.ElicitationFieldKindBoolean{}, true))
	if !strings.Contains(elRendered(dialog), "Space toggle") {
		t.Fatal("the toggle hint is missing")
	}
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0": core.ElicitationValueBoolean(false)}))
	request := elPairedRequest(1, false)
	request.Fields = []core.ElicitationField{request.Fields[1]}
	dialog = newElicitationDialog(request)
	dialog.paste("   ")
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if !dialog.confirmingUnanswered {
		t.Fatal("the warning is not shown")
	}
}

func TestEmptyFormsStillSubmitAndButtonFocusKeepsTheQuestionVisible(t *testing.T) {
	empty := elPairedRequest(0, false)
	empty.Message = "Continue?"
	// Hard-won: 9739e9996: compact fieldless and confirmation forms lost their clickable action row.
	for _, height := range []int{3, 4} {
		dialog := newElicitationDialog(empty)
		buffer := elRenderedInPane(dialog, 100, height)
		column, row, found := 0, 0, false
		for y := 0; y < height && !found; y++ {
			column, found = elFind(elBufferRow(buffer, y, 0, 100), "Answer & Next")
			row = y
		}
		if !found {
			t.Fatal("compact action button remains visible")
		}
		elAssertResponse(t, elClickCellAt(dialog, column, row, time.Now()), elAccept(map[string]core.ElicitationValue{}))
	}
	dialog := newElicitationDialog(empty)
	if !strings.Contains(elBufferText(elRenderedInPane(dialog, 60, 4)), "Continue?") {
		t.Fatal("the message is hidden")
	}
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), elAccept(map[string]core.ElicitationValue{}))
	dialog = newElicitationDialog(elPairedRequest(2, false))
	elKey(dialog, tea.KeyTab, 0)
	screen := elRendered(dialog)
	if !strings.Contains(screen, "Question 1") || !strings.Contains(screen, "Alpha") {
		t.Fatalf("the question is hidden:\n%s", screen)
	}
	if dialog.currentQuestion != 0 {
		t.Fatalf("question %d", dialog.currentQuestion)
	}
}

func TestUpgradeDraftsPreserveConfirmationAndOldDraftsStartUnconfirmed(t *testing.T) {
	request := elPairedRequest(3, false)
	dialog := newElicitationDialog(request)
	elKey(dialog, tea.KeyEnd, 0)
	dialog.paste("custom text")
	elKey(dialog, tea.KeyEnter, 0)
	encoded, err := json.Marshal(dialog.draft())
	if err != nil {
		t.Fatal(err)
	}
	var draft ElicitationDraft
	if err := json.Unmarshal(encoded, &draft); err != nil {
		t.Fatal(err)
	}
	restored := elicitationDialogFromDraft(request, draft)
	if restored == nil {
		t.Fatal("the draft does not restore")
	}
	if restored.currentQuestion != 1 {
		t.Fatalf("question %d", restored.currentQuestion)
	}
	if !slices.Equal(restored.confirmed, indexSet{0}) {
		t.Fatalf("confirmed = %v", restored.confirmed)
	}
	var old map[string]json.RawMessage
	if err := json.Unmarshal(encoded, &old); err != nil {
		t.Fatal(err)
	}
	delete(old, "confirmed")
	delete(old, "current_question")
	oldEncoded, err := json.Marshal(old)
	if err != nil {
		t.Fatal(err)
	}
	var oldDraft ElicitationDraft
	if err := json.Unmarshal(oldEncoded, &oldDraft); err != nil {
		t.Fatal(err)
	}
	restored = elicitationDialogFromDraft(request, oldDraft)
	if restored == nil {
		t.Fatal("the old draft does not restore")
	}
	if len(restored.unanswered()) != 3 {
		t.Fatalf("unanswered = %v", restored.unanswered())
	}
	elKey(restored, tea.KeyLeft, tea.ModAlt)
	if !strings.Contains(elRendered(restored), "custom text") {
		t.Fatal("the custom text is lost")
	}
}

func elClickQuestionButton(t *testing.T, dialog *elicitationDialog, label string) *core.ElicitationResponse {
	t.Helper()
	return elClickQuestionButtonAt(t, dialog, label, time.Now())
}

// elClickQuestionButtonAt clicks label at an explicit time, so two clicks
// are a double-click exactly when the test places them within the interval.
func elClickQuestionButtonAt(t *testing.T, dialog *elicitationDialog, label string, now time.Time) *core.ElicitationResponse {
	t.Helper()
	column, row := elLabelCell(t, dialog, label)
	return elClickCellAt(dialog, column, row, now)
}

// elLabelCell is the screen cell where label starts once the dialog is
// drawn.
func elLabelCell(t *testing.T, dialog *elicitationDialog, label string) (int, int) {
	t.Helper()
	buffer := elRenderedInPane(dialog, 110, 22)
	for row := range buffer.Height() {
		if column, ok := elFind(elBufferRow(buffer, row, 0, buffer.Width()), label); ok {
			return column, row
		}
	}
	t.Fatalf("label %q not rendered", label)
	return 0, 0
}

// elClickCellAt presses and releases the left button on one screen cell, as
// a real click does, whatever is drawn there.
func elClickCellAt(dialog *elicitationDialog, column, row int, now time.Time) *core.ElicitationResponse {
	dialog.handleMouseAt(tea.MouseClickMsg{X: column, Y: row, Button: tea.MouseLeft}, now)
	return dialog.handleMouseAt(tea.MouseReleaseMsg{X: column, Y: row, Button: tea.MouseLeft}, now)
}

func TestMouseNavigationAndSubmissionShareTheKeyboardConfirmationRules(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(2, false))
	elKey(dialog, tea.KeyEnd, 0)
	dialog.paste(" ")
	elAssertResponse(t, elClickQuestionButton(t, dialog, "Answer & Next"), nil)
	elAssertResponse(t, elClickQuestionButton(t, dialog, "Answer & Next"), nil)
	if !dialog.confirmingUnanswered {
		t.Fatal("the warning is not shown")
	}
	// Hard-won: 9739e9996: compact confirmations had no mouse action to finish the answer.
	buffer := elRenderedInPane(dialog, 100, 4)
	column, row, found := 0, 0, false
	for y := 0; y < 4 && !found; y++ {
		column, found = elFind(elBufferRow(buffer, y, 0, 100), "Answer & Next")
		row = y
	}
	if !found {
		t.Fatal("confirmation action remains visible")
	}
	elAssertResponse(t, elClickCellAt(dialog, column, row, time.Now()),
		elAccept(map[string]core.ElicitationValue{"question_1": core.ElicitationValueString("alpha")}))
}

func TestMultilineQuestionTextWrapsScrollsAndSurvivesResize(t *testing.T) {
	request := elPairedRequest(1, false)
	details := make([]string, 100)
	for index := range details {
		details[index] = fmt.Sprintf("detail%03d", index)
	}
	request.Fields[0].Title = "How should we handle JPEG support given that our existing linearization differs?\n" +
		strings.Join(details, " ") + "\nFinal authored line"
	dialog := newElicitationDialog(request)
	first := elBufferText(elRenderedInPane(dialog, 48, 16))
	for _, want := range []string{"How should we handle JPEG", "existing linearization", "Answer & Next"} {
		if !strings.Contains(first, want) {
			t.Fatalf("%q missing:\n%s", want, first)
		}
	}
	elKey(dialog, tea.KeyPgDown, 0)
	before := elRenderedInPane(dialog, 48, 16)
	area, _ := dialog.messageArea()
	top := elBufferRow(before, area.Min.Y, area.Min.X, area.Max.X)
	anchor := strings.Fields(top)[0]
	after := elRenderedInPane(dialog, 64, 16)
	area, _ = dialog.messageArea()
	if !strings.Contains(elBufferRow(after, area.Min.Y, area.Min.X, area.Max.X), anchor) {
		t.Fatalf("anchor %q moved off the top row", anchor)
	}
	dialog.scrollMessage(int(^uint(0) >> 1))
	last := elBufferText(elRenderedInPane(dialog, 48, 16))
	if !strings.Contains(last, "Final authored line") || !strings.Contains(last, "Answer & Next") {
		t.Fatalf("the end of the question is hidden:\n%s", last)
	}
}

func TestWarningButtonsDoNotEditTheCustomAnswerBehindThem(t *testing.T) {
	dialog := newElicitationDialog(elPairedRequest(2, false))
	elKey(dialog, tea.KeyRight, tea.ModAlt)
	elKey(dialog, tea.KeyEnd, 0)
	dialog.paste("custom answer")
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0), nil)
	if !dialog.confirmingUnanswered {
		t.Fatal("the warning is not shown")
	}
	dialog.paste("accidental paste")
	elKey(dialog, 'x', 0)
	elBackTab(dialog)
	elAssertResponse(t, elKey(dialog, ' ', 0),
		elAccept(map[string]core.ElicitationValue{"question_1__other": core.ElicitationValueString("custom answer")}))
}

func TestSimultaneousSurfacesKeepHitboxesCaptureAndScrollIndependent(t *testing.T) {
	defaultFalse := false
	request := elRequest(core.ElicitationFieldKindBoolean{Default: &defaultFalse}, true)
	lines := make([]string, 30)
	for i := range lines {
		lines[i] = fmt.Sprintf("Question line %d", i)
	}
	request.Fields[0].Title = strings.Join(lines, "\n")
	dialog := newElicitationDialog(request)
	buffer := uv.NewBuffer(110, 44)
	frame := NewFrame(buffer)
	var notificationField, conversationField uv.Rectangle
	// Both presentations are rendered in the same frame, as in the
	// dashboard.
	for _, c := range []struct {
		surface ElicitationSurface
		bounds  uv.Rectangle
	}{
		{ElicitationSurfaceNotification, uv.Rect(0, 0, 70, 18)},
		{ElicitationSurfaceConversation, uv.Rect(0, 22, 110, 22)},
	} {
		dialog.selectSurface(c.surface)
		surfaces := NewFrameSurfaces()
		renderElicitationIn(frame, dialog, &surfaces, c.bounds, true)
		field := surfaces.Surface(SurfaceIdModalBody{}).Rect
		if c.surface == ElicitationSurfaceNotification {
			notificationField = field
		} else {
			conversationField = field
		}
	}
	dialog.selectSurface(ElicitationSurfaceNotification)
	dialog.handleMouse(tea.MouseClickMsg{X: notificationField.Min.X + 1, Y: notificationField.Min.Y, Button: tea.MouseLeft})
	if surface, ok := dialog.pointerSurface(); !ok || surface != ElicitationSurfaceNotification {
		t.Fatalf("pointer surface = %v, %t", surface, ok)
	}
	dialog.selectSurface(ElicitationSurfaceConversation)
	if surface, ok := dialog.pointerSurface(); !ok || surface != ElicitationSurfaceNotification {
		t.Fatalf("pointer surface after switching = %v, %t", surface, ok)
	}
	dialog.selectSurface(ElicitationSurfaceNotification)
	dialog.handleMouse(tea.MouseReleaseMsg{X: notificationField.Min.X + 1, Y: notificationField.Min.Y, Button: tea.MouseLeft})
	if value, ok := dialog.values[0].(*fieldValueBoolean); !ok || !value.Value {
		t.Fatalf("value = %s", elFieldValueDebug(dialog.values[0]))
	}
	dialog.scrollMessage(3)
	notificationScroll := dialog.view.messageScroll
	if notificationScroll <= 0 {
		t.Fatal("the notification did not scroll")
	}
	dialog.selectSurface(ElicitationSurfaceConversation)
	if dialog.view.messageScroll != 0 {
		t.Fatalf("conversation scroll = %d", dialog.view.messageScroll)
	}
	if !dialog.componentHandlesMouseAt(conversationField.Min.X+1, conversationField.Min.Y) {
		t.Fatal("the conversation field does not take the click")
	}
	elClickCellAt(dialog, conversationField.Min.X+1, conversationField.Min.Y, time.Now())
	if value, ok := dialog.values[0].(*fieldValueBoolean); !ok || value.Value {
		t.Fatalf("value = %s", elFieldValueDebug(dialog.values[0]))
	}
	dialog.selectSurface(ElicitationSurfaceNotification)
	if dialog.view.messageScroll != notificationScroll {
		t.Fatalf("notification scroll = %d, want %d", dialog.view.messageScroll, notificationScroll)
	}
	// Hard-won: a882023b6: a notification-only draft restored its scroll into the conversation slot.
	// Hard-won: 4c23909e0: the conversation's later render overwrote the notification viewport in the saved draft.
	dialog.selectSurface(ElicitationSurfaceConversation)
	stored, err := json.Marshal(dialog.draft())
	if err != nil {
		t.Fatal(err)
	}
	var draft ElicitationDraft
	if err := json.Unmarshal(stored, &draft); err != nil {
		t.Fatal(err)
	}
	restored := elicitationDialogFromDraft(*dialog.request(), draft)
	if restored == nil {
		t.Fatal("the draft does not restore")
	}
	restored.selectSurface(ElicitationSurfaceNotification)
	if restored.view.messageScroll != notificationScroll {
		t.Fatalf("restored notification scroll = %d, want %d", restored.view.messageScroll, notificationScroll)
	}
	restored.selectSurface(ElicitationSurfaceConversation)
	if restored.view.messageScroll != 0 {
		t.Fatalf("restored conversation scroll = %d", restored.view.messageScroll)
	}
	elAssertResponse(t, elKey(dialog, tea.KeyEnter, 0),
		elAccept(map[string]core.ElicitationValue{"question_0": core.ElicitationValueBoolean(false)}))
}
