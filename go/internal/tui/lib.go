// Port of mj-tui/src/lib.rs.

// Package tui is the state and input handling for Hel's combined terminal
// surface, the Go port of the mj-tui crate.
//
// One screen holds the Sessions pane, the conversation, the Prompt composer,
// and the Targets and Quota summaries; RenderCombined draws it. This package
// owns what that surface knows and what a key press means to it.
//
// It deliberately has no provisioning or persistence side effects. Input is
// reduced to DashboardAction values for the controller to run.
//
// The crate's modules are files of this one package, named as the port brief
// describes (wizards/picker.rs is wizards_picker.go). The terminal surfaces
// draw with Charm's libraries in place of ratatui and crossterm: Bubble Tea v2
// messages stand for terminal events (tea.Msg for crossterm's Event,
// tea.KeyMsg for KeyEvent, tea.MouseMsg for MouseEvent), an ultraviolet cell
// buffer for ratatui's Buffer and Rect (uv.Rectangle; terminal coordinates are
// int), and the widget, style and layout helpers of package chat for
// ratatui's widgets.
//
// Porting conventions shared by every file of this package:
//
//   - A Rust enum with data is a sealed interface whose variants are value
//     structs (DashboardActionOpen{SessionID: id}). Mode's variants embed a
//     pointer to the open dialog (ModeNew{NewWizard: wizard}), so a type
//     switch can edit the dialog in place.
//   - Rust tuples are client.DaemonPair values, except terminal coordinates,
//     which are uv.Position, and the few named structs declared beside the
//     fields that hold them.
//   - BTreeSet<String> is map[string]struct{}; BTreeMap<String, T> is
//     map[string]T, sorted where iteration order matters.
//   - RefCell and Cell fields are plain fields: Go has no borrow checker.
//   - A DashboardState field whose Go name would equal a method's Go name
//     takes the suffix Field (sessionFactsField behind sessionFacts()).
//   - Golden files copied unchanged from mj-tui/tests/golden live under
//     testdata/tests/golden, fixtures under testdata/tests/fixtures.
package tui

import (
	"reflect"
	"runtime"
	"slices"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// noColorOverrideForTests is the Rust test build's thread-local
// NO_COLOR_OVERRIDE_FOR_TESTS: golden renders pin it so they do not depend on
// the host's NO_COLOR. Nil uses the host value.
var noColorOverrideForTests *bool

// noColorRequested uses the host value in production and a scoped fixed value
// in golden renders.
func noColorRequested() bool {
	if override := noColorOverrideForTests; override != nil {
		return *override
	}
	return chat.NoColorRequested()
}

// withNoColorOverrideForTest runs run with noColorRequested pinned to
// noColor, restoring the previous override afterwards.
func withNoColorOverrideForTest[R any](noColor bool, run func() R) R {
	previous := noColorOverrideForTests
	noColorOverrideForTests = &noColor
	defer func() { noColorOverrideForTests = previous }()
	return run()
}

// pinNoColorOverrideForTest pins noColorRequested to noColor for the rest of
// the test binary's run.
func pinNoColorOverrideForTest(noColor bool) {
	noColorOverrideForTests = &noColor
}

// Re-exports of the crate root (pub use) from other crates. The crate's own
// re-exports (CommandId, RenderCombined, ...) are package-level names already.
type (
	PaneSize           = core.PaneSize
	PaneSizes          = core.PaneSizes
	WebListenerProcess = client.WebListenerProcess
	WebViewerAccess    = client.WebViewerAccess
	WebViewerRecovery  = client.WebViewerRecovery
)

// SessionsRow is one drawn row of the Sessions pane.
type SessionsRow interface {
	isSessionsRow()
}

// SessionsRowProjectHeading is a project name with its 1-9 toggle number.
// Focused mode only.
type SessionsRowProjectHeading struct {
	Key   string
	Label string
	// Number is nil when the project has no toggle number.
	Number *int
}

// SessionsRowSession is a live session, by index into orderedSessions().
// Expanded picks the four-row form over the one-line form.
type SessionsRowSession struct {
	Index    int
	Expanded bool
}

func (SessionsRowProjectHeading) isSessionsRow() {}
func (SessionsRowSession) isSessionsRow()        {}

// SessionsFilter is the Sessions pane's filter: free text and an optional
// attention state.
//
// Editing means typed characters go into Query; otherwise the pane's plain
// keys keep their meaning and the filter merely stays in force.
type SessionsFilter struct {
	Query chat.TextInput
	// State is nil for every attention state.
	State   *SessionStateFilter
	Editing bool
}

// SessionStateFilter is an attention state the Sessions pane can be narrowed
// to, with the letters that pick them (the same letters as herdr's
// navigator).
type SessionStateFilter uint8

const (
	// SessionStateFilterBlocked is `b`: waiting for input or failed.
	SessionStateFilterBlocked SessionStateFilter = iota
	// SessionStateFilterWorking is `w`: a turn or background work in
	// progress.
	SessionStateFilterWorking
	// SessionStateFilterIdle is `i`: idle with nothing unread.
	SessionStateFilterIdle
	// SessionStateFilterDone is `d`: finished with an unread answer.
	SessionStateFilterDone
	// SessionStateFilterNotIdle is `I`: anything other than idle with
	// nothing unread.
	SessionStateFilterNotIdle
)

// sessionStateFilterFromLetter is the filter a letter picks. ok is false for
// a letter that picks nothing; filter is nil for `a`, every state.
func sessionStateFilterFromLetter(letter rune) (filter *SessionStateFilter, ok bool) {
	var picked SessionStateFilter
	switch letter {
	case 'a':
		return nil, true
	case 'b':
		picked = SessionStateFilterBlocked
	case 'w':
		picked = SessionStateFilterWorking
	case 'i':
		picked = SessionStateFilterIdle
	case 'd':
		picked = SessionStateFilterDone
	case 'I':
		picked = SessionStateFilterNotIdle
	default:
		return nil, false
	}
	return &picked, true
}

func (f SessionStateFilter) label() string {
	switch f {
	case SessionStateFilterBlocked:
		return "blocked"
	case SessionStateFilterWorking:
		return "working"
	case SessionStateFilterIdle:
		return "idle"
	case SessionStateFilterDone:
		return "done"
	default:
		return "not idle"
	}
}

func (f SessionStateFilter) admits(level AttentionLevel) bool {
	switch f {
	case SessionStateFilterBlocked:
		return level == AttentionLevelWaiting || level == AttentionLevelUnreachable ||
			level == AttentionLevelFailed
	case SessionStateFilterWorking:
		return level == AttentionLevelWorking
	case SessionStateFilterIdle:
		return level == AttentionLevelIdle || level == AttentionLevelInactive
	case SessionStateFilterDone:
		return level == AttentionLevelUnread
	default:
		return !SessionStateFilterIdle.admits(level)
	}
}

// dashboardPaneCount is the full-height session sidebar, targets, and quotas.
const dashboardPaneCount = 3

// doubleClickInterval is the maximum gap between two left clicks on the same
// session row for the pair to count as a double click.
const doubleClickInterval = 500 * time.Millisecond

// testVersionLabel stands in for the build on the workspace pane in tests and
// the committed documentation screenshots, so goldens and captures do not
// change with every release and nobody reads them as a claim about the
// running version.
const testVersionLabel = "vX.Y.Z"

// CarriedLaunchSettings are settings a launch copies from an existing session
// because the wizard has no step for them: "New session with these settings"
// sets them, and every wizard launch leaves them at their defaults.
type CarriedLaunchSettings struct {
	NoProjectMemory     bool
	NoGithubEvents      bool
	NoGithubCredentials bool
	// Note is what the copy changed because a setting could not apply now,
	// in a sentence the launch notices show. Nil when nothing changed.
	Note *string
}

// Annotate is notice followed by the note, when the copy changed something.
func (c CarriedLaunchSettings) Annotate(notice string) string {
	if c.Note != nil {
		return notice + " " + *c.Note
	}
	return notice
}

// DashboardActionAnnotateLaunchNotice is Rust's
// DashboardAction::annotate_launch_notice: notice for a launch, followed by
// what a copied launch changed. Any other action leaves it as it is.
func DashboardActionAnnotateLaunchNotice(action DashboardAction, notice string) string {
	if create, ok := action.(DashboardActionCreateSession); ok {
		return create.Carried.Annotate(notice)
	}
	return notice
}

// NotificationCard is the daemon-published identity of the card on which an
// action began.
type NotificationCard struct {
	SessionID string
	// Target is nil when the card has no dismissal target.
	Target core.NotificationDismissal
}

// DashboardAction is the work one input asks the controller to run.
type DashboardAction interface {
	isDashboardAction()
}

type DashboardActionSetOperatorAway struct {
	Away bool
}

type DashboardActionNone struct{}

// DashboardActionCheckGitIdentity reads the host's global Git author identity
// before the first new session; the result goes to
// DashboardState.ApplyGitIdentityCheck.
type DashboardActionCheckGitIdentity struct{}

// DashboardActionSaveGitIdentity writes the host's global Git author
// identity; the result goes to DashboardState.ApplyGitIdentitySave.
type DashboardActionSaveGitIdentity struct {
	Name  string
	Email string
}

type DashboardActionGoLaunch struct {
	Recipe core.GoRecipe
}

type DashboardActionGoPrepareProject struct {
	TargetID string
}

type DashboardActionOpen struct {
	SessionID string
}

type DashboardActionNotificationReply struct {
	Card      NotificationCard
	SessionID string
	Prompt    chat.StartupPrompt
}

type DashboardActionNotificationSkip struct {
	Card NotificationCard
}

// DashboardActionPinSession opens a session in a new conversation pane beside
// or under the focused one. Horizontal puts the new pane to the right,
// Vertical below.
type DashboardActionPinSession struct {
	SessionID string
	Pane      PaneId
}

type DashboardActionUnpinSession struct {
	SessionID string
}

type DashboardActionSplitConversation struct {
	Pane      PaneId
	Direction chat.Direction
}

type DashboardActionOpenSessionInSplit struct {
	SessionID string
	Direction chat.Direction
}

// DashboardActionSplitPane splits the focused conversation pane and leaves
// the new pane empty. The keyboard split falls back to this when no session
// is selected.
type DashboardActionSplitPane struct {
	Direction chat.Direction
}

// DashboardActionClosePane removes one conversation pane, saving what it
// held.
type DashboardActionClosePane struct {
	Pane PaneId
}

// DashboardActionConversationPanesChanged reports that the conversation
// panes were focused or resized. The controller saves the arrangement, and a
// focus move also re-reads which conversation the keyboard is now in.
type DashboardActionConversationPanesChanged struct {
	FocusMoved bool
}

type DashboardActionPreviewContainerUpgrade struct {
	SessionID   string
	UpgradeOnly bool
}

type DashboardActionUpgradeContainer struct {
	Request core.ContainerUpgradeRequest
}

type DashboardActionRestartSession struct {
	SessionID                      string
	AcknowledgeContainerRecreation bool
}

// DashboardActionWarmProfileCapabilities starts shared background hydration
// for unsaved profile definitions.
type DashboardActionWarmProfileCapabilities struct {
	Key    string
	Config string
}

// DashboardActionProbeGitStatus reads the session checkout's branch and
// changed files on its target. The host runs it off the loop and answers
// with SetGitStatus.
type DashboardActionProbeGitStatus struct {
	SessionID string
}

// DashboardActionQueueStartupPrompt is a prompt typed into a standby
// composer while its session was still starting. The host asks the daemon to
// deliver it once the session is live; the dashboard only shows it as a
// queued preview.
type DashboardActionQueueStartupPrompt struct {
	SessionID string
	Prompt    chat.StartupPrompt
}

// DashboardActionWithdrawStartupPrompt reports that the person recalled a
// queued startup prompt into the standby composer to edit it. The host asks
// the daemon to withdraw it, so the composer is the prompt's only owner.
type DashboardActionWithdrawStartupPrompt struct {
	SessionID string
	Prompt    chat.StartupPrompt
}

type DashboardActionCreateSession struct {
	RememberContainerSize bool
	// CreateManagedWorktree is nil when unset.
	CreateManagedWorktree *bool
	Subagents             *core.SubagentPolicy
	Oracle                *bool
	OracleChoice          *core.SubagentChoice
	// Review is the session's automatic turn-review choice. Nil follows the
	// global [review] settings.
	Review *core.SessionReview
	// WorkspaceID is the workspace selected when the creation request was
	// submitted. The dashboard may switch tabs while validation or
	// dirty-repository confirmation is still in flight, so the request keeps
	// its origin.
	WorkspaceID string
	ProfileID   string
	// Provider load-balances across enabled profiles in this provider when
	// ProfileID is empty.
	Provider         *core.QuotaProvider
	BundleID         string
	ProjectDirectory *string
	TargetTemplateID string
	AdditionalMounts []core.AdditionalMount
	// ResourceAllocation is nil when unset.
	ResourceAllocation core.SessionResourceAllocation
	// Carried holds choices the wizard does not offer, copied from another
	// session.
	Carried CarriedLaunchSettings
}

// DashboardActionPreflightCreateSession resolves all network sources for an
// isolated session and leaves the creation wizard open until the person
// reviews that plan.
type DashboardActionPreflightCreateSession struct {
	Launch DashboardAction
}

type DashboardActionRepairRepositoryRemotes struct {
	BundleID string
	Repairs  []core.LocalRemoteRepair
	Retry    DashboardAction
}

// DashboardActionDiscoverProjects asks the host that owns a path field for
// its completion candidates.
type DashboardActionDiscoverProjects struct {
	Context string
	Request core.ProjectDiscoveryRequest
}

type DashboardActionCompletePath struct {
	Host   core.CompletionHost
	Kind   core.CompletionKind
	Prefix string
}

type DashboardActionValidateMountSource struct {
	TargetTemplateID string
	Source           string
}

type DashboardActionValidateSessionMounts struct {
	TargetTemplateID string
	Mounts           []core.AdditionalMount
	Launch           DashboardAction
}

type DashboardActionResolveContainerPath struct {
	SessionID string
	Source    string
}

type DashboardActionResolveSetupPath struct {
	Generation uint64
	Draft      any
	Path       []string
	Value      string
	// Machine is the machine the path belongs to, which is what owns a home
	// directory to expand `~` against.
	Machine core.Machine
}

// DashboardActionPreviewBuildCache resolves the automatic build cache values
// for a machine so the settings page can show them. Key identifies the
// settings resolved.
type DashboardActionPreviewBuildCache struct {
	Generation uint64
	Key        any
	Machine    core.Machine
}

// DashboardActionInstallMbx installs the controller-pinned mbx release on a
// machine's container host.
type DashboardActionInstallMbx struct {
	Generation uint64
	Key        any
	MachineID  string
	Machine    core.Machine
}

// DashboardActionPreviewArchiveSpace measures how much disk Mjolnir's session
// copies use, and how much an `archive_after_days` value would free, for the
// SessionWiki settings page. OlderThanDays is the value being shown or typed.
type DashboardActionPreviewArchiveSpace struct {
	Generation    uint64
	OlderThanDays *uint32
}

type DashboardActionValidateProjectDirectory struct {
	TargetTemplateID string
	Directory        string
}

type DashboardActionResumeSession struct {
	// WorkspaceID is the workspace selected when resume was submitted.
	// Nested mount and repository preflight actions carry this launch
	// unchanged.
	WorkspaceID        string
	SessionID          string
	ProfileID          string
	TargetTemplateID   string
	AdditionalMounts   []core.AdditionalMount
	ResourceAllocation core.SessionResourceAllocation
	DiscardQueue       bool
}

// DashboardActionMoveSession moves a live session through one daemon-owned
// stop/resume operation. The workspace is intentionally absent: it is fixed
// by the session record and never offered as a move selector.
type DashboardActionMoveSession struct {
	// Subagents is the delegation policy for the destination; nil keeps the
	// session's.
	Subagents               *core.SubagentPolicy
	Oracle                  *bool
	OracleChoice            *core.SubagentChoice
	Consent                 core.SuspendConsent
	SkipKeepPaths           []string
	SessionID               string
	ProfileID               string
	TargetTemplateID        string
	AdditionalMounts        []core.AdditionalMount
	ResourceAllocation      core.SessionResourceAllocation
	ClearResourceAllocation bool
	// PreparationRequestID set requests asynchronous preparation; nil
	// executes the preparation retained by the wizard.
	PreparationRequestID *uint64
	Queue                *core.ResumeQueueDisposition
}

// DashboardActionRetryMove retries a retained Move checkpoint, using the
// exact failed destination and queue disposition recorded by the daemon.
type DashboardActionRetryMove struct {
	Operation core.MoveOperation
}

// DashboardActionResumeMove resumes a retained Move checkpoint with the
// source settings recorded before interruption.
type DashboardActionResumeMove struct {
	Operation core.MoveOperation
}

type DashboardActionPreflightResumeRepositories struct {
	Launch DashboardAction
}

// DashboardActionConfirmRawConversion reports that a person confirmed moving
// a local checkout into an isolated workspace. The receipt travels with the
// confirmation so the launch does not have to run the preflight again.
type DashboardActionConfirmRawConversion struct {
	Launch  DashboardAction
	Receipt core.ResumeRepositorySourceReceipt
}

type DashboardActionReplaceResumeRepositoryOrigin struct {
	SessionID    string
	RepositoryID string
	Replacement  string
	Launch       DashboardAction
}

type DashboardActionCancelOperation struct {
	SessionID string
	Kind      SessionOperationKind
}

type DashboardActionCreateBundle struct {
	Sources []string
}

type DashboardActionRemoveBundle struct {
	BundleID string
}

type DashboardActionSuspendPreflight struct {
	SessionID string
}

type DashboardActionSuspend struct {
	SessionID string
	Consent   core.SuspendConsent
}

type DashboardActionDestroyStopped struct {
	SessionID string
}

type DashboardActionForceDestroy struct {
	SessionID string
}

type DashboardActionRenameSession struct {
	SessionID string
	Title     string
}

// DashboardActionChangeWorkspace moves a session, and its sub-agents, to
// another workspace.
type DashboardActionChangeWorkspace struct {
	SessionID     string
	WorkspaceID   string
	WorkspaceName string
}

// DashboardActionRefreshAll re-probes every target's capacity and asks every
// profile for its quota again. One key does both, so there is one action
// rather than two.
type DashboardActionRefreshAll struct{}

type DashboardActionRenameProfile struct {
	OldID string
	NewID string
}

type DashboardActionRenameTarget struct {
	OldID string
	NewID string
}

type DashboardActionTestTarget struct {
	TargetID string
}

type DashboardActionCancelTargetTest struct{}

// DashboardActionCheckAvailability asks the daemon to check launch
// availability and publish it on the feed.
type DashboardActionCheckAvailability struct{}

// DashboardActionCheckProjectDirectories finds out, off the render loop,
// which of these configured project directories exist; answers return
// through DashboardState.ApplyProjectDirectoryCheck.
type DashboardActionCheckProjectDirectories struct {
	Paths []string
}

// DashboardActionLoadMountHistory reads the stored mount and
// project-directory history on a worker and hands it back through
// DashboardState.ApplyMountHistory.
type DashboardActionLoadMountHistory struct{}

type DashboardActionRefreshProjects struct {
	Retry bool
}

type DashboardActionLoadWebAccess struct{}

// DashboardActionRestartDaemon stops the running daemon and starts one from
// the build this surface is running, then reports which build came up. The
// keep-alive never starts a daemon, so this is how a surface gets one back.
type DashboardActionRestartDaemon struct{}

type DashboardActionRecoverWebViewer struct {
	Recovery WebViewerRecovery
}

type DashboardActionInspectWebListener struct{}

type DashboardActionCancelWebAccess struct{}

// DashboardActionPasteFromClipboard reads the system clipboard on a worker
// before applying its contents. Clipboard providers may perform IPC and must
// never run on the TUI loop.
type DashboardActionPasteFromClipboard struct{}

type DashboardActionCopyNativeSessionId struct {
	NativeSessionID string
}

// DashboardActionCopySessionId copies a Mjolnir session's full ID to the
// clipboard, from the session action menu or the palette.
type DashboardActionCopySessionId struct {
	SessionID string
}

type DashboardActionMarkAllRead struct {
	// Receipts are (session id, read-through sequence) pairs.
	Receipts []client.DaemonPair[string, uint64]
}

type DashboardActionDismissNotification struct {
	SessionID string
	Target    core.NotificationDismissal
}

type DashboardActionDismissNotifications struct {
	Dismissals []client.DaemonPair[string, core.NotificationDismissal]
}

// DashboardActionSetSessionNotificationsIgnored toggles whether the session
// raises bell, desktop, and push notifications.
type DashboardActionSetSessionNotificationsIgnored struct {
	SessionID string
	Ignored   bool
}

// DashboardActionPreferLoadBalancingProfile moves a profile to the front of
// `[quota] prefer`.
type DashboardActionPreferLoadBalancingProfile struct {
	ProfileID string
}

// DashboardActionClearLoadBalancingPreferences empties `[quota] prefer`.
type DashboardActionClearLoadBalancingPreferences struct{}

// DashboardActionProbeQuotaProfile probes one profile's quota now, even a
// disabled profile's.
type DashboardActionProbeQuotaProfile struct {
	ProfileID string
}

type DashboardActionOpenResumeDialog struct{}

type DashboardActionOpenFilteredResumeDialog struct {
	Query string
	State *SessionStateFilter
}

type DashboardActionImportSession struct {
	ProfileID       string
	NativeSessionID string
	DisplayTitle    string
}

// DashboardActionSearchArchivedSessions searches the SessionWiki index for
// the resume dialog. Answers can arrive out of order, so the request id
// decides which one the dialog keeps.
type DashboardActionSearchArchivedSessions struct {
	RequestID uint64
	Query     string
}

// DashboardActionLoadResumeRecord fetches the whole record of the Mjolnir row
// picked in the resume dialog, which lists rows rather than records, to open
// the resume wizard on it.
type DashboardActionLoadResumeRecord struct {
	SessionID string
}

// DashboardActionLoadArchivedBrief fetches the briefing shown under the
// resume dialog's list.
type DashboardActionLoadArchivedBrief struct {
	WikiID string
}

// DashboardActionLoadArchivedHits fetches the passages of one archived
// transcript that match the resume dialog's query, which the preview pane
// shows in place of the briefing.
type DashboardActionLoadArchivedHits struct {
	WikiID string
	Query  string
}

// DashboardActionRestoreArchivedSession starts a new session carrying a
// summary of an archived transcript.
type DashboardActionRestoreArchivedSession struct {
	WorkspaceID      string
	WikiID           string
	ProfileID        string
	TargetTemplateID string
}

type DashboardActionCancelImport struct{}

type DashboardActionConfirmImportBundle struct {
	ProjectRoot           string
	CreateManagedWorktree *bool
	Accepted              bool
}

type DashboardActionOpenConfig struct{}

type DashboardActionDiscoverSetup struct {
	Generation uint64
	Scope      DetectScope
	// Machines are the SSH and EC2 machines in the Settings draft, whose
	// runtimes a runtime detection probes as well as this machine's.
	Machines []client.DaemonPair[string, core.Machine]
}

type DashboardActionSaveSetup struct {
	Generation uint64
	Original   string
	Updated    string
}

// DashboardActionSaveSetupAndLogin saves a managed profile and then opens
// its sign-in modal.
type DashboardActionSaveSetupAndLogin struct {
	Generation uint64
	Original   string
	Updated    string
	ProfileID  string
}

// DashboardActionOpenManagedSetup opens the guided chooser for a harness
// Mjolnir installs itself.
type DashboardActionOpenManagedSetup struct{}

// DashboardActionSetupLogin opens the selected profile's sign-in flow in a
// dashboard modal. The generation anchors the result to the open dialog.
type DashboardActionSetupLogin struct {
	Generation uint64
	ProfileID  string
}

// DashboardActionSaveSpinnerStyle persists the client-side activity
// animation without replacing other settings.
type DashboardActionSaveSpinnerStyle struct {
	Style core.SpinnerStyle
}

// DashboardActionSaveContainerSettings saves per-session container
// provisioning inputs, taking effect the next time the container is created.
type DashboardActionSaveContainerSettings struct {
	SessionID          string
	ResourceAllocation core.SessionResourceAllocation
	AdditionalMounts   []core.AdditionalMount
	MountHistory       []string
}

// DashboardActionSelectWorkspace selects a workspace in the dashboard's local
// tab strip. The controller captures any chat/composer draft before applying
// the selection.
type DashboardActionSelectWorkspace struct {
	WorkspaceID string
}

type DashboardActionExitSubagentWorkspace struct{}

// DashboardActionInterruptTurn interrupts the running turn of this session's
// open conversation, as Esc in its composer does.
type DashboardActionInterruptTurn struct {
	SessionID string
}

// DashboardActionInterruptAll ends every turn Interrupt all found under one
// session: each Mjolnir session's turn through the daemon, and each
// harness-native sub-agent through its owner.
type DashboardActionInterruptAll struct {
	SessionID string
}

// DashboardActionOpenSubagents shows a session's sub-agents in their own
// workspace. The controller saves the composer draft first, as the prompt
// border's click does.
type DashboardActionOpenSubagents struct {
	ParentID string
}

type DashboardActionLoadNativeAgentHistory struct {
	Owner string
	Child string
	// Before is the (position, stable id) of the oldest item already held;
	// nil loads the newest page.
	Before *client.DaemonPair[uint64, string]
}

type DashboardActionStopNativeAgent struct {
	Owner string
	Child string
}

// DashboardActionLoadWorkspaceManagement loads the workspace list and
// detached drafts for the workspace manager.
type DashboardActionLoadWorkspaceManagement struct {
	Generation uint64
}

type DashboardActionCreateWorkspace struct {
	Generation uint64
	Name       string
}

type DashboardActionRenameWorkspace struct {
	Generation  uint64
	WorkspaceID string
	Name        string
}

type DashboardActionCancelWorkspaceClose struct {
	WorkspaceID string
}

type DashboardActionCloseWorkspace struct {
	Generation  uint64
	WorkspaceID string
}

type DashboardActionRecoverWorkspaceDraft struct {
	Generation uint64
	DraftID    string
}

// DashboardActionToggleTranscriptRendering switches the visible conversation
// between rendered Markdown and raw text.
type DashboardActionToggleTranscriptRendering struct{}

// DashboardActionToggleDictation starts or stops dictating into the visible
// conversation's composer.
type DashboardActionToggleDictation struct{}

type DashboardActionQuitDetach struct{}

func (DashboardActionSetOperatorAway) isDashboardAction()                {}
func (DashboardActionNone) isDashboardAction()                           {}
func (DashboardActionCheckGitIdentity) isDashboardAction()               {}
func (DashboardActionSaveGitIdentity) isDashboardAction()                {}
func (DashboardActionGoLaunch) isDashboardAction()                       {}
func (DashboardActionGoPrepareProject) isDashboardAction()               {}
func (DashboardActionOpen) isDashboardAction()                           {}
func (DashboardActionNotificationReply) isDashboardAction()              {}
func (DashboardActionNotificationSkip) isDashboardAction()               {}
func (DashboardActionPinSession) isDashboardAction()                     {}
func (DashboardActionUnpinSession) isDashboardAction()                   {}
func (DashboardActionSplitConversation) isDashboardAction()              {}
func (DashboardActionOpenSessionInSplit) isDashboardAction()             {}
func (DashboardActionSplitPane) isDashboardAction()                      {}
func (DashboardActionClosePane) isDashboardAction()                      {}
func (DashboardActionConversationPanesChanged) isDashboardAction()       {}
func (DashboardActionPreviewContainerUpgrade) isDashboardAction()        {}
func (DashboardActionUpgradeContainer) isDashboardAction()               {}
func (DashboardActionRestartSession) isDashboardAction()                 {}
func (DashboardActionWarmProfileCapabilities) isDashboardAction()        {}
func (DashboardActionProbeGitStatus) isDashboardAction()                 {}
func (DashboardActionQueueStartupPrompt) isDashboardAction()             {}
func (DashboardActionWithdrawStartupPrompt) isDashboardAction()          {}
func (DashboardActionCreateSession) isDashboardAction()                  {}
func (DashboardActionPreflightCreateSession) isDashboardAction()         {}
func (DashboardActionRepairRepositoryRemotes) isDashboardAction()        {}
func (DashboardActionDiscoverProjects) isDashboardAction()               {}
func (DashboardActionCompletePath) isDashboardAction()                   {}
func (DashboardActionValidateMountSource) isDashboardAction()            {}
func (DashboardActionValidateSessionMounts) isDashboardAction()          {}
func (DashboardActionResolveContainerPath) isDashboardAction()           {}
func (DashboardActionResolveSetupPath) isDashboardAction()               {}
func (DashboardActionPreviewBuildCache) isDashboardAction()              {}
func (DashboardActionInstallMbx) isDashboardAction()                     {}
func (DashboardActionPreviewArchiveSpace) isDashboardAction()            {}
func (DashboardActionValidateProjectDirectory) isDashboardAction()       {}
func (DashboardActionResumeSession) isDashboardAction()                  {}
func (DashboardActionMoveSession) isDashboardAction()                    {}
func (DashboardActionRetryMove) isDashboardAction()                      {}
func (DashboardActionResumeMove) isDashboardAction()                     {}
func (DashboardActionPreflightResumeRepositories) isDashboardAction()    {}
func (DashboardActionConfirmRawConversion) isDashboardAction()           {}
func (DashboardActionReplaceResumeRepositoryOrigin) isDashboardAction()  {}
func (DashboardActionCancelOperation) isDashboardAction()                {}
func (DashboardActionCreateBundle) isDashboardAction()                   {}
func (DashboardActionRemoveBundle) isDashboardAction()                   {}
func (DashboardActionSuspendPreflight) isDashboardAction()               {}
func (DashboardActionSuspend) isDashboardAction()                        {}
func (DashboardActionDestroyStopped) isDashboardAction()                 {}
func (DashboardActionForceDestroy) isDashboardAction()                   {}
func (DashboardActionRenameSession) isDashboardAction()                  {}
func (DashboardActionChangeWorkspace) isDashboardAction()                {}
func (DashboardActionRefreshAll) isDashboardAction()                     {}
func (DashboardActionRenameProfile) isDashboardAction()                  {}
func (DashboardActionRenameTarget) isDashboardAction()                   {}
func (DashboardActionTestTarget) isDashboardAction()                     {}
func (DashboardActionCancelTargetTest) isDashboardAction()               {}
func (DashboardActionCheckAvailability) isDashboardAction()              {}
func (DashboardActionCheckProjectDirectories) isDashboardAction()        {}
func (DashboardActionLoadMountHistory) isDashboardAction()               {}
func (DashboardActionRefreshProjects) isDashboardAction()                {}
func (DashboardActionLoadWebAccess) isDashboardAction()                  {}
func (DashboardActionRestartDaemon) isDashboardAction()                  {}
func (DashboardActionRecoverWebViewer) isDashboardAction()               {}
func (DashboardActionInspectWebListener) isDashboardAction()             {}
func (DashboardActionCancelWebAccess) isDashboardAction()                {}
func (DashboardActionPasteFromClipboard) isDashboardAction()             {}
func (DashboardActionCopyNativeSessionId) isDashboardAction()            {}
func (DashboardActionCopySessionId) isDashboardAction()                  {}
func (DashboardActionMarkAllRead) isDashboardAction()                    {}
func (DashboardActionDismissNotification) isDashboardAction()            {}
func (DashboardActionDismissNotifications) isDashboardAction()           {}
func (DashboardActionSetSessionNotificationsIgnored) isDashboardAction() {}
func (DashboardActionPreferLoadBalancingProfile) isDashboardAction()     {}
func (DashboardActionClearLoadBalancingPreferences) isDashboardAction()  {}
func (DashboardActionProbeQuotaProfile) isDashboardAction()              {}
func (DashboardActionOpenResumeDialog) isDashboardAction()               {}
func (DashboardActionOpenFilteredResumeDialog) isDashboardAction()       {}
func (DashboardActionImportSession) isDashboardAction()                  {}
func (DashboardActionSearchArchivedSessions) isDashboardAction()         {}
func (DashboardActionLoadResumeRecord) isDashboardAction()               {}
func (DashboardActionLoadArchivedBrief) isDashboardAction()              {}
func (DashboardActionLoadArchivedHits) isDashboardAction()               {}
func (DashboardActionRestoreArchivedSession) isDashboardAction()         {}
func (DashboardActionCancelImport) isDashboardAction()                   {}
func (DashboardActionConfirmImportBundle) isDashboardAction()            {}
func (DashboardActionOpenConfig) isDashboardAction()                     {}
func (DashboardActionDiscoverSetup) isDashboardAction()                  {}
func (DashboardActionSaveSetup) isDashboardAction()                      {}
func (DashboardActionSaveSetupAndLogin) isDashboardAction()              {}
func (DashboardActionOpenManagedSetup) isDashboardAction()               {}
func (DashboardActionSetupLogin) isDashboardAction()                     {}
func (DashboardActionSaveSpinnerStyle) isDashboardAction()               {}
func (DashboardActionSaveContainerSettings) isDashboardAction()          {}
func (DashboardActionSelectWorkspace) isDashboardAction()                {}
func (DashboardActionExitSubagentWorkspace) isDashboardAction()          {}
func (DashboardActionInterruptTurn) isDashboardAction()                  {}
func (DashboardActionInterruptAll) isDashboardAction()                   {}
func (DashboardActionOpenSubagents) isDashboardAction()                  {}
func (DashboardActionLoadNativeAgentHistory) isDashboardAction()         {}
func (DashboardActionStopNativeAgent) isDashboardAction()                {}
func (DashboardActionLoadWorkspaceManagement) isDashboardAction()        {}
func (DashboardActionCreateWorkspace) isDashboardAction()                {}
func (DashboardActionRenameWorkspace) isDashboardAction()                {}
func (DashboardActionCancelWorkspaceClose) isDashboardAction()           {}
func (DashboardActionCloseWorkspace) isDashboardAction()                 {}
func (DashboardActionRecoverWorkspaceDraft) isDashboardAction()          {}
func (DashboardActionToggleTranscriptRendering) isDashboardAction()      {}
func (DashboardActionToggleDictation) isDashboardAction()                {}
func (DashboardActionQuitDetach) isDashboardAction()                     {}

// RemoteRepositoryPreview is the network plan shown before an isolated
// session is created. URLs have already been sanitized at the resolver
// boundary, so this type is safe to render in either the terminal or the
// phone viewer.
type RemoteRepositoryPreview struct {
	RepositoryID  string
	FetchURL      string
	DefaultBranch string
	PushURLs      []string
}

// SessionOperationKind is a lifecycle operation the dashboard shows in
// progress.
type SessionOperationKind uint8

const (
	SessionOperationKindLaunching SessionOperationKind = iota
	SessionOperationKindResuming
	SessionOperationKindMoving
	SessionOperationKindSuspending
	SessionOperationKindDestroying
	// SessionOperationKindStopping is a sub-agent stopped because its parent
	// is being suspended.
	SessionOperationKindStopping
	SessionOperationKindConnecting
	SessionOperationKindImporting
)

func (k SessionOperationKind) Label() string {
	switch k {
	case SessionOperationKindLaunching:
		return "Launch"
	case SessionOperationKindResuming:
		return "Resuming"
	case SessionOperationKindMoving:
		return "Moving"
	case SessionOperationKindSuspending:
		return "Suspending"
	case SessionOperationKindDestroying:
		return "Destroying"
	case SessionOperationKindStopping:
		return "Stopping"
	case SessionOperationKindConnecting:
		return "Connecting"
	default:
		return "Importing"
	}
}

// TransitionKind is the transition an operation temporarily owns in the user
// interface. Connecting and importing an already-stopped record are
// background activities, not conversation-replacing lifecycle transitions,
// so they return nil.
func (k SessionOperationKind) TransitionKind() *core.SessionTransitionKind {
	var kind core.SessionTransitionKind
	switch k {
	case SessionOperationKindLaunching:
		kind = core.SessionTransitionKindStarting
	case SessionOperationKindResuming:
		kind = core.SessionTransitionKindResuming
	case SessionOperationKindMoving:
		kind = core.SessionTransitionKindMoving
	case SessionOperationKindSuspending:
		kind = core.SessionTransitionKindSuspending
	case SessionOperationKindDestroying:
		kind = core.SessionTransitionKindDestroying
	case SessionOperationKindStopping:
		kind = core.SessionTransitionKindStopping
	default:
		return nil
	}
	return &kind
}

// Focus is which part of the combined surface owns the keyboard.
//
// Workspaces, the four support panes, and the composer share a Tab ring; the
// transcript is not a stop on it, because it is read with the wheel and
// PageUp/PageDown rather than driven from the keyboard.
type Focus uint8

const (
	FocusWorkspaces Focus = iota
	FocusSessions
	FocusQuota
	FocusTargets
	FocusNotifications
	FocusPrompt
)

// SupportPane is a dashboard pane whose height the user can control.
type SupportPane uint8

const (
	SupportPaneSessions SupportPane = iota
	SupportPaneTargets
	SupportPaneQuota
	SupportPaneNotifications
)

func (p SupportPane) Focus() Focus {
	switch p {
	case SupportPaneSessions:
		return FocusSessions
	case SupportPaneTargets:
		return FocusTargets
	case SupportPaneQuota:
		return FocusQuota
	default:
		return FocusNotifications
	}
}

// SupportPane is the support pane this focus stands for; ok is false for
// Workspaces and Prompt.
func (f Focus) SupportPane() (pane SupportPane, ok bool) {
	switch f {
	case FocusSessions:
		return SupportPaneSessions, true
	case FocusTargets:
		return SupportPaneTargets, true
	case FocusQuota:
		return SupportPaneQuota, true
	case FocusNotifications:
		return SupportPaneNotifications, true
	default:
		return 0, false
	}
}

// focusOrder is the Tab order: Workspaces, then Sessions, the composer, and
// the remaining support panes. Shift-Tab walks it backwards.
var focusOrder = [6]Focus{
	FocusWorkspaces,
	FocusSessions,
	FocusPrompt,
	FocusTargets,
	FocusQuota,
	FocusNotifications,
}

// Mode is the dialog, wizard or overlay that owns the dashboard, or
// ModeDashboard when none does. Every variant but ModeDashboard embeds a
// pointer to its dialog, so `if m, ok := d.mode.(ModeNew); ok {
// m.NewWizard... }` edits the open dialog in place.
type Mode interface {
	isMode()
}

type ModeDashboard struct{}

type ModeNew struct{ *NewWizard }

type ModeResume struct{ *ResumeWizard }

// ModeResumeDialog is the unified picker for every session that is not live.
type ModeResumeDialog struct{ *ResumeDialog }

type ModeRepositoryOrigin struct{ *RepositoryOriginDialog }

type ModeConfigId struct{ *ConfigIdEditor }

type ModeTargetActions struct{ *TargetActionsDialog }

type ModeWeb struct{ *WebDialog }

type ModeWorkspaceManager struct{ *WorkspaceManager }

type ModeRename struct{ *RenameEditor }

// ModeChangedFiles is the selected session's changed files, branch, and
// upstream distance.
type ModeChangedFiles struct{ *ChangedFilesDialog }

// ModeNoticeLog is the last notices the footer showed, newest first.
type ModeNoticeLog struct{ *NoticeLogDialog }

type ModeSessionCpuReport struct{ *SessionCpuReportDialog }

type ModeSessionUsageReport struct{ *SessionUsageDialog }

type ModeEditContainer struct{ *ContainerEditor }

// ModeGitIdentity asks for the host's missing Git author identity before a
// new session.
type ModeGitIdentity struct{ *GitIdentityDialog }

type ModeImporting struct{ *ImportProgress }

type ModeConfirmImportBundle struct{ *ImportBundleConfirmation }

type ModeConfirm struct{ *ConfirmDialog }

// ModePalette is the searchable command palette: every command that applies
// right now.
type ModePalette struct{ *CommandPalette }

type ModeSetup struct{ *SetupDialog }

type ModeWelcome struct{ *WelcomeDialog }

func (ModeDashboard) isMode()           {}
func (ModeNew) isMode()                 {}
func (ModeResume) isMode()              {}
func (ModeResumeDialog) isMode()        {}
func (ModeRepositoryOrigin) isMode()    {}
func (ModeConfigId) isMode()            {}
func (ModeTargetActions) isMode()       {}
func (ModeWeb) isMode()                 {}
func (ModeWorkspaceManager) isMode()    {}
func (ModeRename) isMode()              {}
func (ModeChangedFiles) isMode()        {}
func (ModeNoticeLog) isMode()           {}
func (ModeSessionCpuReport) isMode()    {}
func (ModeSessionUsageReport) isMode()  {}
func (ModeEditContainer) isMode()       {}
func (ModeGitIdentity) isMode()         {}
func (ModeImporting) isMode()           {}
func (ModeConfirmImportBundle) isMode() {}
func (ModeConfirm) isMode()             {}
func (ModePalette) isMode()             {}
func (ModeSetup) isMode()               {}
func (ModeWelcome) isMode()             {}

// modeDiscriminant is Rust's std::mem::discriminant of a Mode: which variant
// it is, ignoring the dialog it holds.
func modeDiscriminant(mode Mode) reflect.Type {
	return reflect.TypeOf(mode)
}

// cycleControl is the control after current in order, or before it when
// reverse, wrapping at both ends. A current not in order counts as the first.
func cycleControl[T comparable](current T, order []T, reverse bool) T {
	index := max(slices.Index(order, current), 0)
	var next int
	if reverse {
		next = index - 1
		if next < 0 {
			next = len(order) - 1
		}
	} else {
		next = (index + 1) % len(order)
	}
	return order[next]
}

// DashboardPasteTarget is the identity of the dashboard input which requested
// a background text paste.
type DashboardPasteTarget struct {
	revision uint64
	mode     reflect.Type
	session  *string
	pane     PaneId
	focus    Focus
}

// quotaRenderedSelection is the selected quota row and the first and last
// line it was drawn on.
type quotaRenderedSelection struct {
	row   QuotaRow
	first int
	last  int
}

// conversationPaneArea is where one conversation pane's transcript and
// composer sat on the last frame.
type conversationPaneArea struct {
	pane       PaneId
	transcript uv.Rectangle
	prompt     uv.Rectangle
}

// paneSizeControlArea is the click target of one size control in a support
// pane's title.
type paneSizeControlArea struct {
	pane SupportPane
	size PaneSize
	area uv.Rectangle
}

// rowClick is the pane, row index, and time of a left click on a session row.
type rowClick struct {
	focus Focus
	index int
	at    time.Time
}

// modalClickTransition is the cell and time of the click that opened or
// closed a modal.
type modalClickTransition struct {
	position uv.Position
	at       time.Time
}

// workspaceScrollSelection is the selected workspace tab's id and row, the
// tab strip's width and the pane's inner height, as of the last frame.
type workspaceScrollSelection struct {
	workspaceID string
	row         int
	tabsWidth   int
	height      int
}

// DashboardState is the stateful, renderable projection of controller
// configuration and state.
//
// The fields wrapped in Tracked are the inputs the session facts and the
// Sessions pane's order are derived from (see session_view.go).
type DashboardState struct {
	// notificationCards is nil until the daemon publishes cards.
	notificationCards      *core.SnapshotMap[string, client.NotificationCard]
	formHolders            core.SnapshotMap[string, string]
	unreadFrontiers        core.SnapshotMap[string, uint64]
	notificationDismissals Tracked[core.SnapshotMap[string, core.NotificationDismissal]]
	sessionVerdicts        Tracked[core.SnapshotMap[string, client.SessionVerdict]]
	servedAttention        Tracked[core.SnapshotMap[string, core.SessionAttention]]
	pasteRevision          uint64
	// questionPress is a press on a question's answer control: both the
	// form's pending click and a selection candidate; see
	// RouteQuestionPress.
	questionPress    *uv.Position
	pendingWelcome   *WelcomeDialog
	operatorPresence OperatorPresence
	// reconciliationVisits counts reconciliation passes for tests.
	reconciliationVisits  int
	rowIndex              RowIndex
	presentedRecords      core.SnapshotMap[string, core.SessionRecord]
	durableRecords        core.SnapshotMap[string, core.SessionRecord]
	nativeRunningByParent map[string]map[string]struct{}
	nativeByParent        Tracked[map[string]map[string]struct{}]
	nativeByOwner         map[string]map[string]struct{}
	sessionFactsField     SessionFacts
	cpuRollupsField       CpuRollups
	sessionOrderField     SessionOrder
	config                Tracked[core.Config]
	profileCapabilities   core.ProfileCapabilitiesSnapshot
	// profileLoginStatuses is the sign-in status of saved profiles, read off
	// the loop when Settings opens and after a login. Missing entries are
	// drafts or not yet read.
	profileLoginStatuses map[string]core.LoginStatus
	// profileLoginRefresh is set when a surface that shows sign-in status
	// opened, or a login finished. The dashboard loop reads the statuses off
	// the loop when it takes this.
	profileLoginRefresh bool
	// profileLoginGeneration orders status reads so a slow earlier read
	// cannot replace a newer one.
	profileLoginGeneration uint64
	// pendingSetupLogin is a managed profile (generation, profile id) saved
	// from setup whose login starts after the save result and updated
	// configuration have been applied.
	pendingSetupLogin        *client.DaemonPair[uint64, string]
	profileHydrationRequests map[string]map[string]struct{}
	profileHydrationErrors   map[string]string
	// profileHydrationSent is the capability demand last sent for a Settings
	// draft, so each draft selection is warmed once rather than after every
	// background reply.
	profileHydrationSent *string
	// keybinds are the resolved `[keys]` bindings, refreshed whenever
	// configuration is replaced so a `config.toml` edit takes effect without
	// a restart.
	keybinds core.Keybinds
	// prefixPending is whether the prefix key has been pressed and the next
	// key completes a chord. Cleared by that key, by Esc, and by any mouse
	// press.
	prefixPending   bool
	resizeMode      bool
	state           Tracked[core.StateState]
	quotas          map[string]client.ProfileQuota
	quotaRefreshing map[string]struct{}
	sessionDetails  Tracked[map[string]*SessionDetail]
	// unreachableSessions are sessions whose relay worker the controller
	// currently cannot reach. Their summary band renders red so an
	// unreachable target is obvious at a glance.
	unreachableSessions Tracked[map[string]struct{}]
	// sessionReviewsField is the controller's complete review projection,
	// keyed by session. Review state is an overlay on the primary session
	// lifecycle and is replaced as a whole whenever a runtime snapshot
	// arrives, so removals clear stale row badges as well as closing the
	// review pane.
	sessionReviewsField    Tracked[map[string][]client.RuntimeReviewView]
	projectSources         Tracked[map[string]core.ProjectSourceIdentity]
	checkpointArchiveSizes map[string]*uint64
	// stoppedRecords are stopped records loaded one at a time: the row
	// picked in the resume dialog, the session `mj go` starts in, and a
	// session just imported. The runtime feed carries only live sessions;
	// read both through DashboardState.sessionRecord.
	stoppedRecords core.SnapshotMap[string, client.SessionRecordWithVerdict]
	// launchRecency is what new-session defaults are chosen from. The feed
	// carries only live sessions, so this summary of every session arrives
	// beside them.
	launchRecency []client.LaunchRecency
	recentLaunch  *core.RecentLaunch
	// containerUpgradeSessions is the daemon's verdict, delivered with its
	// records in the same runtime frame.
	containerUpgradeSessions map[string]struct{}
	failedLaunchRetries      map[string]struct{}
	sessionOperations        Tracked[map[string]*SessionOperationDisplay]
	// standbyPrompts are the real composers parked in front of sessions that
	// are not attached yet: a Starting/Resuming transition or an in-flight
	// attach. Keyed per session so each draft stays with its row; the
	// controller hands the draft to the real composer when the chat opens.
	standbyPrompts map[string]*chat.ChatState
	// moveOperations are durable move intents retained by the daemon,
	// including failed and cancelled operations that still have an explicit
	// recovery action.
	moveOperations  core.SnapshotMap[string, core.MoveOperation]
	capacityDetails map[string]client.RuntimeHostCapacity
	// containerCapacities is the daemon's hard container budget, including
	// in-flight resume and move reservations. Raw host utilization remains
	// in capacityDetails.
	containerCapacities map[string]client.RuntimeContainerCapacity
	// targetStorage is the daemon storage owner's verdict per target host.
	// The daemon is the only judge of a full disk; this surface shows what
	// it says.
	targetStorage []core.TargetStorageView
	// versionLabel is the build stamped on the workspace pane, as `v2.11.0`.
	// Tests render testVersionLabel instead.
	versionLabel string
	// availability is the one answer to which profiles and runtimes are
	// there.
	availability AvailabilityAvailability
	// projectDirectoryChecks says whether the local directories of
	// configured projects exist, so the project picker can mark the ones
	// that are gone without touching the filesystem while it draws.
	projectDirectoryChecks map[string]ProjectDirectoryCheck
	// mountHistoryRefreshPending is set when the New wizard opened and the
	// stored mount and project history should be read again, since sessions
	// created after startup add to it.
	mountHistoryRefreshPending bool
	gitIdentityGate            GitIdentityGate
	// gitIdentityFollowup is the new-session action a settled Git identity
	// check let through; nil when there is none.
	gitIdentityFollowup DashboardAction
	// heldUIRequests are dashboard requests (`mj ui`) waiting for the open
	// dialog to close, oldest first.
	heldUIRequests         []client.UiRequest
	commandSessionOverride *string
	// sessionsScroll, targetsScroll and quotaScroll are persisted scroll
	// offsets for the three list panes, so each scrolls only far enough to
	// keep its selection visible instead of jumping back to the top every
	// frame. Written by the renderer after it lets the table settle.
	sessionsScroll int
	targetsScroll  int
	quotaScroll    int
	// quotaViewport is the height of the quota viewport at the previous
	// render, in lines.
	quotaViewport int
	// quotaRenderedSelection is the selection and line range from the
	// previous quota render, before refresh reordering.
	quotaRenderedSelection *quotaRenderedSelection
	// recenterOnSelection makes the next render keep one row beyond the
	// selection visible in the direction of the latest keyboard or wheel
	// navigation, when it fits. This is consumed by that pane's renderer
	// after the movement.
	recenterOnSelection *Focus
	// sessionsWheelScrolledFrom is the selected session the user scrolled
	// the Sessions list away from with the wheel. While that session stays
	// selected, the renderer keeps the wheel's offset instead of scrolling
	// the selection back into view.
	sessionsWheelScrolledFrom *string
	capacityIndex             int
	// capacityDisksAreas are the last rendered Disks button bounds, indexed
	// like capacityDetails. Keyboard commands use this geometry to anchor
	// the details dropdown.
	capacityDisksAreas []*uv.Rectangle
	quotaSelection     QuotaRow
	// quotaExpanded is a sorted set of expanded pools.
	quotaExpanded  []PoolKey
	quotaProviders ProviderDiscovery
	// quotaRowAreas are the last rendered Quota row bounds, by profile id,
	// for the row's right-click menu.
	quotaRowAreas  []client.DaemonPair[string, uv.Rectangle]
	quotaPoolAreas []client.DaemonPair[QuotaRow, uv.Rectangle]
	focus          Focus
	// paneSizes are the independently selected sizes of Sessions, Targets,
	// and Quota.
	paneSizes     PaneSizes
	notifications NotificationsPane
	// navigation is the active pane and its assignment, the sole session
	// selection.
	navigation        ConversationNavigation
	browsePane        *PaneId
	pinIDs            map[string]uint32
	pendingNavigation *PendingNavigation
	// navigationSubagentParent is set when a workspace switch was requested
	// for a child: open its parent's Sub-agents view before restoring
	// pendingNavigation.
	navigationSubagentParent *string
	paneMenu                 *PaneMenu
	// conversationZoomed is whether the focused pane fills the conversation
	// band on its own. The arrangement underneath is untouched, so unzooming
	// puts every pane back where it was. It is a view state, not part of the
	// stored layout: a restart comes back unzoomed.
	conversationZoomed bool
	// openingSession is the session an attach is running for, while it is
	// still in flight. The conversation band draws as empty for as long as
	// this is set to a session other than the one on screen, so the
	// transcript never belongs to a different row than the highlight.
	openingSession    *string
	paneAreas         *[dashboardPaneCount]uv.Rectangle
	notificationsArea *uv.Rectangle
	// narrowLayout is whether the last frame was too narrow for a sidebar,
	// so the Sessions list was stacked above the conversation in its compact
	// form. Read by the renderer's size checks; set once per frame.
	narrowLayout bool
	// conversationPaneAreas is where each conversation pane's transcript and
	// composer sat on the last frame, so the controller can route a mouse
	// event by what the pointer is over rather than by what has focus, and
	// to the pane it is over.
	conversationPaneAreas []conversationPaneArea
	// conversationAreaField is the whole conversation band from the last
	// frame: the rectangle the tiled panes are laid out in. Splits and
	// directional pane focus are computed against it.
	conversationAreaField *uv.Rectangle
	resumeSessionsArea    *uv.Rectangle
	// frameSurfaces are the selectable surfaces, rebuilt by every frame in
	// render order so the selection engine can hit-test the screen the user
	// is looking at.
	frameSurfaces chat.FrameSurfaces
	surfaceForm   chat.Form[SurfaceControl]
	// sessionActionFocus is the optional action row selection inside
	// Sessions. A focused action temporarily owns Up/Down/Left/Right/Enter
	// while the pane itself keeps the selected session anchored to the
	// conversation.
	sessionActionFocus *CommandId
	sessionMenuIDs     []string
	// sessionsFilter is the Sessions pane's search and state filter, when one
	// is open.
	sessionsFilter Tracked[*SessionsFilter]
	// sessionsFiltersByScope are filters retained independently for the
	// top-level list and each nested sub-agent workspace. The active filter
	// remains the single authority. The zero ChildWorkspace (no parent id)
	// stands for the top-level list, Rust's None key.
	sessionsFiltersByScope map[ChildWorkspace]*SessionsFilter
	// sessionsText is what the daemon said about the Sessions filter's text
	// in conversations.
	sessionsText Tracked[SessionsTextSearch]
	// recentCommands are the commands run lately, newest first, for the
	// palette's Recent group.
	recentCommands []CommandId
	// resumeRowsField are the rows the open resume dialog shows, derived from
	// the records, the scans, and the dialog's own search. Rebuilt where
	// those change and once a second for the activity labels; empty when no
	// dialog is open.
	resumeRowsField []ResumeRow
	// resumeHitCounts is how many rows the current query matched on each
	// tab, indexed by ResumeTab.index (Rust's [usize; ResumeTab::COUNT]).
	// All zero when nothing is typed, and always zero for the Live tab,
	// which matches names rather than the index. Rebuilt beside
	// resumeRowsField, from the same merge, so the tabs a person is not
	// looking at can still say where their hits are.
	resumeHitCounts []int
	// sessionRowAreas are row hitboxes for the Active pane, keyed by the
	// row's index into the active session list. Each rect spans the summary
	// line and every visible preview line beneath it, so a click anywhere on
	// the row selects it.
	sessionRowAreas     []client.DaemonPair[int, uv.Rectangle]
	projectHeadingAreas []client.DaemonPair[string, uv.Rectangle]
	// paneSizeControlAreas are click targets for the visible size controls
	// in each support-pane title.
	paneSizeControlAreas []paneSizeControlArea
	// paneMaximizeEnabledField says whether the current frame gives each
	// pane a larger allocation when its size changes from Standard to the
	// exclusive Maximized state.
	paneMaximizeEnabledField [dashboardPaneCount]bool
	// collapsedProjectKeys are projects the user has collapsed in the
	// focused Sessions pane. Absent means expanded, so a project that
	// appears later starts expanded without any extra bookkeeping.
	collapsedProjectKeys      Tracked[map[string]struct{}]
	alertSequence             *uint64
	pendingAlerts             []PendingAlert
	attentionNavigation       *AttentionNavigation
	renderedFailureDismissals map[string]struct{}
	// lastRowClick is the pane, row index, and time of the most recent left
	// click on a session row, so the next click can be recognized as a
	// double click.
	lastRowClick           *rowClick
	mode                   Mode
	usageRequestGeneration uint64
	// goMode is Rust's `go` field (a Go keyword).
	goMode     *GoMode
	simpleMode bool
	// launchProjectDirectory is the Git repository `mj` was started in,
	// found before the dashboard opened. The new-session wizard offers it as
	// the local project.
	launchProjectDirectory *string
	goWorkspaces           map[string]GoMode
	// goContexts holds each workspace's (project directory, branch) or the
	// error reading it.
	goContexts map[string]client.DaemonResult[client.DaemonPair[string, string]]
	// gitStatus is what each session's checkout looked like when last read,
	// for the branch on its row and the changed-files overlay.
	gitStatus map[string]client.DaemonResult[core.SessionGitStatus]
	// gitProbeAt is when each session's checkout was last asked about, so
	// the host reads a visible session's status about once a minute and no
	// more.
	gitProbeAt map[string]time.Time
	// unreachableNotices is the unreachable-worker notice last shown for
	// each session, so the notice can be withdrawn once the worker answers
	// again.
	unreachableNotices         map[string]string
	modalClickTransition       *modalClickTransition
	suppressModalRelease       bool
	spinnerSavePending         bool
	sessionPreflightGeneration uint64
	wikiSearchGeneration       uint64
	// nextMovePreparationRequestID is the monotonic identity for move
	// preparation requests. This lives outside the wizard so a late reply
	// cannot match a newly opened wizard.
	nextMovePreparationRequestID uint64
	// archiveRestoresInFlight are archived transcripts, by wiki id, whose
	// restore has been sent and has not reported back yet. The restore
	// wizard closes as soon as it sends the restore, so this, not the
	// wizard, is what keeps a second press or a reopened wizard from
	// restoring the same transcript twice.
	archiveRestoresInFlight map[string]struct{}
	// resumePreflightGeneration is the sessionPreflightGeneration a live
	// resume's check was sent under. The check is in flight while that
	// generation is current: every result of the check, and closing the
	// wizard, moves the generation on. A restore's hold is the set above
	// instead, because the restore wizard closes, and moves the generation
	// on, as soon as it sends the restore.
	resumePreflightGeneration *uint64
	notices                   chat.Notices
	// workspaceName is the attached workspace name, used by the first-run
	// screen.
	workspaceName  string
	workspaceNames map[string]string
	// workspaceOrder is the stable tab order. Runtime snapshots may arrive in
	// a different map order, so existing ids retain their position and new
	// ids append.
	workspaceOrder []string
	// activeWorkspaceID is the local filter applied to the one global live
	// session feed.
	activeWorkspaceID Tracked[*string]
	// childWorkspace is the parent whose direct children temporarily replace
	// the ordinary workspace tabs.
	childWorkspace Tracked[*ChildWorkspace]
	nativeAgents   Tracked[map[string]*NativeAgentPane]
	sessionCPU     Tracked[core.SnapshotMap[string, client.SessionCpuView]]
	nativeSources  core.SnapshotMap[string, core.NativeAgentView]
	// stoppedSubagents are stored conversations of Mjolnir sub-agents that
	// have stopped, drawn read-only because there is no worker to attach to.
	stoppedSubagents map[string]*StoppedSubagentPane
	// stoppedBySuspend are sub-agents whose records left because their
	// parent's suspend stopped them, for the host to let their
	// conversations go.
	stoppedBySuspend StoppedBySuspend
	// workspaceViews is dashboard-only state retained while the user
	// switches tabs.
	workspaceViews map[string]WorkspaceViewState
	// workspacePaneSizesModified: a pane-size update from the controller may
	// not overwrite a local edit made in this client, even when it arrives
	// after the edit.
	workspacePaneSizesModified map[string]struct{}
	// workspaceLayoutsModified is the same rule for the conversation pane
	// layout: a controller update may not overwrite an arrangement this
	// client has already changed.
	workspaceLayoutsModified   map[string]struct{}
	workspaceTabAreas          []client.DaemonPair[string, uv.Rectangle]
	subagentWorkspaceCloseArea *uv.Rectangle
	workspacePaneArea          *uv.Rectangle
	workspaceScroll            int
	workspaceScrollSelected    *workspaceScrollSelection
	// workspaceScrollGeometry is (tab rows, visible rows) from the last
	// frame.
	workspaceScrollGeometry *client.DaemonPair[int, int]
	workspaceScrollbar      chat.ScrollbarDrag
	workspaceDropdownArea   *uv.Rectangle
	// workspaceHamburgerArea is the hamburger's exact three-cell rectangle
	// from the last frame. Rebuilt with the tabs so stale geometry cannot
	// activate an invisible manager control after the pane disappears.
	workspaceHamburgerArea *uv.Rectangle
	// workspaceControlFocus is transient focus inside the workspace pane.
	// The pane remains the persisted top-level focus; this only
	// distinguishes its tabs from the pinned manager button.
	workspaceControlFocus         WorkspaceControlFocus
	workspaceManagementGeneration uint64
	closingWorkspaces             map[string]struct{}
	renderChangeSnapshot          RenderChangeSnapshot
	// lastEventConsumed is whether a handler took responsibility for the
	// event being dispatched.
	lastEventConsumed bool
}

// WorkspaceViewState is dashboard-only state retained while the user
// switches workspace tabs.
type WorkspaceViewState struct {
	sessionsScroll            int
	sessionsWheelScrolledFrom *string
	targetsScroll             int
	quotaScroll               int
	capacityIndex             int
	quotaSelection            QuotaRow
	paneSizes                 PaneSizes
	conversationLayout        core.ConversationLayout
	collapsedProjectKeys      map[string]struct{}
	focus                     Focus
}

func workspaceViewStateFromDashboard(dashboard *DashboardState) WorkspaceViewState {
	panic("unported: mj-tui/src/lib.rs WorkspaceViewState::from_dashboard")
}

func (d *DashboardState) FinishSpinnerStyleSave() {
	d.spinnerSavePending = false
}

// NewDashboardState is Rust's DashboardState::new.
func NewDashboardState(config core.Config, state core.StateState, quotas map[string]client.ProfileQuota) *DashboardState {
	panic("unported: mj-tui/src/lib.rs DashboardState::new")
}

// dashboardVersionLabel is the build stamped on the workspace pane:
// testVersionLabel in tests, `v` and the package version otherwise.
func dashboardVersionLabel() string {
	if testing.Testing() {
		return testVersionLabel
	}
	return "v" + core.PackageVersion
}

func rectContains(area uv.Rectangle, column, row int) bool {
	return column >= area.Min.X && column < area.Max.X && row >= area.Min.Y && row < area.Max.Y
}

// moveIndex moves index by delta, saturating at 0 and len-1.
func moveIndex(index *int, length int, delta int) {
	if length == 0 {
		*index = 0
		return
	}
	*index = min(max(*index+delta, 0), length-1)
}

// nthKey is the index-th key of map in sorted (BTreeMap) order.
func nthKey[T any](m map[string]T, index int) string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	if index < 0 || index >= len(keys) {
		panic("wizard is only opened for non-empty configuration")
	}
	return keys[index]
}

func isPasteShortcut(key tea.KeyMsg) bool {
	pressed := key.Key()
	return pressed.Code == 'v' && pressed.Mod&(tea.ModCtrl|tea.ModSuper) != 0
}

// standbyPromptKey turns macOS's Super accelerator into Control for the
// standby composer, whose readline controls are Control chords.
func standbyPromptKey(key tea.KeyMsg) tea.KeyMsg {
	if runtime.GOOS != "darwin" {
		return key
	}
	pressed := key.Key()
	if !pressed.Mod.Contains(tea.ModSuper) || pressed.Mod.Contains(tea.ModAlt) {
		return key
	}
	pressed.Mod = pressed.Mod&^tea.ModSuper | tea.ModCtrl
	if _, release := key.(tea.KeyReleaseMsg); release {
		return tea.KeyReleaseMsg(pressed)
	}
	return tea.KeyPressMsg(pressed)
}

// dashboardAccelerator is the platform's primary accelerator: Super on
// macOS, Control elsewhere.
func dashboardAccelerator(modifiers tea.KeyMod) bool {
	if runtime.GOOS == "darwin" {
		return modifiers.Contains(tea.ModSuper)
	}
	return modifiers.Contains(tea.ModCtrl)
}
