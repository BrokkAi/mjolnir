// Port of mj-tui/src/dialogs/render.rs.
//
// Drawing for the standard dashboard dialogs, and the button lists and body
// text their key handling shares.

package tui

import (
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"
	"rsc.io/qr/coding"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// webDialogMinInnerWidth is render_web_dialog's MIN_INNER_WIDTH: text that
// names the natural body width. The box hugs the QR, and longer URLs wrap
// beneath it rather than stretching the dialog across the screen.
const webDialogMinInnerWidth = 40

// qrQuietZone is render_qr's QUIET_ZONE: the blank modules around the code.
const qrQuietZone = 4

// confirmationButtons is the button labels for a confirmation dialog,
// ordered Cancel first and the primary action last. This is the single
// declaration used by both key handling and rendering.
func confirmationButtons(confirmation Confirmation) []string {
	switch c := confirmation.(type) {
	case ConfirmationRepairRepositoryRemotes:
		return []string{"Cancel", "Repair and continue"}
	case ConfirmationConfigurationRepair:
		return []string{"Dismiss", "Open transcript", "Open settings"}
	case ConfirmationLaunchFailed:
		if c.Retry != nil {
			return []string{"Dismiss", "Retry launch", "Settings"}
		}
		return []string{"Dismiss", "Settings"}
	case ConfirmationImportFailed:
		return []string{"Dismiss"}
	case ConfirmationDismiss:
		if c.Intent == DismissalIntentCancelImport {
			return []string{"Keep importing", "Cancel import"}
		}
		return []string{"Keep editing", "Discard settings"}
	case ConfirmationInstallMbx:
		if c.Upgrading {
			return []string{"Cancel", "Upgrade mbx"}
		}
		return []string{"Cancel", "Install mbx"}
	case ConfirmationConvertRawCheckout:
		return []string{"Cancel", "Confirm"}
	case ConfirmationDestroyStopped:
		return []string{"Cancel", "Destroy session"}
	case ConfirmationCloseFailed:
		return []string{"Cancel", "Retry suspension", "Suspend without checkpoint…"}
	case ConfirmationSuspendSession:
		if c.Checking {
			return []string{"Cancel"}
		}
		preflight := c.Preflight
		if preflight == nil {
			return []string{"Cancel", "Suspend session"}
		}
		if preflight.HasBlockers() {
			return []string{"Cancel"}
		}
		if untracked := preflight.UntrackedCount(); untracked > 0 {
			noun := "files"
			if untracked == 1 {
				noun = "file"
			}
			return []string{"Cancel", fmt.Sprintf("Discard %d %s and continue", untracked, noun)}
		}
		if preflight.HasWarnings() || c.Interrupting || len(c.ChildrenNotHandedBack) > 0 {
			return []string{"Cancel", "Continue anyway"}
		}
		return []string{"Cancel", "Suspend session"}
	case ConfirmationInterruptWork:
		if c.Restart {
			return []string{"Cancel", "Restart now"}
		}
		return []string{"Cancel", "Suspend now"}
	case ConfirmationRestartAgent:
		return []string{"Cancel", "Restart agent"}
	case ConfirmationRestartContainer:
		if c.UpgradeOnly {
			if c.Preview.Settings != nil {
				return []string{"Cancel", "Apply settings and restart…"}
			}
			return []string{"Cancel", "Upgrade container and restart…"}
		}
		return []string{"Cancel", "Restart agent", "Upgrade container and restart…"}
	case ConfirmationInterruptAll:
		return []string{"Cancel", "Interrupt all"}
	case ConfirmationRecoverFailed:
		switch {
		case c.RetryLaunch:
			return []string{"Cancel", "Open transcript", "Retry launch"}
		case c.Recoverable:
			return []string{"Cancel", "Open transcript", "Recover"}
		}
		return []string{"Cancel", "Open transcript"}
	case ConfirmationRecoverMove:
		if c.Operation.QueueAdmissionStarted && !c.Operation.QueueAdmissionFinished {
			return []string{"Cancel", "Open transcript", "Retry move"}
		}
		return []string{"Cancel", "Open transcript", "Retry move", "Resume previous settings"}
	case ConfirmationForceDestroy:
		return []string{"Cancel", "Destroy session"}
	}
	panic(fmt.Sprintf("unknown confirmation %T", confirmation))
}

// confirmationAccelerators is one letter per button, in button order, so
// every confirmation answers a key without Tab. The letter is the first
// letter of the label's first word that no earlier button took, then of a
// later word, then any later letter of the label; a label with nothing left
// gets no letter (0).
//
// `Cancel` and `Continue anyway` therefore answer `c` and `o`; `Yes` and
// `No` answer `y` and `n`.
func confirmationAccelerators(labels []string) []rune {
	var taken []rune
	accelerators := make([]rune, 0, len(labels))
	for _, label := range labels {
		lower := strings.ToLower(label)
		var candidates []rune
		// Rust's char::is_alphanumeric: alphabetic or numeric.
		words := strings.FieldsFunc(lower, func(character rune) bool {
			return !unicode.IsLetter(character) && !unicode.IsNumber(character)
		})
		for _, word := range words {
			first, _ := utf8.DecodeRuneInString(word)
			candidates = append(candidates, first)
		}
		for _, character := range lower {
			if character < utf8.RuneSelf && (unicode.IsLetter(character) || unicode.IsDigit(character)) {
				candidates = append(candidates, character)
			}
		}
		letter := rune(0)
		for _, candidate := range candidates {
			if !dialogsContainsRune(taken, candidate) {
				letter = candidate
				break
			}
		}
		taken = append(taken, letter)
		accelerators = append(accelerators, letter)
	}
	return accelerators
}

func dialogsContainsRune(runes []rune, wanted rune) bool {
	for _, candidate := range runes {
		if candidate == wanted {
			return true
		}
	}
	return false
}

// confirmationKeyLine is the line under a confirmation's text naming each
// button's letter, in the same order the buttons are drawn:
// `c Cancel · o Continue anyway`.
func confirmationKeyLine(labels []string) chat.Line {
	accelerators := confirmationAccelerators(labels)
	var spans []chat.Span
	for index, label := range labels {
		if index > 0 {
			spans = append(spans, chat.StyledSpan("  ·  ", chat.Muted()))
		}
		if letter := accelerators[index]; letter != 0 {
			spans = append(spans, chat.StyledSpan(
				string(letter)+" ",
				chat.Style{}.Foreground(chat.ThemePalette().Secondary).Bold(),
			))
		}
		spans = append(spans, chat.StyledSpan(label, chat.Muted()))
	}
	return chat.LineFrom(spans...)
}

// primaryButton is the index of the primary (rightmost) button, which is
// focused when a dialog opens.
func primaryButton(labels []string) int {
	return max(len(labels)-1, 0)
}

func initialConfirmationButton(confirmation Confirmation, labels []string) int {
	switch confirmation.(type) {
	case ConfirmationDismiss, ConfirmationConfigurationRepair, ConfirmationLaunchFailed,
		ConfirmationImportFailed, ConfirmationForceDestroy, ConfirmationDestroyStopped,
		ConfirmationCloseFailed, ConfirmationSuspendSession, ConfirmationInterruptWork,
		ConfirmationRestartContainer, ConfirmationRestartAgent, ConfirmationInterruptAll,
		ConfirmationRepairRepositoryRemotes, ConfirmationInstallMbx, ConfirmationConvertRawCheckout:
		return 0
	default:
		return primaryButton(labels)
	}
}

func importProgressStatus(progress *ImportProgress) chat.Line {
	stalledFor := time.Since(progress.lastUpdated)
	if stalledFor >= importStallWarningAfter {
		return chat.StyledLine(
			fmt.Sprintf("No progress for %ds; the filesystem may be stalled.", int64(stalledFor/time.Second)),
			chat.Style{}.Foreground(chat.ThemePalette().Warning),
		)
	}
	return chat.StyledLine(
		"The dashboard remains responsive while the import runs.",
		chat.Style{}.Foreground(chat.ThemePalette().Muted),
	)
}

// importProgressMut finds an active import even while a cancellation
// confirmation temporarily owns the foreground. Progress replies must keep
// updating the preserved dialog so rejecting the confirmation returns to
// current state. Nil when no import is open.
func importProgressMut(mode *Mode) *ImportProgress {
	switch current := (*mode).(type) {
	case ModeImporting:
		return current.ImportProgress
	case ModeConfirm:
		if dismiss, ok := current.confirmation.(ConfirmationDismiss); ok {
			return importProgressMut(&dismiss.Mode)
		}
	}
	return nil
}

// dialogsInner is ratatui's `popup.inner(Margin { horizontal: 1, vertical: 1 })`.
func dialogsInner(popup uv.Rectangle) uv.Rectangle {
	return chat.RectInner(popup, chat.Margin{Horizontal: 1, Vertical: 1})
}

// dialogsParagraph is a paragraph of lines (ratatui's `Paragraph::new(lines)`).
func dialogsParagraph(lines []chat.Line) chat.Paragraph {
	return chat.NewParagraph(chat.TextFrom(lines...))
}

// dialogsTextParagraph is a paragraph of plain text (ratatui's
// `Paragraph::new("…")`).
func dialogsTextParagraph(text string) chat.Paragraph {
	return chat.NewParagraph(chat.RawText(text))
}

func dialogsWarningStyle() chat.Style {
	return chat.Style{}.Foreground(chat.ThemePalette().Warning)
}

func renderImportProgress(frame *chat.Frame, area uv.Rectangle, progress *ImportProgress, surfaces *chat.FrameSurfaces) {
	total := "?"
	if progress.total != nil {
		total = strconv.Itoa(*progress.total)
	}
	status := importProgressStatus(progress)
	paragraph := dialogsParagraph([]chat.Line{
		chat.StyledLine(truncateToCells(progress.sessionTitle, 60, TruncateSummary), chat.Style{}.Bold()),
		chat.RawLine(""),
		chat.RawLine(progress.message),
		status,
		chat.RawLine(""),
	}).WithWrap(chat.Wrap{Trim: false})
	popup := centeredModal(frame, surfaces, 76, popupHeight(&paragraph, 76, 11, area), area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(&progress.form)
		return
	}
	frame.RenderWidget(paragraph, uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-1, 0)))
	form := &progress.form
	form.BeginFrame()
	title := dismissibleModalTitle(
		&form.Form,
		popup,
		fmt.Sprintf("Importing session · progress %d/%s", progress.step, total),
		chat.Title(true),
		progress.cancellable,
	)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	chat.ButtonRender(
		frame,
		uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1),
		"Cancel",
		progress.cancellable,
		&form.Form,
		DialogControlCancel,
	)
	form.EndFrame(DialogControlCancel)
}

func renderImportBundleConfirmation(frame *chat.Frame, area uv.Rectangle, confirmation *ImportBundleConfirmation, surfaces *chat.FrameSurfaces) {
	var lines []chat.Line
	if len(confirmation.dirtyGitRoots) > 0 {
		lines = append(lines, chat.RawLine("These checkouts have uncommitted changes:"))
		for _, root := range confirmation.dirtyGitRoots {
			lines = append(lines, chat.StyledLine(root, dialogsWarningStyle()))
		}
	}
	if len(confirmation.omittedNonGitDirs) > 0 {
		if len(lines) > 0 {
			lines = append(lines, chat.RawLine(""))
		}
		lines = append(lines, chat.RawLine("These changed directories are outside Git and won't be included:"))
		for _, directory := range confirmation.omittedNonGitDirs {
			lines = append(lines, chat.StyledLine(directory, dialogsWarningStyle()))
		}
	}
	if len(confirmation.scratchGitRoots) > 0 {
		if len(lines) > 0 {
			lines = append(lines, chat.RawLine(""))
		}
		lines = append(lines, chat.RawLine(
			"These scratch repositories are in temporary directories and stay outside the import:",
		))
		for _, root := range confirmation.scratchGitRoots {
			lines = append(lines, chat.StyledLine(root, dialogsWarningStyle()))
		}
	}
	lines = append(lines, chat.RawLine(""))
	if len(confirmation.dirtyGitRoots) == 0 {
		lines = append(lines, chat.RawLine("Git checkouts are clean."))
	}
	if confirmation.managedWorktree.Available {
		lines = append(lines, chat.RawLine(
			"Checked creates a clean, separate checkout. Unchecked uses the original directory with its current files.",
		))
	} else {
		lines = append(lines, chat.RawLine("The imported session will use this project on the daemon host."))
	}
	controlLines := 4
	if confirmation.managedWorktree.Available {
		controlLines++
	}
	bodyParagraph := dialogsParagraph(lines).WithWrap(chat.Wrap{Trim: false})
	height := popupHeight(&bodyParagraph, 76, controlLines+10, area)
	popup := centeredModal(frame, surfaces, 76, height, area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(&confirmation.form)
		return
	}
	bodyHeight := max(inner.Dy()-controlLines, 0)
	frame.RenderWidget(dialogsParagraph(lines).WithWrap(chat.Wrap{Trim: false}), uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), bodyHeight))
	form := &confirmation.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Confirm import", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	y := inner.Min.Y + bodyHeight
	frame.RenderWidget(dialogsTextParagraph("Project root on daemon host:"), uv.Rect(inner.Min.X, y, inner.Dx(), 1))
	chat.TextFieldRender(frame, uv.Rect(inner.Min.X, y+1, inner.Dx(), 1), &confirmation.projectRoot, &form.Form, DialogControlField)
	y += 2
	// The worktree choice only exists when the import can create one.
	if confirmation.managedWorktree.Available {
		chat.CheckboxRender(
			frame,
			uv.Rect(inner.Min.X, y, inner.Dx(), 1),
			"Create isolated checkout",
			confirmation.createManagedWorktree,
			true,
			&form.Form,
			DialogControlImportManagedWorktree,
		)
	}
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlImportCancel, Label: "Cancel", Enabled: true},
		{ID: DialogControlImportContinue, Label: "Continue", Enabled: true},
	})
	form.EndFrame(DialogControlImportContinue)
}

// renderTextPrompt draws a one-field modal: a header line naming what is
// being renamed, the field itself, and a Cancel/Save footer. The rename
// editors are the two dialogs shaped this way.
func renderTextPrompt(frame *chat.Frame, area uv.Rectangle, form *chat.Dialog[DialogControl], value *chat.TextInput, header string, title string, surfaces *chat.FrameSurfaces) {
	popup := centeredModal(frame, surfaces, 60, 8, area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(form)
		return
	}
	frame.RenderWidget(dialogsTextParagraph(FitSessionName(header, inner.Dx())), uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), 1))
	field := uv.Rect(inner.Min.X, inner.Min.Y+2, inner.Dx(), 1)
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	form.BeginFrame()
	titleLine := dismissibleModalTitle(&form.Form, popup, title, chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	chat.TextFieldRender(frame, field, value, &form.Form, DialogControlField)
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlCancel, Label: "Cancel", Enabled: true},
		{ID: DialogControlSave, Label: "Save", Enabled: true},
	})
	form.EndFrame(DialogControlField)
}

// noticeLogAges is the age column of the notice log, one cell per age in
// seconds: each "… ago" right-aligned to the widest one and followed by a
// space, so the messages start in the same column whether an age reads "44s"
// or "1m17s".
func noticeLogAges(ages []uint64) []string {
	formatted := make([]string, 0, len(ages))
	width := 0
	for _, secs := range ages {
		age := client.FormatClock(secs) + " ago"
		width = max(width, utf8.RuneCountInString(age))
		formatted = append(formatted, age)
	}
	column := make([]string, 0, len(formatted))
	for _, age := range formatted {
		column = append(column, strings.Repeat(" ", width-utf8.RuneCountInString(age))+age+" ")
	}
	return column
}

// renderNoticeLog draws the notice log: one row per remembered notice,
// newest first, with how long ago it was reported.
func renderNoticeLog(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *NoticeLogDialog, surfaces *chat.FrameSurfaces) {
	history := dashboard.notices.History()
	popup := centeredModal(frame, surfaces, 80, min(max(len(history)+5, 8), 30), area)
	inner := dialogsInner(popup)
	if inner.Dy() < 2 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Recent messages", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	listArea := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-2, 0))
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	if len(history) == 0 {
		frame.RenderWidget(dialogsTextParagraph("Nothing has been reported yet.").WithStyle(chat.Muted()), listArea)
	} else {
		now := time.Now()
		seconds := make([]uint64, 0, len(history))
		for _, record := range history {
			seconds = append(seconds, uint64(max(now.Sub(record.At), 0)/time.Second))
		}
		ages := noticeLogAges(seconds)
		// Every message is kept whole: a long failure wraps after its age
		// column rather than losing its tail, which is the part that says
		// what went wrong. The widget wraps by cell, so an unbroken path or
		// token still wraps instead of running off the edge.
		lines := make([]chat.Line, 0, len(history))
		for index, record := range history {
			style := chat.Style{}.Foreground(chat.ThemePalette().Text)
			if record.Failure {
				style = dialogsWarningStyle()
			}
			lines = append(lines, chat.LineFrom(
				chat.StyledSpan(ages[index], chat.Muted()),
				chat.StyledSpan(record.Text, style),
			))
		}
		paragraph := dialogsParagraph(lines).WithWrap(chat.Wrap{Trim: false})
		total := paragraph.LineCount(max(listArea.Dx(), 1))
		maxScroll := max(total-listArea.Dy(), 0)
		dialog.maxScroll = maxScroll
		scroll := min(dialog.scroll, maxScroll)
		frame.RenderWidget(paragraph.WithScroll(min(scroll, dialogsMaxU16), 0), listArea)
	}
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlNoticeLogClose, Label: "Close", Enabled: true},
	})
	form.EndFrame(DialogControlNoticeLogClose)
}

// dialogsMaxU16 is u16::MAX, where Rust converts a count into a terminal coordinate.
const dialogsMaxU16 = 1<<16 - 1

func renderSessionCpuReport(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *SessionCpuReportDialog, surfaces *chat.FrameSurfaces) {
	metric := dialog.metric
	groups := reportGroups(dashboard, metric)
	groupIndex, hasGroup := reconcileSessionCpuReport(dashboard, dialog, groups)
	// Sized for the longest machine, so switching tabs never resizes it.
	popupH := 9
	if len(groups) > 0 {
		longest := 0
		for _, group := range groups {
			longest = max(longest, len(group.lines))
		}
		popupH = min(max(longest+8, 9), 30)
	}
	popup := centeredModal(frame, surfaces, 80, popupH, area)
	inner := dialogsInner(popup)
	if inner.Dy() < 4 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "CPU by session", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	tabsArea := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), 1)
	// A blank row parts the tabs from the list.
	bodyArea := uv.Rect(inner.Min.X, inner.Min.Y+2, inner.Dx(), inner.Dy()-4)
	hintArea := uv.Rect(inner.Min.X, max(inner.Max.Y-2, 0), inner.Dx(), 1)
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	check := chat.ThemeGlyphs().Check
	mark := func(shown CpuMetric, label string) string {
		if shown == metric {
			return check + " " + label
		}
		return strings.Repeat(" ", chat.RawSpan(check).Width()) + " " + label
	}
	metricButtons := []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlSessionCpuReportHourly, Label: mark(CpuMetricHourly, "Hourly"), Enabled: true},
		{ID: DialogControlSessionCpuReportRecent, Label: mark(CpuMetricRecent, "Recent"), Enabled: true},
	}
	split := chat.ButtonColumnSplit(bodyArea, metricButtons)
	listArea, metricArea := split.Body, split.Actions
	// Padded like a button, so each tab reads as one.
	labels := make([]string, 0, len(groups))
	for index := range groups {
		labels = append(labels, "  "+groups[index].tabLabel()+"  ")
	}
	selectedTab := 0
	if hasGroup {
		selectedTab = groupIndex
	}
	if len(groups) == 0 {
		chat.TabStripRenderEnabled(frame, tabsArea, labels, 0, false, &form.Form, DialogControlSessionCpuReportTabs)
		form.DeclareWithEnabled(
			DialogControlSessionCpuReportRows,
			chat.ControlKindChoiceList{Len: 0, Selected: 0},
			false,
		)
		frame.RenderWidget(dialogsTextParagraph("No active sessions.").WithStyle(chat.Muted()), listArea)
	} else {
		group := &groups[selectedTab]
		chat.TabStripRender(frame, tabsArea, labels, selectedTab, &form.Form, DialogControlSessionCpuReportTabs)
		chat.ChoiceListRenderWithRows(
			frame,
			listArea,
			group.lines,
			dialog.selectedRow,
			group.rowMap,
			nil,
			&form.Form,
			DialogControlSessionCpuReportRows,
		)
	}
	chat.ButtonColumnRender(frame, metricArea, metricButtons, &form.Form)
	if _, focused := form.Focused(); !focused && len(groups) > 0 {
		form.Focus(DialogControlSessionCpuReportRows)
	}
	frame.RenderWidget(
		dialogsTextParagraph("←/→ tabs · ↑/↓ sessions · Enter opens · Esc closes").WithStyle(chat.Muted()),
		hintArea,
	)
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlSessionCpuReportClose, Label: "Close", Enabled: true},
	})
	form.EndFrame(DialogControlSessionCpuReportClose)
}

// renderChangedFiles draws the changed-files overlay: the branch line, the
// totals, then one row per file with its kind and line counts.
func renderChangedFiles(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *ChangedFilesDialog, surfaces *chat.FrameSurfaces) {
	status, present := dashboard.gitStatus[dialog.sessionID]
	var files []core.ChangedFile
	if present && status.Err == nil {
		files = status.Ok.Changed
	}
	popup := centeredModal(frame, surfaces, 80, min(max(len(files)+7, 9), 40), area)
	inner := dialogsInner(popup)
	if inner.Dy() < 3 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	form := &dialog.form
	form.BeginFrame()
	name := dialog.sessionID
	if session, ok := dashboard.state.Get().Sessions.Get(dialog.sessionID); ok {
		name = sessionName(&session)
	}
	name = FitSessionName(name, max(popup.Dx()-24, 0))
	title := dismissibleModalTitle(&form.Form, popup, "Changed files · "+name, chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)

	header := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), 1)
	listArea := uv.Rect(inner.Min.X, inner.Min.Y+2, max(inner.Dx()-1, 0), max(inner.Dy()-4, 0))
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	var headerLine chat.Line
	switch {
	case !present:
		headerLine = chat.StyledLine("Reading the checkout"+chat.ThemeGlyphs().Ellipsis, chat.Muted())
	case status.Err != nil:
		headerLine = chat.StyledLine("Could not read the checkout: "+*status.Err, dialogsWarningStyle())
	default:
		git := status.Ok
		added, removed := git.Totals()
		branch := git.RowText()
		if branch == "" {
			branch = git.Branch
		}
		headerLine = chat.LineFrom(
			chat.StyledSpan(branch, chat.Title(true)),
			chat.StyledSpan(
				fmt.Sprintf("   %s · +%d −%d", counted(len(git.Changed), "file", "files"), added, removed),
				chat.Muted(),
			),
		)
	}
	frame.RenderWidget(dialogsParagraph([]chat.Line{headerLine}), header)

	if len(files) == 0 {
		text := ""
		if present && status.Err == nil {
			text = "Nothing has changed since the last commit."
		}
		frame.RenderWidget(dialogsTextParagraph(text).WithStyle(chat.Muted()), listArea)
	} else {
		// The longest status word is eight cells (`modified`, `conflict`), so
		// the column is nine: a word that fills it must still leave a space
		// before the path.
		const kindWidth = 9
		const countWidth = 12
		pathWidth := max(listArea.Dx()-(kindWidth+countWidth+2), 0)
		scroll := min(dialog.scroll, max(len(files)-listArea.Dy(), 0))
		end := min(scroll+listArea.Dy(), len(files))
		rows := make([]chat.Line, 0, end-scroll)
		for index := scroll; index < end; index++ {
			file := &files[index]
			counts := ""
			if file.Added != nil && file.Removed != nil {
				counts = fmt.Sprintf("+%d −%d", *file.Added, *file.Removed)
			}
			width := min(
				max(listArea.Dx()-(kindWidth+min(pathWidth, utf8.RuneCountInString(file.Path))), 0),
				countWidth+2,
			)
			rows = append(rows, chat.LineFrom(
				chat.StyledSpan(fmt.Sprintf("%-*s", kindWidth, file.Kind()), chat.Muted()),
				chat.RawSpan(truncateToCells(file.Path, pathWidth, TruncatePlain)),
				chat.StyledSpan(fmt.Sprintf("%*s", width, counts), chat.Muted()),
			))
		}
		frame.RenderWidget(dialogsParagraph(rows), listArea)
		renderSessionScrollbar(
			frame,
			uv.Rect(listArea.Max.X, listArea.Min.Y, 1, listArea.Dy()),
			len(files),
			scroll,
			max(listArea.Dy(), 1),
		)
	}
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlChangedFilesRefresh, Label: "Refresh (r)", Enabled: true},
		{ID: DialogControlChangedFilesClose, Label: "Close", Enabled: true},
	})
	form.EndFrame(DialogControlChangedFilesClose)
}

func renderRenameEditor(frame *chat.Frame, area uv.Rectangle, editor *RenameEditor, surfaces *chat.FrameSurfaces) {
	renderTextPrompt(frame, area, &editor.form, &editor.title, "Session: "+editor.sessionName, "Rename session", surfaces)
}

func renderConfigIDEditor(frame *chat.Frame, area uv.Rectangle, editor *ConfigIdEditor, surfaces *chat.FrameSurfaces) {
	renderTextPrompt(
		frame,
		area,
		&editor.form,
		&editor.value,
		fmt.Sprintf("Current %s ID: %s", editor.kind.label(), editor.oldID),
		fmt.Sprintf("Rename %s ID", editor.kind.label()),
		surfaces,
	)
}

func renderTargetActions(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *TargetActionsDialog, surfaces *chat.FrameSurfaces) {
	config := dashboard.config.Get()
	rows := make([]chat.Line, 0, len(dialog.targetIDs))
	for _, id := range dialog.targetIDs {
		kind := "missing"
		if target, ok := config.Targets[id]; ok {
			kind = targetKindLabel(target)
		}
		rows = append(rows, chat.LineFrom(
			chat.StyledSpan(fmt.Sprintf("%-24s ", id), chat.Title(false)),
			chat.StyledSpan(kind, chat.Muted()),
		))
	}
	listRows := rows
	if len(rows) == 0 {
		listRows = []chat.Line{chat.StyledLine("No targets configured.", chat.Style{}.Foreground(chat.ThemePalette().Muted))}
	}
	height := max(len(listRows)+8, 12)
	popup := centeredModal(frame, surfaces, 72, height, area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	listHeight := min(len(listRows), max(inner.Dy()-4, 0))
	listArea := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(listHeight, 1))
	statusY := listArea.Max.Y + 1
	if dialog.testing != nil {
		cancelHint := " Esc cancels test"
		if key, ok := dashboard.FirstKeyLabel(CommandIdCancelOperation); ok {
			cancelHint = " " + key + " cancels test"
		}
		frame.RenderWidget(
			dialogsParagraph([]chat.Line{chat.LineFrom(
				chat.CompactSpan(config.Spinner, chat.ElapsedMs()),
				chat.StyledSpan(
					" Testing "+*dialog.testing+chat.ThemeGlyphs().Ellipsis,
					chat.Style{}.Foreground(chat.ThemePalette().Accent),
				),
				chat.StyledSpan(cancelHint, chat.Muted()),
			)}),
			uv.Rect(inner.Min.X, statusY, inner.Dx(), 1),
		)
	} else if result := dialog.result; result != nil {
		text := result.targetID + ": ready"
		color := chat.ThemePalette().Success
		if result.failure != nil {
			text = result.targetID + ": " + *result.failure
			color = chat.ThemePalette().Warning
		}
		frame.RenderWidget(
			dialogsTextParagraph(text).WithStyle(chat.Style{}.Foreground(color)),
			uv.Rect(inner.Min.X, statusY, inner.Dx(), 1),
		)
	}
	hint := uv.Rect(inner.Min.X, max(inner.Max.Y-2, 0), inner.Dx(), 1)
	frame.RenderWidget(
		dialogsTextParagraph("Up/Down selects target · Tab selects action · Esc closes").
			WithStyle(chat.Style{}.Foreground(chat.ThemePalette().Muted)),
		hint,
	)
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Target actions", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	chat.ChoiceListRender(frame, listArea, listRows, dialog.targetIndex, &form.Form, DialogControlTargetList)
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlTargetRename, Label: "Rename", Enabled: true},
		{ID: DialogControlTargetTest, Label: "Test", Enabled: dialog.testing == nil},
		{ID: DialogControlTargetSettings, Label: "Settings", Enabled: dialog.testing == nil},
	})
	form.EndFrame(DialogControlTargetList)
}

func targetKindLabel(target core.TargetTemplate) string {
	switch target.(type) {
	case core.TargetTemplateLocalBare:
		return "local bare"
	case core.TargetTemplateLocalPodman:
		return "local Podman"
	case core.TargetTemplateLocalDocker:
		return "local Docker"
	case core.TargetTemplateAppleContainer:
		return "Apple container"
	case core.TargetTemplateAwsEc2:
		return "AWS EC2"
	case core.TargetTemplateSshBare:
		return "SSH bare"
	case core.TargetTemplateSshPodman:
		return "SSH Podman"
	case core.TargetTemplateSshDocker:
		return "SSH Docker"
	case core.TargetTemplateSshAppleContainer:
		return "SSH Apple container"
	case core.TargetTemplateSshNitro:
		return "EC2 Nitro sandbox"
	}
	panic(fmt.Sprintf("unknown target template %T", target))
}

// dialogsLines is Rust's str::lines: the text split at line feeds, without a
// trailing empty line and with a carriage return before each feed removed.
func dialogsLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for index, line := range lines {
		lines[index] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

func renderWebDialog(frame *chat.Frame, area uv.Rectangle, dialog *WebDialog, surfaces *chat.FrameSurfaces) {
	// The QR is the widest single element, so it decides the box width and
	// only shows when the terminal can hold it plus a border and the footer
	// rows.
	var qrLines []string
	if dialog.qr != nil {
		qrLines = dialogsLines(*dialog.qr)
	}
	qrWidth, hasQR := 0, len(qrLines) > 0
	for _, line := range qrLines {
		qrWidth = max(qrWidth, utf8.RuneCountInString(line))
	}
	// Size against the region a modal may occupy so the QR fit and the box
	// width both respect the screen-edge margin that centering will enforce.
	innerArea := modalArea(area)
	maxInner := max(innerArea.Dx()-2, 0)
	maxQRHeight := max(innerArea.Dy()-6, 0)
	showQR := hasQR && qrWidth <= maxInner && len(qrLines) <= maxQRHeight

	innerWidth := webDialogMinInnerWidth
	var lines []chat.Line
	switch {
	case dialog.confirmStop != nil:
		process := dialog.confirmStop
		innerWidth = 60
		lines = append(lines,
			chat.StyledLine("Stop this Mjolnir server?", dialogsWarningStyle()),
			chat.RawLine(fmt.Sprintf("%s · PID %d", process.Name, process.Pid)),
			chat.RawLine(process.Executable),
			chat.RawLine(""),
			chat.RawLine("Other viewers and dashboards using that server will be disconnected. Mjolnir will request a graceful stop, then retry this port."),
		)
	case dialog.loading:
		lines = append(lines, chat.StyledLine("Starting web viewer"+chat.ThemeGlyphs().Ellipsis, dialogsWarningStyle()))
	case dialog.message != nil:
		innerWidth = 60
		for _, line := range dialogsLines(*dialog.message) {
			lines = append(lines, chat.StyledLine(line, dialogsWarningStyle()))
		}
		if address := dialog.failedAddress; address != nil {
			lines = append(lines,
				chat.RawLine("Address: "+address.String()),
				chat.RawLine(""),
				chat.RawLine("Use another port to get connected now. The new port lasts until the daemon restarts."),
			)
			if dialog.portConflict {
				lines = append(lines, chat.RawLine("Inspect the port to see which process is using it."))
			}
		}
		if dialog.inspecting {
			lines = append(lines, chat.StyledLine(
				"Inspecting listener"+chat.ThemeGlyphs().Ellipsis,
				chat.Style{}.Foreground(chat.ThemePalette().Accent),
			))
		}
		if message := dialog.inspectionMessage; message != nil {
			lines = append(lines, chat.RawLine(""))
			for _, line := range dialogsLines(*message) {
				lines = append(lines, chat.RawLine(line))
			}
		}
		if dialog.listenerIndex >= 0 && dialog.listenerIndex < len(dialog.listeners) {
			process := dialog.listeners[dialog.listenerIndex]
			lines = append(lines,
				chat.RawLine(""),
				chat.RawLine(fmt.Sprintf(
					"Process %d of %d: %s (PID %d)",
					dialog.listenerIndex+1,
					len(dialog.listeners),
					process.Name,
					process.Pid,
				)),
				chat.RawLine(process.Executable),
			)
			if reason := process.StopDisabledReason; reason != nil {
				lines = append(lines, chat.RawLine(*reason))
			}
		}
	default:
		if showQR {
			innerWidth = max(innerWidth, qrWidth)
			for _, line := range qrLines {
				lines = append(lines, chat.RawLine(line).Centered())
			}
			lines = append(lines, chat.RawLine(""))
		} else if hasQR {
			lines = append(lines,
				chat.StyledLine("Terminal is too small for a scannable QR code.", dialogsWarningStyle()).Centered(),
				chat.RawLine(""),
			)
		}
		if url := dialog.viewerURL; url != nil {
			// The QR encodes this URL; the text is the fallback for hand
			// entry, so it wraps within the box instead of widening it.
			lines = append(lines, chat.LineFrom(
				chat.StyledSpan("Web: ", chat.Style{}.Foreground(chat.ThemePalette().Muted)),
				chat.StyledSpan(*url, chat.Actionable().Underlined()),
			))
		}
		if code := dialog.viewerCode; code != nil {
			lines = append(lines, chat.LineFrom(
				chat.StyledSpan("Viewer code: ", chat.Style{}.Foreground(chat.ThemePalette().Muted)),
				chat.StyledSpan(*code, chat.Style{}.Foreground(chat.ThemePalette().Accent)),
			))
		}
		if reason := dialog.fallbackReason; reason != nil {
			lines = append(lines, chat.StyledLine("Local fallback: "+*reason, dialogsWarningStyle()))
		}
	}
	lines = append(lines, chat.RawLine(""))

	innerWidth = max(min(innerWidth, maxInner), 1)
	boxWidth := innerWidth + 2
	paragraph := dialogsParagraph(lines).WithWrap(chat.Wrap{Trim: false})
	wrapped := min(paragraph.LineCount(max(boxWidth-2, 0)), dialogsMaxU16)
	buttonRows := dialog.buttonRows()
	footerHeight := len(buttonRows)
	boxHeight := min(wrapped+2+footerHeight, innerArea.Dy())
	popup := centeredModalFixed(frame, surfaces, boxWidth, boxHeight, area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	body := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-footerHeight, 0))
	frame.RenderWidget(paragraph, body)
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Web viewer", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	footerTop := max(max(inner.Max.Y-footerHeight, 0), inner.Min.Y)
	for index, buttons := range buttonRows {
		y := footerTop + index
		footer := uv.Rectangle{}
		if y < inner.Max.Y {
			footer = uv.Rect(inner.Min.X, y, inner.Dx(), 1)
		}
		form.RenderActions(frame, footer, buttons)
	}
	form.EndFrame(dialog.defaultControl())
}

// renderQR draws data as a QR code of half-block characters, two module rows
// per text line, with a quiet zone of qrQuietZone modules.
//
// The symbol is the one Rust's qrcode crate builds for the same data at
// error-correction level L: its segmentation into numeric, alphanumeric,
// byte and Kanji runs (qrSegments), the smallest version that holds them,
// and the mask with the lowest penalty score (qrPenalty), the first on a
// tie. rsc.io/qr/coding does the encoding, error correction and placement.
func renderQR(data string) (string, error) {
	code, err := qrEncode([]byte(data))
	if err != nil {
		return "", fmt.Errorf("encode web login QR: %w", err)
	}
	total := code.Size + qrQuietZone*2
	module := func(x, y int) bool {
		x -= qrQuietZone
		y -= qrQuietZone
		if x < 0 || y < 0 {
			return false
		}
		return x < code.Size && y < code.Size && code.Black(x, y)
	}
	var output strings.Builder
	for y := 0; y < total; y += 2 {
		for x := 0; x < total; x++ {
			switch top, bottom := module(x, y), module(x, y+1); {
			case top && bottom:
				output.WriteRune('█')
			case top:
				output.WriteRune('▀')
			case bottom:
				output.WriteRune('▄')
			default:
				output.WriteRune(' ')
			}
		}
		output.WriteByte('\n')
	}
	return output.String(), nil
}

// errQRDataTooLong is the qrcode crate's QrError::DataTooLong.
var errQRDataTooLong = errors.New("data too long")

// qrMode is a QR data mode (the qrcode crate's Mode).
type qrMode uint8

const (
	qrModeNumeric qrMode = iota
	qrModeAlphanumeric
	qrModeByte
	qrModeKanji
)

// qrModeMax is the lowest mode both a and b can encode with (Mode::max over
// its partial order: Numeric < Alphanumeric, everything < Byte).
func qrModeMax(a, b qrMode) qrMode {
	switch {
	case a == b:
		return a
	case (a == qrModeNumeric && b == qrModeAlphanumeric) || b == qrModeByte:
		return b
	case (a == qrModeAlphanumeric && b == qrModeNumeric) || a == qrModeByte:
		return a
	default:
		return qrModeByte
	}
}

// qrSizeClass is which of the three character-count widths a version uses.
func qrSizeClass(version coding.Version) int {
	switch {
	case version <= 9:
		return 0
	case version <= 26:
		return 1
	default:
		return 2
	}
}

var qrLengthBits = [4][3]int{
	qrModeNumeric:      {10, 12, 14},
	qrModeAlphanumeric: {9, 11, 13},
	qrModeByte:         {8, 16, 16},
	qrModeKanji:        {8, 10, 12},
}

// qrSegment is the qrcode crate's Segment: data[begin:end] in one mode.
type qrSegment struct {
	mode       qrMode
	begin, end int
}

// encodedLen is Segment::encoded_len: the mode indicator, the character
// count and the data bits.
func (s qrSegment) encodedLen(version coding.Version) int {
	chars := s.end - s.begin
	data := 0
	switch s.mode {
	case qrModeNumeric:
		data = (chars*10 + 2) / 3
	case qrModeAlphanumeric:
		data = (chars*11 + 1) / 2
	case qrModeByte:
		data = chars * 8
	case qrModeKanji:
		chars /= 2
		data = chars * 13
	}
	return 4 + qrLengthBits[s.mode][qrSizeClass(version)] + data
}

// qrCharSet is the qrcode crate's ExclCharSet: the groups a byte falls in
// when choosing a mode.
func qrCharSet(c byte) int {
	switch {
	case c == 0x20 || c == 0x24 || c == 0x25 || c == 0x2a || c == 0x2b || (0x2d <= c && c <= 0x2f) || c == 0x3a:
		return 1 // Symbol
	case '0' <= c && c <= '9':
		return 2 // Numeric
	case 'A' <= c && c <= 'Z':
		return 3 // Alpha
	case 0x81 <= c && c <= 0x9f:
		return 4 // KanjiHi1
	case 0xe0 <= c && c <= 0xea:
		return 5 // KanjiHi2
	case c == 0xeb:
		return 6 // KanjiHi3
	case c == 0x40 || (0x5b <= c && c <= 0x7e) || c == 0x80 || (0xa0 <= c && c <= 0xbf):
		return 7 // KanjiLo1
	case (0xc0 <= c && c <= 0xdf) || (0xec <= c && c <= 0xfc):
		return 8 // KanjiLo2
	default:
		return 9 // Byte
	}
}

// The parser's states and actions (the qrcode crate's State and Action).
const (
	qrStateInit      = 0
	qrStateNumeric   = 10
	qrStateAlpha     = 20
	qrStateByte      = 30
	qrStateKanjiHi12 = 40
	qrStateKanjiHi3  = 50
	qrStateKanji     = 60
)

type qrAction uint8

const (
	qrActionIdle qrAction = iota
	qrActionNumeric
	qrActionAlpha
	qrActionByte
	qrActionKanji
	qrActionKanjiAndSingleByte
)

type qrTransition struct {
	state  int
	action qrAction
}

// qrStateTransition is STATE_TRANSITION[state + character set]. The columns
// are End, Symbol, Numeric, Alpha, KanjiHi1, KanjiHi2, KanjiHi3, KanjiLo1,
// KanjiLo2, Byte.
var qrStateTransition = [70]qrTransition{
	// Init state:
	{qrStateInit, qrActionIdle}, {qrStateAlpha, qrActionIdle}, {qrStateNumeric, qrActionIdle},
	{qrStateAlpha, qrActionIdle}, {qrStateKanjiHi12, qrActionIdle}, {qrStateKanjiHi12, qrActionIdle},
	{qrStateKanjiHi3, qrActionIdle}, {qrStateByte, qrActionIdle}, {qrStateByte, qrActionIdle},
	{qrStateByte, qrActionIdle},
	// Numeric state:
	{qrStateInit, qrActionNumeric}, {qrStateAlpha, qrActionNumeric}, {qrStateNumeric, qrActionIdle},
	{qrStateAlpha, qrActionNumeric}, {qrStateKanjiHi12, qrActionNumeric}, {qrStateKanjiHi12, qrActionNumeric},
	{qrStateKanjiHi3, qrActionNumeric}, {qrStateByte, qrActionNumeric}, {qrStateByte, qrActionNumeric},
	{qrStateByte, qrActionNumeric},
	// Alpha state:
	{qrStateInit, qrActionAlpha}, {qrStateAlpha, qrActionIdle}, {qrStateNumeric, qrActionAlpha},
	{qrStateAlpha, qrActionIdle}, {qrStateKanjiHi12, qrActionAlpha}, {qrStateKanjiHi12, qrActionAlpha},
	{qrStateKanjiHi3, qrActionAlpha}, {qrStateByte, qrActionAlpha}, {qrStateByte, qrActionAlpha},
	{qrStateByte, qrActionAlpha},
	// Byte state:
	{qrStateInit, qrActionByte}, {qrStateAlpha, qrActionByte}, {qrStateNumeric, qrActionByte},
	{qrStateAlpha, qrActionByte}, {qrStateKanjiHi12, qrActionByte}, {qrStateKanjiHi12, qrActionByte},
	{qrStateKanjiHi3, qrActionByte}, {qrStateByte, qrActionIdle}, {qrStateByte, qrActionIdle},
	{qrStateByte, qrActionIdle},
	// KanjiHi12 state:
	{qrStateInit, qrActionKanjiAndSingleByte}, {qrStateAlpha, qrActionKanjiAndSingleByte},
	{qrStateNumeric, qrActionKanjiAndSingleByte}, {qrStateKanji, qrActionIdle}, {qrStateKanji, qrActionIdle},
	{qrStateKanji, qrActionIdle}, {qrStateKanji, qrActionIdle}, {qrStateKanji, qrActionIdle},
	{qrStateKanji, qrActionIdle}, {qrStateByte, qrActionKanjiAndSingleByte},
	// KanjiHi3 state:
	{qrStateInit, qrActionKanjiAndSingleByte}, {qrStateAlpha, qrActionKanjiAndSingleByte},
	{qrStateNumeric, qrActionKanjiAndSingleByte}, {qrStateKanji, qrActionIdle}, {qrStateKanji, qrActionIdle},
	{qrStateKanjiHi12, qrActionKanjiAndSingleByte}, {qrStateKanjiHi3, qrActionKanjiAndSingleByte},
	{qrStateKanji, qrActionIdle}, {qrStateByte, qrActionKanjiAndSingleByte},
	{qrStateByte, qrActionKanjiAndSingleByte},
	// Kanji state:
	{qrStateInit, qrActionKanji}, {qrStateAlpha, qrActionKanji}, {qrStateNumeric, qrActionKanji},
	{qrStateAlpha, qrActionKanji}, {qrStateKanjiHi12, qrActionIdle}, {qrStateKanjiHi12, qrActionIdle},
	{qrStateKanjiHi3, qrActionIdle}, {qrStateByte, qrActionKanji}, {qrStateByte, qrActionKanji},
	{qrStateByte, qrActionKanji},
}

// qrParse is the qrcode crate's Parser: data split into runs that each fit
// one mode exactly, before any merging.
func qrParse(data []byte) []qrSegment {
	var segments []qrSegment
	state, begin := qrStateInit, 0
	for index := 0; index <= len(data); index++ {
		set := 0 // End
		if index < len(data) {
			set = qrCharSet(data[index])
		}
		transition := qrStateTransition[state+set]
		state = transition.state
		var mode qrMode
		switch transition.action {
		case qrActionIdle:
			continue
		case qrActionNumeric:
			mode = qrModeNumeric
		case qrActionAlpha:
			mode = qrModeAlphanumeric
		case qrActionByte:
			mode = qrModeByte
		case qrActionKanji:
			mode = qrModeKanji
		case qrActionKanjiAndSingleByte:
			nextBegin := index - 1
			if begin == nextBegin {
				mode = qrModeByte
			} else {
				segments = append(segments,
					qrSegment{mode: qrModeKanji, begin: begin, end: nextBegin},
					qrSegment{mode: qrModeByte, begin: nextBegin, end: nextBegin + 1},
				)
				begin = nextBegin + 1
				continue
			}
		}
		segments = append(segments, qrSegment{mode: mode, begin: begin, end: index})
		begin = index
	}
	return segments
}

// qrOptimize is the qrcode crate's Optimizer: adjacent runs merge, left to
// right, while the merged run is no longer than the two apart.
func qrOptimize(segments []qrSegment, version coding.Version) []qrSegment {
	if len(segments) == 0 {
		return nil
	}
	var optimized []qrSegment
	last := segments[0]
	lastSize := last.encodedLen(version)
	for _, segment := range segments[1:] {
		size := segment.encodedLen(version)
		merged := qrSegment{mode: qrModeMax(last.mode, segment.mode), begin: last.begin, end: segment.end}
		mergedSize := merged.encodedLen(version)
		if lastSize+size >= mergedSize {
			last, lastSize = merged, mergedSize
		} else {
			optimized = append(optimized, last)
			last, lastSize = segment, size
		}
	}
	return append(optimized, last)
}

// qrKanji is the Kanji mode encoding (the qrcode crate's push_kanji_data),
// which rsc.io/qr/coding does not provide: Shift JIS byte pairs, 13 bits
// each.
type qrKanji string

func (s qrKanji) Check() error {
	if len(s)%2 != 0 {
		return errors.New("invalid character")
	}
	return nil
}

func (s qrKanji) Bits(version coding.Version) int {
	return 4 + qrLengthBits[qrModeKanji][qrSizeClass(version)] + 13*(len(s)/2)
}

func (s qrKanji) Encode(bits *coding.Bits, version coding.Version) {
	bits.Write(8, 4)
	bits.Write(uint(len(s)/2), qrLengthBits[qrModeKanji][qrSizeClass(version)])
	for index := 0; index+1 < len(s); index += 2 {
		point := uint(s[index])<<8 | uint(s[index+1])
		if point < 0xe040 {
			point -= 0x8140
		} else {
			point -= 0xc140
		}
		bits.Write(((point>>8)*0xc0+(point&0xff))&0x1fff, 13)
	}
}

// qrEncode builds the symbol the qrcode crate builds for data at level L.
func qrEncode(data []byte) (*coding.Code, error) {
	segments := qrParse(data)
	for _, optimizeFor := range []coding.Version{9, 26, 40} {
		optimized := qrOptimize(segments, optimizeFor)
		totalBits := 0
		for _, segment := range optimized {
			totalBits += segment.encodedLen(optimizeFor)
		}
		if totalBits > optimizeFor.DataBytes(coding.L)*8 {
			continue
		}
		version := coding.Version(coding.MinVersion)
		for version.DataBytes(coding.L)*8 < totalBits {
			version++
		}
		encodings := make([]coding.Encoding, 0, len(optimized))
		for _, segment := range optimized {
			text := string(data[segment.begin:segment.end])
			switch segment.mode {
			case qrModeNumeric:
				encodings = append(encodings, coding.Num(text))
			case qrModeAlphanumeric:
				encodings = append(encodings, coding.Alpha(text))
			case qrModeByte:
				encodings = append(encodings, coding.String(text))
			case qrModeKanji:
				encodings = append(encodings, qrKanji(text))
			}
		}
		var best *coding.Code
		bestPenalty := 0
		for mask := coding.Mask(0); mask < 8; mask++ {
			plan, err := coding.NewPlan(version, coding.L, mask)
			if err != nil {
				return nil, err
			}
			code, err := plan.Encode(encodings...)
			if err != nil {
				return nil, err
			}
			// The first mask with the lowest penalty wins, as Rust's
			// min_by_key keeps the first minimum.
			if penalty := qrPenalty(code); best == nil || penalty < bestPenalty {
				best, bestPenalty = code, penalty
			}
		}
		return best, nil
	}
	return nil, errQRDataTooLong
}

// qrPenalty is the qrcode crate's compute_total_penalty_scores for a normal
// QR symbol: runs of one colour, 2×2 blocks, finder-like patterns and the
// dark/light balance.
func qrPenalty(code *coding.Code) int {
	width := code.Size
	dark := func(x, y int) bool { return code.Black(x, y) }
	total := 0
	for _, horizontal := range []bool{true, false} {
		at := func(i, j int) bool {
			if horizontal {
				return dark(j, i)
			}
			return dark(i, j)
		}
		// Every 5+N adjacent modules of one colour in a row or column score
		// 3+N.
		for i := 0; i < width; i++ {
			run := 1
			for j := 1; j <= width; j++ {
				if j < width && at(i, j) == at(i, j-1) {
					run++
					continue
				}
				if run >= 5 {
					total += run - 2
				}
				run = 1
			}
		}
	}
	// Every 2×2 block of one colour, overlapping blocks counted, scores 3.
	for i := 0; i < width-1; i++ {
		for j := 0; j < width-1; j++ {
			this := dark(i, j)
			if this == dark(i+1, j) && this == dark(i, j+1) && this == dark(i+1, j+1) {
				total += 3
			}
		}
	}
	// Every `#.###.#` without four light modules on one side scores 40; the
	// three real finder patterns account for the 360 taken off.
	pattern := [7]bool{true, false, true, true, true, false, true}
	for _, horizontal := range []bool{true, false} {
		score := 0
		for i := 0; i < width; i++ {
			at := func(k int) bool {
				if horizontal {
					return dark(k, i)
				}
				return dark(i, k)
			}
			for j := 0; j < width-6; j++ {
				matches := true
				for k := range pattern {
					if at(j+k) != pattern[k] {
						matches = false
						break
					}
				}
				if !matches {
					continue
				}
				check := func(k int) bool { return 0 <= k && k < width && at(k) }
				before, after := false, false
				for k := j - 4; k < j; k++ {
					before = before || check(k)
				}
				for k := j + 7; k < j+11; k++ {
					after = after || check(k)
				}
				if !before || !after {
					score += 40
				}
			}
		}
		total += score - 360
	}
	// The deviation from half dark modules, scored linearly up to 100.
	darkModules := 0
	for y := 0; y < width; y++ {
		for x := 0; x < width; x++ {
			if dark(x, y) {
				darkModules++
			}
		}
	}
	ratio := darkModules * 200 / (width * width)
	if ratio >= 100 {
		total += ratio - 100
	} else {
		total += 100 - ratio
	}
	return total
}

func renderRepositoryOrigin(frame *chat.Frame, area uv.Rectangle, dialog *RepositoryOriginDialog, surfaces *chat.FrameSurfaces) {
	lines := []chat.Line{
		chat.RawLine("Repository: " + dialog.repositoryID),
		chat.RawLine(""),
		chat.RawLine(fmt.Sprintf("The configured source does not contain checkpoint base %s.", dialog.missingCommit)),
		chat.RawLine("Checkpoint origin: " + dialog.archivedOrigin),
		chat.RawLine("Configured source checked: " + dialog.configuredOrigin),
		chat.RawLine(""),
		chat.RawLine("Enter a GitHub origin or absolute local path that contains this history:"),
	}
	if dialog.error != nil {
		lines = append(lines, chat.StyledLine(*dialog.error, dialogsWarningStyle()))
	}
	bodyParagraph := dialogsParagraph(lines).WithWrap(chat.Wrap{Trim: false})
	popup := centeredModal(frame, surfaces, 76, popupHeight(&bodyParagraph, 76, 14, area), area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	const controlsHeight = 5
	text := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-controlsHeight, 0))
	frame.RenderWidget(bodyParagraph, text)
	fieldY := inner.Min.Y + text.Dy()
	labelWidth := min(8, inner.Dx())
	frame.RenderWidget(dialogsTextParagraph("Source:"), uv.Rect(inner.Min.X, fieldY, labelWidth, 1))
	field := uv.Rect(inner.Min.X+labelWidth, fieldY, max(inner.Dx()-8, 0), 1)
	hintY := max(inner.Max.Y-3, 0)
	frame.RenderWidget(
		dialogsTextParagraph("Type or paste into Source · Tab moves · Enter checks").
			WithStyle(chat.Style{}.Foreground(chat.ThemePalette().Muted)),
		uv.Rect(inner.Min.X, hintY, inner.Dx(), 1),
	)
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), 1)
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Repository history is missing", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	chat.PathFieldRender(frame, field, &dialog.replacement, &form.Form, DialogControlField)
	form.RenderActions(frame, footer, []chat.ButtonSpec[DialogControl]{
		{ID: DialogControlCancel, Label: "Cancel", Enabled: true},
		{ID: DialogControlPrimary, Label: "Check origin", Enabled: true},
	})
	form.EndFrame(DialogControlField)
}

// sessionLine is the line that says which session a dialog is about: its
// name when the caller knows one (sessionName non-nil and not empty), else
// its id.
func sessionLine(sessionID string, sessionName *string, width int) chat.Line {
	name := sessionID
	if sessionName != nil && *sessionName != "" {
		name = *sessionName
	}
	name = FitSessionName(name, max(width-len("Session: "), 0))
	return chat.RawLine("Session: " + name)
}

// confirmationBody is the title and body of one confirmation, without its
// buttons.
//
// Split out so the wording a dialog shows can be asserted without rendering
// a frame and reading cells back.
func confirmationBody(confirmation Confirmation, sessionName *string, width int) (string, []chat.Line) {
	warning := dialogsWarningStyle()
	switch c := confirmation.(type) {
	case ConfirmationRepairRepositoryRemotes:
		lines := make([]chat.Line, 0, 6*len(c.Repairs))
		for _, repair := range c.Repairs {
			lines = append(lines,
				chat.RawLine(repair.Path),
				chat.RawLine(fmt.Sprintf("Branch %s tracks missing remote %s.", repair.Branch, repair.MissingRemote)),
				chat.RawLine(fmt.Sprintf("Set its tracking remote to %s.", repair.ReplacementRemote)),
				chat.RawLine("Fetch: "+repair.FetchURL),
				chat.RawLine("Push: "+strings.Join(repair.PushURLs, ", ")),
				chat.RawLine(""),
			)
		}
		return "Repair Git tracking?", lines
	case ConfirmationConfigurationRepair:
		return " Configuration repair ", []chat.Line{
			chat.RawLine(c.Error),
			chat.RawLine(""),
			chat.RawLine("Open settings to restore the named entries. The session and its history are retained."),
			chat.RawLine("PgUp/PgDn scroll the full details. Esc dismisses."),
		}
	case ConfirmationLaunchFailed:
		resolve := "Dismiss, resolve the problem below, then select the failed session to retry or remove it."
		if c.Retry != nil {
			resolve = "Resolve the problem below, then Retry launch with the same settings."
		}
		lines := []chat.Line{
			chat.RawLine("The session could not start. This message stays until you dismiss it."),
			chat.RawLine(resolve),
			chat.RawLine("PgUp/PgDn scroll the full details. Esc dismisses."),
			chat.RawLine(""),
		}
		if strings.Contains(c.Error, "Operation not permitted") && strings.Contains(c.Error, "chmod") {
			lines = append(lines,
				chat.RawLine("The container user cannot change the uploaded worker's permissions. If using a custom image, try the standard Mjolnir agent image. Include the diagnostic below when reporting this problem."),
				chat.RawLine(""),
			)
		}
		for _, line := range dialogsLines(c.Error) {
			lines = append(lines, chat.RawLine(line))
		}
		return " Launch failed ", lines
	case ConfirmationImportFailed:
		lines := []chat.Line{
			chat.RawLine("The session was not imported."),
			chat.RawLine("Resolve the problem below, then import the session again."),
			chat.RawLine("PgUp/PgDn scroll the full details. Esc dismisses."),
			chat.RawLine(""),
		}
		for _, line := range dialogsLines(c.Error) {
			lines = append(lines, chat.RawLine(line))
		}
		return " Import failed ", lines
	case ConfirmationDismiss:
		if c.Intent == DismissalIntentCancelImport {
			return " Cancel import? ", []chat.Line{
				chat.RawLine("The active import has not finished."),
				chat.RawLine("Keep importing, or cancel the operation?"),
			}
		}
		return " Discard Settings changes? ", []chat.Line{
			chat.RawLine("Settings has unsaved changes."),
			chat.RawLine("Keep editing to preserve them, or discard the draft."),
		}
	case ConfirmationInstallMbx:
		lines := []chat.Line{
			chat.RawLine(fmt.Sprintf(
				"This installs the pinned mbx release (a build cache shared by your sessions) on %s and runs `mbx setup --yes`.",
				c.Host,
			)),
			chat.RawLine(""),
			chat.RawLine(fmt.Sprintf("It edits `%s` to add mbx to PATH.", c.ProfileFile)),
		}
		if c.ProfileWarning != nil {
			lines = append(lines, chat.RawLine(""), chat.RawLine(*c.ProfileWarning))
		} else {
			lines = append(lines, chat.RawLine("New login shells will pick up the PATH change."))
		}
		if c.ManualPathLine != nil {
			lines = append(lines, chat.RawLine(*c.ManualPathLine))
		}
		if c.Upgrading {
			return " Upgrade mbx? ", lines
		}
		return " Install mbx? ", lines
	case ConfirmationConvertRawCheckout:
		preview := c.Preview
		lines := []chat.Line{chat.RawLine(preview.SummaryLine()), chat.RawLine("")}
		for _, line := range preview.WarningLines() {
			lines = append(lines, chat.StyledLine(line, warning))
		}
		return " Move this checkout into the target? ", lines
	case ConfirmationDestroyStopped:
		return " Destroy suspended session? ", []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.RawLine("Mjolnir will permanently destroy the recovery archive and session record."),
			chat.RawLine("The managed clone and any unpublished work in its recovery copy will be removed."),
		}
	case ConfirmationCloseFailed:
		lines := []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.StyledLine("Suspension failed: "+c.Error, warning),
		}
		if nestedRepositoryCheckpointFailure(c.Error) {
			lines = append(lines,
				chat.RawLine(""),
				chat.StyledLine("Ignore and Suspend omits untracked nested Git repositories from the recovery copy.", warning),
				chat.StyledLine("Files inside them will not be recoverable after suspension removes the environment.", warning),
			)
		}
		return " Suspension could not complete ", lines
	case ConfirmationRestartAgent:
		interrupted := ""
		if c.Interrupting {
			interrupted = "Any running turn will be interrupted."
		}
		return " Restart agent? ", []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.RawLine("Restart the worker and harness inside the existing container."),
			chat.RawLine("The container filesystem, workspace, and caches are preserved."),
			chat.RawLine(interrupted),
		}
	case ConfirmationRestartContainer:
		title := " Restart agent "
		switch {
		case c.Preview.Settings != nil:
			title = " Apply container settings and restart? "
		case c.UpgradeOnly:
			title = " Upgrade container and restart? "
		}
		targetImageID := "not cached"
		if c.Preview.TargetImageID != nil {
			targetImageID = *c.Preview.TargetImageID
		}
		return title, []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.RawLine(fmt.Sprintf("Current image: %s (%s)", c.Preview.CurrentImage, c.Preview.CurrentImageID)),
			chat.RawLine(fmt.Sprintf("Target image: %s (%s)", c.Preview.TargetImage, targetImageID)),
			chat.RawLine("Restart agent keeps the current container and its filesystem; it interrupts any running turn."),
			chat.RawLine("Container upgrade reattaches the existing workspace and build cache mounts."),
			chat.StyledLine("Writable-layer state, including manually installed packages, may be lost.", warning),
			chat.RawLine("A checkpoint does not preserve the whole environment."),
			chat.RawLine("Upgrades wait for accepted work; pending upgrades can be cancelled."),
		}
	case ConfirmationInterruptWork:
		title := " Suspend while working? "
		consequence := "Suspending ends that turn, saves a recovery copy, and frees the target."
		if c.Restart {
			title = " Restart while working? "
			consequence = "Restarting ends that turn; the workspace and the conversation so far are kept."
		}
		return title, []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.StyledLine("The agent is in the middle of a turn.", warning),
			chat.RawLine(consequence),
		}
	case ConfirmationInterruptAll:
		subagents := counted(c.SubagentsRunning, "sub-agent", "sub-agents")
		var running string
		switch {
		case c.ParentRunning && c.SubagentsRunning == 0:
			running = "This session's turn is running."
		case c.ParentRunning:
			running = fmt.Sprintf("This session and %s have turns running.", subagents)
		case c.SubagentsRunning == 1:
			running = "1 sub-agent has a turn running."
		default:
			running = fmt.Sprintf("%s have turns running.", subagents)
		}
		return " Interrupt all? ", []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.StyledLine(running, warning),
			chat.RawLine("Interrupting ends those turns. The sessions keep running, and their workspaces and conversations are kept."),
		}
	case ConfirmationSuspendSession:
		return " Suspend session? ", suspendSessionBody(c, sessionName, width)
	case ConfirmationRecoverFailed:
		lines := []chat.Line{sessionLine(c.SessionID, sessionName, width), chat.RawLine("")}
		if c.Error != nil {
			lines = append(lines, chat.StyledLine("Failed: "+*c.Error, warning))
		} else {
			lines = append(lines, chat.RawLine("This session failed without a recorded error."))
		}
		lines = append(lines, chat.RawLine(""))
		switch {
		case c.RetryLaunch:
			lines = append(lines,
				chat.RawLine("Relaunch this session with its saved settings."),
				chat.RawLine("Prompts that failed with the previous launch are not resent."),
			)
		case c.Recoverable:
			lines = append(lines,
				chat.RawLine("Recover repairs the existing environment, or restores its recovery copy if absent."),
				chat.RawLine("Its transcript is readable either way; opening it changes nothing."),
			)
		default:
			lines = append(lines,
				chat.RawLine("There is no retained environment or verified recovery copy to resume."),
				chat.RawLine("Its transcript is still readable."),
			)
		}
		return " Session failed ", lines
	case ConfirmationRecoverMove:
		operation := &c.Operation
		outcome := "needs recovery"
		switch operation.Phase {
		case core.MovePhaseFailed:
			outcome = "failed"
		case core.MovePhaseCancelled:
			outcome = "was cancelled"
		}
		profile := "current profile"
		if operation.Selection.ProfileID != nil {
			profile = *operation.Selection.ProfileID
		}
		target := "current target"
		if operation.Selection.TargetTemplateID != nil {
			target = *operation.Selection.TargetTemplateID
		}
		lines := []chat.Line{
			chat.RawLine("Session: " + operation.Selection.SessionID),
			chat.RawLine(""),
			chat.StyledLine(fmt.Sprintf("Move %s at destination %s / %s.", outcome, profile, target), warning),
		}
		if operation.Error != nil {
			lines = append(lines, chat.StyledLine("Error: "+*operation.Error, warning))
		}
		lines = append(lines, chat.RawLine(""))
		if operation.QueueAdmissionStarted && !operation.QueueAdmissionFinished {
			lines = append(lines,
				chat.RawLine("Some queued work may already have been accepted; only retry on this exact destination."),
				chat.RawLine("Resume with previous settings is unavailable until queue admission finishes."),
			)
		} else {
			lines = append(lines,
				chat.RawLine("Retry move keeps the failed destination and queue choice."),
				chat.RawLine("Resume with previous settings restores the source configuration instead."),
			)
		}
		if operation.Queue == core.ResumeQueueDispositionStart {
			lines = append(lines, chat.RawLine("Queued work was selected to run after the move."))
		} else {
			lines = append(lines, chat.RawLine("Queued work was selected for discard."))
		}
		return " Move recovery ", lines
	case ConfirmationForceDestroy:
		return " Destroy session? ", []chat.Line{
			sessionLine(c.SessionID, sessionName, width),
			chat.RawLine(""),
			chat.RawLine("Permanently destroy this session, its environment, and its recovery archive?"),
			chat.RawLine("Cleanup failures warn about remaining resources; the session is still removed."),
			chat.RawLine("The managed clone and any unpublished work in its recovery copy will be removed."),
		}
	}
	panic(fmt.Sprintf("unknown confirmation %T", confirmation))
}

// suspendSessionBody is the body of the suspend confirmation.
func suspendSessionBody(c ConfirmationSuspendSession, sessionName *string, width int) []chat.Line {
	warning := dialogsWarningStyle()
	lines := []chat.Line{sessionLine(c.SessionID, sessionName, width)}
	preflight := c.Preflight
	switch {
	case c.Checking:
		lines = append(lines, chat.RawLine("Checking workspace…"))
	case preflight != nil && preflight.HasBlockers():
		lines = append(lines, chat.StyledLine(
			"Cannot suspend until these blockers are resolved:",
			chat.Style{}.Foreground(chat.ThemePalette().Error),
		))
		for _, blocker := range preflight.Blockers {
			lines = append(lines, chat.RawLine(fmt.Sprintf("  %s — %s", blocker.Kind, blocker.Remedy)))
		}
	case preflight != nil:
		lines = append(lines,
			chat.RawLine("Save a recovery copy and release the environment."),
			chat.RawLine("You can resume this session later."),
		)
		if preflight.Raw {
			lines = append(lines, chat.StyledLine(
				"Raw checkout is kept in place; untracked files do not travel with the snapshot.",
				warning,
			))
		}
		if len(preflight.Operations) > 0 {
			kinds := make([]string, 0, len(preflight.Operations))
			for _, operation := range preflight.Operations {
				kinds = append(kinds, operation.Kind.String())
			}
			lines = append(lines, chat.StyledLine(
				fmt.Sprintf("In-progress Git operation (%s) will be aborted, discarding conflict resolution.", strings.Join(kinds, ", ")),
				warning,
			))
		}
		if untrackedPaths := preflight.UntrackedDisplayPaths(); len(untrackedPaths) > 0 {
			noun := "files"
			if len(untrackedPaths) == 1 {
				noun = "file"
			}
			lines = append(lines, chat.StyledLine(
				fmt.Sprintf("%d untracked %s will not be kept (ignored files are never kept):", len(untrackedPaths), noun),
				warning,
			))
			limit := 5
			if c.ViewingAllUntracked {
				limit = len(untrackedPaths)
			}
			for _, path := range untrackedPaths[:min(limit, len(untrackedPaths))] {
				lines = append(lines, chat.RawLine("  "+path))
			}
			if !c.ViewingAllUntracked && len(untrackedPaths) > limit {
				lines = append(lines, chat.StyledLine(
					fmt.Sprintf("  and %d more (v to view all)", len(untrackedPaths)-limit),
					chat.Style{}.Foreground(chat.ThemePalette().Muted),
				))
			}
		}
		if preflight.KeepDirBytes > 0 {
			lines = append(lines, chat.RawLine(fmt.Sprintf(".mj/keep/ size: %d bytes (not saved on suspend)", preflight.KeepDirBytes)))
		}
	default:
		lines = append(lines,
			chat.RawLine("Save a recovery copy and release the environment."),
			chat.RawLine("You can resume this session later."),
		)
	}
	if preflight != nil {
		if warningText, ok := core.OwningChildrenSuspendWarning(preflight.OwningChildrenToSuspend); ok {
			lines = append(lines, chat.StyledLine(warningText, warning))
		}
	}
	if c.Interrupting {
		lines = append(lines, chat.RawLine("The current turn will be interrupted."))
	}
	if warningText, ok := core.SuspendWarningNaming(c.ChildrenNotHandedBack); ok {
		lines = append(lines, chat.StyledLine(warningText+".", warning))
	}
	return lines
}

func renderConfirmation(frame *chat.Frame, area uv.Rectangle, dialog *ConfirmDialog, surfaces *chat.FrameSurfaces) {
	confirmation := dialog.confirmation
	// Minimum height per dialog; popupHeight grows it to fit wrapped content.
	var nominal int
	switch confirmation.(type) {
	case ConfirmationConfigurationRepair, ConfirmationLaunchFailed, ConfirmationImportFailed,
		ConfirmationRepairRepositoryRemotes:
		nominal = 16
	case ConfirmationDismiss:
		nominal = 8
	case ConfirmationInstallMbx:
		nominal = 12
	case ConfirmationConvertRawCheckout:
		nominal = 16
	case ConfirmationCloseFailed:
		nominal = 12
	case ConfirmationSuspendSession:
		nominal = 16
	case ConfirmationInterruptWork:
		nominal = 10
	case ConfirmationRestartContainer:
		nominal = 15
	case ConfirmationRestartAgent:
		nominal = 10
	case ConfirmationInterruptAll:
		nominal = 10
	case ConfirmationDestroyStopped:
		nominal = 10
	case ConfirmationRecoverFailed:
		nominal = 12
	case ConfirmationRecoverMove:
		nominal = 14
	case ConfirmationForceDestroy:
		nominal = 11
	}
	bodyWidth := max(centeredRect(72, 1, area).Dx()-2, 0)
	title, lines := confirmationBody(confirmation, dialog.sessionName, bodyWidth)
	buttons := confirmationButtons(confirmation)
	lines = append(lines, chat.RawLine(""), confirmationKeyLine(buttons), chat.RawLine(""))
	paragraph := dialogsParagraph(lines).WithWrap(chat.Wrap{Trim: false})
	innerWidth := max(centeredRect(72, 1, area).Dx()-2, 0)
	buttonsWidth := 0
	for _, label := range buttons {
		buttonsWidth += chat.RawLine(label).Width() + 5
	}
	stacked := max(buttonsWidth-1, 0) > innerWidth
	controlsHeight := 1
	if stacked {
		controlsHeight = len(buttons)
	}
	height := popupHeight(&paragraph, 72, nominal, area) + controlsHeight + 2
	popup := centeredModal(frame, surfaces, 72, height, area)
	inner := dialogsInner(popup)
	if inner.Dy() == 0 {
		clearDialogFormGeometry(&dialog.form)
		return
	}
	body := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-controlsHeight, 0))
	maxScroll := max(min(paragraph.LineCount(max(body.Dx(), 1)), dialogsMaxU16)-body.Dy(), 0)
	dialog.maxScroll = maxScroll
	frame.RenderWidget(paragraph.WithScroll(min(dialog.scroll, maxScroll), 0), body)
	form := &dialog.form
	form.BeginFrame()
	errorStyle := chat.Style{}.Foreground(chat.ThemePalette().Error).Bold()
	titleLine := dismissibleModalTitle(&form.Form, popup, strings.TrimSpace(title), errorStyle, true)
	frame.RenderWidget(
		chat.Modal().
			WithBorderStyle(chat.Style{}.Foreground(chat.ThemePalette().Error)).
			WithTitleStyle(errorStyle).
			WithTitle(titleLine),
		popup,
	)
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-controlsHeight, 0), inner.Dx(), controlsHeight)
	actions := make([]chat.ButtonSpec[DialogControl], 0, len(buttons))
	for index, label := range buttons {
		actions = append(actions, chat.ButtonSpec[DialogControl]{ID: DialogControlConfirmButton(index), Label: label, Enabled: true})
	}
	if stacked {
		for row, action := range actions {
			form.RenderActions(frame, uv.Rect(footer.Min.X, footer.Min.Y+row, footer.Dx(), 1), []chat.ButtonSpec[DialogControl]{action})
		}
	} else {
		form.RenderActions(frame, footer, actions)
	}
	form.EndFrame(DialogControlConfirmButton(initialConfirmationButton(confirmation, buttons)))
}
