// Port of mj-tui/src/actions.rs.
//
// The one place that knows what the combined surface's keys do.
//
// Every pane command has exactly one [CommandSpec] here, carrying its keys,
// the word the footer uses for it, and a function that says whether it
// applies right now. Key handling (DashboardState.handleDashboardKey), the
// footer (combinedFooterText in render), and the command palette all read
// this table, so a binding and its advertisement cannot drift apart.
//
// The registry lives in the tui package rather than a package of its own
// because availability is a question about [DashboardState].
//
// Key events are Bubble Tea's. The hints are written against keys as
// crossterm reports them; [readPaneKey] translates a Bubble Tea key press
// into that reading.
package tui

import (
	"fmt"
	"strings"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CommandId is one thing the surface can be asked to do.
type CommandId int

const (
	CommandIdNotificationOpen CommandId = iota
	CommandIdNotificationSend
	CommandIdNotificationRestore
	CommandIdNotificationDefault
	CommandIdNotificationClear
	CommandIdNotificationSkip
	CommandIdNotificationIgnoreSession
	CommandIdNotificationPreviousMessage
	CommandIdNotificationNextMessage
	CommandIdNotificationScrollUp
	CommandIdNotificationScrollDown
	CommandIdGoAfk
	CommandIdOpenSession
	CommandIdPinSession
	CommandIdChangeWorkspace
	CommandIdOpenSessionSplitRight
	CommandIdOpenSessionSplitBelow
	CommandIdClosePane
	CommandIdFocusPaneLeft
	CommandIdFocusPaneDown
	CommandIdFocusPaneUp
	CommandIdFocusPaneRight
	CommandIdFocusLastPane
	CommandIdZoomPane
	CommandIdResizeMode
	CommandIdSwapPaneLeft
	CommandIdSwapPaneDown
	CommandIdSwapPaneUp
	CommandIdSwapPaneRight
	CommandIdSuspendSession
	CommandIdRenameWorkspace
	CommandIdCloseWorkspace
	CommandIdResizePaneLeft
	CommandIdResizePaneDown
	CommandIdResizePaneUp
	CommandIdResizePaneRight
	CommandIdNewSessionWizard
	CommandIdChangeGoSetup
	CommandIdRestartSession
	CommandIdUpgradeContainer
	CommandIdResumeDialog
	CommandIdRenameSession
	CommandIdChangedFiles
	CommandIdDismissNotification
	CommandIdOpenWaitingQuestion
	CommandIdIgnoreNotifications
	CommandIdContainerSettings
	CommandIdMoveSession
	CommandIdCopySessionId
	CommandIdDestroySession
	CommandIdMarkAllRead
	CommandIdFilterSessions
	CommandIdNextAttention
	CommandIdPreviousAttention
	CommandIdCancelOperation
	CommandIdToggleProject
	CommandIdTargetActions
	CommandIdTargetDisks
	CommandIdEditProfile
	CommandIdToggleQuotaPool
	CommandIdRefresh
	CommandIdOpenConfig
	CommandIdManageProfiles
	CommandIdManageMachines
	CommandIdManageTargets
	CommandIdCycleFocus
	CommandIdCycleFocusReverse
	CommandIdCycleFocusedPaneSize
	CommandIdTogglePanePreset
	CommandIdTargetsMenu
	CommandIdProfilesMenu
	CommandIdWorkspaces
	CommandIdFocusWorkspaces
	CommandIdSelectWorkspacePrevious
	CommandIdSelectWorkspaceNext
	CommandIdSwitchWorkspace
	CommandIdWebViewer
	CommandIdRestartDaemon
	CommandIdNoticeLog
	CommandIdSessionCpuReport
	CommandIdSessionUsageReport
	CommandIdQuitDetach
	CommandIdPalette
	CommandIdCycleSpinner
	CommandIdToggleTranscriptRendering
	CommandIdToggleDictation
	CommandIdOpenSubagents
	CommandIdSessionActions
	CommandIdInterruptTurn
	CommandIdInterruptAll
)

var commandIdNames = [...]string{
	"NotificationOpen",
	"NotificationSend",
	"NotificationRestore",
	"NotificationDefault",
	"NotificationClear",
	"NotificationSkip",
	"NotificationIgnoreSession",
	"NotificationPreviousMessage",
	"NotificationNextMessage",
	"NotificationScrollUp",
	"NotificationScrollDown",
	"GoAfk",
	"OpenSession",
	"PinSession",
	"ChangeWorkspace",
	"OpenSessionSplitRight",
	"OpenSessionSplitBelow",
	"ClosePane",
	"FocusPaneLeft",
	"FocusPaneDown",
	"FocusPaneUp",
	"FocusPaneRight",
	"FocusLastPane",
	"ZoomPane",
	"ResizeMode",
	"SwapPaneLeft",
	"SwapPaneDown",
	"SwapPaneUp",
	"SwapPaneRight",
	"SuspendSession",
	"RenameWorkspace",
	"CloseWorkspace",
	"ResizePaneLeft",
	"ResizePaneDown",
	"ResizePaneUp",
	"ResizePaneRight",
	"NewSessionWizard",
	"ChangeGoSetup",
	"RestartSession",
	"UpgradeContainer",
	"ResumeDialog",
	"RenameSession",
	"ChangedFiles",
	"DismissNotification",
	"OpenWaitingQuestion",
	"IgnoreNotifications",
	"ContainerSettings",
	"MoveSession",
	"CopySessionId",
	"DestroySession",
	"MarkAllRead",
	"FilterSessions",
	"NextAttention",
	"PreviousAttention",
	"CancelOperation",
	"ToggleProject",
	"TargetActions",
	"TargetDisks",
	"EditProfile",
	"ToggleQuotaPool",
	"Refresh",
	"OpenConfig",
	"ManageProfiles",
	"ManageMachines",
	"ManageTargets",
	"CycleFocus",
	"CycleFocusReverse",
	"CycleFocusedPaneSize",
	"TogglePanePreset",
	"TargetsMenu",
	"ProfilesMenu",
	"Workspaces",
	"FocusWorkspaces",
	"SelectWorkspacePrevious",
	"SelectWorkspaceNext",
	"SwitchWorkspace",
	"WebViewer",
	"RestartDaemon",
	"NoticeLog",
	"SessionCpuReport",
	"SessionUsageReport",
	"QuitDetach",
	"Palette",
	"CycleSpinner",
	"ToggleTranscriptRendering",
	"ToggleDictation",
	"OpenSubagents",
	"SessionActions",
	"InterruptTurn",
	"InterruptAll",
}

// String is the variant name, as Rust's derived Debug prints it.
func (id CommandId) String() string {
	if id < 0 || int(id) >= len(commandIdNames) {
		return "CommandId(?)"
	}
	return commandIdNames[id]
}

// Scope is where a command belongs: which pane has to own the keyboard for it
// to apply, or Global/Pane/Settings for commands available from any pane.
//
// Sessions is the pane itself (create, mark read); Session is the selected
// row (rename, container settings, stop). Setup is the first-run path that
// only exists while the configuration is empty.
type Scope int

const (
	ScopeNotifications Scope = iota
	ScopeGlobal
	ScopeSettings
	ScopePane
	ScopeSessions
	ScopeSession
	ScopeTargets
	ScopeQuota
	ScopeSetup
)

// heading is the heading the command palette prints above this scope.
func (s Scope) heading() string {
	switch s {
	case ScopeNotifications:
		return "Notifications pane"
	case ScopeGlobal:
		return "Anywhere"
	case ScopeSettings:
		return "Settings"
	case ScopePane:
		return "Panes"
	case ScopeSessions:
		return "Sessions pane"
	case ScopeSession:
		return "Selected session"
	case ScopeTargets:
		return "Targets pane"
	case ScopeQuota:
		return "Profiles pane"
	case ScopeSetup:
		return "First-run settings"
	}
	panic("unknown Scope")
}

// Availability is whether a command can be run, and if not, why the user
// cannot see it.
//
// AvailabilityHidden means the command makes no sense here at all (container
// settings for a session that is not on a container). AvailabilityBlocked
// means it would make sense but something is in the way, and carries the
// sentence saying so. Every variant is a comparable value, so
// `availability == (AvailabilityReady{})` is Rust's `== Availability::Ready`.
type Availability interface {
	isAvailability()
}

type AvailabilityReady struct{}

type AvailabilityHidden struct{}

type AvailabilityBlocked struct {
	Reason string
}

func (AvailabilityReady) isAvailability()   {}
func (AvailabilityHidden) isAvailability()  {}
func (AvailabilityBlocked) isAvailability() {}

// KeyHint is one plain key a focused pane answers, with the text used to
// name it.
//
// Pane keys are fixed in the registry. Text-bearing panes use modifiers so
// bare characters remain editor input. Configurable global bindings are
// core.KeyAction entries on the spec instead.
//
// code is a crossterm-style key code: a character for crossterm's
// `KeyCode::Char`, otherwise Bubble Tea's named key (tea.KeyEnter, ...).
type KeyHint struct {
	code      rune
	label     string
	modifiers tea.KeyMod
	exact     bool
}

func keyHintPlain(code rune, label string) KeyHint {
	return KeyHint{code: code, label: label}
}

func keyHintAlt(code rune, label string) KeyHint {
	return KeyHint{code: code, label: label, modifiers: tea.ModAlt, exact: true}
}

func keyHintEnter() KeyHint {
	return KeyHint{code: tea.KeyEnter, label: "Enter", exact: true}
}

func (h KeyHint) acceptsFocus(focus Focus) bool {
	if !chat.IsCharKeyCode(h.code) {
		return true
	}
	switch focus {
	case FocusPrompt:
		return false
	case FocusNotifications:
		return h.modifiers != 0
	default:
		return true
	}
}

// paneKey is a key press as crossterm reports it, which is what the hints
// are written against.
type paneKey struct {
	code      rune
	modifiers tea.KeyMod
	// backTab is crossterm's BackTab. Bubble Tea reports Shift-Tab as Tab
	// carrying Shift, where crossterm names a separate key that the Tab hint
	// does not answer.
	backTab bool
}

// readPaneKey reads a Bubble Tea key press the way crossterm reports it: the
// typed character for a printable key, keypad keys folded onto the main
// ones, and no lock states among the modifiers.
func readPaneKey(key tea.KeyPressMsg) paneKey {
	return paneKey{
		code:      chat.KeyPressCode(key),
		modifiers: key.Mod &^ (tea.ModCapsLock | tea.ModNumLock | tea.ModScrollLock),
		backTab:   key.Code == tea.KeyTab && key.Mod.Contains(tea.ModShift),
	}
}

// matches reports whether a pressed key is this hint. plain is the caller's
// reading of "no accelerator and no Alt", computed once per key press.
func (h KeyHint) matches(key paneKey, plain bool) bool {
	if key.backTab {
		return false
	}
	if h.exact {
		return h.code == key.code && key.modifiers == h.modifiers
	}
	if h.code != key.code {
		return false
	}
	if chat.IsCharKeyCode(h.code) {
		// Plain letters are pane keys: a modifier means something else.
		return plain
	}
	// Enter and Tab have always answered whatever modifiers came with them.
	return true
}

// FooterGroup is which of the footer's two groups a hint prints in.
//
// The row reads `pane commands │ prefix chords`, so the reader always finds a
// key in the same place: what applies here, then what applies everywhere
// after the prefix. Group membership is stated here rather than inferred from
// [Scope], because Tab and the pane preset share a scope and belong in
// different groups. The order of the constants is the order of the groups.
type FooterGroup int

const (
	FooterGroupPane FooterGroup = iota
	FooterGroupChord
)

// CommandSpec is one command: what it is called, what runs it, and when it
// applies.
type CommandSpec struct {
	id          CommandId
	label       string
	description string
	scope       Scope
	// paneKeys are keys a focused pane answers. Not configurable.
	paneKeys []KeyHint
	// action is the configurable action that also runs this command, if any.
	action *core.KeyAction
	// footer is the word the footer prints after the key, or false for
	// commands the footer never has room to name. Dynamic because
	// CommandIdCancelOperation names the operation it would cancel.
	footer func(dashboard *DashboardState) (string, bool)
	// footerGroup is which footer group the hint prints in, and footerRank
	// where inside it. Equal ranks keep registry order, so only the groups
	// whose order matters to the reader — the chords and the function keys —
	// carry distinct ranks.
	footerGroup FooterGroup
	footerRank  uint8
	available   func(dashboard *DashboardState) Availability
}

// keyAction is Rust's `Some(KeyAction::...)` for a registry entry.
func keyAction(action core.KeyAction) *core.KeyAction {
	return &action
}

func noFooter(*DashboardState) (string, bool) {
	return "", false
}

// footerWord builds a footer function that always prints the same word.
func footerWord(word string) func(*DashboardState) (string, bool) {
	return func(*DashboardState) (string, bool) {
		return word, true
	}
}

func alwaysReady(*DashboardState) Availability {
	return AvailabilityReady{}
}

func operatorAwayAvailable(dashboard *DashboardState) Availability {
	if _, returning := dashboard.operatorPresence.(OperatorPresenceReturning); returning {
		return AvailabilityBlocked{Reason: "Waiting for the daemon to confirm your return"}
	}
	return AvailabilityReady{}
}

func spinnerAvailable(dashboard *DashboardState) Availability {
	if dashboard.spinnerSavePending {
		return AvailabilityBlocked{Reason: "The spinner preference is being saved"}
	}
	return AvailabilityReady{}
}

func activeWorkspaceReady(dashboard *DashboardState) Availability {
	if _, ok := dashboard.ActiveWorkspaceID(); ok {
		return AvailabilityReady{}
	}
	return AvailabilityBlocked{Reason: "No active workspace"}
}

// conversationPaneReady is the gate the conversation-pane commands share.
// They act on the pane the conversation is in, so they apply where a
// conversation is what the keyboard is near: the composer itself, or the
// Sessions list beside it.
func conversationPaneReady(dashboard *DashboardState) Availability {
	if dashboard.focus == FocusPrompt || dashboard.focus == FocusSessions {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

func selectedSessionReady(dashboard *DashboardState) Availability {
	if dashboard.commandSession() != nil {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

func dismissNotificationAvailable(dashboard *DashboardState) Availability {
	session := dashboard.commandSession()
	if session == nil {
		return AvailabilityHidden{}
	}
	return dismissNotificationAvailableFor(dashboard, session.ID)
}

// dismissNotificationAvailableFor is [dismissNotificationAvailable] for a
// named session, so a menu on one row answers for that row and not for the
// selected session.
func dismissNotificationAvailableFor(dashboard *DashboardState, sessionID string) Availability {
	if !dashboard.state.Get().Sessions.ContainsKey(sessionID) {
		return AvailabilityHidden{}
	}
	// A child's open form is answered in the child, never dismissed here:
	// Open sub-agent question takes this row's place.
	if _, ok := dashboard.waitingChildOf(sessionID); ok {
		return AvailabilityHidden{}
	}
	if dashboard.notificationDismissalPending(sessionID) {
		return AvailabilityBlocked{Reason: "Dismissing notification"}
	}
	if _, ok := dashboard.notificationDismissalTarget(sessionID); !ok {
		return AvailabilityBlocked{Reason: "No dismissible notification is active"}
	}
	return AvailabilityReady{}
}

func openWaitingQuestionAvailable(dashboard *DashboardState) Availability {
	if session := dashboard.commandSession(); session != nil {
		if _, ok := dashboard.waitingChildOf(session.ID); ok {
			return AvailabilityReady{}
		}
	}
	return AvailabilityHidden{}
}

func ignoreNotificationsAvailable(dashboard *DashboardState) Availability {
	// Always applies to a session: the row's label says which way it flips.
	if dashboard.commandSession() != nil {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

func pinSessionAvailable(dashboard *DashboardState) Availability {
	// The row flips with the pin state: it pins, or it unpins.
	if dashboard.commandSession() != nil {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

// sessionIdle is the gate the session commands share: there must be a
// selected session, and it must not be in the middle of a launch or a stop.
func sessionIdle(dashboard *DashboardState) Availability {
	session := dashboard.commandSession()
	if session == nil {
		return AvailabilityHidden{}
	}
	if dashboard.verdict(session.ID).MoveQueueHeld {
		return AvailabilityBlocked{Reason: "Move queue admission is incomplete; retry Move first"}
	}
	if dashboard.TransitionKind(session.ID) != nil {
		return AvailabilityBlocked{Reason: "a session transition is in progress"}
	}
	if dashboard.TransitionFailureKind(session.ID) != nil {
		return AvailabilityBlocked{Reason: "recover the failed transition first"}
	}
	if _, ok := dashboard.SessionOperationKind(session.ID); ok {
		return AvailabilityBlocked{Reason: "an operation is in progress"}
	}
	return AvailabilityReady{}
}

func offer(offer client.SessionOffer) Availability {
	switch offer {
	case client.SessionOfferHidden:
		return AvailabilityHidden{}
	case client.SessionOfferReady:
		return AvailabilityReady{}
	case client.SessionOfferMoveQueueHeld:
		return AvailabilityBlocked{Reason: "Move queue admission is incomplete; retry Move first"}
	case client.SessionOfferTransitioning:
		return AvailabilityBlocked{Reason: "a session transition is in progress"}
	case client.SessionOfferTransitionFailed:
		return AvailabilityBlocked{Reason: "recover the failed transition first"}
	case client.SessionOfferOperationInProgress:
		return AvailabilityBlocked{Reason: "an operation is in progress"}
	case client.SessionOfferNoRecoveryCopy:
		return AvailabilityBlocked{Reason: "this session has no recovery copy"}
	case client.SessionOfferNoOtherWorkspace:
		return AvailabilityBlocked{Reason: "there is no other workspace"}
	case client.SessionOfferActive:
		return AvailabilityBlocked{Reason: "this session is active"}
	case client.SessionOfferMoveEnvironmentRetained:
		return AvailabilityBlocked{Reason: "retry Move to recover the retained environment"}
	}
	panic("unknown SessionOffer")
}

func lifecycleOffer(
	dashboard *DashboardState,
	published func(client.SessionVerdict) client.SessionOffer,
) Availability {
	id, ok := dashboard.commandSessionID()
	if !ok {
		return AvailabilityHidden{}
	}
	availability := offer(published(dashboard.verdict(id)))
	if availability == (AvailabilityReady{}) {
		if _, pending := dashboard.pendingLocalSessionOperation(id); pending {
			return AvailabilityBlocked{Reason: "an operation is in progress"}
		}
	}
	return availability
}

func suspendSessionAvailable(dashboard *DashboardState) Availability {
	return lifecycleOffer(dashboard, func(verdict client.SessionVerdict) client.SessionOffer {
		return verdict.Stop
	})
}

func restartSessionAvailable(dashboard *DashboardState) Availability {
	return lifecycleOffer(dashboard, func(verdict client.SessionVerdict) client.SessionOffer {
		return verdict.Restart
	})
}

func moveSessionAvailable(dashboard *DashboardState) Availability {
	return lifecycleOffer(dashboard, func(verdict client.SessionVerdict) client.SessionOffer {
		return verdict.MoveSession
	})
}

// liveSession is a selected session whose target is running, which is what
// reading its checkout needs.
func liveSession(dashboard *DashboardState) Availability {
	session := dashboard.commandSession()
	if session == nil {
		return AvailabilityHidden{}
	}
	if session.State.IsActive() && session.Target != nil {
		return AvailabilityReady{}
	}
	return AvailabilityBlocked{Reason: "the session's target is not running"}
}

func changeWorkspaceAvailable(dashboard *DashboardState) Availability {
	// The zero SessionOffer is its default, Hidden.
	var published client.SessionOffer
	if id, ok := dashboard.commandSessionID(); ok {
		published = dashboard.verdict(id).ChangeWorkspace
	}
	return offer(published)
}

func containerSession(dashboard *DashboardState) Availability {
	// The zero SessionOffer is its default, Hidden.
	var published client.SessionOffer
	if id, ok := dashboard.commandSessionID(); ok {
		published = dashboard.verdict(id).ContainerSettings
	}
	return offer(published)
}

func configPresent(dashboard *DashboardState) Availability {
	if dashboard.configIsEmpty() {
		return AvailabilityBlocked{Reason: "configure at least one profile and target first"}
	}
	return AvailabilityReady{}
}

func profilesPresent(dashboard *DashboardState) Availability {
	if dashboard.PaneSize(SupportPaneQuota) == core.PaneSizeMinimized {
		return AvailabilityHidden{}
	}
	for range dashboard.config.Get().EnabledProfiles() {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

func targetsVisible(dashboard *DashboardState) Availability {
	if dashboard.PaneSize(SupportPaneTargets) == core.PaneSizeMinimized {
		return AvailabilityHidden{}
	}
	return AvailabilityReady{}
}

func supportPaneFocused(dashboard *DashboardState) Availability {
	if _, ok := dashboard.Focus().SupportPane(); ok {
		return AvailabilityReady{}
	}
	return AvailabilityBlocked{Reason: "select Sessions, Targets, or Profiles first"}
}

func cancelFooter(dashboard *DashboardState) (string, bool) {
	session := dashboard.commandSession()
	if session == nil {
		return "", false
	}
	operation, ok := (*dashboard.sessionOperations.Get())[session.ID]
	if !ok || !operation.cancellable {
		return "", false
	}
	return "cancel " + strings.ToLower(operation.kind.Label()), true
}

// attentionFooter names the next-attention key only while something is
// actually waiting, and carries the same badge as the tabs, so the hint is a
// signal as well as a reminder of the key.
func attentionFooter(dashboard *DashboardState) (string, bool) {
	level, count, ok := dashboard.attentionBadgeSummary()
	if !ok {
		return "", false
	}
	return fmt.Sprintf("next (%s%d)", attentionGlyph(level), count), true
}

// subagentsAvailable: the selected session (or, from the composer, the
// conversation's) must have sub-agents to show. With none, the command stays
// listed and says why, so a search for "sub-agent" always finds it.
func subagentsAvailable(dashboard *DashboardState) Availability {
	session, ok := dashboard.commandSessionID()
	if !ok {
		return AvailabilityHidden{}
	}
	if dashboard.SubagentCountFor(session) > 0 {
		return AvailabilityReady{}
	}
	return AvailabilityBlocked{Reason: "this session has no sub-agents"}
}

// interruptAvailable keeps the command listed when there is no turn or user
// shell to interrupt, so a search for "interrupt" always finds it.
func interruptAvailable(dashboard *DashboardState) Availability {
	session, ok := dashboard.commandSessionID()
	if !ok {
		return AvailabilityHidden{}
	}
	verdict := dashboard.verdict(session)
	if verdict.Interrupt {
		return AvailabilityReady{}
	}
	if verdict.InterruptShell {
		if _, open := dashboard.PaneForSession(session); open {
			return AvailabilityReady{}
		}
		return AvailabilityBlocked{Reason: "open the conversation to interrupt its shell"}
	}
	return AvailabilityBlocked{Reason: "no turn is running"}
}

// interruptAllAvailable: Interrupt all stays listed with its reason when
// nothing in the session's tree is running, for the same reason Interrupt
// turn does.
func interruptAllAvailable(dashboard *DashboardState) Availability {
	session, ok := dashboard.commandSessionID()
	if !ok {
		return AvailabilityHidden{}
	}
	if !dashboard.verdict(session).InterruptAll {
		return AvailabilityBlocked{Reason: "no turn is running here or in its sub-agents"}
	}
	return AvailabilityReady{}
}

// subagentsFooter is named in the footer only while there are sub-agents to
// open.
func subagentsFooter(dashboard *DashboardState) (string, bool) {
	if subagentsAvailable(dashboard) == (AvailabilityReady{}) {
		return "sub-agents", true
	}
	return "", false
}

func operationInFlight(dashboard *DashboardState) Availability {
	if _, ok := cancelFooter(dashboard); ok {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

// commands is every command the surface has. The order here is the order the
// footer prints its hints and the command palette groups its entries.
//
// It is filled by init rather than by its declaration: its entries refer to
// functions that reach back into the registry, which Go would reject as an
// initialization cycle in a package-level initializer.
var commands []CommandSpec

func init() {
	commands = commandTable()
}

func commandTable() []CommandSpec {
	return []CommandSpec{
		{
			id:          CommandIdNotificationOpen,
			label:       "Go to Session",
			description: "Open the notification session.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt('g', "Alt+g")},
			footer:      footerWord("session"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   notificationAvailable,
		},
		{
			id:          CommandIdNotificationSend,
			label:       "Send",
			description: "Send the quick reply to its session.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{{code: tea.KeyEnter, label: "Ctrl+Enter", modifiers: tea.ModCtrl, exact: true}},
			footer:      footerWord("send"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   notificationReplyAvailable,
		},
		{
			id:          CommandIdNotificationRestore,
			label:       "Restore unsent",
			description: "Restore a retained unsent reply without submitting it.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt(tea.KeyEnter, "Alt+Enter")},
			footer:      footerWord("restore"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   notificationRestoreAvailable,
		},
		{
			id:          CommandIdNotificationDefault,
			label:       "Reply or open session",
			description: "Send a nonempty quick reply, otherwise open the session.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintEnter()},
			footer:      footerWord("send/session"),
			footerGroup: FooterGroupPane,
			footerRank:  1,
			available:   notificationReplyAvailable,
		},
		{
			id:          CommandIdNotificationClear,
			label:       "Clear & Next",
			description: "Clear & Next in the Notifications pane.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt('n', "Alt+n")},
			footer:      footerWord("next"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   notificationClearAvailable,
		},
		{
			id:          CommandIdNotificationSkip,
			label:       "Skip",
			description: "Skip in the Notifications pane.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt('s', "Alt+s")},
			footer:      footerWord("skip"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   notificationAvailable,
		},
		{
			id:          CommandIdNotificationIgnoreSession,
			label:       "Ignore Session",
			description: "Skip this card and ignore all notifications from its session.",
			scope:       ScopeNotifications,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   notificationAvailable,
		},
		{
			id:          CommandIdNotificationPreviousMessage,
			label:       "Previous message",
			description: "Show the previous message on this notification card.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt(tea.KeyPgUp, "Alt+PgUp")},
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available: func(dashboard *DashboardState) Availability {
				return notificationNavigationAvailable(dashboard, CommandIdNotificationPreviousMessage)
			},
		},
		{
			id:          CommandIdNotificationNextMessage,
			label:       "Next message",
			description: "Show the next message on this notification card.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt(tea.KeyPgDown, "Alt+PgDn")},
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available: func(dashboard *DashboardState) Availability {
				return notificationNavigationAvailable(dashboard, CommandIdNotificationNextMessage)
			},
		},
		{
			id:          CommandIdNotificationScrollUp,
			label:       "Scroll notification up",
			description: "Scroll notification up in the Notifications pane.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt(tea.KeyUp, "Alt+↑")},
			footer:      footerWord("scroll"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available: func(dashboard *DashboardState) Availability {
				if dashboard.NotificationFormOwnsKeyboard() {
					return AvailabilityHidden{}
				}
				return notificationAvailable(dashboard)
			},
		},
		{
			id:          CommandIdNotificationScrollDown,
			label:       "Scroll notification down",
			description: "Scroll notification down in the Notifications pane.",
			scope:       ScopeNotifications,
			paneKeys:    []KeyHint{keyHintAlt(tea.KeyDown, "Alt+↓")},
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available: func(dashboard *DashboardState) Availability {
				if dashboard.NotificationFormOwnsKeyboard() {
					return AvailabilityHidden{}
				}
				return notificationAvailable(dashboard)
			},
		},
		{
			id:          CommandIdChangeGoSetup,
			label:       "Change fast-start setup",
			description: "Choose and remember a new account, target, or isolation for this project.",
			scope:       ScopeSettings,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionChangeGoSetup),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available: func(dashboard *DashboardState) Availability {
				// Ready while the dashboard runs in fast-start (`mj go`) mode,
				// otherwise Hidden.
				if dashboard.goMode != nil {
					return AvailabilityReady{}
				}
				return AvailabilityHidden{}
			},
		},
		{
			id:          CommandIdOpenSession,
			label:       "Open session",
			description: "Show the selected session's conversation and type in it.",
			scope:       ScopeSessions,
			paneKeys:    []KeyHint{keyHintPlain(tea.KeyEnter, "Enter")},
			footer:      footerWord("open"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   selectedSessionReady,
		},
		{
			id:          CommandIdPinSession,
			label:       "Pin session…",
			description: "Keep a session visible while browsing others; when pinned, leaves this pane without stopping the session.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   pinSessionAvailable,
		},
		{
			id:          CommandIdOpenSessionSplitRight,
			label:       "Split right",
			description: "Split this pane right; keep the old Browse session pinned and browse in the new pane.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSplitVertical),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdOpenSessionSplitBelow,
			label:       "Split down",
			description: "Split this pane down; keep the old Browse session pinned and browse in the new pane.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSplitHorizontal),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdClosePane,
			label:       "Close pane",
			description: "Remove the conversation pane you are in; the last one is emptied instead. The Browse pane cannot be closed; move it with Swap pane.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionClosePane),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdFocusPaneLeft,
			label:       "Focus pane left",
			description: "Move the keyboard to the conversation pane left of this one.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionFocusPaneLeft),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdFocusPaneDown,
			label:       "Focus pane down",
			description: "Move the keyboard to the conversation pane below this one.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionFocusPaneDown),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdFocusPaneUp,
			label:       "Focus pane up",
			description: "Move the keyboard to the conversation pane above this one.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionFocusPaneUp),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdFocusPaneRight,
			label:       "Focus pane right",
			description: "Move the keyboard to the conversation pane right of this one.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionFocusPaneRight),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdFocusLastPane,
			label:       "Focus last pane",
			description: "Move the keyboard back to the conversation pane it was in before.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionLastPane),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdZoomPane,
			label:       "Zoom pane",
			description: "Fill the conversation area with the pane you are in, or put the others back.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionZoom),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdResizeMode,
			label:       "Resize panes",
			description: "Resize conversation panes with h/j/k/l or arrows until Escape.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionResizeMode),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdSwapPaneLeft,
			label:       "Swap pane left",
			description: "Move the focused conversation pane left, exchanging places with its neighbor.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSwapPaneLeft),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdSwapPaneDown,
			label:       "Swap pane down",
			description: "Move the focused conversation pane down, exchanging places with its neighbor.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSwapPaneDown),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdSwapPaneUp,
			label:       "Swap pane up",
			description: "Move the focused conversation pane up, exchanging places with its neighbor.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSwapPaneUp),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdSwapPaneRight,
			label:       "Swap pane right",
			description: "Move the focused conversation pane right, exchanging places with its neighbor.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSwapPaneRight),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdSuspendSession,
			label:       "Suspend session…",
			description: "Save a recovery copy and release the environment. Resume this session later.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSuspendSession),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   suspendSessionAvailable,
		},
		{
			id:          CommandIdGoAfk,
			label:       "Go AFK",
			description: "Tell agents the operator is away until this window is closed.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   operatorAwayAvailable,
		},
		{
			id:          CommandIdRenameWorkspace,
			label:       "Rename workspace…",
			description: "Rename the active workspace.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionRenameWorkspace),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   activeWorkspaceReady,
		},
		{
			id:          CommandIdCloseWorkspace,
			label:       "Delete workspace…",
			description: "Confirm suspending workspace sessions, preserving history, and discarding drafts.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionCloseWorkspace),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   activeWorkspaceReady,
		},
		{
			id:          CommandIdResizePaneLeft,
			label:       "Resize pane left",
			description: "Move the border of the conversation pane you are in left one step.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionResizePaneLeft),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdResizePaneDown,
			label:       "Resize pane down",
			description: "Move the border of the conversation pane you are in down one step.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionResizePaneDown),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdResizePaneUp,
			label:       "Resize pane up",
			description: "Move the border of the conversation pane you are in up one step.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionResizePaneUp),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdResizePaneRight,
			label:       "Resize pane right",
			description: "Move the border of the conversation pane you are in right one step.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionResizePaneRight),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   conversationPaneReady,
		},
		{
			id:          CommandIdNewSessionWizard,
			label:       "Create session",
			description: "Choose the profile, project, target, and mounts in the full wizard.",
			scope:       ScopeSessions,
			paneKeys:    []KeyHint{keyHintPlain('n', "n"), keyHintPlain('N', "N")},
			action:      keyAction(core.KeyActionNewSession),
			footer:      footerWord("create"),
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   configPresent,
		},
		{
			id:          CommandIdRestartSession,
			label:       "Restart agent",
			description: "Restart the worker and harness inside the existing environment.",
			scope:       ScopeSession,
			// No pane key: a mis-hit must not restart a live session. Reachable
			// from the palette, the row menu, and an explicit `[keys]` binding.
			paneKeys:    nil,
			action:      keyAction(core.KeyActionRestartSession),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  2,
			available:   restartSessionAvailable,
		},
		{
			id:          CommandIdUpgradeContainer,
			label:       "Upgrade container and restart…",
			description: "Choose a new image while retaining workspace and build cache storage.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  2,
			available:   upgradeContainerAvailable,
		},
		{
			id:          CommandIdResumeDialog,
			label:       "Sessions",
			description: "Open every running session, in every workspace, on one list. The tabs to its right list the sessions that can be resumed, imported, or restored from the search index.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionResume),
			footer:      footerWord("sessions"),
			footerGroup: FooterGroupChord,
			footerRank:  1,
			available:   alwaysReady,
		},
		{
			id:          CommandIdMarkAllRead,
			label:       "Mark all read",
			description: "Clear the unread marker on every session in this workspace at once; questions, failures, and unreachable sessions stay flagged.",
			scope:       ScopeSessions,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionMarkAllRead),
			footer:      footerWord("read"),
			footerGroup: FooterGroupChord,
			footerRank:  2,
			available:   alwaysReady,
		},
		{
			id:          CommandIdFilterSessions,
			label:       "Search sessions",
			description: "Type to keep only the sessions whose name, project, profile, or target matches. The letters b, w, i, d, I, and a keep only blocked, working, idle, done, or not idle sessions, or all of them.",
			scope:       ScopeSessions,
			paneKeys:    []KeyHint{keyHintPlain('/', "/")},
			footer:      footerWord("search (filter a/b/w/i/d)"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdNextAttention,
			label:       "Next session needing you",
			description: "Jump to the session that most needs a person: a failure first, then a question, then unread activity, in any workspace.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionNextAttention),
			footer:      attentionFooter,
			footerGroup: FooterGroupChord,
			footerRank:  3,
			available:   alwaysReady,
		},
		{
			id:          CommandIdPreviousAttention,
			label:       "Previous session needing you",
			description: "Walk the sessions that need a person in the other direction.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionPreviousAttention),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  3,
			available:   alwaysReady,
		},
		{
			id:          CommandIdCancelOperation,
			label:       "Cancel operation",
			description: "Stop the launch, resume, or stop the selected session is in the middle of.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionCancelOperation),
			footer:      cancelFooter,
			footerGroup: FooterGroupChord,
			footerRank:  5,
			available:   operationInFlight,
		},
		{
			id:          CommandIdToggleProject,
			label:       "Fold project",
			description: "Space folds the selected session's project; 1 to 9 fold by number.",
			scope:       ScopeSessions,
			paneKeys:    []KeyHint{keyHintPlain(' ', "Space")},
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   selectedSessionReady,
		},
		{
			id:          CommandIdRenameSession,
			label:       "Rename session",
			description: "Give the selected session your own title.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionRenameSession),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   sessionIdle,
		},
		{
			id:          CommandIdChangedFiles,
			label:       "Changed files",
			description: "List the files the selected session's checkout has changed, with the branch and its distance from upstream.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionChangedFiles),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   liveSession,
		},
		{
			id:          CommandIdDismissNotification,
			label:       "Dismiss notification",
			description: "Dismiss this notification without sending input or granting approval.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   dismissNotificationAvailable,
		},
		{
			id:          CommandIdOpenWaitingQuestion,
			label:       "Open sub-agent question",
			description: "Go to the sub-agent whose question or permission request makes this session wait.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   openWaitingQuestionAvailable,
		},
		{
			id:          CommandIdIgnoreNotifications,
			label:       "Ignore notifications",
			description: "Toggle whether this session raises bell, desktop, and push notifications. Its attention state still renders.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   ignoreNotificationsAvailable,
		},
		{
			id:          CommandIdInterruptTurn,
			label:       "Interrupt turn",
			description: "Stop the agent's running turn, as Esc does in the composer. The session keeps running.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionInterruptTurn),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   interruptAvailable,
		},
		{
			id:          CommandIdInterruptAll,
			label:       "Interrupt all",
			description: "Stop the running turn of the selected session and of every sub-agent under it, after you confirm. The sessions keep running.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   interruptAllAvailable,
		},
		{
			id:          CommandIdSessionActions,
			label:       "Session actions…",
			description: "Open the selected session's actions menu, the same one the ⋯ on its row opens.",
			scope:       ScopeSession,
			paneKeys:    []KeyHint{keyHintPlain('.', ".")},
			action:      keyAction(core.KeyActionSessionActions),
			footer:      footerWord("actions"),
			footerGroup: FooterGroupPane,
			footerRank:  2,
			available:   selectedSessionReady,
		},
		{
			id:          CommandIdOpenSubagents,
			label:       "Sub-agents",
			description: "Open the sub-agents of this session in their own list. Esc or the X on the workspace strip returns to the parent.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionOpenSubagents),
			footer:      subagentsFooter,
			footerGroup: FooterGroupChord,
			footerRank:  2,
			available:   subagentsAvailable,
		},
		{
			id:          CommandIdContainerSettings,
			label:       "Container settings",
			description: "Edit CPU, memory, and mounts for the next time the container is created.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionContainerSettings),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   containerSession,
		},
		{
			id:          CommandIdChangeWorkspace,
			label:       "Change workspace…",
			description: "Move the selected session, and its sub-agents, to another workspace, after you confirm.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   changeWorkspaceAvailable,
		},
		{
			id:          CommandIdMoveSession,
			label:       "Move session…",
			description: "Restore the selected session on another profile and/or target.",
			scope:       ScopeSession,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionMoveSession),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   moveSessionAvailable,
		},
		{
			id:          CommandIdCopySessionId,
			label:       "Copy session ID",
			description: "Copy the selected session's full ID to the clipboard.",
			scope:       ScopeSession,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   selectedSessionReady,
		},
		{
			id:          CommandIdDestroySession,
			label:       "Destroy session…",
			description: "Permanently remove the selected session, its target, and its recovery archive.",
			scope:       ScopeSession,
			// No pane key: a mis-hit must not begin deleting a session.
			paneKeys:    nil,
			action:      keyAction(core.KeyActionDestroySession),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			// Deliberately available while an operation runs: preempting a wedged
			// one is what force destruction is for.
			available: selectedSessionReady,
		},
		{
			id:          CommandIdTargetActions,
			label:       "Target actions",
			description: "Test or rename the selected target.",
			scope:       ScopeTargets,
			paneKeys:    []KeyHint{keyHintPlain(tea.KeyEnter, "Enter"), keyHintPlain('e', "e")},
			footer:      footerWord("actions"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   targetsVisible,
		},
		{
			id:          CommandIdTargetDisks,
			label:       "Show disks",
			description: "Open the selected target's filesystems, free space, and status. Clicking the Disks cell does the same.",
			scope:       ScopeTargets,
			paneKeys:    []KeyHint{keyHintPlain('d', "d")},
			// The Disks cell shows it can be clicked; the key is listed in the
			// palette rather than taking footer room from global keys.
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   targetsVisible,
		},
		{
			id:          CommandIdToggleQuotaPool,
			label:       "Expand quota pool",
			description: "Expand or collapse the selected provider pool's next 24 hours.",
			scope:       ScopeQuota,
			paneKeys:    []KeyHint{keyHintPlain(tea.KeyEnter, "Enter")},
			footer:      footerWord("expand/collapse"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   profilesPresent,
		},
		{
			id:          CommandIdEditProfile,
			label:       "Rename profile",
			description: "Rename the selected profile's configuration id.",
			scope:       ScopeQuota,
			paneKeys:    []KeyHint{keyHintPlain('e', "e")},
			footer:      footerWord("rename profile"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   profilesPresent,
		},
		{
			id:          CommandIdManageProfiles,
			label:       "Manage agent profiles",
			description: "Add or edit agent accounts, harnesses, and environment settings.",
			scope:       ScopeSettings,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionManageProfiles),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdManageMachines,
			label:       "Manage machines",
			description: "Add SSH hosts or EC2 launch templates and edit their shared settings.",
			scope:       ScopeSettings,
			paneKeys:    nil,
			// Unbound by default like its siblings; `[keys] manage_machines`
			// binds it.
			action:      keyAction(core.KeyActionManageMachines),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdManageTargets,
			label:       "Manage runtimes",
			description: "Add a runtime (the engine or directory a target runs in) and choose the machine it runs on.",
			scope:       ScopeSettings,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionManageTargets),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdOpenConfig,
			label:       "Open settings",
			description: "Edit all configuration in the Settings modal.",
			scope:       ScopeSettings,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionOpenSettings),
			footer:      footerWord("settings"),
			footerGroup: FooterGroupChord,
			footerRank:  9,
			available:   alwaysReady,
		},
		{
			id:          CommandIdCycleFocus,
			label:       "Next pane",
			description: "Move the keyboard down the layout; Shift-Tab reverses it.",
			scope:       ScopePane,
			paneKeys:    []KeyHint{keyHintPlain(tea.KeyTab, "Tab")},
			action:      keyAction(core.KeyActionNextPane),
			footer:      focusCycleFooter,
			footerGroup: FooterGroupPane,
			footerRank:  1,
			available:   alwaysReady,
		},
		{
			id:          CommandIdCycleFocusReverse,
			label:       "Previous pane",
			description: "Move the keyboard up the layout.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionPreviousPane),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  1,
			available:   alwaysReady,
		},
		{
			id:          CommandIdCycleFocusedPaneSize,
			label:       "Pane size",
			description: "Cycle the focused pane through minimized, standard, and maximized.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionPaneSize),
			// The pane title carries clickable size controls in the place they
			// apply, so the footer spends its width on keys with no such affordance.
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  3,
			available:   supportPaneFocused,
		},
		// The Targets and Profiles titles are buttons that open these menus; `.`
		// is the same menu from the keyboard, as `.` is the row menu on Sessions.
		{
			id:          CommandIdTargetsMenu,
			label:       "Targets pane menu",
			description: "Open the menu under the Targets title: refresh targets and quotas, or open the runtime or machine settings. Clicking the title opens it too.",
			scope:       ScopeTargets,
			paneKeys:    []KeyHint{keyHintPlain('.', ".")},
			footer:      footerWord("menu"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdProfilesMenu,
			label:       "Profiles pane menu",
			description: "Open the menu under the Profiles title: refresh targets and quotas, or open the profile settings. Clicking the title opens it too.",
			scope:       ScopeQuota,
			paneKeys:    []KeyHint{keyHintPlain('.', ".")},
			footer:      footerWord("menu"),
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdTogglePanePreset,
			label:       "Pane preset",
			description: "Restore standard panes, or minimize all three for the conversation.",
			scope:       ScopePane,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionPanePreset),
			footer:      footerWord("panes"),
			footerGroup: FooterGroupChord,
			footerRank:  4,
			available:   alwaysReady,
		},
		{
			id:          CommandIdWorkspaces,
			label:       "Workspaces",
			description: "Open the workspace manager to add, rename, or delete a workspace.",
			scope:       ScopeGlobal,
			// Workspace management is opened by the pinned hamburger, its key, or
			// the command palette. It intentionally has no footer hint.
			paneKeys:    nil,
			action:      keyAction(core.KeyActionWorkspaceManager),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdFocusWorkspaces,
			label:       "Focus the workspace strip",
			description: "Put the keyboard on the workspace tabs.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionFocusWorkspaces),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdSelectWorkspacePrevious,
			label:       "Previous workspace",
			description: "Select the previous workspace tab.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionPreviousWorkspace),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdSelectWorkspaceNext,
			label:       "Next workspace",
			description: "Select the next workspace tab.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionNextWorkspace),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdSwitchWorkspace,
			label:       "Switch to workspace by number",
			description: "Select the first to ninth workspace tab directly.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionSwitchWorkspace),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdWebViewer,
			label:       "Web viewer",
			description: "Show the address and code for the browser and phone viewer.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionWebViewer),
			footer:      footerWord("web"),
			footerGroup: FooterGroupChord,
			footerRank:  7,
			available:   alwaysReady,
		},
		{
			id:          CommandIdRestartDaemon,
			label:       "Restart the Mjolnir daemon",
			description: "Stop the background daemon and start one from this build, then report which build came up.",
			scope:       ScopeGlobal,
			// Restarting the daemon is rare and disruptive, so it has no default
			// key; `[keys] restart_daemon` binds one. It is always offered: the
			// daemon being gone is exactly when it is needed, and that is also
			// when nothing can be asked.
			paneKeys:    nil,
			action:      keyAction(core.KeyActionRestartDaemon),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdRefresh,
			label:       "Refresh targets and quotas",
			description: "Re-probe every target's capacity and ask every profile for its quota again.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionRefresh),
			footer:      footerWord("refresh"),
			footerGroup: FooterGroupChord,
			footerRank:  8,
			available:   alwaysReady,
		},
		{
			id:          CommandIdSessionCpuReport,
			label:       "CPU by session",
			description: "Show live sessions grouped by machine, with each session's CPU share over about the last hour, highest first.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdSessionUsageReport,
			label:       "Usage by session",
			description: "Compare each parent session and its sub-agents by model and estimated API cost, highest total first; filter by parent profile and trailing hour, week or month.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdNoticeLog,
			label:       "Recent messages",
			description: "Show the last notices the footer reported, newest first, including failures that replaced each other.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionNoticeLog),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  0,
			available:   alwaysReady,
		},
		{
			id:          CommandIdQuitDetach,
			label:       "Detach from this terminal",
			description: "Leave this terminal client; the daemon and its sessions keep running.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionDetach),
			footer:      footerWord("detach"),
			footerGroup: FooterGroupChord,
			footerRank:  6,
			available:   alwaysReady,
		},
		{
			id:          CommandIdToggleTranscriptRendering,
			label:       "Toggle transcript rendering",
			description: "Switch the conversation between rendered Markdown and raw text.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionToggleTranscriptRendering),
			footer:      footerWord("rendering"),
			footerGroup: FooterGroupChord,
			footerRank:  10,
			available:   alwaysReady,
		},
		{
			id:          CommandIdToggleDictation,
			label:       "Dictation",
			description: "Start or stop dictating into the composer.",
			scope:       ScopeGlobal,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionToggleDictation),
			footer:      noFooter,
			footerGroup: FooterGroupChord,
			footerRank:  13,
			available:   alwaysReady,
		},
		{
			id:          CommandIdPalette,
			label:       "Command palette",
			description: "Search every command that applies right now and run one.",
			scope:       ScopeGlobal,
			paneKeys:    []KeyHint{keyHintPlain('?', "?")},
			action:      keyAction(core.KeyActionPalette),
			footer:      footerWord("palette"),
			footerGroup: FooterGroupChord,
			footerRank:  11,
			available:   alwaysReady,
		},
		{
			id:          CommandIdCycleSpinner,
			label:       "Next spinner style",
			description: "Cycle activity animations: scan, pulse, wave, bars, shimmer, globe.",
			scope:       ScopeSettings,
			paneKeys:    nil,
			action:      keyAction(core.KeyActionCycleSpinner),
			footer:      noFooter,
			footerGroup: FooterGroupPane,
			footerRank:  0,
			available:   spinnerAvailable,
		},
	}
}

// paletteHidden is the commands the palette omits because a visible control
// already runs them.
//
// Each of these is one click away on the dashboard itself, so listing them
// again in the palette only lengthens the search. The keyboard chords, footer
// hints, and onboarding buttons that run them are unaffected.
var paletteHidden = []CommandId{
	CommandIdPalette,         // already open when the list is drawn
	CommandIdSwitchWorkspace, // the numbered keys act on the visible tab strip
	CommandIdTargetsMenu,     // the Targets title opens it, and it lists three palette commands
	CommandIdProfilesMenu,    // the Profiles title opens it, and it lists two palette commands
}

// recentCommands is how many commands the palette remembers under its Recent
// heading.
const recentCommands = 5

// hiddenFromPalette is whether paletteEntries (palette) skips id.
func hiddenFromPalette(id CommandId) bool {
	for _, hidden := range paletteHidden {
		if hidden == id {
			return true
		}
	}
	return false
}

// spec is the specification for one command. Panics only if commands has
// lost an entry, which a unit test in this module rules out.
func spec(id CommandId) *CommandSpec {
	for i := range commands {
		if commands[i].id == id {
			return &commands[i]
		}
	}
	panic("every CommandId has one entry in COMMANDS")
}

func notificationAvailable(dashboard *DashboardState) Availability {
	if len(dashboard.notificationEntries()) == 0 {
		return AvailabilityHidden{}
	}
	return AvailabilityReady{}
}

func focusCycleFooter(dashboard *DashboardState) (string, bool) {
	if dashboard.NotificationFormOwnsKeyboard() {
		return "", false
	}
	return "pane", true
}

func notificationReplyAvailable(dashboard *DashboardState) Availability {
	if _, ok := dashboard.NotificationFormSession(); ok {
		return AvailabilityHidden{}
	}
	return notificationAvailable(dashboard)
}

func notificationRestoreAvailable(dashboard *DashboardState) Availability {
	if _, ok := dashboard.NotificationFormSession(); !ok && dashboard.NotificationHasRecoverableReply() {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

func notificationNavigationAvailable(dashboard *DashboardState, command CommandId) Availability {
	if dashboard.canNavigateNotification(command) {
		return AvailabilityReady{}
	}
	return AvailabilityHidden{}
}

func notificationClearAvailable(dashboard *DashboardState) Availability {
	entries := dashboard.notificationEntries()
	switch {
	case len(entries) == 0:
		return AvailabilityHidden{}
	case entries[0].target != nil:
		return AvailabilityReady{}
	default:
		return AvailabilityBlocked{Reason: "Answer this request in the session."}
	}
}

// scopeApplies is whether a command in scope can be run while focus owns the
// keyboard.
//
// ScopeSetup is deliberately excluded from key matching (see
// [paneCommandForKey]); it is listed here so the footer can offer it while
// the configuration is still empty.
func scopeApplies(scope Scope, focus Focus) bool {
	switch scope {
	case ScopeGlobal, ScopeSettings, ScopePane, ScopeSetup:
		return true
	case ScopeSessions, ScopeSession:
		return focus == FocusSessions
	case ScopeTargets:
		return focus == FocusTargets
	case ScopeQuota:
		return focus == FocusQuota
	case ScopeNotifications:
		return focus == FocusNotifications
	}
	return false
}

// paneCommandForKey is the command a pane key press runs, or false if the
// panes do not answer it.
//
// The "plain rule": a bare character only counts when no accelerator and no
// Alt is held, because the panes are the one place where a letter cannot be
// mistaken for text. Characters are ignored entirely while the composer has
// focus, which keeps the old behaviour where the panes answered nothing
// there.
//
// ScopeSetup never matches here. Its key is `e`, which the Sessions,
// Targets, and Quota panes also use; the caller resolves that ambiguity by
// checking for an empty configuration first, exactly as the surface always
// has.
func paneCommandForKey(key tea.KeyPressMsg, focus Focus) (CommandId, bool) {
	pressed := readPaneKey(key)
	plain := pressed.modifiers&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) == 0
	for i := range commands {
		spec := &commands[i]
		if spec.scope == ScopeSetup || !scopeApplies(spec.scope, focus) {
			continue
		}
		for _, hint := range spec.paneKeys {
			if hint.acceptsFocus(focus) && hint.matches(pressed, plain) {
				return spec.id, true
			}
		}
	}
	return 0, false
}

// available is the commands that are ready to run right now, in registry
// order.
//
// A non-nil scopeFilter asks for one group; nil asks for everything that
// applies where the keyboard currently is, which is what the footer wants.
func available(dashboard *DashboardState, scopeFilter *Scope) []CommandId {
	ids := []CommandId{}
	for i := range commands {
		spec := &commands[i]
		if scopeFilter != nil {
			if spec.scope != *scopeFilter {
				continue
			}
		} else if !scopeApplies(spec.scope, dashboard.focus) {
			continue
		}
		if nativeAgentSessionCommand(spec.id) {
			if id, ok := dashboard.commandSessionID(); ok && dashboard.IsNativeAgent(id) {
				continue
			}
		}
		if spec.available(dashboard) == (AvailabilityReady{}) {
			ids = append(ids, spec.id)
		}
	}
	return ids
}

// nativeAgentSessionCommand is whether available leaves id out while a
// native agent is the command session: these act on a Mjolnir session's
// record, which a native agent's parent owns.
func nativeAgentSessionCommand(id CommandId) bool {
	switch id {
	case CommandIdRestartSession, CommandIdRenameSession, CommandIdContainerSettings,
		CommandIdChangedFiles, CommandIdDismissNotification, CommandIdMoveSession,
		CommandIdChangeWorkspace, CommandIdDestroySession:
		return true
	}
	return false
}

// CommandAllowedNow is whether a bound command still answers with the current
// dialog open.
//
// Detach always answers from every surface, and the pane preset only changes
// the layout underneath, so both survive a modal.
func (d *DashboardState) CommandAllowedNow(id CommandId) bool {
	switch id {
	// Refreshing is harmless over a modal: it asks the daemon for fresh
	// capacity and quota figures and changes nothing on screen the dialog
	// owns.
	case CommandIdQuitDetach, CommandIdTogglePanePreset, CommandIdRefresh:
		return true
	// Cancel is allowed through exactly one modal: the target-actions
	// dialog, where it cancels the test that dialog is running.
	case CommandIdCancelOperation:
		return d.targetTestRunning() || !d.ModalOpen()
	case CommandIdCycleFocusedPaneSize:
		return !d.ModalOpen()
	// Moving the keyboard or the workspace out from under an open dialog
	// would act on a surface the user cannot see.
	case CommandIdCycleFocus, CommandIdCycleFocusReverse, CommandIdFocusWorkspaces,
		CommandIdSelectWorkspacePrevious, CommandIdSelectWorkspaceNext, CommandIdSwitchWorkspace:
		return !d.ModalOpen()
	default:
		return !d.ModalOpen()
	}
}

// DispatchCommand runs one registry command. Every arm calls the same entry
// point the key handler used to call directly, so the footer, the palette,
// and the keyboard cannot disagree about what a command does.
func (d *DashboardState) DispatchCommand(id CommandId) DashboardAction {
	saved := d.commandSessionOverride
	// From the composer a session command acts on the conversation it
	// writes to. With the pane still empty (a launch whose attach has not
	// finished) there is no such conversation, so the command acts on the
	// session selected in Sessions, as it would from there.
	if spec(id).scope == ScopeSession && saved == nil && d.focus == FocusPrompt {
		if session, ok := d.CurrentSessionID(); ok {
			d.commandSessionOverride = &session
		}
	}
	action := d.dispatchCommandInner(id)
	d.commandSessionOverride = saved
	return action
}

// rememberCommand puts a command at the head of the palette's Recent group.
func (d *DashboardState) rememberCommand(id CommandId) {
	recent := make([]CommandId, 0, len(d.recentCommands)+1)
	recent = append(recent, id)
	for _, command := range d.recentCommands {
		if command != id {
			recent = append(recent, command)
		}
	}
	if len(recent) > recentCommands {
		recent = recent[:recentCommands]
	}
	d.recentCommands = recent
}

func (d *DashboardState) dispatchCommandInner(id CommandId) DashboardAction {
	switch id {
	case CommandIdResizePaneLeft, CommandIdResizePaneDown, CommandIdResizePaneUp, CommandIdResizePaneRight:
	default:
		d.resizeMode = false
	}
	if selected, ok := d.commandSessionID(); ok && d.IsNativeAgent(selected) {
		switch id {
		case CommandIdRestartSession, CommandIdRenameSession, CommandIdContainerSettings,
			CommandIdChangedFiles, CommandIdDismissNotification, CommandIdMoveSession,
			CommandIdChangeWorkspace, CommandIdSuspendSession, CommandIdDestroySession:
			d.SetNotice("Native agents are owned by their parent session")
			return DashboardActionNone{}
		}
	}
	// Commands a person reaches for by name are worth remembering; the pane
	// keys and the palette itself are not. The palette records whatever it
	// runs itself, pane keys included.
	if len(spec(id).paneKeys) == 0 && id != CommandIdPalette {
		d.rememberCommand(id)
	}
	switch id {
	case CommandIdSuspendSession, CommandIdRestartSession, CommandIdMoveSession,
		CommandIdChangeWorkspace, CommandIdContainerSettings, CommandIdGoAfk:
		switch availability := spec(id).available(d).(type) {
		case AvailabilityHidden:
			return DashboardActionNone{}
		case AvailabilityBlocked:
			d.SetNotice(availability.Reason)
			return DashboardActionNone{}
		}
	}
	switch id {
	case CommandIdPinSession:
		if sessionID, ok := d.commandSessionID(); ok {
			if _, pinned := d.PinID(sessionID); pinned {
				return DashboardActionUnpinSession{SessionID: sessionID}
			}
			d.beginPinMenu(sessionID)
		}
		return DashboardActionNone{}
	case CommandIdOpenSession:
		return d.openSelectedSession()
	case CommandIdOpenSessionSplitRight:
		return d.splitCommand(chat.DirectionHorizontal)
	case CommandIdOpenSessionSplitBelow:
		return d.splitCommand(chat.DirectionVertical)
	case CommandIdClosePane:
		return DashboardActionClosePane{Pane: d.FocusedPane()}
	case CommandIdFocusPaneLeft:
		return d.focusPaneCommand(NavDirectionLeft)
	case CommandIdFocusPaneDown:
		return d.focusPaneCommand(NavDirectionDown)
	case CommandIdFocusPaneUp:
		return d.focusPaneCommand(NavDirectionUp)
	case CommandIdFocusPaneRight:
		return d.focusPaneCommand(NavDirectionRight)
	case CommandIdFocusLastPane:
		return d.focusLastPaneCommand()
	case CommandIdZoomPane:
		return d.zoomPaneCommand()
	case CommandIdResizeMode:
		return d.beginResizeMode()
	case CommandIdSwapPaneLeft:
		return d.swapPaneCommand(NavDirectionLeft)
	case CommandIdSwapPaneDown:
		return d.swapPaneCommand(NavDirectionDown)
	case CommandIdSwapPaneUp:
		return d.swapPaneCommand(NavDirectionUp)
	case CommandIdSwapPaneRight:
		return d.swapPaneCommand(NavDirectionRight)
	case CommandIdRenameWorkspace:
		return d.beginWorkspaceCommand(false)
	case CommandIdCloseWorkspace:
		return d.beginWorkspaceCommand(true)
	case CommandIdSuspendSession:
		if session := d.selectedSession(); session != nil {
			sessionID := session.ID
			childrenNotHandedBack := d.subagentsNotHandedBack(sessionID)
			interrupting := d.hasActiveWork(sessionID)
			dialog := newConfirmDialog(ConfirmationSuspendSession{
				SessionID:             sessionID,
				ChildrenNotHandedBack: childrenNotHandedBack,
				Interrupting:          interrupting,
				Preflight:             nil,
				Checking:              true,
				ViewingAllUntracked:   false,
			}).namingSession(sessionName(session))
			d.mode = ModeConfirm{&dialog}
			return DashboardActionSuspendPreflight{SessionID: sessionID}
		}
		return DashboardActionNone{}
	case CommandIdResizePaneLeft:
		return d.resizePaneCommand(NavDirectionLeft)
	case CommandIdResizePaneDown:
		return d.resizePaneCommand(NavDirectionDown)
	case CommandIdResizePaneUp:
		return d.resizePaneCommand(NavDirectionUp)
	case CommandIdResizePaneRight:
		return d.resizePaneCommand(NavDirectionRight)
	case CommandIdNewSessionWizard:
		return d.beginNew()
	case CommandIdChangeGoSetup:
		return d.changeGoSetup()
	case CommandIdUpgradeContainer:
		session := d.selectedSession()
		if session == nil {
			return DashboardActionNone{}
		}
		return DashboardActionPreviewContainerUpgrade{SessionID: session.ID, UpgradeOnly: true}
	case CommandIdRestartSession:
		session := d.selectedSession()
		if session == nil {
			return DashboardActionNone{}
		}
		sessionID, name := session.ID, sessionName(session)
		if _, upgrade := d.containerUpgradeSessions[sessionID]; upgrade {
			return DashboardActionPreviewContainerUpgrade{SessionID: sessionID, UpgradeOnly: false}
		}
		// Mid-turn work is lost by a restart, so that case asks first; an
		// idle session restarts at once.
		if d.hasActiveWork(sessionID) {
			dialog := newConfirmDialog(ConfirmationInterruptWork{
				SessionID: sessionID,
				Restart:   true,
			}).namingSession(name)
			d.mode = ModeConfirm{&dialog}
			return DashboardActionNone{}
		}
		return DashboardActionRestartSession{SessionID: sessionID, AcknowledgeContainerRecreation: false}
	case CommandIdResumeDialog:
		return DashboardActionOpenResumeDialog{}
	case CommandIdPalette:
		d.beginPalette()
		return DashboardActionNone{}
	case CommandIdCycleSpinner:
		if d.spinnerSavePending {
			return DashboardActionNone{}
		}
		style := d.config.Get().Spinner.Next()
		d.spinnerSavePending = true
		d.SetNotice(fmt.Sprintf("Saving %s spinner…", style))
		return DashboardActionSaveSpinnerStyle{Style: style}
	case CommandIdRenameSession:
		d.beginRename()
		return DashboardActionNone{}
	case CommandIdContainerSettings:
		d.beginContainerEdit()
		return DashboardActionNone{}
	case CommandIdChangeWorkspace:
		d.beginChangeWorkspace()
		return DashboardActionNone{}
	case CommandIdChangedFiles:
		return d.beginChangedFiles()
	case CommandIdDismissNotification:
		switch availability := dismissNotificationAvailable(d).(type) {
		case AvailabilityReady:
			return d.beginDismissNotification()
		case AvailabilityBlocked:
			d.SetNotice(sentence(availability.Reason))
		}
		return DashboardActionNone{}
	case CommandIdOpenWaitingQuestion:
		if session, ok := d.commandSessionID(); ok {
			if child, waiting := d.waitingChildOf(session); waiting {
				// The menu's session was the parent; from here the command
				// moves the selection, and Open must read it.
				d.commandSessionOverride = nil
				return d.focusSessionFromCPUReport(child)
			}
		}
		return DashboardActionNone{}
	case CommandIdNotificationOpen, CommandIdNotificationSend, CommandIdNotificationRestore,
		CommandIdNotificationDefault, CommandIdNotificationPreviousMessage, CommandIdNotificationNextMessage:
		return d.notificationAction(id)
	case CommandIdNotificationClear:
		return d.notificationClear()
	case CommandIdNotificationSkip:
		return d.notificationSkip()
	case CommandIdNotificationIgnoreSession:
		return d.notificationAction(CommandIdNotificationIgnoreSession)
	case CommandIdNotificationScrollUp:
		d.scrollNotification(-1)
		return DashboardActionNone{}
	case CommandIdNotificationScrollDown:
		d.scrollNotification(1)
		return DashboardActionNone{}
	case CommandIdIgnoreNotifications:
		session := d.commandSession()
		if session == nil {
			return DashboardActionNone{}
		}
		return DashboardActionSetSessionNotificationsIgnored{SessionID: session.ID, Ignored: !session.NotificationsIgnored}
	case CommandIdInterruptTurn:
		switch availability := interruptAvailable(d).(type) {
		case AvailabilityReady:
			if sessionID, ok := d.commandSessionID(); ok {
				return DashboardActionInterruptTurn{SessionID: sessionID}
			}
		case AvailabilityBlocked:
			d.SetNotice(sentence(availability.Reason))
		}
		return DashboardActionNone{}
	case CommandIdInterruptAll:
		switch availability := interruptAllAvailable(d).(type) {
		case AvailabilityReady:
			if session := d.commandSession(); session != nil {
				sessionID := session.ID
				name := sessionName(session)
				verdict := d.verdict(sessionID)
				parentRunning := verdict.InterruptMember
				subagentsRunning := verdict.InterruptCount
				if parentRunning {
					subagentsRunning = max(subagentsRunning-1, 0)
				}
				dialog := newConfirmDialog(ConfirmationInterruptAll{
					SubagentsRunning: subagentsRunning,
					ParentRunning:    parentRunning,
					SessionID:        sessionID,
				}).namingSession(name)
				d.mode = ModeConfirm{&dialog}
			}
		case AvailabilityBlocked:
			d.SetNotice(sentence(availability.Reason))
		}
		return DashboardActionNone{}
	case CommandIdSessionActions:
		if _, ok := d.commandSessionID(); ok {
			d.beginSessionPalette()
		}
		return DashboardActionNone{}
	case CommandIdOpenSubagents:
		switch availability := subagentsAvailable(d).(type) {
		case AvailabilityReady:
			if parentID, ok := d.commandSessionID(); ok {
				return DashboardActionOpenSubagents{ParentID: parentID}
			}
		case AvailabilityBlocked:
			d.SetNotice(sentence(availability.Reason))
		}
		return DashboardActionNone{}
	case CommandIdMoveSession:
		return d.beginMove()
	case CommandIdCopySessionId:
		session := d.commandSession()
		if session == nil {
			return DashboardActionNone{}
		}
		return DashboardActionCopySessionId{SessionID: session.ID}
	case CommandIdDestroySession:
		session := d.selectedSession()
		if session == nil {
			return DashboardActionNone{}
		}
		dialog := newConfirmDialog(ConfirmationForceDestroy{SessionID: session.ID}).
			namingSession(sessionName(session))
		d.mode = ModeConfirm{&dialog}
		return DashboardActionNone{}
	case CommandIdGoAfk:
		return DashboardActionSetOperatorAway{Away: true}
	case CommandIdMarkAllRead:
		return d.markAllRead()
	case CommandIdFilterSessions:
		d.beginSessionsFilter()
		return DashboardActionNone{}
	case CommandIdNextAttention:
		return d.stepAttention(1)
	case CommandIdPreviousAttention:
		return d.stepAttention(-1)
	case CommandIdCancelOperation:
		// The target-actions dialog's running test is the one thing cancel
		// reaches through a modal.
		if action := d.cancelTargetTest(); action != nil {
			return action
		}
		if session := d.selectedSession(); session != nil {
			if kind, ok := d.SessionOperationKind(session.ID); ok {
				return DashboardActionCancelOperation{SessionID: session.ID, Kind: kind}
			}
		}
		// A key that does nothing silently reads as broken.
		d.SetNotice("Nothing to cancel")
		return DashboardActionNone{}
	case CommandIdToggleProject:
		d.toggleSelectedProject()
		return DashboardActionNone{}
	case CommandIdTargetActions:
		if d.PaneSize(SupportPaneTargets) == core.PaneSizeMinimized {
			return DashboardActionNone{}
		}
		d.beginTargetActions()
		return DashboardActionNone{}
	case CommandIdTargetDisks:
		if d.PaneSize(SupportPaneTargets) == core.PaneSizeMinimized {
			return DashboardActionNone{}
		}
		d.beginCapacityDisksMenu(d.capacityIndex)
		return DashboardActionNone{}
	case CommandIdToggleQuotaPool:
		if d.PaneSize(SupportPaneQuota) != core.PaneSizeMinimized {
			d.toggleQuotaPool()
		}
		return DashboardActionNone{}
	case CommandIdEditProfile:
		if d.PaneSize(SupportPaneQuota) == core.PaneSizeMinimized {
			return DashboardActionNone{}
		}
		d.beginProfileRename()
		return DashboardActionNone{}
	case CommandIdRefresh:
		d.quotaProviders.requestRefresh()
		// Refreshing checks again what is available. It is allowed through
		// an open modal (see CommandAllowedNow), so an open picker gets fresh
		// answers.
		clear(d.projectDirectoryChecks)
		d.wantAvailabilityCheck()
		return DashboardActionRefreshAll{}
	case CommandIdManageProfiles:
		d.beginSettingsSection("profiles", nil)
		return DashboardActionNone{}
	case CommandIdManageMachines:
		d.beginSettingsSection("machines", nil)
		return DashboardActionNone{}
	case CommandIdManageTargets:
		d.beginSettingsSection("targets", nil)
		return DashboardActionNone{}
	case CommandIdOpenConfig:
		d.BeginSetup()
		return DashboardActionNone{}
	case CommandIdCycleFocus:
		d.CycleFocus(false)
		return DashboardActionNone{}
	case CommandIdCycleFocusReverse:
		d.CycleFocus(true)
		return DashboardActionNone{}
	case CommandIdCycleFocusedPaneSize:
		d.CycleFocusedPaneSize()
		return DashboardActionNone{}
	case CommandIdTargetsMenu:
		d.beginSupportPaneMenu(SupportPaneTargets)
		return DashboardActionNone{}
	case CommandIdProfilesMenu:
		d.beginSupportPaneMenu(SupportPaneQuota)
		return DashboardActionNone{}
	case CommandIdTogglePanePreset:
		d.TogglePanePreset()
		return DashboardActionNone{}
	case CommandIdWorkspaces:
		return d.BeginWorkspaceManager()
	case CommandIdFocusWorkspaces:
		d.focus = FocusWorkspaces
		d.workspaceControlFocus = WorkspaceControlFocusTabs
		d.setSessionActionFocus(nil)
		return DashboardActionNone{}
	case CommandIdSwitchWorkspace:
		// The numbered workspace keys carry an index, and a registry command
		// carries no argument, so the router calls selectWorkspaceIndex
		// directly.
		return DashboardActionNone{}
	case CommandIdToggleTranscriptRendering:
		return DashboardActionToggleTranscriptRendering{}
	case CommandIdToggleDictation:
		return DashboardActionToggleDictation{}
	case CommandIdSelectWorkspacePrevious:
		return d.selectAdjacentWorkspace(-1)
	case CommandIdSelectWorkspaceNext:
		return d.selectAdjacentWorkspace(1)
	case CommandIdWebViewer:
		return d.OpenWebDialog()
	case CommandIdRestartDaemon:
		return DashboardActionRestartDaemon{}
	case CommandIdSessionCpuReport:
		dialog := newSessionCpuReportDialog()
		d.mode = ModeSessionCpuReport{&dialog}
		return DashboardActionNone{}
	case CommandIdSessionUsageReport:
		d.beginSessionUsageReport(nil)
		return DashboardActionNone{}
	case CommandIdNoticeLog:
		d.beginNoticeLog()
		return DashboardActionNone{}
	case CommandIdQuitDetach:
		return DashboardActionQuitDetach{}
	}
	panic(fmt.Sprintf("unknown CommandId %v", id))
}

// DispatchCommandResult runs one command. A command always consumes its
// chord, whether or not it also hands the caller an action to execute.
func (d *DashboardState) DispatchCommandResult(id CommandId) chat.EventResult[DashboardAction] {
	action := d.DispatchCommand(id)
	result := chat.EventResult[DashboardAction]{Consumed: true}
	if _, none := action.(DashboardActionNone); !none {
		result.Action = action
	}
	return result
}

// sentence presents an availability reason, a fragment for notices and menu
// descriptions, as a complete sentence in those command surfaces.
func sentence(reason string) string {
	first, size := utf8.DecodeRuneInString(reason)
	if size == 0 {
		return ""
	}
	text := strings.ToUpper(string(first)) + reason[size:]
	if !strings.HasSuffix(text, ".") && !strings.HasSuffix(text, "!") && !strings.HasSuffix(text, "?") {
		text += "."
	}
	return text
}

func upgradeContainerAvailable(dashboard *DashboardState) Availability {
	if session := dashboard.commandSession(); session != nil {
		if _, upgrade := dashboard.containerUpgradeSessions[session.ID]; upgrade {
			return restartSessionAvailable(dashboard)
		}
	}
	return AvailabilityHidden{}
}
