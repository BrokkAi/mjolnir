// Port of mj-cli/src/dashboard/io.rs.
//
// One-shot background jobs the dashboard starts and the updates they report.
//
// Network jobs run as goroutines; synchronous work runs on its own goroutine
// too. Both answer over the dashboard's single dashboardIoUpdate channel,
// keeping work off the render loop and reporting failures as errors in the
// update.
//
// Rust's `Result<T, String>` payloads are a value field and an `err error`
// (nil for Ok) here, which is what the tui setters take; GoContext and
// GitStatus keep a client.DaemonResult because tui.DashboardState's
// SetGoContext and SetGitStatus store one. Rust's Box around a payload is
// dropped: Go values need no boxing.

package cli

import (
	"context"
	"errors"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// dashboardIoUpdate is everything the dashboard learns from a background
// job.
type dashboardIoUpdate interface{ isDashboardIoUpdate() }

type dashboardIoUpdateRuntimeConfirmationMissing struct{ message string }

type dashboardIoUpdateOperatorAway struct {
	away     bool
	revision uint64
	err      error
}

type dashboardIoUpdateProfileHydration struct {
	key string
	err error
}

type dashboardIoUpdateSessionUsageFinished struct {
	generation uint64
	report     core.SessionUsageReport
	err        error
}

type dashboardIoUpdateRepositoryRemotesRepaired struct {
	generation *uint64
	retry      tui.DashboardAction
	err        error
}

type dashboardIoUpdateWorkspaceClosed struct {
	generation  uint64
	workspaceID string
	// err is the daemon's confirmation, nil when it closed the workspace.
	// The dashboard removes the workspace from the manager it already shows
	// instead of querying everything again.
	err error
}

type dashboardIoUpdateWorkspaceCloseCancelled struct{ err error }

// dashboardIoUpdateWorkspaceManagement is a workspace manager snapshot
// loaded away from the event loop. The selection hint is set only by a
// successful create; the dashboard must still verify the modal generation
// before applying it. (Rust attaches this comment to WorkspaceClosed.)
type dashboardIoUpdateWorkspaceManagement struct {
	generation uint64
	result     workspaceManagementResult
	err        error
}

type dashboardIoUpdateWorkspacePaneSizes struct {
	sizes map[string]core.PaneSizes
	err   error
}

type dashboardIoUpdateWorkspaceLayouts struct {
	layouts map[string]core.ConversationLayout
	err     error
}

type dashboardIoUpdateMaterializedSessionProjection struct {
	sessionID string
	detail    *tui.PreparedMaterializedSessionDetail
	err       error
}

type dashboardIoUpdateNotificationDismissed struct {
	sessionID string
	target    core.NotificationDismissal
	ordinal   uint64
	err       error
}

// dashboardIoUpdateNotificationsDismissed is every result of one workspace
// "Clear notifications".
type dashboardIoUpdateNotificationsDismissed struct {
	results []notificationDismissalResult
}

// notificationDismissalResult is one entry of NotificationsDismissed: Rust's
// `(String, NotificationDismissal, Result<u64, String>)`.
type notificationDismissalResult struct {
	sessionID string
	target    core.NotificationDismissal
	ordinal   uint64
	err       error
}

// dashboardIoUpdateTranscriptTailSeed is a bounded tail of a resumed
// session's stored transcript, loaded off the event loop so the
// conversation is not blank while the poller catches up.
type dashboardIoUpdateTranscriptTailSeed struct {
	materialized *core.MaterializedSession
}

type dashboardIoUpdateNotificationMessageRead struct {
	request    tui.NotificationReadRequest
	generation uint64
	// text is nil when the message had no text to show.
	text *string
	err  error
}

type dashboardIoUpdateStoredSessionSummary struct {
	sessionID  string
	generation uint64
	summary    tui.PreparedMaterializedSessionSummary
	err        error
}

// dashboardIoUpdateStoppedSubagentTranscript is the stored tail of a
// stopped sub-agent's conversation, which is read rather than attached to.
type dashboardIoUpdateStoppedSubagentTranscript struct {
	sessionID string
	// session is nil when nothing is stored.
	session *core.MaterializedSession
	err     error
}

type dashboardIoUpdateProjectSource struct {
	sessionID string
	source    core.ProjectSourceIdentity
	err       error
}

type dashboardIoUpdateChatOpened struct {
	generation uint64
	sessionID  string
	prepared   *chat.PreparedChat
	err        error
}

type dashboardIoUpdateNativeAgentHistory struct {
	owner string
	child string
	page  core.NativeAgentHistoryPage
	err   error
}

type dashboardIoUpdateNativeAgentStopped struct {
	owner string
	child string
	err   error
}

// dashboardIoUpdateInterruptAllFinished: Interrupt all finished sending.
// Each failure names the session or native sub-agent view it was for.
type dashboardIoUpdateInterruptAllFinished struct {
	result client.InterruptAllResult
	err    error
}

// dashboardIoUpdateReviewRefused: the daemon refused a review action. The
// message is a sentence for the person who pressed the key, so it goes back
// to the chat that sent it. (Rust attaches this comment to
// NativeAgentHistory.)
type dashboardIoUpdateReviewRefused struct {
	sessionID string
	message   string
}

type dashboardIoUpdateCreateSession struct {
	update dashboardCreateSessionUpdate
}

// dashboardIoUpdateDaemonRestarted: an explicit daemon restart finished.
// The sentence is what the surface shows: which build came up, or (as err)
// why no daemon of this build did.
type dashboardIoUpdateDaemonRestarted struct {
	sentence string
	err      error
}

type dashboardIoUpdateGoSelectionSaved struct{ err error }

// dashboardIoUpdateGoContext carries Rust's `Result<(PathBuf, String),
// String>` as tui.DashboardState.SetGoContext takes it.
type dashboardIoUpdateGoContext struct {
	sessionID string
	result    client.DaemonResult[client.DaemonPair[string, string]]
}

type dashboardIoUpdateGitStatus struct {
	sessionID string
	result    client.DaemonResult[core.SessionGitStatus]
}

type dashboardIoUpdateGoPrepared struct {
	workspaceID string
	retry       tui.DashboardAction
	recipe      core.GoRecipe
	err         error
}

type dashboardIoUpdateRemotePreflight struct {
	generation uint64
	launch     tui.DashboardAction
	outcome    remotePreflightOutcome
	err        error
}

type dashboardIoUpdateRenameSession struct {
	title string
	// saved is the title the daemon stored.
	saved string
	err   error
}

type dashboardIoUpdateNotificationsIgnored struct {
	ignored bool
	err     error
}

type dashboardIoUpdateQuotaPreferencesSaved struct{ err error }

type dashboardIoUpdateChangeWorkspace struct {
	sessionID     string
	workspaceName string
	err           error
}

type dashboardIoUpdateMoveOriginPrepared struct {
	sessionID       string
	moveOperationID string
	operation       *atomic.Bool
}

type dashboardIoUpdateStartupPromptOutcomes struct {
	sessionID string
	outcomes  map[string]client.StartupPromptOutcome
	err       error
}

// dashboardIoUpdateStartupPromptQueued is a prompt typed into a standby
// composer and handed to the daemon for delivery once the session is live.
// (Rust attaches this comment to MoveOriginPrepared.)
type dashboardIoUpdateStartupPromptQueued struct {
	sessionID            string
	prompt               chat.StartupPrompt
	err                  error
	definitelyUnaccepted bool
}

// dashboardIoUpdateStartupPromptWithdrawn is the daemon's answer to taking
// a recalled prompt back: withdrawn true when it is withdrawn, false when it
// had already been sent.
type dashboardIoUpdateStartupPromptWithdrawn struct {
	sessionID string
	prompt    chat.StartupPrompt
	withdrawn bool
	err       error
}

// dashboardIoUpdateContainerSettings carries Rust's
// `Result<Result<DashboardMetadata, ContainerUpgradePreview>, String>`:
// upgradePreview is set when the daemon answered with the inner Err,
// metadata otherwise.
type dashboardIoUpdateContainerSettings struct {
	sessionID      string
	metadata       dashboardMetadata
	upgradePreview *core.ContainerUpgradePreview
	err            error
}

// dashboardIoUpdateProjectDirectory is whether a configured project's local
// directory exists; nil when the check could not tell.
type dashboardIoUpdateProjectDirectory struct {
	path   string
	exists *bool
}

type dashboardIoUpdateProjectCatalog struct {
	context *string
	catalog core.ProjectCatalogView
	err     error
}

type dashboardIoUpdateMountHistory struct {
	context *string
	history map[string][]string
	err     error
}

type dashboardIoUpdateTargetTest struct {
	targetID string
	err      error
}

type dashboardIoUpdateConfigRename struct {
	what     string
	metadata dashboardMetadata
	err      error
}

// dashboardIoUpdateQuotaRefreshRequested is the daemon's answer to a
// Refresh: it accepted the request, or it could not be reached.
type dashboardIoUpdateQuotaRefreshRequested struct{ err error }

type dashboardIoUpdateCapacityRefreshRequested struct{ err error }

// dashboardIoUpdateQuotaProbeRequested is the daemon's answer to a single
// profile's Refresh: it accepted the probe, or it could not be reached.
type dashboardIoUpdateQuotaProbeRequested struct {
	profileID string
	err       error
}

// dashboardIoUpdateGitIdentityChecked is the host's global Git author
// identity, read before the first new session of this run.
type dashboardIoUpdateGitIdentityChecked struct {
	identity client.GitIdentity
	err      error
}

// dashboardIoUpdateGitIdentitySaved is the host's global Git author
// identity after the dialog saved it.
type dashboardIoUpdateGitIdentitySaved struct {
	identity client.GitIdentity
	err      error
}

type dashboardIoUpdateWebAccess struct {
	generation uint64
	access     tui.WebViewerAccess
}

// dashboardIoUpdateResumeCandidates is the resume dialog's stopped
// sessions. The discovery id is the dialog's; an answer for one that has
// closed is dropped there.
type dashboardIoUpdateResumeCandidates struct {
	discoveryID uint64
	candidates  client.ResumeCandidates
	err         error
}

// dashboardIoUpdateResumeRecord is the record of the row picked in the
// resume dialog; nil when the daemon has none.
type dashboardIoUpdateResumeRecord struct {
	sessionID string
	record    *client.SessionRecordWithVerdict
	err       error
}

// dashboardIoUpdateWikiSync is one SessionWiki sync status. The episode is
// the dialog's; an answer for an older one is dropped there.
type dashboardIoUpdateWikiSync struct {
	dialog  bool
	episode uint64
	status  client.WikiSyncStatus
	err     error
}

// dashboardIoUpdateWikiRows is one SessionWiki search result. The request
// id is the dialog's; an answer for an older one is dropped there.
type dashboardIoUpdateWikiRows struct {
	requestID uint64
	page      client.WikiSearchPage
	err       error
}

type dashboardIoUpdateResumeTextMatches struct {
	requestID uint64
	page      client.SessionTextSearchPage
	err       error
}

// dashboardIoUpdateSessionTextMatches is the daemon's answer to a Sessions
// filter search of conversation text. The request id is the dashboard's; an
// answer for an older one is dropped there.
type dashboardIoUpdateSessionTextMatches struct {
	requestID uint64
	page      client.SessionTextSearchPage
	err       error
}

// dashboardIoUpdateWikiBrief is one archived session's briefing, for the
// resume dialog's preview.
type dashboardIoUpdateWikiBrief struct {
	wikiID string
	brief  string
	err    error
}

// dashboardIoUpdateWikiHits is one archived session's matching passages,
// for the resume dialog's preview while a query is active; nil when none.
type dashboardIoUpdateWikiHits struct {
	wikiID string
	query  string
	hits   *client.WikiHitTranscript
	err    error
}

type dashboardIoUpdateWebListeners struct {
	generation uint64
	processes  []tui.WebListenerProcess
	err        error
}

type dashboardIoUpdateWebAccessError struct {
	generation uint64
	message    string
}

type dashboardIoUpdateFirstRunStarted struct{}

type dashboardIoUpdateFirstRunConfigured struct {
	report client.FirstRunSetupReport
}

// dashboardIoUpdateFirstRunChecked carries Rust's
// `Result<Option<Vec<String>>, String>`: checked is false when setup was not
// due (None), and problems are the prerequisite failures otherwise.
type dashboardIoUpdateFirstRunChecked struct {
	checked  bool
	problems []string
	err      error
}

type dashboardIoUpdateSetupSaved struct {
	generation   uint64
	config       core.Config
	err          error
	loginProfile *string
}

type dashboardIoUpdateSetupDiscovered struct {
	generation uint64
	detection  tui.SetupDetection
	err        error
}

// dashboardIoUpdateProfileLoginStatuses is saved-profile sign-in statuses
// read before Settings draws them.
type dashboardIoUpdateProfileLoginStatuses struct {
	generation uint64
	statuses   map[string]core.LoginStatus
	err        error
}

type dashboardIoUpdateAvailabilityCheckRequested struct{ err error }

// dashboardIoUpdateBuildCachePreviewed: key is the serde_json::Value the
// tui hands out (tui.DashboardState.BuildCachePreviewed takes it as any).
type dashboardIoUpdateBuildCachePreviewed struct {
	generation          uint64
	key                 any
	preview             *core.BuildCachePreview
	err                 error
	installMbxAvailable bool
}

type dashboardIoUpdateMbxInstalled struct {
	generation uint64
	key        any
	machineID  string
	// installed and err are Rust's `result`; preview and previewErr its
	// `preview`.
	installed           string
	err                 error
	preview             *core.BuildCachePreview
	previewErr          error
	installMbxAvailable bool
}

type dashboardIoUpdateArchiveSpacePreviewed struct {
	generation    uint64
	olderThanDays *uint32
	preview       core.ArchiveSpacePreview
	err           error
}

type dashboardIoUpdateSpinnerStyleSaved struct {
	config core.Config
	err    error
}

type dashboardIoUpdateTerminalDraft struct {
	sessionID string
	revision  uint64
	err       error
}

type dashboardIoUpdateReadReceipt struct {
	sessionID string
	through   uint64
	err       error
}

type dashboardIoUpdateCreatedBundle struct {
	created createdBundleUpdate
	err     error
}

type dashboardIoUpdateRemovedBundle struct {
	bundleID string
	err      error
}

type dashboardIoUpdateImportedSessionApplied struct {
	applied importedDashboardSessionApply
	err     error
}

type dashboardIoUpdateLifecycleReloaded struct {
	reloaded *lifecycleReloaded
}

type dashboardIoUpdateLifecycleCancellation struct {
	sessionID string
	err       error
}

type dashboardIoUpdateContainerUpgradePreview struct {
	sessionID   string
	upgradeOnly bool
	preview     core.ContainerUpgradePreview
	err         error
}

type dashboardIoUpdateSuspendPreflight struct {
	sessionID string
	preflight core.SuspendPreflight
	err       error
}

type dashboardIoUpdateContainerUpgradeQueued struct{ err error }

type dashboardIoUpdateMovePrepared struct {
	sessionID   string
	requestID   uint64
	preparation core.MovePreparation
	err         error
}

// dashboardIoUpdateCheckpointArchiveSizes: sizes carries Rust's
// `BTreeMap<String, Result<Option<u64>, String>>` as the daemon's
// CheckpointArchiveSizes reply does.
type dashboardIoUpdateCheckpointArchiveSizes struct {
	generation uint64
	targets    map[string]string
	sizes      map[string]client.DaemonResult[*uint64]
}

type dashboardIoUpdateWorkerDiagnosis struct {
	sessionID string
	episodeID uint64
	// diagnosis is nil when the worker had nothing to add.
	diagnosis *string
	err       error
}

type dashboardIoUpdateProjectDiscovery struct {
	context   string
	discovery core.ProjectDiscovery
	err       error
}

type dashboardIoUpdatePathCompletions struct {
	context    string
	prefix     string
	completion core.PathCompletion
	err        error
}

type dashboardIoUpdateMountValidation struct {
	context  string
	source   string
	resolved string
	forced   *string
	err      error
}

// dashboardIoUpdateSessionMountValidation: failure is Rust's
// `Option<(String, String)>`, the mount source (First) and why it failed
// (Second); nil when every mount passed.
type dashboardIoUpdateSessionMountValidation struct {
	generation uint64
	launch     tui.DashboardAction
	failure    *client.DaemonPair[string, string]
	err        error
}

type dashboardIoUpdateResumeRepositoryPreflight struct {
	generation            uint64
	launch                tui.DashboardAction
	submittedRepositoryID *string
	apply                 resumeRepositoryPreflightApply
	err                   error
}

type dashboardIoUpdateContainerPathResolved struct {
	context string
	path    string
	err     error
}

type dashboardIoUpdateSetupPathResolved struct {
	generation uint64
	draft      any
	path       []string
	value      string
	resolved   string
	err        error
}

// dashboardIoUpdateProjectValidation carries Rust's `Result<(PathBuf,
// ManagedWorktreeOptions, bool), String>` as
// tui.DashboardState.ApplyResolvedProjectDirectory takes it.
type dashboardIoUpdateProjectValidation struct {
	context        string
	directory      string
	resolved       string
	options        core.ManagedWorktreeOptions
	reviewTurnsOff bool
	err            error
}

// dashboardIoUpdateClipboardText: clipboard providers may use a blocking
// desktop IPC call. The result is delivered here after that work finishes
// off the loop.
type dashboardIoUpdateClipboardText struct {
	target tui.DashboardPasteTarget
	text   string
	err    error
}

// dashboardIoUpdateClipboardWritten: a copied selection reached the desktop
// clipboard, or did not. Only a failure needs reporting: the notice for a
// successful copy is already on screen.
type dashboardIoUpdateClipboardWritten struct{ err error }

func (dashboardIoUpdateRuntimeConfirmationMissing) isDashboardIoUpdate()    {}
func (dashboardIoUpdateOperatorAway) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateProfileHydration) isDashboardIoUpdate()              {}
func (dashboardIoUpdateSessionUsageFinished) isDashboardIoUpdate()          {}
func (dashboardIoUpdateRepositoryRemotesRepaired) isDashboardIoUpdate()     {}
func (dashboardIoUpdateWorkspaceClosed) isDashboardIoUpdate()               {}
func (dashboardIoUpdateWorkspaceCloseCancelled) isDashboardIoUpdate()       {}
func (dashboardIoUpdateWorkspaceManagement) isDashboardIoUpdate()           {}
func (dashboardIoUpdateWorkspacePaneSizes) isDashboardIoUpdate()            {}
func (dashboardIoUpdateWorkspaceLayouts) isDashboardIoUpdate()              {}
func (dashboardIoUpdateMaterializedSessionProjection) isDashboardIoUpdate() {}
func (dashboardIoUpdateNotificationDismissed) isDashboardIoUpdate()         {}
func (dashboardIoUpdateNotificationsDismissed) isDashboardIoUpdate()        {}
func (dashboardIoUpdateTranscriptTailSeed) isDashboardIoUpdate()            {}
func (dashboardIoUpdateNotificationMessageRead) isDashboardIoUpdate()       {}
func (dashboardIoUpdateStoredSessionSummary) isDashboardIoUpdate()          {}
func (dashboardIoUpdateStoppedSubagentTranscript) isDashboardIoUpdate()     {}
func (dashboardIoUpdateProjectSource) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateChatOpened) isDashboardIoUpdate()                    {}
func (dashboardIoUpdateNativeAgentHistory) isDashboardIoUpdate()            {}
func (dashboardIoUpdateNativeAgentStopped) isDashboardIoUpdate()            {}
func (dashboardIoUpdateInterruptAllFinished) isDashboardIoUpdate()          {}
func (dashboardIoUpdateReviewRefused) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateCreateSession) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateDaemonRestarted) isDashboardIoUpdate()               {}
func (dashboardIoUpdateGoSelectionSaved) isDashboardIoUpdate()              {}
func (dashboardIoUpdateGoContext) isDashboardIoUpdate()                     {}
func (dashboardIoUpdateGitStatus) isDashboardIoUpdate()                     {}
func (dashboardIoUpdateGoPrepared) isDashboardIoUpdate()                    {}
func (dashboardIoUpdateRemotePreflight) isDashboardIoUpdate()               {}
func (dashboardIoUpdateRenameSession) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateNotificationsIgnored) isDashboardIoUpdate()          {}
func (dashboardIoUpdateQuotaPreferencesSaved) isDashboardIoUpdate()         {}
func (dashboardIoUpdateChangeWorkspace) isDashboardIoUpdate()               {}
func (dashboardIoUpdateMoveOriginPrepared) isDashboardIoUpdate()            {}
func (dashboardIoUpdateStartupPromptOutcomes) isDashboardIoUpdate()         {}
func (dashboardIoUpdateStartupPromptQueued) isDashboardIoUpdate()           {}
func (dashboardIoUpdateStartupPromptWithdrawn) isDashboardIoUpdate()        {}
func (dashboardIoUpdateContainerSettings) isDashboardIoUpdate()             {}
func (dashboardIoUpdateProjectDirectory) isDashboardIoUpdate()              {}
func (dashboardIoUpdateProjectCatalog) isDashboardIoUpdate()                {}
func (dashboardIoUpdateMountHistory) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateTargetTest) isDashboardIoUpdate()                    {}
func (dashboardIoUpdateConfigRename) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateQuotaRefreshRequested) isDashboardIoUpdate()         {}
func (dashboardIoUpdateCapacityRefreshRequested) isDashboardIoUpdate()      {}
func (dashboardIoUpdateQuotaProbeRequested) isDashboardIoUpdate()           {}
func (dashboardIoUpdateGitIdentityChecked) isDashboardIoUpdate()            {}
func (dashboardIoUpdateGitIdentitySaved) isDashboardIoUpdate()              {}
func (dashboardIoUpdateWebAccess) isDashboardIoUpdate()                     {}
func (dashboardIoUpdateResumeCandidates) isDashboardIoUpdate()              {}
func (dashboardIoUpdateResumeRecord) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateWikiSync) isDashboardIoUpdate()                      {}
func (dashboardIoUpdateWikiRows) isDashboardIoUpdate()                      {}
func (dashboardIoUpdateResumeTextMatches) isDashboardIoUpdate()             {}
func (dashboardIoUpdateSessionTextMatches) isDashboardIoUpdate()            {}
func (dashboardIoUpdateWikiBrief) isDashboardIoUpdate()                     {}
func (dashboardIoUpdateWikiHits) isDashboardIoUpdate()                      {}
func (dashboardIoUpdateWebListeners) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateWebAccessError) isDashboardIoUpdate()                {}
func (dashboardIoUpdateFirstRunStarted) isDashboardIoUpdate()               {}
func (dashboardIoUpdateFirstRunConfigured) isDashboardIoUpdate()            {}
func (dashboardIoUpdateFirstRunChecked) isDashboardIoUpdate()               {}
func (dashboardIoUpdateSetupSaved) isDashboardIoUpdate()                    {}
func (dashboardIoUpdateSetupDiscovered) isDashboardIoUpdate()               {}
func (dashboardIoUpdateProfileLoginStatuses) isDashboardIoUpdate()          {}
func (dashboardIoUpdateAvailabilityCheckRequested) isDashboardIoUpdate()    {}
func (dashboardIoUpdateBuildCachePreviewed) isDashboardIoUpdate()           {}
func (dashboardIoUpdateMbxInstalled) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateArchiveSpacePreviewed) isDashboardIoUpdate()         {}
func (dashboardIoUpdateSpinnerStyleSaved) isDashboardIoUpdate()             {}
func (dashboardIoUpdateTerminalDraft) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateReadReceipt) isDashboardIoUpdate()                   {}
func (dashboardIoUpdateCreatedBundle) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateRemovedBundle) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateImportedSessionApplied) isDashboardIoUpdate()        {}
func (dashboardIoUpdateLifecycleReloaded) isDashboardIoUpdate()             {}
func (dashboardIoUpdateLifecycleCancellation) isDashboardIoUpdate()         {}
func (dashboardIoUpdateContainerUpgradePreview) isDashboardIoUpdate()       {}
func (dashboardIoUpdateSuspendPreflight) isDashboardIoUpdate()              {}
func (dashboardIoUpdateContainerUpgradeQueued) isDashboardIoUpdate()        {}
func (dashboardIoUpdateMovePrepared) isDashboardIoUpdate()                  {}
func (dashboardIoUpdateCheckpointArchiveSizes) isDashboardIoUpdate()        {}
func (dashboardIoUpdateWorkerDiagnosis) isDashboardIoUpdate()               {}
func (dashboardIoUpdateProjectDiscovery) isDashboardIoUpdate()              {}
func (dashboardIoUpdatePathCompletions) isDashboardIoUpdate()               {}
func (dashboardIoUpdateMountValidation) isDashboardIoUpdate()               {}
func (dashboardIoUpdateSessionMountValidation) isDashboardIoUpdate()        {}
func (dashboardIoUpdateResumeRepositoryPreflight) isDashboardIoUpdate()     {}
func (dashboardIoUpdateContainerPathResolved) isDashboardIoUpdate()         {}
func (dashboardIoUpdateSetupPathResolved) isDashboardIoUpdate()             {}
func (dashboardIoUpdateProjectValidation) isDashboardIoUpdate()             {}
func (dashboardIoUpdateClipboardText) isDashboardIoUpdate()                 {}
func (dashboardIoUpdateClipboardWritten) isDashboardIoUpdate()              {}

// activeLifecycleOperation is a lifecycle operation this dashboard started
// and is still waiting on.
type activeLifecycleOperation struct {
	retirement *chatRetirement
	cancelled  *atomic.Bool
	kind       tui.SessionOperationKind
	// retryLaunch is nil when the operation has no launch to retry.
	retryLaunch tui.DashboardAction
	// noticeName is how notices named the session when the operation
	// began. The completion notice falls back to it when the record is
	// already gone.
	noticeName string
}

type workspaceManagementResult struct {
	entries            []tui.WorkspaceManagementEntry
	selectWorkspace    *string
	deletedWorkspaceID *string
	// detailsPending: the entries carry the workspace list ahead of their
	// drafts: the dialog shows them at once and keeps mutations off until
	// the details answer.
	detailsPending bool
}

type registeredDashboardSession struct {
	retryLaunch             tui.DashboardAction
	session                 core.SessionRecord
	rememberedContainerSize *client.DaemonPair[string, core.HostContainerSize]
	cancelled               *atomic.Bool
}

type dashboardCreateSessionUpdate interface {
	isDashboardCreateSessionUpdate()
}

type dashboardCreateSessionUpdateRemoteRepair struct {
	bundleID string
	repairs  []core.LocalRemoteRepair
	retry    tui.DashboardAction
}

type dashboardCreateSessionUpdateRegistered struct {
	registered *registeredDashboardSession
}

type dashboardCreateSessionUpdateFailed struct {
	message     string
	retryLaunch tui.DashboardAction
}

func (dashboardCreateSessionUpdateRemoteRepair) isDashboardCreateSessionUpdate() {}
func (dashboardCreateSessionUpdateRegistered) isDashboardCreateSessionUpdate()   {}
func (dashboardCreateSessionUpdateFailed) isDashboardCreateSessionUpdate()       {}

type remotePreflightOutcome interface{ isRemotePreflightOutcome() }

type remotePreflightOutcomeReady struct {
	repositories []tui.RemoteRepositoryPreview
}

type remotePreflightOutcomeRepair struct {
	repairs []core.LocalRemoteRepair
}

func (remotePreflightOutcomeReady) isRemotePreflightOutcome()  {}
func (remotePreflightOutcomeRepair) isRemotePreflightOutcome() {}

type importedDashboardSessionApply struct {
	verdict         client.SessionVerdict
	harness         string
	nativeSessionID string
	session         core.SessionRecord
	bundle          core.ProjectBundle
}

type createdBundleUpdate struct {
	bundleID string
	bundle   core.ProjectBundle
}

type resumeRepositoryPreflightApply struct {
	preflight core.ResumeRepositorySourcePreflight
}

type lifecycleReload struct {
	update dashboardLifecycleUpdate
	// operation is nil when no local operation was waiting.
	operation *activeLifecycleOperation
}

type lifecycleReloaded struct {
	reload   lifecycleReload
	metadata dashboardMetadata
	err      error
}

// remotePreflightOutcomeFrom is Rust's `From<PreflightNew> for
// RemotePreflightOutcome`.
func remotePreflightOutcomeFrom(result client.PreflightNew) remotePreflightOutcome {
	if len(result.RemoteRepairs) != 0 {
		return remotePreflightOutcomeRepair{repairs: result.RemoteRepairs}
	}
	repositories := make([]tui.RemoteRepositoryPreview, 0, len(result.RemoteRepositories))
	for _, repository := range result.RemoteRepositories {
		repositories = append(repositories, tui.RemoteRepositoryPreview{
			RepositoryID:  repository.ID,
			FetchURL:      repository.FetchURL,
			DefaultBranch: repository.DefaultBranch,
			PushURLs:      repository.PushURLs,
		})
	}
	return remotePreflightOutcomeReady{repositories: repositories}
}

// dashboardMetadata is the preferences absent from the runtime feed. A
// background completion cannot carry config or session records back into the
// dashboard's authoritative view.
type dashboardMetadata struct {
	mountHistory   core.SnapshotMap[string, []string]
	containerSizes core.SnapshotMap[string, core.HostContainerSize]
}

// dashboardMetadataFrom is Rust's `From<StoredSessionMetadata> for
// DashboardMetadata`.
func dashboardMetadataFrom(stored client.StoredSessionMetadata) dashboardMetadata {
	return dashboardMetadata{
		mountHistory:   stored.MountHistory,
		containerSizes: stored.ContainerSizes,
	}
}

// storedSessionMetadata is the store's session metadata, read by the
// daemon, which alone opens the store.
func storedSessionMetadata(ctx context.Context) (dashboardMetadata, error) {
	daemonClient, err := ConnectOrStart(ctx)
	if err != nil {
		return dashboardMetadata{}, err
	}
	defer daemonClient.Close()
	stored, err := daemonClient.StoredSessionMetadata(ctx)
	if err != nil {
		return dashboardMetadata{}, err
	}
	return dashboardMetadataFrom(stored), nil
}

func (m dashboardMetadata) apply(state *core.StateState) {
	state.MountHistory = m.mountHistory
	state.ContainerSizes = m.containerSizes
}

// pendingRuntimeUpdate: completed mutations wait for their referenced
// objects in the runtime feed. They never install the independent snapshots
// used to perform the mutation.
type pendingRuntimeUpdate struct {
	update   dashboardIoUpdate
	deadline time.Time
}

// runtimeConfirmationMissing is the failure a reference-dependent result
// reports when the runtime feed has not confirmed it by its deadline.
const runtimeConfirmationMissing = "The operation was saved, but the runtime feed has not confirmed it. Reconnect to refresh the view before retrying"

// awaitingRuntime is Rust's `DashboardIoUpdate::awaiting_runtime`: whether
// update refers to objects the runtime feed has not delivered yet.
func awaitingRuntime(update dashboardIoUpdate, ctrl *controller.Controller) bool {
	panic("unported: mj-cli/src/dashboard/io.rs awaiting_runtime")
}

// runtimeWaitExpired is Rust's `DashboardIoUpdate::runtime_wait_expired`:
// the failure that replaces a reference-dependent result whose objects did
// not arrive in time.
func runtimeWaitExpired(update dashboardIoUpdate) dashboardIoUpdate {
	err := errors.New(runtimeConfirmationMissing)
	switch update := update.(type) {
	case dashboardIoUpdateCreatedBundle:
		return dashboardIoUpdateCreatedBundle{err: err}
	case dashboardIoUpdateRemovedBundle:
		return dashboardIoUpdateRemovedBundle{bundleID: update.bundleID, err: err}
	case dashboardIoUpdateImportedSessionApplied:
		return dashboardIoUpdateImportedSessionApplied{err: err}
	case dashboardIoUpdateCreateSession:
		return dashboardIoUpdateRuntimeConfirmationMissing{message: runtimeConfirmationMissing}
	}
	panic("only reference-dependent results wait for runtime state")
}

// sessionNoticeName is how a notice names a session: the title the session
// list shows, or the short id when the session has no title or its record
// is gone (launch findings B-3 and R5-5).
func (c *dashboardContext) sessionNoticeName(sessionID string) string {
	panic("unported: mj-cli/src/dashboard/io.rs DashboardContext::session_notice_name")
}

func (c *dashboardContext) applyControllerMetadata(metadata dashboardMetadata) {
	panic("unported: mj-cli/src/dashboard/io.rs apply_controller_metadata")
}

// applyDashboardIoUpdate folds one finished background job into dashboard
// and controller state.
func (c *dashboardContext) applyDashboardIoUpdate(update dashboardIoUpdate) {
	panic("unported: mj-cli/src/dashboard/io.rs apply_dashboard_io_update")
}

func (c *dashboardContext) loadResumePreview() {
	panic("unported: mj-cli/src/dashboard/io.rs load_resume_preview")
}

func (c *dashboardContext) applyCreateSessionUpdate(update dashboardCreateSessionUpdate) {
	panic("unported: mj-cli/src/dashboard/io.rs apply_create_session_update")
}

func (c *dashboardContext) applyLifecycleReloaded(reloaded *lifecycleReloaded) {
	panic("unported: mj-cli/src/dashboard/io.rs apply_lifecycle_reloaded")
}

func (c *dashboardContext) applyWorkerDiagnosis(sessionID string, episodeID uint64, diagnosis *string, err error) {
	panic("unported: mj-cli/src/dashboard/io.rs apply_worker_diagnosis")
}

// sessionNoticeName is how a notice names a session; see
// core.StateState.SessionNoticeName.
func sessionNoticeName(state *core.StateState, sessionID string) string {
	// A session named by its initial prompt can have a title of many
	// paragraphs; a notice names it by the start, on one line.
	return tui.FitSessionName(state.SessionNoticeName(sessionID), tui.NoticeNameCells)
}

// lifecycleNoticeName is how a lifecycle's completion notice names its
// session. The daemon's runtime snapshot can drop a destroyed session's
// record before the lifecycle reload lands, so when the record is gone the
// name taken when the operation began stands in (launch finding R5-4).
func lifecycleNoticeName(state *core.StateState, sessionID string, takenAtStart *string) string {
	if takenAtStart != nil && !state.Sessions.ContainsKey(sessionID) {
		return tui.FitSessionName(*takenAtStart, tui.NoticeNameCells)
	}
	return sessionNoticeName(state, sessionID)
}
