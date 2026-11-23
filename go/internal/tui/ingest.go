// Port of mj-tui/src/ingest.rs.

// Controller state ingestion: projections, quotas, capacity, and notices.

package tui

import (
	"maps"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SessionOperationDisplay is one daemon-owned lifecycle operation the
// dashboard shows on a session row.
type SessionOperationDisplay struct {
	kind SessionOperationKind
	// operationID is the daemon-owned identity used to reject a stale
	// lifecycle result after a later operation for the same session has
	// started.
	operationID *string
	// cancellable says whether the daemon still accepts cancellation.
	// Create becomes uncancellable at its atomic commit boundary.
	cancellable           bool
	startedAtEpochSeconds uint64
	// placeholder: the operation owns its preview until completion, even when
	// a state publication temporarily omits a newly registered session.
	placeholder *core.SessionRecord
	// activeStages is the launch stages currently in flight and when each
	// began, sorted by stage (Rust's BTreeMap order). More than one entry
	// means independent setup lanes are overlapping.
	activeStages []ProvisionStageStart
	// resumeDestination is the (profile, target) a resume is moving the
	// session TO. The controller updates the session record's own
	// profile/target as soon as a resume starts, but that update lands in a
	// separate, disk-persisted Controller inside the background task; the
	// dashboard's local session snapshot is not refreshed until the
	// operation finishes. This field lets the in-flight row show the
	// destination instead of the stale snapshot's pre-resume profile/target.
	resumeDestination *ResumeDestination
}

// ProvisionStageStart is one entry of SessionOperationDisplay's active
// stages: a launch stage and the epoch second it began.
type ProvisionStageStart struct {
	Stage     core.ProvisionStage
	StartedAt uint64
}

// ResumeDestination is the (profile, target) pair a resume is moving a
// session to.
type ResumeDestination struct {
	ProfileID        string
	TargetTemplateID string
}

// SessionDetail is what the dashboard derived about one session from its
// materialized projection and the daemon's feeds. The zero value is Rust's
// Default.
type SessionDetail struct {
	materializedAppliedEventOrdinal *uint64
	currentTurnStartedAt            *uint64
	questionMessage                 *string
	supportsOpenRequest             bool
	// legacyAwaitingInput is the latest-turn rule, used only for workers
	// before relay protocol 64, whose questions may have no daemon dismissal
	// target.
	legacyAwaitingInput              bool
	notificationDismissal            NotificationDismissalState
	lastActivityAtMs                 *uint64
	lastAgentMessage                 *string
	lastUserMessage                  *string
	lastAgentMessageFollowsLastUser  bool
	latestAgentActivityAfterLastUser *string
	// currentStepStartedAtMs is when the step the agent is on began, so a
	// row can age it.
	currentStepStartedAtMs *uint64
	// activity is what the session is doing beyond its turn clock: the turn
	// the harness started on its own, and the commands the agent left
	// running.
	activity client.SessionActivity
	// agentMessageLatestContentOrdinals are the latest agent-content
	// ordinals retained so a state-only read-cursor update can recompute
	// unread agent messages exactly.
	agentMessageLatestContentOrdinals []uint64
	unreadAgentMessages               int
	interruptionEventOrdinals         []uint64
	unreadInterruptions               int
	transcript                        *chat.TranscriptSnapshot
	transcriptHydration               TranscriptHydration
	queuedPrompts                     []core.QueuedPrompt
	// pendingElicitations are the form requests the agent is currently
	// waiting for. This comes from the complete materialized projection and
	// drives the dashboard's attention indicator without opening a live chat
	// connection.
	pendingElicitations []core.ElicitationRequest
	// pendingElicitationsAppliedEventOrdinal is the ordinal paired with
	// pendingElicitations. Summary reads can advance the general
	// materialized ordinal without carrying a pending-request list, so keep
	// this freshness boundary separate.
	pendingElicitationsAppliedEventOrdinal *uint64
	// projection is what the last projection derived, so the next one only
	// rescans the transcript items that changed.
	projection MaterializedProjectionCache
	// outputPreview is the Sessions row's rendering of the message it shows.
	outputPreview OutputPreviewCache
	// currentModel and currentEffort are the session's current model and
	// effort as its worker reports them.
	currentModel  *string
	currentEffort *string
}

// NotificationDismissalState is a dismissal this dashboard submitted and
// the daemon's answer to it. A nil value is Rust's None.
type NotificationDismissalState interface {
	isNotificationDismissalState()
}

// NotificationDismissalStateSubmitting is a dismissal sent and not yet
// answered.
type NotificationDismissalStateSubmitting struct {
	Value core.NotificationDismissal
}

// NotificationDismissalStateAccepted is a dismissal the daemon accepted at
// Ordinal.
type NotificationDismissalStateAccepted struct {
	Target  core.NotificationDismissal
	Ordinal uint64
}

func (NotificationDismissalStateSubmitting) isNotificationDismissalState() {}
func (NotificationDismissalStateAccepted) isNotificationDismissalState()   {}

// notificationDismissalTarget is the dismissal a state carries, whichever
// variant it is.
func notificationDismissalTarget(state NotificationDismissalState) (core.NotificationDismissal, bool) {
	switch state := state.(type) {
	case NotificationDismissalStateSubmitting:
		return state.Value, true
	case NotificationDismissalStateAccepted:
		return state.Target, true
	default:
		return nil, false
	}
}

// notificationDismissalEqual is NotificationDismissal's derived PartialEq.
// FailureIdentity holds a pointer, so interface == is not enough.
func notificationDismissalEqual(a, b core.NotificationDismissal) bool {
	switch a := a.(type) {
	case nil:
		return b == nil
	case core.NotificationDismissalFailure:
		other, ok := b.(core.NotificationDismissalFailure)
		return ok && a.Value.Generation == other.Value.Generation &&
			a.Value.State == other.Value.State &&
			ingestOptionEqual(a.Value.LastError, other.Value.LastError)
	default:
		return a == b
	}
}

// ingestOptionEqual compares two Rust Options held as pointers.
func ingestOptionEqual[T comparable](a, b *T) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

func (d *SessionDetail) updateUnread(through uint64) {
	d.unreadAgentMessages = ingestCountAbove(d.agentMessageLatestContentOrdinals, through)
	d.unreadInterruptions = ingestCountAbove(d.interruptionEventOrdinals, through)
}

func (d *SessionDetail) clearUnread() {
	d.unreadAgentMessages = 0
	d.unreadInterruptions = 0
}

// ingestCountAbove counts the ordinals greater than through.
func ingestCountAbove(ordinals []uint64, through uint64) int {
	count := 0
	for _, ordinal := range ordinals {
		if ordinal > through {
			count++
		}
	}
	return count
}

// indexedOrdinal is a transcript index and an ordinal.
type indexedOrdinal struct {
	index   int
	ordinal uint64
}

// indexedText is a transcript index and the text found there.
type indexedText struct {
	index int
	text  string
}

// toolDiffstatKey is a terminal tool item's logical identity and revision.
type toolDiffstatKey struct {
	stableID        string
	lastChangedAtMs int64
}

// MaterializedProjectionCache is the per-item results the previous session
// projection derived, kept so the next projection can reuse them.
//
// Transcript items are shared by pointer and copied on write, so the items
// two consecutive projections agree on are the ones that are
// pointer-equal. Everything before the first difference keeps its cached
// result, and the per-item JSON work is spent only on the changed tail.
// The zero value is Rust's Default.
type MaterializedProjectionCache struct {
	// transcript is the transcript these results were derived from.
	transcript []*core.TranscriptItem
	// agentMessages are the transcript index and latest content ordinal of
	// every agent message that has content, in transcript order.
	agentMessages []indexedOrdinal
	// interruptionEvents are the transcript index and event ordinal of every
	// work-interruption marker.
	interruptionEvents []indexedOrdinal
	// lastAgentMessage is the transcript index and text of the last agent
	// message with text.
	lastAgentMessage *indexedText
	// latestAgentActivity is the transcript index and text of the latest
	// thought or tool activity.
	latestAgentActivity *indexedText
	// toolDiffstats are exact stats for terminal tool items, keyed by
	// logical identity and revision so unrelated transcript updates never
	// repeat their diff.
	toolDiffstats map[toolDiffstatKey][]string
	// convertedEntries are shared (Rust's Arc<Vec<ChatEntry>>) and never
	// mutated.
	convertedEntries   []core.ChatEntry
	convertedDiffstats map[string][]string
}

func (c *MaterializedProjectionCache) agentMessageText(stableID string) (string, bool) {
	for _, entry := range c.convertedEntries {
		if entry.Role == core.ChatRoleAgent && entry.Source.Item != nil && entry.Source.Item.StableID == stableID {
			return entry.Text, true
		}
	}
	return "", false
}

// unchangedPrefix is how many leading items this cache and transcript
// share by pointer.
func (c *MaterializedProjectionCache) unchangedPrefix(transcript []*core.TranscriptItem) int {
	count := 0
	for count < len(c.transcript) && count < len(transcript) && c.transcript[count] == transcript[count] {
		count++
	}
	return count
}

// lastAgentMessageIn is the last agent message with text in
// transcript[start:end], searched from the end so it stops at the first one
// it finds.
func lastAgentMessageIn(transcript []*core.TranscriptItem, start, end int) *indexedText {
	for index := end - 1; index >= start; index-- {
		body, ok := transcript[index].Body.(core.TranscriptBodyAgent)
		if !ok {
			continue
		}
		text := core.MaterializedChunksText(body.Chunks)
		if strings.TrimSpace(text) != "" {
			return &indexedText{index: index, text: text}
		}
	}
	return nil
}

// lastAgentMessage is the last agent message with text, scanning the
// changed tail first and reusing the previous answer when it still holds.
//
// The previous answer holds when it came from an item inside the unchanged
// prefix: nothing after that item had a message, or the previous scan would
// have stopped later. "No message at all" holds outright, because the
// previous scan covered every item the prefix is made of. Only an answer
// that came from an item that changed forces a rescan of the prefix, and
// that rescan still stops at the first message it finds.
func lastAgentMessage(transcript []*core.TranscriptItem, unchangedPrefix int, previous *MaterializedProjectionCache) *indexedText {
	if found := lastAgentMessageIn(transcript, unchangedPrefix, len(transcript)); found != nil {
		return found
	}
	switch {
	case previous.lastAgentMessage == nil:
		return nil
	case previous.lastAgentMessage.index < unchangedPrefix:
		found := *previous.lastAgentMessage
		return &found
	default:
		return lastAgentMessageIn(transcript, 0, unchangedPrefix)
	}
}

func agentActivityText(item *core.TranscriptItem) (string, bool) {
	panic("unported: mj-tui/src/ingest.rs agent_activity_text")
}

func latestAgentActivityIn(transcript []*core.TranscriptItem, start, end int) *indexedText {
	for index := end - 1; index >= start; index-- {
		if text, ok := agentActivityText(transcript[index]); ok {
			return &indexedText{index: index, text: text}
		}
	}
	return nil
}

func latestAgentActivity(transcript []*core.TranscriptItem, unchangedPrefix int, previous *MaterializedProjectionCache) *indexedText {
	if found := latestAgentActivityIn(transcript, unchangedPrefix, len(transcript)); found != nil {
		return found
	}
	switch {
	case previous.latestAgentActivity == nil:
		return nil
	case previous.latestAgentActivity.index < unchangedPrefix:
		found := *previous.latestAgentActivity
		return &found
	default:
		return latestAgentActivityIn(transcript, 0, unchangedPrefix)
	}
}

// PreparedMaterializedSessionDetail is one session's dashboard projection,
// prepared off the UI task.
type PreparedMaterializedSessionDetail struct {
	sessionID                         string
	appliedEventOrdinal               uint64
	currentTurnStartedAt              *uint64
	legacyAwaitingInput               bool
	lastActivityAtMs                  *uint64
	lastAgentMessage                  *string
	lastUserMessage                   *string
	lastAgentMessageFollowsLastUser   bool
	latestAgentActivityAfterLastUser  *string
	agentMessageLatestContentOrdinals []uint64
	unreadAgentMessages               int
	interruptionEventOrdinals         []uint64
	unreadInterruptions               int
	transcript                        *chat.TranscriptSnapshot
	queuedPrompts                     []core.QueuedPrompt
	pendingElicitations               []core.ElicitationRequest
	projection                        MaterializedProjectionCache
}

// PreparedMaterializedSessionSummary is the lightweight persisted fields
// used to restore dashboard rows at startup without constructing a full
// transcript snapshot.
type PreparedMaterializedSessionSummary struct {
	sessionID                         string
	appliedEventOrdinal               uint64
	currentTurnStartedAt              *uint64
	lastActivityAtMs                  *uint64
	lastAgentMessage                  *string
	lastUserMessage                   *string
	lastAgentMessageFollowsLastUser   bool
	agentMessageLatestContentOrdinals []uint64
	unreadAgentMessages               int
	interruptionEventOrdinals         []uint64
	unreadInterruptions               int
}

// PreparedMaterializedSessionSummaryFromMaterialized is
// PreparedMaterializedSessionSummary::from_materialized.
func PreparedMaterializedSessionSummaryFromMaterialized(summary core.MaterializedSessionSummary, viewedThroughEventOrdinal uint64) PreparedMaterializedSessionSummary {
	panic("unported: mj-tui/src/ingest.rs PreparedMaterializedSessionSummary::from_materialized")
}

// PreparedMaterializedSessionDetailFromMaterialized projects one session
// for the dashboard, reusing what previous derived for the transcript items
// that did not change.
func PreparedMaterializedSessionDetailFromMaterialized(session core.MaterializedSession, viewedThroughEventOrdinal uint64, previous MaterializedProjectionCache) PreparedMaterializedSessionDetail {
	return PreparedMaterializedSessionDetailFromAssessedMaterialized(session, nil, viewedThroughEventOrdinal, previous)
}

// PreparedMaterializedSessionDetailFromAssessedMaterialized builds
// transcript detail; semantic attention is served separately by the daemon,
// except the latest-turn rule kept for pre-64 workers.
func PreparedMaterializedSessionDetailFromAssessedMaterialized(session core.MaterializedSession, assessment *core.TurnAssessment, viewedThroughEventOrdinal uint64, previous MaterializedProjectionCache) PreparedMaterializedSessionDetail {
	panic("unported: mj-tui/src/ingest.rs PreparedMaterializedSessionDetail::from_assessed_materialized")
}

// TranscriptHydration is whether a session's transcript has arrived. The
// zero value is Rust's default, Unavailable.
type TranscriptHydration int

const (
	TranscriptHydrationUnavailable TranscriptHydration = iota
	TranscriptHydrationLoading
	// TranscriptHydrationSummary: summary content has arrived, but carries
	// no stable message identity.
	TranscriptHydrationSummary
	TranscriptHydrationReady
)

// MovePromptOrigin is one durable Move intent as the prompt-origin tracker
// reads it: Terminal is nil while the move runs, true once it completed and
// false once it failed or was cancelled.
type MovePromptOrigin struct {
	SessionID   string
	OperationID string
	Terminal    *bool
}

// SessionFilesystem is one filesystem a session writes to, with the
// daemon's verdict on the target host it belongs to.
type SessionFilesystem struct {
	View       *core.TargetStorageView
	Filesystem *core.FilesystemView
}

// NotificationDismissalResult is one entry of a "Clear notifications"
// result: the session, the dismissal submitted for it, and the ordinal the
// daemon accepted it at or why it failed.
type NotificationDismissalResult struct {
	SessionID string
	Target    core.NotificationDismissal
	Ordinal   uint64
	Err       error
}

// sessionDetailEntry is session_details.entry(id).or_default().
func (d *DashboardState) sessionDetailEntry(sessionID string) *SessionDetail {
	details := d.sessionDetails.Mut()
	detail, ok := (*details)[sessionID]
	if !ok {
		if *details == nil {
			*details = map[string]*SessionDetail{}
		}
		detail = &SessionDetail{}
		(*details)[sessionID] = detail
	}
	return detail
}

func (d *DashboardState) SetNotificationCards(cards *core.SnapshotMap[string, client.NotificationCard]) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_notification_cards")
}

func (d *DashboardState) SetSessionAttentionFor(sessionID string, attention core.SessionAttention) {
	if current, ok := d.servedAttention.Get().Get(sessionID); !ok || current != attention {
		d.servedAttention.Mut().Insert(sessionID, attention)
	}
}

func (d *DashboardState) SetFormHolders(holders core.SnapshotMap[string, string]) {
	d.formHolders = holders
}

func (d *DashboardState) SetUnreadFrontiers(frontiers core.SnapshotMap[string, uint64]) {
	d.unreadFrontiers = frontiers
}

func (d *DashboardState) SetNotificationDismissals(targets core.SnapshotMap[string, core.NotificationDismissal]) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_notification_dismissals")
}

func (d *DashboardState) SetSessionAttention(attention core.SnapshotMap[string, core.SessionAttention]) {
	same := d.servedAttention.Get().Equal(attention, func(a, b *core.SessionAttention) bool { return *a == *b })
	if !same {
		*d.servedAttention.Mut() = attention
	}
}

func (d *DashboardState) SetWorkspaceNames(names map[string]string) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_workspace_names")
}

func (d *DashboardState) SetWorkspaceName(workspaceName string) {
	if d.workspaceName != workspaceName {
		d.workspaceName = workspaceName
	}
}

// NotifyConfig is the [notify] section in force, for the host that emits
// notifications.
func (d *DashboardState) NotifyConfig() *core.NotifyConfig {
	return &d.config.Get().Notify
}

func (d *DashboardState) SetConfig(config core.Config) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_config")
}

func (d *DashboardState) SetState(state core.StateState) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_state")
}

// SetMoveOperations replaces the daemon's complete durable Move projection.
// Active intents are projected into the Sessions pane immediately, even when
// the latest session record is still stopped or provisioning; this keeps a
// moving row visible while the normal lifecycle feed catches up. Retained
// failed and cancelled intents stay attached to resume rows for explicit
// recovery.
func (d *DashboardState) SetMoveOperations(operations []core.MoveOperation) {
	snapshot := core.NewSnapshotMap[string, core.MoveOperation]()
	for _, operation := range operations {
		snapshot.Insert(operation.Selection.SessionID, operation)
	}
	d.SetMoveSnapshot(snapshot)
}

func (d *DashboardState) SetMoveSnapshot(operations core.SnapshotMap[string, core.MoveOperation]) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_move_snapshot")
}

func (d *DashboardState) BeginSessionOperation(sessionID string, kind SessionOperationKind, placeholder *core.SessionRecord) {
	startedAtEpochSeconds := uint64(max(time.Now().Unix(), 0))
	d.BeginSessionOperationAt(sessionID, kind, placeholder, startedAtEpochSeconds)
}

func (d *DashboardState) BeginSessionOperationAt(sessionID string, kind SessionOperationKind, placeholder *core.SessionRecord, startedAtEpochSeconds uint64) {
	d.BeginSessionOperationAtWithID(sessionID, kind, placeholder, startedAtEpochSeconds, nil, true)
}

func (d *DashboardState) BeginSessionOperationAtWithID(sessionID string, kind SessionOperationKind, placeholder *core.SessionRecord, startedAtEpochSeconds uint64, operationID *string, cancellable bool) {
	if placeholder == nil {
		if record := d.SessionRecord(sessionID); record != nil {
			cloned := *record
			placeholder = &cloned
		}
	}
	operations := d.sessionOperations.Mut()
	if *operations == nil {
		*operations = map[string]*SessionOperationDisplay{}
	}
	(*operations)[sessionID] = &SessionOperationDisplay{
		kind:                  kind,
		operationID:           operationID,
		cancellable:           cancellable,
		startedAtEpochSeconds: startedAtEpochSeconds,
		placeholder:           placeholder,
	}
	d.applyOperationProjection()
	d.RebuildResumeRows()
	d.clampSelections()
}

func (d *DashboardState) ReplaceSessionOperationStages(sessionID string, stages []ProvisionStageStart) {
	operation, ok := (*d.sessionOperations.Mut())[sessionID]
	if !ok {
		return
	}
	sorted := make([]ProvisionStageStart, 0, len(stages))
	for _, stage := range stages {
		// Collecting into a BTreeMap keeps the last value for each stage.
		at, found := slices.BinarySearchFunc(sorted, stage, func(a, b ProvisionStageStart) int { return a.Stage.Compare(b.Stage) })
		if found {
			sorted[at] = stage
		} else {
			sorted = slices.Insert(sorted, at, stage)
		}
	}
	operation.activeStages = sorted
}

func (d *DashboardState) SetSessionOperationIdentity(sessionID string, operationID *string, cancellable bool) {
	if operation, ok := (*d.sessionOperations.Mut())[sessionID]; ok {
		operation.operationID = operationID
		operation.cancellable = cancellable
	}
}

// SetResumeDestination records the profile/target a resume is moving
// sessionID to, so its in-flight "Resuming" row shows the destination rather
// than the session's pre-resume profile/target. A finished or unknown
// operation is left alone.
func (d *DashboardState) SetResumeDestination(sessionID string, profileID string, targetTemplateID string) {
	if operation, ok := (*d.sessionOperations.Mut())[sessionID]; ok {
		operation.resumeDestination = &ResumeDestination{ProfileID: profileID, TargetTemplateID: targetTemplateID}
	}
}

func (d *DashboardState) FinishSessionOperation(sessionID string) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::finish_session_operation")
}

// releaseFinishedConversation: a lifecycle ending releases its pane and
// therefore its selection in the same transition. A manually selected
// historical row may still be inspected without trying to attach to a
// stopped worker.
func (d *DashboardState) releaseFinishedConversation(sessionID string) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::release_finished_conversation")
}

func (d *DashboardState) SessionOperationKind(sessionID string) (SessionOperationKind, bool) {
	operation, ok := (*d.sessionOperations.Get())[sessionID]
	if !ok {
		return 0, false
	}
	return operation.kind, true
}

// MovePromptOrigins lists every durable Move intent with whether it ended
// and how. Surfaces use this to keep the transition row while intermediate
// durable records (Stopped/Provisioning/Running) catch up.
func (d *DashboardState) MovePromptOrigins() []MovePromptOrigin {
	origins := []MovePromptOrigin{}
	for operation := range d.moveOperations.Values() {
		var terminal *bool
		switch operation.Phase {
		case core.MovePhaseCompleted:
			terminal = new(bool)
			*terminal = true
		case core.MovePhaseFailed, core.MovePhaseCancelled:
			terminal = new(bool)
		}
		origins = append(origins, MovePromptOrigin{
			SessionID:   operation.Selection.SessionID,
			OperationID: operation.OperationID,
			Terminal:    terminal,
		})
	}
	return origins
}

func (d *DashboardState) MoveOperationActive(sessionID string) bool {
	operation, ok := d.moveOperations.Get(sessionID)
	return ok && operation.IsActive()
}

func (d *DashboardState) applyOperationProjection() {
	state := d.state.Mut()
	for _, sessionID := range slices.Sorted(maps.Keys(*d.sessionOperations.Get())) {
		operation := (*d.sessionOperations.Get())[sessionID]
		if operation.placeholder != nil && !state.Sessions.ContainsKey(sessionID) {
			state.Sessions.Insert(sessionID, *operation.placeholder)
		}
	}
}

// usagePricedProfiles is the profiles of config billed per use: their quota
// report says so, or their harness implies it (OpenCode reports no
// allowance). A profile whose billing neither decides counts as a
// subscription.
func (d *DashboardState) usagePricedProfiles(config *core.Config) map[string]struct{} {
	priced := map[string]struct{}{}
	for id, profile := range config.EnabledProfiles() {
		var report *client.ProfileQuota
		if quota, ok := d.quotas[id]; ok {
			report = &quota
		}
		if client.ProfileQuotaBillingFor(profile.Kind, report) == client.QuotaBillingUsage {
			priced[id] = struct{}{}
		}
	}
	return priced
}

func (d *DashboardState) SetQuotas(quotas map[string]client.ProfileQuota) {
	for id := range d.quotaRefreshing {
		if _, ok := quotas[id]; ok {
			delete(d.quotaRefreshing, id)
		}
	}
	d.quotas = quotas
	d.clampSelections()
	d.refreshReviewChoices()
}

// SetQuotaSnapshot shows what the daemon says about quota. The daemon is
// the only process that probes, so this replaces what was held rather than
// merging into it: the reports, and which profiles it is asking a provider
// about now.
func (d *DashboardState) SetQuotaSnapshot(snapshot client.QuotaSnapshot) {
	d.quotas = snapshot.Reports
	d.quotaRefreshing = snapshot.Probing
	d.clampSelections()
	d.refreshReviewChoices()
}

func (d *DashboardState) ApplyQuota(quota client.ProfileQuota) {
	delete(d.quotaRefreshing, quota.ProfileID)
	if d.quotas == nil {
		d.quotas = map[string]client.ProfileQuota{}
	}
	d.quotas[quota.ProfileID] = quota
	d.clampSelections()
	d.refreshReviewChoices()
}

// SetTargetStorage installs the daemon's storage verdicts, replacing what
// was held.
func (d *DashboardState) SetTargetStorage(views []core.TargetStorageView) {
	d.targetStorage = views
}

// sessionFilesystems is the filesystems a session writes to, with the
// daemon's verdict on each: its worker root, workspace, staged profile home
// and /tmp.
func (d *DashboardState) sessionFilesystems(sessionID string) []SessionFilesystem {
	panic("unported: mj-tui/src/ingest.rs DashboardState::session_filesystems")
}

// sessionStorageProblem is "disk full: …" while one of the session's own
// filesystems is full.
func (d *DashboardState) sessionStorageProblem(sessionID string) (string, bool) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::session_storage_problem")
}

func (d *DashboardState) SetHostCapacities(capacities []client.RuntimeHostCapacity) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::set_host_capacities")
}

// ApplyMaterializedSession replaces dashboard detail with the controller's
// durable logical-session projection. Unread is a count of logical agent
// messages with content added after the last detach cursor, never a count
// of stream chunks.
func (d *DashboardState) ApplyMaterializedSession(session *core.MaterializedSession) {
	var viewedThroughEventOrdinal uint64
	if record, ok := d.state.Get().Sessions.Get(session.SessionID); ok {
		viewedThroughEventOrdinal = record.ViewedThroughEventOrdinal
	}
	previous := d.TakeProjectionCache(session.SessionID)
	d.ApplyPreparedMaterializedSession(PreparedMaterializedSessionDetailFromMaterialized(*session, viewedThroughEventOrdinal, previous))
}

// TakeProjectionCache hands the last projection's per-item results to the
// next projection, which runs off the UI task. A projection that never
// comes back, or one that arrives too late to apply, only costs the next
// one a full rescan.
func (d *DashboardState) TakeProjectionCache(sessionID string) MaterializedProjectionCache {
	detail, ok := (*d.sessionDetails.Mut())[sessionID]
	if !ok {
		return MaterializedProjectionCache{}
	}
	detail.transcriptHydration = TranscriptHydrationLoading
	taken := detail.projection
	detail.projection = MaterializedProjectionCache{}
	return taken
}

func (d *DashboardState) ApplyPreparedMaterializedSession(prepared PreparedMaterializedSessionDetail) bool {
	panic("unported: mj-tui/src/ingest.rs DashboardState::apply_prepared_materialized_session")
}

// ApplyPreparedMaterializedSessionSummary applies the small startup
// projection while leaving transcript hydration pending for the live
// session's complete snapshot.
func (d *DashboardState) ApplyPreparedMaterializedSessionSummary(prepared PreparedMaterializedSessionSummary) bool {
	panic("unported: mj-tui/src/ingest.rs DashboardState::apply_prepared_materialized_session_summary")
}

func (d *DashboardState) notificationDismissalPending(sessionID string) bool {
	detail, ok := (*d.sessionDetails.Get())[sessionID]
	return ok && detail.notificationDismissal != nil
}

func (d *DashboardState) notificationDismissalTarget(sessionID string) (core.NotificationDismissal, bool) {
	target, ok := d.notificationDismissals.Get().Get(sessionID)
	return target, ok
}

func (d *DashboardState) beginDismissNotification() DashboardAction {
	// While the resume dialog is open, `d` acts on its selected row only; a
	// native or archive row must not fall through to the dashboard
	// selection behind the dialog.
	var target string
	var ok bool
	if _, resume := d.mode.(ModeResumeDialog); resume {
		target, ok = d.resumeCommandSessionID()
	} else {
		target, ok = d.commandSessionID()
	}
	if !ok {
		return DashboardActionNone{}
	}
	if action, ok := d.beginDismissNotificationFor(target); ok {
		return action
	}
	return DashboardActionNone{}
}

// beginDismissNotificationFor starts the dismissal of one session's current
// notification, unless one is already being submitted for it.
func (d *DashboardState) beginDismissNotificationFor(sessionID string) (DashboardAction, bool) {
	if d.notificationDismissalPending(sessionID) {
		return nil, false
	}
	target, ok := d.submitNotificationDismissal(sessionID)
	if !ok {
		return nil, false
	}
	return DashboardActionDismissNotification{SessionID: sessionID, Target: target}, true
}

// submitNotificationDismissal marks the session's current notification as
// being dismissed and returns its target, or false when it has nothing to
// dismiss. Every dismissal starts here, so its result is matched against
// this mark.
func (d *DashboardState) submitNotificationDismissal(sessionID string) (core.NotificationDismissal, bool) {
	target, ok := d.notificationDismissalTarget(sessionID)
	if !ok {
		return nil, false
	}
	d.submitNotificationTarget(sessionID, target)
	return target, true
}

func (d *DashboardState) submitNotificationTarget(sessionID string, target core.NotificationDismissal) {
	detail := d.sessionDetailEntry(sessionID)
	detail.notificationDismissal = NotificationDismissalStateSubmitting{Value: target}
	delete(d.notifications.errors, sessionID)
}

func (d *DashboardState) NotificationDismissalFinished(sessionID string, target core.NotificationDismissal, ordinal uint64, err error) {
	applied, ok := d.applyNotificationDismissal(sessionID, target, ordinal, err)
	switch {
	case !ok:
	case applied == nil:
		d.SetNotice("Notification dismissed.")
	default:
		d.SetNotice("Could not dismiss notification: " + applied.Error())
	}
}

// NotificationsDismissed takes the results of one "Clear notifications":
// each applied as a single dismissal is, then one notice, so a later
// success cannot hide an earlier failure.
func (d *DashboardState) NotificationsDismissed(results []NotificationDismissalResult) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::notifications_dismissed")
}

// applyNotificationDismissal applies one dismissal result to the session it
// was submitted for. It returns the result to report (nil for success) and
// false when the notification has since changed and the result no longer
// applies.
func (d *DashboardState) applyNotificationDismissal(sessionID string, target core.NotificationDismissal, ordinal uint64, err error) (error, bool) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::apply_notification_dismissal")
}

func (d *DashboardState) SetCurrentStepStart(sessionID string, timestampMs *int64) {
	var timestamp *uint64
	if timestampMs != nil && *timestampMs >= 0 {
		value := uint64(*timestampMs)
		timestamp = &value
	}
	detail := d.sessionDetailEntry(sessionID)
	if !ingestOptionEqual(detail.currentStepStartedAtMs, timestamp) {
		detail.currentStepStartedAtMs = timestamp
	}
}

// SessionHarnessKind is the session's harness kind, for harness-specific
// readings of its state.
func (d *DashboardState) SessionHarnessKind(sessionID string) (core.HarnessKind, bool) {
	record, ok := d.state.Get().Sessions.Get(sessionID)
	if !ok {
		return "", false
	}
	return record.HarnessKind, true
}

// SetSessionActivity records what a session is doing beyond its turn clock,
// so a row can say that an idle agent still has a command of its own
// running.
func (d *DashboardState) SetSessionActivity(sessionID string, activity client.SessionActivity) {
	d.sessionDetailEntry(sessionID).activity = activity
}

// SetSessionCurrentConfig records the session's current model and effort as
// its worker reports them, for the session menu's top line.
func (d *DashboardState) SetSessionCurrentConfig(sessionID string, model *string, effort *string) {
	detail := d.sessionDetailEntry(sessionID)
	if !ingestOptionEqual(detail.currentModel, model) || !ingestOptionEqual(detail.currentEffort, effort) {
		detail.currentModel = model
		detail.currentEffort = effort
	}
}

// SetQuestionMessage records the current open question target, if the
// worker located one.
func (d *DashboardState) SetQuestionMessage(sessionID string, target *string, supportsOpenRequest bool) {
	detail := d.sessionDetailEntry(sessionID)
	detail.questionMessage = target
	detail.supportsOpenRequest = supportsOpenRequest
}

func (d *DashboardState) QuestionMessage(sessionID string) (string, bool) {
	if d.notificationCards != nil {
		if card := d.notificationCards.GetShared(sessionID); card != nil {
			if entry := card.ShownQuestion(); entry != nil {
				if found, ok := entry.Message.(core.EntryMessageFound); ok {
					return found.Value, true
				}
				return "", false
			}
		}
	}
	detail, ok := (*d.sessionDetails.Get())[sessionID]
	if !ok || detail.questionMessage == nil {
		return "", false
	}
	return *detail.questionMessage, true
}

// SetSessionConnectivity records whether the controller can currently reach
// a session's relay worker. An unreachable session renders its summary band
// red.
func (d *DashboardState) SetSessionConnectivity(sessionID string, connected bool) {
	sessions := d.unreachableSessions.Mut()
	if connected {
		delete(*sessions, sessionID)
		return
	}
	if *sessions == nil {
		*sessions = map[string]struct{}{}
	}
	(*sessions)[sessionID] = struct{}{}
}

// SetSessionReviews replaces the review projection published by the
// controller. The full replacement is intentional: a missing session means
// its review closed and must disappear from every row immediately.
func (d *DashboardState) SetSessionReviews(reviews []client.RuntimeReviewView) {
	next := map[string][]client.RuntimeReviewView{}
	for _, review := range reviews {
		next[review.SessionID] = append(next[review.SessionID], review)
	}
	*d.sessionReviewsField.Mut() = next
}

// sessionReviews is the authoritative reviews currently open for a session,
// in the order the daemon published them.
func (d *DashboardState) sessionReviews(sessionID string) []client.RuntimeReviewView {
	return (*d.sessionReviewsField.Get())[sessionID]
}

// SyncTranscriptHydration projects the existing summary/projection task
// owners into hydration status. Missing content alone never means a request
// is running.
func (d *DashboardState) SyncTranscriptHydration(pending map[string]struct{}) {
	panic("unported: mj-tui/src/ingest.rs DashboardState::sync_transcript_hydration")
}

func (d *DashboardState) MarkTranscriptUnavailable(sessionID string) {
	detail := d.sessionDetailEntry(sessionID)
	if detail.transcriptHydration != TranscriptHydrationUnavailable {
		detail.transcriptHydration = TranscriptHydrationUnavailable
	}
}

func (d *DashboardState) ApplyQueuedPrompts(sessionID string, queuedPrompts []core.QueuedPrompt) {
	d.sessionDetailEntry(sessionID).queuedPrompts = queuedPrompts
}

func (d *DashboardState) PatchCheckpointArchiveSizes(sizes map[string]*uint64) {
	if len(sizes) == 0 {
		return
	}
	if d.checkpointArchiveSizes == nil {
		d.checkpointArchiveSizes = map[string]*uint64{}
	}
	for id, size := range sizes {
		if size != nil {
			d.checkpointArchiveSizes[id] = size
		} else {
			delete(d.checkpointArchiveSizes, id)
		}
	}
	d.RebuildResumeRows()
}

func (d *DashboardState) ApplyCheckpointArchiveSizes(sizes map[string]*uint64) {
	same := len(sizes) == len(d.checkpointArchiveSizes)
	for id, size := range sizes {
		held, ok := d.checkpointArchiveSizes[id]
		if !ok || !ingestOptionEqual(held, size) {
			same = false
			break
		}
	}
	if !same {
		d.checkpointArchiveSizes = sizes
		d.RebuildResumeRows()
	}
}

// ShareNotices installs the process-wide notifications bar, so every view
// reports through one shared slot.
func (d *DashboardState) ShareNotices(notices chat.Notices) {
	d.notices = notices
}

func (d *DashboardState) SetNotice(notice string) {
	d.notices.Set(notice)
}

func (d *DashboardState) SetFailureNotice(notice string) {
	d.notices.SetFailure(notice)
}

func (d *DashboardState) ReplaceNoticeIf(expected string, replacement string) bool {
	return d.notices.ReplaceIf(expected, replacement)
}

func (d *DashboardState) ClearNotice() {
	d.notices.Clear()
}

// Notice is the current shared notice, if any.
func (d *DashboardState) Notice() (string, bool) {
	return d.notices.Current()
}

func (d *DashboardState) SetSessionCPU(cpu core.SnapshotMap[string, client.SessionCpuView]) {
	*d.sessionCPU.Mut() = cpu
}
