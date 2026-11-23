// Port of mj-tui/src/dialogs.rs.
//
// Modal dialogs: session import, confirmations, and the rename editor.
//
// Rust keeps each dialog's form in a RefCell so rendering through `&self`
// can update it; Go methods take pointers, so the form is a plain
// chat.Dialog value and each dialog's Clone deep-copies it, as Rust's derived
// Clone does. Cell fields are plain values for the same reason.
//
// The submodules are dialogs_render.go, dialogs_container.go and
// dialogs_git_identity.go.

package tui

import (
	"maps"
	"math"
	"net/netip"
	"slices"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const importStallWarningAfter = 10 * time.Second

// DialogControl is a stable control identity used by the standard dashboard
// dialogs.
//
// Each dialog owns its own chat.Dialog, so the shared identities can be
// reused across modes while retaining focus and pointer state during
// redraws. Rust's data-carrying ConfirmButton(usize) variant is
// DialogControlConfirmButton(index): the indices after every unit variant,
// so the identity stays a comparable value.
type DialogControl int

const (
	DialogControlField DialogControl = iota
	DialogControlCancel
	DialogControlSave
	DialogControlPrimary
	DialogControlTargetList
	DialogControlTargetRename
	DialogControlTargetTest
	DialogControlTargetSettings
	DialogControlImportManagedWorktree
	DialogControlImportCancel
	DialogControlImportContinue
	DialogControlWebRetry
	DialogControlWebAnotherPort
	DialogControlWebInspect
	DialogControlWebNextProcess
	DialogControlWebStop
	DialogControlWebConfirmStop
	DialogControlWebCancelStop
	DialogControlChangedFilesRefresh
	DialogControlChangedFilesClose
	DialogControlNoticeLogClose
	DialogControlSessionCpuReportTabs
	DialogControlSessionCpuReportRows
	DialogControlSessionCpuReportHourly
	DialogControlSessionCpuReportRecent
	DialogControlSessionCpuReportClose
	// dialogControlConfirmButtonBase is ConfirmButton(0); ConfirmButton(n)
	// follows it.
	dialogControlConfirmButtonBase
)

// DialogControlConfirmButton is the confirmation button at index, Cancel
// first (Rust's DialogControl::ConfirmButton(usize)).
func DialogControlConfirmButton(index int) DialogControl {
	return dialogControlConfirmButtonBase + DialogControl(index)
}

// confirmButtonIndex is the index of a ConfirmButton control, false for
// every other control.
func (c DialogControl) confirmButtonIndex() (int, bool) {
	if c < dialogControlConfirmButtonBase {
		return 0, false
	}
	return int(c - dialogControlConfirmButtonBase), true
}

// ImportSessionOption is one native harness session the import list offers.
type ImportSessionOption struct {
	NativeSessionID string
	Title           string
	// Cwd is the working directory the harness recorded, as `cwd` shows it on
	// this machine. ProjectDirectory is its display form.
	Cwd               string
	ProjectDirectory  string
	Details           string
	UnavailableReason *string
	// LastActivityMs is when the harness last wrote this session's file, in
	// epoch milliseconds. The resume dialog sorts hel records and native
	// sessions against each other and renders their activity in one column,
	// so the raw instant travels alongside the details.
	LastActivityMs int64
	// NativelyArchived is archived inside the harness itself; only Codex
	// reports this. Hel mirrors it one way and never writes the harness home
	// back.
	NativelyArchived bool
}

// ImportProfileOption is one profile's native sessions in the import list.
type ImportProfileOption struct {
	ProfileID   string
	HarnessKind core.HarnessKind
	Sessions    []ImportSessionOption
	// ScanProgress is (scanned, total) while the scan runs; nil otherwise.
	ScanProgress *[2]int
	Error        *string
}

// NoticeLogDialog is the notice log: the last notices the footer showed,
// newest first, so a burst of background failures that overwrote each other
// can still be read.
type NoticeLogDialog struct {
	// scroll is the first wrapped line drawn.
	scroll int
	// maxScroll is the largest useful scroll, measured by the renderer at its
	// width.
	maxScroll int
	form      chat.Dialog[DialogControl]
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *NoticeLogDialog) Clone() NoticeLogDialog {
	clone := *d
	clone.form = d.form.Clone()
	return clone
}

// SessionCpuReportDialog is the CPU-by-session report.
type SessionCpuReportDialog struct {
	// machineKey is the selected machine's identity, which survives CPU
	// re-sorting and tab list changes.
	machineKey *string
	// selectedSessionID is the selected session's identity, which survives
	// live CPU re-sorting.
	selectedSessionID *string
	// selectedRow is used to choose the nearest row if the selected session
	// leaves the report.
	selectedRow int
	// metric is the figure the report shows, sorts by and totals.
	metric CpuMetric
	form   chat.Dialog[DialogControl]
}

// newSessionCpuReportDialog is SessionCpuReportDialog::new: the default
// report with the session rows focused.
func newSessionCpuReportDialog() SessionCpuReportDialog {
	dialog := SessionCpuReportDialog{form: chat.NewDialog[DialogControl]()}
	dialog.form.Focus(DialogControlSessionCpuReportRows)
	return dialog
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *SessionCpuReportDialog) Clone() SessionCpuReportDialog {
	clone := *d
	if d.machineKey != nil {
		key := *d.machineKey
		clone.machineKey = &key
	}
	if d.selectedSessionID != nil {
		id := *d.selectedSessionID
		clone.selectedSessionID = &id
	}
	clone.form = d.form.Clone()
	return clone
}

// ChangedFilesDialog is the changed-files overlay for one session. The data
// lives on the dashboard (`gitStatus`), so a probe that answers while the
// overlay is open, or after it closed, lands in the same place.
type ChangedFilesDialog struct {
	sessionID string
	// scroll is the first listed file drawn, for a list longer than the
	// overlay.
	scroll int
	form   chat.Dialog[DialogControl]
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *ChangedFilesDialog) Clone() ChangedFilesDialog {
	clone := *d
	clone.form = d.form.Clone()
	return clone
}

// RenameEditor renames one session.
type RenameEditor struct {
	sessionID string
	// sessionName is the name the Sessions list shows, which the dialog
	// names it by.
	sessionName string
	title       chat.TextInput
	form        chat.Dialog[DialogControl]
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (e *RenameEditor) Clone() RenameEditor {
	clone := *e
	clone.form = e.form.Clone()
	return clone
}

// ConfigEntryKind is what a ConfigIdEditor renames.
type ConfigEntryKind int

const (
	ConfigEntryKindProfile ConfigEntryKind = iota
	ConfigEntryKindTarget
)

func (k ConfigEntryKind) label() string {
	switch k {
	case ConfigEntryKindTarget:
		return "target"
	default:
		return "profile"
	}
}

// ConfigIdEditor renames a profile or target id.
type ConfigIdEditor struct {
	// returnTo is the target actions dialog the rename was opened from, which
	// a cancelled rename returns to; nil when it was opened directly.
	returnTo *TargetActionsDialog
	kind     ConfigEntryKind
	oldID    string
	value    chat.TextInput
	form     chat.Dialog[DialogControl]
}

// Clone is Rust's derived Clone: the form and the return dialog are copied,
// not shared.
func (e *ConfigIdEditor) Clone() ConfigIdEditor {
	clone := *e
	if e.returnTo != nil {
		parent := e.returnTo.Clone()
		clone.returnTo = &parent
	}
	clone.form = e.form.Clone()
	return clone
}

// targetTestResult is Rust's `(String, Result<(), String>)`: the target a
// test ran on and its outcome.
type targetTestResult struct {
	targetID string
	// failure is the error text; nil when the target was ready.
	failure *string
}

// TargetActionsDialog lists the configured targets with rename, test and
// settings actions.
type TargetActionsDialog struct {
	targetIDs   []string
	targetIndex int
	form        chat.Dialog[DialogControl]
	testing     *string
	result      *targetTestResult
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *TargetActionsDialog) Clone() TargetActionsDialog {
	clone := *d
	clone.targetIDs = append([]string(nil), d.targetIDs...)
	clone.form = d.form.Clone()
	return clone
}

// WebDialog shows how to reach the web viewer, or why it could not start.
type WebDialog struct {
	loading           bool
	viewerURL         *string
	viewerCode        *string
	fallbackReason    *string
	message           *string
	qr                *string
	failedAddress     *netip.AddrPort
	portConflict      bool
	inspecting        bool
	listeners         []client.WebListenerProcess
	listenerIndex     int
	inspectionMessage *string
	confirmStop       *client.WebListenerProcess
	form              chat.Dialog[DialogControl]
}

// webDialogLoading is WebDialog::loading: the dialog while the viewer
// starts.
func webDialogLoading() WebDialog {
	return WebDialog{
		loading: true,
		form:    dialogForm([]DialogControl{DialogControlWebRetry}, DialogControlWebRetry),
	}
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *WebDialog) Clone() WebDialog {
	clone := *d
	clone.listeners = append([]client.WebListenerProcess(nil), d.listeners...)
	clone.form = d.form.Clone()
	return clone
}

// buttonRows is the dialog's footer: one row of buttons per line.
func (d *WebDialog) buttonRows() [][]chat.ButtonSpec[DialogControl] {
	if d.confirmStop != nil {
		return [][]chat.ButtonSpec[DialogControl]{{
			{ID: DialogControlWebCancelStop, Label: "Cancel", Enabled: true},
			{ID: DialogControlWebConfirmStop, Label: "Stop and retry", Enabled: true},
		}}
	}
	if d.loading || d.failedAddress == nil {
		// One empty row: the footer keeps its height while nothing is
		// actionable.
		return [][]chat.ButtonSpec[DialogControl]{nil}
	}
	rows := [][]chat.ButtonSpec[DialogControl]{{
		{ID: DialogControlWebAnotherPort, Label: "Use another port", Enabled: !d.inspecting},
		{ID: DialogControlWebRetry, Label: "Retry", Enabled: !d.inspecting},
	}}
	if d.listenerIndex >= 0 && d.listenerIndex < len(d.listeners) {
		process := d.listeners[d.listenerIndex]
		buttons := []chat.ButtonSpec[DialogControl]{{
			ID:      DialogControlWebStop,
			Label:   "Stop server…",
			Enabled: process.StopDisabledReason == nil,
		}}
		if len(d.listeners) > 1 {
			buttons = append(buttons, chat.ButtonSpec[DialogControl]{ID: DialogControlWebNextProcess, Label: "Next process", Enabled: true})
		}
		rows = append(rows, buttons)
	}
	var footer []chat.ButtonSpec[DialogControl]
	if d.portConflict {
		footer = append(footer, chat.ButtonSpec[DialogControl]{ID: DialogControlWebInspect, Label: "Inspect port", Enabled: !d.inspecting})
	}
	rows = append(rows, footer)
	return rows
}

func (d *WebDialog) defaultControl() DialogControl {
	switch {
	case d.confirmStop != nil:
		return DialogControlWebCancelStop
	case d.failedAddress != nil && !d.loading && !d.inspecting:
		return DialogControlWebAnotherPort
	default:
		return DialogControlWebRetry
	}
}

func (d *WebDialog) resetForm() {
	var controls []DialogControl
	for _, row := range d.buttonRows() {
		for _, button := range row {
			controls = append(controls, button.ID)
		}
	}
	d.form = dialogForm(controls, d.defaultControl())
}

// RepositoryOriginDialog asks for a replacement origin when the configured
// source lacks a checkpoint's base commit.
type RepositoryOriginDialog struct {
	sessionID        string
	repositoryID     string
	missingCommit    string
	archivedOrigin   string
	configuredOrigin string
	replacement      chat.PathInput
	error            *string
	form             chat.Dialog[DialogControl]
	launch           DashboardAction
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *RepositoryOriginDialog) Clone() RepositoryOriginDialog {
	clone := *d
	clone.form = d.form.Clone()
	return clone
}

// Confirmation is what a confirmation dialog asks about. One struct per Rust
// variant; a nil DashboardAction or Mode field is Rust's None.
type Confirmation interface {
	isConfirmation()
}

type ConfirmationRepairRepositoryRemotes struct {
	Action   DashboardAction
	Repairs  []core.LocalRemoteRepair
	Previous Mode
}

type ConfirmationConfigurationRepair struct {
	SessionID string
	Error     string
	Previous  Mode
}

type ConfirmationLaunchFailed struct {
	Error string
	// Retry is the launch to retry; nil when there is none.
	Retry    DashboardAction
	Previous Mode
}

// ConfirmationImportFailed is a session import that stopped with an error.
// The person was waiting on the import dialog, so the reason stays on screen
// until dismissed rather than going only to the footer.
type ConfirmationImportFailed struct {
	Error    string
	Previous Mode
}

// ConfirmationDismiss is a standard modal dismissal that may need to protect
// an in-memory draft or a running operation. The held mode is restored by
// the safe answer.
type ConfirmationDismiss struct {
	Mode   Mode
	Intent DismissalIntent
}

type ConfirmationInstallMbx struct {
	Generation uint64
	// Key is a serde_json::Value in the settings form (see client's
	// settings.go).
	Key            any
	MachineID      string
	Machine        core.Machine
	Host           string
	Upgrading      bool
	ProfileFile    string
	ProfileWarning *string
	ManualPathLine *string
	Previous       Mode
}

type ConfirmationCloseFailed struct {
	SessionID string
	Error     string
}

type ConfirmationSuspendSession struct {
	SessionID string
	// ChildrenNotHandedBack lists the titles of the sub-agents still at their
	// task, which the suspend stops.
	ChildrenNotHandedBack []string
	Interrupting          bool
	Preflight             *core.SuspendPreflight
	Checking              bool
	ViewingAllUntracked   bool
}

// ConfirmationInterruptWork is stop or restart asked for while the agent is
// mid-turn. An idle session stops without asking; this exists for the one
// case a mis-click costs work in progress.
type ConfirmationInterruptWork struct {
	SessionID string
	Restart   bool
}

type ConfirmationRestartAgent struct {
	SessionID    string
	Interrupting bool
}

type ConfirmationRestartContainer struct {
	SessionID   string
	Preview     core.ContainerUpgradePreview
	UpgradeOnly bool
}

// ConfirmationInterruptAll is Interrupt all, with what was running when it
// opened. The targets are found again on confirm, since turns end while the
// dialog is open.
type ConfirmationInterruptAll struct {
	SessionID        string
	ParentRunning    bool
	SubagentsRunning int
}

type ConfirmationForceDestroy struct {
	SessionID string
}

type ConfirmationDestroyStopped struct {
	SessionID string
	// Reopen is the resume dialog to restore afterwards, so confirming or
	// cancelling destruction leaves the user where they were; nil when the
	// confirmation did not come from one.
	Reopen *ResumeDialog
}

// ConfirmationRecoverFailed is Enter on a failed session. Opening its
// conversation and recovering it are both reasonable answers, and recovery
// replaces the target, so the surface asks rather than guessing.
type ConfirmationRecoverFailed struct {
	SessionID string
	Error     *string
	// Recoverable says whether an environment can be repaired or a verified
	// archive restored.
	Recoverable bool
	RetryLaunch bool
}

// ConfirmationConvertRawCheckout is a resume that moves a local checkout into
// an isolated workspace. The preflight already produced the receipt and the
// preview; this asks the person to agree to what travels before anything is
// stopped.
type ConfirmationConvertRawCheckout struct {
	Launch   DashboardAction
	Receipt  core.ResumeRepositorySourceReceipt
	Preview  core.RawConversionPreview
	Previous Mode
}

// ConfirmationRecoverMove is a Move that left a verified checkpoint or a
// ready destination that needs an explicit same-destination retry. Resume
// with the source settings is deliberately hidden while queue admission may
// already have run.
type ConfirmationRecoverMove struct {
	Operation core.MoveOperation
}

func (ConfirmationRepairRepositoryRemotes) isConfirmation() {}
func (ConfirmationConfigurationRepair) isConfirmation()     {}
func (ConfirmationLaunchFailed) isConfirmation()            {}
func (ConfirmationImportFailed) isConfirmation()            {}
func (ConfirmationDismiss) isConfirmation()                 {}
func (ConfirmationInstallMbx) isConfirmation()              {}
func (ConfirmationCloseFailed) isConfirmation()             {}
func (ConfirmationSuspendSession) isConfirmation()          {}
func (ConfirmationInterruptWork) isConfirmation()           {}
func (ConfirmationRestartAgent) isConfirmation()            {}
func (ConfirmationRestartContainer) isConfirmation()        {}
func (ConfirmationInterruptAll) isConfirmation()            {}
func (ConfirmationForceDestroy) isConfirmation()            {}
func (ConfirmationDestroyStopped) isConfirmation()          {}
func (ConfirmationRecoverFailed) isConfirmation()           {}
func (ConfirmationConvertRawCheckout) isConfirmation()      {}
func (ConfirmationRecoverMove) isConfirmation()             {}

// DismissalIntent is what a ConfirmationDismiss protects.
type DismissalIntent int

const (
	DismissalIntentDiscardSetup DismissalIntent = iota
	DismissalIntentCancelImport
)

// ConfirmDialog is a confirmation dialog and its persistent standard-control
// state.
type ConfirmDialog struct {
	scroll       int
	maxScroll    int
	confirmation Confirmation
	// sessionName is the name the Sessions list shows for the session the
	// confirmation is about. The body names the session by it, and falls back
	// to the id when it is nil.
	sessionName *string
	form        chat.Dialog[DialogControl]
}

// newConfirmDialog is ConfirmDialog::new.
func newConfirmDialog(confirmation Confirmation) ConfirmDialog {
	return ConfirmDialog{
		form:         confirmationForm(confirmation),
		confirmation: confirmation,
	}
}

// namingSession names the session in the body by name rather than by its
// id.
func (d ConfirmDialog) namingSession(name string) ConfirmDialog {
	d.sessionName = &name
	return d
}

// refreshForm rebuilds the dialog's button focus state after confirmation
// changes its buttons (for example when suspend preflight arrives).
func (d *ConfirmDialog) refreshForm() {
	d.form = confirmationForm(d.confirmation)
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *ConfirmDialog) Clone() ConfirmDialog {
	clone := *d
	clone.form = d.form.Clone()
	return clone
}

// ImportProgress is the dialog shown while a session import runs.
type ImportProgress struct {
	cancellable  bool
	sessionTitle string
	step         int
	total        *int
	message      string
	lastUpdated  time.Time
	form         chat.Dialog[DialogControl]
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (p *ImportProgress) Clone() ImportProgress {
	clone := *p
	clone.form = p.form.Clone()
	return clone
}

// ImportBundleConfirmation asks where an imported session's project lives
// and whether to give it an isolated checkout.
type ImportBundleConfirmation struct {
	projectRoot           chat.TextInput
	managedWorktree       core.ManagedWorktreeOptions
	createManagedWorktree bool
	dirtyGitRoots         []string
	omittedNonGitDirs     []string
	scratchGitRoots       []string
	form                  chat.Dialog[DialogControl]
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (c *ImportBundleConfirmation) Clone() ImportBundleConfirmation {
	clone := *c
	clone.form = c.form.Clone()
	return clone
}

func dialogForm(controls []DialogControl, initial DialogControl) chat.Dialog[DialogControl] {
	form := chat.NewDialog[DialogControl]()
	for _, id := range controls {
		var kind chat.ControlKind
		switch id {
		case DialogControlField:
			kind = chat.ControlKindTextField{}
		case DialogControlTargetList:
			kind = chat.ControlKindChoiceList{Len: 0, Selected: 0}
		case DialogControlImportManagedWorktree:
			kind = chat.ControlKindCheckbox{}
		default:
			kind = chat.ControlKindButton{}
		}
		form.Declare(id, kind)
	}
	form.EndFrame(initial)
	return form
}

func confirmationForm(confirmation Confirmation) chat.Dialog[DialogControl] {
	buttons := confirmationButtons(confirmation)
	form := chat.NewDialog[DialogControl]()
	for index := range buttons {
		form.Declare(DialogControlConfirmButton(index), chat.ControlKindButton{})
	}
	initial := initialConfirmationButton(confirmation, buttons)
	form.EndFrame(DialogControlConfirmButton(initial))
	return form
}

func targetActionsForm(targetCount int, selected int, initial DialogControl) chat.Dialog[DialogControl] {
	form := chat.NewDialog[DialogControl]()
	form.Declare(DialogControlTargetList, chat.ControlKindChoiceList{
		Len:      targetCount,
		Selected: min(selected, max(targetCount-1, 0)),
	})
	form.Declare(DialogControlTargetRename, chat.ControlKindButton{})
	form.Declare(DialogControlTargetTest, chat.ControlKindButton{})
	form.Declare(DialogControlTargetSettings, chat.ControlKindButton{})
	form.EndFrame(initial)
	return form
}

func syncTargetActionsForm(dialog *TargetActionsDialog) {
	form := &dialog.form
	form.Declare(DialogControlTargetList, chat.ControlKindChoiceList{
		Len:      len(dialog.targetIDs),
		Selected: dialog.targetIndex,
	})
	form.Declare(DialogControlTargetRename, chat.ControlKindButton{})
	form.DeclareWithEnabled(DialogControlTargetTest, chat.ControlKindButton{}, dialog.testing == nil)
	form.Declare(DialogControlTargetSettings, chat.ControlKindButton{})
	form.EndFrame(DialogControlTargetList)
}

// TextPromptOutcome is what one event did to a text-prompt dialog: a modal
// holding a single text field with Cancel and Save.
type TextPromptOutcome int

const (
	// TextPromptOutcomeCancel is Esc or Cancel. The dialog's owner decides
	// where the cancel goes.
	TextPromptOutcomeCancel TextPromptOutcome = iota
	// TextPromptOutcomeEdited means the dialog stays open, whether or not the
	// event changed the value.
	TextPromptOutcomeEdited
	// TextPromptOutcomeRejected is Save pressed on an empty value. The notice
	// is already set; the dialog stays open.
	TextPromptOutcomeRejected
	// TextPromptOutcomeSubmit is Save pressed on a value the dialog can act
	// on. The owner builds its own action from it.
	TextPromptOutcomeSubmit
)

func clearDialogFormGeometry(form *chat.Dialog[DialogControl]) {
	// Keep declarations available for keyboard input while the modal is
	// clipped, but discard hitboxes and any in-flight mouse gesture.
	form.CancelPointer()
	form.ResetGeometry()
}

func nestedRepositoryCheckpointFailure(message string) bool {
	return strings.Contains(message, core.NestedRepositoryCheckpointError)
}

// confirmationSessionID is Confirmation::session_id: the session this
// confirmation is about, when it is about one.
func confirmationSessionID(confirmation Confirmation) (string, bool) {
	switch c := confirmation.(type) {
	case ConfirmationConfigurationRepair:
		return c.SessionID, true
	case ConfirmationCloseFailed:
		return c.SessionID, true
	case ConfirmationSuspendSession:
		return c.SessionID, true
	case ConfirmationInterruptWork:
		return c.SessionID, true
	case ConfirmationRestartContainer:
		return c.SessionID, true
	case ConfirmationRestartAgent:
		return c.SessionID, true
	case ConfirmationInterruptAll:
		return c.SessionID, true
	case ConfirmationForceDestroy:
		return c.SessionID, true
	case ConfirmationDestroyStopped:
		return c.SessionID, true
	case ConfirmationRecoverFailed:
		return c.SessionID, true
	default:
		return "", false
	}
}

// confirmDialog is a confirmation dialog that names its session by the title
// the user sees in the session list, with the id only when the record is
// gone.
func (d *DashboardState) confirmDialog(confirmation Confirmation) ConfirmDialog {
	var name *string
	if sessionID, ok := confirmationSessionID(confirmation); ok {
		if session := d.SessionRecord(sessionID); session != nil {
			listed := sessionName(session)
			name = &listed
		}
	}
	dialog := newConfirmDialog(confirmation)
	if name != nil {
		return dialog.namingSession(*name)
	}
	return dialog
}

func (d *DashboardState) ApplyWebAccess(access client.WebViewerAccess) {
	current, ok := d.mode.(ModeWeb)
	if !ok {
		return
	}
	if _, starting := access.(client.WebViewerAccessStarting); starting {
		current.loading = true
		current.resetForm()
		return
	}
	dialog := webDialogLoading()
	dialog.loading = false
	switch access := access.(type) {
	case client.WebViewerAccessReady:
		viewerURL, viewerCode := access.ViewerURL, access.ViewerCode
		dialog.viewerURL = &viewerURL
		dialog.viewerCode = &viewerCode
		dialog.fallbackReason = dialogsCloneString(access.FallbackReason)
		if access.QrLoginURL != nil {
			qr, err := renderQR(*access.QrLoginURL)
			if err != nil {
				reason := err.Error()
				dialog.fallbackReason = &reason
			} else {
				dialog.qr = &qr
			}
		}
	case client.WebViewerAccessFailed:
		address, message := access.Address, access.Message
		dialog.failedAddress = &address
		dialog.portConflict = access.PortConflict
		dialog.message = &message
	case client.WebViewerAccessUnavailable:
		message := access.Message
		dialog.message = &message
	}
	dialog.resetForm()
	d.mode = ModeWeb{&dialog}
}

// dialogsCloneString is a copy of an optional string, so the copy owns its value
// as Rust's moved Option<String> does.
func dialogsCloneString(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// ApplyWebListeners takes the processes found listening on the web viewer's
// port, or the error that stopped the inspection.
func (d *DashboardState) ApplyWebListeners(processes []client.WebListenerProcess, err error) {
	current, ok := d.mode.(ModeWeb)
	if !ok {
		return
	}
	dialog := current.WebDialog
	dialog.inspecting = false
	if err == nil {
		dialog.inspectionMessage = nil
		if len(processes) == 0 {
			message := "No visible listener was found. It may have exited, or your account may not have permission to inspect it. Retry or use another port."
			dialog.inspectionMessage = &message
		}
		dialog.listeners = processes
		dialog.listenerIndex = 0
	} else {
		message := err.Error()
		dialog.inspectionMessage = &message
	}
	dialog.resetForm()
}

func (d *DashboardState) ApplyWebError(message string) {
	current, ok := d.mode.(ModeWeb)
	if !ok {
		return
	}
	dialog := current.WebDialog
	dialog.loading = false
	dialog.confirmStop = nil
	inspection := message
	dialog.inspectionMessage = &inspection
	if dialog.failedAddress == nil {
		dialog.message = &message
	}
	dialog.resetForm()
}

func (d *DashboardState) handleWebEvent(event tea.Msg, dialog WebDialog) DashboardAction {
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	var action DashboardAction = DashboardActionNone{}
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		if dialog.confirmStop != nil {
			dialog.confirmStop = nil
			dialog.resetForm()
		} else {
			d.CancelModal()
			return DashboardActionCancelWebAccess{}
		}
	case chat.InteractionActivate[DialogControl]:
		switch {
		case interaction.ID == DialogControlWebRetry && !dialog.loading:
			action = DashboardActionRecoverWebViewer{Recovery: client.WebViewerRecoveryRetry{}}
		case interaction.ID == DialogControlWebAnotherPort && !dialog.loading:
			action = DashboardActionRecoverWebViewer{Recovery: client.WebViewerRecoveryAnotherPort{}}
		case interaction.ID == DialogControlWebInspect && !dialog.inspecting:
			dialog.inspecting = true
			dialog.inspectionMessage = nil
			dialog.listeners = dialog.listeners[:0]
			dialog.resetForm()
			action = DashboardActionInspectWebListener{}
		case interaction.ID == DialogControlWebNextProcess && len(dialog.listeners) > 0:
			dialog.listenerIndex = (dialog.listenerIndex + 1) % len(dialog.listeners)
		case interaction.ID == DialogControlWebStop:
			if dialog.listenerIndex >= 0 && dialog.listenerIndex < len(dialog.listeners) {
				process := dialog.listeners[dialog.listenerIndex]
				if process.StopDisabledReason == nil {
					dialog.confirmStop = &process
					dialog.resetForm()
				}
			}
		case interaction.ID == DialogControlWebCancelStop:
			dialog.confirmStop = nil
			dialog.resetForm()
		case interaction.ID == DialogControlWebConfirmStop:
			if process := dialog.confirmStop; process != nil {
				dialog.confirmStop = nil
				action = DashboardActionRecoverWebViewer{Recovery: client.WebViewerRecoveryStopAndRetry{Process: *process}}
			}
		}
	}
	if _, recovering := action.(DashboardActionRecoverWebViewer); recovering {
		dialog.loading = true
		dialog.inspectionMessage = nil
		dialog.resetForm()
	}
	d.mode = ModeWeb{&dialog}
	return action
}

func (d *DashboardState) beginProfileRename() {
	var oldID string
	found := false
	switch row := d.selectedQuotaRow().(type) {
	case QuotaRowLane:
		oldID, found = row.ID, true
	case QuotaRowProfile:
		oldID, found = row.ID, true
	case QuotaRowPool:
	pools:
		for _, provider := range d.quotaPools() {
			for _, pool := range provider.Pools {
				if pool.Key == row.Key && len(pool.Members) == 1 {
					oldID, found = pool.Members[0].ID, true
					break pools
				}
			}
		}
	}
	if !found {
		d.beginSettingsSection("profiles", nil)
		return
	}
	d.mode = ModeConfigId{&ConfigIdEditor{
		kind:  ConfigEntryKindProfile,
		value: chat.TextInputFromValue(oldID).WithMaxChars(64),
		oldID: oldID,
		form: dialogForm(
			[]DialogControl{DialogControlField, DialogControlCancel, DialogControlSave},
			DialogControlField,
		),
	}}
}

func (d *DashboardState) beginTargetActions() {
	var preferred *string
	capacityKeys := slices.Sorted(maps.Keys(d.capacityDetails))
	if d.capacityIndex >= 0 && d.capacityIndex < len(capacityKeys) {
		if detail := d.capacityDetails[capacityKeys[d.capacityIndex]]; len(detail.Reading.TargetIDs) > 0 {
			first := detail.Reading.TargetIDs[0]
			preferred = &first
		}
	}
	targetIDs := slices.Sorted(maps.Keys(d.config.Get().Targets))
	targetIndex := 0
	if preferred != nil {
		if index := slices.Index(targetIDs, *preferred); index >= 0 {
			targetIndex = index
		}
	}
	targetCount := len(targetIDs)
	d.mode = ModeTargetActions{&TargetActionsDialog{
		targetIDs:   targetIDs,
		targetIndex: targetIndex,
		form:        targetActionsForm(targetCount, targetIndex, DialogControlTargetList),
	}}
}

// targetTestRunning says whether the target-actions dialog is running a test
// right now.
//
// The cancel command answers over this dialog, which is the one modal it is
// allowed through, so both the availability gate and the dispatch arm ask
// this question.
func (d *DashboardState) targetTestRunning() bool {
	mode, ok := d.mode.(ModeTargetActions)
	return ok && mode.testing != nil
}

// cancelTargetTest cancels the target test the dialog is running, if there
// is one; nil when there is none.
func (d *DashboardState) cancelTargetTest() DashboardAction {
	mode, ok := d.mode.(ModeTargetActions)
	if !ok || mode.testing == nil {
		return nil
	}
	dialog := mode.TargetActionsDialog
	dialog.testing = nil
	cancelled := "cancelled"
	dialog.result = &targetTestResult{targetID: "Target test", failure: &cancelled}
	syncTargetActionsForm(dialog)
	return DashboardActionCancelTargetTest{}
}

func (d *DashboardState) handleTargetActionsEvent(event tea.Msg, dialog TargetActionsDialog) DashboardAction {
	// Preserve the convenient Up/Down target selection from the old surface
	// while letting the form own the selection metadata.
	if press, ok := event.(tea.KeyPressMsg); ok && !press.IsRepeat {
		switch chat.KeyPressCode(press) {
		case tea.KeyUp, tea.KeyDown, 'j', 'k':
			if !dialog.form.IsFocused(DialogControlTargetList) {
				dialog.form.Focus(DialogControlTargetList)
			}
		}
	}
	// The field's kind carries its popup state, so it is re-declared on every
	// event before the form reads keys against it.
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		cancelTest := dialog.testing != nil
		d.CancelModal()
		if cancelTest {
			return DashboardActionCancelTargetTest{}
		}
	case chat.InteractionSelect[DialogControl]:
		if interaction.ID != DialogControlTargetList {
			d.mode = ModeTargetActions{&dialog}
			break
		}
		dialog.targetIndex = min(interaction.Index, max(len(dialog.targetIDs)-1, 0))
		d.mode = ModeTargetActions{&dialog}
	case chat.InteractionActivate[DialogControl]:
		if dialog.targetIndex < 0 || dialog.targetIndex >= len(dialog.targetIDs) {
			d.CancelModal()
			return DashboardActionNone{}
		}
		targetID := dialog.targetIDs[dialog.targetIndex]
		switch {
		case interaction.ID == DialogControlTargetSettings && dialog.testing == nil:
			d.beginSettingsSection("targets", &targetID)
			return DashboardActionNone{}
		case interaction.ID == DialogControlTargetRename:
			parent := dialog
			d.mode = ModeConfigId{&ConfigIdEditor{
				kind:     ConfigEntryKindTarget,
				returnTo: &parent,
				value:    chat.TextInputFromValue(targetID).WithMaxChars(64),
				oldID:    targetID,
				form: dialogForm(
					[]DialogControl{DialogControlField, DialogControlCancel, DialogControlSave},
					DialogControlField,
				),
			}}
			return DashboardActionNone{}
		case interaction.ID == DialogControlTargetTest && dialog.testing == nil:
			testing := targetID
			dialog.testing = &testing
			dialog.result = nil
			syncTargetActionsForm(&dialog)
			d.mode = ModeTargetActions{&dialog}
			return DashboardActionTestTarget{TargetID: targetID}
		}
		d.mode = ModeTargetActions{&dialog}
	default:
		d.mode = ModeTargetActions{&dialog}
	}
	return DashboardActionNone{}
}

// handleTextPromptEvent routes one event through a text-prompt dialog: the
// single text field, Cancel, and Save with the empty-value check both dialogs
// make. The caller keeps what only it knows: where a cancel goes and what a
// submitted value means.
func (d *DashboardState) handleTextPromptEvent(form *chat.Dialog[DialogControl], value *chat.TextInput, event tea.Msg, emptyMessage string) TextPromptOutcome {
	result := form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		return TextPromptOutcomeCancel
	case chat.InteractionActivate[DialogControl]:
		switch interaction.ID {
		case DialogControlCancel:
			return TextPromptOutcomeCancel
		case DialogControlField, DialogControlSave:
			if strings.TrimSpace(value.Value()) == "" {
				d.notices.Set(emptyMessage)
				return TextPromptOutcomeRejected
			}
			return TextPromptOutcomeSubmit
		}
	case chat.InteractionEdit[DialogControl]:
		if interaction.ID == DialogControlField {
			chat.TextFieldApply(value, interaction.Edit)
			return TextPromptOutcomeEdited
		}
	}
	return TextPromptOutcomeEdited
}

func (d *DashboardState) handleConfigIDEvent(event tea.Msg, editor ConfigIdEditor) DashboardAction {
	outcome := d.handleTextPromptEvent(&editor.form, &editor.value, event, "Configuration ID cannot be empty.")
	switch outcome {
	case TextPromptOutcomeCancel:
		// This editor can be reached from the target actions dialog, which
		// expects to come back when the rename is abandoned.
		if parent := editor.returnTo; parent != nil {
			editor.returnTo = nil
			d.mode = ModeTargetActions{parent}
		} else {
			d.CancelModal()
		}
	case TextPromptOutcomeEdited, TextPromptOutcomeRejected:
		d.mode = ModeConfigId{&editor}
	case TextPromptOutcomeSubmit:
		d.CancelModal()
		switch editor.kind {
		case ConfigEntryKindTarget:
			return DashboardActionRenameTarget{OldID: editor.oldID, NewID: editor.value.IntoValue()}
		default:
			return DashboardActionRenameProfile{OldID: editor.oldID, NewID: editor.value.IntoValue()}
		}
	}
	return DashboardActionNone{}
}

// ApplyTargetTest takes the result of testing targetID; err is nil when the
// target was ready.
func (d *DashboardState) ApplyTargetTest(targetID string, err error) {
	mode, ok := d.mode.(ModeTargetActions)
	if !ok || mode.testing == nil || *mode.testing != targetID {
		return
	}
	dialog := mode.TargetActionsDialog
	dialog.testing = nil
	result := &targetTestResult{targetID: targetID}
	if err != nil {
		failure := err.Error()
		result.failure = &failure
	}
	dialog.result = result
	syncTargetActionsForm(dialog)
}

func (d *DashboardState) ShowRepositoryOriginDialog(sessionID string, repositoryID string, missingCommit string, archivedOrigin string, configuredOrigin string, launch DashboardAction) {
	// This dialog is a result of the resume check. Moving the generation on
	// ends the check, so any other result sent under it is dropped instead of
	// replacing this dialog or launching behind it.
	d.invalidateSessionPreflight()
	d.mode = ModeRepositoryOrigin{&RepositoryOriginDialog{
		sessionID:        sessionID,
		repositoryID:     repositoryID,
		missingCommit:    missingCommit,
		archivedOrigin:   archivedOrigin,
		replacement:      chat.NewPathInput(),
		configuredOrigin: configuredOrigin,
		form: dialogForm(
			[]DialogControl{DialogControlField, DialogControlCancel, DialogControlPrimary},
			DialogControlField,
		),
		launch: launch,
	}}
}

func (d *DashboardState) ApplyRepositoryOriginFailure(repositoryID string, message string) {
	mode, ok := d.mode.(ModeRepositoryOrigin)
	if !ok || mode.repositoryID != repositoryID {
		return
	}
	mode.error = &message
	mode.form.Focus(DialogControlField)
}

func (d *DashboardState) FinishResumeRepositoryPreflight() {
	d.CancelModal()
}

func (d *DashboardState) handleRepositoryOriginEvent(event tea.Msg, dialog RepositoryOriginDialog) DashboardAction {
	// The field's kind carries its popup state, so it is re-declared on every
	// event before the form reads keys against it.
	dialog.form.Declare(DialogControlField, dialog.replacement.ControlKind())
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	action, interaction, consumed := routePathCompletion(d, &dialog, result.Action)
	if consumed {
		d.mode = ModeRepositoryOrigin{&dialog}
		return action
	}
	switch interaction := interaction.(type) {
	case chat.InteractionCancel[DialogControl]:
		d.CancelModal()
	case chat.InteractionActivate[DialogControl]:
		switch interaction.ID {
		case DialogControlCancel:
			d.CancelModal()
		case DialogControlField, DialogControlPrimary:
			if strings.TrimSpace(dialog.replacement.Value()) == "" {
				message := "Enter the repository's new origin."
				dialog.error = &message
				dialog.form.Focus(DialogControlField)
				d.mode = ModeRepositoryOrigin{&dialog}
			} else {
				action := DashboardActionReplaceResumeRepositoryOrigin{
					SessionID:    dialog.sessionID,
					RepositoryID: dialog.repositoryID,
					Replacement:  dialog.replacement.String(),
					Launch:       dialog.launch,
				}
				d.mode = ModeRepositoryOrigin{&dialog}
				return action
			}
		default:
			d.mode = ModeRepositoryOrigin{&dialog}
		}
	case chat.InteractionEdit[DialogControl]:
		if interaction.ID == DialogControlField {
			if chat.PathFieldApply(&dialog.replacement, interaction.Edit) == chat.EditOutcomeChanged {
				dialog.error = nil
			}
		}
		d.mode = ModeRepositoryOrigin{&dialog}
	default:
		d.mode = ModeRepositoryOrigin{&dialog}
	}
	return DashboardActionNone{}
}

// ShowLaunchFailure keeps launch errors independent of the transient shared
// status line. retry is the launch to retry, nil when there is none.
//
// The status line still said "Launching … via …" after a failure, since
// only a successful launch replaced it (launch finding R13-8). The failure
// replaces it now, with the dialog's own first sentence, while the details
// stay in the dialog.
func (d *DashboardState) ShowLaunchFailure(message string, retry DashboardAction) {
	d.SetFailureNotice("The session could not start.")
	previous := d.mode
	dialog := newConfirmDialog(ConfirmationLaunchFailed{Error: message, Retry: retry, Previous: previous})
	d.mode = ModeConfirm{&dialog}
}

// ShowImportFailure reports a failed import in a dialog that stays until
// dismissed. The footer gets only a short failure line; the details stay in
// the dialog.
func (d *DashboardState) ShowImportFailure(message string) {
	d.SetFailureNotice("The session import failed.")
	previous := d.mode
	dialog := newConfirmDialog(ConfirmationImportFailed{Error: message, Previous: previous})
	d.mode = ModeConfirm{&dialog}
}

func (d *DashboardState) ShowRemoteRepairConfirmation(bundleID string, repairs []core.LocalRemoteRepair, retry DashboardAction) {
	previous := d.mode
	action := DashboardActionRepairRepositoryRemotes{
		BundleID: bundleID,
		Repairs:  slices.Clone(repairs),
		Retry:    retry,
	}
	dialog := newConfirmDialog(ConfirmationRepairRepositoryRemotes{Action: action, Repairs: repairs, Previous: previous})
	d.mode = ModeConfirm{&dialog}
}

// ShowCloseFailure shows the recovery choices after a checkpointed close
// could not finish.
func (d *DashboardState) ShowCloseFailure(sessionID string, message string) {
	dialog := d.confirmDialog(ConfirmationCloseFailed{SessionID: sessionID, Error: message})
	d.mode = ModeConfirm{&dialog}
}

func (d *DashboardState) ShowImportProgress(sessionTitle string) {
	d.mode = ModeImporting{&ImportProgress{
		cancellable:  true,
		sessionTitle: sessionTitle,
		step:         1,
		message:      "Locating native session…",
		lastUpdated:  time.Now(),
		form:         dialogForm([]DialogControl{DialogControlCancel}, DialogControlCancel),
	}}
}

func (d *DashboardState) ShowImportUpload() {
	if progress := importProgressMut(&d.mode); progress != nil {
		progress.cancellable = true
		progress.message = "Uploading archive…"
		progress.lastUpdated = time.Now()
		shown := progress.Clone()
		d.mode = ModeImporting{&shown}
	}
}

func (d *DashboardState) ShowImportRegistration() {
	if progress := importProgressMut(&d.mode); progress != nil {
		progress.cancellable = false
		progress.message = "Registering on the daemon — cannot be cancelled."
		progress.lastUpdated = time.Now()
		// An upload-cancel confirmation cannot outlive that upload phase.
		shown := progress.Clone()
		d.mode = ModeImporting{&shown}
	}
}

// UpdateImportProgress records an import step; total is nil while unknown.
func (d *DashboardState) UpdateImportProgress(step int, total *int, message string) {
	progress := importProgressMut(&d.mode)
	if progress == nil {
		return
	}
	progress.step = step
	progress.total = total
	progress.message = message
	progress.lastUpdated = time.Now()
}

func (d *DashboardState) ShowImportBundleConfirmation(dirtyGitRoots []string, omittedNonGitDirs []string, scratchGitRoots []string, managedWorktree core.ManagedWorktreeOptions, recordedRoot string) {
	form := dialogForm(
		[]DialogControl{DialogControlField, DialogControlImportCancel, DialogControlImportContinue},
		DialogControlImportContinue,
	)
	if managedWorktree.Available {
		form.DeclareWithEnabled(DialogControlImportManagedWorktree, chat.ControlKindCheckbox{}, true)
	}
	d.mode = ModeConfirmImportBundle{&ImportBundleConfirmation{
		projectRoot:           chat.TextInputFromValue(recordedRoot),
		managedWorktree:       managedWorktree,
		createManagedWorktree: managedWorktree.DefaultCreate,
		dirtyGitRoots:         dirtyGitRoots,
		omittedNonGitDirs:     omittedNonGitDirs,
		scratchGitRoots:       scratchGitRoots,
		form:                  form,
	}}
}

// ShowRawConversionConfirmation asks before a resume moves a local checkout
// into an isolated workspace. Cancelling returns to the wizard the preflight
// was started from.
func (d *DashboardState) ShowRawConversionConfirmation(launch DashboardAction, receipt core.ResumeRepositorySourceReceipt, preview core.RawConversionPreview) {
	// This confirmation is a result of the resume check. Moving the
	// generation on ends the check, so any other result sent under it is
	// dropped instead of opening a second confirmation over this one.
	d.invalidateSessionPreflight()
	previous := d.mode
	dialog := newConfirmDialog(ConfirmationConvertRawCheckout{
		Launch:   launch,
		Receipt:  receipt,
		Preview:  preview,
		Previous: previous,
	})
	d.mode = ModeConfirm{&dialog}
}

func (d *DashboardState) FinishImport() {
	d.CancelModal()
}

func (d *DashboardState) handleImportProgressEvent(event tea.Msg, progress ImportProgress) DashboardAction {
	if !progress.cancellable {
		d.mode = ModeImporting{&progress}
		return DashboardActionNone{}
	}
	result := progress.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	cancel := false
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		cancel = true
	case chat.InteractionActivate[DialogControl]:
		cancel = interaction.ID == DialogControlCancel
	}
	if cancel {
		dialog := newConfirmDialog(ConfirmationDismiss{
			Mode:   ModeImporting{&progress},
			Intent: DismissalIntentCancelImport,
		})
		d.mode = ModeConfirm{&dialog}
		return DashboardActionNone{}
	}
	d.mode = ModeImporting{&progress}
	return DashboardActionNone{}
}

func (d *DashboardState) beginRename() {
	session := d.selectedSession()
	if session == nil {
		return
	}
	given, _ := session.GivenName()
	d.mode = ModeRename{&RenameEditor{
		sessionID:   session.ID,
		sessionName: sessionName(session),
		title:       chat.TextInputFromValue(given).WithMaxChars(64),
		form: dialogForm(
			[]DialogControl{DialogControlField, DialogControlCancel, DialogControlSave},
			DialogControlField,
		),
	}}
}

func (d *DashboardState) beginNoticeLog() {
	d.mode = ModeNoticeLog{&NoticeLogDialog{form: chat.NewDialog[DialogControl]()}}
}

func (d *DashboardState) handleNoticeLogEvent(event tea.Msg, dialog NoticeLogDialog) DashboardAction {
	last := dialog.maxScroll
	if press, ok := event.(tea.KeyPressMsg); ok {
		scroll, scrolled := 0, true
		switch chat.KeyPressCode(press) {
		case tea.KeyDown, 'j':
			scroll = dialog.scroll + 1
		case tea.KeyUp, 'k':
			scroll = max(dialog.scroll-1, 0)
		case tea.KeyPgDown:
			scroll = dialog.scroll + 10
		case tea.KeyPgUp:
			scroll = max(dialog.scroll-10, 0)
		case tea.KeyHome:
			scroll = 0
		case tea.KeyEnd:
			scroll = last
		default:
			scrolled = false
		}
		if scrolled {
			dialog.scroll = min(scroll, last)
			d.recordEventHandled()
			d.mode = ModeNoticeLog{&dialog}
			return DashboardActionNone{}
		}
	}
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		d.CancelModal()
	case chat.InteractionActivate[DialogControl]:
		if interaction.ID == DialogControlNoticeLogClose {
			d.CancelModal()
		} else {
			d.mode = ModeNoticeLog{&dialog}
		}
	default:
		d.mode = ModeNoticeLog{&dialog}
	}
	return DashboardActionNone{}
}

func (d *DashboardState) handleSessionCpuReportEvent(event tea.Msg, dialog SessionCpuReportDialog) DashboardAction {
	groups := reportGroups(d, dialog.metric)
	groupIndex, hasGroup := reconcileSessionCpuReport(d, &dialog, groups)
	if press, ok := event.(tea.KeyPressMsg); ok {
		code := chat.KeyPressCode(press)
		tabStep := 0
		switch code {
		case tea.KeyLeft:
			tabStep = -1
		case tea.KeyRight:
			tabStep = 1
		}
		if tabStep != 0 && len(groups) > 1 {
			current := 0
			if hasGroup {
				current = groupIndex
			}
			count := len(groups)
			next := ((current+tabStep)%count + count) % count
			switchSessionCpuReportTab(&dialog, groups, next)
			d.recordEventHandled()
			d.mode = ModeSessionCpuReport{&dialog}
			return DashboardActionNone{}
		}
		rowDelta, hasDelta := 0, true
		switch code {
		case tea.KeyDown, 'j':
			rowDelta = 1
		case tea.KeyUp, 'k':
			rowDelta = -1
		case tea.KeyPgDown:
			rowDelta = 10
		case tea.KeyPgUp:
			rowDelta = -10
		case tea.KeyHome:
			rowDelta = math.MinInt
		case tea.KeyEnd:
			rowDelta = math.MaxInt
		default:
			hasDelta = false
		}
		if hasDelta && hasGroup {
			moveSessionCpuReportSelection(&dialog, &groups[groupIndex], rowDelta)
			d.recordEventHandled()
			d.mode = ModeSessionCpuReport{&dialog}
			return DashboardActionNone{}
		}
		if code == tea.KeyEnter {
			focused, isFocused := dialog.form.Focused()
			switch {
			case isFocused && focused == DialogControlSessionCpuReportTabs:
				dialog.form.Focus(DialogControlSessionCpuReportRows)
				d.recordEventHandled()
				d.mode = ModeSessionCpuReport{&dialog}
				return DashboardActionNone{}
			case isFocused && (focused == DialogControlSessionCpuReportClose ||
				focused == DialogControlSessionCpuReportHourly ||
				focused == DialogControlSessionCpuReportRecent):
			case !isFocused || focused == DialogControlSessionCpuReportRows:
				if dialog.selectedSessionID != nil {
					sessionID := *dialog.selectedSessionID
					d.recordEventHandled()
					return d.focusSessionFromCPUReport(sessionID)
				}
			}
		}
	}
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		d.CancelModal()
	case chat.InteractionSelect[DialogControl]:
		switch interaction.ID {
		case DialogControlSessionCpuReportTabs:
			switchSessionCpuReportTab(&dialog, groups, interaction.Index)
		case DialogControlSessionCpuReportRows:
			if hasGroup {
				selectSessionCpuReportRow(&dialog, &groups[groupIndex], interaction.Index)
			}
		}
		d.mode = ModeSessionCpuReport{&dialog}
	case chat.InteractionActivate[DialogControl]:
		switch interaction.ID {
		case DialogControlSessionCpuReportClose:
			d.CancelModal()
			return DashboardActionNone{}
		case DialogControlSessionCpuReportHourly:
			dialog.metric = CpuMetricHourly
		case DialogControlSessionCpuReportRecent:
			dialog.metric = CpuMetricRecent
		case DialogControlSessionCpuReportTabs:
			dialog.form.Focus(DialogControlSessionCpuReportRows)
		case DialogControlSessionCpuReportRows:
			selection, ok := dialog.form.Selected(DialogControlSessionCpuReportRows)
			if !ok {
				selection = dialog.selectedRow
			}
			if hasGroup {
				selectSessionCpuReportRow(&dialog, &groups[groupIndex], selection)
			}
			if dialog.selectedSessionID != nil {
				return d.focusSessionFromCPUReport(*dialog.selectedSessionID)
			}
		}
		d.mode = ModeSessionCpuReport{&dialog}
	default:
		d.mode = ModeSessionCpuReport{&dialog}
	}
	return DashboardActionNone{}
}

// beginChangedFiles opens the changed-files overlay for the selected session
// and asks the host to read its checkout, so the list is fresh even when a
// row was probed a while ago.
func (d *DashboardState) beginChangedFiles() DashboardAction {
	session := d.selectedSession()
	if session == nil {
		return DashboardActionNone{}
	}
	sessionID := session.ID
	d.mode = ModeChangedFiles{&ChangedFilesDialog{
		sessionID: sessionID,
		form:      chat.NewDialog[DialogControl](),
	}}
	return d.requestGitProbe(sessionID)
}

func (d *DashboardState) handleChangedFilesEvent(event tea.Msg, dialog ChangedFilesDialog) DashboardAction {
	files := 0
	if status, ok := d.gitStatus[dialog.sessionID]; ok && status.Err == nil {
		files = len(status.Ok.Changed)
	}
	if press, ok := event.(tea.KeyPressMsg); ok {
		last := max(files-1, 0)
		scroll, scrolled := 0, true
		switch chat.KeyPressCode(press) {
		case tea.KeyDown, 'j':
			scroll = dialog.scroll + 1
		case tea.KeyUp, 'k':
			scroll = max(dialog.scroll-1, 0)
		case tea.KeyPgDown:
			scroll = dialog.scroll + 10
		case tea.KeyPgUp:
			scroll = max(dialog.scroll-10, 0)
		case tea.KeyHome:
			scroll = 0
		case tea.KeyEnd:
			scroll = last
		case 'r':
			sessionID := dialog.sessionID
			d.mode = ModeChangedFiles{&dialog}
			return d.requestGitProbe(sessionID)
		default:
			scrolled = false
		}
		if scrolled {
			dialog.scroll = min(scroll, last)
			d.recordEventHandled()
			d.mode = ModeChangedFiles{&dialog}
			return DashboardActionNone{}
		}
	}
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		d.CancelModal()
	case chat.InteractionActivate[DialogControl]:
		switch interaction.ID {
		case DialogControlChangedFilesClose:
			d.CancelModal()
		case DialogControlChangedFilesRefresh:
			sessionID := dialog.sessionID
			d.mode = ModeChangedFiles{&dialog}
			return d.requestGitProbe(sessionID)
		default:
			d.mode = ModeChangedFiles{&dialog}
		}
	default:
		d.mode = ModeChangedFiles{&dialog}
	}
	return DashboardActionNone{}
}

func (d *DashboardState) handleRenameEvent(event tea.Msg, editor RenameEditor) DashboardAction {
	outcome := d.handleTextPromptEvent(&editor.form, &editor.title, event, "Session name cannot be empty.")
	switch outcome {
	case TextPromptOutcomeCancel:
		d.CancelModal()
	case TextPromptOutcomeEdited:
		d.mode = ModeRename{&editor}
	case TextPromptOutcomeRejected:
		// Put the caret back where the missing name has to be typed.
		editor.form.Focus(DialogControlField)
		d.mode = ModeRename{&editor}
	case TextPromptOutcomeSubmit:
		d.CancelModal()
		return DashboardActionRenameSession{SessionID: editor.sessionID, Title: editor.title.IntoValue()}
	}
	return DashboardActionNone{}
}

func (d *DashboardState) handleImportBundleEvent(event tea.Msg, confirmation ImportBundleConfirmation) DashboardAction {
	result := confirmation.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		return DashboardActionConfirmImportBundle{ProjectRoot: confirmation.projectRoot.Value()}
	case chat.InteractionActivate[DialogControl]:
		switch interaction.ID {
		case DialogControlImportCancel:
			return DashboardActionConfirmImportBundle{ProjectRoot: confirmation.projectRoot.Value()}
		case DialogControlImportContinue:
			create := confirmation.managedWorktree.Available && confirmation.createManagedWorktree
			return DashboardActionConfirmImportBundle{
				CreateManagedWorktree: &create,
				ProjectRoot:           confirmation.projectRoot.Value(),
				Accepted:              true,
			}
		}
		d.mode = ModeConfirmImportBundle{&confirmation}
	case chat.InteractionToggle[DialogControl]:
		if interaction.ID == DialogControlImportManagedWorktree && confirmation.managedWorktree.Available {
			confirmation.createManagedWorktree = !confirmation.createManagedWorktree
		}
		d.mode = ModeConfirmImportBundle{&confirmation}
	case chat.InteractionEdit[DialogControl]:
		if interaction.ID == DialogControlField {
			chat.TextFieldApply(&confirmation.projectRoot, interaction.Edit)
		}
		d.mode = ModeConfirmImportBundle{&confirmation}
	default:
		d.mode = ModeConfirmImportBundle{&confirmation}
	}
	return DashboardActionNone{}
}

func (d *DashboardState) handleConfirmationEvent(event tea.Msg, dialog ConfirmDialog) DashboardAction {
	// Every button has a letter, derived from its label by
	// confirmationAccelerators, so a confirmation never needs Tab.
	if press, ok := event.(tea.KeyPressMsg); ok {
		letter, modifiers := chat.NormalizeKey(chat.KeyPressCode(press), press.Mod)
		if modifiers&(tea.ModCtrl|tea.ModAlt) == 0 && chat.IsCharKeyCode(letter) {
			if letter == 'v' || letter == 'V' {
				if suspend, ok := dialog.confirmation.(ConfirmationSuspendSession); ok {
					suspend.ViewingAllUntracked = !suspend.ViewingAllUntracked
					dialog.confirmation = suspend
					d.mode = ModeConfirm{&dialog}
					return DashboardActionNone{}
				}
			}
			buttons := confirmationButtons(dialog.confirmation)
			accelerators := confirmationAccelerators(buttons)
			lower := letter
			if 'A' <= lower && lower <= 'Z' {
				lower += 'a' - 'A'
			}
			if index := slices.Index(accelerators, lower); index >= 0 {
				return d.activateConfirmationButton(dialog.confirmation, index)
			}
		}
	}
	switch dialog.confirmation.(type) {
	case ConfirmationConfigurationRepair, ConfirmationLaunchFailed, ConfirmationImportFailed,
		ConfirmationRepairRepositoryRemotes, ConfirmationConvertRawCheckout, ConfirmationSuspendSession:
		if press, ok := event.(tea.KeyPressMsg); ok {
			switch chat.KeyPressCode(press) {
			case tea.KeyPgDown:
				dialog.scroll = min(dialog.scroll+5, dialog.maxScroll)
			case tea.KeyPgUp:
				dialog.scroll = max(dialog.scroll-5, 0)
			case tea.KeyHome:
				dialog.scroll = 0
			default:
				return d.handleConfirmationControls(dialog, event)
			}
			d.mode = ModeConfirm{&dialog}
			return DashboardActionNone{}
		}
	}
	return d.handleConfirmationControls(dialog, event)
}

func (d *DashboardState) handleConfirmationControls(dialog ConfirmDialog, event tea.Msg) DashboardAction {
	// The field's kind carries its popup state, so it is re-declared on every
	// event before the form reads keys against it.
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[DialogControl]:
		switch confirmation := dialog.confirmation.(type) {
		case ConfirmationDismiss:
			d.restoreDismissedMode(confirmation.Mode)
		case ConfirmationRepairRepositoryRemotes:
			d.restoreDismissedMode(confirmation.Previous)
		case ConfirmationConfigurationRepair:
			d.restoreDismissedMode(confirmation.Previous)
		case ConfirmationLaunchFailed:
			d.restoreDismissedMode(confirmation.Previous)
		case ConfirmationImportFailed:
			d.restoreDismissedMode(confirmation.Previous)
		case ConfirmationInstallMbx:
			d.restoreDismissedMode(confirmation.Previous)
		case ConfirmationConvertRawCheckout:
			d.restoreDismissedMode(confirmation.Previous)
		case ConfirmationDestroyStopped:
			d.restoreAfterConfirmation(confirmation.Reopen)
		default:
			d.CancelModal()
		}
	case chat.InteractionActivate[DialogControl]:
		if index, ok := interaction.ID.confirmButtonIndex(); ok {
			return d.activateConfirmationButton(dialog.confirmation, index)
		}
		d.mode = ModeConfirm{&dialog}
	default:
		d.mode = ModeConfirm{&dialog}
	}
	return DashboardActionNone{}
}

// activateConfirmationButton runs the button at index of
// confirmationButtons, where index 0 is always Cancel.
func (d *DashboardState) activateConfirmationButton(confirmation Confirmation, index int) DashboardAction {
	switch c := confirmation.(type) {
	case ConfirmationConfigurationRepair:
		d.restoreDismissedMode(c.Previous)
		switch index {
		case 1:
			return DashboardActionOpen{SessionID: c.SessionID}
		case 2:
			d.BeginSetup()
		}
		return DashboardActionNone{}
	case ConfirmationRepairRepositoryRemotes:
		d.restoreDismissedMode(c.Previous)
		if index == 1 {
			return c.Action
		}
		return DashboardActionNone{}
	case ConfirmationImportFailed:
		d.restoreDismissedMode(c.Previous)
		return DashboardActionNone{}
	case ConfirmationLaunchFailed:
		d.restoreDismissedMode(c.Previous)
		switch {
		case (c.Retry != nil && index == 2) || (c.Retry == nil && index == 1):
			d.BeginSetup()
			return DashboardActionNone{}
		case index == 1:
			var retry DashboardAction = DashboardActionNone{}
			if c.Retry != nil {
				retry = c.Retry
			}
			// A retried restore holds its archive as the wizard's Resume does,
			// so reopening the archive cannot start a second restore beside
			// it.
			if restore, ok := retry.(DashboardActionRestoreArchivedSession); ok && !d.claimArchiveRestore(restore.WikiID) {
				return DashboardActionNone{}
			}
			return retry
		}
		return DashboardActionNone{}
	case ConfirmationInstallMbx:
		d.restoreDismissedMode(c.Previous)
		if index == 1 {
			return DashboardActionInstallMbx{
				Generation: c.Generation,
				Key:        c.Key,
				MachineID:  c.MachineID,
				Machine:    c.Machine,
			}
		}
		return DashboardActionNone{}
	case ConfirmationDismiss:
		switch index {
		case 0:
			return d.restoreDismissedMode(c.Mode)
		case 1:
			d.CancelModal()
			if c.Intent == DismissalIntentCancelImport {
				return DashboardActionCancelImport{}
			}
			return DashboardActionNone{}
		}
	case ConfirmationConvertRawCheckout:
		if index == 1 {
			d.CancelModal()
			return DashboardActionConfirmRawConversion{Launch: c.Launch, Receipt: c.Receipt}
		}
		return d.restoreDismissedMode(c.Previous)
	case ConfirmationForceDestroy:
		if index == 1 {
			d.CancelModal()
			return DashboardActionForceDestroy{SessionID: c.SessionID}
		}
	case ConfirmationDestroyStopped:
		d.restoreAfterConfirmation(c.Reopen)
		if index == 1 {
			return DashboardActionDestroyStopped{SessionID: c.SessionID}
		}
		return DashboardActionNone{}
	case ConfirmationCloseFailed:
		switch index {
		case 1:
			d.CancelModal()
			return DashboardActionSuspend{SessionID: c.SessionID, Consent: core.SuspendConsent{}}
		case 2:
			d.CancelModal()
			return DashboardActionSuspend{SessionID: c.SessionID, Consent: core.SuspendConsent{
				WithoutCheckpoint:   true,
				ConfirmNoCheckpoint: true,
			}}
		}
	case ConfirmationSuspendSession:
		if index == 1 && !c.Checking && (c.Preflight == nil || !c.Preflight.HasBlockers()) {
			d.CancelModal()
			consent := core.SuspendConsent{}
			if c.Preflight != nil {
				consent = c.Preflight.ShownConsent()
			}
			return DashboardActionSuspend{SessionID: c.SessionID, Consent: consent}
		}
	case ConfirmationRestartAgent:
		if index == 1 {
			d.CancelModal()
			return DashboardActionRestartSession{SessionID: c.SessionID}
		}
	case ConfirmationRestartContainer:
		if !c.UpgradeOnly && index == 1 {
			d.CancelModal()
			return DashboardActionRestartSession{SessionID: c.SessionID}
		}
		upgradeIndex := 2
		if c.UpgradeOnly {
			upgradeIndex = 1
		}
		if index == upgradeIndex {
			d.CancelModal()
			return DashboardActionUpgradeContainer{Request: core.ContainerUpgradeRequest{
				Selection:                    c.Preview,
				AcknowledgeWritableLayerLoss: true,
			}}
		}
	case ConfirmationInterruptWork:
		if index == 1 {
			d.CancelModal()
			if c.Restart {
				return DashboardActionRestartSession{SessionID: c.SessionID}
			}
			return DashboardActionSuspend{SessionID: c.SessionID, Consent: core.SuspendConsent{}}
		}
	case ConfirmationInterruptAll:
		if index == 1 {
			d.CancelModal()
			return DashboardActionInterruptAll{SessionID: c.SessionID}
		}
	case ConfirmationRecoverFailed:
		switch index {
		case 1:
			d.CancelModal()
			d.focus = FocusPrompt
			return DashboardActionOpen{SessionID: c.SessionID}
		case 2:
			d.CancelModal()
			return d.BeginResumeFor(c.SessionID)
		}
	case ConfirmationRecoverMove:
		switch index {
		case 2:
			d.CancelModal()
			return DashboardActionRetryMove{Operation: c.Operation}
		case 3:
			d.CancelModal()
			return DashboardActionResumeMove{Operation: c.Operation}
		}
	}
	d.CancelModal()
	return DashboardActionNone{}
}

func (d *DashboardState) restoreDismissedMode(mode Mode) DashboardAction {
	d.mode = mode
	return DashboardActionNone{}
}

// restoreAfterConfirmation returns to the resume dialog a confirmation
// interrupted, or to the dashboard when the confirmation did not come from
// one (reopen nil).
func (d *DashboardState) restoreAfterConfirmation(reopen *ResumeDialog) {
	if reopen == nil {
		d.CancelModal()
		return
	}
	d.mode = ModeResumeDialog{reopen}
	d.RebuildResumeRows()
}

// focusedPathInput is CompletesPaths::focused_path_input. The replacement
// origin may be a URL, so it completes only while it reads as a path on the
// controller.
func (d *RepositoryOriginDialog) focusedPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	if !d.form.IsFocused(DialogControlField) || !core.LooksLikePath(d.replacement.Value()) {
		return nil, nil, 0, false
	}
	return &d.replacement, core.CompletionHostLocal{}, core.CompletionKindDirectories, true
}

// dismissUnfocusedCompletions is CompletesPaths::dismiss_unfocused_completions.
func (d *RepositoryOriginDialog) dismissUnfocusedCompletions() {
	if !d.form.IsFocused(DialogControlField) {
		d.replacement.DismissCompletion()
	}
}

// reconcileSessionCpuReport settles the report's machine tab and selected row
// against the current groups, returning the selected group's index (false
// when there are no groups).
func reconcileSessionCpuReport(dashboard *DashboardState, dialog *SessionCpuReportDialog, groups []MachineReport) (int, bool) {
	if len(groups) == 0 {
		dialog.machineKey = nil
		dialog.selectedSessionID = nil
		dialog.selectedRow = 0
		return 0, false
	}

	rememberedKey := dialog.machineKey
	dashboardSelection, hasDashboardSelection := dashboard.SelectedSessionID()
	groupIndex := -1
	if rememberedKey != nil {
		groupIndex = slices.IndexFunc(groups, func(group MachineReport) bool { return group.key == *rememberedKey })
	}
	if groupIndex < 0 && rememberedKey == nil && hasDashboardSelection {
		groupIndex = slices.IndexFunc(groups, func(group MachineReport) bool {
			return slices.Contains(group.sessionIDs, dashboardSelection)
		})
	}
	if groupIndex < 0 {
		groupIndex = 0
	}
	group := &groups[groupIndex]
	key := group.key
	dialog.machineKey = &key

	previousRow := dialog.selectedRow
	storedSelection := dialog.selectedSessionID
	rowIndex := -1
	if storedSelection != nil {
		rowIndex = slices.Index(group.sessionIDs, *storedSelection)
	}
	if rowIndex < 0 && storedSelection == nil && hasDashboardSelection {
		rowIndex = slices.Index(group.sessionIDs, dashboardSelection)
	}
	if rowIndex < 0 {
		rowIndex = min(previousRow, max(len(group.sessionIDs)-1, 0))
	}
	dialog.selectedSessionID = nil
	if rowIndex < len(group.sessionIDs) {
		selected := group.sessionIDs[rowIndex]
		dialog.selectedSessionID = &selected
	}
	dialog.selectedRow = rowIndex
	dialog.form.SetSelected(DialogControlSessionCpuReportRows, rowIndex)
	return groupIndex, true
}

func switchSessionCpuReportTab(dialog *SessionCpuReportDialog, groups []MachineReport, index int) {
	if index < 0 || index >= len(groups) {
		return
	}
	group := &groups[index]
	key := group.key
	dialog.machineKey = &key
	dialog.selectedRow = 0
	dialog.selectedSessionID = nil
	if len(group.sessionIDs) > 0 {
		first := group.sessionIDs[0]
		dialog.selectedSessionID = &first
	}
	dialog.form.SetSelected(DialogControlSessionCpuReportRows, 0)
	dialog.form.Focus(DialogControlSessionCpuReportRows)
}

// moveSessionCpuReportSelection moves the selected row by delta, where
// math.MinInt and math.MaxInt (Rust's isize::MIN and isize::MAX) jump to the
// first and last rows.
func moveSessionCpuReportSelection(dialog *SessionCpuReportDialog, group *MachineReport, delta int) {
	last := max(len(group.sessionIDs)-1, 0)
	var selected int
	switch {
	case delta == math.MinInt:
		selected = 0
	case delta == math.MaxInt:
		selected = last
	case delta < 0:
		selected = max(dialog.selectedRow+delta, 0)
	default:
		selected = min(dialog.selectedRow+delta, last)
	}
	selectSessionCpuReportRow(dialog, group, selected)
}

func selectSessionCpuReportRow(dialog *SessionCpuReportDialog, group *MachineReport, index int) {
	if index < 0 || index >= len(group.sessionIDs) {
		return
	}
	sessionID := group.sessionIDs[index]
	dialog.selectedRow = index
	dialog.selectedSessionID = &sessionID
	dialog.form.SetSelected(DialogControlSessionCpuReportRows, index)
	dialog.form.Focus(DialogControlSessionCpuReportRows)
}

func (d *DashboardState) ShowContainerUpgradePreview(preview core.ContainerUpgradePreview, upgradeOnly bool) {
	if !preview.Available {
		if upgradeOnly {
			d.SetNotice("The container already uses the selected image.")
		} else {
			d.ShowNormalRestart(preview.SessionID)
		}
		return
	}
	dialog := newConfirmDialog(ConfirmationRestartContainer{
		SessionID:   preview.SessionID,
		Preview:     preview,
		UpgradeOnly: upgradeOnly,
	})
	d.mode = ModeConfirm{&dialog}
}

func (d *DashboardState) ShowNormalRestart(sessionID string) {
	interrupting := d.hasActiveWork(sessionID)
	dialog := newConfirmDialog(ConfirmationRestartAgent{SessionID: sessionID, Interrupting: interrupting})
	d.mode = ModeConfirm{&dialog}
}
