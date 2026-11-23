// Port of mj-tui/src/setup/tests/search.rs.

package tui

import (
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupHasPath is whether paths holds path.
func setupHasPath(paths [][]string, path ...string) bool {
	return slices.ContainsFunc(paths, func(candidate []string) bool { return slices.Equal(candidate, path) })
}

func TestSearchSelectsASwitchWithoutFlippingIt(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupSearch(dashboard, "detailed activity")
	dashboard.HandleKey(key(tea.KeyEnter))
	dialog := setupDialog(t, dashboard)
	if !slices.Equal(dialog.path, []string{"advanced"}) {
		t.Fatalf("path = %v", dialog.path)
	}
	if path, ok := dialog.focusedPath(); !ok || path[len(path)-1] != "detailed_activity_clocks" {
		t.Fatalf("the switch is left selected on its own page: %v", path)
	}
	if dialog.editor != nil {
		t.Fatal("an editor opened")
	}
	if value := setupValuePath(dialog.draft, "advanced", "detailed_activity_clocks"); !setupValueEqual(value, false) {
		t.Fatalf("finding a setting must not change it: %s", setupValueDisplay(value))
	}
}

func TestGlobalSubagentSearchTargetsInlineControlsAndHidesUnavailableEffort(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupSearch(dashboard, "straightforward")
	paths := setupResultPaths(t, dashboard)
	category := []string{"subagents", "straightforward"}
	if !setupHasPath(paths, category...) {
		t.Fatalf("paths = %v", paths)
	}
	if setupHasPath(paths, "subagents", "straightforward", "0", "effort") {
		t.Fatalf("paths = %v", paths)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	if path := setupDialog(t, dashboard).path; !slices.Equal(path, category) {
		t.Fatalf("path = %v", path)
	}

	configured := dashboardWithSession(stoppedSession())
	configured.BeginSetup()
	dialog := setupDialog(t, configured)
	setupValueInsert(&dialog.draft, []string{"subagents"}, "straightforward", setupJSON(t, `[{ "model": "", "effort": null }]`))
	setupSearch(configured, "effort")
	if setupHasPath(setupResultPaths(t, configured), "subagents", "straightforward", "0", "effort") {
		t.Fatal("effort stays hidden while the row has no model")
	}
	configured.HandleKey(key(tea.KeyEscape))

	dialog = setupDialog(t, configured)
	setupValueInsert(&dialog.draft, []string{"subagents"}, "straightforward", setupJSON(t, `[{ "model": "model-x", "effort": null }]`))
	setupSearch(configured, "effort")
	paths = setupResultPaths(t, configured)
	effortIndex := slices.IndexFunc(paths, func(path []string) bool {
		return slices.Equal(path, []string{"subagents", "straightforward", "0", "effort"})
	})
	if effortIndex < 0 {
		t.Fatal("effort appears once model is chosen")
	}
	if setupHasPath(paths, "subagents", "straightforward", "0") {
		t.Fatal("no Item N subpage is indexed")
	}
	for range effortIndex {
		configured.HandleKey(key(tea.KeyDown))
	}
	configured.HandleKey(key(tea.KeyEnter))
	dialog = setupDialog(t, configured)
	if !slices.Equal(dialog.path, category) {
		t.Fatalf("path = %v", dialog.path)
	}
	if path, _ := dialog.focusedPath(); !slices.Equal(path, []string{"subagents", "straightforward", "0", "effort"}) {
		t.Fatalf("focused %v", path)
	}
}

func TestTypingInTheSearchDoesNotReachThePageShortcuts(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("profiles", nil)
	before := len(setupDialog(t, dashboard).keys())
	setupSearch(dashboard, "")
	// On a collection page a bare "a" adds an entry; inside the query it is
	// just a letter.
	dashboard.HandleKey(key('a'))
	if query := setupSearchState(t, dashboard).input.Value(); query != "a" {
		t.Fatalf("query = %q", query)
	}
	dialog := setupDialog(t, dashboard)
	if dialog.editor != nil {
		t.Fatal("no entry was added")
	}
	if after := len(dialog.keys()); after != before {
		t.Fatalf("%d entries, want %d", after, before)
	}
}

func TestCacheSearchAliasesFindEveryMachineAndPreserveTheDraft(t *testing.T) {
	for _, query := range []string{"mbx", "cache", "build cache"} {
		dashboard := dashboardWithSession(stoppedSession())
		builder, err := core.UnmarshalMachineJSON([]byte(`{
			"kind":"ssh", "host":"builder.example.com", "build_cache":{"enabled":true,"max_total_size":"50GB"}
		}`))
		if err != nil {
			t.Fatal(err)
		}
		dashboard.config.Mut().Machines["builder"] = builder
		dashboard.BeginSetup()
		dialog := setupDialog(t, dashboard)
		setupValueInsert(&dialog.draft, []string{"phone"}, "enabled", false)
		setupSearch(dashboard, query)
		paths := setupResultPaths(t, dashboard)
		for _, machine := range []string{"local", "builder"} {
			if !setupHasPath(paths, "machines", machine, "build_cache") {
				t.Fatalf("%s: %v", query, paths)
			}
		}
		if setupHasPath(paths, "build_cache") {
			t.Fatalf("%s: %v", query, paths)
		}
		search := setupSearchState(t, dashboard)
		index := slices.IndexFunc(search.matches, func(index int) bool {
			return slices.Equal(search.entries[index].path, []string{"machines", "builder", "build_cache"})
		})
		if index < 0 {
			t.Fatalf("%s: builder's cache is not found", query)
		}
		entry := search.entries[search.matches[index]]
		if entry.trail != "Machines › builder" || entry.value != "Enabled · 50 GB budget  ›" {
			t.Fatalf("trail %q, value %q", entry.trail, entry.value)
		}
		search.selected = index
		dashboard.HandleKey(key(tea.KeyEnter))
		dialog = setupDialog(t, dashboard)
		if !slices.Equal(dialog.path, []string{"machines", "builder", "build_cache"}) {
			t.Fatalf("path = %v", dialog.path)
		}
		if enabled := setupValuePath(dialog.draft, "phone", "enabled"); !setupValueEqual(enabled, false) {
			t.Fatalf("phone.enabled = %s", setupValueDisplay(enabled))
		}
		if !dialog.isDirty() {
			t.Fatal("the draft lost its edit")
		}
	}
}

func TestSearchFocusesNullableDropdownsWithoutOpeningOrChangingThem(t *testing.T) {
	for _, test := range []struct {
		query string
		path  []string
	}{
		{"theme", []string{"interface", "theme"}},
		{"build caches enabled", []string{"machines", "local", "build_cache", "enabled"}},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		dashboard.BeginSetup()
		before := setupValueClone(setupDialog(t, dashboard).draft)
		setupSearch(dashboard, test.query)
		paths := setupResultPaths(t, dashboard)
		target := slices.IndexFunc(paths, func(candidate []string) bool { return slices.Equal(candidate, test.path) })
		if target < 0 {
			t.Fatalf("%s: %v", test.query, paths)
		}
		for range target {
			dashboard.HandleKey(key(tea.KeyDown))
		}
		dashboard.HandleKey(key(tea.KeyEnter))
		dialog := setupDialog(t, dashboard)
		if path, _ := dialog.focusedPath(); !slices.Equal(path, test.path) {
			t.Fatalf("focused %v", path)
		}
		if _, open := dialog.combo.OpenID(); open {
			t.Fatal("search opened the dropdown")
		}
		if !setupValueEqual(dialog.draft, before) {
			t.Fatal("search changed the draft")
		}
	}
}

// Hard-won: Settings live test: the first search-result click only selected its row.
func TestSearchResultsOpenOnOneClickAcrossTheLabelTrailAndValue(t *testing.T) {
	for _, test := range []struct {
		query, label, trail string
		expected            []string
		page                bool
	}{
		{"Ring the terminal bell", "Ring the terminal bell", "Notifications", []string{"notify", "bell"}, false},
		{"Theme", "Theme", "Interface", []string{"interface", "theme"}, false},
		{"Interface", "Interface", "", []string{"interface"}, true},
		{"Enabled", "Enabled", "codex-1", []string{"profiles", "codex-1", "enabled"}, false},
	} {
		// Each position starts with a fresh dialog and click history. A
		// second click elsewhere on the same row must not be required for
		// activation.
		for _, offset := range []int{0, 8, 40, 70, 90} {
			dashboard := dashboardWithSession(stoppedSession())
			dashboard.BeginSetup()
			before := setupValueClone(setupDialog(t, dashboard).draft)
			setupSearch(dashboard, test.query)
			lines := drawn(dashboard, 140, 42)
			row := -1
			for index := len(lines) - 1; index >= 0; index-- {
				if strings.Contains(lines[index], test.label) && strings.Contains(lines[index], test.trail) {
					row = index
					break
				}
			}
			if row < 0 {
				t.Fatalf("missing %q result: %q", test.label, lines)
			}
			column := cellColumn(lines[row], test.label) + offset
			setupClickAt(dashboard, uv.Pos(column, row), 140, 42)
			dialog := setupDialog(t, dashboard)
			if dialog.search != nil {
				t.Fatalf("%q at offset %d must open on its first click", test.query, offset)
			}
			if test.page {
				if !slices.Equal(dialog.path, test.expected) {
					t.Fatalf("path = %v", dialog.path)
				}
			} else if path, _ := dialog.focusedPath(); !slices.Equal(path, test.expected) {
				t.Fatalf("a click must open the clicked result, including a result not previously selected: %v", path)
			}
			if _, open := dialog.combo.OpenID(); open {
				t.Fatal("search must only focus dropdowns")
			}
			if !setupValueEqual(dialog.draft, before) {
				t.Fatal("search must not toggle or change a value")
			}
		}
	}
}
