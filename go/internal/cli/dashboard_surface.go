// Port of mj-cli/src/dashboard/surface.rs.

package cli

import (
	"fmt"
	"os/exec"
	"strings"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// postSystemNotification posts one desktop notification with the
// platform's own helper. Output is piped and discarded so a helper's stderr
// cannot reach the terminal that the dashboard is drawing on. Blocking: run
// it off the loop.
func postSystemNotification(notification tui.Notification) error {
	panic("unported: mj-cli/src/dashboard/surface.rs post_system_notification")
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
	panic("unported: mj-cli/src/dashboard/surface.rs windows_toast_command")
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
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::draw")
}

// emitNotifications reports sessions that started needing a person since
// the last frame: the terminal title, the bell, and a desktop notification,
// by the `[notify]` configuration. The dashboard decides what is due; this
// only writes it out. A desktop notification runs its helper off the loop.
func (c *dashboardContext) emitNotifications() error {
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::emit_notifications")
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
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::apply_autoscroll")
}

// routeSelection routes one terminal event through the selection engine,
// hit-testing against the surfaces the view on screen registered.
func (c *dashboardContext) routeSelection(event tea.Msg) selectionRouting {
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::route_selection")
}

// copySelection copies the finished selection, using only the terminal
// clipboard over SSH.
//
// The frame on screen predates the release that finished the drag, so this
// redraws before reading. Surfaces that scroll their own rows own the text
// a selection covers, because most of it is not on the frame the stash is
// read from; everything else comes out of that stash.
func (c *dashboardContext) copySelection(surface chat.SurfaceId, selectionRange chat.SelectionRange) error {
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::copy_selection")
}

func (c *dashboardContext) copyText(text, notice string) error {
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::copy_text")
}

// refreshControllerDerivedState recomputes what depends on controller state
// after it may have changed.
func (c *dashboardContext) refreshControllerDerivedState() {
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::refresh_controller_derived_state")
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
	panic("unported: mj-cli/src/dashboard/surface.rs DashboardContext::drop_warm_chat_for")
}
