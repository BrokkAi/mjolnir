// Port of mj-chat/src/path_input.rs (tests).

package chat

import (
	"fmt"
	"strconv"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func pathReply(candidates []string, insert *string, truncated bool) core.PathCompletion {
	return core.PathCompletion{Candidates: candidates, Insert: insert, Truncated: truncated}
}

func pathInsert(text string) *string { return &text }

// pathOptionDebug is Rust's Debug for an Option<&str>.
func pathOptionDebug(value string, ok bool) string {
	if !ok {
		return "None"
	}
	return "Some(" + strconv.Quote(value) + ")"
}

// pathEditOutcomeDebug is Rust's Debug for an EditOutcome.
func pathEditOutcomeDebug(outcome EditOutcome) string {
	switch outcome {
	case EditOutcomeHandled:
		return "Handled"
	case EditOutcomeChanged:
		return "Changed"
	default:
		return "Unhandled"
	}
}

// pathControlKindDebug is Rust's Debug for the ControlKind a path field
// declares.
func pathControlKindDebug(kind ControlKind) string {
	field := kind.(ControlKindPathField)
	return fmt.Sprintf("PathField { len: %d, selected: %d, expanded: %t }", field.Len, field.Selected, field.Expanded)
}

func pathFieldScreen(input *PathInput) string {
	buffer := uv.NewBuffer(40, 14)
	form := NewForm[uint8]()
	form.Declare(1, input.ControlKind())
	form.EndFrame(1)
	frame := NewFrame(buffer)
	form.BeginFrame()
	PathFieldRender(frame, uv.Rect(0, 1, 24, 1), input, &form, 1)
	form.EndFrame(1)
	return strings.Join(BufferLines(buffer), "\n")
}

func appendPathState(output *strings.Builder, label string, input *PathInput) {
	fmt.Fprintf(output, "=== %s (40x14) ===\n", label)
	pending, ok := input.CompletionPending()
	fmt.Fprintf(output, "value: %s; cursor: %d; control: %s; pending: %s; truncated: %t\n",
		strconv.Quote(input.Value()),
		input.Cursor(),
		pathControlKindDebug(input.ControlKind()),
		pathOptionDebug(pending, ok),
		input.CompletionTruncated(),
	)
	output.WriteString(pathFieldScreen(input))
	output.WriteString("\n")
}

func TestGoldenPathField(t *testing.T) {
	var output strings.Builder
	input := NewPathInput()
	edit := PathFieldApply(&input, FieldEditPaste{Text: "~/.codex4"})
	fmt.Fprintf(&output, "=== edited draft (40x14) ===\nedit: %s\n", pathEditOutcomeDebug(edit))
	appendPathState(&output, "before resolution", &input)
	home := "/workspace/user"
	resolved, err := input.Resolve(&home)
	if err != nil {
		t.Fatalf("resolve draft: %v", err)
	}
	fmt.Fprintf(&output, "resolved with supplied home: %s\n", strconv.Quote(resolved))
	_, err = input.Resolve(nil)
	fmt.Fprintf(&output, "missing home rejected: %t\n", err != nil)
	appendPathState(&output, "draft after resolution attempts", &input)
	output.WriteString("=== end of rendered states ===\n")
	core.AssertGolden(t, "testdata", "path-field", output.String())
}

func TestReplyForStaleTextIsIgnored(t *testing.T) {
	input := PathInputFromValue("~/pr")
	if prefix, ok := input.RequestCompletion(); !ok || prefix != "~/pr" {
		t.Fatalf("RequestCompletion = %q, %t", prefix, ok)
	}
	input.SetValue("~/proj")
	if input.ApplyCompletion("~/pr", pathReply([]string{"~/pr1/", "~/pr2/"}, nil, false)) {
		t.Fatal("a reply for stale text was applied")
	}
	if input.Value() != "~/proj" {
		t.Fatalf("value = %q", input.Value())
	}
	if input.IsCompleting() {
		t.Fatal("a stale reply opened the popup")
	}
	if pending, ok := input.CompletionPending(); ok {
		t.Fatalf("pending = %q", pending)
	}
}

func TestGoldenPathCompletion(t *testing.T) {
	var output strings.Builder

	input := PathInputFromValue("~/pr")
	prefix, requested := input.RequestCompletion()
	applied := input.ApplyCompletion("~/pr", pathReply([]string{"~/projects/"}, pathInsert("~/projects/"), false))
	fmt.Fprintf(&output, "single completion request: %s; reply applied: %t\n", pathOptionDebug(prefix, requested), applied)
	appendPathState(&output, "single match inserts without popup", &input)

	input = PathInputFromValue("~/p")
	prefix, requested = input.RequestCompletion()
	applied = input.ApplyCompletion("~/p", pathReply([]string{"~/projects/", "~/provision/"}, pathInsert("~/pro"), false))
	fmt.Fprintf(&output, "common-prefix request: %s; reply applied: %t\n", pathOptionDebug(prefix, requested), applied)
	appendPathState(&output, "common prefix and two candidates", &input)

	input = PathInputFromValue("~/p")
	input.RequestCompletion()
	input.ApplyCompletion("~/p", pathReply([]string{"~/projects/", "~/provision/"}, nil, true))
	appendPathState(&output, "truncated candidates before edit", &input)
	outcome := PathFieldApply(&input, FieldEditKey{Key: tea.KeyPressMsg{Code: 'r', Text: "r"}})
	fmt.Fprintf(&output, "edit outcome: %s\n", pathEditOutcomeDebug(outcome))
	appendPathState(&output, "popup dismissed after edit", &input)

	input = PathInputFromValue("~/p")
	input.RequestCompletion()
	input.ApplyCompletion("~/p", pathReply([]string{"~/projects/", "~/provision/"}, nil, false))
	input.SelectCompletion(9)
	appendPathState(&output, "selected candidate is clamped", &input)
	accepted := input.AcceptCompletion()
	acceptedAgain := input.AcceptCompletion()
	fmt.Fprintf(&output, "accepted: %t; accepting closed popup: %t\n", accepted, acceptedAgain)
	appendPathState(&output, "accepted candidate", &input)
	output.WriteString("=== end of rendered states ===\n")
	core.AssertGolden(t, "testdata", "path-completion", output.String())
}

func TestRequestIsNotRepeatedWhilePending(t *testing.T) {
	input := NewPathInput()
	if prefix, ok := input.RequestCompletion(); ok {
		t.Fatalf("an empty field asked for %q", prefix)
	}
	input.SetValue("~/pr")
	if prefix, ok := input.RequestCompletion(); !ok || prefix != "~/pr" {
		t.Fatalf("RequestCompletion = %q, %t", prefix, ok)
	}
	if prefix, ok := input.RequestCompletion(); ok {
		t.Fatalf("a pending request was repeated for %q", prefix)
	}
	input.SetValue("~/prq")
	if prefix, ok := input.RequestCompletion(); !ok || prefix != "~/prq" {
		t.Fatalf("RequestCompletion = %q, %t", prefix, ok)
	}
	input.DismissCompletion()
	if pending, ok := input.CompletionPending(); ok {
		t.Fatalf("pending = %q", pending)
	}
	if prefix, ok := input.RequestCompletion(); !ok || prefix != "~/prq" {
		t.Fatalf("RequestCompletion = %q, %t", prefix, ok)
	}
}
