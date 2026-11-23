// Port of mj-tui/src/render/footer.rs.
package tui

import (
	"cmp"
	"slices"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// footerCommands is the registry commands shared by pane and composer
// footers, retaining their identities.
//
// The chord group comes back without the prefix: the fitter puts
// [chordPrefix] on the first chord that survives — `ctrl+b then c create` —
// so the reader always sees which key starts the chord.
func footerCommands(dashboard *DashboardState, group FooterGroup) []chat.FooterItem[CommandId] {
	type rankedHint struct {
		rank uint8
		item chat.FooterItem[CommandId]
	}
	var hints []rankedHint
	for _, id := range available(dashboard, nil) {
		spec := spec(id)
		if spec.footerGroup != group {
			continue
		}
		word, ok := spec.footer(dashboard)
		if !ok {
			continue
		}
		key, ok := dashboard.footerKey(id)
		if !ok {
			continue
		}
		hints = append(hints, rankedHint{spec.footerRank, chat.FooterItem[CommandId]{Key: id, Text: key + " " + word}})
	}
	// Stable, as Rust's sort_by_key: equal ranks keep registry order.
	slices.SortStableFunc(hints, func(a, b rankedHint) int { return cmp.Compare(a.rank, b.rank) })
	items := make([]chat.FooterItem[CommandId], len(hints))
	for i, hint := range hints {
		items[i] = hint.item
	}
	return items
}

// chordPrefix is the words that lead the chord group: `ctrl+b then `.
// `then : / ? palette` names both prefix aliases without a doubled colon.
func chordPrefix(dashboard *DashboardState) string {
	if dashboard.SimpleMode() {
		return ""
	}
	return dashboard.Keybinds().PrefixLabel() + " then "
}

// protectedHint: the palette hint survives longest when the row runs out of
// width: a narrow terminal must still say how to reach command search.
func protectedHint(id CommandId) bool {
	return id == CommandIdPalette
}

// footerGroups is the two groups the dashboard footer draws. The third slot
// exists because the shared fitting helper takes three groups; the dashboard
// has no separate function-key group any more.
func footerGroups(dashboard *DashboardState) [3][]chat.FooterItem[CommandId] {
	return [3][]chat.FooterItem[CommandId]{
		footerCommands(dashboard, FooterGroupPane),
		footerCommands(dashboard, FooterGroupChord),
		directPaletteCommands(dashboard),
	}
}

// directPaletteCommands is the palette hint under its direct (non-chord)
// key, when the palette is available and has one.
func directPaletteCommands(dashboard *DashboardState) []chat.FooterItem[CommandId] {
	if !slices.Contains(available(dashboard, nil), CommandIdPalette) {
		return nil
	}
	key, ok := dashboard.directPaletteFooterKey()
	if !ok {
		return nil
	}
	word, ok := spec(CommandIdPalette).footer(dashboard)
	if !ok {
		return nil
	}
	return []chat.FooterItem[CommandId]{{Key: CommandIdPalette, Text: key + " " + word}}
}

// fittedFooterGroups is the groups that fit in width, with the prefix still
// named on the first chord that survived (see
// [chat.FitPrefixedFooterItems]).
func fittedFooterGroups(dashboard *DashboardState, width int) [3][]chat.FooterItem[CommandId] {
	return chat.FitPrefixedFooterItems(footerGroups(dashboard), width, chordPrefix(dashboard), protectedHint)
}

// combinedFooterText is the hotkey hints for whatever applies right now.
//
// Built from the action registry (actions.go) rather than written out as a
// string per pane, so a hint can never name a key the surface does not
// answer, and a command can never be added without the footer knowing.
//
// The row groups pane keys, prefix chords, then direct keys. The reader
// therefore always looks in the same place for a given kind of key, and the
// row does not reshuffle itself as the pane changes. Each group's order
// comes from footer_group and footer_rank on the spec, not from where the
// command sits in the table.
//
// width is the row's width in cells. When the hints do not fit, whole
// segments are dropped from the right — never truncated mid-word, because
// half a hint names a key that does not exist — first from the chords, then
// from the pane group. The palette hint stays protected so command search
// remains discoverable on narrow terminals.
//
// The composer's own hints come from the chat itself, because they depend
// on what it is doing (a queued prompt, dictation, a history search); this
// text is only drawn when a pane has the keyboard.
func combinedFooterText(dashboard *DashboardState, width int) string {
	groups := fittedFooterGroups(dashboard, width)
	return chat.FooterItemsText(groups, func(item chat.FooterItem[CommandId]) string { return item.Text })
}

// renderFooter draws the shared footer row.
//
// A notice replaces the hints while one is showing, the same way the
// composer's own footer works, so the two are interchangeable and the row
// costs one line whichever surface drew it.
func renderFooter(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	rowStyle := chat.Muted().Background(chat.ThemePalette().Surface)
	// A half-typed chord owns the row: what the reader needs there is the
	// way out of it, not the hints they are already part-way through.
	if dashboard.PrefixPending() {
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(prefixBannerLine(dashboard))).WithStyle(rowStyle), area)
		return
	}
	if dashboard.ResizeModeActive() {
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(resizeBannerLine())).WithStyle(rowStyle), area)
		return
	}
	notice, hasNotice := dashboard.notices.Current()
	groups := fittedFooterGroups(dashboard, area.Dx())
	var line chat.Line
	if hasNotice {
		line = chat.StyledLine(notice, chat.Style{}.Foreground(chat.ThemePalette().Warning))
	} else {
		line = chat.Hints(combinedFooterText(dashboard, area.Dx()))
	}
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)).WithStyle(rowStyle), area)
	// The dashboard has no active composer to register these controls for
	// us. Register the same fitted segments that were drawn so a footer
	// click dispatches the command represented by that exact hint.
	if hasNotice {
		return
	}
	x := area.Min.X
	for _, group := range groups {
		if len(group) == 0 {
			continue
		}
		if x > area.Min.X {
			x += chat.RawLine(chat.FooterGroupSeparator()).Width()
		}
		for index, item := range group {
			if index > 0 {
				x += chat.RawLine(chat.FooterSeparator()).Width()
			}
			width := chat.RawLine(item.Text).Width()
			renderFooterCommand(frame,
				uv.Rect(x, area.Min.Y, min(width, max(area.Max.X-x, 0)), area.Dy()),
				dashboard, item.Key, item.Text)
			x += width
		}
	}
}

// resizeBannerLine is the footer row while panes are being resized.
func resizeBannerLine() chat.Line {
	return chat.RawLine("Resize panes: h/j/k/l or arrows · Esc done")
}

// prefixBannerLine is the `PREFIX` banner with this dashboard's live prefix
// and palette keys.
func prefixBannerLine(dashboard *DashboardState) chat.Line {
	var paletteKeys *string
	if labels := dashboard.paletteBindingLabels(core.TriggerPrefix, true); len(labels) > 0 {
		joined := strings.Join(labels, " / ")
		paletteKeys = &joined
	}
	return chat.PrefixBanner(dashboard.Keybinds().PrefixLabel(), paletteKeys)
}

// refreshAge is Rust's `pub(crate) use mj_core::text::refresh_age` in
// render/footer.rs, which render_changes.rs and the quota pane import from
// crate::render.
func refreshAge(now, refreshed uint64) string {
	return core.RefreshAge(now, refreshed)
}
