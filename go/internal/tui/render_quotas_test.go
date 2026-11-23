// Port of mj-tui/src/render/quotas.rs tests.

package tui

import (
	"encoding/json"
	"fmt"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 8253cfeee: tiny refills took their minimum cells from the
// available fill, drawing a 97% pool as 70% of a 20-cell bar.
func TestRefillMinimumCellsNeverShrinkTheAvailableFill(t *testing.T) {
	refills := make([]float64, 6)
	for i := range refills {
		refills[i] = 100.0
	}
	widths := refillWidths(200.0*100.0, refills, 206.0*100.0, 20)
	total := 0
	for _, width := range widths {
		total += width
	}
	if total != 20 {
		t.Fatalf("%v sums to %d", widths, total)
	}
	// 97% of 20 cells; refills that cannot fit go under the bar.
	if widths[0] < 19 {
		t.Fatalf("%v", widths)
	}
}

// quotaFixture is the recorded provider readings, configured as one
// enabled profile each, with the Quota pane focused, and the time of the
// newest reading.
func quotaFixture(t *testing.T) (*DashboardState, uint64) {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", "tests", "fixtures", "quota-provider-readings.json"))
	if err != nil {
		t.Fatal(err)
	}
	var quotas map[string]client.ProfileQuota
	if err := json.Unmarshal(data, &quotas); err != nil {
		t.Fatal(err)
	}
	var now uint64
	for _, quota := range quotas {
		now = max(now, quota.RefreshedAtEpochSeconds)
	}
	testConfig := config()
	testConfig.Profiles = map[string]core.HarnessProfile{}
	for id, quota := range quotas {
		testConfig.Profiles[id] = core.NewHarnessProfile(quota.Harness, "/quota-fixture/"+id)
	}
	dashboard := NewDashboardState(testConfig, core.NewStateState(), quotas)
	dashboard.quotaProviders.settle()
	dashboard.focus = FocusQuota
	return dashboard, now
}

// fixtureClock formats as the readings were recorded: in Chicago in
// November. It keeps the render portable across CI hosts without changing
// the process timezone.
func fixtureClock(epoch int64, format string) string {
	return chronoFormat(time.Unix(epoch, 0).In(time.FixedZone("", -6*3600)), format)
}

// captureQuotas draws the Quota pane alone and appends its text, then its
// background runs: labels can cover fill glyphs, so backgrounds and
// modifiers keep monochrome fill boundaries part of the surface contract.
func captureQuotas(output *strings.Builder, label string, dashboard *DashboardState, now uint64, width, height int) {
	terminal := newRenderTerminal(width, height)
	terminal.draw(func(frame *chat.Frame) {
		renderQuotasAt(frame, frame.Area(), dashboard, nil, now, fixtureClock)
		if chat.ASCII() {
			chat.FoldBufferToASCII(frame.Buffer())
		}
	})
	buffer := terminal.buffer
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	for y := range height {
		var row strings.Builder
		for x := range width {
			row.WriteString(renderCellSymbol(buffer, x, y))
		}
		fmt.Fprintf(output, "%s\n", strings.TrimRight(row.String(), " \t\n\r"))
	}
	output.WriteString("background runs (x..end, colour):\n")
	for y := range height {
		var runs []string
		start := 0
		for x := 1; x <= width; x++ {
			startStyle := renderCellStyle(buffer, start, y)
			if x == width {
				runs = append(runs, fmt.Sprintf("%d..%d %#v %#v", start, x, startStyle.Bg, startStyle.AddModifier))
				break
			}
			style := renderCellStyle(buffer, x, y)
			if style.Bg != startStyle.Bg || style.AddModifier != startStyle.AddModifier {
				runs = append(runs, fmt.Sprintf("%d..%d %#v %#v", start, x, startStyle.Bg, startStyle.AddModifier))
				start = x
			}
		}
		fmt.Fprintf(output, "%d: %s\n", y, strings.Join(runs, ", "))
	}
}

// quotasAllPoolKeys is every pool of the dashboard's quota, as the sorted set of
// expanded pools holds them.
func quotasAllPoolKeys(dashboard *DashboardState) []PoolKey {
	var keys []PoolKey
	for _, provider := range dashboard.quotaPools() {
		for _, pool := range provider.Pools {
			keys = append(keys, pool.Key)
		}
	}
	slices.SortFunc(keys, PoolKey.compare)
	return slices.CompactFunc(keys, func(a, b PoolKey) bool { return a == b })
}

// quotasRowIndex is the index of the first quota row matching match; it
// fails the test when none does.
func quotasRowIndex(t *testing.T, dashboard *DashboardState, match func(QuotaRow) bool) int {
	t.Helper()
	index := slices.IndexFunc(dashboard.quotaRows(), match)
	if index < 0 {
		t.Fatalf("no matching quota row in %#v", dashboard.quotaRows())
	}
	return index
}

// quotasLaneOf matches the lane of profile id in any pool.
func quotasLaneOf(id string) func(QuotaRow) bool {
	return func(row QuotaRow) bool {
		lane, ok := row.(QuotaRowLane)
		return ok && lane.ID == id
	}
}

// Hard-won: quota review: an indexed accent cannot be tinted, so filled and spent cells matched.
func TestIndexedAccentMarksFilledCellsApartFromSpentOnes(t *testing.T) {
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		chat.WithTheme(core.UiThemeTerminalLight, func() {
			dashboard, now := quotaFixture(t)
			pool := dashboard.quotaPools()[0].Pools[0]
			pool.Members = pool.Members[:1]
			pool.Members[0].Remaining = 50
			pool.Members[0].AwaitingReading = false
			pool.Remaining = 50
			accent := chat.ThemePalette().Muted
			if _, _, _, rgb := accent.RGB(); rgb {
				t.Fatal("the accent is not RGB")
			}
			spans, _ := poolBar(&pool, 20, now, accent, map[string]struct{}{})
			find := func(glyph string) chat.Span {
				index := slices.IndexFunc(spans, func(span chat.Span) bool { return strings.Contains(span.Content, glyph) })
				if index < 0 {
					t.Fatalf("no %q span in %#v", glyph, spans)
				}
				return spans[index]
			}
			available, refill := find("█"), find("=")
			// Filled and spent cells must look different: glyph or colour.
			look := func(span chat.Span) [3]any {
				first, _ := quotasFirstRune(span.Content)
				return [3]any{first, span.Style.Fg, span.Style.Bg}
			}
			if look(available) == look(refill) {
				t.Fatalf("filled %#v and spent %#v look alike", available, refill)
			}
		})
	})
}

// quotasFirstRune is the first character of text.
func quotasFirstRune(text string) (rune, bool) {
	for _, character := range text {
		return character, true
	}
	return 0, false
}

// Hard-won: db4dc77c9: refresh reordered pool lanes and stranded selection on another profile.
func TestQuotaLaneSelectionSurvivesRefreshByIdentity(t *testing.T) {
	dashboard, now := quotaFixture(t)
	poolKey, _ := dashboard.quotaRows()[0].pool()
	dashboard.toggleQuotaPool()
	dashboard.handleDashboardKey(key(tea.KeyDown))
	var selected QuotaRow = QuotaRowLane{Key: poolKey, ID: "claude2"}
	if row := dashboard.selectedQuotaRow(); row != selected {
		t.Fatalf("selected %#v", row)
	}
	before := dashboard.selectionFor(FocusQuota)
	var ignored strings.Builder
	captureQuotas(&ignored, "before refresh", dashboard, now, 60, 6)
	previousOffset := dashboard.quotaScroll
	dashboard.recenterOnSelection = nil
	dashboard.SetQuotas(maps.Clone(dashboard.quotas))
	captureQuotas(&ignored, "unchanged refresh", dashboard, now, 60, 6)
	if dashboard.quotaScroll != previousOffset {
		t.Fatalf("scroll %d, was %d", dashboard.quotaScroll, previousOffset)
	}
	// A wheel offset away from the selection survives a stationary refresh.
	dashboard.quotaScroll = 0
	dashboard.recenterOnSelection = nil
	dashboard.SetQuotas(maps.Clone(dashboard.quotas))
	captureQuotas(&ignored, "wheel then refresh", dashboard, now, 60, 6)
	if dashboard.quotaScroll != 0 {
		t.Fatalf("scroll %d", dashboard.quotaScroll)
	}
	reports := maps.Clone(dashboard.quotas)
	claude2 := reports["claude2"]
	claude2.Windows = slices.Clone(claude2.Windows)
	resets := int64(now) + 10*86400
	claude2.Windows[0].ResetsAtEpochSeconds = &resets
	reports["claude2"] = claude2
	dashboard.SetQuotaSnapshot(client.QuotaSnapshot{Reports: reports})
	if row := dashboard.selectedQuotaRow(); row != selected {
		t.Fatalf("selected %#v", row)
	}
	if dashboard.selectionFor(FocusQuota) == before {
		t.Fatal("the reordered lane moved")
	}
	captureQuotas(&ignored, "reordered refresh", dashboard, now, 60, 6)
	if !slices.ContainsFunc(dashboard.quotaPoolAreas, func(area client.DaemonPair[QuotaRow, uv.Rectangle]) bool {
		return dashboard.selectedQuotaRow() == area.First
	}) {
		t.Fatal("the selected lane is on screen")
	}
	if dashboard.quotaScroll <= previousOffset {
		t.Fatalf("scroll %d, was %d", dashboard.quotaScroll, previousOffset)
	}
	reports = maps.Clone(dashboard.quotas)
	delete(reports, "claude2")
	dashboard.SetQuotas(reports)
	// Hard-won: quota review: a profile whose row changed type lost its
	// selection to the first row, so `e` renamed another profile.
	if row := dashboard.selectedQuotaRow(); row != (QuotaRowProfile{ID: "claude2"}) {
		t.Fatalf("selected %#v", row)
	}
	dashboard.quotaSelection = QuotaRowPool{Key: poolKey}
	dashboard.handleDashboardKey(key(tea.KeyDown))
	if _, lane := dashboard.selectedQuotaRow().(QuotaRowLane); !lane {
		t.Fatalf("selected %#v", dashboard.selectedQuotaRow())
	}
	dashboard.handleDashboardKey(key(tea.KeyEnter))
	if row := dashboard.selectedQuotaRow(); row != (QuotaRowPool{Key: poolKey}) {
		t.Fatalf("selected %#v", row)
	}
	if slices.Contains(dashboard.quotaExpanded, poolKey) {
		t.Fatal("Enter on a lane collapses its pool")
	}
}

// Hard-won: quota review: clicks used stale positions after a quota refresh reordered rows.
func TestQuotaClickUsesRenderedIdentityAfterSnapshotChanges(t *testing.T) {
	dashboard, now := quotaFixture(t)
	dashboard.toggleQuotaPool()
	var ignored strings.Builder
	captureQuotas(&ignored, "click", dashboard, now, 132, 40)
	index := slices.IndexFunc(dashboard.quotaPoolAreas, func(area client.DaemonPair[QuotaRow, uv.Rectangle]) bool {
		return quotasLaneOf("claude5")(area.First)
	})
	if index < 0 {
		t.Fatal("claude5's lane is on screen")
	}
	identity, area := dashboard.quotaPoolAreas[index].First, dashboard.quotaPoolAreas[index].Second
	click := mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, area.Min)
	reports := maps.Clone(dashboard.quotas)
	claude5 := reports["claude5"]
	claude5.Windows = slices.Clone(claude5.Windows)
	for i := range claude5.Windows {
		resets := int64(now)
		claude5.Windows[i].ResetsAtEpochSeconds = &resets
	}
	reports["claude5"] = claude5
	dashboard.SetQuotas(reports)
	dashboard.HandleMouse(click)
	if row := dashboard.selectedQuotaRow(); row != identity {
		t.Fatalf("selected %#v, want %#v", row, identity)
	}
	reports = maps.Clone(dashboard.quotas)
	delete(reports, "claude5")
	dashboard.SetQuotas(reports)
	selection := dashboard.selectedQuotaRow()
	dashboard.HandleMouse(click)
	if row := dashboard.selectedQuotaRow(); row != selection {
		t.Fatalf("selected %#v, want %#v", row, selection)
	}
}

// quotasSymbolsName is the Debug name of a symbol set.
func quotasSymbolsName(symbols core.SymbolSet) string {
	if symbols == core.SymbolSetAscii {
		return "Ascii"
	}
	return "Unicode"
}

// quotasKeyName is crossterm's KeyCode Debug for the keys the golden presses.
func quotasKeyName(code rune) string {
	switch code {
	case tea.KeyUp:
		return "Up"
	case tea.KeyPgDown:
		return "PageDown"
	}
	return string(code)
}

// window is a QuotaWindow with only the fields the goldens set.
func quotasWindow(label string, remaining *uint8, duration *int64, resetsAt *int64) client.QuotaWindow {
	return client.QuotaWindow{
		Label:                label,
		RemainingPercent:     remaining,
		DurationSeconds:      duration,
		ResetsAtEpochSeconds: resetsAt,
	}
}

// quotasRetainProfiles keeps only the configured profiles keep accepts.
func quotasRetainProfiles(dashboard *DashboardState, keep func(id string) bool) {
	profiles := dashboard.config.Mut().Profiles
	for id := range profiles {
		if !keep(id) {
			delete(profiles, id)
		}
	}
}

func TestGoldenQuotaProviderPools(t *testing.T) {
	var output strings.Builder
	percent := func(value uint8) *uint8 { return &value }
	seconds := func(value int64) *int64 { return &value }
	text := func(value string) *string { return &value }
	enter := key(tea.KeyEnter)
	editQuota := func(dashboard *DashboardState, id string, edit func(quota *client.ProfileQuota)) {
		quota, ok := dashboard.quotas[id]
		if !ok {
			t.Fatalf("no report for %s", id)
		}
		quota.Windows = slices.Clone(quota.Windows)
		edit(&quota)
		dashboard.quotas[id] = quota
	}
	for _, symbols := range []core.SymbolSet{core.SymbolSetUnicode, core.SymbolSetAscii} {
		name := quotasSymbolsName(symbols)
		chat.WithSymbols(symbols, func() {
			chat.WithTheme(core.UiThemeDarcula, func() {
				dashboard, now := quotaFixture(t)
				captureQuotas(&output, name+" collapsed", dashboard, now, 132, 23)
				captureQuotas(&output, name+" narrow collapsed", dashboard, now, 60, 30)
				// Show missing and cached readings through the daemon's
				// probing snapshot.
				reports := maps.Clone(dashboard.quotas)
				refreshing := maps.Clone(reports)
				delete(refreshing, "claude2")
				dashboard.SetQuotaSnapshot(client.QuotaSnapshot{
					Reports: refreshing,
					Probing: map[string]struct{}{"claude2": {}, "claude5": {}, "claude6": {}},
				})
				captureQuotas(&output, name+" missing and cached readings while probing", dashboard, now, 132, 24)
				dashboard.SetQuotaSnapshot(client.QuotaSnapshot{Reports: reports})
				// Exercise the actual key dispatch and selection transition.
				dashboard.handleDashboardKey(enter)
				if len(dashboard.quotaExpanded) != 1 {
					t.Fatalf("%d expanded", len(dashboard.quotaExpanded))
				}
				google := quotasRowIndex(t, dashboard, func(row QuotaRow) bool {
					pool, ok := row.(QuotaRowPool)
					return ok && pool.Key.HasScope && pool.Key.Scope == "gemini"
				})
				dashboard.setSelectionFor(FocusQuota, google)
				dashboard.handleDashboardKey(enter)
				if len(dashboard.quotaExpanded) != 2 {
					t.Fatalf("%d expanded", len(dashboard.quotaExpanded))
				}
				dashboard.setSelectionFor(FocusQuota, 0)
				captureQuotas(&output, name+" expanded Anthropic and Gemini", dashboard, now, 132, 36)
				dashboard.handleDashboardKey(enter)
				if len(dashboard.quotaExpanded) != 1 {
					t.Fatalf("%d expanded", len(dashboard.quotaExpanded))
				}
				dashboard.handleDashboardKey(enter)
				if len(dashboard.quotaExpanded) != 2 {
					t.Fatalf("%d expanded", len(dashboard.quotaExpanded))
				}
				captureQuotas(&output, name+" narrow expanded", dashboard, now, 60, 15)
				captureQuotas(&output, name+" narrow expanded full Anthropic", dashboard, now, 60, 40)
				dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
				dashboard.setSelectionFor(FocusQuota, 0)
				captureQuotas(&output, name+" all periods and status labels", dashboard, now, 132, 68)
				dashboard.quotaExpanded = nil
				rows := dashboard.quotaRows()
				lastPool := -1
				for index, row := range rows {
					if _, ok := row.(QuotaRowPool); ok {
						lastPool = index
					}
				}
				if lastPool < 0 {
					t.Fatal("a pool row")
				}
				dashboard.setSelectionFor(FocusQuota, lastPool)
				dashboard.handleDashboardKey(enter)
				dashboard.handleDashboardKey(key(tea.KeyEnd))
				if _, ok := dashboard.selectedQuotaRow().(QuotaRowProfile); !ok {
					t.Fatalf("selected %#v", dashboard.selectedQuotaRow())
				}
				dashboard.handleDashboardKey(key(tea.KeyUp))
				if !quotasLaneOf("muse6")(dashboard.selectedQuotaRow()) {
					t.Fatalf("selected %#v", dashboard.selectedQuotaRow())
				}
				lastPoolKey, _ := dashboard.selectedQuotaRow().pool()
				captureQuotas(&output, name+" last pool final lane in short pane", dashboard, now, 132, 5)
				laneIndex := quotasRowIndex(t, dashboard, quotasLaneOf("muse5"))
				areaIndex := slices.IndexFunc(dashboard.quotaPoolAreas, func(area client.DaemonPair[QuotaRow, uv.Rectangle]) bool {
					return area.First == dashboard.quotaRows()[laneIndex]
				})
				if areaIndex < 0 {
					t.Fatal("muse5's lane is on screen")
				}
				area := dashboard.quotaPoolAreas[areaIndex].Second
				dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, area.Min))
				if !quotasLaneOf("muse5")(dashboard.selectedQuotaRow()) {
					t.Fatalf("selected %#v", dashboard.selectedQuotaRow())
				}
				dashboard.handleDashboardKey(enter)
				if row := dashboard.selectedQuotaRow(); row != (QuotaRowPool{Key: lastPoolKey}) {
					t.Fatalf("selected %#v", row)
				}
				dashboard.quotaExpanded = nil
				short := quotasRowIndex(t, dashboard, func(row QuotaRow) bool {
					pool, ok := row.(QuotaRowPool)
					_, fiveHour := pool.Key.Period.(PoolPeriodFiveHour)
					return ok && fiveHour
				})
				dashboard.setSelectionFor(FocusQuota, short)
				captureQuotas(&output, name+" six-cell full and spent segments", dashboard, now, 30, 6)
			})
		})
	}
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		chat.WithTheme(core.UiThemeMono, func() {
			dashboard, now := quotaFixture(t)
			dashboard.toggleQuotaPool()
			captureQuotas(&output, "monochrome expanded Anthropic", dashboard, now, 132, 30)
			dashboard.quotaExpanded = nil
			short := quotasRowIndex(t, dashboard, func(row QuotaRow) bool {
				pool, ok := row.(QuotaRowPool)
				_, fiveHour := pool.Key.Period.(PoolPeriodFiveHour)
				return ok && fiveHour
			})
			dashboard.setSelectionFor(FocusQuota, short)
			captureQuotas(&output, "monochrome six-cell full and spent segments", dashboard, now, 30, 6)
		})
	})
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		dashboard, now := quotaFixture(t)
		reports := maps.Clone(dashboard.quotas)
		claude7 := reports["claude7"]
		claude7.Windows = slices.Clone(claude7.Windows)
		claude7.Windows[0].RemainingPercent = percent(3)
		reports["claude7"] = claude7
		long := reports["claude4"]
		delete(reports, "claude4")
		longID := "claude-with-an-extremely-long-profile-name-that-must-not-hide-readings"
		long.ProfileID = longID
		reports[longID] = long
		profiles := dashboard.config.Mut().Profiles
		profile := profiles["claude4"]
		delete(profiles, "claude4")
		profiles[longID] = profile
		dashboard.quotaProviders = providerDiscoveryStart(dashboard.config.Get())
		dashboard.quotaProviders.settle()
		dashboard.SetQuotas(reports)
		dashboard.toggleQuotaPool()
		captureQuotas(&output, "small balance and long profile at narrow width", dashboard, now, 60, 40)
		lane := quotasRowIndex(t, dashboard, quotasLaneOf("claude5"))
		dashboard.setSelectionFor(FocusQuota, lane)
		captureQuotas(&output, "selected wrapped lane in short viewport", dashboard, now, 60, 6)
		recenter := FocusQuota
		dashboard.recenterOnSelection = &recenter
		captureQuotas(&output, "wrapped lane first line in one-line viewport", dashboard, now, 60, 3)
		selection := dashboard.selectedQuotaRow()
		first := dashboard.quotaScroll
		dashboard.paneAreas = &[dashboardPaneCount]uv.Rectangle{{}, {}, uv.Rect(0, 0, 60, 3)}
		for step := 1; step <= 2; step++ {
			dashboard.HandleMouse(mouseAt(tea.MouseWheelMsg{Button: tea.MouseWheelDown}, uv.Pos(10, 1)))
			captureQuotas(&output, fmt.Sprintf("wrapped lane wheel continuation %d", step), dashboard, now, 60, 3)
			if dashboard.quotaScroll != first+step {
				t.Fatalf("scroll %d, want %d", dashboard.quotaScroll, first+step)
			}
			if row := dashboard.selectedQuotaRow(); row != selection {
				t.Fatalf("selected %#v", row)
			}
		}
		// Hard-won: quota review: keyboard-only users could not reach the rest of a wrapped note.
		for _, press := range []struct {
			code rune
			step int
		}{{tea.KeyUp, 1}, {tea.KeyUp, 2}, {tea.KeyPgDown, 1}} {
			dashboard.handleDashboardKey(key(press.code))
			captureQuotas(&output, fmt.Sprintf("wrapped lane key continuation %s %d", quotasKeyName(press.code), press.step), dashboard, now, 60, 3)
			if row := dashboard.selectedQuotaRow(); row != selection {
				t.Fatalf("selected %#v", row)
			}
		}
		if dashboard.quotaScroll != first+1 {
			t.Fatalf("scroll %d, want %d", dashboard.quotaScroll, first+1)
		}
		dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
		moonshot := quotasRowIndex(t, dashboard, quotasLaneOf("kimi"))
		dashboard.setSelectionFor(FocusQuota, moonshot)
		captureQuotas(&output, "Month and 5H with booster notice", dashboard, now, 100, 12)
		reports = maps.Clone(dashboard.quotas)
		quotasRetainProfiles(dashboard, func(id string) bool { return id == "codex-short-full" || id == "codex-short-spent" })
		for id := range reports {
			if id != "codex-short-full" {
				delete(reports, id)
			}
		}
		dashboard.SetQuotaSnapshot(client.QuotaSnapshot{
			Reports: reports,
			Probing: map[string]struct{}{"codex-short-spent": {}},
		})
		dashboard.setSelectionFor(FocusQuota, 0)
		captureQuotas(&output, "full balance with one missing subscription", dashboard, now, 70, 9)
		minimized := minimizedQuotaLine(dashboard, 110, true, now)
		// Hard-won: quota review: Kimi's Week balance vanished behind its Month window.
		dashboard, now = quotaFixture(t)
		quotasRetainProfiles(dashboard, func(id string) bool { return id == "kimi" || id == "claude2" || id == "grok" })
		editQuota(dashboard, "kimi", func(kimi *client.ProfileQuota) {
			week := kimi.Windows[0]
			week.Label = "Week"
			week.RemainingPercent = percent(25)
			week.ResetsAtEpochSeconds = seconds(int64(now) + 3*3600)
			kimi.Windows = append(kimi.Windows, week)
		})
		// The cached reset has passed with no newer reading.
		editQuota(dashboard, "claude2", func(stale *client.ProfileQuota) {
			stale.Windows[0].RemainingPercent = percent(80)
			stale.Windows[0].ResetsAtEpochSeconds = seconds(int64(now) - 600)
			stale.RefreshedAtEpochSeconds = now - 3600
		})
		dashboard.SetQuotas(maps.Clone(dashboard.quotas))
		dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
		dashboard.setSelectionFor(FocusQuota, 0)
		captureQuotas(&output, "Week and Month pools, and a reset due without a reading", dashboard, now, 100, 20)
		// Judge staleness on the fixture's clock, not the wall clock.
		pools := providerPools(dashboard.config.Get(), dashboard.quotas, &dashboard.quotaProviders, now)
		moonshotIndex := slices.IndexFunc(pools, func(provider ProviderQuota) bool {
			return provider.Provider == core.QuotaProviderMoonshot{}
		})
		if moonshotIndex < 0 {
			t.Fatal("the Moonshot provider")
		}
		var periods []string
		for _, pool := range pools[moonshotIndex].Pools {
			periods = append(periods, fmt.Sprintf("%s %d", pool.Key.Period.label(), pool.Remaining))
		}
		if !slices.Equal(periods, []string{"week 25", "Month 40"}) {
			t.Fatalf("%q", periods)
		}
		// Hard-won: quota review: foreground over a light fill used the theme's near-white colour.
		chat.WithSymbols(core.SymbolSetAscii, func() {
			chat.WithTheme(core.UiThemeMidnight, func() {
				terminal := newRenderTerminal(100, 20)
				terminal.draw(func(frame *chat.Frame) {
					renderQuotasAt(frame, frame.Area(), dashboard, nil, now, fixtureClock)
				})
				light := providerAccent(core.QuotaProviderXAI{})
				var filled []chat.Style
				for y := range 20 {
					for x := range 100 {
						if style := renderCellStyle(terminal.buffer, x, y); style.Bg == light && renderCellSymbol(terminal.buffer, x, y) == "#" {
							filled = append(filled, style)
						}
					}
				}
				if len(filled) == 0 {
					t.Fatal("a light fill is drawn")
				}
				for _, style := range filled {
					if style.Fg != readableOn(light) {
						t.Fatalf("fill foreground %#v", style.Fg)
					}
				}
				if readableOn(light) == chat.ThemePalette().Text {
					t.Fatal("the fill text is not the theme text")
				}
			})
		})
		fmt.Fprintf(&output, "minimized partial totals: %s\n", strings.TrimRight(minimized.String(), " \t\n\r"))
	})
	// Hard-won: quota review: unreadable long windows hid usable profiles and short limits did not block lanes.
	for _, symbols := range []core.SymbolSet{core.SymbolSetUnicode, core.SymbolSetAscii} {
		chat.WithSymbols(symbols, func() {
			dashboard, now := quotaFixture(t)
			quotasRetainProfiles(dashboard, func(id string) bool { return id == "kimi" || strings.HasPrefix(id, "codex") })
			var shortFallback client.QuotaWindow
			editQuota(dashboard, "kimi", func(kimi *client.ProfileQuota) {
				unknown := kimi.Windows[0]
				unknown.Label = "Week"
				unknown.DurationSeconds = seconds(604800)
				unknown.RemainingPercent = nil
				unknown.Used = nil
				limit := int64(100)
				unknown.Limit = &limit
				kimi.Windows = []client.QuotaWindow{
					unknown,
					quotasWindow("5H", percent(70), seconds(18000), seconds(int64(now)+7200)),
				}
				shortFallback = kimi.Windows[1]
			})
			for _, codex := range []struct {
				id           string
				week, short  uint8
				resetSeconds int64
			}{{"codex", 80, 0, 7200}, {"codex2", 0, 0, 21600}, {"codex3", 0, 0, 3600}} {
				editQuota(dashboard, codex.id, func(report *client.ProfileQuota) {
					report.Windows = []client.QuotaWindow{
						quotasWindow("Week", percent(codex.week), seconds(604800), seconds(int64(now)+10800)),
						quotasWindow("5H", percent(codex.short), seconds(18000), seconds(int64(now)+codex.resetSeconds)),
					}
				})
			}
			editQuota(dashboard, "codex4", func(report *client.ProfileQuota) {
				unknown := report.Windows[0]
				unknown.RemainingPercent = nil
				unknown.Used = nil
				report.Windows = []client.QuotaWindow{unknown, shortFallback}
			})
			dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
			dashboard.setSelectionFor(FocusQuota, 0)
			captureQuotas(&output, quotasSymbolsName(symbols)+" unreadable week and exhausted overlapping windows", dashboard, now, 132, 34)
		})
	}
	for _, symbols := range []core.SymbolSet{core.SymbolSetUnicode, core.SymbolSetAscii} {
		chat.WithSymbols(symbols, func() {
			chat.WithTheme(core.UiThemeMono, func() {
				dashboard, now := quotaFixture(t)
				quotasRetainProfiles(dashboard, func(id string) bool { return strings.HasPrefix(id, "codex-short-") })
				editQuota(dashboard, "codex-short-spent", func(report *client.ProfileQuota) {
					report.Windows[0].RemainingPercent = percent(30)
				})
				dashboard.setSelectionFor(FocusQuota, 0)
				captureQuotas(&output, quotasSymbolsName(symbols)+" monochrome 30 vs 100 percent under labels", dashboard, now, 30, 7)
			})
		})
	}
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		dashboard, now := quotaFixture(t)
		editQuota(dashboard, "claude5", func(report *client.ProfileQuota) {
			reset := report.WeeklyWindow().ResetsAtEpochSeconds
			index := slices.IndexFunc(report.Windows, func(window client.QuotaWindow) bool { return window.Label == "5H" })
			if index < 0 {
				t.Fatal("claude5's 5H window")
			}
			report.Windows[index].ResetsAtEpochSeconds = reset
		})
		dashboard.toggleQuotaPool()
		captureQuotas(&output, "coincident weekly and five-hour resets", dashboard, now, 60, 32)
	})
	// Hard-won: quota review: a Month limit did not block the weekly lane, a stale
	// five-hour reading hid the weekly reset marker, and indexed accents drew
	// filled and spent cells alike.
	for _, symbols := range []core.SymbolSet{core.SymbolSetUnicode, core.SymbolSetAscii} {
		chat.WithSymbols(symbols, func() {
			dashboard, now := quotaFixture(t)
			quotasRetainProfiles(dashboard, func(id string) bool { return id == "kimi" || id == "claude5" })
			editQuota(dashboard, "kimi", func(kimi *client.ProfileQuota) {
				kimi.Windows[0].RemainingPercent = percent(0)
				kimi.Windows[0].ResetsAtEpochSeconds = seconds(int64(now) + 12*3600)
				week := kimi.Windows[0]
				week.Label = "Week"
				week.RemainingPercent = percent(100)
				week.ResetsAtEpochSeconds = seconds(int64(now) + 2*86400)
				kimi.Windows = append(kimi.Windows, week)
			})
			editQuota(dashboard, "claude5", func(claude *client.ProfileQuota) {
				claude.RefreshedAtEpochSeconds = now - 3600
				for i := range claude.Windows {
					claude.Windows[i].ResetsAtEpochSeconds = seconds(int64(now) + 4*3600)
					if claude.Windows[i].Label == "5H" {
						claude.Windows[i].ResetsAtEpochSeconds = seconds(int64(now) - 600)
					}
				}
			})
			dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
			dashboard.setSelectionFor(FocusQuota, 0)
			captureQuotas(&output, quotasSymbolsName(symbols)+" month limit blocks week lane; stale five-hour keeps week marker", dashboard, now, 100, 20)
		})
	}
	claudeOnly := func(id string) bool {
		return strings.HasPrefix(id, "claude") && id != "claude-aws" && id != "claude-login"
	}
	for _, symbols := range []core.SymbolSet{core.SymbolSetUnicode, core.SymbolSetAscii} {
		chat.WithSymbols(symbols, func() {
			dashboard, now := quotaFixture(t)
			quotasRetainProfiles(dashboard, claudeOnly)
			// Twelve timeline cells at this width: reset on cell starts,
			// including the first cell, must belong to exactly one cell.
			claude5 := dashboard.quotas["claude5"]
			template := *claude5.FiveHourWindow()
			editQuota(dashboard, "claude2", func(report *client.ProfileQuota) {
				report.Windows[0].ResetsAtEpochSeconds = seconds(int64(now) + 4*3600)
			})
			for _, shifted := range []struct {
				id    string
				hours int64
			}{{"claude5", 2}, {"claude7", 0}} {
				editQuota(dashboard, shifted.id, func(report *client.ProfileQuota) {
					five := template
					five.ResetsAtEpochSeconds = seconds(int64(now) + shifted.hours*3600)
					report.Windows = slices.DeleteFunc(report.Windows, func(window client.QuotaWindow) bool { return window.Label == "5H" })
					report.Windows = append(report.Windows, five)
				})
			}
			dashboard.toggleQuotaPool()
			captureQuotas(&output, quotasSymbolsName(symbols)+" reset at cell start and banked fallback", dashboard, now, 60, 36)
		})
	}
	// Hard-won: quota review: one-cell and omitted segments looked spent at 40%.
	for _, symbols := range []core.SymbolSet{core.SymbolSetUnicode, core.SymbolSetAscii} {
		chat.WithSymbols(symbols, func() {
			dashboard, now := quotaFixture(t)
			quotasRetainProfiles(dashboard, claudeOnly)
			editQuota(dashboard, "claude5", func(report *client.ProfileQuota) {
				index := slices.IndexFunc(report.Windows, func(window client.QuotaWindow) bool { return window.Label == "Week" })
				if index < 0 {
					t.Fatal("claude5's Week window")
				}
				report.Windows[index].RemainingPercent = percent(40)
				price := uint32(1)
				report.MonthlyPriceUsd = &price
				banked := uint64(1)
				report.BankedResets = &banked
			})
			for _, size := range [][2]int{{60, 8}, {16, 26}, {11, 44}} {
				captureQuotas(&output, quotasSymbolsName(symbols)+" tiny segment remaining and banked resets", dashboard, now, size[0], size[1])
			}
		})
	}
	// Hard-won: quota review: failed probes vanished from partial totals for other periods.
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		dashboard, now := quotaFixture(t)
		quotasRetainProfiles(dashboard, func(id string) bool { return strings.HasPrefix(id, "codex") || strings.HasPrefix(id, "gemini") })
		for _, id := range []string{"codex2", "gemini2"} {
			editQuota(dashboard, id, func(report *client.ProfileQuota) {
				report.Error = text("login refused; sign in again")
				report.Windows = nil
			})
		}
		dashboard.setSelectionFor(FocusQuota, 0)
		captureQuotas(&output, "failed probes missing from every eligible period and scope", dashboard, now, 132, 22)
		for _, id := range []string{"codex2", "gemini2"} {
			editQuota(dashboard, id, func(report *client.ProfileQuota) { report.Error = nil })
		}
		captureQuotas(&output, "empty reports missing from every eligible period and scope", dashboard, now, 132, 22)
	})
	// Muse while no five-hour window is open: Meta reports nothing, so one
	// profile shows the weekly figure carried from its earlier probes and
	// the other, with none to carry, says it is idle.
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		dashboard, now := quotaFixture(t)
		quotasRetainProfiles(dashboard, func(id string) bool { return id == "muse2" || id == "muse3" })
		editQuota(dashboard, "muse2", func(carried *client.ProfileQuota) {
			carried.Extra = nil
			carried.Windows = []client.QuotaWindow{
				quotasWindow("Week", percent(78), seconds(604800), seconds(int64(now)+3*86400)),
			}
		})
		editQuota(dashboard, "muse3", func(idle *client.ProfileQuota) {
			idle.Windows = nil
			idle.Extra = text(client.IdleLabel)
		})
		dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
		dashboard.setSelectionFor(FocusQuota, 0)
		captureQuotas(&output, "idle Muse with and without a weekly reading", dashboard, now, 100, 10)
	})
	// A Pro plan reports Gemini 4 Argon as its own pool; a plan that does
	// not keeps its Gemini reading in the same Gemini pool and is not
	// missing from Argon's.
	chat.WithSymbols(core.SymbolSetUnicode, func() {
		dashboard, now := quotaFixture(t)
		quotasRetainProfiles(dashboard, func(id string) bool { return strings.HasPrefix(id, "gemini") })
		editQuota(dashboard, "gemini", func(separate *client.ProfileQuota) {
			for i := range separate.Windows {
				if pool, ok := client.AntigravityPoolOfWindow(&separate.Windows[i]); ok && pool == core.AntigravityQuotaPoolGemini {
					separate.Windows[i].ModelScope = text(core.AntigravityQuotaPoolGemini.ReportedScope(true))
				}
			}
			argon := quotasWindow("Week", percent(64), seconds(604800), seconds(int64(now)+6*86400+23*3600))
			argon.ModelScope = text(core.AntigravityQuotaPoolArgon.ReportedScope(true))
			separate.Windows = append(separate.Windows, argon)
		})
		dashboard.quotaExpanded = quotasAllPoolKeys(dashboard)
		dashboard.setSelectionFor(FocusQuota, 0)
		captureQuotas(&output, "Antigravity with and without a separate Argon pool", dashboard, now, 132, 16)
	})
	core.AssertGolden(t, "testdata", "quota-provider-pools", output.String())
}
