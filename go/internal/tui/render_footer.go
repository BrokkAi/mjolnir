// Port of mj-tui/src/render/footer.rs.
package tui

import (
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
	panic("unported: mj-tui/src/render/footer.rs footer_commands")
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
	panic("unported: mj-tui/src/render/footer.rs direct_palette_commands")
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
	panic("unported: mj-tui/src/render/footer.rs render_footer")
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
