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
	"fmt"
	"log/slog"
	"maps"
	"reflect"
	"slices"
	"strings"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/daemon"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
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
	switch update := update.(type) {
	case dashboardIoUpdateCreatedBundle:
		if update.err != nil {
			return false
		}
		bundle, ok := ctrl.Config.Bundles[update.created.bundleID]
		return !ok || !projectBundleEqual(bundle, update.created.bundle)
	case dashboardIoUpdateRemovedBundle:
		_, present := ctrl.Config.Bundles[update.bundleID]
		return update.err == nil && present
	// An imported session is stopped, and the feed carries only live
	// sessions; only the project it may have added arrives there.
	case dashboardIoUpdateImportedSessionApplied:
		if update.err != nil {
			return false
		}
		bundle, ok := ctrl.Config.Bundles[update.applied.session.BundleID]
		return !ok || !projectBundleEqual(bundle, update.applied.bundle)
	case dashboardIoUpdateCreateSession:
		registered, ok := update.update.(dashboardCreateSessionUpdateRegistered)
		return ok && !ctrl.State.Sessions.ContainsKey(registered.registered.session.ID)
	}
	return false
}

// projectBundleEqual is ProjectBundle's derived PartialEq.
func projectBundleEqual(a, b core.ProjectBundle) bool {
	return a.PrimaryRepo == b.PrimaryRepo &&
		slices.EqualFunc(a.Repositories, b.Repositories, func(left, right core.ProjectRepository) bool {
			return reflect.DeepEqual(left, right)
		})
}

// daemonResultOf is Rust's `Result<T, String>` for a value and its error,
// as the tui setters that store one take it.
func daemonResultOf[T any](value T, err error) client.DaemonResult[T] {
	if err != nil {
		return client.DaemonErr[T](err.Error())
	}
	return client.DaemonOk(value)
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
	return sessionNoticeName(&c.controller.State, sessionID)
}

func (c *dashboardContext) applyControllerMetadata(metadata dashboardMetadata) {
	metadata.apply(&c.controller.State)
}

// applyDashboardIoUpdate folds one finished background job into dashboard
// and controller state.
func (c *dashboardContext) applyDashboardIoUpdate(update dashboardIoUpdate) {
	var projectContext **string
	switch update := update.(type) {
	case dashboardIoUpdateProjectCatalog:
		projectContext = &update.context
	case dashboardIoUpdateMountHistory:
		projectContext = &update.context
	}
	if projectContext != nil {
		current, ok := c.dashboard.ProjectCatalogContext()
		if (*projectContext == nil) != !ok || (ok && **projectContext != current) {
			return
		}
	}
	if awaitingRuntime(update, &c.controller) {
		c.dashboard.SetNotice("Saved; waiting for the runtime view…")
		c.pendingRuntimeUpdates = append(c.pendingRuntimeUpdates, pendingRuntimeUpdate{
			update:   update,
			deadline: time.Now().Add(saveAckTimeout),
		})
		return
	}
	switch update := update.(type) {
	case dashboardIoUpdateOperatorAway:
		c.dashboard.OperatorAwayRequestFinished(update.away, update.revision, update.err)
	case dashboardIoUpdateRuntimeConfirmationMissing:
		c.dashboard.SetFailureNotice(update.message)
	case dashboardIoUpdateSessionUsageFinished:
		c.dashboard.ApplySessionUsageResult(update.generation, update.report, update.err)
	case dashboardIoUpdateWorkspacePaneSizes:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not load workspace pane sizes: %s", update.err.Error()))
			return
		}
		for _, workspaceID := range slices.Sorted(maps.Keys(update.sizes)) {
			if _, known := c.knownWorkspaceLayouts[workspaceID]; !known {
				continue
			}
			sizes := update.sizes[workspaceID]
			c.dashboard.CacheWorkspacePaneSizes(workspaceID, sizes)
			c.paneSizePersistence.remember(workspaceID, sizes)
		}
	case dashboardIoUpdateWorkspaceLayouts:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not load workspace layouts: %s", update.err.Error()))
			return
		}
		for _, workspaceID := range slices.Sorted(maps.Keys(update.layouts)) {
			if _, known := c.knownWorkspaceLayouts[workspaceID]; !known {
				continue
			}
			layout := update.layouts[workspaceID]
			c.workspaceLayouts[workspaceID] = layout
			c.dashboard.CacheWorkspaceLayout(workspaceID, layout)
			c.layoutPersistence.remember(workspaceID, layout)
		}
	case dashboardIoUpdateWorkspaceClosed:
		generation := update.generation
		if finished, ok := c.dashboard.WorkspaceCloseFinished(update.workspaceID); ok {
			generation = finished
		}
		deleted := update.err == nil
		management := dashboardIoUpdateWorkspaceManagement{generation: generation, err: update.err}
		if deleted {
			deletedWorkspaceID := update.workspaceID
			management.result = workspaceManagementResult{
				entries:            c.dashboard.WorkspaceEntriesWithout(update.workspaceID),
				deletedWorkspaceID: &deletedWorkspaceID,
			}
		}
		c.applyDashboardIoUpdate(management)
		if deleted {
			c.dashboard.SetNotice("Workspace deleted.")
		}
	case dashboardIoUpdateWorkspaceCloseCancelled:
		if update.err == nil {
			c.dashboard.SetNotice("Workspace deletion cancellation requested; completed suspensions cannot be undone.")
		} else {
			c.dashboard.SetNotice(fmt.Sprintf("Could not cancel workspace close: %s", update.err.Error()))
		}
	case dashboardIoUpdateWorkspaceManagement:
		c.applyWorkspaceManagement(update)
	case dashboardIoUpdateNativeAgentHistory:
		c.dashboard.NativeAgentHistoryLoaded(update.owner, update.child, update.page, update.err)
	case dashboardIoUpdateNativeAgentStopped:
		c.dashboard.NativeAgentStopFinished(update.owner, update.child, update.err)
	case dashboardIoUpdateInterruptAllFinished:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not interrupt all: %s", update.err.Error()))
			return
		}
		failures := make([]tui.InterruptFailure, 0, len(update.result.Failures))
		for _, failure := range update.result.Failures {
			failures = append(failures, tui.InterruptFailure{SessionID: failure.First, Error: failure.Second})
		}
		c.dashboard.InterruptAllFinished(int(update.result.Requested), failures)
	case dashboardIoUpdateReviewRefused:
		if conversation, ok := c.chats[update.sessionID]; ok {
			conversation.ReportReviewRefusal(update.message)
		} else {
			// The chat moved on; the refusal still belongs on screen.
			c.dashboard.SetNotice(update.message)
		}
	case dashboardIoUpdateMaterializedSessionProjection:
		c.finishMaterializedProjection(update.sessionID, update.detail, update.err)
	case dashboardIoUpdateNotificationDismissed:
		c.dashboard.NotificationDismissalFinished(update.sessionID, update.target, update.ordinal, update.err)
	case dashboardIoUpdateNotificationsDismissed:
		results := make([]tui.NotificationDismissalResult, 0, len(update.results))
		for _, result := range update.results {
			results = append(results, tui.NotificationDismissalResult{
				SessionID: result.sessionID,
				Target:    result.target,
				Ordinal:   result.ordinal,
				Err:       result.err,
			})
		}
		c.dashboard.NotificationsDismissed(results)
	case dashboardIoUpdateTranscriptTailSeed:
		var viewedThroughEventOrdinal uint64
		if session := c.controller.State.Sessions.GetShared(update.materialized.SessionID); session != nil {
			viewedThroughEventOrdinal = session.ViewedThroughEventOrdinal
		}
		c.requestMaterializedProjection(materializedProjectionFromSession(*update.materialized), viewedThroughEventOrdinal)
	case dashboardIoUpdateStoppedSubagentTranscript:
		c.dashboard.SetStoppedSubagentTranscript(update.sessionID, update.session, update.err)
	case dashboardIoUpdateNotificationMessageRead:
		if !c.summaryReads.finishItem(update.request, update.generation) {
			return
		}
		var text *string
		if update.err == nil {
			text = update.text
		}
		if c.dashboard.NotificationMessageReadFinished(&update.request, text) && update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not read notification message: %s", update.err.Error()))
		}
	case dashboardIoUpdateStoredSessionSummary:
		if !c.summaryReads.finish(update.sessionID, update.generation) {
			return
		}
		if update.err == nil {
			c.dashboard.ApplyPreparedMaterializedSessionSummary(update.summary)
		} else {
			slog.Warn("could not restore stored session summary: "+update.err.Error(), "session_id", update.sessionID)
		}
		// Either way this session has answered, so the startup pick is one
		// summary closer to being able to choose.
		c.finishStartupSummary(update.sessionID)
	case dashboardIoUpdateProjectSource:
		delete(c.projectSourcesInFlight, update.sessionID)
		if update.err == nil {
			c.dashboard.SetProjectSource(update.sessionID, update.source)
		} else {
			slog.Warn("could not resolve canonical project source: "+update.err.Error(), "session_id", update.sessionID)
		}
	case dashboardIoUpdateChatOpened:
		c.applyChatOpened(update)
	case dashboardIoUpdateDaemonRestarted:
		if update.err == nil {
			c.dashboard.SetNotice(update.sentence)
		} else {
			c.dashboard.SetFailureNotice(update.err.Error())
		}
	case dashboardIoUpdateGoSelectionSaved:
		c.goSelectionInFlight = false
		if update.err != nil {
			c.dashboard.SetFailureNotice(fmt.Sprintf("Could not remember this conversation: %s", update.err.Error()))
		}
	case dashboardIoUpdateGitStatus:
		delete(c.gitProbesInFlight, update.sessionID)
		c.dashboard.SetGitStatus(update.sessionID, update.result)
	case dashboardIoUpdateGoContext:
		c.goContextInFlight = false
		c.dashboard.SetGoContext(update.sessionID, update.result)
	case dashboardIoUpdateGoPrepared:
		if update.err == nil {
			action := c.dashboard.GoLaunchAction(update.workspaceID, update.recipe)
			startSessionLaunch(c, action)
			return
		}
		if active, ok := c.dashboard.ActiveWorkspaceID(); ok && active == update.workspaceID {
			c.dashboard.ShowLaunchFailure(update.err.Error(), update.retry)
		} else {
			c.dashboard.SetFailureNotice(fmt.Sprintf(
				"Session preparation failed in workspace %s: %s. Return to that workspace to retry.",
				update.workspaceID, update.err.Error(),
			))
		}
	case dashboardIoUpdateCreateSession:
		c.applyCreateSessionUpdate(update.update)
	case dashboardIoUpdateRemotePreflight:
		if update.generation != c.dashboard.SessionPreflightGeneration() {
			return
		}
		if update.err != nil {
			c.dashboard.ApplyRemoteSessionPreflight(update.generation, nil, update.err)
			return
		}
		switch outcome := update.outcome.(type) {
		case remotePreflightOutcomeRepair:
			c.dashboard.ApplyRemoteSessionPreflight(update.generation, nil, errors.New("Git tracking needs repair"))
			if create, ok := update.launch.(tui.DashboardActionCreateSession); ok {
				c.dashboard.ShowRemoteRepairConfirmation(
					create.BundleID,
					outcome.repairs,
					tui.DashboardActionPreflightCreateSession{Launch: update.launch},
				)
			}
		case remotePreflightOutcomeReady:
			c.dashboard.ApplyRemoteSessionPreflight(update.generation, outcome.repositories, nil)
		}
	case dashboardIoUpdateRepositoryRemotesRepaired:
		if update.generation != nil && *update.generation != c.dashboard.SessionPreflightGeneration() {
			return
		}
		c.dashboard.ClearNotice()
		if update.err != nil {
			if update.generation != nil {
				c.dashboard.ApplyRemoteSessionPreflight(*update.generation, nil, update.err)
			}
			c.dashboard.ShowLaunchFailure(update.err.Error(), update.retry)
			return
		}
		if preflight, ok := update.retry.(tui.DashboardActionPreflightCreateSession); ok {
			startCreateSessionPreflight(c, preflight.Launch)
		} else {
			startSessionLaunch(c, update.retry)
		}
	case dashboardIoUpdateMoveOriginPrepared:
		if current, ok := c.lifecycleOperations[update.sessionID]; ok && current.cancelled == update.operation {
			moveOperationID := update.moveOperationID
			c.startupPrompts.prepareMove(update.sessionID, &moveOperationID)
			c.dashboard.SetStandbyMoveOrigin(update.sessionID, c.standbyMoveOrigin(update.sessionID))
		}
	case dashboardIoUpdateStartupPromptOutcomes:
		c.startupPrompts.outcomes(update.sessionID, update.err == nil)
		if update.err != nil {
			slog.Warn("could not check startup prompt outcomes; retrying with backoff", "session_id", update.sessionID, "error", update.err.Error())
			return
		}
		c.startupPrompts.applyOutcomes(update.sessionID, update.outcomes, c.dashboard, c.chats)
	case dashboardIoUpdateStartupPromptQueued:
		c.applyStartupPromptQueued(update)
	case dashboardIoUpdateStartupPromptWithdrawn:
		c.applyStartupPromptWithdrawn(update)
	case dashboardIoUpdateRenameSession:
		if update.err == nil {
			c.dashboard.SetNotice(fmt.Sprintf("Renamed session to %s", tui.FitSessionName(update.saved, tui.NoticeNameCells)))
		} else {
			c.dashboard.SetNotice(fmt.Sprintf(
				"Rename failed for %s: %s",
				tui.FitSessionName(update.title, tui.NoticeNameCells), update.err.Error(),
			))
		}
	case dashboardIoUpdateNotificationsIgnored:
		if update.err != nil {
			verb := "unignore"
			if update.ignored {
				verb = "ignore"
			}
			c.dashboard.SetNotice(fmt.Sprintf("Could not %s notifications: %s", verb, update.err.Error()))
		}
	case dashboardIoUpdateQuotaPreferencesSaved:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not save load-balancing preferences: %s", update.err.Error()))
		}
	case dashboardIoUpdateChangeWorkspace:
		workspaceName := tui.FitSessionName(update.workspaceName, tui.NoticeNameCells)
		if update.err == nil {
			c.dashboard.SetNotice(fmt.Sprintf("Moved %s to workspace \"%s\".", c.sessionNoticeName(update.sessionID), workspaceName))
		} else {
			c.dashboard.SetFailureNotice(fmt.Sprintf(
				"Could not move %s to workspace \"%s\": %s",
				c.sessionNoticeName(update.sessionID), workspaceName, update.err.Error(),
			))
		}
	case dashboardIoUpdateContainerSettings:
		switch {
		case update.err != nil:
			c.dashboard.SetNotice(fmt.Sprintf(
				"Container settings failed for %s: %s",
				c.sessionNoticeName(update.sessionID), update.err.Error(),
			))
		case update.upgradePreview != nil:
			c.dashboard.ShowContainerUpgradePreview(*update.upgradePreview, true)
		default:
			c.applyControllerMetadata(update.metadata)
			c.dashboard.SetConfig(c.controller.Config)
			c.dashboard.SetState(c.controller.State)
			c.refreshChatContext()
			c.dashboard.SetNotice(fmt.Sprintf(
				"Container settings saved for %s; applies when it is next recreated.",
				c.sessionNoticeName(update.sessionID),
			))
		}
	case dashboardIoUpdateProjectCatalog:
		if update.err != nil {
			c.dashboard.ApplyProjectCatalogStatus(core.ProjectCatalogStatus{
				State:  core.ProjectCatalogStateFailed,
				Errors: []string{update.err.Error()},
			})
			c.dashboard.SetNotice(fmt.Sprintf("Project discovery failed: %s", update.err.Error()))
			return
		}
		history := c.controller.State.MountHistory
		for _, location := range update.catalog.Locations {
			key := fmt.Sprintf("project:%s", location.Host)
			paths, _ := history.Get(key)
			if !slices.ContainsFunc(paths, func(path string) bool { return core.PathBufEqual(path, location.CheckoutRoot) }) {
				history.Insert(key, append(slices.Clone(paths), location.CheckoutRoot))
			}
		}
		c.controller.State.MountHistory = history
		c.dashboard.ApplyMountHistory(mountHistoryMap(history))
		c.dashboard.ApplyProjectCatalogStatus(update.catalog.Status)
		if summary := update.catalog.Status.FailureSummary(); summary != nil {
			c.dashboard.SetNotice(*summary)
		} else {
			c.dashboard.SetNotice("Recent projects refreshed.")
		}
	case dashboardIoUpdateMountHistory:
		if update.err != nil {
			slog.Warn("could not refresh recent project directories", "error", update.err.Error())
			return
		}
		// The controller copy is what later SetState calls publish, so it
		// has to carry the fresh history too.
		history := core.NewSnapshotMap[string, []string]()
		for _, key := range slices.Sorted(maps.Keys(update.history)) {
			history.Insert(key, update.history[key])
		}
		c.controller.State.MountHistory = history
		c.dashboard.ApplyMountHistory(update.history)
	case dashboardIoUpdateProjectDirectory:
		c.dashboard.ApplyProjectDirectoryCheck(update.path, update.exists)
	case dashboardIoUpdateTargetTest:
		c.targetTestCancel = nil
		c.dashboard.ApplyTargetTest(update.targetID, update.err)
	case dashboardIoUpdateConfigRename:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not rename %s: %s", update.what, update.err.Error()))
			return
		}
		c.applyControllerMetadata(update.metadata)
		c.dashboard.SetConfig(c.controller.Config)
		c.dashboard.SetState(c.controller.State)
		c.refreshChatContext()
		// The daemon renamed the profile, saw the profile set change and
		// probes the new id once; nothing to ask here.
		c.dashboard.SetNotice(fmt.Sprintf("Renamed %s.", update.what))
	case dashboardIoUpdateGitIdentityChecked:
		c.dashboard.ApplyGitIdentityCheck(update.identity, update.err)
	case dashboardIoUpdateGitIdentitySaved:
		c.dashboard.ApplyGitIdentitySave(update.identity, update.err)
	case dashboardIoUpdateCapacityRefreshRequested:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not refresh target capacity: %s", update.err.Error()))
		}
	case dashboardIoUpdateQuotaRefreshRequested:
		if update.err != nil {
			c.manualQuotaRefreshCycles = nil
			c.dashboard.SetNotice(fmt.Sprintf("Could not refresh quota: %s", update.err.Error()))
		}
	case dashboardIoUpdateQuotaProbeRequested:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Could not refresh %s's quota: %s", update.profileID, update.err.Error()))
		}
	case dashboardIoUpdateWebAccess:
		if update.generation == c.webRequestGeneration {
			c.dashboard.ApplyWebAccess(update.access)
		}
	case dashboardIoUpdateResumeCandidates:
		if c.dashboard.ApplyResumeCandidates(update.discoveryID, update.candidates, update.err) {
			// Checkpoint sizes follow the stopped records.
			c.controllerChanged = true
		}
	case dashboardIoUpdateResumeRecord:
		action := c.dashboard.ApplyResumeRecord(update.sessionID, update.record, update.err)
		if _, none := action.(tui.DashboardActionNone); !none {
			slog.Warn("the resume wizard asked for an action its record loader cannot run", "action", fmt.Sprintf("%#v", action))
		}
	case dashboardIoUpdateWikiSync:
		if c.shutdownRequestedAt != nil || !c.dashboard.ApplyWikiSync(update.dialog, update.episode, update.status, update.err) {
			return
		}
		if update.dialog {
			if requestID, query, ok := c.dashboard.RefreshWikiSearch(); ok {
				if previous := c.wikiSearchTask; previous != nil {
					previous.cancel()
				}
				c.wikiSearchTask = &generationTask{
					generation: requestID,
					cancel:     spawnWikiSearch(requestID, query, 0, c.dashboardIoTx),
				}
			}
		} else if requestID, query, ok := c.dashboard.RefreshSessionsTextSearch(); ok {
			if previous := c.sessionsTextTask; previous != nil {
				previous()
			}
			c.sessionsTextTask = spawnSessionsTextSearch(requestID, query, c.dashboardIoTx)
		}
	case dashboardIoUpdateWikiRows:
		if c.dashboard.ApplyWikiSearchResult(update.requestID, update.page, update.err) {
			c.loadResumePreview()
		}
	case dashboardIoUpdateResumeTextMatches:
		if c.dashboard.ApplyResumeTextSearchResult(update.requestID, update.page.Matches, update.err) {
			c.loadResumePreview()
		}
	case dashboardIoUpdateSessionTextMatches:
		c.dashboard.ApplySessionsText(update.requestID, update.page.Matches, update.err)
	case dashboardIoUpdateWikiBrief:
		if update.err == nil {
			c.dashboard.ApplyWikiBrief(update.wikiID, update.brief)
		} else {
			c.dashboard.ApplyWikiBrief(update.wikiID, fmt.Sprintf("Could not load the transcript: %s", update.err.Error()))
		}
	case dashboardIoUpdateWikiHits:
		if update.err == nil {
			c.dashboard.ApplyWikiHits(update.wikiID, update.query, update.hits)
		} else {
			// Shown in the pane the same way a failed briefing is, so the
			// reason sits where the passages were promised.
			c.dashboard.ApplyWikiHits(update.wikiID, update.query, &client.WikiHitTranscript{
				Blocks: []client.WikiHitBlock{{
					Text: fmt.Sprintf("Could not load the matching passages: %s", update.err.Error()),
				}},
				OmittedAfter: 0,
			})
		}
	case dashboardIoUpdateWebListeners:
		if update.generation == c.webRequestGeneration {
			c.dashboard.ApplyWebListeners(update.processes, update.err)
		}
	case dashboardIoUpdateWebAccessError:
		if update.generation == c.webRequestGeneration {
			c.dashboard.ApplyWebError(update.message)
		}
	case dashboardIoUpdateBuildCachePreviewed:
		c.dashboard.BuildCachePreviewed(update.generation, update.key, update.preview, update.err, update.installMbxAvailable)
	case dashboardIoUpdateMbxInstalled:
		c.dashboard.MbxInstallFinished(
			update.generation,
			update.key,
			update.machineID,
			update.installed,
			update.err,
			update.preview,
			update.previewErr,
			update.installMbxAvailable,
		)
	case dashboardIoUpdateArchiveSpacePreviewed:
		c.dashboard.ArchiveSpacePreviewed(update.generation, update.olderThanDays, update.preview, update.err)
	case dashboardIoUpdateSetupDiscovered:
		c.dashboard.SetupDiscovered(update.generation, update.detection, update.err)
	case dashboardIoUpdateProfileLoginStatuses:
		if update.err != nil {
			slog.Warn("could not read profile sign-in status", "error", update.err.Error())
			return
		}
		c.dashboard.SetProfileLoginStatuses(update.generation, update.statuses)
	case dashboardIoUpdateAvailabilityCheckRequested:
		if update.err != nil {
			c.dashboard.SetFailureNotice(fmt.Sprintf("Could not request an availability check: %s", update.err.Error()))
		}
	case dashboardIoUpdateFirstRunStarted:
		c.dashboard.BeginWelcome()
	case dashboardIoUpdateFirstRunConfigured:
		c.dashboard.WelcomeConfigured(update.report.Summary(), len(update.report.Agents) != 0)
	case dashboardIoUpdateFirstRunChecked:
		c.dashboard.ReleaseAvailabilityCheck()
		switch {
		case update.err != nil:
			c.dashboard.WelcomeChecked([]string{fmt.Sprintf(
				"Setup could not finish: %s. Run `mj doctor` to check the installation, and restart `mj` to retry.",
				update.err.Error(),
			)})
		case update.checked:
			c.dashboard.WelcomeChecked(update.problems)
		default:
			c.dashboard.WelcomeNotNeeded()
		}
	case dashboardIoUpdateSetupSaved:
		saved := update.err == nil
		// The acknowledgement closes the editor; only the runtime feed
		// installs configuration, including edits from other clients. Rust
		// maps the saved config through with_local_targets and then replaces
		// it with the controller's, so only the controller's reaches here.
		config := core.Config{}
		if saved {
			config = c.controller.Config
		}
		c.dashboard.SetupSaved(update.generation, config, update.err)
		if saved && update.loginProfile != nil {
			c.dashboard.QueueSetupLogin(update.generation, *update.loginProfile)
		}
	case dashboardIoUpdateSpinnerStyleSaved:
		c.dashboard.FinishSpinnerStyleSave()
		if update.err != nil {
			c.dashboard.SetFailureNotice(fmt.Sprintf("Could not save spinner style: %s", update.err.Error()))
			return
		}
		palette := ""
		if key, ok := c.dashboard.FirstKeyLabel(tui.CommandIdPalette); ok {
			palette = fmt.Sprintf("%s → ", key)
		}
		c.dashboard.SetNotice(fmt.Sprintf("Spinner: %s. %sNext spinner style to change it.", update.config.Spinner, palette))
	case dashboardIoUpdateClipboardWritten:
		if update.err != nil {
			c.dashboard.SetFailureNotice(fmt.Sprintf("Copy to the system clipboard failed: %s", update.err.Error()))
		}
	case dashboardIoUpdateClipboardText:
		c.clipboardReadInFlight = false
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Paste failed: %s", update.err.Error()))
			return
		}
		target := update.target
		c.dashboard.ApplyClipboardText(&target, update.text)
	case dashboardIoUpdateTerminalDraft:
		c.composerDrafts.complete(update.sessionID, update.revision, update.err == nil)
		if update.err == nil {
			delete(c.draftSaveFailures, update.sessionID)
		} else {
			message := fmt.Sprintf("Could not confirm saved draft for %s: %s", c.sessionNoticeName(update.sessionID), update.err.Error())
			c.draftSaveFailures[update.sessionID] = message
			c.dashboard.SetNotice(message)
		}
		c.flushComposerDraft(update.sessionID, false)
	case dashboardIoUpdateReadReceipt:
		c.finishReadReceipt(update.sessionID, update.through, update.err)
	case dashboardIoUpdateCreatedBundle:
		if update.err != nil {
			c.dashboard.FailBundleCreation(update.err.Error())
			return
		}
		c.dashboard.ApplyCreatedBundle(c.controller.Config, update.created.bundleID)
	case dashboardIoUpdateRemovedBundle:
		if update.err != nil {
			c.dashboard.FailBundleRemoval(update.err.Error())
			return
		}
		c.dashboard.ApplyRemovedBundle(c.controller.Config, update.bundleID)
	case dashboardIoUpdateImportedSessionApplied:
		if update.err != nil {
			slog.Error("Saving imported session failed", "error", update.err.Error())
			c.dashboard.ShowImportFailure(update.err.Error())
			return
		}
		applied := update.applied
		sessionID := applied.session.ID
		// The import's own answer carries the stopped record the resume
		// wizard opens on.
		c.dashboard.RememberStoppedRecord(client.SessionRecordWithVerdict{Record: applied.session, Verdict: applied.verdict})
		c.resolveProjectSources()
		c.dashboard.SetNotice(fmt.Sprintf("Imported %s session %s.", applied.harness, applied.nativeSessionID))
		// Import completion can arrive after a tab switch. Keep the imported
		// record globally visible, but only open the resume modal in the
		// workspace that owns it.
		if c.sessionInActiveWorkspace(sessionID) {
			c.dashboard.BeginResumeFor(sessionID)
		}
	case dashboardIoUpdateLifecycleReloaded:
		c.applyLifecycleReloaded(update.reloaded)
	case dashboardIoUpdateLifecycleCancellation:
		if update.err != nil {
			c.dashboard.SetFailureNotice(fmt.Sprintf(
				"Could not cancel operation for %s: %s",
				c.sessionNoticeName(update.sessionID), update.err.Error(),
			))
		}
	case dashboardIoUpdateContainerUpgradePreview:
		switch {
		case update.err == nil:
			c.dashboard.ShowContainerUpgradePreview(update.preview, update.upgradeOnly)
		case update.upgradeOnly:
			c.dashboard.SetFailureNotice(fmt.Sprintf("Container upgrade unavailable: %s", update.err.Error()))
		default:
			c.dashboard.ShowNormalRestart(update.sessionID)
		}
	case dashboardIoUpdateSuspendPreflight:
		if update.err != nil {
			if c.dashboard.DismissSuspendPreflight(update.sessionID) {
				c.dashboard.SetFailureNotice(fmt.Sprintf(
					"Could not check workspace for %s: %s",
					c.sessionNoticeName(update.sessionID), update.err.Error(),
				))
			}
			return
		}
		if suspend, ok := c.dashboard.ApplySuspendPreflight(update.preflight).(tui.DashboardActionSuspend); ok {
			startSuspension(c, suspend.SessionID, suspend.Consent)
		}
	case dashboardIoUpdateContainerUpgradeQueued:
		if update.err == nil {
			c.dashboard.SetNotice("Container upgrade pending; waiting for accepted work. Cancel through the session operation.")
		} else {
			c.dashboard.SetFailureNotice(fmt.Sprintf("Container upgrade was not accepted: %s", update.err.Error()))
		}
	case dashboardIoUpdateMovePrepared:
		if update.err == nil {
			c.dashboard.ApplyMovePreparation(update.requestID, update.preparation)
		} else {
			c.dashboard.SetMovePreparationFailed(update.sessionID, update.requestID, update.err.Error())
		}
	case dashboardIoUpdateCheckpointArchiveSizes:
		answered := map[string]*uint64{}
		var failures []string
		for _, id := range slices.Sorted(maps.Keys(update.sizes)) {
			result := update.sizes[id]
			pending, isPending := c.checkpointArchivePending[id]
			target, hasTarget := update.targets[id]
			record, hasRecord := c.checkpointArchiveRecords.Get(id)
			if !isPending || pending != update.generation || hasTarget != hasRecord ||
				(hasTarget && !core.PathBufEqual(target, record)) {
				continue
			}
			delete(c.checkpointArchivePending, id)
			if result.Err != nil {
				failures = append(failures, fmt.Sprintf("%s: %s", id, *result.Err))
				continue
			}
			if hasTarget {
				c.checkpointArchiveTargetsSeen[id] = target
			}
			answered[id] = result.Ok
		}
		c.dashboard.PatchCheckpointArchiveSizes(answered)
		if len(failures) != 0 {
			c.dashboard.SetNotice(fmt.Sprintf(
				"Could not read checkpoint archive sizes: %s. Use Refresh to retry.",
				strings.Join(failures, "; "),
			))
		}
	case dashboardIoUpdateWorkerDiagnosis:
		c.applyWorkerDiagnosis(update.sessionID, update.episodeID, update.diagnosis, update.err)
	case dashboardIoUpdateProfileHydration:
		c.dashboard.ApplyProfileHydration(update.key, update.err)
	case dashboardIoUpdateProjectDiscovery:
		c.dashboard.ApplyProjectDiscovery(update.context, update.discovery, update.err)
	case dashboardIoUpdatePathCompletions:
		if update.err != nil {
			c.dashboard.SetNotice(fmt.Sprintf("Path completion failed: %s", update.err.Error()))
			return
		}
		c.dashboard.ApplyPathCompletions(update.context, update.prefix, update.completion)
	case dashboardIoUpdateMountValidation:
		action := c.dashboard.ApplyResolvedMountSource(update.context, update.source, update.resolved, update.forced, update.err)
		startMovePreparation(c, action)
	case dashboardIoUpdateSessionMountValidation:
		c.applySessionMountValidation(update)
	case dashboardIoUpdateResumeRepositoryPreflight:
		c.applyResumeRepositoryPreflight(update)
	case dashboardIoUpdateContainerPathResolved:
		c.dashboard.ContainerPathResolved(update.context, update.path, update.err)
	case dashboardIoUpdateSetupPathResolved:
		c.dashboard.SetupPathResolved(update.generation, update.draft, update.path, update.value, update.resolved, update.err)
	case dashboardIoUpdateProjectValidation:
		c.dashboard.ApplyResolvedProjectDirectory(update.context, update.directory, update.resolved, update.options, update.reviewTurnsOff, update.err)
	}
}

// mountHistoryMap is a mount history as the dashboard takes it (Rust's
// `BTreeMap<String, Vec<PathBuf>>` from the persistent map's clone).
func mountHistoryMap(history core.SnapshotMap[string, []string]) map[string][]string {
	result := make(map[string][]string, history.Len())
	for key, paths := range history.All() {
		result[key] = slices.Clone(*paths)
	}
	return result
}

// applyWorkspaceManagement is the WorkspaceManagement arm of
// applyDashboardIoUpdate.
func (c *dashboardContext) applyWorkspaceManagement(update dashboardIoUpdateWorkspaceManagement) {
	if update.err != nil {
		if !c.dashboard.FinishWorkspaceManagement(update.generation, nil, update.err, false).Foreground {
			c.dashboard.SetNotice(fmt.Sprintf("Workspace operation failed: %s", update.err.Error()))
		}
		return
	}
	result := update.result
	// The runtime feed alone owns tab names, layouts and the active tab. A
	// deletion owns only the composers, because the feed also hides an
	// empty default it cannot tell apart from a deleted one.
	if deleted := result.deletedWorkspaceID; deleted != nil {
		c.discardWorkspaceComposers(*deleted)
		if pending := c.pendingWorkspaceSelection; pending != nil && *pending == *deleted {
			c.pendingWorkspaceSelection = nil
		}
	}
	// The TUI owns the modal generation guard. It returns whether this
	// result still belongs to the visible manager, so a late create cannot
	// switch another tab.
	outcome := c.dashboard.FinishWorkspaceManagement(update.generation, result.entries, nil, result.detailsPending)
	if closing, ok := outcome.Action.(tui.DashboardActionCloseWorkspace); ok {
		spawnWorkspaceClose(closing.Generation, closing.WorkspaceID, c.dashboardIoTx)
	}
	if outcome.Foreground && result.selectWorkspace != nil {
		workspaceID := *result.selectWorkspace
		c.dashboard.CancelModal()
		if _, known := c.knownWorkspaceLayouts[workspaceID]; known {
			c.selectWorkspace(&workspaceID)
		} else {
			// The tab opens with the feed frame that adds it.
			c.pendingWorkspaceSelection = &workspaceID
		}
	}
}

// applyChatOpened is the ChatOpened arm of applyDashboardIoUpdate.
func (c *dashboardContext) applyChatOpened(update dashboardIoUpdateChatOpened) {
	sessionID := update.sessionID
	// Ignore a late result after the pane that asked for it moved on (or
	// the dashboard has shut down).
	attachment, ok := c.attachments[sessionID]
	if !ok || !attachment.acceptsSurfaceResult(update.generation, c.dashboard, sessionID) {
		if ok && attachment.accepts(update.generation, &sessionID) {
			attachment.deferAttach()
			c.forgetOpening(sessionID)
			c.syncOpeningSession()
		}
		return
	}
	conversationOpening := attachment.hasConversationInterest(c.dashboard, sessionID)
	attachment.finish()
	c.forgetOpening(sessionID)
	if _, wanted := c.wantedChatSessions()[sessionID]; !wanted {
		return
	}
	c.syncOpeningSession()
	// The attach may have crossed a lifecycle boundary while it was
	// preparing. Keep the warm chat/draft untouched and drop the late result
	// instead of reviving a retiring conversation.
	if c.dashboard.TransitionKind(sessionID) != nil || c.dashboard.TransitionFailureKind(sessionID) != nil {
		attachment.deferAttach()
		return
	}
	if update.err != nil {
		slog.Warn("could not open session", "session_id", sessionID, "error", update.err.Error())
		message := update.err.Error()
		c.dashboard.ReportChatAttachmentResult(sessionID, &message, conversationOpening)
		return
	}
	prepared := *update.prepared
	// A chat already warm for this session continued receiving feed updates
	// while the attach was in flight. Capture and persist its latest local
	// composer just before replacing it.
	c.recordChatDetach(sessionID)
	c.saveQuestionDraft(sessionID)
	// Whatever the user typed into the standby composer while this attach
	// ran is the newest draft, so it wins over the copy captured when the
	// open started.
	composer := c.dashboard.TakeStandbyComposer(sessionID)
	if composer == nil {
		if warm, ok := c.chats[sessionID]; ok {
			composer = warm.TakeLiveComposer()
		}
	}
	if composer == nil {
		if draft, ok := c.composerDrafts.get(sessionID); ok {
			prepared = prepared.WithDraft(draft.Text)
		}
	}
	var opened *chat.ActiveChat
	if composer != nil {
		opened = prepared.OpenWithComposer(*composer)
	} else {
		opened = prepared.Open()
	}
	applyPolicyConfigKey(opened, c.runtimeSessions)
	// Only a newly opened chat may receive a locator jump. A warm chat
	// retains the reader's current position.
	var position *chat.TranscriptPosition
	if _, warm := c.chats[sessionID]; !warm {
		if target, ok := c.dashboard.QuestionMessage(sessionID); ok {
			seen, ok := c.questionTargetsSeen[sessionID]
			if !ok {
				seen = map[string]struct{}{}
				c.questionTargetsSeen[sessionID] = seen
			}
			if _, already := seen[target]; !already {
				seen[target] = struct{}{}
				jump := chat.TranscriptPositionAtStableID(target)
				position = &jump
			}
		}
	}
	if position == nil {
		if stored, ok := c.transcriptPositions[sessionID]; ok {
			position = &stored
		}
	}
	if position != nil {
		opened.RestoreTranscriptPosition(*position)
	}
	c.restoreQuestionDraft(sessionID, opened)
	for _, retry := range opened.StartupRetryPrompts() {
		c.startupPrompts.resume(
			namedSession{name: sessionNoticeName(&c.controller.State, sessionID), id: sessionID},
			retry.Prompt,
			retry.InheritedDraft,
			c.dashboardIoTx,
			c.criticalOperations,
		)
	}
	c.chats[sessionID] = opened
	// The context travelled with the attach, which is asynchronous;
	// anything the surface learned while it was in flight is handed over
	// now.
	c.refreshChatContext()
	c.applyRuntimeReviewToChat(sessionID)
	c.dashboard.ReportChatAttachmentResult(sessionID, nil, conversationOpening)
	c.acknowledgeVisibleChats()
}

// forgetOpening is Rust's `opening_chat_sessions.retain(|_, session| session
// != &session_id)`.
func (c *dashboardContext) forgetOpening(sessionID string) {
	for pane, opening := range c.openingChatSessions {
		if opening == sessionID {
			delete(c.openingChatSessions, pane)
		}
	}
}

// applyStartupPromptQueued is the StartupPromptQueued arm of
// applyDashboardIoUpdate.
func (c *dashboardContext) applyStartupPromptQueued(update dashboardIoUpdateStartupPromptQueued) {
	sessionID, prompt := update.sessionID, update.prompt
	if c.startupPrompts.isSettled(sessionID, prompt.CommandID) {
		return
	}
	if update.err == nil {
		return
	}
	failure := update.err.Error()
	if !c.startupPrompts.admissionFailed(sessionID, prompt.CommandID, failure, update.definitelyUnaccepted) {
		if c.dashboard.NotificationReplyUnconfirmed(sessionID, prompt.CommandID, failure) {
			return
		}
		if update.definitelyUnaccepted {
			c.dashboard.SetFailureNotice(fmt.Sprintf("The prompt was not queued: %s; the recalled draft is kept.", failure))
		} else {
			c.dashboard.SetFailureNotice(fmt.Sprintf("Could not confirm whether the prompt was queued: %s; checking its status.", failure))
		}
		return
	}
	if finishNotificationReply(c.dashboard, c.chats, sessionID, &prompt, errors.New(failure)) {
		return
	}
	var restored error
	if conversation, ok := c.chats[sessionID]; ok && !c.dashboard.HasStandbyComposer(sessionID) {
		restored = conversation.RestoreStartupPrompt(prompt)
	} else {
		restored = c.dashboard.RestoreStartupPrompt(sessionID, prompt)
	}
	recovery := ""
	if restored != nil {
		recovery = fmt.Sprintf("; restoring the draft failed: %s", restored.Error())
	}
	c.dashboard.SetFailureNotice(fmt.Sprintf(
		"Could not queue the prompt for session %s: %s%s",
		c.sessionNoticeName(sessionID), failure, recovery,
	))
}

// applyStartupPromptWithdrawn is the StartupPromptWithdrawn arm of
// applyDashboardIoUpdate.
func (c *dashboardContext) applyStartupPromptWithdrawn(update dashboardIoUpdateStartupPromptWithdrawn) {
	sessionID, prompt := update.sessionID, update.prompt
	if c.startupPrompts.withdrawalNeedsReceipt(sessionID, prompt.CommandID) {
		return
	}
	alreadySent := update.err == nil && !update.withdrawn
	var (
		dropped bool
		err     error
	)
	if conversation, ok := c.chats[sessionID]; ok && !c.dashboard.HasStandbyComposer(sessionID) {
		dropped, err = conversation.StartupPromptWithdrawn(prompt, update.withdrawn, update.err)
	} else {
		dropped, err = c.dashboard.StartupPromptWithdrawn(sessionID, prompt, update.withdrawn, update.err)
	}
	switch {
	case err != nil:
		c.dashboard.SetFailureNotice(fmt.Sprintf("Could not take the prompt back (%s); it is still queued.", err.Error()))
	case alreadySent && dropped:
		c.dashboard.SetNotice("That prompt had already been sent, so its unchanged copy was removed from the composer.")
	case alreadySent:
		c.dashboard.SetNotice("That prompt had already been sent; Enter sends your edited draft as a new prompt.")
	}
}

// applySessionMountValidation is the SessionMountValidation arm of
// applyDashboardIoUpdate.
func (c *dashboardContext) applySessionMountValidation(update dashboardIoUpdateSessionMountValidation) {
	if update.generation != c.dashboard.SessionPreflightGeneration() {
		if update.err != nil {
			slog.Warn("cancelled mount preflight failed", "error", update.err.Error())
		}
		return
	}
	switch {
	case update.err != nil:
		failure := fmt.Sprintf("Could not check attached directories: %s", update.err.Error())
		c.dashboard.ApplyRemoteSessionPreflight(update.generation, nil, errors.New(failure))
		c.dashboard.FailResumePreflight(failure)
	case update.failure != nil:
		c.dashboard.ApplySessionMountPreflightFailure(update.failure.First, update.failure.Second)
	default:
		if resume, ok := update.launch.(tui.DashboardActionPreflightResumeRepositories); ok {
			if err := startResumeRepositoryPreflight(c, resume.Launch); err != nil {
				c.dashboard.FailResumePreflight(fmt.Sprintf("Could not check checkpoint repositories: %s", err.Error()))
			}
			return
		}
		if _, ok := update.launch.(tui.DashboardActionCreateSession); ok {
			startCreateSessionPreflight(c, update.launch)
		} else {
			c.dashboard.FinishSessionMountPreflight()
			startSessionLaunch(c, update.launch)
		}
	}
}

// applyResumeRepositoryPreflight is the ResumeRepositoryPreflight arm of
// applyDashboardIoUpdate.
func (c *dashboardContext) applyResumeRepositoryPreflight(update dashboardIoUpdateResumeRepositoryPreflight) {
	if update.generation != c.dashboard.SessionPreflightGeneration() {
		if update.err != nil {
			slog.Warn("cancelled repository preflight failed", "error", update.err.Error())
		}
		return
	}
	if update.err != nil {
		if update.submittedRepositoryID != nil {
			c.dashboard.ApplyRepositoryOriginFailure(*update.submittedRepositoryID, update.err.Error())
		} else {
			c.dashboard.FailResumePreflight(fmt.Sprintf("Could not check checkpoint repositories: %s", update.err.Error()))
		}
		return
	}
	switch preflight := update.apply.preflight.(type) {
	case core.ResumeRepositorySourcePreflightReady:
		c.dashboard.FinishResumeRepositoryPreflight()
		startPreflightedSessionLaunch(c, update.launch, preflight.Value)
	case core.ResumeRepositorySourcePreflightConvertingRawCheckout:
		c.dashboard.ShowRawConversionConfirmation(update.launch, preflight.Receipt, *preflight.Preview)
	case core.ResumeRepositorySourcePreflightRepositoryMoved:
		mismatch := preflight.Value
		if update.submittedRepositoryID != nil && *update.submittedRepositoryID == mismatch.RepositoryID {
			c.dashboard.ApplyRepositoryOriginFailure(
				mismatch.RepositoryID,
				fmt.Sprintf("That origin does not contain checkpoint base %s.", mismatch.MissingCommit),
			)
		} else {
			c.dashboard.ShowRepositoryOriginDialog(
				mismatch.SessionID,
				mismatch.RepositoryID,
				mismatch.MissingCommit,
				mismatch.ArchivedOrigin,
				mismatch.ConfiguredOrigin,
				update.launch,
			)
		}
	}
}

func (c *dashboardContext) loadResumePreview() {
	switch action := c.dashboard.NextWikiPreview().(type) {
	case tui.DashboardActionLoadArchivedBrief:
		spawnWikiBrief(action.WikiID, c.dashboardIoTx)
	case tui.DashboardActionLoadArchivedHits:
		spawnWikiHits(action.WikiID, action.Query, c.dashboardIoTx)
	}
}

func (c *dashboardContext) applyCreateSessionUpdate(update dashboardCreateSessionUpdate) {
	switch update := update.(type) {
	case dashboardCreateSessionUpdateRemoteRepair:
		c.dashboard.ClearNotice()
		c.dashboard.ShowRemoteRepairConfirmation(update.bundleID, update.repairs, update.retry)
	case dashboardCreateSessionUpdateRegistered:
		registered := update.registered
		sessionID := registered.session.ID
		// A restore has its session now; the archive may be restored again
		// from here on.
		c.dashboard.FinishArchiveRestore(registered.retryLaunch)
		if remembered := registered.rememberedContainerSize; remembered != nil {
			c.controller.State.RememberContainerSize(remembered.First, remembered.Second)
		}
		c.resolveProjectSources()
		record := c.controller.State.Sessions.GetShared(sessionID)
		stillLaunching := record != nil && record.State == core.SessionStateProvisioning
		if stillLaunching {
			c.dashboard.BeginSessionOperation(sessionID, tui.SessionOperationKindLaunching, nil)
		}
		noticeName := c.sessionNoticeName(sessionID)
		c.dashboard.SetNotice(tui.DashboardActionAnnotateLaunchNotice(registered.retryLaunch, fmt.Sprintf("Launching %s…", noticeName)))
		if stillLaunching {
			c.lifecycleOperations[sessionID] = &activeLifecycleOperation{
				cancelled:   registered.cancelled,
				kind:        tui.SessionOperationKindLaunching,
				retryLaunch: registered.retryLaunch,
				noticeName:  noticeName,
			}
		}
	case dashboardCreateSessionUpdateFailed:
		c.dashboard.FinishArchiveRestore(update.retryLaunch)
		c.dashboard.ShowLaunchFailure(update.message, update.retryLaunch)
	}
}

func (c *dashboardContext) applyLifecycleReloaded(reloaded *lifecycleReloaded) {
	update, operation := reloaded.reload.update, reloaded.reload.operation
	sessionID := update.sessionID
	if current, ok := c.lifecycleOperations[sessionID]; ok && current.cancelled != update.operation {
		if update.failure != nil {
			slog.Warn("superseded lifecycle reload failed", "session_id", sessionID, "error", update.failure.Error())
		}
		return
	}
	// Taken before the reload: a destroy removes the record, and the runtime
	// snapshot may already have dropped it.
	var takenAtStart *string
	if operation != nil {
		takenAtStart = &operation.noticeName
	}
	name := lifecycleNoticeName(&c.controller.State, sessionID, takenAtStart)
	if failure := update.failure; failure != nil &&
		(failure.kind == dashboardLifecycleObservationUnavailable || failure.kind == dashboardLifecycleAdmissionUnconfirmed) {
		if reloaded.err == nil {
			c.applyControllerMetadata(reloaded.metadata)
			c.dashboard.SetState(c.controller.State)
			c.resolveProjectSources()
		} else {
			slog.Warn("could not reload lifecycle observation", "session_id", sessionID, "error", reloaded.err.Error())
		}
		if failure.kind == dashboardLifecycleAdmissionUnconfirmed {
			c.dashboard.SetNotice(fmt.Sprintf("Could not confirm daemon acceptance for %s; check session status before retrying", name))
		} else {
			c.dashboard.SetNotice(fmt.Sprintf("The daemon accepted the operation for %s; check session status for its result", name))
		}
		return
	}
	if operation != nil && operation.kind == tui.SessionOperationKindMoving {
		var completed *string
		if moved, ok := update.success.(daemon.LifecycleSuccessMoved); ok && update.failure == nil &&
			moved.Outcome.Outcome != "failed" && moved.Outcome.Outcome != "cancelled" {
			operationID := moved.Outcome.OperationID
			completed = &operationID
		}
		c.startupPrompts.finishMove(sessionID, completed)
		if update.failure != nil {
			c.dashboard.KeepStandbySubmissionAsDraft(sessionID)
		}
	}
	if reloaded.err != nil {
		c.dashboard.SetNotice(fmt.Sprintf("Could not reload completed operation: %s", reloaded.err.Error()))
		return
	}
	c.applyControllerMetadata(reloaded.metadata)
	c.dashboard.SetState(c.controller.State)
	c.resolveProjectSources()
	// Navigation owns pane assignments. Completion retires the old
	// attachment; reconciliation reconnects only panes still showing it.
	if update.failure == nil && operation != nil && operation.retirement != nil {
		c.dropWarmChatFor(operation.retirement)
	}
	if update.failure != nil {
		message := update.failure.Error()
		switch {
		case operation != nil && operation.kind == tui.SessionOperationKindSuspending:
			c.dashboard.ShowCloseFailure(sessionID, message)
		case operation != nil && operation.kind == tui.SessionOperationKindLaunching:
			var retry tui.DashboardAction
			if !c.controller.State.Sessions.ContainsKey(sessionID) {
				retry = operation.retryLaunch
			}
			c.dashboard.ShowLaunchFailure(message, retry)
		default:
			label := "Operation"
			if operation != nil {
				label = operation.kind.Label()
			}
			c.dashboard.SetFailureNotice(fmt.Sprintf("%s failed: %s", label, message))
		}
		return
	}
	switch success := update.success.(type) {
	case daemon.LifecycleSuccessCreated:
		c.dashboard.SetNotice(fmt.Sprintf("Session %s is ready", name))
		// No quota probe here. Creating, resuming or moving a session reads
		// the quota already held; only the explicit Refresh and the
		// scheduled poll ask the usage endpoint.
	case daemon.LifecycleSuccessResumed:
		// Seed the conversation from the tail rather than from a transcript
		// shipped back through the daemon reply. The view keeps
		// chat.TailSeedItems and discards everything before it, so reading
		// the whole projection was work proportional to history for a
		// result that was thrown away.
		if _, shown := c.dashboard.PaneForSession(sessionID); shown {
			c.requestTranscriptTailSeed(sessionID)
		}
		c.dashboard.SetNotice(fmt.Sprintf("Resumed %s with %s on %s", name, success.ProfileID, success.TargetID))
	case daemon.LifecycleSuccessMoved:
		if _, shown := c.dashboard.PaneForSession(sessionID); shown {
			c.requestTranscriptTailSeed(sessionID)
		}
		outcome := success.Outcome
		destination := fmt.Sprintf("%s/%s", outcome.ProfileID, outcome.TargetTemplateID)
		if outcome.Outcome == "unchanged" {
			c.dashboard.SetNotice(fmt.Sprintf("Move of %s unchanged on %s (operation %s)", name, destination, outcome.OperationID))
		} else {
			c.dashboard.SetNotice(fmt.Sprintf("Moved %s to %s; ready and idle (operation %s)", name, destination, outcome.OperationID))
		}
	case daemon.LifecycleSuccessClosed:
		c.dashboard.SetNotice(fmt.Sprintf("Suspended %s", name))
	case daemon.LifecycleSuccessDestroyedStopped:
		c.dashboard.SetNotice(fmt.Sprintf("Permanently destroyed suspended session %s", name))
	case daemon.LifecycleSuccessForceDestroyed:
		c.dashboard.SetNotice(fmt.Sprintf("Permanently destroyed session %s", name))
	}
}

func (c *dashboardContext) applyWorkerDiagnosis(sessionID string, episodeID uint64, diagnosis *string, err error) {
	record := c.controller.State.Sessions.GetShared(sessionID)
	if record == nil || !sessionruntime.SessionTargetIsPollable(record) {
		c.workerDiagnoses.Observe(sessionID, true, nil)
		c.workerDiagnoses.Finish(sessionID, episodeID)
		c.dashboard.ClearSessionUnreachable(sessionID)
		if err != nil {
			slog.Warn("retired worker diagnosis task failed: "+err.Error(), "session_id", sessionID)
		}
		return
	}
	completion := c.workerDiagnoses.Finish(sessionID, episodeID)
	if completion.DisplayError != nil {
		message := fmt.Sprintf("relay unreachable: %s", *completion.DisplayError)
		switch {
		case err != nil:
			message += "; worker diagnostics failed: " + err.Error()
		case diagnosis != nil:
			message += "; " + *diagnosis
		}
		slog.Warn(message, "session_id", sessionID)
	} else if err != nil {
		slog.Warn("stale worker diagnosis task failed: "+err.Error(), "session_id", sessionID)
	}
	if restart := completion.RestartEpisode; restart != nil {
		spawnWorkerDiagnosis(&c.controller, sessionID, *restart, c.dashboardIoTx, c.criticalOperations)
	}
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
