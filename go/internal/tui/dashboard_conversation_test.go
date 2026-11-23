// Port of mj-tui/src/dashboard_conversation.rs tests (pin_tests).

package tui

import (
	"fmt"
	"maps"
	"reflect"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

func pinDashboard() *DashboardState {
	dashboard := dashboardWithSession(runningSession())
	for i := 2; i <= 6; i++ {
		session := runningSession()
		session.ID = fmt.Sprintf("session-%d", i)
		dashboard.state.Mut().Sessions.Insert(session.ID, session)
	}
	publishFixtureVerdicts(dashboard)
	area := uv.Rect(0, 0, 160, 60)
	dashboard.conversationAreaField = &area
	current := "session-1"
	dashboard.SetCurrentSession(&current)
	return dashboard
}

func pinRequireSelected(t *testing.T, dashboard *DashboardState, want string) {
	t.Helper()
	got, ok := dashboard.SelectedSessionID()
	if want == "" {
		if ok {
			t.Fatalf("selected = %q, want none", got)
		}
		return
	}
	if !ok || got != want {
		t.Fatalf("selected = %q, %v; want %q", got, ok, want)
	}
}

func pinRequireCurrent(t *testing.T, dashboard *DashboardState, want string) {
	t.Helper()
	got, ok := dashboard.CurrentSessionID()
	if !ok || got != want {
		t.Fatalf("current = %q, %v; want %q", got, ok, want)
	}
}

func pinRequirePaneSession(t *testing.T, dashboard *DashboardState, pane PaneId, want string) {
	t.Helper()
	got, ok := dashboard.PaneSession(pane)
	if want == "" {
		if ok {
			t.Fatalf("pane %d shows %q, want empty", pane, got)
		}
		return
	}
	if !ok || got != want {
		t.Fatalf("pane %d shows %q, %v; want %q", pane, got, ok, want)
	}
}

func pinRequirePin(t *testing.T, dashboard *DashboardState, session string, want uint32, pinned bool) {
	t.Helper()
	got, ok := dashboard.PinID(session)
	if ok != pinned || (pinned && got != want) {
		t.Fatalf("pin of %s = %d, %v; want %d, %v", session, got, ok, want, pinned)
	}
}

func pinMustSplit(t *testing.T, dashboard *DashboardState, direction chat.Direction) PaneId {
	t.Helper()
	pane, ok := dashboard.SplitFocusedPane(direction, nil)
	if !ok {
		t.Fatal("the split succeeds")
	}
	return pane
}

func TestAnEmptyPaneSelectsTheLastRowOnUpAndCannotCommandAnotherPane(t *testing.T) {
	d := pinDashboard()
	pinMustSplit(t, d, chat.DirectionHorizontal)
	pinRequireSelected(t, d, "")
	if id, ok := d.commandSessionID(); ok {
		t.Fatalf("command session = %q", id)
	}
	d.HandleKey(key(tea.KeyUp))
	pinRequireSelected(t, d, "session-6")
	pinRequireCurrent(t, d, "session-6")
}

func TestFiltersKeepTheActiveRowUntilNavigationSelectsAMatch(t *testing.T) {
	d := pinDashboard()
	d.FocusSessions()
	d.HandleKey(key('/'))
	for _, ch := range "session-2" {
		d.HandleKey(key(ch))
	}
	pinRequireSelected(t, d, "session-1")
	pinRequireCurrent(t, d, "session-1")
	var ids []string
	for _, session := range d.orderedSessions() {
		ids = append(ids, session.ID)
	}
	if !slices.Equal(ids, []string{"session-1", "session-2"}) {
		t.Fatalf("ordered = %q", ids)
	}
	rows := drawn(d, 140, 40)
	if !slices.ContainsFunc(rows, func(row string) bool { return strings.Contains(row, "Outside filter") }) {
		t.Fatalf("%q", rows)
	}
	d.HandleKey(key(tea.KeyEnter))
	d.HandleKey(key(tea.KeyDown))
	pinRequireSelected(t, d, "session-2")
	pinRequireCurrent(t, d, "session-2")
	if len(d.orderedSessions()) != 1 {
		t.Fatalf("ordered = %d", len(d.orderedSessions()))
	}
	if slices.ContainsFunc(drawn(d, 140, 40), func(row string) bool { return strings.Contains(row, "Outside filter") }) {
		t.Fatal("the outside-filter row leaves once a match is selected")
	}
}

func TestSplittingBuildsAGridWithOneBrowseAndStablePins(t *testing.T) {
	d := pinDashboard()
	left := d.BrowsePane()
	right := pinMustSplit(t, d, chat.DirectionHorizontal)
	pinRequirePin(t, d, "session-1", 0, true)
	session2 := "session-2"
	d.SetPaneSession(right, &session2)
	d.FocusPane(left)
	lowerLeft := pinMustSplit(t, d, chat.DirectionVertical)
	pinRequirePin(t, d, "session-2", 1, true)
	session3 := "session-3"
	d.SetPaneSession(lowerLeft, &session3)
	d.FocusPane(right)
	lowerRight := pinMustSplit(t, d, chat.DirectionVertical)
	if d.BrowsePane() != lowerRight {
		t.Fatalf("browse = %d", d.BrowsePane())
	}
	pinRequirePin(t, d, "session-3", 2, true)
	pinRequirePaneSession(t, d, lowerRight, "")
	pins := maps.Clone(d.pinIDs)
	d.SwapConversationPanes(left, lowerRight)
	if d.BrowsePane() != lowerRight {
		t.Fatalf("browse = %d", d.BrowsePane())
	}
	if !maps.Equal(d.pinIDs, pins) {
		t.Fatalf("pins = %v, want %v", d.pinIDs, pins)
	}
	d.SelectActiveSession("session-4")
	pinRequireSelected(t, d, "session-4")
	d.FocusPane(right)
	pinRequireSelected(t, d, "session-2")
	pinRequirePaneSession(t, d, lowerRight, "session-4")
	pinRequirePaneSession(t, d, left, "session-1")
	pinRequirePaneSession(t, d, right, "session-2")
	pinRequirePaneSession(t, d, lowerLeft, "session-3")
	layout := d.ExportConversationLayout()
	if err := layout.Validate(); err != nil {
		t.Fatal(err)
	}
}

func TestARefusedSplitPreservesFocusZoomAndRoles(t *testing.T) {
	d := pinDashboard()
	pinMustSplit(t, d, chat.DirectionHorizontal)
	d.conversationZoomed = true
	small := uv.Rect(0, 0, 40, 8)
	d.conversationAreaField = &small
	before := d.ExportConversationLayout()
	if _, ok := d.SplitFocusedPane(chat.DirectionHorizontal, nil); ok {
		t.Fatal("the split is refused")
	}
	if after := d.ExportConversationLayout(); !reflect.DeepEqual(after, before) {
		t.Fatalf("layout = %#v, want %#v", after, before)
	}
	if !d.conversationZoomed {
		t.Fatal("the zoom survives a refused split")
	}
}

func TestRestoreKeepsBrowseSeparateFromFocusAndLegacyLayoutsConvert(t *testing.T) {
	d := pinDashboard()
	pin := d.BrowsePane()
	browse := pinMustSplit(t, d, chat.DirectionHorizontal)
	session2 := "session-2"
	d.SetPaneSession(browse, &session2)
	d.FocusPane(pin)
	saved := d.ExportConversationLayout()
	d.restoreConversationLayout(&saved)
	if d.FocusedPane() != pin {
		t.Fatalf("focused = %d", d.FocusedPane())
	}
	if d.BrowsePane() != browse {
		t.Fatalf("browse = %d", d.BrowsePane())
	}
	pinRequireSelected(t, d, "session-1")
	pinRequirePin(t, d, "session-1", 0, true)
	legacy := saved
	legacy.Browse = nil
	legacy.Pins = map[string]uint32{}
	d.restoreConversationLayout(&legacy)
	if d.BrowsePane() != pin {
		t.Fatalf("browse = %d", d.BrowsePane())
	}
	pinRequirePin(t, d, "session-2", 0, true)
}

func TestRefreshRemovalClearsSelectionAndMissingPinsLeaveSlots(t *testing.T) {
	d := pinDashboard()
	pin := d.BrowsePane()
	pinMustSplit(t, d, chat.DirectionHorizontal)
	d.SelectActiveSession("session-2")
	state := *d.state.Get()
	state.Sessions.Remove("session-1")
	state.Sessions.Remove("session-2")
	d.SetState(state)
	pinRequireSelected(t, d, "")
	pinRequirePaneSession(t, d, pin, "")
	if d.navigation.layout().PaneCount() != 2 {
		t.Fatalf("pane count = %d", d.navigation.layout().PaneCount())
	}
	if len(d.pinIDs) != 0 {
		t.Fatalf("pins = %v", d.pinIDs)
	}
	d.FocusSessions()
	d.HandleKey(key(tea.KeyDown))
	selected, selectedOK := d.SelectedSessionID()
	current, currentOK := d.CurrentSessionID()
	if selected != current || selectedOK != currentOK {
		t.Fatalf("selected = %q, %v; current = %q, %v", selected, selectedOK, current, currentOK)
	}
}

func TestClearingAPinKeepsTheSlotAndDoesNotRenumberOtherPins(t *testing.T) {
	d := pinDashboard()
	first := d.BrowsePane()
	second := pinMustSplit(t, d, chat.DirectionHorizontal)
	session2 := "session-2"
	d.SetPaneSession(second, &session2)
	pinMustSplit(t, d, chat.DirectionVertical)
	before := d.navigation.layout().PaneCount()
	d.SetPaneSession(first, nil)
	if d.navigation.layout().PaneCount() != before {
		t.Fatalf("pane count = %d", d.navigation.layout().PaneCount())
	}
	pinRequirePin(t, d, "session-2", 1, true)
	pinRequirePin(t, d, "session-1", 0, false)
	browse := d.BrowsePane()
	if session, ok := d.ClosePane(browse); ok {
		t.Fatalf("closing browse returned %q", session)
	}
	if d.navigation.layout().PaneCount() != before {
		t.Fatalf("pane count = %d", d.navigation.layout().PaneCount())
	}
	d.ClosePane(first)
	if d.navigation.layout().PaneCount() != before-1 {
		t.Fatalf("pane count = %d", d.navigation.layout().PaneCount())
	}
}
