// Port of mj-tui/src/setup/tests/rendering.rs.

package tui

import (
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupRow is the first drawn row containing text.
func setupRow(t testing.TB, lines []string, text string) string {
	t.Helper()
	index := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, text) })
	if index < 0 {
		t.Fatalf("missing %q: %q", text, lines)
	}
	return lines[index]
}

// Every on/off setting uses the same checkbox, whichever section holds it.
// Launch campaign finding C-3.
//
// Hard-won: e03accb6: boolean settings used On/Off text instead of checkbox markers.
func TestEveryBooleanSettingIsDrawnAsACheckbox(t *testing.T) {
	for _, test := range []struct{ section, label string }{
		{"github.watch", "Enabled"},
		{"jev", "Enabled"},
		{"phone", "Enabled"},
		{"phone", "Detect Tailscale"},
		{"notify", "Ring the terminal bell"},
		{"notify", "Show counts in the terminal title"},
		{"advanced", "Detailed activity clocks"},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		dashboard.beginSettingsSection(strings.Split(test.section, ".")[0], nil)
		if test.section == "github.watch" {
			dialog := setupDialog(t, dashboard)
			dialog.path = append(dialog.path, "watch")
		}
		setupDialog(t, dashboard).prepare()
		row := setupRow(t, drawn(dashboard, 140, 40), test.label)
		if !strings.Contains(row, "☒") && !strings.Contains(row, "☐") {
			t.Fatalf("%s › %s is a checkbox: %q", test.section, test.label, row)
		}
		if strings.Contains(row, " On ") || strings.Contains(row, " Off ") {
			t.Fatalf("%s › %s has no On/Off text: %q", test.section, test.label, row)
		}
	}
}

func TestGithubEventsReadAsOffWhileJevIsOff(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.config.Mut().Jev.Enabled = false
	dashboard.BeginSetup()
	row := setupRow(t, drawn(dashboard, 140, 48), "GitHub events")
	if !strings.Contains(row, "Off") || !strings.Contains(row, "Jev") || strings.Contains(row, "On ·") {
		t.Fatalf("%q", row)
	}

	dashboard.beginSettingsSection("github", nil)
	dialog := setupDialog(t, dashboard)
	dialog.path = append(dialog.path, "watch")
	dialog.prepare()
	page := strings.Join(drawn(dashboard, 140, 40), "\n")
	if !strings.Contains(page, "Jev is off") || !strings.Contains(page, "child reports") {
		t.Fatal(page)
	}
	help := client.Help([]string{"github", "watch"})
	for _, expected := range []string{"GitHub activity", "child reports", "tool boundaries", "remain available"} {
		if !strings.Contains(help, expected) {
			t.Fatal(help)
		}
	}

	dashboard = dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	row = setupRow(t, drawn(dashboard, 140, 48), "GitHub events")
	if !strings.Contains(row, "On · GitHub activity") {
		t.Fatalf("%q", row)
	}
}

// The breadcrumb names each page the way the page above it named its row, so
// a machine the user called `local` is not retitled as the repository
// setting that shares the key.
//
// Hard-won: a9226786: a user-chosen machine name collided with a settings key in the breadcrumb.
func TestTheBreadcrumbShowsAUserChosenNameAsTheUserWroteIt(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "machines")
	setupChoose(t, dashboard, "local")
	text := strings.Join(drawn(dashboard, 140, 30), "\n")
	if !strings.Contains(text, "Settings › Machines › local") {
		t.Fatal(text)
	}
	if strings.Contains(text, "Local repository directory") {
		t.Fatalf("the machine's name went through the label table:\n%s", text)
	}
	setupChoose(t, dashboard, "build_cache")
	text = strings.Join(drawn(dashboard, 140, 30), "\n")
	if !strings.Contains(text, "Settings › Machines › local › Build caches") {
		t.Fatalf("a schema key below it still gets its label:\n%s", text)
	}
}

// A-14: with `NO_COLOR` set the screen is monochrome whatever theme is
// configured, so Setup says so instead of naming a theme it is not drawing.
// The configured theme is still named, because it returns once `NO_COLOR`
// is unset.
//
// Hard-won: 584574a5: Setup claimed monochrome mode while the renderer still applied its theme.
func TestSetupReportsMonochromeWhileNoColorOverridesTheTheme(t *testing.T) {
	midnight := core.ThemeChoiceFixed{Value: core.UiThemeMidnight}
	if report := setupThemeReport(midnight, true, nil); report != "Monochrome (NO_COLOR; configured: Midnight)" {
		t.Fatal(report)
	}
	if report := setupThemeReport(midnight, false, nil); report != "Midnight" {
		t.Fatal(report)
	}
	// The override is the one the renderer applies.
	if theme := chat.Resolve(midnight, true, nil).Theme; theme != core.UiThemeMono {
		t.Fatalf("theme = %s", theme)
	}
	// Auto names the palette it drew and what the terminal said.
	background := &chat.Rgb{R: 253, G: 246, B: 227}
	if report := setupThemeReport(core.ThemeChoiceAuto{}, false, background); report != "Auto: Light (terminal background #fdf6e3)" {
		t.Fatal(report)
	}
	if report := setupThemeReport(core.ThemeChoiceAuto{}, true, background); report != "Monochrome (NO_COLOR; configured: Auto)" {
		t.Fatal(report)
	}
}

// An open dropdown shows its value once on the row, in the dropdown.
// Launch campaign finding C-16.
//
// Hard-won: 27dfd170: an open dropdown rendered the selected value twice.
func TestAnOpenDropdownDrawsItsValueOnce(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginSettingsSection("notify", nil)
	setupChoose(t, dashboard, "mode")
	row := setupRow(t, drawn(dashboard, 140, 40), "Notify through")
	if count := strings.Count(row, "terminal"); count != 1 {
		t.Fatal(row)
	}
}

// Every description fits the two rows above a page at the narrowest width
// Settings takes, so none stops mid-sentence. Launch campaign finding C-18.
//
// Hard-won: 524f4307: setting descriptions were clipped by the two-row layout.
func TestEverySettingDescriptionFitsItsTwoRows(t *testing.T) {
	paths := [][]string{
		{"interface", "prefix"},
		{"theme"},
		{"profiles"},
		{"profiles", "p", "home"},
		{"machines"},
		{"targets"},
		{"targets", "t", "machine"},
		{"phone"},
		{"advanced"},
		{"notify"},
		{"notify", "mode"},
		{"notify", "bell"},
		{"notify", "delay_seconds"},
		{"notify", "title"},
		{"advanced", "detailed_activity_clocks"},
		{"advanced", "session_order"},
		{"advanced", "symbols"},
		{"bundles"},
		{"bundles", "b", "repositories"},
		{"review"},
		{"jev"},
		{"sessionwiki"},
		{"sessionwiki", "archive_after_days"},
		{"subagents"},
		{"machines", "m", "build_cache"},
		{"machines", "m", "build_cache", "directory"},
		{"machines", "m", "build_cache", "tools_directory"},
		{"machines", "m", "build_cache", "max_total_size"},
		{"machines", "m", "build_cache", "scheduler"},
		{"machines", "m", "build_cache", "scheduler", "cpus"},
		{"machines", "m", "build_cache", "scheduler", "memory"},
		{"targets", "t", "memory"},
		{"targets", "t", "pull_policy"},
		{"profiles", "p", "context_window_bytes"},
		{"profiles", "p", "guardian_review_model"},
		{},
		{"other"},
	}
	// The dialog is at least 64 columns wide, less its border and margin.
	const width = 60
	var overflowing []string
	for _, path := range paths {
		if chat.NewParagraph(chat.RawText(client.Help(path))).WithWrap(chat.Wrap{Trim: false}).LineCount(width) > 2 {
			overflowing = append(overflowing, strings.Join(path, "."))
		}
	}
	if len(overflowing) > 0 {
		t.Fatalf("%q", overflowing)
	}
}

// A page lists its settings in one fixed order, whichever of them the file
// happens to store and in whatever order. Launch campaign finding C-24.
//
// Hard-won: 9a0d40d3: editing a value changed the settings row order.
func TestAPageListsItsSettingsInAFixedOrder(t *testing.T) {
	order := func(text string, path ...string) []string {
		draft, err := jsonvalue.Parse([]byte(text))
		if err != nil {
			t.Fatal(err)
		}
		root := []string{}
		client.Expand(&draft, &root)
		value, ok := setupLookup(draft, path)
		if !ok {
			t.Fatalf("%v exists", path)
		}
		return client.VisibleKeys(path, value)
	}
	if left, right := order(`{"phone": {"tailscale_detect": false}}`, "phone"), order(`{"phone": {}}`, "phone"); !slices.Equal(left, right) {
		t.Fatalf("%v != %v", left, right)
	}
	if keys := order(`{"machines": {"local": {"kind": "local", "build_cache": {"max_total_size": "20GB", "enabled": false}}}}`,
		"machines", "local", "build_cache"); !slices.Equal(keys, []string{"enabled", "directory", "tools_directory", "max_total_size", "scheduler"}) {
		t.Fatalf("%v", keys)
	}
	if keys := order(`{"machines": {"local": {"kind": "local", "build_cache": {"scheduler": {"memory": "8GiB"}}}}}`,
		"machines", "local", "build_cache", "scheduler"); !slices.Equal(keys, []string{"cpus", "memory"}) {
		t.Fatalf("%v", keys)
	}
	left := order(`{"machines": {"box": {"workspace_prefix": "w", "host": "h", "kind": "ssh"}}}`, "machines", "box")
	right := order(`{"machines": {"box": {"kind": "ssh"}}}`, "machines", "box")
	if !slices.Equal(left, right) {
		t.Fatalf("%v != %v", left, right)
	}
}

// Launch campaign finding A-4: with `symbols = "ascii"`, dialog titles and
// hints kept `·`, popup headers kept `↑/↓`, notices kept `…`, and Settings
// fields kept `▾`. Every page and popup below is drawn with the ASCII set
// and must contain nothing else.
//
// Hard-won: 71a1ad4b: ASCII mode left literal Unicode glyphs in rendered settings.
func TestAsciiSymbolsDrawSettingsAndDialogsWithoutNonAsciiText(t *testing.T) {
	assertASCII := func(context string, lines []string) {
		t.Helper()
		offenders := map[rune]struct{}{}
		for _, line := range lines {
			for _, character := range line {
				if character > 0x7f {
					offenders[character] = struct{}{}
				}
			}
		}
		if len(offenders) > 0 {
			t.Fatalf("%s: non-ASCII %q\n%q", context, offenders, lines)
		}
	}
	asciiDashboard := func() *DashboardState {
		dashboard := dashboardWithSession(stoppedSession())
		configuration := *dashboard.config.Get()
		symbols := core.SymbolSetAscii
		configuration.Advanced.Symbols = &symbols
		dashboard.SetConfig(configuration)
		return dashboard
	}

	dashboard := asciiDashboard()
	dashboard.BeginSetup()
	roots := setupDialog(t, dashboard).keys()
	assertASCII("settings root", drawn(dashboard, 120, 40))
	for _, root := range roots {
		dashboard := asciiDashboard()
		dashboard.beginSettingsSection(root, nil)
		assertASCII("settings "+root, drawn(dashboard, 120, 40))
		assertASCII("settings "+root+" narrow", drawn(dashboard, 80, 30))
	}
	// A field's popup and its header.
	for _, field := range [][2]string{
		{"advanced", "symbols"},
		{"advanced", "session_order"},
		{"interface", "theme"},
		{"interface", "spinner"},
		{"interface", "sessions_side"},
	} {
		dashboard := asciiDashboard()
		dashboard.beginSettingsSection(field[0], nil)
		setupChoose(t, dashboard, field[1])
		lines := drawn(dashboard, 100, 30)
		if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "^/v select") }) {
			t.Fatalf("%s: the popup header is drawn: %q", field[1], lines)
		}
		assertASCII(fmt.Sprintf("settings %s %s popup", field[0], field[1]), lines)
	}

	// The dashboard's own dialogs.
	for _, command := range []CommandId{CommandIdPalette, CommandIdNewSessionWizard, CommandIdWorkspaces, CommandIdResumeDialog} {
		dashboard := asciiDashboard()
		dashboard.DispatchCommand(command)
		assertASCII(fmt.Sprintf("%v", command), drawn(dashboard, 120, 40))
		assertASCII(fmt.Sprintf("%v narrow", command), drawn(dashboard, 80, 30))
	}
}
