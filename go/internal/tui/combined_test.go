// Port of mj-tui/src/combined.rs tests.
package tui

import "github.com/BrokkAi/mjolnir/go/internal/chat"

// renderCombinedForTest is Rust's #[cfg(test)] render_combined_for_test:
// the combined surface in the golden theme and Unicode symbols, independent
// of NO_COLOR and of the terminal, with the quota provider discovery
// settled.
func renderCombinedForTest(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, openingPanes map[PaneId]string, transcriptSelected bool) []string {
	panic("unported: mj-tui/src/combined.rs render_combined_for_test")
}
