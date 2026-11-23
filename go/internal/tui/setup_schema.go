// Port of mj-tui/src/setup/schema.rs.

// The shared settings schema, plus the theme report only a terminal can
// make: what Auto resolved to against this terminal's background, and
// whether `NO_COLOR` overrides the configured theme.
//
// Rust's `pub(super) use mj_client::settings::schema::*` has no Go form: the
// setup files call package client's schema functions directly.

package tui

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// setupSectionSummary is client.SectionSummary with the theme reported as
// this terminal draws it (Rust's setup::schema::section_summary).
func setupSectionSummary(key string, draft any) (string, bool) {
	return client.SectionSummary(key, draft, draftThemeReport)
}

// setupThemeReport is the theme as Setup reports it: what is on screen and
// why. `NO_COLOR` is an override, not a theme: chat.EffectiveTheme draws in
// monochrome whatever is configured, and the configured choice is kept and
// comes back once `NO_COLOR` is unset, so the report names the choice that
// is waiting. Auto names the palette it picked and what the terminal
// reported.
func setupThemeReport(configured core.ThemeChoice, noColor bool, background *chat.Rgb) string {
	resolution := chat.Resolve(configured, noColor, background)
	label := resolution.Theme.Label()
	switch reason := resolution.Reason.(type) {
	case chat.ThemeReasonNoColor:
		return fmt.Sprintf("Monochrome (NO_COLOR; configured: %s)", configured.Label())
	case chat.ThemeReasonTerminalBackground:
		return fmt.Sprintf("Auto: %s (terminal background #%02x%02x%02x)", label,
			reason.Background.R, reason.Background.G, reason.Background.B)
	case chat.ThemeReasonUnreported:
		return fmt.Sprintf("Auto: %s (terminal did not report its background)", label)
	default:
		return label
	}
}

// draftThemeReport is setupThemeReport for the draft's theme value, as this
// process sees it.
func draftThemeReport(value, draft any) string {
	encoded := setupValueDisplay(value)
	configured, err := core.UnmarshalThemeChoiceJSON([]byte(encoded))
	if err != nil {
		return client.ChoiceLabel([]string{"theme"}, value, draft)
	}
	return setupThemeReport(configured, noColorRequested(), chat.TerminalBackground())
}
