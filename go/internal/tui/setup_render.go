// Port of mj-tui/src/setup/render.rs.

package tui

import (
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// settingGutter is the indent in front of every setting row.
const settingGutter = "  "

// preferredSize is the dialog size the draft's widest and tallest page
// needs, clamped to what Settings ever draws.
func preferredSize(draft any) SetupSize {
	panic("unported: mj-tui/src/setup/render.rs preferred_size")
}

// renderSetup draws the Settings dialog.
func renderSetup(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *SetupDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/setup/render.rs render_setup")
}

func renderCombo(frame *chat.Frame, bounds uv.Rectangle, field uv.Rectangle, control *SettingControl, dialog *SetupDialog, form *chat.Dialog[SetupControl], expanded bool) {
	panic("unported: mj-tui/src/setup/render.rs render_combo")
}

// renderControls draws the page's setting rows and returns where an
// expanded combobox and a path completion popup are anchored, to draw them
// last.
func renderControls(frame *chat.Frame, bounds uv.Rectangle, body uv.Rectangle, dashboard *DashboardState, dialog *SetupDialog, form *chat.Dialog[SetupControl]) (*uv.Rectangle, *setupPathPopup) {
	panic("unported: mj-tui/src/setup/render.rs render_controls")
}

// setupPathPopup is the focused path field whose completion popup draws
// last: its control and its field area (Rust's `(SetupControl, Rect)`).
type setupPathPopup struct {
	id    SetupControl
	field uv.Rectangle
}

// SubagentRowControls are the controls of one subagent entry.
type SubagentRowControls struct {
	index  int
	model  *SettingControl
	effort *SettingControl
	remove *SettingControl
}

func renderSubagentValue(frame *chat.Frame, bounds uv.Rectangle, fieldArea uv.Rectangle, control *SettingControl, dialog *SetupDialog, form *chat.Dialog[SetupControl], expanded **uv.Rectangle) {
	panic("unported: mj-tui/src/setup/render.rs render_subagent_value")
}

func renderSubagentControls(frame *chat.Frame, bounds uv.Rectangle, body uv.Rectangle, dialog *SetupDialog, form *chat.Dialog[SetupControl]) (*uv.Rectangle, *setupPathPopup) {
	panic("unported: mj-tui/src/setup/render.rs render_subagent_controls")
}

func renderSetupSearch(frame *chat.Frame, popup uv.Rectangle, inner uv.Rectangle, dialog *SetupDialog) {
	panic("unported: mj-tui/src/setup/render.rs render_search")
}

// splitPage splits a settings page into its body and the stacked column of
// actions.
//
// A page with no actions of its own keeps the full width; splitting on an
// empty list would still surrender the body gap to an empty column.
func splitPage(form *chat.Dialog[SetupControl], area uv.Rectangle, actions []chat.ButtonSpec[SetupControl]) chat.ColumnSplit {
	if len(actions) == 0 {
		return chat.ColumnSplit{
			Body:    area,
			Actions: uv.Rect(area.Max.X, area.Min.Y, 0, area.Dy()),
		}
	}
	return form.SplitActions(area, actions)
}

func managedHarnessDescription(kind core.HarnessKind) string {
	switch kind {
	case core.HarnessKindCodex:
		return "ChatGPT subscription or an OpenAI-compatible provider"
	case core.HarnessKindClaude:
		return "Claude Pro, Max, Team, or an Anthropic API key"
	case core.HarnessKindKimi:
		return "Kimi Code subscription"
	case core.HarnessKindGrok:
		return "Grok Build subscription"
	case core.HarnessKindMuse:
		return "Muse account; container use is supported"
	case core.HarnessKindOpenCode:
		return "OpenCode provider login or an API key"
	case core.HarnessKindDsh:
		return "DeepSeek API key"
	case core.HarnessKindAntigravity:
		return "Google OAuth login"
	}
	panic("unknown harness kind " + string(kind))
}

func renderManagedSetup(frame *chat.Frame, popup uv.Rectangle, inner uv.Rectangle, dialog *SetupDialog) {
	panic("unported: mj-tui/src/setup/render.rs render_managed_setup")
}
