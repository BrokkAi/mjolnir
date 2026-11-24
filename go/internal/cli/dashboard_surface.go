// Port of mj-cli/src/dashboard/surface.rs.

package cli

import (
	"encoding/base64"
	"encoding/binary"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"runtime"
	"strings"
	"unicode/utf16"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// postSystemNotification posts one desktop notification with the
// platform's own helper. Output is piped and discarded so a helper's stderr
// cannot reach the terminal that the dashboard is drawing on. Blocking: run
// it off the loop.
func postSystemNotification(notification tui.Notification) error {
	title := fmt.Sprintf("mj: %s", notification.SessionTitle)
	var command *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		command = exec.Command("osascript", "-e", fmt.Sprintf(
			"display notification %s with title %s",
			applescriptString(notification.Body),
			applescriptString(title),
		))
	case "windows":
		command = windowsToastCommand(title, notification.Body)
	default:
		command = exec.Command("notify-send", "--app-name=mj", title, notification.Body)
	}
	output, err := core.RunWithInput(command, []byte{})
	if err != nil {
		return err
	}
	if !output.Status.Success() {
		return fmt.Errorf("%s exited with %s: %s", command.Path, output.Status, strings.TrimSpace(string(output.Stderr)))
	}
	return nil
}

// windowsToastScript shows a toast through the WinRT notification API. `mj`
// registers no app ID of its own, so the toast is posted under the one
// Windows registers for PowerShell. The script is passed encoded and the
// text in the environment, so neither the command line nor PowerShell ever
// parses the text.
const windowsToastScript = `
$ErrorActionPreference = 'Stop'
$null = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$toast = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$lines = $toast.GetElementsByTagName('text')
$null = $lines.Item(0).AppendChild($toast.CreateTextNode($env:MJ_TOAST_TITLE))
$null = $lines.Item(1).AppendChild($toast.CreateTextNode($env:MJ_TOAST_BODY))
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($toast))
`

// windowsToastCommand is the PowerShell command that shows the toast.
// `-EncodedCommand` takes base64 of the script's UTF-16LE text. Without a
// console of its own (CREATE_NO_WINDOW on Windows), PowerShell cannot change
// the mode or title of the console the dashboard is drawing on.
func windowsToastCommand(title, body string) *exec.Cmd {
	units := utf16.Encode([]rune(windowsToastScript))
	script := make([]byte, 0, 2*len(units))
	for _, unit := range units {
		script = binary.LittleEndian.AppendUint16(script, unit)
	}
	command := exec.Command("powershell.exe", "-NoProfile", "-NonInteractive", "-EncodedCommand",
		base64.StdEncoding.EncodeToString(script))
	command.Env = append(os.Environ(), "MJ_TOAST_TITLE="+title, "MJ_TOAST_BODY="+body)
	withoutConsoleWindow(command)
	return command
}

// applescriptString is an AppleScript string literal: double quotes and
// backslashes escaped.
func applescriptString(text string) string {
	return `"` + strings.ReplaceAll(strings.ReplaceAll(text, `\`, `\\`), `"`, `\"`) + `"`
}

// runningOverSSH reports whether any of the SSH session variables is set to
// a non-empty value; env is os.LookupEnv or a test's stand-in.
func runningOverSSH(env func(string) (string, bool)) bool {
	for _, name := range []string{"SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"} {
		if value, ok := env(name); ok && value != "" {
			return true
		}
	}
	return false
}

func copySelectedText(text string, overSSH bool, scheduleSystemCopy func(string), terminalCopy func(string) error) error {
	// Over SSH the desktop clipboard belongs to the remote host. Only OSC 52
	// targets the connecting terminal, even when X11 forwarding sets
	// DISPLAY.
	if !overSSH {
		scheduleSystemCopy(text)
	}
	return terminalCopy(text)
}

// copyAndReport copies text and says so on the notice bar with notice, or
// reports why the terminal clipboard refused it. A desktop clipboard
// failure arrives later, as a clipboard-written dashboard I/O update.
func copyAndReport(dashboard *tui.DashboardState, text, notice string, overSSH bool, scheduleSystemCopy func(string), terminalCopy func(string) error) {
	if err := copySelectedText(text, overSSH, scheduleSystemCopy, terminalCopy); err != nil {
		dashboard.SetFailureNotice(fmt.Sprintf("Copy to the terminal clipboard failed: %s", err))
		return
	}
	dashboard.SetNotice(notice)
}

// draw rebuilds the view on screen.
//
// The renderer compares this frame with the previous one and writes only
// the cells that differ, so drawing when nothing moved costs CPU time and
// no terminal output. That is why the loop does not track which mutations
// were visible; it only paces how often background changes are drawn.
func (c *dashboardContext) draw() error {
	if splash := c.splash; splash != nil {
		if !splash.finished() {
			// Nothing behind the splash has been seen, so nothing is
			// acknowledged as drawn.
			return c.terminal.draw(splash.render)
		}
		c.splash = nil
	}
	c.drawnNoticeGeneration = c.notices.Generation()
	_, transcriptSelected := c.selection.ActiveSurface().(chat.SurfaceIdTranscript)
	loginOpen := c.login != nil
	// The renderer decides which pane draws which conversation and reports
	// back what it drew; that list is what the read receipts follow.
	var drawn []string
	// The highlight and the extraction both run inside the draw closure,
	// once the surface has drawn: the hitboxes are registered by that render
	// and the cells the selection covers only exist in this frame.
	paneAccess := map[tui.PaneId]bool{}
	for _, entry := range c.dashboard.PaneSessions() {
		allowed := true
		if attachment := c.attachments[entry.SessionID]; attachment != nil {
			allowed = attachment.paneCanShow(c.dashboard, entry.Pane)
		}
		paneAccess[entry.Pane] = allowed
	}
	err := c.terminal.draw(func(frame *chat.Frame) {
		drawn = tui.RenderCombinedWithChatAccess(
			frame,
			c.dashboard,
			c.chats,
			c.openingChatSessions,
			transcriptSelected,
			func(pane tui.PaneId, _ string) bool {
				if allowed, ok := paneAccess[pane]; ok {
					return allowed
				}
				return true
			},
		)
		if loginOpen {
			c.selectionText = nil
		} else {
			c.selectionText = drawSelection(frame, &c.selection, c.dashboard.FrameSurfaces())
		}
		if c.login != nil {
			c.login.render(frame, frame.Area())
		}
	})
	if err != nil {
		return err
	}
	// The modal obscures its center while leaving the dashboard visible
	// around it. Do not advance read receipts for a covered conversation.
	if loginOpen {
		c.drawnChatSessions = nil
	} else {
		c.drawnChatSessions = drawn
	}
	c.dashboard.AcknowledgeRender()
	if !loginOpen {
		for _, action := range c.dashboard.TakeDrawnFailureDismissals() {
			spawnNotificationDismissal(c, action)
		}
	}
	if visible := c.visibleChat(); visible != nil {
		visible.AcknowledgeRender()
	}
	// The transcript reports a row space it can no longer measure the
	// selection in — a width change, a rebuilt cache, a jump across the
	// deep past. Dropping the selection is the honest answer; walking
	// history to rescue it is the full-transcript probe this design exists
	// to avoid.
	visible := c.visibleChat()
	invalidated := visible != nil && visible.TranscriptSelectionInvalidated()
	if _, transcript := c.selection.ActiveSurface().(chat.SurfaceIdTranscript); invalidated && transcript {
		c.selection.Clear()
	}
	return nil
}

// emitNotifications reports sessions that started needing a person since
// the last frame: the terminal title, the bell, and a desktop notification,
// by the `[notify]` configuration. The dashboard decides what is due; this
// only writes it out. A desktop notification runs its helper off the loop.
func (c *dashboardContext) emitNotifications() error {
	if title, ok := c.dashboard.TerminalTitle(); ok {
		if err := c.terminal.setTitle(title); err != nil {
			return err
		}
	}
	due := c.dashboard.NotificationEvents()
	if len(due) == 0 {
		return nil
	}
	notify := *c.dashboard.NotifyConfig()
	for _, notification := range due {
		if notify.Bell {
			if err := c.terminal.ringBell(); err != nil {
				return err
			}
		}
		if notify.Mode == core.NotifyModeSystem {
			core.SpawnBlocking(func() struct{} {
				if err := postSystemNotification(notification); err != nil {
					slog.Warn("desktop notification failed", "error", err, "session", notification.SessionID)
				}
				return struct{}{}
			})
		}
	}
	return nil
}

// frameSurfaces is the hitboxes the surface registered on its last frame.
// The combined renderer merges the conversation's into this one registry,
// so there is only ever one to consult.
func (c *dashboardContext) frameSurfaces() *chat.FrameSurfaces {
	return c.dashboard.FrameSurfaces()
}

// autoscrollRequest is the surface a held drag is asking to scroll, and
// which way; false when none.
func (c *dashboardContext) autoscrollRequest() (chat.SurfaceId, int8, bool) {
	return c.selection.AutoscrollRequest(c.frameSurfaces())
}

// applyAutoscroll scrolls the surface a drag is holding against its edge,
// then re-resolves the still pointer against the frame that scroll
// produced.
//
// The pointer emits no events while it is held still, so the rows that
// moved under it only join the selection once the registry is rebuilt,
// which needs the redraw in between.
func (c *dashboardContext) applyAutoscroll() error {
	surface, direction, ok := c.autoscrollRequest()
	if !ok {
		return nil
	}
	if _, notification := surface.(chat.SurfaceIdNotificationText); notification {
		if session, ok := c.dashboard.NotificationFormSession(); ok && c.chats[session] != nil && c.chats[session].HasElicitation() {
			c.chats[session].AutoscrollElicitationIn(chat.ElicitationSurfaceNotification, int(direction))
		} else {
			c.dashboard.AutoscrollNotificationSelection(direction)
		}
	} else if focused := c.focusedChat(); focused != nil {
		focused.AutoscrollSelection(surface, int(direction))
	} else {
		return nil
	}
	if err := c.draw(); err != nil {
		return err
	}
	c.selection.Retrack(c.dashboard.FrameSurfaces())
	return nil
}

// routeSelection routes one terminal event through the selection engine,
// hit-testing against the surfaces the view on screen registered.
func (c *dashboardContext) routeSelection(event tea.Msg) selectionRouting {
	chatOwnsPointer := false
	if mouse, isMouse := event.(tea.MouseMsg); isMouse && !c.dashboard.ModalOpen() {
		if owns, ok := c.dashboard.NotificationFormMouseOwnership(c.chats, mouse); ok {
			chatOwnsPointer = owns
		} else if visible := c.visibleChat(); visible != nil {
			chatOwnsPointer = visible.ComponentHandlesMouse(mouse)
		}
	}
	if forwarded := c.dashboard.RouteQuestionPress(&c.selection, chatOwnsPointer, event); forwarded != nil {
		return selectionRoutingForward{event: forwarded}
	}
	if mouse, isMouse := event.(tea.MouseMsg); isMouse {
		heldDrag := false
		if c.selection.OwnsPointer() {
			switch event := event.(type) {
			case tea.MouseMotionMsg:
				heldDrag = event.Button == tea.MouseLeft
			case tea.MouseReleaseMsg:
				heldDrag = event.Button == tea.MouseLeft
			}
		}
		if !heldDrag && (c.dashboard.ComponentHandlesMouse(mouse) || chatOwnsPointer) {
			c.selection.Clear()
			return selectionRoutingForward{event: event}
		}
	}
	routing := routePromptSelection(&c.selection, c.dashboard, event)
	c.dashboard.SettleQuestionPress(&c.selection, c.chats)
	return routing
}

// copySelection copies the finished selection, using only the terminal
// clipboard over SSH.
//
// The frame on screen predates the release that finished the drag, so this
// redraws before reading. Surfaces that scroll their own rows own the text
// a selection covers, because most of it is not on the frame the stash is
// read from; everything else comes out of that stash.
func (c *dashboardContext) copySelection(surface chat.SurfaceId, selectionRange chat.SelectionRange) error {
	if err := c.draw(); err != nil {
		return err
	}
	var extracted string
	var found bool
	switch surface := surface.(type) {
	case chat.SurfaceIdNotificationText:
		generation := uint64(surface)
		if !c.dashboard.NotificationSelectionIsCurrent(generation) {
			return nil
		}
		if session, ok := c.dashboard.NotificationFormSession(); ok && c.chats[session] != nil && c.chats[session].HasElicitation() {
			extracted, found = c.chats[session].ElicitationSelectionTextIn(chat.ElicitationSurfaceNotification, &selectionRange)
		} else {
			extracted, found = c.dashboard.NotificationSelectionText(generation, &selectionRange)
		}
	case chat.SurfaceIdTranscript:
		if focused := c.focusedChat(); focused != nil {
			extracted, found = focused.TranscriptSelectionText(&selectionRange)
		}
	case chat.SurfaceIdElicitationMessage:
		if focused := c.focusedChat(); focused != nil {
			extracted, found = focused.ElicitationSelectionText(&selectionRange)
		}
	default:
		if c.selectionText != nil {
			extracted, found = *c.selectionText, true
			c.selectionText = nil
		}
	}
	if !found || strings.TrimSpace(extracted) == "" {
		slog.Debug("selection covered no text", "surface", fmt.Sprintf("%#v", surface), "range", fmt.Sprintf("%+v", selectionRange))
		return nil
	}
	lines := max(rustLineCount(extracted), 1)
	plural := "s"
	if lines == 1 {
		plural = ""
	}
	return c.copyText(extracted, fmt.Sprintf("Copied %d line%s", lines, plural))
}

// rustLineCount is str::lines().count(): lines split on \n, a trailing
// newline ending the last line rather than starting an empty one.
func rustLineCount(text string) int {
	if text == "" {
		return 0
	}
	return strings.Count(strings.TrimSuffix(text, "\n"), "\n") + 1
}

func (c *dashboardContext) copyText(text, notice string) error {
	updates := c.dashboardIoTx
	copyAndReport(
		c.dashboard,
		text,
		notice,
		runningOverSSH(os.LookupEnv),
		func(text string) {
			// Opening the desktop clipboard can block, so keep it off the
			// render loop.
			spawnClipboardWrite(text, updates)
		},
		c.terminal.copyToTerminalClipboard,
	)
	return nil
}

// refreshControllerDerivedState recomputes what depends on controller state
// after it may have changed.
func (c *dashboardContext) refreshControllerDerivedState() {
	if !c.controllerChanged {
		return
	}
	c.controllerChanged = false
	changedTargets := map[string]string{}
	removedSizes := map[string]*uint64{}
	// Only the resume dialog shows checkpoint sizes, for its Mjolnir rows.
	archives := c.dashboard.ResumeCheckpointArchives()
	for id, target := range c.checkpointArchiveRecords.Changes(archives, func(a, b *string) bool { return *a == *b }) {
		seen, wasSeen := c.checkpointArchiveTargetsSeen[id]
		if (target == nil && !wasSeen) || (target != nil && wasSeen && seen == *target) {
			continue
		}
		if target != nil {
			changedTargets[id] = *target
		} else {
			delete(c.checkpointArchiveTargetsSeen, id)
			delete(c.checkpointArchivePending, id)
			removedSizes[id] = nil
		}
	}
	c.checkpointArchiveRecords = archives
	c.dashboard.PatchCheckpointArchiveSizes(removedSizes)
	if len(changedTargets) > 0 {
		if c.checkpointArchiveGeneration == ^uint64(0) {
			panic("archive request sequence exhausted")
		}
		c.checkpointArchiveGeneration++
		for id := range changedTargets {
			c.checkpointArchivePending[id] = c.checkpointArchiveGeneration
		}
		spawnCheckpointArchiveSizeRefresh(c.checkpointArchiveGeneration, changedTargets, c.dashboardIoTx)
	}
}

// requestQuotaRefresh asks the daemon to probe quota now. The daemon is the
// only process that asks a provider; the reports and the refreshing marks
// come back through the runtime feed, and a refusal or a missing daemon
// comes back as a notice. Returns nothing to wait for: the notice completes
// when the daemon finishes a later cycle.
func (c *dashboardContext) requestQuotaRefresh() {
	cycles := c.quotaCycles
	c.manualQuotaRefreshCycles = &cycles
	spawnQuotaRefreshRequest(c.dashboardIoTx)
}

// dropWarmChatFor retires the chat and attachments captured when the
// lifecycle began.
//
// Pause and destroy retire that session's actor. Resume starts a new one,
// often on a different profile. Keeping the old view would redraw a
// Closing/Closed snapshot and refuse prompts.
func (c *dashboardContext) dropWarmChatFor(retirement *chatRetirement) {
	if !retirement.isCurrent(c.chats, c.attachments) {
		return
	}
	sessionID := retirement.sessionID()
	retirement.retireAttachments(c.attachments)
	for pane, session := range c.openingChatSessions {
		if session == sessionID {
			delete(c.openingChatSessions, pane)
		}
	}
	c.syncOpeningSession()
	if _, ok := c.chats[sessionID]; ok {
		// A completed lifecycle retires the warm actor without passing
		// through the normal session-switch path. Preserve its latest
		// composer before dropping that actor as well.
		c.recordChatDetach(sessionID)
		delete(c.chats, sessionID)
		if current, ok := c.dashboard.CurrentSessionID(); ok && current == sessionID {
			c.selection.Clear()
		}
	}
}
