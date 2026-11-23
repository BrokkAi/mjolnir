// Port of mj-tui/src/combined.rs tests.
package tui

import (
	"fmt"
	"maps"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// renderCombinedForTest is Rust's #[cfg(test)] render_combined_for_test:
// the combined surface in the golden theme and Unicode symbols, independent
// of NO_COLOR and of the terminal, with the quota provider discovery
// settled.
func renderCombinedForTest(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, openingPanes map[PaneId]string, transcriptSelected bool) []string {
	dashboard.quotaProviders.settle()
	// Golden output defaults to Unicode; cases can request ASCII in config.
	symbols := core.SymbolSetUnicode
	if configured := dashboard.config.Get().Advanced.Symbols; configured != nil {
		symbols = *configured
	}
	return renderCombinedWithThemeAndSymbols(
		frame,
		dashboard,
		chats,
		ConversationAccess{opening: openingPanes, canShow: func(PaneId, string) bool { return true }},
		transcriptSelected,
		// Golden output is independent of NO_COLOR and of the terminal.
		chat.Resolve(dashboard.config.Get().Theme, false, nil).Theme,
		symbols,
	)
}

// combinedRender is Rust's #[cfg(test)] render::render: the combined
// surface with no conversation on it, as a workspace with no live session
// draws.
func combinedRender(frame *chat.Frame, dashboard *DashboardState) {
	withNoColorOverrideForTest(true, func() []string {
		return renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
	})
}

// combinedDraw is one ratatui Terminal::draw on a TestBackend of the given
// size: a fresh buffer, a frame over it, and what draw left in both.
func combinedDraw(width, height int, draw func(frame *chat.Frame)) (*uv.Buffer, *chat.Frame) {
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	draw(frame)
	return buffer, frame
}

// combinedRect is ratatui's Debug of a Rect.
func combinedRect(rect uv.Rectangle) string {
	return fmt.Sprintf("Rect { x: %d, y: %d, width: %d, height: %d }", rect.Min.X, rect.Min.Y, rect.Dx(), rect.Dy())
}

// combinedOptionString is Rust's Debug of an `Option<&str>`.
func combinedOptionString(value string, ok bool) string {
	if !ok {
		return "None"
	}
	return "Some(" + core.RustDebugString(value) + ")"
}

// combinedOptionBool is Rust's Debug of an `Option<bool>`.
func combinedOptionBool(value *bool) string {
	if value == nil {
		return "None"
	}
	return fmt.Sprintf("Some(%t)", *value)
}

// combinedDismissalDebug is Rust's Debug of a NotificationDismissal.
func combinedDismissalDebug(target core.NotificationDismissal) string {
	switch target := target.(type) {
	case core.NotificationDismissalQuestion:
		return fmt.Sprintf("Question(InferredQuestion { turn_id: %s, completed_ordinal: %d })",
			core.RustDebugString(target.Value.TurnID), target.Value.CompletedOrdinal)
	case core.NotificationDismissalReadThrough:
		return fmt.Sprintf("ReadThrough(%d)", target.Value)
	case core.NotificationDismissalStack:
		return fmt.Sprintf("Stack { revision: %d }", target.Revision)
	default:
		return fmt.Sprintf("%#v", target)
	}
}

// combinedSubagentPolicyDebug is Rust's Debug of a SubagentPolicy.
func combinedSubagentPolicyDebug(policy core.SubagentPolicy) string {
	choice := func(choice *core.SubagentChoice) string {
		if choice == nil {
			return "None"
		}
		effort := "None"
		if choice.Effort != nil {
			effort = "Some(" + core.RustDebugString(*choice.Effort) + ")"
		}
		return fmt.Sprintf("Some(SubagentChoice { model: %s, effort: %s })", core.RustDebugString(choice.Model), effort)
	}
	switch policy.Mode {
	case core.SubagentPolicyMjolnir:
		return fmt.Sprintf("Mjolnir { straightforward: %s, subtle: %s, orchestrators: %t }",
			choice(policy.Straightforward), choice(policy.Subtle), policy.Orchestrators)
	case core.SubagentPolicyNone:
		return "None"
	default:
		return "Native"
	}
}

// combinedActionDebug is Rust's derived Debug of the DashboardAction
// variants the goldens of the combined surface, the palette and the
// workspace manager print.
func combinedActionDebug(action DashboardAction) string {
	switch action := action.(type) {
	case nil:
		return "None"
	case DashboardActionNone:
		return "None"
	case DashboardActionOpen:
		return fmt.Sprintf("Open { session_id: %s }", core.RustDebugString(action.SessionID))
	case DashboardActionCopySessionId:
		return fmt.Sprintf("CopySessionId { session_id: %s }", core.RustDebugString(action.SessionID))
	case DashboardActionSuspendPreflight:
		return fmt.Sprintf("SuspendPreflight { session_id: %s }", core.RustDebugString(action.SessionID))
	case DashboardActionSelectWorkspace:
		return fmt.Sprintf("SelectWorkspace { workspace_id: %s }", core.RustDebugString(action.WorkspaceID))
	case DashboardActionLoadWorkspaceManagement:
		return fmt.Sprintf("LoadWorkspaceManagement { generation: %d }", action.Generation)
	case DashboardActionRecoverWorkspaceDraft:
		return fmt.Sprintf("RecoverWorkspaceDraft { generation: %d, draft_id: %s }", action.Generation, core.RustDebugString(action.DraftID))
	case DashboardActionChangeWorkspace:
		return fmt.Sprintf("ChangeWorkspace { session_id: %s, workspace_id: %s, workspace_name: %s }",
			core.RustDebugString(action.SessionID), core.RustDebugString(action.WorkspaceID), core.RustDebugString(action.WorkspaceName))
	case DashboardActionDismissNotification:
		return fmt.Sprintf("DismissNotification { session_id: %s, target: %s }",
			core.RustDebugString(action.SessionID), combinedDismissalDebug(action.Target))
	case DashboardActionDismissNotifications:
		dismissals := make([]string, 0, len(action.Dismissals))
		for _, dismissal := range action.Dismissals {
			dismissals = append(dismissals, fmt.Sprintf("(%s, %s)", core.RustDebugString(dismissal.First), combinedDismissalDebug(dismissal.Second)))
		}
		return "DismissNotifications { dismissals: [" + strings.Join(dismissals, ", ") + "] }"
	case DashboardActionGoLaunch:
		recipe := action.Recipe
		bundle := "None"
		if recipe.BundleID != nil {
			bundle = "Some(" + core.RustDebugString(*recipe.BundleID) + ")"
		}
		project := "None"
		if recipe.ProjectDirectory != nil {
			project = "Some(" + core.RustDebugString(*recipe.ProjectDirectory) + ")"
		}
		subagents := "None"
		if recipe.Subagents != nil {
			subagents = "Some(" + combinedSubagentPolicyDebug(*recipe.Subagents) + ")"
		}
		allocation := "None"
		if recipe.ResourceAllocation != nil {
			allocation = fmt.Sprintf("Some(%#v)", recipe.ResourceAllocation)
		}
		mounts := "[]"
		if len(recipe.AdditionalMounts) > 0 {
			mounts = fmt.Sprintf("%#v", recipe.AdditionalMounts)
		}
		return fmt.Sprintf(
			"GoLaunch { recipe: GoRecipe { profile_id: %s, target_id: %s, bundle_id: %s, project_directory: %s, create_managed_worktree: %s, subagents: %s, additional_mounts: %s, resource_allocation: %s } }",
			core.RustDebugString(recipe.ProfileID), core.RustDebugString(recipe.TargetID), bundle, project,
			combinedOptionBool(recipe.CreateManagedWorktree), subagents, mounts, allocation,
		)
	default:
		return fmt.Sprintf("%#v", action)
	}
}

// Launch finding E-8 (right edge): a conversation title too long for its
// row stopped at the pane chips with no ellipsis ("End to follo ◇"). The
// title must end in an ellipsis before the chips.
//
// Hard-won: 41bf4a6f83: the launch re-verification capture showed long titles overlapping the pin and menu chips.
func TestALongConversationTitleEndsInAnEllipsisBeforeThePaneChips(t *testing.T) {
	session := runningSession()
	sessionID := session.ID
	for _, width := range []int{140, 100, 80} {
		dashboard := dashboardWithSession(session)
		fixture := client.NewReplacementSessionTestFixture(sessionID, 1)
		active := chat.ActiveChatOpen(
			fixture.Stopped,
			"hel",
			nil,
			fixture.Control,
			chat.SessionHeaderIdentity{Title: strings.Repeat("a very long session title ", 8)},
			"",
			chat.NewNotices(),
		)
		chats := map[string]*chat.ActiveChat{sessionID: active}
		buffer, _ := combinedDraw(width, 40, func(frame *chat.Frame) {
			renderCombinedForTest(frame, dashboard, chats, map[PaneId]string{}, false)
		})
		lines := bufferLines(buffer)
		row := lines[0]
		_, title, found := strings.Cut(row, "| Conversation")
		if !found {
			t.Fatalf("%d: no pane title in %s", width, row)
		}
		beforeChips, _, _ := strings.Cut(title, chat.ThemeGlyphs().Pin)
		if !strings.HasSuffix(strings.TrimRight(beforeChips, " ─"), "…") {
			t.Fatalf("%d: %s", width, row)
		}
	}
}

// Launch finding A-12 / R1-2: the composer footer drew the host's unlabeled
// first chord over the "ctrl+b then " label, so the row read "g sessionsn g
// sessions". The label and the chord after it must read as one intact hint
// at every width the campaign captured.
//
// Hard-won: 125232cbba: the first chord overwrote the composer footer label.
func TestComposerFooterKeepsTheChordLabelAndFirstChordApart(t *testing.T) {
	session := runningSession()
	sessionID := session.ID
	openChat := func() *chat.ActiveChat {
		fixture := client.NewReplacementSessionTestFixture(sessionID, 1)
		return chat.ActiveChatOpen(
			fixture.Stopped,
			"hel",
			nil,
			fixture.Control,
			chat.SessionHeaderIdentity{},
			"",
			chat.NewNotices(),
		)
	}
	for _, width := range []int{140, 100, 80} {
		dashboard := dashboardWithSession(session)
		dashboard.focus = FocusPrompt
		chats := map[string]*chat.ActiveChat{sessionID: openChat()}
		buffer, _ := combinedDraw(width, 40, func(frame *chat.Frame) {
			renderCombinedForTest(frame, dashboard, chats, map[PaneId]string{}, false)
		})
		lines := bufferLines(buffer)
		footer := lines[len(lines)-1]
		groups := strings.Split(footer, chat.FooterGroupSeparator())
		if len(groups) < 2 {
			t.Fatalf("%d: no chord group in %q", width, footer)
		}
		label, found := strings.CutPrefix(strings.TrimLeft(groups[1], " "), "ctrl+b then ")
		if !found {
			t.Fatalf("%d: label overdrawn in %q", width, footer)
		}
		first, _, _ := strings.Cut(label, chat.FooterSeparator())
		chords := footerCommands(dashboard, FooterGroupChord)
		if !slices.ContainsFunc(chords, func(chord chat.FooterItem[CommandId]) bool {
			return chord.Text == strings.TrimRight(first, " ")
		}) {
			t.Fatalf("%d: %q is not a whole chord in %q", width, first, footer)
		}
	}

	dashboard := dashboardWithSession(session)
	dashboard.focus = FocusPrompt
	config := *dashboard.config.Get()
	config.Keys.Prefix = "ctrl+a"
	config.Keys.Help = core.BindingConfigFromMany("prefix+?", "ctrl+alt+p")
	dashboard.SetConfig(config)
	chats := map[string]*chat.ActiveChat{sessionID: openChat()}
	buffer, _ := combinedDraw(140, 40, func(frame *chat.Frame) {
		renderCombinedForTest(frame, dashboard, chats, map[PaneId]string{}, false)
	})
	lines := bufferLines(buffer)
	footer := lines[len(lines)-1]
	for _, want := range []string{"ctrl+a then", ": / ? palette", "ctrl+alt+p palette"} {
		if !strings.Contains(footer, want) {
			t.Fatalf("%q missing from %s", want, footer)
		}
	}

	chords := footerCommands(dashboard, FooterGroupChord)
	direct := directPaletteCommands(dashboard)
	directCommandIndex := len(chords)
	if direct[0].Key != CommandIdPalette {
		t.Fatalf("first direct command = %v", direct[0].Key)
	}
	index := slices.IndexFunc(chats[sessionID].FooterCommandAreas(), func(area chat.FooterCommandArea) bool {
		return area.Command == directCommandIndex && area.Text == "ctrl+alt+p palette"
	})
	if index < 0 {
		t.Fatal("direct palette footer is connected to the host command list")
	}
}

// Launch campaign finding A-13: in an empty pane the "Pin selected here"
// hint on the first inside row was drawn over the splash at 79 and 60
// columns. The splash keeps clear of that row, and is dropped when the pane
// is too short for both.
//
// Hard-won: 0f6de648aa: the pin hint overlaid the empty-pane splash.
func TestTheSplashKeepsClearOfThePinHintRow(t *testing.T) {
	for height := 3; height <= 16; height++ {
		buffer, _ := combinedDraw(34, height, func(frame *chat.Frame) {
			renderEmptyTranscript(
				frame,
				frame.Area(),
				EmptyConversationNoConversationOpen,
				core.DefaultSpinnerStyle(),
				nil,
				0,
				false,
				true,
			)
		})
		lines := bufferLines(buffer)
		if strings.Contains(lines[1], "M J O") || strings.Contains(lines[1], "Your next") {
			t.Fatalf("height %d: %#v", height, lines)
		}
		if height >= 8 && !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "M J O L N I R") }) {
			t.Fatalf("height %d: %#v", height, lines)
		}
	}
}

// Launch campaign finding A-15: the "Opening session" advice kept a
// hard-coded "·" in ASCII symbol mode.
//
// Hard-won: 987af2e4b4: an open transcript kept stale Unicode glyphs after symbol mode changed.
func TestTheOpeningAdviceUsesTheSymbolSetInForce(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	var lines []string
	chat.WithSymbols(core.SymbolSetAscii, func() {
		buffer, _ := combinedDraw(100, 6, func(frame *chat.Frame) {
			renderEmptyPromptAdvice(frame, frame.Area(), false, EmptyConversationOpening, dashboard)
		})
		lines = bufferLines(buffer)
	})
	text := strings.Join(lines, "\n")
	if !strings.Contains(text, "Esc cancels") {
		t.Fatal(text)
	}
	for _, character := range text {
		if character > 0x7f {
			t.Fatal(text)
		}
	}
}

func TestMinimizedSessionsCapContentLinesAtFourShortAndTenTall(t *testing.T) {
	for _, check := range []struct{ frame, content, want int }{
		{40, 0, 1},
		{40, 1, 1},
		{40, 3, 3},
		{40, 4, 4},
		{39, 6, 4},
		{39, 100, 4},
		{40, 15, 10},
		{100, 100, 10},
	} {
		if got := minimizedSessionRows(check.frame, check.content); got != check.want {
			t.Fatalf("minimizedSessionRows(%d, %d) = %d, want %d", check.frame, check.content, got, check.want)
		}
	}
}

// Hard-won: 7e71f7eccce4: focusing a mode-two pane changed the prompt split size.
func TestPromptTargetClampsBetweenTheMinimumAndAThird(t *testing.T) {
	// Below the minimum is raised to it; within range is kept; above a third
	// of the frame is capped there.
	if got := promptTarget(2, 60); got != promptMinimum {
		t.Fatalf("promptTarget(2, 60) = %d", got)
	}
	if got := promptTarget(5, 60); got != 5 {
		t.Fatalf("promptTarget(5, 60) = %d", got)
	}
	if got := promptTarget(50, 60); got != 20 {
		t.Fatalf("promptTarget(50, 60) = %d", got)
	}
}

// A Starting transition turns the prompt band into the standby composer —
// the real chat prompt: the draft is on screen, the cancel chord moved onto
// the pane's bottom border, and the old status panel is gone.
func TestAStartingTransitionDrawsTheStandbyComposerWithTheCancelChordAtItsBottom(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	for _, character := range "first message ahead" {
		dashboard.HandleKey(key(character))
	}
	buffer, _ := combinedDraw(100, 40, func(frame *chat.Frame) { combinedRender(frame, dashboard) })

	prompt, ok := dashboard.focusedPromptArea()
	if !ok {
		t.Fatal("prompt pane rendered")
	}
	lines := bufferLines(buffer)
	// The draft is editable text inside the pane...
	content := lines[prompt.Min.Y+1 : prompt.Max.Y-1]
	if !slices.ContainsFunc(content, func(line string) bool { return strings.Contains(line, "first message ahead") }) {
		t.Fatalf("draft missing from %#v", content)
	}
	// ...and the cancel chord sits on the pane's bottom border row.
	border := lines[prompt.Max.Y-1]
	if !strings.Contains(border, "ctrl+b shift+c to cancel starting") {
		t.Fatalf("cancel chord missing from %q", border)
	}
	if slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, " Status ") }) {
		t.Fatalf("the status panel should not replace the composer: %#v", lines)
	}
	if draft, ok := dashboard.TakeStandbyPromptDraft("session-1"); !ok || draft != "first message ahead" {
		t.Fatalf("standby draft = %q, %v", draft, ok)
	}
	// The keystrokes were input, not actions.
	if action := dashboard.HandleKey(key(tea.KeyEnter)); action != DashboardAction(DashboardActionNone{}) {
		t.Fatalf("Enter = %s", combinedActionDebug(action))
	}
}

// Hard-won: c90e988448: single-pane transitions confused border highlighting with caret focus.
func TestTransitionCaretFollowsKeyboardFocusWithoutAHighlightedPaneBorder(t *testing.T) {
	for _, kind := range []SessionOperationKind{
		SessionOperationKindLaunching,
		SessionOperationKindResuming,
		SessionOperationKindMoving,
	} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.BeginSessionOperation("session-1", kind, nil)
		for _, draft := range []struct {
			text   string
			column int
		}{{"", 0}, {"a界b", 3}} {
			for _, character := range draft.text {
				dashboard.HandleKey(key(character))
			}
			if draft.text != "" {
				dashboard.HandleKey(key(tea.KeyLeft))
			}
			for _, focused := range []bool{true, false} {
				if focused {
					dashboard.FocusPrompt()
				} else {
					dashboard.FocusSessions()
				}
				_, frame := combinedDraw(100, 40, func(frame *chat.Frame) { combinedRender(frame, dashboard) })
				cursor, shown := frame.CursorPosition()
				if shown != focused {
					t.Fatalf("%v, focus=%v: cursor shown = %v", kind, focused, shown)
				}
				if focused {
					prompt, _ := dashboard.focusedPromptArea()
					want := uv.Pos(prompt.Min.X+3+draft.column, prompt.Min.Y+1)
					if cursor != want {
						t.Fatalf("%v: cursor at %v, want %v", kind, cursor, want)
					}
				}
			}
			dashboard.FocusPrompt()
		}
	}
}

// Launch findings B-2 / D-1 for Suspending: the transition panel's title
// started under the pane chrome label, so the row read "Conversation g". It
// starts after the label, as the chat title does.
//
// Hard-won: 8bdf82815c: transition titles were covered by pane chrome.
func TestThePaneChromeDoesNotCoverTheTransitionTitle(t *testing.T) {
	for _, check := range []struct {
		kind SessionOperationKind
		word string
	}{
		{SessionOperationKindSuspending, "Suspending"},
		{SessionOperationKindLaunching, "Starting"},
	} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.BeginSessionOperation("session-1", check.kind, nil)
		for _, width := range []int{140, 100} {
			buffer, _ := combinedDraw(width, 40, func(frame *chat.Frame) { combinedRender(frame, dashboard) })
			lines := bufferLines(buffer)
			if !slices.ContainsFunc(lines, func(line string) bool {
				return strings.Contains(line, "Transition · "+check.word)
			}) {
				t.Fatalf("%d: %s title covered: %#v", width, check.word, lines)
			}
		}
	}
}

func TestSizedBandsEncodeEachStatesMinimumAndCap(t *testing.T) {
	if got := sizedBand(core.PaneSizeMinimized, 7, 40, 20); got != (PaneBand{minimum: 7, full: 7, cap: 7}) {
		t.Fatalf("minimized: %+v", got)
	}
	if got := sizedBand(core.PaneSizeStandard, 1, 40, 20); got != (PaneBand{minimum: 3, full: 40, cap: 20}) {
		t.Fatalf("standard: %+v", got)
	}
	if got := sizedBand(core.PaneSizeMaximized, 1, 40, 20); got != (PaneBand{minimum: 3, full: 40, cap: 40}) {
		t.Fatalf("maximized: %+v", got)
	}
}

func TestAllocationReportsTheDynamicStateMinimum(t *testing.T) {
	fixed := func(height int) PaneBand {
		return PaneBand{minimum: height, full: height, cap: height}
	}
	result := allocateCombinedHeights(
		12,
		fixed(4),
		fixed(1),
		fixed(1),
		fixed(1),
		3,
		[4]supportPaneSize{
			{SupportPaneSessions, core.PaneSizeMinimized},
			{SupportPaneTargets, core.PaneSizeMinimized},
			{SupportPaneQuota, core.PaneSizeMinimized},
			{SupportPaneNotifications, core.PaneSizeMinimized},
		},
	)
	if result != CombinedAllocation(CombinedAllocationTooSmall{RequiredFrameHeight: 14}) {
		t.Fatalf("allocation = %#v", result)
	}
}

func TestResizingSidebarRelayoutsSupportPanesWithoutResizingTerminal(t *testing.T) {
	for _, side := range []core.SessionsSide{core.SessionsSideLeft, core.SessionsSideRight} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.config.Mut().SessionsSide = side
		required := max(capacityTableWidth(dashboard), quotaPaneWidth(dashboard))
		width := -1
		for candidate := minimumTerminalWidth; candidate <= 480; candidate++ {
			if candidate-sessionsSidebarWidth(candidate, core.PaneSizeMinimized) >= required &&
				candidate-sessionsSidebarWidth(candidate, core.PaneSizeMaximized) < required {
				width = candidate
				break
			}
		}
		if width < 0 {
			t.Fatal("sidebar sizes straddle the support layout threshold")
		}
		for _, size := range []core.PaneSize{core.PaneSizeMaximized, core.PaneSizeMinimized, core.PaneSizeMaximized} {
			dashboard.SetPaneSize(SupportPaneSessions, size)
			combinedDraw(width, 40, func(frame *chat.Frame) { combinedRender(frame, dashboard) })
			areas := *dashboard.paneAreas
			sessions, targets, quota := areas[0], areas[1], areas[2]
			if targets.Min.X != quota.Min.X || targets.Dx() != quota.Dx() || targets.Max.Y != quota.Min.Y {
				t.Fatalf("%v %v: targets %v, quota %v", side, size, targets, quota)
			}
			if size == core.PaneSizeMinimized {
				wantX := 0
				if side == core.SessionsSideLeft {
					wantX = sessions.Max.X
				}
				if targets.Dx() != width-sessions.Dx() || sessions.Max.Y != 39 || targets.Min.X != wantX {
					t.Fatalf("%v minimized: sessions %v, targets %v", side, sessions, targets)
				}
			} else if targets.Dx() != width || targets.Min.Y != sessions.Max.Y {
				t.Fatalf("%v %v: sessions %v, targets %v", side, size, sessions, targets)
			}
		}
	}
}

func renderCombinedGolden(dashboard *DashboardState, width, height int) []string {
	buffer, _ := combinedDraw(width, height, func(frame *chat.Frame) { combinedRender(frame, dashboard) })
	lines := bufferLines(buffer)
	pinSpinnerGlyphs(lines)
	return lines
}

func appendCombinedGolden(output *strings.Builder, label string, width, height int, lines []string) {
	if output.Len() > 0 {
		output.WriteByte('\n')
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteByte('\n')
}

func combinedAppendPaneGeometry(t *testing.T, output *strings.Builder, dashboard *DashboardState) {
	t.Helper()
	if dashboard.paneAreas == nil {
		t.Fatal("rendered support panes")
	}
	areas := *dashboard.paneAreas
	transcript, ok := dashboard.focusedTranscriptArea()
	if !ok {
		t.Fatal("rendered transcript")
	}
	prompt, ok := dashboard.focusedPromptArea()
	if !ok {
		t.Fatal("rendered prompt")
	}
	if dashboard.notificationsArea == nil {
		t.Fatal("rendered notifications pane")
	}
	fmt.Fprintf(
		output,
		"layout: sessions=%s; transcript=%s; prompt=%s; targets=%s; quota=%s; notifications=%s\n",
		combinedRect(areas[0]), combinedRect(transcript), combinedRect(prompt),
		combinedRect(areas[1]), combinedRect(areas[2]), combinedRect(*dashboard.notificationsArea),
	)
}

func pinCombinedGoldenOperationClock(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	operation := (*dashboard.sessionOperations.Mut())["session-1"]
	if operation == nil {
		t.Fatal("golden session operation")
	}
	operation.startedAtEpochSeconds = ^uint64(0)
}

// combinedFormChat is notifications_pane's test helper form_chat: a warm
// chat whose session holds request as its open form.
func combinedFormChat(session string, request core.ElicitationRequest) *chat.ActiveChat {
	active, _ := formChat(session, request)
	return active
}

func TestGoldenCombinedDashboardRender(t *testing.T) {
	var output strings.Builder
	midsizeSupport := dashboardWithSession(runningSession())
	targets := make([]core.DeploymentCapacityTarget, 0, 12)
	for index := 1; index <= 12; index++ {
		host := fmt.Sprintf("host-%02d", index)
		target := testCapacityTarget()
		target.ID = host
		target.Host = host
		target.Local = false
		targets = append(targets, target)
	}
	midsizeSupport.setDeploymentCapacityTargets(targets)
	config := *midsizeSupport.config.Get()
	config.Profiles = maps.Clone(config.Profiles)
	for index := 1; index <= 12; index++ {
		profile := fmt.Sprintf("claude-%02d", index)
		config.Profiles[profile] = core.NewHarnessProfile(core.HarnessKindClaude, filepath.Join("/profiles", profile))
	}
	midsizeSupport.SetConfig(config)
	serveAttention(midsizeSupport, []string{"session-1"}, core.SessionAttentionNeedsInput)
	questionLines := make([]string, 0, 12)
	for index := 1; index <= 12; index++ {
		questionLines = append(questionLines, fmt.Sprintf("Question line %d", index))
	}
	questionText := strings.Join(questionLines, "\n")
	details := midsizeSupport.sessionDetails.Mut()
	if *details == nil {
		*details = map[string]*SessionDetail{}
	}
	if (*details)["session-1"] == nil {
		(*details)["session-1"] = &SessionDetail{}
	}
	(*details)["session-1"].lastAgentMessage = &questionText
	for _, pane := range []SupportPane{SupportPaneTargets, SupportPaneQuota, SupportPaneNotifications} {
		midsizeSupport.SetPaneSize(pane, core.PaneSizeStandard)
	}
	for _, check := range []struct {
		label  string
		height int
	}{
		{"midsize support panes show nine content rows", 100},
		{"short frame keeps notification text and reply controls", 31},
	} {
		lines := renderCombinedGolden(midsizeSupport, 160, check.height)
		appendCombinedGolden(&output, check.label, 160, check.height, lines)
		combinedAppendPaneGeometry(t, &output, midsizeSupport)
	}

	standby := dashboardWithSession(runningSession())
	standby.BeginSessionOperation("session-1", SessionOperationKindResuming, nil)
	pinCombinedGoldenOperationClock(t, standby)
	standby.FocusPrompt()
	lines := renderCombinedGolden(standby, 100, 40)
	appendCombinedGolden(&output, "empty standby composer", 100, 40, lines)
	combinedAppendPaneGeometry(t, &output, standby)

	split := dashboardWithSession(runningSession())
	second := runningSession()
	second.ID = "session-2"
	splitState := *split.state.Get()
	splitState.Sessions.Insert(second.ID, second)
	split.SetState(splitState)
	renderCombinedGolden(split, 160, 55)
	inactive := split.FocusedPane()
	secondID := "session-2"
	focused, ok := split.SplitFocusedPane(chat.DirectionHorizontal, &secondID)
	if !ok {
		t.Fatal("split the focused pane")
	}
	split.BeginSessionOperation("session-1", SessionOperationKindMoving, nil)
	split.BeginSessionOperation("session-2", SessionOperationKindResuming, nil)
	for _, operation := range *split.sessionOperations.Mut() {
		operation.startedAtEpochSeconds = ^uint64(0)
	}
	split.SeedStandbyPrompt("session-1", "queued before move")
	split.standbyPromptMut("session-1").HandleKey(key(tea.KeyEnter))
	split.SeedStandbyPrompt("session-1", "one\ntwo\nthree\nfour\nfive\nsix\nseven")
	split.SeedStandbyPrompt("session-2", "focused draft")
	split.FocusPrompt()
	buffer, frame := combinedDraw(160, 55, func(frame *chat.Frame) { combinedRender(frame, split) })
	inactiveTranscript, inactivePrompt, _ := split.paneBands(inactive)
	_, focusedPrompt, _ := split.paneBands(focused)
	wanted := split.standbyPromptMut("session-1").DesiredPromptHeight(inactivePrompt.Dx())
	if inactivePrompt.Dy() != promptTarget(wanted, inactivePrompt.Dy()+inactiveTranscript.Dy()) {
		t.Fatal("inactive Moving composer must determine the shared band's height")
	}
	cursor, _ := frame.CursorPosition()
	if !cursor.In(focusedPrompt) {
		t.Fatalf("cursor %v outside the focused prompt %v", cursor, focusedPrompt)
	}
	edge := buffer.CellAt(inactivePrompt.Min.X, inactivePrompt.Min.Y+1)
	if chat.CellStyle(edge).Fg != chat.Border(false).Fg {
		t.Fatal("inactive composer must not draw a focused border or cursor")
	}
	lines = renderCombinedGolden(split, 160, 55)
	appendCombinedGolden(
		&output,
		"split panes retain the inactive Moving draft and only focus the active composer",
		160,
		55,
		lines,
	)
	fmt.Fprintf(&output, "inactive prompt height: %d; desired: %d; focused cursor: Position { x: %d, y: %d }\n",
		inactivePrompt.Dy(), wanted, cursor.X, cursor.Y)

	suspending := dashboardWithSession(runningSession())
	suspending.BeginSessionOperation("session-1", SessionOperationKindSuspending, nil)
	pinCombinedGoldenOperationClock(t, suspending)
	lines = renderCombinedGolden(suspending, 100, 40)
	appendCombinedGolden(&output, "suspending status panel", 100, 40, lines)
	combinedAppendPaneGeometry(t, &output, suspending)

	draft := dashboardWithSession(runningSession())
	draft.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	pinCombinedGoldenOperationClock(t, draft)
	draft.SeedStandbyPrompt("session-1", "a\nb\nc\nd\ne\nf\ng")
	draft.FocusPrompt()
	launchDraft := draft.standbyPrompts["session-1"]
	if launchDraft == nil {
		t.Fatal("launch draft")
	}
	desired := launchDraft.DesiredPromptHeight(100)
	lines = renderCombinedGolden(draft, 100, 40)
	appendCombinedGolden(&output, "wrapped standby draft", 100, 40, lines)
	combinedAppendPaneGeometry(t, &output, draft)
	fmt.Fprintf(&output, "desired prompt height: %d\n", desired)
	draft.FinishSessionOperation("session-1")
	draft.BeginSessionOperation("session-1", SessionOperationKindSuspending, nil)
	pinCombinedGoldenOperationClock(t, draft)
	_, standbyDuringSuspension := draft.standbyPromptSession()
	fmt.Fprintf(&output, "standby prompt during suspension: %t\n", standbyDuringSuspension)

	priority := dashboardWithSession(runningSession())
	priority.SetPaneSize(SupportPaneSessions, core.PaneSizeStandard)
	priority.SetPaneSize(SupportPaneTargets, core.PaneSizeMaximized)
	priority.SetPaneSize(SupportPaneQuota, core.PaneSizeStandard)
	priority.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	pinCombinedGoldenOperationClock(t, priority)
	priority.SeedStandbyPrompt("session-1", "first\nsecond\nthird")
	priority.FocusPrompt()
	lines = renderCombinedGolden(priority, 100, 40)
	appendCombinedGolden(&output, "prompt then maximized then standard pane allocation", 100, 40, lines)
	combinedAppendPaneGeometry(t, &output, priority)
	band := func(full, cap int) PaneBand {
		return PaneBand{minimum: 3, full: full, cap: cap}
	}
	sizes := [4]supportPaneSize{
		{SupportPaneSessions, core.PaneSizeStandard},
		{SupportPaneTargets, core.PaneSizeMaximized},
		{SupportPaneQuota, core.PaneSizeStandard},
		{SupportPaneNotifications, core.PaneSizeStandard},
	}
	fits, ok := allocateCombinedHeights(40, band(12, 10), band(15, 15), band(8, 8), band(3, 3), 6, sizes).(CombinedAllocationFits)
	if !ok {
		t.Fatal("40 rows should fit")
	}
	heights := CombinedHeights(fits)
	fmt.Fprintf(&output, "layout allocation: prompt=%d; targets=%d; sessions=%d; quota=%d; transcript=%d\n",
		heights.prompt, heights.targets, heights.sessions, heights.quota, heights.transcript)

	for _, side := range []core.SessionsSide{core.SessionsSideLeft, core.SessionsSideRight} {
		for _, size := range []core.PaneSize{core.PaneSizeMinimized, core.PaneSizeStandard, core.PaneSizeMaximized} {
			dashboard := dashboardWithSession(runningSession())
			dashboard.config.Mut().SessionsSide = side
			dashboard.SetPaneSize(SupportPaneSessions, size)
			required := max(capacityTableWidth(dashboard), quotaPaneWidth(dashboard))
			threshold := -1
			for width := minimumTerminalWidth; width <= 480; width++ {
				if width-sessionsSidebarWidth(width, size) >= required {
					threshold = width
					break
				}
			}
			if threshold < 0 {
				t.Fatal("support panes fit in a wide terminal")
			}
			for _, width := range []int{max(threshold-1, minimumTerminalWidth), threshold, 80, 480} {
				lines := renderCombinedGolden(dashboard, width, 40)
				label := fmt.Sprintf("support panes side=%s sessions=%s threshold=%d",
					combinedSessionsSideDebug(side), combinedPaneSizeDebug(size), threshold)
				appendCombinedGolden(&output, label, width, 40, lines)
				combinedAppendPaneGeometry(t, &output, dashboard)
			}
		}
	}

	for _, frameSize := range [][2]int{{70, 25}, {80, 16}, {160, 16}} {
		width, height := frameSize[0], frameSize[1]
		dashboard := dashboardWithSession(runningSession())
		names := map[string]string{}
		for index := range 48 {
			names[fmt.Sprintf("workspace-%d", index)] = fmt.Sprintf("Project %02d", index)
		}
		dashboard.SetWorkspaceNames(names)
		active := "workspace-0"
		dashboard.SetActiveWorkspace(&active)
		lines := renderCombinedGolden(dashboard, width, height)
		if dashboard.workspacePaneArea == nil || dashboard.paneAreas == nil {
			t.Fatal("workspaces and support panes fit")
		}
		workspace := *dashboard.workspacePaneArea
		areas := *dashboard.paneAreas
		sessions, targets := areas[0], areas[1]
		if workspace.Dy() < paneMinimum || sessions.Dy() < paneMinimum || workspace.Max.Y != sessions.Min.Y {
			t.Fatalf("%dx%d: workspace %v, sessions %v", width, height, workspace, sessions)
		}
		if width == minimumTerminalWidth && targets.Dx() != width {
			t.Fatal("exercise support panes below the sidebar")
		}
		if width >= minimumTerminalWidth && targets.Dx() == width && sessions.Max.Y != targets.Min.Y {
			t.Fatalf("%dx%d: sessions %v, targets %v", width, height, sessions, targets)
		}
		if _, ok := dashboard.workspaceScrollbar.Geometry(); !ok {
			t.Fatalf("%dx%d: no workspace scrollbar", width, height)
		}
		// Hard-won: 1b13e4a69: workspace rows overflowed the allocated sidebar band on short terminals.
		appendCombinedGolden(&output, "short terminal scrolls workspaces within its sidebar band", width, height, lines)
		combinedAppendPaneGeometry(t, &output, dashboard)
	}

	session := runningSession()
	session.ACPSessionTitle = nil
	checkout := "/actual/checkout"
	session.ProjectDirectory = &checkout
	sessionID := session.ID
	goDashboard := dashboardWithSession(session)
	workspaceID := core.DefaultWorkspaceID
	bundle := "hel"
	createWorktree := false
	mode := GoMode{
		WorkspaceID:   &workspaceID,
		LastSessionID: nil,
		Directory:     "/projects/current",
		SaveAsDefault: false,
		Simple:        false,
		Recipe: &core.GoRecipe{
			ProfileID:             "codex-1",
			TargetID:              "podman",
			BundleID:              &bundle,
			ProjectDirectory:      nil,
			CreateManagedWorktree: &createWorktree,
			Subagents:             &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir},
			AdditionalMounts:      nil,
			ResourceAllocation:    nil,
		},
	}
	var startup *core.SessionRecord
	for _, record := range goDashboard.state.Get().Sessions.All() {
		copied := *record
		startup = &copied
		break
	}
	goDashboard.BeginGo(mode, false, startup)
	goDashboard.SetGoContext(sessionID, client.DaemonResult[client.DaemonPair[string, string]]{
		Ok: client.DaemonPair[string, string]{First: "/actual/checkout", Second: "feature-x"},
	})
	buffer, _ = combinedDraw(140, 45, func(frame *chat.Frame) { combinedRender(frame, goDashboard) })
	lines = bufferLines(buffer)
	appendCombinedGolden(&output, "selected checkout and branch", 140, 45, lines)
	newRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, " New ") })
	if newRow < 0 {
		t.Fatal("New action in workspace banner")
	}
	column := cellColumn(lines[newRow], " New ") + 1
	goDashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, uv.Pos(column, newRow)))
	action := goDashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, uv.Pos(column, newRow)))
	fmt.Fprintf(&output, "action: %s\n", combinedActionDebug(action))

	failedSession := runningSession()
	failedSession.State = core.SessionStateDestroying
	failure := "Podman exited before the workspace was removed"
	failedSession.LastError = &failure
	failedSessionID := failedSession.ID
	failed := dashboardWithSession(failedSession)
	var attention core.SnapshotMap[string, core.SessionAttention]
	attention.Insert(failedSessionID, core.SessionAttentionFailed)
	failed.SetSessionAttention(attention)
	// The shared fixture resets updated_at, so pin the clock after building
	// the dashboard.
	state := failed.state.Mut()
	record := state.Sessions.GetMut(failedSessionID)
	if record == nil {
		t.Fatal("failed destroy session remains in the dashboard")
	}
	record.UpdatedAt = "2099-01-01T00:00:00Z"
	lines = renderCombinedGolden(failed, 100, 40)
	appendCombinedGolden(&output, "failed destroy transition and recovery guidance", 100, 40, lines)

	inline := dashboardWithSession(runningSession())
	serveAttention(inline, []string{"session-1"}, core.SessionAttentionNeedsInput)
	request := question("session-1")
	request.Fields[0].Title = "Which deployment path should be used?"
	description := "Keep the question visible on short terminals."
	request.Fields[0].Description = &description
	inlineDetails := *inline.sessionDetails.Mut()
	inlineDetails["session-1"].pendingElicitations = []core.ElicitationRequest{request}
	serveFormCard(inline, "session-1", nil, &request)
	zero := uint64(0)
	(*inline.sessionDetails.Mut())["session-1"].lastActivityAtMs = &zero
	inline.notifications.size = core.PaneSizeMaximized
	inline.focus = FocusNotifications
	for _, check := range []struct {
		label string
		err   *string
	}{
		{"notification form attaches in the background", nil},
		{"notification form attachment fails visibly", func() *string { text := "Worker unavailable"; return &text }()},
	} {
		inline.SetNotificationFormError("session-1", check.err)
		lines := renderCombinedGolden(inline, 120, 44)
		appendCombinedGolden(&output, check.label, 120, 44, lines)
		// Hard-won: b817d9202: compact loading/error cards reserved a hidden reply row and lost their status.
		entries := inline.notificationEntries()
		text := inline.selectedNotificationText(&entries[0])
		buffer, _ := combinedDraw(120, 8, func(frame *chat.Frame) {
			notificationsPaneRender(frame, frame.Area(), inline, entries, text, nil)
		})
		lines = bufferLines(buffer)
		want := "Loading form"
		if check.err != nil {
			want = "Could not load form"
		}
		if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, want) }) {
			t.Fatalf("%q missing from %#v", want, lines)
		}
		appendCombinedGolden(&output, "compact form attachment status", 120, 8, lines)
	}
	inline.SetNotificationFormError("session-1", nil)
	chats := map[string]*chat.ActiveChat{"session-1": combinedFormChat("session-1", request)}
	drawInline := func(height int) []string {
		var buffer *uv.Buffer
		withNoColorOverrideForTest(true, func() []string {
			buffer, _ = combinedDraw(120, height, func(frame *chat.Frame) {
				renderCombinedForTest(frame, inline, chats, map[PaneId]string{}, false)
			})
			return nil
		})
		lines := bufferLines(buffer)
		pinSpinnerGlyphs(lines)
		return lines
	}
	lines = drawInline(44)
	appendCombinedGolden(&output, "notification and conversation share an inline form", 120, 44, lines)

	// Hard-won: 1f62bc101: a dismissal failure was hidden when the form replaced the card text.
	var cards core.SnapshotMap[string, client.NotificationCard]
	cards.Insert("session-1", client.NotificationCard{
		SinceMs: 1,
		Badge:   core.NotificationKindNeedsInput,
		Entries: []core.NotificationStackEntry{{
			CompletedAtMs:    1,
			CompletedOrdinal: 1,
			Kind:             core.NotificationKindImportant,
			Message:          core.EntryMessageNone{},
		}},
		Dismissal: core.NotificationDismissalStack{Revision: 1},
		Holder:    nil,
		Form: &client.CardForm{
			Source:     client.CardFormSourceElicitation,
			RaisedAtMs: 2,
			RequestID:  "mixed-question",
			Message:    "Choose a path",
		},
	})
	inline.SetNotificationCards(&cards)
	if _, ok := inline.HandleKey(altKey('n')).(DashboardActionDismissNotification); !ok {
		t.Fatal("Alt-n dismisses the notification")
	}
	inline.NotificationsDismissed([]NotificationDismissalResult{{
		SessionID: "session-1",
		Target:    core.NotificationDismissalStack{Revision: 1},
		Err:       fmt.Errorf("permission denied"),
	}})
	buffer, _ = combinedDraw(120, 44, func(frame *chat.Frame) {
		renderCombinedForTest(frame, inline, chats, map[PaneId]string{}, false)
	})
	lines = bufferLines(buffer)
	pinSpinnerGlyphs(lines)
	for _, want := range []string{"Could not clear: permission denied", "Which deployment path should be used?"} {
		if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, want) }) {
			t.Fatalf("%q missing from %#v", want, lines)
		}
	}
	appendCombinedGolden(&output, "inline form keeps dismissal failure visible", 120, 44, lines)
	serveFormCard(inline, "session-1", nil, &request)
	clear(inline.notifications.errors)
	inline.ClearNotice()

	inline.notifications.size = core.PaneSizeStandard
	for _, height := range []int{21, 25, 31, 39} {
		lines := drawInline(height)
		card := *inline.notificationsArea
		if !slices.ContainsFunc(lines[card.Min.Y:card.Max.Y], func(line string) bool {
			return strings.Contains(line, "Which deployment path should be used?")
		}) {
			t.Fatalf("height %d: question missing from the card", height)
		}
		// Hard-won: dc29e81c6: maximize controls ignored the live form reservation and offered a size that added no rows.
		if height == 21 && (inline.paneMaximizeEnabled(SupportPaneQuota) || inline.paneMaximizeEnabled(SupportPaneNotifications)) {
			t.Fatal("maximize offered a size that adds no rows")
		}
		// Hard-won: 4c23909e0: attaching a form replaced an otherwise usable short dashboard with the resize screen.
		if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "Workspaces") }) {
			t.Fatalf("height %d: the dashboard was replaced: %#v", height, lines)
		}
		label := "short notification form keeps the dashboard usable"
		if height >= 31 {
			label = "standard notification form keeps its question readable"
		}
		appendCombinedGolden(&output, label, 120, height, lines)
	}

	// Hard-won: 1f62bc101: minimized maximize probes measured only the short published message.
	choice := question("session-1")
	choice.Fields[0].Title = "Choose a deployment target"
	options := make([]core.ElicitationOption, 0, 20)
	for index := range 20 {
		review := "Review this target before deploying"
		options = append(options, core.ElicitationOption{
			Value:       fmt.Sprint(index),
			Title:       fmt.Sprintf("Deployment target %d", index),
			Description: &review,
			Preview:     nil,
		})
	}
	choice.Fields[0].Kind = core.ElicitationFieldKindSingleSelect{Options: options, Default: nil}
	chats["session-1"] = combinedFormChat("session-1", choice)
	(*inline.sessionDetails.Mut())["session-1"].pendingElicitations = []core.ElicitationRequest{choice}
	serveFormCard(inline, "session-1", nil, &choice)
	inline.notifications.size = core.PaneSizeMinimized
	buffer, _ = combinedDraw(120, 44, func(frame *chat.Frame) {
		renderCombinedForTest(frame, inline, chats, map[PaneId]string{}, false)
	})
	if inline.notificationsArea.Dy() != 1 {
		t.Fatalf("minimized notifications height %d", inline.notificationsArea.Dy())
	}
	if !inline.paneMaximizeEnabled(SupportPaneNotifications) {
		t.Fatal("the minimized form can be maximized")
	}
	lines = bufferLines(buffer)
	pinSpinnerGlyphs(lines)
	appendCombinedGolden(&output, "minimized notification measures its warm expanded form", 120, 44, lines)
	inline.SetPaneSize(SupportPaneNotifications, core.PaneSizeStandard)
	if !inline.NotificationInputNeedsFrame() {
		t.Fatal("a resized form needs a frame")
	}
	combinedDraw(120, 44, func(frame *chat.Frame) {
		renderCombinedForTest(frame, inline, chats, map[PaneId]string{}, false)
	})
	if inline.NotificationInputNeedsFrame() {
		t.Fatal("the frame settled the form's input")
	}
	standardHeight := inline.notificationsArea.Dy()
	inline.SetPaneSize(SupportPaneNotifications, core.PaneSizeMaximized)
	combinedDraw(120, 44, func(frame *chat.Frame) {
		renderCombinedForTest(frame, inline, chats, map[PaneId]string{}, false)
	})
	if inline.notificationsArea.Dy() <= standardHeight {
		t.Fatalf("maximized %d is not taller than standard %d", inline.notificationsArea.Dy(), standardHeight)
	}

	core.AssertGolden(t, "testdata", "combined-dashboard-render", output.String())
}

// combinedSessionsSideDebug is Rust's Debug of a SessionsSide.
func combinedSessionsSideDebug(side core.SessionsSide) string {
	if side == core.SessionsSideRight {
		return "Right"
	}
	return "Left"
}

// combinedPaneSizeDebug is Rust's Debug of a PaneSize.
func combinedPaneSizeDebug(size core.PaneSize) string {
	switch size {
	case core.PaneSizeMinimized:
		return "Minimized"
	case core.PaneSizeMaximized:
		return "Maximized"
	default:
		return "Standard"
	}
}
