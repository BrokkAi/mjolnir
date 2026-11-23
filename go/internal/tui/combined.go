// Port of mj-tui/src/combined.rs.
//
// The combined conversation surface.
//
// One screen holds all of Hel's terminal UI: a Sessions pane, the
// transcript of the conversation on screen, the Prompt composer, and Targets
// and Quota summaries under it, with a shared one-row footer. There is no
// second screen to switch to, so nothing is ever hidden behind a navigation
// step.
//
// Rust's fixed `[(SupportPane, T); 4]` arrays are arrays of the small pair
// types below, in the order Sessions, Targets, Quota, Notifications.
package tui

import (
	"math"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	// footerHeight is the rows the footer always keeps.
	footerHeight = 1
	// transcriptMinimum is the fewest rows the transcript is worth drawing
	// in.
	transcriptMinimum = 3
	// promptMinimum is a bordered composer with one row of text.
	promptMinimum = 3
	// emptyPromptHeight is a border plus the two lines of create-or-resume
	// guidance.
	emptyPromptHeight = 4
	// paneMinimum is a bordered pane with one row of content.
	paneMinimum = 3
	// combinedSummaryRow is the one-row form Targets and Quota collapse to
	// (Rust's SUMMARY_ROW, which shares its Go name with render/quotas'
	// summary_row).
	combinedSummaryRow = 1
	// standardSupportPaneHeight is the upper bound for midsize support
	// panes: nine content rows and two border rows.
	standardSupportPaneHeight = 11
)

// tallTerminalHeight is the terminal height at or above which the minimized
// Sessions list gets its taller five-row cap; below it the list is capped at
// two rows. Compact summaries occupy two lines, so the returned value is a
// content height.
const tallTerminalHeight = 40

// sessionsSidebarWidth is the width of the Sessions sidebar for an explicit
// pane size. The final half width bound keeps the conversation visible on
// narrow terminals even when a clamped size would otherwise be wider than
// the available screen.
func sessionsSidebarWidth(width int, size core.PaneSize) int {
	var sidebar int
	switch size {
	case core.PaneSizeMinimized:
		sidebar = 20
	case core.PaneSizeStandard:
		sidebar = min(max(width/3, 40), 80)
	case core.PaneSizeMaximized:
		sidebar = min(max(width/2, 40), 100)
	}
	return min(sidebar, width/2)
}

// minimizedSessionRows is how many content lines the minimized Sessions list
// can show. Short and tall terminals retain the existing two- and five-entry
// caps respectively; each compact entry now occupies two lines.
func minimizedSessionRows(frameHeight int, contentHeight int) int {
	limit := 4
	if frameHeight >= tallTerminalHeight {
		limit = 10
	}
	return min(max(contentHeight, 1), limit)
}

// promptTarget is the height the composer settles at: its desired height,
// but never below [promptMinimum] and never above a third of the frame.
func promptTarget(desiredPrompt int, frameHeight int) int {
	ceiling := max(frameHeight/3, promptMinimum)
	return min(max(desiredPrompt, promptMinimum), ceiling)
}

// PaneBand is how tall one band wants to be and how short it may get.
type PaneBand struct {
	minimum int
	full    int
	cap     int
}

// PaneDimensions is one support pane's minimized height, its full height,
// and its Standard cap.
type PaneDimensions struct {
	minimized   int
	full        int
	standardCap int
}

// CombinedHeights is the frame divided between the seven bands.
type CombinedHeights struct {
	sessions      int
	transcript    int
	prompt        int
	targets       int
	quota         int
	notifications int
	footer        int
}

func (h CombinedHeights) pane(pane SupportPane) int {
	switch pane {
	case SupportPaneSessions:
		return h.sessions
	case SupportPaneTargets:
		return h.targets
	case SupportPaneQuota:
		return h.quota
	default:
		return h.notifications
	}
}

// CombinedAllocation is either the bands' heights or the frame height the
// bands need.
type CombinedAllocation interface {
	isCombinedAllocation()
}

// CombinedAllocationFits is the bands' heights for a frame that holds them.
type CombinedAllocationFits CombinedHeights

// CombinedAllocationTooSmall is the frame height the bands' minimums need.
type CombinedAllocationTooSmall struct {
	RequiredFrameHeight int
}

func (CombinedAllocationFits) isCombinedAllocation()     {}
func (CombinedAllocationTooSmall) isCombinedAllocation() {}

// supportPaneSize is one `(SupportPane, PaneSize)` pair.
type supportPaneSize struct {
	pane SupportPane
	size core.PaneSize
}

// supportPaneDimensions is one `(SupportPane, PaneDimensions)` pair.
type supportPaneDimensions struct {
	pane       SupportPane
	dimensions PaneDimensions
}

// supportPaneEnabled is one `(SupportPane, bool)` pair: whether maximizing
// the pane would give it more rows.
type supportPaneEnabled struct {
	pane    SupportPane
	enabled bool
}

// allocateCombinedHeights divides the frame between the seven bands.
//
// Every band starts at its minimum, and the surplus is then spent in a
// fixed order: the composer first, because it is where the user is typing;
// then workspace growth in the stacked Sessions band; then the one maximized
// pane; then Standard Sessions, Targets, Quota and Notifications in screen
// order. The transcript takes whatever is left. Focus never participates.
func allocateCombinedHeights(frameHeight int, sessions, targets, quota, notifications PaneBand, desiredPrompt int, sizes [4]supportPaneSize) CombinedAllocation {
	panic("unported: mj-tui/src/combined.rs allocate_combined_heights")
}

// supportBands builds each support pane's band from its size. The live
// layout and maximize probes use the same form-height reservation.
func supportBands(dimensions [4]supportPaneDimensions, sizes [4]supportPaneSize, notificationMinimum *int) [4]PaneBand {
	panic("unported: mj-tui/src/combined.rs support_bands")
}

// maximizedPaneIsEffective says whether making each pane Maximized would
// give it more rows than Standard at this frame size. Maximized is
// exclusive, so the hypothetical maximum demotes any other maximum before
// running the same allocator used to draw the frame.
func maximizedPaneIsEffective(frameHeight int, dimensions [4]supportPaneDimensions, sessions PaneBand, desiredPrompt int, sizes [4]supportPaneSize, notificationMinimum *int) [4]supportPaneEnabled {
	panic("unported: mj-tui/src/combined.rs maximized_pane_is_effective")
}

func sizesWithPaneSize(sizes [4]supportPaneSize, pane SupportPane, size core.PaneSize) [4]supportPaneSize {
	adjusted := sizes
	if size == core.PaneSizeMaximized {
		for index := range adjusted {
			if adjusted[index].pane != pane && adjusted[index].size == core.PaneSizeMaximized {
				adjusted[index].size = core.PaneSizeStandard
			}
		}
	}
	for index := range adjusted {
		if adjusted[index].pane == pane {
			adjusted[index].size = size
		}
	}
	return adjusted
}

func sizedBand(size core.PaneSize, minimizedHeight int, full int, standardCap int) PaneBand {
	switch size {
	case core.PaneSizeMinimized:
		return PaneBand{minimum: minimizedHeight, full: minimizedHeight, cap: minimizedHeight}
	case core.PaneSizeStandard:
		return PaneBand{minimum: paneMinimum, full: full, cap: max(standardCap, paneMinimum)}
	default:
		return PaneBand{minimum: paneMinimum, full: full, cap: max(full, paneMinimum)}
	}
}

func supportPanesFit(contentWidth int, dashboard *DashboardState) bool {
	panic("unported: mj-tui/src/combined.rs support_panes_fit")
}

// RenderCombined draws the whole combined surface: Sessions, the
// conversation, Prompt, Targets, Quota, the footer, and any modal over the
// top.
//
// chats holds every warm conversation, keyed by session id; the panes of
// the conversation layout pick the ones on screen out of it. openingPanes
// says which pane is waiting for which session's attach, so a pane draws
// nothing for a session it has not finished opening. transcriptSelected
// says the selection engine still owns a selection on the transcript, so its
// row space has to stay frozen for this frame.
//
// It reports the sessions whose conversations this frame actually drew,
// which is what the read receipts follow.
func RenderCombined(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, openingPanes map[PaneId]string, transcriptSelected bool) []string {
	return RenderCombinedWithChatAccess(frame, dashboard, chats, openingPanes, transcriptSelected, func(PaneId, string) bool { return true })
}

// RenderCombinedWithChatAccess draws conversations only when their
// attachment owner grants the pane access. Warm chats can still serve
// Notifications while a conversation is cancelled.
func RenderCombinedWithChatAccess(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, openingPanes map[PaneId]string, transcriptSelected bool, canShow func(PaneId, string) bool) []string {
	panic("unported: mj-tui/src/combined.rs render_combined_with_chat_access")
}

// ConversationAccess is which pane is opening which session, and whether
// the attachment owner lets a pane show a session's conversation.
type ConversationAccess struct {
	opening map[PaneId]string
	canShow func(PaneId, string) bool
}

// RenderCombinedWithTheme draws the combined surface with a theme already
// selected by the caller.
//
// This is useful for deterministic captures and tests. Interactive callers
// should use [RenderCombined] so NO_COLOR can override configuration.
func RenderCombinedWithTheme(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, openingPanes map[PaneId]string, transcriptSelected bool, selectedTheme core.UiTheme) []string {
	panic("unported: mj-tui/src/combined.rs render_combined_with_theme")
}

func renderCombinedWithThemeAndSymbols(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, access ConversationAccess, transcriptSelected bool, selectedTheme core.UiTheme, symbols core.SymbolSet) []string {
	panic("unported: mj-tui/src/combined.rs render_combined_with_theme_and_symbols")
}

func renderCombinedThemed(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, access ConversationAccess, transcriptSelected bool) []string {
	panic("unported: mj-tui/src/combined.rs render_combined_themed")
}

// titleControls is the columns a pane's title has to leave clear for the
// close chip.
func titleControls(closeChip bool) int {
	if closeChip {
		return paneCloseControlReserve
	}
	return 0
}

// zoomTitleControls is the further columns the zoom chip takes, left of the
// close chip.
func zoomTitleControls(zoomChip bool) int {
	if zoomChip {
		return paneZoomControlReserve
	}
	return 0
}

// leafBands is how one pane's leaf divides into a transcript and a
// composer. The composer takes what its draft asks for, within the same
// bounds the single band uses: never under [promptMinimum] and never over a
// third of the pane.
func leafBands(rect uv.Rectangle, desiredPrompt int) (uv.Rectangle, uv.Rectangle) {
	prompt := min(promptTarget(desiredPrompt, rect.Dy()), rect.Dy())
	transcriptHeight := max(rect.Dy()-prompt, 0)
	return uv.Rect(rect.Min.X, rect.Min.Y, rect.Dx(), transcriptHeight),
		uv.Rect(rect.Min.X, rect.Min.Y+transcriptHeight, rect.Dx(), prompt)
}

// chatShowsInPane reports whether a pane shows the conversation it holds,
// or hides it because the pane is still attaching to a different session.
// opening is nil when the pane is not attaching.
func chatShowsInPane(opening *string, sessionID string) bool {
	return opening == nil || *opening == sessionID
}

// focusedChatOnScreen is the session the focused pane's conversation is
// drawn for, if it has one on screen. A conversation stays off screen while
// its pane is attaching to another session, while the session it belongs to
// is in a transition, and while the Sessions selection names a different
// row: the transcript must never belong to a row other than the highlighted
// one.
func focusedChatOnScreen(dashboard *DashboardState, chats map[string]*chat.ActiveChat, access ConversationAccess) (string, bool) {
	panic("unported: mj-tui/src/combined.rs focused_chat_on_screen")
}

// EmptyConversation is why the conversation band is empty, which is what
// decides the advice it gives.
type EmptyConversation int

const (
	// EmptyConversationNoLiveSession: the workspace has no live session at
	// all.
	EmptyConversationNoLiveSession EmptyConversation = iota
	// EmptyConversationNoConversationOpen: the workspace has live sessions
	// and none of them is open.
	EmptyConversationNoConversationOpen
	// EmptyConversationOpening: an attach is in flight, so a conversation is
	// on its way.
	EmptyConversationOpening
	// EmptyConversationFailed: the selected session's target failed:
	// nothing to attach to until it is recovered or its transcript is opened
	// on purpose.
	EmptyConversationFailed
)

// renderEmptyTranscript draws the bordered chrome that stands in for a
// conversation when none is on screen. The three reasons for an empty band
// need different advice: a workspace with no live session needs one created
// or resumed, a workspace that has live sessions just needs one opened, and
// an attach that is still running needs nothing but a moment. Telling the
// second user there is no live session would be a plain lie — the pane
// above is listing them.
//
// It returns the failure text's area when it draws the failed state, nil
// otherwise: a failed session has no chat to register a transcript surface,
// so the caller registers this one to keep the error selectable for a bug
// report.
func renderEmptyTranscript(frame *chat.Frame, transcriptArea uv.Rectangle, reason EmptyConversation, spinnerStyle chat.SpinnerStyle, failure *string, titleControls int, paneFocused bool, pinHint bool) *uv.Rectangle {
	panic("unported: mj-tui/src/combined.rs render_empty_transcript")
}

// pushFailureSurface registers a failed session's error text so a drag
// copies it like any other pane's text. body is nil when there is none.
func pushFailureSurface(dashboard *DashboardState, paneID PaneId, body *uv.Rectangle) {
	panic("unported: mj-tui/src/combined.rs push_failure_surface")
}

// renderEmptyPromptAdvice draws the prompt-band advice that accompanies
// [renderEmptyTranscript] for the two reasons where there is nothing to
// type toward yet. While an attach runs the standby composer fills the band
// instead, so Opening never reaches here.
func renderEmptyPromptAdvice(frame *chat.Frame, promptArea uv.Rectangle, promptFocused bool, reason EmptyConversation, dashboard *DashboardState) {
	panic("unported: mj-tui/src/combined.rs render_empty_prompt_advice")
}

// drawStandbyPrompt draws the session's standby composer — the real chat
// prompt — into the prompt band, and merges its surfaces into the
// dashboard's so clicks and selections inside the band behave like an
// attached chat's. note adds a left-aligned line to the band's bottom
// border; nil adds none.
func drawStandbyPrompt(frame *chat.Frame, promptArea uv.Rectangle, dashboard *DashboardState, sessionID string, paneID PaneId, note *chat.Line) {
	panic("unported: mj-tui/src/combined.rs draw_standby_prompt")
}

// drawStandbyBand draws one standby composer into the prompt band and
// reports its surfaces, shared by the per-session standby and the launch
// standby.
func drawStandbyBand(frame *chat.Frame, promptArea uv.Rectangle, standby *chat.ChatState, focused bool, note *chat.Line) chat.FrameSurfaces {
	standby.DrawPromptBand(frame, promptArea, focused, note)
	surfaces := chat.NewFrameSurfaces()
	surfaces.Append(standby.FrameSurfaces())
	return surfaces
}

// TransitionSurface is what one pane's transition panel reports: the
// operation under way, whether it failed, which pane holds it, and whether
// to highlight its border.
//
// The panel is the conversation replacement shown while a lifecycle owns
// the selected session. The warm chat remains alive off-screen so its
// draft, read cursor, and history survive the operation, but the transcript
// cannot be mistaken for a session that is being retired. Starting,
// Resuming, and Moving keep the real composer on screen: the standby prompt
// below the transition panel is the chat's own composer, so the first
// message can be drafted with the usual readline keys before the session is
// live (Enter queues it for the destination), and the draft carries into
// the conversation when the chat opens. Retiring and failed transitions have
// no conversation to type toward, so they keep the plain status panel.
type TransitionSurface struct {
	transition      core.SessionTransitionKind
	failed          bool
	paneID          PaneId
	highlightBorder bool
	// titleLead is the columns the pane chrome draws its label into at the
	// left of the title row; the title starts after them.
	titleLead int
}

// transitionTitle is a transition panel title that starts after the pane
// chrome label, the same way the conversation title does, so the label does
// not cover the state word (launch findings B-2 and D-1: "Conversation g"
// for Suspending).
func transitionTitle(transition core.SessionTransitionKind, lead int) string {
	return strings.Repeat(" ", lead) + " Transition · " + transition.Label() + " "
}

func renderTransitionSurface(frame *chat.Frame, transcriptArea uv.Rectangle, promptArea uv.Rectangle, dashboard *DashboardState, sessionID string, surface TransitionSurface) {
	panic("unported: mj-tui/src/combined.rs render_transition_surface")
}

// tableHeight is a bordered table with a header row and rows data rows.
func tableHeight(rows int) int {
	return min(min(rows, math.MaxUint16)+paneMinimum, math.MaxUint16)
}
