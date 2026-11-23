// Tests ported from mj-chat/src/components/dialog.rs.

package chat

import (
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
)

func dialogEditor() Dialog[uint8] {
	dialog := NewDialog[uint8]()
	dialog.Declare(1, ControlKindTextField{})
	dialog.DeclareActions([]DialogAction[uint8]{
		{ID: 2, Label: "Cancel", Role: ActionRoleCancel, Enabled: true},
		{ID: 3, Label: "Save", Role: ActionRolePrimary, Enabled: true},
	})
	dialog.SetSubmit(1, 3)
	dialog.EndFrame(1)
	dialog.TrackDraft([]string{"original"})
	return dialog
}

func requireFocused[K comparable](t *testing.T, form *Form[K], want K) {
	t.Helper()
	if got, ok := form.Focused(); !ok || got != want {
		t.Fatalf("focused = %v (%t), want %v", got, ok, want)
	}
}

func requireAction[K comparable](t *testing.T, got Interaction[K], want Interaction[K]) {
	t.Helper()
	if !interactionsEqual(got, want) {
		t.Fatalf("action = %s, want %s", debugInteraction(got), debugInteraction(want))
	}
}

func TestDismissalProtectsDraftAndRestoresFieldFocus(t *testing.T) {
	dialog := dialogEditor()
	dialog.TrackDraft([]string{"edited"})
	requireAction(t, dialog.Handle(key(tea.KeyEscape)).Action, nil)
	if !dialog.ConfirmationOpen() {
		t.Fatal("escape on a dirty draft does not ask")
	}
	requireAction(t, dialog.Handle(key(tea.KeyEnter)).Action, nil)
	if dialog.ConfirmationOpen() {
		t.Fatal("keep editing leaves the prompt open")
	}
	requireFocused(t, &dialog.Form, 1)
	dialog.Handle(key(tea.KeyEscape))
	dialog.Handle(key(tea.KeyRight))
	requireAction[uint8](t, dialog.Handle(key(tea.KeyEnter)).Action, InteractionCancel[uint8]{})
}

func TestRevertingChangesClosesWithoutAPromptAndCancelButtonUsesSameGuard(t *testing.T) {
	dialog := dialogEditor()
	dialog.TrackDraft([]string{"edited"})
	dialog.Focus(2)
	dialog.Handle(key(tea.KeyEnter))
	if !dialog.ConfirmationOpen() {
		t.Fatal("cancel on a dirty draft does not ask")
	}
	dialog.Handle(key(tea.KeyEscape))
	dialog.TrackDraft([]string{"original"})
	requireAction[uint8](t, dialog.Handle(key(tea.KeyEnter)).Action, InteractionActivate[uint8]{ID: 2})
}

func TestChildDismissalDoesNotDiscardTheParentDraft(t *testing.T) {
	dialog := dialogEditor()
	dialog.Declare(4, ControlKindButton{})
	dialog.SetDismissActions([]uint8{2, 4})
	child := uint8(4)
	dialog.SetEscapeAction(&child)
	dialog.SetDismissalScope(4, "child")
	dialog.TrackDraftPart("child", []string{"unchanged"})
	dialog.TrackDraft([]string{"edited parent"})
	requireAction[uint8](t, dialog.Handle(key(tea.KeyEscape)).Action, InteractionActivate[uint8]{ID: 4})
	dialog.SetEscapeAction(nil)
	dialog.Handle(key(tea.KeyEscape))
	if !dialog.ConfirmationOpen() {
		t.Fatal("escape without the child scope does not protect the parent draft")
	}
}

func TestDiscardActionsRemainVisibleOnASmallTerminal(t *testing.T) {
	dialog := dialogEditor()
	dialog.TrackDraft([]string{"changed"})
	dialog.Handle(key(tea.KeyEscape))
	terminal := newComponentTerminal(45, 12)
	terminal.draw(func(frame *Frame) {
		surfaces := NewFrameSurfaces()
		dialog.RenderConfirmation(frame, frame.Area(), &surfaces)
	})
	text := terminal.content()
	if !strings.Contains(text, "Keep editing") {
		t.Fatalf("Keep editing missing from %q", text)
	}
	if !strings.Contains(text, "Discard") {
		t.Fatalf("Discard missing from %q", text)
	}
}
