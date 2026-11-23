// Port of the tests in mj-tui/src/dialogs/container.rs.

package tui

import (
	"errors"
	"reflect"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func dialogsOpenContainerEditor(t *testing.T) *DashboardState {
	t.Helper()
	dashboard := dashboardWithSession(runningSession())
	dashboard.beginContainerEdit()
	if _, ok := dashboard.mode.(ModeEditContainer); !ok {
		t.Fatalf("expected the container editor, got %T", dashboard.mode)
	}
	return dashboard
}

func dialogsContainerDraw(dashboard *DashboardState, width, height int) []string {
	terminal := newDialogsTerminal(width, height)
	terminal.draw(dashboard)
	return bufferLines(terminal.buffer)
}

func dialogsContainsLine(lines []string, text string) bool {
	return slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, text) })
}

func TestContainerPathApplyWaitsForResolutionAndPreservesFailure(t *testing.T) {
	dashboard := dialogsOpenContainerEditor(t)
	editor := containerEditor(t, dashboard)
	editor.source.SetValue("~/cache")
	editor.destination.SetValue("/mnt/cache")
	editor.form.Focus(ContainerEditFocusSource)
	original := len(editor.mounts)
	if _, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionResolveContainerPath); !ok {
		t.Fatal("a home-relative source is resolved on the target first")
	}
	context := dashboard.PathInputContext()
	dashboard.ContainerPathResolved(context, "", errors.New("SSH failed"))
	editor = containerEditor(t, dashboard)
	if source := editor.source.Value(); source != "~/cache" {
		t.Fatalf("source %q", source)
	}
	if mounts := len(editor.mounts); mounts != original {
		t.Fatalf("%d mounts", mounts)
	}
	dashboard.ContainerPathResolved(context, "/remote/cache", nil)
	editor = containerEditor(t, dashboard)
	if source := editor.mounts[len(editor.mounts)-1].Source; source != "/remote/cache" {
		t.Fatalf("attached %q", source)
	}
	if !editor.source.IsEmpty() {
		t.Fatalf("source %q", editor.source.Value())
	}
}

func TestSaveClickRequiresReleaseInsideAndFocusesOnFirstPress(t *testing.T) {
	dashboard := dialogsOpenContainerEditor(t)
	dashboard.HandleKey(key('4'))
	containerEditor(t, dashboard).memory.SetValue("8")
	lines := dialogsContainerDraw(dashboard, 100, 40)
	save := point(lines, "  Save  ")
	if action := dashboard.HandleMouse(mouseAt(dialogsLeftDown, save)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("press: %#v", action)
	}
	if focused := containerEditor(t, dashboard).focused(); focused != ContainerEditFocusSave {
		t.Fatalf("editor closed on press or focused %v", focused)
	}
	if !dashboard.ComponentHandlesMouse(mouseAt(dialogsLeftUp, uv.Pos(0, 0))) {
		t.Fatal("the pressed button owns the release")
	}
	if action := dashboard.HandleMouse(mouseAt(dialogsLeftUp, uv.Pos(0, 0))); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("release outside: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeEditContainer); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
	dashboard.HandleMouse(mouseAt(dialogsLeftDown, save))
	action, ok := dashboard.HandleMouse(mouseAt(dialogsLeftUp, save)).(DashboardActionSaveContainerSettings)
	if !ok {
		t.Fatal("a release inside saves")
	}
	if allocation, ok := action.ResourceAllocation.(core.SessionResourceAllocationContainer); !ok || allocation.Cpus != 4 {
		t.Fatalf("allocation %#v", action.ResourceAllocation)
	}
}

func TestSmallFormScrollsToFieldAndPaletteChordKeepsItsDraft(t *testing.T) {
	dashboard := dialogsOpenContainerEditor(t)
	dashboard.HandleKey(key(tea.KeyTab))
	dashboard.HandleKey(key(tea.KeyTab))
	if focused := containerEditor(t, dashboard).focused(); focused != ContainerEditFocusSource {
		t.Fatalf("focused %v", focused)
	}
	dashboard.HandlePaste("/srv/資料 with spaces")
	if lines := dialogsContainerDraw(dashboard, 80, 18); !dialogsContainsLine(lines, "Attach host directory:") {
		t.Fatal(strings.Join(lines, "\n"))
	}
	route(dashboard, []tea.KeyMsg{prefixKey(), key('?')})
	mode, ok := dashboard.mode.(ModeEditContainer)
	if !ok {
		t.Fatalf("help did not restore editor, got %T", dashboard.mode)
	}
	if focused := mode.focused(); focused != ContainerEditFocusSource {
		t.Fatalf("focused %v", focused)
	}
	if source := mode.source.Value(); source != "/srv/資料 with spaces" {
		t.Fatalf("source %q", source)
	}
	if lines := dialogsContainerDraw(dashboard, 40, 10); !dialogsContainsLine(lines, "Terminal too small") {
		t.Fatal(strings.Join(lines, "\n"))
	}
	if lines := dialogsContainerDraw(dashboard, 80, 18); !dialogsContainsLine(lines, "  Save  ") {
		t.Fatal(strings.Join(lines, "\n"))
	}
}

func TestTooSmallFrameRemovesStaleButtonHitboxes(t *testing.T) {
	dashboard := dialogsOpenContainerEditor(t)
	save := point(dialogsContainerDraw(dashboard, 100, 40), "  Save  ")
	if lines := dialogsContainerDraw(dashboard, 20, 8); !dialogsContainsLine(lines, "Terminal too small") {
		t.Fatal(strings.Join(lines, "\n"))
	}
	down := mouseAt(dialogsLeftDown, save)
	if dashboard.ComponentHandlesMouse(down) {
		t.Fatal("a hidden button keeps no hitbox")
	}
	if action := dashboard.HandleMouse(down); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("press: %#v", action)
	}
	if action := dashboard.HandleMouse(mouseAt(dialogsLeftUp, save)); !reflect.DeepEqual(action, DashboardActionNone{}) {
		t.Fatalf("release: %#v", action)
	}
	if _, ok := dashboard.mode.(ModeEditContainer); !ok {
		t.Fatalf("mode %T", dashboard.mode)
	}
}

// The attached directory is a path on the host that runs the session's
// container, so its completions come from that target.
func TestContainerEditorCompletesOnTheSessionTarget(t *testing.T) {
	dashboard := dialogsOpenContainerEditor(t)
	editor := containerEditor(t, dashboard)
	editor.form.Focus(ContainerEditFocusSource)
	editor.source.SetValue("/opt/c")
	editor.prepare()
	complete := dialogsControlKey(' ')

	if action := dashboard.HandleKey(complete); !reflect.DeepEqual(action, DashboardActionCompletePath{
		Host:   core.CompletionHostTarget{ID: "podman"},
		Kind:   core.CompletionKindDirectories,
		Prefix: "/opt/c",
	}) {
		t.Fatalf("complete: %#v", action)
	}
	context := dashboard.PathInputContext()
	dashboard.ApplyPathCompletions(context, "/opt/c", core.PathCompletion{
		Candidates: []string{"/opt/cache/", "/opt/config/"},
	})
	if !containerEditor(t, dashboard).source.IsCompleting() {
		t.Fatal("the completions open")
	}

	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyEnter))
	editor = containerEditor(t, dashboard)
	if source := editor.source.Value(); source != "/opt/config/" {
		t.Fatalf("source %q", source)
	}
	if editor.source.IsCompleting() {
		t.Fatal("accepting closes the completions")
	}
	// Accepting a candidate is not attaching it: the editor keeps the entry
	// open so a destination can still be chosen.
	if len(editor.mounts) != 0 {
		t.Fatalf("%d mounts", len(editor.mounts))
	}
}
