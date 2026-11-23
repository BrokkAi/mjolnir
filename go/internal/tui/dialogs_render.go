// Port of mj-tui/src/dialogs/render.rs.
//
// Drawing for the standard dashboard dialogs, and the button lists and body
// text their key handling shares.

package tui

import (
	"fmt"
	"strings"
	"time"
	"unicode/utf8"

	uv "github.com/charmbracelet/ultraviolet"

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
	panic("unported: mj-tui/src/dialogs/render.rs confirmation_buttons")
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
	panic("unported: mj-tui/src/dialogs/render.rs confirmation_accelerators")
}

// confirmationKeyLine is the line under a confirmation's text naming each
// button's letter, in the same order the buttons are drawn:
// `c Cancel · o Continue anyway`.
func confirmationKeyLine(labels []string) chat.Line {
	panic("unported: mj-tui/src/dialogs/render.rs confirmation_key_line")
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
	panic("unported: mj-tui/src/dialogs/render.rs import_progress_mut")
}

func renderImportProgress(frame *chat.Frame, area uv.Rectangle, progress *ImportProgress, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_import_progress")
}

func renderImportBundleConfirmation(frame *chat.Frame, area uv.Rectangle, confirmation *ImportBundleConfirmation, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_import_bundle_confirmation")
}

// renderTextPrompt draws a one-field modal: a header line naming what is
// being renamed, the field itself, and a Cancel/Save footer. The rename
// editors are the two dialogs shaped this way.
func renderTextPrompt(frame *chat.Frame, area uv.Rectangle, form *chat.Dialog[DialogControl], value *chat.TextInput, header string, title string, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_text_prompt")
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
	panic("unported: mj-tui/src/dialogs/render.rs render_notice_log")
}

func renderSessionCpuReport(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *SessionCpuReportDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_session_cpu_report")
}

// renderChangedFiles draws the changed-files overlay: the branch line, the
// totals, then one row per file with its kind and line counts.
func renderChangedFiles(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *ChangedFilesDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_changed_files")
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
	panic("unported: mj-tui/src/dialogs/render.rs render_target_actions")
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

func renderWebDialog(frame *chat.Frame, area uv.Rectangle, dialog *WebDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_web_dialog")
}

// renderQR draws data as a QR code of half-block characters, two module rows
// per text line, with a quiet zone of qrQuietZone modules.
func renderQR(data string) (string, error) {
	panic("unported: mj-tui/src/dialogs/render.rs render_qr")
}

func renderRepositoryOrigin(frame *chat.Frame, area uv.Rectangle, dialog *RepositoryOriginDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_repository_origin")
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
	panic("unported: mj-tui/src/dialogs/render.rs confirmation_body")
}

func renderConfirmation(frame *chat.Frame, area uv.Rectangle, dialog *ConfirmDialog, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/render.rs render_confirmation")
}
