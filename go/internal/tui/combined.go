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
	"cmp"
	"fmt"
	"math"
	"slices"
	"strings"
	"time"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
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
	otherMinimum := sessions.minimum + transcriptMinimum + promptMinimum +
		targets.minimum + quota.minimum + footerHeight
	// A form's readable height is a preference above the notification
	// band's structural minimum. Spend only the space this frame has left;
	// a newly attached question must not turn a usable dashboard into a
	// resize screen. Minimized bands retain their existing one-row minimum.
	notificationFloor := notifications.minimum
	if sizes[3].size != core.PaneSizeMinimized {
		notificationFloor = min(notifications.minimum, paneMinimum)
	}
	notificationMinimum := min(
		notifications.minimum,
		max(max(frameHeight-otherMinimum, 0), notificationFloor),
	)
	required := otherMinimum + notificationMinimum
	if frameHeight < required {
		return CombinedAllocationTooSmall{RequiredFrameHeight: required}
	}
	heights := CombinedHeights{
		sessions:      sessions.minimum,
		transcript:    transcriptMinimum,
		prompt:        promptMinimum,
		targets:       targets.minimum,
		quota:         quota.minimum,
		notifications: notificationMinimum,
		footer:        footerHeight,
	}
	surplus := max(frameHeight-required, 0)
	grow := func(current *int, want int) {
		step := min(surplus, max(want-*current, 0))
		*current += step
		surplus -= step
	}
	grow(&heights.prompt, promptTarget(desiredPrompt, frameHeight))
	if sizes[0].size == core.PaneSizeMinimized {
		// Compact session rows stay fixed; only the workspace viewport can
		// grow beyond the stacked band's minimum.
		grow(&heights.sessions, min(sessions.full, sessions.cap))
	}
	growPane := func(pane SupportPane) {
		switch pane {
		case SupportPaneSessions:
			grow(&heights.sessions, min(sessions.full, sessions.cap))
		case SupportPaneTargets:
			grow(&heights.targets, min(targets.full, targets.cap))
		case SupportPaneQuota:
			grow(&heights.quota, min(quota.full, quota.cap))
		case SupportPaneNotifications:
			grow(&heights.notifications, min(notifications.full, notifications.cap))
		}
	}
	for _, size := range sizes {
		if size.size == core.PaneSizeMaximized {
			growPane(size.pane)
			break
		}
	}
	for _, size := range sizes {
		if size.size == core.PaneSizeStandard {
			growPane(size.pane)
		}
	}
	heights.transcript += surplus
	return CombinedAllocationFits(heights)
}

// supportBands builds each support pane's band from its size. The live
// layout and maximize probes use the same form-height reservation.
func supportBands(dimensions [4]supportPaneDimensions, sizes [4]supportPaneSize, notificationMinimum *int) [4]PaneBand {
	var bands [4]PaneBand
	for index := range bands {
		pane := dimensions[index].dimensions
		band := sizedBand(sizes[index].size, pane.minimized, pane.full, pane.standardCap)
		if sizes[index].pane == SupportPaneNotifications &&
			sizes[index].size != core.PaneSizeMinimized &&
			notificationMinimum != nil {
			band.minimum = *notificationMinimum
		}
		bands[index] = band
	}
	return bands
}

// maximizedPaneIsEffective says whether making each pane Maximized would
// give it more rows than Standard at this frame size. Maximized is
// exclusive, so the hypothetical maximum demotes any other maximum before
// running the same allocator used to draw the frame.
func maximizedPaneIsEffective(frameHeight int, dimensions [4]supportPaneDimensions, sessions PaneBand, desiredPrompt int, sizes [4]supportPaneSize, notificationMinimum *int) [4]supportPaneEnabled {
	var enabled [4]supportPaneEnabled
	for index := range enabled {
		pane := dimensions[index].pane
		standardSizes := sizesWithPaneSize(sizes, pane, core.PaneSizeStandard)
		maximumSizes := sizesWithPaneSize(sizes, pane, core.PaneSizeMaximized)
		standardBands := supportBands(dimensions, standardSizes, notificationMinimum)
		maximumBands := supportBands(dimensions, maximumSizes, notificationMinimum)
		standard, standardFits := allocateCombinedHeights(
			frameHeight, sessions, standardBands[1], standardBands[2], standardBands[3], desiredPrompt, standardSizes,
		).(CombinedAllocationFits)
		maximum, maximumFits := allocateCombinedHeights(
			frameHeight, sessions, maximumBands[1], maximumBands[2], maximumBands[3], desiredPrompt, maximumSizes,
		).(CombinedAllocationFits)
		// Both allocations use the same minimum requirements, so a misfit is
		// only defensive if that invariant changes later.
		effective := standardFits && maximumFits &&
			CombinedHeights(maximum).pane(pane) > CombinedHeights(standard).pane(pane)
		enabled[index] = supportPaneEnabled{pane: pane, enabled: effective}
	}
	return enabled
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
	return max(capacityTableWidth(dashboard), quotaPaneWidth(dashboard)) <= contentWidth
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
	config := dashboard.config.Get()
	return renderCombinedWithThemeAndSymbols(
		frame,
		dashboard,
		chats,
		ConversationAccess{opening: openingPanes, canShow: canShow},
		transcriptSelected,
		chat.EffectiveTheme(config.Theme),
		chat.SymbolsFor(config.Advanced.Symbols),
	)
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
	symbols := chat.SymbolsFor(dashboard.config.Get().Advanced.Symbols)
	return renderCombinedWithThemeAndSymbols(
		frame,
		dashboard,
		chats,
		ConversationAccess{opening: openingPanes, canShow: func(PaneId, string) bool { return true }},
		transcriptSelected,
		selectedTheme,
		symbols,
	)
}

func renderCombinedWithThemeAndSymbols(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, access ConversationAccess, transcriptSelected bool, selectedTheme core.UiTheme, symbols core.SymbolSet) []string {
	if failure, ok := dashboard.quotaProviders.poll(dashboard.config.Get()); ok {
		dashboard.notices.Set(failure)
	}
	clear(dashboard.renderedFailureDismissals)
	var drawn []string
	chat.WithTheme(selectedTheme, func() {
		chat.WithSymbols(symbols, func() {
			drawn = renderCombinedThemed(frame, dashboard, chats, access, transcriptSelected)
			// Dialog titles, hints and notices spell marks like `·` and `…`
			// as literal text; ASCII mode folds whatever the glyph table did
			// not already draw.
			if chat.ASCII() {
				chat.FoldBufferToASCII(frame.Buffer())
			}
		})
	})
	return drawn
}

// combinedFlag is Rust's `u16::from(bool)`.
func combinedFlag(flag bool) int {
	if flag {
		return 1
	}
	return 0
}

// combinedTransition is one pane's `(session id, transition, failed)`: the
// lifecycle operation that owns the session, and whether it failed.
type combinedTransition struct {
	sessionID  string
	transition core.SessionTransitionKind
	failed     bool
}

// combinedTransitionFor is the transition that owns sessionID, failed or not, if
// any.
func (d *DashboardState) combinedTransitionFor(sessionID string) *combinedTransition {
	if kind := d.TransitionKind(sessionID); kind != nil {
		return &combinedTransition{sessionID: sessionID, transition: *kind, failed: false}
	}
	if kind := d.TransitionFailureKind(sessionID); kind != nil {
		return &combinedTransition{sessionID: sessionID, transition: *kind, failed: true}
	}
	return nil
}

// combinedPaneBand is one conversation pane's leaf: its id, rectangle,
// focus, and the transcript and prompt it divides into.
type combinedPaneBand struct {
	id         PaneId
	rect       uv.Rectangle
	focused    bool
	transcript uv.Rectangle
	prompt     uv.Rectangle
}

func renderCombinedThemed(frame *chat.Frame, dashboard *DashboardState, chats map[string]*chat.ActiveChat, access ConversationAccess, transcriptSelected bool) []string {
	dashboard.rebuildPaletteEntries()
	dashboard.resetComponentGeometry()
	dashboard.beginSurfaceFrame()
	notification, hasNotification := dashboard.NotificationFormSession()
	notificationChanged := dashboard.notificationPresentationChanged()
	for _, active := range chats {
		if notificationChanged || !hasNotification || notification != active.SessionID() {
			active.CancelElicitationPointerIn(chat.ElicitationSurfaceNotification)
		}
		if !dashboard.ChatInputAvailable(active.SessionID()) {
			active.CancelComponentPointer()
		}
		active.ResetComponentGeometry()
	}
	dashboard.paneAreas = nil
	dashboard.notificationsArea = nil
	dashboard.clearWorkspaceTabAreas()
	dashboard.workspacePaneArea = nil
	dashboard.sessionRowAreas = dashboard.sessionRowAreas[:0]
	dashboard.projectHeadingAreas = dashboard.projectHeadingAreas[:0]
	dashboard.paneSizeControlAreas = dashboard.paneSizeControlAreas[:0]
	dashboard.frameSurfaces.Clear()
	dashboard.conversationPaneAreas = dashboard.conversationPaneAreas[:0]
	dashboard.conversationAreaField = nil
	area := frame.Area()
	frame.RenderWidget(chat.NewBlock().WithStyle(chat.Base()), area)
	if dashboard.goMode != nil {
		context := dashboard.goContext()
		lines := make([]chat.Line, 0, len(context))
		for _, line := range context {
			lines = append(lines, chat.RawLine(line))
		}
		paragraph := chat.NewParagraph(chat.TextFrom(lines...)).
			WithStyle(chat.Muted()).
			WithWrap(chat.Wrap{Trim: false})
		height := min(paragraph.LineCount(max(area.Dx(), 1)), area.Dy())
		frame.RenderWidget(paragraph, uv.Rect(area.Min.X, area.Min.Y, area.Dx(), height))
		area = uv.Rect(area.Min.X, area.Min.Y+height, area.Dx(), max(area.Dy()-height, 0))
	}
	if area.Dx() < narrowTerminalWidth {
		dashboard.workspaceScrollbar.Clear()
		renderTerminalTooSmall(frame, area, TerminalSizeRequirementWidth{Value: narrowTerminalWidth})
		dashboard.endSurfaceFrame()
		if dashboard.awayDialog() != nil {
			renderModal(frame, area, dashboard)
		}
		return nil
	}
	// Below the sidebar width the Sessions list stacks above the
	// conversation in its compact form, and the support panes go under it.
	narrow := area.Dx() < minimumTerminalWidth
	dashboard.narrowLayout = narrow
	if dialog, ok := dashboard.mode.(ModeResumeDialog); ok {
		pane := resumeSessionsPane(area, dialog.hasPreview(dashboard.resumeRows()))
		dashboard.resumeSessionsArea = &pane
	} else {
		dashboard.resumeSessionsArea = nil
	}

	sidebarWidth := area.Dx()
	if !narrow {
		sidebarWidth = sessionsSidebarWidth(area.Dx(), dashboard.PaneSize(SupportPaneSessions))
	}
	sidebarRight := !narrow && dashboard.config.Get().SessionsSide == core.SessionsSideRight
	// Stacked, the sidebar's band height is only known once the bands are
	// allocated; the content area's top is moved down then.
	contentArea := area
	if !narrow {
		contentX := area.Min.X + sidebarWidth
		if sidebarRight {
			contentX = area.Min.X
		}
		contentArea = uv.Rect(contentX, area.Min.Y, max(area.Dx()-sidebarWidth, 0), area.Dy())
	}
	sidebarX := area.Min.X
	if sidebarRight {
		sidebarX = contentArea.Max.X
	}
	sidebarArea := uv.Rect(sidebarX, area.Min.Y, sidebarWidth, max(area.Dy()-footerHeight, 0))
	workspaceTarget := workspacePaneHeight(dashboard, sidebarArea.Dx())
	var selectedTransition *combinedTransition
	if id, ok := dashboard.CurrentSessionID(); ok {
		if session := dashboard.state.Get().Sessions.GetShared(id); session != nil {
			selectedTransition = dashboard.combinedTransitionFor(session.ID)
		}
	}
	// With no conversation the prompt band holds the two-line guidance that
	// stands in for a composer, so it asks for the rows to show both. A
	// retiring transition uses only the compact status panel. A
	// Starting/Resuming/Moving transition or an in-flight attach parks the
	// real composer in the band (the standby prompt), whose height grows
	// with the wrapped draft exactly like an attached chat's.
	selectedSessionID, hasSelectedSession := dashboard.CurrentSessionID()
	// Pane widths do not depend on the band's height, so they can be
	// measured before the bands are allocated — which is what the composer
	// heights the allocation needs are measured against.
	focusedPane := dashboard.FocusedPane()
	paneWidths := map[PaneId]int{}
	for _, pane := range dashboard.ConversationPanes(uv.Rect(contentArea.Min.X, contentArea.Min.Y, contentArea.Dx(), area.Dy())) {
		paneWidths[pane.ID] = pane.Rect.Dx()
	}
	focusedWidth, ok := paneWidths[focusedPane]
	if !ok {
		focusedWidth = contentArea.Dx()
	}
	focusedChatSession, hasFocusedChat := focusedChatOnScreen(dashboard, chats, access)
	var standbyDrawn bool
	if selectedTransition != nil {
		standbyDrawn = !selectedTransition.failed && DashboardStateTransitionAcceptsPrompts(selectedTransition.transition)
	} else {
		opening, isOpening := dashboard.OpeningSession()
		standbyDrawn = !hasFocusedChat && hasSelectedSession && isOpening && opening == selectedSessionID
	}
	var focusedDesiredPrompt int
	switch {
	case standbyDrawn && hasSelectedSession:
		focusedDesiredPrompt = dashboard.standbyPromptMut(selectedSessionID).DesiredPromptHeight(focusedWidth)
	case selectedTransition != nil:
		focusedDesiredPrompt = promptMinimum
	default:
		focusedDesiredPrompt = emptyPromptHeight
		if hasFocusedChat {
			if active, ok := chats[focusedChatSession]; ok {
				focusedDesiredPrompt = active.DesiredPromptHeight(focusedWidth)
			}
		}
	}
	// The band has to hold the tallest composer the panes want, because
	// every pane's prompt is carved out of the one band.
	type paneSessionPair struct {
		pane      PaneId
		sessionID string
	}
	var otherPanes []paneSessionPair
	for pane, sessionID := range dashboard.navigation.sessions() {
		if pane != focusedPane {
			otherPanes = append(otherPanes, paneSessionPair{pane: pane, sessionID: sessionID})
		}
	}
	slices.SortFunc(otherPanes, func(a, b paneSessionPair) int { return cmp.Compare(a.pane, b.pane) })
	desiredPrompt := focusedDesiredPrompt
	desiredByPane := map[PaneId]int{focusedPane: focusedDesiredPrompt}
	for _, other := range otherPanes {
		width, ok := paneWidths[other.pane]
		if !ok {
			continue
		}
		kind := dashboard.TransitionKind(other.sessionID)
		opening, isOpening := access.opening[other.pane]
		parked := (kind != nil && DashboardStateTransitionAcceptsPrompts(*kind)) ||
			(isOpening && opening == other.sessionID)
		var height int
		switch {
		case !access.canShow(other.pane, other.sessionID):
			height = emptyPromptHeight
		case parked:
			height = dashboard.standbyPromptMut(other.sessionID).DesiredPromptHeight(width)
		default:
			height = emptyPromptHeight
			if active, ok := chats[other.sessionID]; ok {
				height = active.DesiredPromptHeight(width)
			}
		}
		desiredPrompt = max(desiredPrompt, height)
		desiredByPane[other.pane] = height
	}
	sessionsSize := dashboard.PaneSize(SupportPaneSessions)
	if narrow {
		sessionsSize = core.PaneSizeMinimized
	}
	sizes := [4]supportPaneSize{
		{pane: SupportPaneSessions, size: sessionsSize},
		{pane: SupportPaneTargets, size: dashboard.PaneSize(SupportPaneTargets)},
		{pane: SupportPaneQuota, size: dashboard.PaneSize(SupportPaneQuota)},
		{pane: SupportPaneNotifications, size: dashboard.PaneSize(SupportPaneNotifications)},
	}
	// The Sessions pane is as wide as the sidebar; its rows are laid out
	// once, for both the sizes asked here and the draw below.
	sessionsLayout := newSessionsLayout(dashboard, max(sidebarArea.Dx()-2, 0))
	notificationWidth := area.Dx()
	if supportPanesFit(contentArea.Dx(), dashboard) {
		notificationWidth = contentArea.Dx()
	}
	notificationEntries := dashboard.notificationEntries()
	var firstEntry *NotificationEntry
	if len(notificationEntries) > 0 {
		firstEntry = &notificationEntries[0]
	}
	dashboard.syncNotificationSelection(firstEntry)
	notificationText := ""
	if firstEntry != nil {
		notificationText = dashboard.selectedNotificationText(firstEntry)
	}
	// formChat is the warm chat of the session whose form the
	// Notifications card shows as its content, if any.
	formChat := func() *chat.ActiveChat {
		if session, ok := dashboard.notificationFormContentSession(); ok {
			return chats[session]
		}
		return nil
	}
	var notificationMinimum *int
	if active := formChat(); active != nil {
		if height, ok := active.ElicitationMinimumHeight(max(notificationWidth-4, 0)); ok {
			// Card borders, header, buttons.
			minimum := height + 4
			notificationMinimum = &minimum
		}
	}
	notificationFull := paneMinimum
	if firstEntry != nil {
		height, measured := 0, false
		if active := formChat(); active != nil {
			height, measured = active.ElicitationHeight(max(notificationWidth-4, 0))
		}
		if measured {
			notificationFull = height + 4
		} else {
			lines := chat.NewParagraph(chat.RawText(notificationText)).
				WithWrap(chat.Wrap{Trim: false}).
				LineCount(max(notificationWidth-4, 1))
			notificationFull = min(lines, math.MaxUint16) + 8 + combinedFlag(firstEntry.target == nil)
		}
	}
	proportionalSupportHeight := area.Dy() / 4
	_, syncing := dashboard.sessionsSyncLabel()
	notificationsCap := min(max(proportionalSupportHeight, 9), standardSupportPaneHeight)
	if notificationMinimum != nil {
		notificationsCap = max(notificationsCap, *notificationMinimum)
	}
	dimensions := [4]supportPaneDimensions{
		{pane: SupportPaneSessions, dimensions: PaneDimensions{
			minimized: minimizedSessionRows(area.Dy(), sessionsLayout.minimizedContentHeight()) +
				sessionActionsHeight + combinedFlag(syncing) + 2,
			full:        sessionsLayout.contentHeight() + 2 + combinedFlag(syncing),
			standardCap: area.Dy() / 3,
		}},
		{pane: SupportPaneTargets, dimensions: PaneDimensions{
			minimized:   combinedSummaryRow,
			full:        tableHeight(capacityTableLines(dashboard)),
			standardCap: min(proportionalSupportHeight, standardSupportPaneHeight),
		}},
		{pane: SupportPaneQuota, dimensions: PaneDimensions{
			minimized:   combinedSummaryRow,
			full:        min(quotaContentHeight(dashboard)+2, math.MaxUint16),
			standardCap: min(proportionalSupportHeight, standardSupportPaneHeight),
		}},
		{pane: SupportPaneNotifications, dimensions: PaneDimensions{
			minimized: combinedSummaryRow,
			full:      notificationFull,
			// Nine rows fit one line of notification text and the reply
			// controls.
			standardCap: notificationsCap,
		}},
	}
	bands := supportBands(dimensions, sizes, notificationMinimum)
	// Stacked, reserve one workspace content row, then grow its viewport
	// only after the other bands' minimums and the composer have room.
	var sessions PaneBand
	if narrow {
		compact := dimensions[0].dimensions.minimized
		full := compact + workspaceTarget
		sessions = PaneBand{minimum: compact + paneMinimum, full: full, cap: full}
	}
	targets := bands[1]
	quota := bands[2]
	// Keep the support panes stacked together, using the space left by the
	// current sidebar size to decide whether they fit beside Sessions.
	supportsAdjacent := !narrow && supportPanesFit(contentArea.Dx(), dashboard)
	maximizeEnabled := maximizedPaneIsEffective(
		area.Dy(),
		dimensions,
		sessions,
		desiredPrompt,
		sizes,
		notificationMinimum,
	)
	maximizeEnabled[0].enabled = !narrow &&
		sessionsSidebarWidth(area.Dx(), core.PaneSizeMaximized) > sessionsSidebarWidth(area.Dx(), core.PaneSizeStandard)
	dashboard.setPaneMaximizeEnabled(maximizeEnabled)
	var heights CombinedHeights
	switch allocation := allocateCombinedHeights(area.Dy(), sessions, targets, quota, bands[3], desiredPrompt, sizes).(type) {
	case CombinedAllocationFits:
		heights = CombinedHeights(allocation)
	case CombinedAllocationTooSmall:
		dashboard.workspaceScrollbar.Clear()
		renderTerminalTooSmall(frame, area, TerminalSizeRequirementHeight{Value: allocation.RequiredFrameHeight})
		dashboard.endSurfaceFrame()
		// A support-pane minimum must not prevent the user from using an
		// already open dialog that fits this terminal.
		renderModal(frame, area, dashboard)
		return nil
	}

	upperContentHeight := heights.transcript + heights.prompt
	var sidebarHeight int
	switch {
	case narrow:
		sidebarHeight = heights.sessions
	case supportsAdjacent:
		sidebarHeight = max(area.Dy()-footerHeight, 0)
	default:
		sidebarHeight = upperContentHeight
	}
	var workspaceHeight int
	if narrow {
		workspaceHeight = max(sidebarHeight-dimensions[0].dimensions.minimized, 0)
	} else {
		// Sessions keeps its minimum frame plus one row for a session below
		// the Create/Open/Filter controls. The workspace pane gives up rows
		// for that row, but never shrinks below its own minimum, and
		// Sessions' minimum frame always wins.
		sessionsFloor := paneMinimum + 1
		workspaceHeight = min(
			min(workspaceTarget, max(max(sidebarHeight-sessionsFloor, 0), paneMinimum)),
			max(sidebarHeight-paneMinimum, 0),
		)
	}
	workspaceArea := uv.Rect(sidebarArea.Min.X, sidebarArea.Min.Y, sidebarArea.Dx(), workspaceHeight)
	sessionsArea := uv.Rect(
		sidebarArea.Min.X,
		sidebarArea.Min.Y+workspaceHeight,
		sidebarArea.Dx(),
		max(sidebarHeight-workspaceHeight, 0),
	)
	// Stacked, everything below the sidebar band moves down by its height.
	stackedBand := 0
	if narrow {
		stackedBand = heights.sessions
	}
	// The whole conversation band, before it is divided into a transcript
	// and a prompt. The tiled panes are laid out in this rectangle, so it is
	// what a split or a directional pane move is measured against.
	conversationArea := uv.Rect(contentArea.Min.X, contentArea.Min.Y+stackedBand, contentArea.Dx(), upperContentHeight)
	dashboard.conversationAreaField = &conversationArea
	panes := dashboard.ConversationPanes(conversationArea)
	// One pane keeps the band heights the allocator computed for the whole
	// frame. Several panes each carve their own leaf, because a pane's
	// composer is as tall as that pane's draft needs and no taller.
	paneBands := make([]combinedPaneBand, 0, len(panes))
	for _, pane := range panes {
		if len(panes) == 1 {
			split := chat.VerticalLayout(
				chat.ConstraintLength(heights.transcript),
				chat.ConstraintLength(heights.prompt),
			).Split(pane.Rect)
			paneBands = append(paneBands, combinedPaneBand{pane.ID, pane.Rect, pane.IsFocused, split[0], split[1]})
			continue
		}
		desired, ok := desiredByPane[pane.ID]
		if !ok {
			desired = emptyPromptHeight
		}
		transcript, prompt := leafBands(pane.Rect, desired)
		paneBands = append(paneBands, combinedPaneBand{pane.ID, pane.Rect, pane.IsFocused, transcript, prompt})
	}
	for _, band := range paneBands {
		dashboard.conversationPaneAreas = append(dashboard.conversationPaneAreas, conversationPaneArea{
			pane:       band.id,
			transcript: band.transcript,
			prompt:     band.prompt,
		})
	}
	supportX, supportWidth := area.Min.X, area.Dx()
	if supportsAdjacent {
		supportX, supportWidth = contentArea.Min.X, contentArea.Dx()
	}
	supportArea := uv.Rect(
		supportX,
		area.Min.Y+stackedBand+upperContentHeight,
		supportWidth,
		heights.targets+heights.quota+heights.notifications,
	)
	supportRows := chat.VerticalLayout(
		chat.ConstraintLength(heights.targets),
		chat.ConstraintLength(heights.quota),
		chat.ConstraintLength(heights.notifications),
	).Split(supportArea)
	targetsArea, quotaArea := supportRows[0], supportRows[1]
	renderWorkspaceTabs(frame, workspaceArea, dashboard)
	footerArea := uv.Rect(area.Min.X, max(area.Max.Y-1, 0), area.Dx(), 1)
	notificationsArea := supportRows[2]
	dashboard.notificationsArea = &notificationsArea
	var notificationChat *chat.ActiveChat
	if session, ok := dashboard.NotificationFormSession(); ok {
		notificationChat = chats[session]
	}
	notificationsPaneRender(frame, notificationsArea, dashboard, notificationEntries, notificationText, notificationChat)
	dashboard.paneAreas = &[dashboardPaneCount]uv.Rectangle{sessionsArea, targetsArea, quotaArea}
	for _, pane := range []struct {
		pane SupportPane
		area uv.Rectangle
	}{
		{SupportPaneSessions, sessionsArea},
		{SupportPaneTargets, targetsArea},
		{SupportPaneQuota, quotaArea},
	} {
		for _, control := range paneSizeControlAreas(pane.area, pane.pane, dashboard.paneMaximizeEnabled(pane.pane)) {
			dashboard.paneSizeControlAreas = append(dashboard.paneSizeControlAreas, paneSizeControlArea{
				pane: control.Pane,
				size: control.Size,
				area: control.Area,
			})
		}
	}

	rendered := renderSessions(frame, sessionsArea, dashboard, &sessionsLayout)
	dashboard.sessionRowAreas = rendered.sessionRowAreas
	dashboard.projectHeadingAreas = rendered.projectHeadingAreas
	renderSessionRowActions(frame, dashboard)
	sessionsContent := borderedContent(sessionsArea)
	dashboard.frameSurfaces.Push(chat.SurfaceFrameFixed(chat.SurfaceIdDashboardPane(0), sessionsContent))

	// Draw support panes before chat: chat modals can cover the whole
	// screen. Targets and Quota choose their representations
	// independently.
	if sizes[1].size == core.PaneSizeMinimized {
		focused := dashboard.Focus() == FocusTargets
		maximizable := dashboard.paneMaximizeEnabled(SupportPaneTargets)
		frame.RenderWidget(
			chat.Panel(focused).
				WithBorders(chat.BordersTop).
				WithTitle(minimizedTargetsLine(dashboard, paneTitleContentWidth(targetsArea.Dx(), maximizable), focused)).
				WithTitle(minimizedPaneSizeControls(focused, maximizable)),
			targetsArea,
		)
	} else {
		size := sizes[1].size
		renderCapacity(frame, targetsArea, dashboard, &size)
	}
	if sizes[2].size == core.PaneSizeMinimized {
		focused := dashboard.Focus() == FocusQuota
		maximizable := dashboard.paneMaximizeEnabled(SupportPaneQuota)
		frame.RenderWidget(
			chat.Panel(focused).
				WithBorders(chat.BordersTop).
				WithTitle(minimizedQuotaLine(dashboard, paneTitleContentWidth(quotaArea.Dx(), maximizable), focused, epochSeconds())).
				WithTitle(minimizedPaneSizeControls(focused, maximizable)),
			quotaArea,
		)
	} else {
		size := sizes[2].size
		renderQuotas(frame, quotaArea, dashboard, &size)
	}
	targetsContent := targetsArea
	if sizes[1].size != core.PaneSizeMinimized {
		targetsContent = borderedContent(targetsArea)
	}
	quotaContent := quotaArea
	if sizes[2].size != core.PaneSizeMinimized {
		quotaContent = borderedContent(quotaArea)
	}
	dashboard.frameSurfaces.Push(chat.SurfaceFrameFixed(chat.SurfaceIdDashboardPane(1), targetsContent))
	dashboard.frameSurfaces.Push(chat.SurfaceFrameFixed(chat.SurfaceIdDashboardPane(2), quotaContent))

	promptFocused := dashboard.PromptHasFocus()
	// A focused border only says anything when there is another pane to
	// tell it apart from, so the single-pane surface draws as it always has.
	focusBorders := len(paneBands) > 1
	var drawnSessions []string
	chatDrewFooter := false
	// The focused pane draws last so its focus border wins where two panes
	// meet. Its conversation's dialogs stay inside its own rectangle.
	ordered := make([]combinedPaneBand, 0, len(paneBands))
	for _, band := range paneBands {
		if !band.focused {
			ordered = append(ordered, band)
		}
	}
	for _, band := range paneBands {
		if band.focused {
			ordered = append(ordered, band)
		}
	}
	for _, band := range ordered {
		paneID, paneRect, transcriptArea, promptArea := band.id, band.rect, band.transcript, band.prompt
		if id, ok := dashboard.PaneSession(paneID); ok && dashboard.IsNativeAgent(id) {
			dashboard.renderNativeAgent(frame, paneID, id, transcriptArea, promptArea)
			continue
		}
		if id, ok := dashboard.PaneSession(paneID); ok && dashboard.IsStoppedSubagent(id) {
			dashboard.renderStoppedSubagent(frame, paneID, id, transcriptArea, promptArea)
			continue
		}
		if !band.focused {
			// Every pane but the last carries a close chip, and an unfocused
			// pane only exists while there are several.
			closeChip := true
			// An unfocused pane answers only for the session it holds: the
			// Sessions selection and the launch standby belong to the pane
			// with the keyboard.
			paneSession, hasPaneSession := dashboard.PaneSession(paneID)
			var transition *combinedTransition
			if hasPaneSession {
				transition = dashboard.combinedTransitionFor(paneSession)
			}
			var opening *string
			if session, ok := access.opening[paneID]; ok {
				opening = &session
			}
			var paneChat *chat.ActiveChat
			if hasPaneSession && transition == nil &&
				access.canShow(paneID, paneSession) &&
				chatShowsInPane(opening, paneSession) {
				if active, ok := chats[paneSession]; ok && active.HasLiveComposer() {
					paneChat = active
				}
			}
			paneFailed := hasPaneSession && opening == nil && dashboard.SessionFailed(paneSession)
			switch {
			case transition == nil && paneFailed:
				if transcriptArea.Dy() > 3 {
					dashboard.failureDrawn(paneSession)
				}
				failure := dashboard.sessionFailureText(paneSession)
				failureBody := renderEmptyTranscript(
					frame,
					transcriptArea,
					EmptyConversationFailed,
					dashboard.config.Get().Spinner,
					&failure,
					paneTitleReserve(dashboard, transcriptArea.Dx(), titleControls(closeChip)),
					false,
					false,
				)
				pushFailureSurface(dashboard, paneID, failureBody)
				renderEmptyPromptAdvice(frame, promptArea, false, EmptyConversationFailed, dashboard)
			case transition != nil:
				renderTransitionSurface(frame, transcriptArea, promptArea, dashboard, transition.sessionID, TransitionSurface{
					transition:      transition.transition,
					failed:          transition.failed,
					paneID:          paneID,
					highlightBorder: false,
					titleLead:       paneChromeWidth(dashboard, paneID, transcriptArea.Dx()),
				})
			case paneChat != nil:
				drawnSessions = append(drawnSessions, paneChat.SessionID())
				paneChat.DrawIn(frame, chat.ChatRegions{
					Transcript: transcriptArea,
					Prompt:     promptArea,
					Footer:     nil,
					Overlay:    paneRect,
					TitleControls: paneTitleReserve(
						dashboard,
						transcriptArea.Dx(),
						paneTitleReserve(dashboard, transcriptArea.Dx(), titleControls(closeChip)),
					),
					TitleLead:   paneChromeWidth(dashboard, paneID, transcriptArea.Dx()),
					PaneFocused: false,
				}, false, false)
			default:
				reason := EmptyConversationNoConversationOpen
				if opening != nil {
					reason = EmptyConversationOpening
				}
				renderEmptyTranscript(
					frame,
					transcriptArea,
					reason,
					dashboard.config.Get().Spinner,
					nil,
					paneTitleReserve(dashboard, transcriptArea.Dx(), titleControls(closeChip)),
					false,
					dashboard.paneShowsPinHint(paneID),
				)
				renderEmptyPromptAdvice(frame, promptArea, false, reason, dashboard)
			}
			if closeChip {
				renderPaneCloseControl(frame, dashboard, transcriptArea, paneID)
			}
			continue
		}
		// The lone pane offers a close only while it holds a conversation:
		// closing it empties it, which is nothing to ask for when it is
		// already empty.
		zoomChip := dashboard.ConversationZoomed()
		_, holdsSession := dashboard.PaneSession(paneID)
		closeChip := len(paneBands) > 1 || holdsSession
		if selectedTransition != nil {
			renderTransitionSurface(frame, transcriptArea, promptArea, dashboard, selectedTransition.sessionID, TransitionSurface{
				transition:      selectedTransition.transition,
				failed:          selectedTransition.failed,
				paneID:          paneID,
				highlightBorder: focusBorders,
				titleLead:       paneChromeWidth(dashboard, paneID, transcriptArea.Dx()),
			})
			chatDrewFooter = false
		} else if active := chats[focusedChatSession]; hasFocusedChat && active != nil {
			drawnSessions = append(drawnSessions, active.SessionID())
			chords := footerCommands(dashboard, FooterGroupChord)
			functions := directPaletteCommands(dashboard)
			prefix := chordPrefix(dashboard)
			commands := append(slices.Clone(chords), functions...)
			chordTexts := make([]string, 0, len(chords))
			for _, chord := range chords {
				chordTexts = append(chordTexts, chord.Text)
			}
			functionTexts := make([]string, 0, len(functions))
			for _, function := range functions {
				functionTexts = append(functionTexts, function.Text)
			}
			var banner *chat.Line
			if dashboard.PrefixPending() {
				line := prefixBannerLine(dashboard)
				banner = &line
			} else if dashboard.ResizeModeActive() {
				line := resizeBannerLine()
				banner = &line
			}
			var footer *chat.ChatFooter
			if promptFocused {
				footer = &chat.ChatFooter{
					Area:        footerArea,
					Chords:      chordTexts,
					ChordPrefix: prefix,
					Functions:   functionTexts,
					Banner:      banner,
				}
			}
			active.DrawIn(frame, chat.ChatRegions{
				Transcript: transcriptArea,
				Prompt:     promptArea,
				Footer:     footer,
				Overlay:    paneRect,
				TitleControls: paneTitleReserve(
					dashboard,
					transcriptArea.Dx(),
					titleControls(closeChip)+zoomTitleControls(zoomChip),
				),
				TitleLead:   paneChromeWidth(dashboard, paneID, transcriptArea.Dx()),
				PaneFocused: focusBorders,
			}, promptFocused, transcriptSelected)
			if promptFocused {
				// Draw the text the chat fitted into each area, not the
				// host's hint: the first chord carries the prefix.
				for _, command := range active.FooterCommandAreas() {
					if command.Command >= 0 && command.Command < len(commands) {
						renderFooterCommand(frame, command.Area, dashboard, commands[command.Command].Key, command.Text)
					}
				}
			}
			// A chat's dialogs live inside its own pane, so its surfaces
			// join the frame's rather than replacing them: the navigator and
			// the other panes stay selectable.
			dashboard.frameSurfaces.Append(active.FrameSurfaces())
			chatDrewFooter = promptFocused
		} else {
			_, opening := dashboard.OpeningSession()
			// A selected session whose target failed has no worker to attach
			// to; say so and how to get out of it.
			failed := !opening && hasSelectedSession &&
				(dashboard.SessionFailed(selectedSessionID) || dashboard.stoppedNotificationRow(selectedSessionID))
			var reason EmptyConversation
			switch {
			case opening:
				reason = EmptyConversationOpening
			case failed:
				reason = EmptyConversationFailed
			case len(dashboard.orderedSessions()) == 0:
				reason = EmptyConversationNoLiveSession
			default:
				reason = EmptyConversationNoConversationOpen
			}
			if failed && transcriptArea.Dy() > 3 && !dashboard.stoppedNotificationRow(selectedSessionID) {
				dashboard.failureDrawn(selectedSessionID)
			}
			var failure *string
			if failed {
				text := dashboard.sessionFailureText(selectedSessionID)
				failure = &text
			}
			failureBody := renderEmptyTranscript(
				frame,
				transcriptArea,
				reason,
				dashboard.config.Get().Spinner,
				failure,
				paneTitleReserve(
					dashboard,
					transcriptArea.Dx(),
					titleControls(closeChip)+zoomTitleControls(zoomChip),
				),
				focusBorders,
				dashboard.paneShowsPinHint(paneID),
			)
			pushFailureSurface(dashboard, paneID, failureBody)
			if opening {
				// The real composer parks in the prompt band while the
				// attach runs, so anything typed lands in the chat that
				// opens.
				if hasSelectedSession {
					drawStandbyPrompt(frame, promptArea, dashboard, selectedSessionID, paneID, nil)
				}
			} else {
				renderEmptyPromptAdvice(frame, promptArea, promptFocused, reason, dashboard)
			}
			chatDrewFooter = false
		}
		if closeChip {
			renderPaneCloseControl(frame, dashboard, transcriptArea, paneID)
		}
		if zoomChip {
			renderPaneZoomControl(frame, dashboard, transcriptArea)
		}
	}

	if !chatDrewFooter {
		renderFooter(frame, footerArea, dashboard)
	}
	renderPaneChrome(frame, dashboard)
	dashboard.endSurfaceFrame()
	renderModal(frame, area, dashboard)
	renderPaneMenu(frame, area, dashboard)
	return drawnSessions
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
	pane := dashboard.FocusedPane()
	sessionID, ok := dashboard.PaneSession(pane)
	if !ok {
		return "", false
	}
	var opening *string
	if session, ok := access.opening[pane]; ok {
		opening = &session
	}
	active, live := chats[sessionID]
	if live && active.HasLiveComposer() &&
		access.canShow(pane, sessionID) &&
		chatShowsInPane(opening, sessionID) &&
		dashboard.ChatInputAvailable(sessionID) {
		return sessionID, true
	}
	return "", false
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
	if reason == EmptyConversationFailed {
		panel := chat.Panel(paneFocused).
			WithTitle(chat.RawLine(" Session failed ")).
			WithTitleStyle(chat.Style{}.Foreground(chat.ThemePalette().Error).Bold())
		inner := panel.Inner(transcriptArea)
		frame.RenderWidget(panel, transcriptArea)
		body := uv.Rect(
			inner.Min.X+1,
			inner.Min.Y+1,
			max(inner.Dx()-2, 0),
			max(inner.Dy()-1, 0),
		)
		lines := []chat.Line{
			chat.StyledLine(
				"The session's target failed, so there is no conversation to open.",
				chat.Style{}.Foreground(chat.ThemePalette().Error),
			),
			{},
		}
		if failure != nil && *failure != "" {
			for _, line := range chat.RawText(*failure).Lines {
				lines = append(lines, line.WithStyle(chat.Muted()))
			}
		}
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(lines...)).WithWrap(chat.Wrap{Trim: false}), body)
		return &body
	}
	panel := chat.Panel(paneFocused).WithTitle(chat.RawLine(" Conversation "))
	if reason == EmptyConversationOpening {
		panel = panel.WithTitle(chat.LineFrom(
			chat.RawSpan(" "),
			chat.CompactSpan(spinnerStyle, chat.ElapsedMs()),
			chat.RawSpan(" "),
			// Clear of whatever the host draws at the right of the row.
			chat.RawSpan(strings.Repeat(" ", titleControls)),
		).RightAligned())
	}
	frame.RenderWidget(panel, transcriptArea)
	// The pane chrome puts "Pin selected here" on the first inside row of an
	// empty pane; the splash starts below it, and is left out when the pane
	// is too short for both.
	firstRow := 1
	if pinHint {
		firstRow = 2
	}
	height := transcriptArea.Dy()
	top := max(max(height-5, 0)/2, firstRow)
	if height >= 7 && top+3 < height {
		hero := uv.Rect(
			transcriptArea.Min.X+1,
			transcriptArea.Min.Y+top,
			max(transcriptArea.Dx()-2, 0),
			min(height-1-top, 4),
		)
		var invitation string
		switch reason {
		case EmptyConversationNoLiveSession:
			invitation = "Create a session to start building."
		case EmptyConversationNoConversationOpen:
			invitation = "Select a session to open its conversation."
		case EmptyConversationOpening:
			invitation = "Bringing your conversation into focus" + chat.ThemeGlyphs().Ellipsis
		}
		frame.RenderWidget(
			chat.NewParagraph(chat.TextFrom(
				chat.StyledLine(chat.ThemeGlyphs().Spark+"  M J O L N I R", chat.Title(true)),
				chat.Line{},
				chat.StyledLine(invitation, chat.Muted()),
			)).WithAlignment(chat.AlignmentCenter),
			hero,
		)
	}
	return nil
}

// pushFailureSurface registers a failed session's error text so a drag
// copies it like any other pane's text. body is nil when there is none.
func pushFailureSurface(dashboard *DashboardState, paneID PaneId, body *uv.Rectangle) {
	if body != nil {
		dashboard.frameSurfaces.Push(chat.SurfaceFrameFixed(chat.SurfaceIdFailurePane(paneID.Raw()), *body))
	}
}

// renderEmptyPromptAdvice draws the prompt-band advice that accompanies
// [renderEmptyTranscript] for the two reasons where there is nothing to
// type toward yet. While an attach runs the standby composer fills the band
// instead, so Opening never reaches here.
func renderEmptyPromptAdvice(frame *chat.Frame, promptArea uv.Rectangle, promptFocused bool, reason EmptyConversation, dashboard *DashboardState) {
	var title string
	var lines [2]string
	switch reason {
	case EmptyConversationNoLiveSession:
		title = " No live session "
		lines[0] = "No live session in this workspace."
		create, hasCreate := dashboard.FirstKeyLabel(CommandIdNewSessionWizard)
		sessions, hasSessions := dashboard.FirstKeyLabel(CommandIdResumeDialog)
		if hasCreate && hasSessions {
			// `prefix+g` opens the session list, which shows running
			// sessions first and stopped ones a tab away, so telling
			// someone it resumes a session misses most of what it does.
			lines[1] = fmt.Sprintf("Press %s to create a session or %s to find one.", create, sessions)
		} else {
			lines[1] = "Create a session, or find an existing one, from the buttons above."
		}
	case EmptyConversationNoConversationOpen:
		title = " No conversation open "
		lines = [2]string{
			"No conversation open.",
			"Press Tab for Sessions, then Enter on the one to open.",
		}
	case EmptyConversationOpening:
		title = " Opening session "
		lines[0] = "Opening session" + chat.ThemeGlyphs().Ellipsis
		separator := chat.ThemeGlyphs().FooterSeparator
		if detach, ok := dashboard.FirstKeyLabel(CommandIdQuitDetach); ok {
			lines[1] = "Esc cancels" + separator + "select another session to switch" + separator + detach + " quits"
		} else {
			lines[1] = "Esc cancels" + separator + "select another session to switch"
		}
	default:
		if id, ok := dashboard.SelectedSessionID(); ok && dashboard.stoppedNotificationRow(id) {
			title = " Session stopped "
			lines = [2]string{dashboard.stoppedFailureGuidance(id), ""}
			break
		}
		title = " Session failed "
		lines[0] = "Enter in Sessions opens its transcript or recovers it on a fresh target."
		if palette, ok := dashboard.FirstKeyLabel(CommandIdPalette); ok {
			lines[1] = palette + " then Delete session removes it for good."
		} else {
			lines[1] = "Delete session in the command palette removes it for good."
		}
	}
	frame.RenderWidget(
		chat.NewParagraph(chat.TextFrom(chat.RawLine(lines[0]), chat.RawLine(lines[1]))).
			WithStyle(chat.Muted()).
			WithWrap(chat.Wrap{Trim: true}).
			WithBlock(chat.Panel(promptFocused).WithTitle(chat.RawLine(title))),
		promptArea,
	)
}

// drawStandbyPrompt draws the session's standby composer — the real chat
// prompt — into the prompt band, and merges its surfaces into the
// dashboard's so clicks and selections inside the band behave like an
// attached chat's. note adds a left-aligned line to the band's bottom
// border; nil adds none.
func drawStandbyPrompt(frame *chat.Frame, promptArea uv.Rectangle, dashboard *DashboardState, sessionID string, paneID PaneId, note *chat.Line) {
	focused := paneID == dashboard.FocusedPane() && dashboard.PromptHasFocus()
	surfaces := drawStandbyBand(frame, promptArea, dashboard.standbyPromptMut(sessionID), focused, note)
	dashboard.frameSurfaces.Append(&surfaces)
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
	transition, failed, paneID := surface.transition, surface.failed, surface.paneID
	if failed && transcriptArea.Dy() > 3 {
		dashboard.failureDrawn(sessionID)
	}
	session := dashboard.state.Get().Sessions.GetShared(sessionID)
	if session == nil {
		return
	}
	operation := (*dashboard.sessionOperations.Get())[sessionID]
	stages := "waiting"
	if operation != nil && len(operation.activeStages) > 0 {
		labels := make([]string, 0, len(operation.activeStages))
		for _, stage := range operation.activeStages {
			labels = append(labels, stage.Stage.Label())
		}
		stages = strings.Join(labels, ", ")
	}
	var startedAt uint64
	if operation != nil {
		startedAt = operation.startedAtEpochSeconds
	} else if updated, err := time.Parse(time.RFC3339, session.UpdatedAt); err == nil && updated.Unix() >= 0 {
		startedAt = uint64(updated.Unix())
	}
	now := uint64(max(time.Now().Unix(), 0))
	targetID := session.TargetTemplateID
	profile := session.LastProfile
	if operation != nil && operation.resumeDestination != nil {
		targetID = operation.resumeDestination.TargetTemplateID
		profile = operation.resumeDestination.ProfileID
	}
	config := dashboard.config.Get()
	target := dashboard.state.Get().ProjectIdentitySession(session).ProjectTarget(config, targetID)
	panel := chat.Panel(surface.highlightBorder).WithTitle(chat.RawLine(transitionTitle(transition, surface.titleLead)))
	if !failed {
		panel = panel.WithTitle(chat.LineFrom(
			chat.RawSpan(" "),
			chat.CompactSpan(config.Spinner, chat.ElapsedMs()),
			chat.RawSpan(" "),
		).RightAligned())
	}
	typeAhead := !failed && DashboardStateTransitionAcceptsPrompts(transition)
	elapsed := uint64(0)
	if now > startedAt {
		elapsed = now - startedAt
	}
	details := []chat.Line{
		chat.StyledLine(target+" · "+session.ListedTitle(), chat.Style{}.Bold()),
		chat.RawLine("Operation: " + transition.Label()),
		chat.RawLine("Current stage: " + stages),
		chat.RawLine("Elapsed: " + client.FormatClock(elapsed) + " · Profile: " + profile),
	}
	if typeAhead {
		details = append(details, chat.Line{}, chat.StyledLine("Select another session to keep working.", chat.Muted()))
	}
	frame.RenderWidget(
		chat.NewParagraph(chat.TextFrom(details...)).WithWrap(chat.Wrap{Trim: true}).WithBlock(panel),
		transcriptArea,
	)
	cancelKey := func(fallback string) string {
		if key, ok := dashboard.FirstKeyLabel(CommandIdCancelOperation); ok {
			return key
		}
		return fallback
	}
	if typeAhead {
		var note *chat.Line
		if operation != nil && operation.cancellable {
			line := chat.StyledLine(
				" "+cancelKey("the cancel key")+" to cancel "+strings.ToLower(transition.Label())+" ",
				chat.Muted(),
			).LeftAligned()
			note = &line
		}
		drawStandbyPrompt(frame, promptArea, dashboard, sessionID, paneID, note)
		return
	}
	var cancelLine string
	switch {
	case !failed && operation != nil && operation.cancellable:
		cancelLine = cancelKey("The cancel key") + " to cancel " + strings.ToLower(transition.Label())
	case failed:
		reason := "recovery required"
		if session.LastError != nil {
			reason = *session.LastError
		}
		cancelLine = "Operation failed: " + reason
	case operation != nil:
		cancelLine = "This operation is at its commit boundary."
	default:
		cancelLine = "This transition is owned by the daemon; select another session."
	}
	status := chat.Panel(false).WithTitle(chat.RawLine(" Status "))
	if failed {
		body := status.Inner(promptArea)
		pushFailureSurface(dashboard, paneID, &body)
	}
	guidance := "Select another session to keep working."
	if failed {
		guidance = "Press Enter for recovery, or select another session."
	}
	frame.RenderWidget(
		chat.NewParagraph(chat.TextFrom(
			chat.StyledLine(cancelLine, chat.Style{}.Bold()),
			chat.RawLine(guidance),
		)).WithStyle(chat.Muted()).WithWrap(chat.Wrap{Trim: true}).WithBlock(status),
		promptArea,
	)
}

// tableHeight is a bordered table with a header row and rows data rows.
func tableHeight(rows int) int {
	return min(min(rows, math.MaxUint16)+paneMinimum, math.MaxUint16)
}
