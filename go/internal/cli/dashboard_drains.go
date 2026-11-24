// Port of mj-cli/src/dashboard/drains.rs.
//
// The dashboard loop's drains: each applies what one background feed has
// queued since the last pass.

package cli

import (
	"maps"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/controller"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// remoteLifecycleObservation: operation identity and the notice already
// reported belong to one observation.
type remoteLifecycleObservation struct {
	operationID string
	// notice is nil when the daemon reported none.
	notice *string
}

// observeRemoteLifecycle records lifecycle as the observed operation for
// its session and reports whether it supersedes an observed one.
func observeRemoteLifecycle(
	operations map[string]remoteLifecycleObservation,
	dashboard *tui.DashboardState,
	lifecycle *client.RuntimeLifecycleView,
) bool {
	previous, hadPrevious := operations[lifecycle.SessionID]
	var notice *string
	if lifecycle.Notice != nil {
		text := *lifecycle.Notice
		notice = &text
	}
	operations[lifecycle.SessionID] = remoteLifecycleObservation{
		operationID: lifecycle.OperationID,
		notice:      notice,
	}
	supersedes := hadPrevious && previous.operationID != lifecycle.OperationID
	if notice != nil && (supersedes || !hadPrevious || previous.notice == nil || *previous.notice != *notice) {
		dashboard.SetNotice(*notice)
	}
	return supersedes
}

// lifecycleCompletion is Rust's LifecycleCompletion: Current, carrying the
// local operation the completion belongs to (nil when none was waiting), or
// Superseded.
type lifecycleCompletion struct {
	operation  *activeLifecycleOperation
	superseded bool
}

// takeLifecycleCompletion removes and returns the local operation update
// completes, unless a newer local operation for the session has replaced it.
func takeLifecycleCompletion(
	operations map[string]*activeLifecycleOperation,
	update *dashboardLifecycleUpdate,
) lifecycleCompletion {
	current, ok := operations[update.sessionID]
	if !ok {
		return lifecycleCompletion{}
	}
	if current.cancelled != update.operation {
		return lifecycleCompletion{superseded: true}
	}
	delete(operations, update.sessionID)
	return lifecycleCompletion{operation: current}
}

func (c *dashboardContext) cancelBackgroundWork() {
	panic("unported: mj-cli/src/dashboard/drains.rs cancel_background_work")
}

// drainFeeds takes every message queued behind the one that woke the loop,
// feed by feed, in the order the UI depends on. It applies whatever the
// background feeds have queued, and reports whether any message arrived.
// The report is what lets a timer wakeup that had nothing of its own to show
// still draw the message that rode with it.
func (c *dashboardContext) drainFeeds() bool {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_feeds")
}

// reconcileComposerOwnership: worker feeds may remain warm while one live
// composer belongs to standby.
func (c *dashboardContext) reconcileComposerOwnership() {
	panic("unported: mj-cli/src/dashboard/drains.rs reconcile_composer_ownership")
}

func (c *dashboardContext) collectPreparedStartupPrompts() bool {
	panic("unported: mj-cli/src/dashboard/drains.rs collect_prepared_startup_prompts")
}

// clockTickRedraws is whether the once-a-second clock has anything new to
// put on screen.
//
// A notice is the only report several background failures get, and any task
// can write it through the shared slot without waking this loop, so the slot
// is compared with what the frame on screen was drawn from.
func (c *dashboardContext) clockTickRedraws() bool {
	panic("unported: mj-cli/src/dashboard/drains.rs clock_tick_redraws")
}

// drainRuntimeReviews hands the open chat whatever turn review the daemon
// is running for it.
//
// The chat draws the pane and sends the resolutions; it hosts nothing. A
// session with no review gets none, which closes the pane.
func (c *dashboardContext) drainRuntimeReviews() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_runtime_reviews")
}

// drainRuntimeNotices reports each background notice the daemon published
// since the last one this surface showed.
func (c *dashboardContext) drainRuntimeNotices() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_runtime_notices")
}

// drainUIRequests applies each dashboard request published since the last
// one applied.
func (c *dashboardContext) drainUIRequests() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_ui_requests")
}

// applyRuntimeReviewsToChats applies the retained daemon projection to
// every warm chat. Absence is meaningful: it closes a review the previous
// projection held.
func (c *dashboardContext) applyRuntimeReviewsToChats() {
	panic("unported: mj-cli/src/dashboard/drains.rs apply_runtime_reviews_to_chats")
}

// applyRuntimeReviewToChat is the same for one conversation, for a chat
// that has just opened.
func (c *dashboardContext) applyRuntimeReviewToChat(sessionID string) {
	panic("unported: mj-cli/src/dashboard/drains.rs apply_runtime_review_to_chat")
}

func (c *dashboardContext) drainQuotaUpdates() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_quota_updates")
}

func (c *dashboardContext) drainWorkerUpdates() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_worker_updates")
}

func (c *dashboardContext) drainRuntimeState() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_runtime_state")
}

func (c *dashboardContext) applyRuntimeLifecycles(lifecycles []client.RuntimeLifecycleView) {
	panic("unported: mj-cli/src/dashboard/drains.rs apply_runtime_lifecycles")
}

// refreshChatContext hands the open conversation the surface's current
// view of the config and its own session record. The chat snapshots both
// when it opens, so without this a long-lived chat would keep offering
// reviewer profiles that a config reload has since renamed or removed.
func (c *dashboardContext) refreshChatContext() {
	panic("unported: mj-cli/src/dashboard/drains.rs refresh_chat_context")
}

func (c *dashboardContext) drainRuntimeConfig() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_runtime_config")
}

func (c *dashboardContext) applyRuntimeRecords(
	records core.SnapshotMap[string, core.SessionRecord],
	runtimeSessions core.SnapshotMap[string, client.RuntimeSessionView],
	subagents core.SnapshotMap[string, core.SubagentMetadata],
	environmentOwners core.SnapshotMap[string, string],
	lastSubagentPolicies map[string]core.SubagentPolicy,
) {
	panic("unported: mj-cli/src/dashboard/drains.rs apply_runtime_records")
}

func (c *dashboardContext) drainImportProfiles() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_import_profiles")
}

func (c *dashboardContext) drainImportTasks() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_import_tasks")
}

func (c *dashboardContext) drainLifecycleUpdates() {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_lifecycle_updates")
}

func (c *dashboardContext) drainDashboardIo() bool {
	panic("unported: mj-cli/src/dashboard/drains.rs drain_dashboard_io")
}

// refreshOpenChats hands every open conversation the controller's current
// config and its own session record, so its header names the session the
// way the Sessions row does (`listed_title`) and its other context matches
// the dashboard's.
func refreshOpenChats(
	chats map[string]*chat.ActiveChat,
	ctrl *controller.Controller,
	dashboard *tui.DashboardState,
	runtimeSessions core.SnapshotMap[string, client.RuntimeSessionView],
) {
	for _, sessionID := range slices.Sorted(maps.Keys(chats)) {
		conversation := chats[sessionID]
		applyPolicyConfigKey(conversation, runtimeSessions)
		record := ctrl.State.Sessions.GetShared(conversation.SessionID())
		var project *core.SessionRecord
		if record != nil {
			project = ctrl.State.ProjectIdentitySession(record)
		}
		conversation.RefreshContext(&ctrl.Config, record, project)
		if dashboard.GoMode() != nil {
			conversation.SetDisplayTitle(dashboard.GoConversationTitle(conversation.SessionID()))
		}
	}
	refreshSubagentCounts(chats, dashboard)
}

func applyPolicyConfigKey(
	conversation *chat.ActiveChat,
	sessions core.SnapshotMap[string, client.RuntimeSessionView],
) {
	var keys []string
	if view := sessions.GetShared(conversation.SessionID()); view != nil {
		keys = view.PolicyConfigKeys
	}
	conversation.SetPolicyConfigKeys(keys)
}

// refreshSubagentCounts hands every open conversation the dashboard's count
// of its sub-agents and of how many are working, which the composer footer
// displays.
//
// The working count changes whenever a child's worker reports different
// activity, which is not a change to any record, so it is refreshed from the
// worker updates as well as with the records (I2-7).
func refreshSubagentCounts(chats map[string]*chat.ActiveChat, dashboard *tui.DashboardState) {
	for _, sessionID := range slices.Sorted(maps.Keys(chats)) {
		conversation := chats[sessionID]
		session := conversation.SessionID()
		conversation.SetSubagentCount(dashboard.SubagentCountFor(session))
		conversation.SetReviewerCounts(dashboard.ReviewerCountsFor(session))
		conversation.SetSubagentWorkingCount(dashboard.WorkingSubagentCountFor(session))
	}
}

// nextWorkspace is the tab to show after a feed frame: a workspace the user
// just created once the frame carries it, else the active tab while it
// exists, else the first. pending is the dashboard's pending selection,
// cleared once the frame carries it (Rust: `&mut Option<String>`).
func nextWorkspace(active *string, pending **string, names map[string]string) *string {
	if created := *pending; created != nil {
		if _, ok := names[*created]; ok {
			*pending = nil
			return created
		}
	}
	if active != nil {
		if _, ok := names[*active]; ok {
			id := *active
			return &id
		}
	}
	if len(names) == 0 {
		return nil
	}
	first := slices.Min(slices.Collect(maps.Keys(names)))
	return &first
}
