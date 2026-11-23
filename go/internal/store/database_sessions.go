// Port of mj-store/src/database/sessions.rs.

package store

import (
	"context"
	"database/sql"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SaveImportedSession persists one operational session without rewriting
// unrelated controller state. Dashboard lifecycle jobs use this path so
// independent jobs can commit concurrently without restoring stale copies of
// other sessions.
func SaveImportedSession(session *core.SessionRecord, materialized *core.MaterializedSession) error {
	panic("unported: mj-store/src/database/sessions.rs save_imported_session")
}

func SaveSession(session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/sessions.rs save_session")
}

// HostContainerSizeChoice is a host and the container size most recently
// launched on it: Rust's `(String, HostContainerSize)` in the optional
// container size a session save remembers.
type HostContainerSizeChoice struct {
	Host string
	Size core.HostContainerSize
}

func SaveNewSession(session *core.SessionRecord, containerSize *HostContainerSizeChoice, rememberLaunchChoices bool, rememberContainerSize bool) error {
	return SaveNewSessionWithPreferences(session, containerSize, NewSessionPreferences{
		RememberLaunchChoices:  rememberLaunchChoices,
		RememberContainerSize:  rememberContainerSize,
		RememberSubagentPolicy: rememberLaunchChoices,
	})
}

type NewSessionPreferences struct {
	RememberLaunchChoices  bool
	RememberContainerSize  bool
	RememberSubagentPolicy bool
}

func SaveNewSessionWithPreferences(session *core.SessionRecord, containerSize *HostContainerSizeChoice, preferences NewSessionPreferences) error {
	panic("unported: mj-store/src/database/sessions.rs save_new_session_with_preferences")
}

// saveNewSessionTo is test-only in Rust.
func saveNewSessionTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice, rememberLaunchChoices bool, rememberContainerSize bool) error {
	return saveNewSessionWithPreferencesTo(path, session, containerSize, NewSessionPreferences{
		RememberLaunchChoices:  rememberLaunchChoices,
		RememberContainerSize:  rememberContainerSize,
		RememberSubagentPolicy: rememberLaunchChoices,
	})
}

func saveNewSessionWithPreferencesTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice, preferences NewSessionPreferences) error {
	panic("unported: mj-store/src/database/sessions.rs save_new_session_with_preferences_to")
}

func saveSubagentPreference(tx *sql.Tx, profile string, policy core.SubagentPolicy) error {
	panic("unported: mj-store/src/database/sessions.rs save_subagent_preference")
}

// SetPublicationAssessmentIfCurrent updates publication evidence only while
// the exact stopped checkpoint is still current. An age scan may race a
// user's Resume or a new checkpoint.
func SetPublicationAssessmentIfCurrent(sessionID string, assessment *core.PublicationAssessment) (bool, error) {
	panic("unported: mj-store/src/database/sessions.rs set_publication_assessment_if_current")
}

// OracleAlreadyLive: a primary may have one live oracle; registration
// refused another.
type OracleAlreadyLive struct{}

func (OracleAlreadyLive) Error() string { return "this session already has a live oracle" }

// SaveSubagentSession persists a borrowed-target child and its parent
// relationship atomically.
func SaveSubagentSession(session *core.SessionRecord, subagent *core.SubagentRecord) error {
	return RegisterSubagentWithStartup(session, subagent, nil, nil, nil, false)
}

// RegisterSubagentWithStartup publishes the child and its complete initial
// input in the same transaction. Recovery must never find a newly registered
// worker without its first brief.
func RegisterSubagentWithStartup(session *core.SessionRecord, subagent *core.SubagentRecord, startup []StartupDelivery, reportDir *string, category *string, ownEnvironment bool) error {
	return RegisterSubagentWithStartupAdmitted(session, subagent, startup, reportDir, category, ownEnvironment,
		func(*core.StateState, *core.SubagentRecord) error { return nil })
}

// RegisterSubagentWithStartupAdmitted runs the controller's admission policy
// against the latest committed state in the same serialized write that
// registers the child.
func RegisterSubagentWithStartupAdmitted(session *core.SessionRecord, subagent *core.SubagentRecord, startup []StartupDelivery, reportDir *string, category *string, ownEnvironment bool, admit func(*core.StateState, *core.SubagentRecord) error) error {
	panic("unported: mj-store/src/database/sessions.rs register_subagent_with_startup_admitted")
}

// MarkSubagentTurnNoticed records the child turn whose completion notice the
// parent already has.
func MarkSubagentTurnNoticed(childSessionID string, turn uint64) error {
	panic("unported: mj-store/src/database/sessions.rs mark_subagent_turn_noticed")
}

func LoadSubagent(childSessionID string) (*core.SubagentRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs load_subagent")
}

func SetSubagentResumeWithParent(childSessionID string, resumeWithParent bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_subagent_resume_with_parent")
}

// SubagentHasHandedBack is whether a sub-agent child has handed back its
// report for its parent's newest task, from what the store holds; see
// core.HasHandedBack. A session that is not a child, or that has never
// finished a turn, has not.
func SubagentHasHandedBack(childSessionID string, liveness *core.Liveness) (bool, error) {
	panic("unported: mj-store/src/database/sessions.rs subagent_has_handed_back")
}

func ListSubagents(parentSessionID string) ([]core.SubagentRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs list_subagents")
}

// SubagentListingDetails is a child's durable role and latest turn selection
// for the parent's listing.
type SubagentListingDetails struct {
	Relation  core.SubagentRecord
	Category  *string
	Selection core.UsageSelection
}

func ListSubagentDetails(parent string) ([]SubagentListingDetails, error) {
	panic("unported: mj-store/src/database/sessions.rs list_subagent_details")
}

// ListSubagentIDs is a parent's child session ids in registration order,
// read from the index without decoding any record.
func ListSubagentIDs(parentSessionID string) ([]string, error) {
	panic("unported: mj-store/src/database/sessions.rs list_subagent_ids")
}

// ListSubagentsAmong is the parent's records for the given children, in
// registration order. A parent can hold thousands of children; a caller that
// needs a few decodes only those.
func ListSubagentsAmong(parentSessionID string, childSessionIDs []string) ([]core.SubagentRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs list_subagents_among")
}

// RecordStoppedSubagents records, on the parent's session, the sub-agents its
// suspend is about to stop. A child already listed keeps its place and takes
// the newer details, so a suspend that runs again after a restart lists each
// child once.
func RecordStoppedSubagents(parentSessionID string, stopped []core.StoppedSubagent) error {
	panic("unported: mj-store/src/database/sessions.rs record_stopped_subagents")
}

func recordStoppedSubagentsTo(path string, parentSessionID string, stopped []core.StoppedSubagent) error {
	panic("unported: mj-store/src/database/sessions.rs record_stopped_subagents_to")
}

// LoadStoppedSubagents is the sub-agents a suspend of this session stopped
// and its model has not been told about yet, in the order they were
// recorded.
func LoadStoppedSubagents(parentSessionID string) ([]core.StoppedSubagent, error) {
	return loadStoppedSubagentsFrom(DatabasePath(), parentSessionID)
}

func loadStoppedSubagentsFrom(path string, parentSessionID string) ([]core.StoppedSubagent, error) {
	panic("unported: mj-store/src/database/sessions.rs load_stopped_subagents_from")
}

// ClearStoppedSubagents forgets the stopped sub-agents whose note the
// parent's relay has taken. Only the named children go, so a child listed
// after the note was built waits for the next one.
func ClearStoppedSubagents(parentSessionID string, childSessionIDs []string) error {
	panic("unported: mj-store/src/database/sessions.rs clear_stopped_subagents")
}

func clearStoppedSubagentsFrom(path string, parentSessionID string, childSessionIDs []string) error {
	panic("unported: mj-store/src/database/sessions.rs clear_stopped_subagents_from")
}

// LoadSessionState is one session's stored lifecycle state; false when the
// store holds no such session.
func LoadSessionState(sessionID string) (core.SessionState, bool, error) {
	panic("unported: mj-store/src/database/sessions.rs load_session_state")
}

// LoadSubagentReport is everything recorded about one child's report. A
// child with nothing recorded yet reads as the empty report.
func LoadSubagentReport(childSessionID string) (core.SubagentReport, error) {
	return loadSubagentReportFrom(DatabasePath(), childSessionID)
}

func loadSubagentReportFrom(path string, childSessionID string) (core.SubagentReport, error) {
	panic("unported: mj-store/src/database/sessions.rs load_subagent_report_from")
}

// loadSubagentReportWith is a child's recorded report, or nil when nothing
// is recorded for it.
func loadSubagentReportWith(connection Queryer, childSessionID string) (*core.SubagentReport, error) {
	panic("unported: mj-store/src/database/sessions.rs load_subagent_report_with")
}

// RecordSubagentReportDir records the directory Mjolnir created for a
// child's report files. It is recorded only for a sub-agent child.
func RecordSubagentReportDir(childSessionID string, reportDir string) error {
	panic("unported: mj-store/src/database/sessions.rs record_subagent_report_dir")
}

func recordSubagentReportDirTo(path string, childSessionID string, reportDir string) error {
	panic("unported: mj-store/src/database/sessions.rs record_subagent_report_dir_to")
}

func recordSubagentReportDirWith(connection Queryer, childSessionID string, reportDir string) error {
	_, err := connection.Exec(`INSERT INTO subagent_handbacks(child_session_id, report_dir)
         SELECT ?1, ?2 WHERE EXISTS (
             SELECT 1 FROM subagent_sessions WHERE child_session_id = ?1
         )
         ON CONFLICT(child_session_id) DO UPDATE SET report_dir = excluded.report_dir`,
		childSessionID, reportDir)
	return err
}

// RecordSubagentPrompt records that the parent gave a child a prompt,
// accepted at ordinal. It is recorded only for a sub-agent child, and never
// moves backwards.
func RecordSubagentPrompt(childSessionID string, ordinal uint64) error {
	panic("unported: mj-store/src/database/sessions.rs record_subagent_prompt")
}

func recordSubagentPromptTo(path string, childSessionID string, ordinal uint64) error {
	panic("unported: mj-store/src/database/sessions.rs record_subagent_prompt_to")
}

// RecordSubagentHandback records a child's report for the turn it names,
// unless a report for that turn is already recorded. Returns whether this
// one was recorded: a child delivers one report per turn, and the check and
// the write are one transaction so two racing calls cannot both be accepted.
func RecordSubagentHandback(childSessionID string, handback *core.SubagentHandback) (bool, error) {
	panic("unported: mj-store/src/database/sessions.rs record_subagent_handback")
}

func recordSubagentHandbackTo(path string, childSessionID string, handback *core.SubagentHandback) (bool, error) {
	panic("unported: mj-store/src/database/sessions.rs record_subagent_handback_to")
}

// RecordHandbackReminder records the reminder sent after a child's turn
// ended without a report.
func RecordHandbackReminder(childSessionID string, reminder *core.HandbackReminder) error {
	panic("unported: mj-store/src/database/sessions.rs record_handback_reminder")
}

// RecordHandbackReminderFailed records that the reminder for a turn could
// not be sent, so the child's last message stands as its report.
func RecordHandbackReminderFailed(childSessionID string, forCommandID string) error {
	panic("unported: mj-store/src/database/sessions.rs record_handback_reminder_failed")
}

func LookupSubagentRequest(parentSessionID string, requestKey string) (*core.SubagentRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs lookup_subagent_request")
}

// SaveSessionWithContainerSize persists a session and the container size it
// most recently launched on its host in one transaction.
func SaveSessionWithContainerSize(session *core.SessionRecord, host string, size core.HostContainerSize) error {
	panic("unported: mj-store/src/database/sessions.rs save_session_with_container_size")
}

// SaveResumedSession: resume publication owns the target and checkout.
// Requested size and mounts, client titles, drafts, archive visibility and
// worker titles keep their owners.
func SaveResumedSession(session *core.SessionRecord, containerSize *HostContainerSizeChoice) error {
	panic("unported: mj-store/src/database/sessions.rs save_resumed_session")
}

// AcceptResumedSession commits explicit dialog choices together with the
// first provisioning publication.
func AcceptResumedSession(session *core.SessionRecord, containerSize *HostContainerSizeChoice, requests *SessionResumeOptions) error {
	panic("unported: mj-store/src/database/sessions.rs accept_resumed_session")
}

func saveResumedSessionTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice) error {
	return saveResumedSessionInEnvironmentTo(path, session, containerSize, nil, nil, nil)
}

func RestoreResumedSession(session *core.SessionRecord, environmentID string) error {
	panic("unported: mj-store/src/database/sessions.rs restore_resumed_session")
}

// SettleSupersededMoveSource restores the source and releases Move's restore
// authority in one commit. Read the latest intent here: destination
// retirement may have updated it.
func SettleSupersededMoveSource(session *core.SessionRecord, environmentID *string, operationID string) error {
	panic("unported: mj-store/src/database/sessions.rs settle_superseded_move_source")
}

func saveResumedSessionInEnvironmentTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice, environmentID *string, requests *SessionResumeOptions, closeMove *string) error {
	panic("unported: mj-store/src/database/sessions.rs save_resumed_session_in_environment_to")
}

// SaveLifecycleSession updates only the fields a lifecycle transition owns on
// a session that already exists. Everything else — display titles,
// checkpoints, container settings, and attached directories — stays with its
// own writer.
func SaveLifecycleSession(session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/sessions.rs save_lifecycle_session")
}

// AdoptionCut is the optional current-state cut an adoption installs: Rust's
// `(MaterializedSession, LiveSessionBaseline)`.
type AdoptionCut struct {
	Projection core.MaterializedSession
	Baseline   core.LiveSessionBaseline
}

// EnqueueAdoptedSession claims the worker and installs its optional cut as
// Disconnected in one writer transaction. Ordinary attachment later owns all
// post-cut replay.
func EnqueueAdoptedSession[G any](ctx context.Context, expected core.SessionRecord, cut *AdoptionCut, updatedAt string, guard G) (*AcceptedDatabaseWrite[core.SessionRecord, G], error) {
	panic("unported: mj-store/src/database/sessions.rs enqueue_adopted_session")
}

func installAdoptedSessionWith(connection *Connection, expected core.SessionRecord, cut *AdoptionCut, updatedAt string) (core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs install_adopted_session_with")
}

// EnqueueAdoptionConnected publishes only the worker that the atomic adoption
// install claimed. Ordinary catch-up runs without an install holder and may
// race a destroy or another attach.
func EnqueueAdoptionConnected[G any](ctx context.Context, expected core.SessionRecord, nativeSessionID *string, updatedAt string, guard G) (*AcceptedDatabaseWrite[core.SessionRecord, G], error) {
	panic("unported: mj-store/src/database/sessions.rs enqueue_adoption_connected")
}

func publishAdoptionConnectedWith(connection *Connection, expected *core.SessionRecord, nativeSessionID *string, updatedAt string) (core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs publish_adoption_connected_with")
}

// RefuseAdoptionIfCurrent refuses a legacy provisional adoption without
// restoring stale session edits.
func RefuseAdoptionIfCurrent(expected *core.SessionRecord, detail string, updatedAt string) (core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs refuse_adoption_if_current")
}

func refuseAdoptionWith(connection *Connection, expected *core.SessionRecord, detail string, updatedAt string) (core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs refuse_adoption_with")
}

func RecordAdoptionFailureIfCurrent(expected *core.SessionRecord, detail string, updatedAt string) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs record_adoption_failure_if_current")
}

func recordAdoptionFailureIfCurrentTo(path string, expected *core.SessionRecord, detail string, updatedAt string) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs record_adoption_failure_if_current_to")
}

// FailUnreadySession applies either a live preparation failure or a dead
// worker's startup failure only to the lifecycle state and revision that the
// caller observed.
func FailUnreadySession(sessionID string, cause string, observedState core.SessionState, observedUpdatedAt string) (*core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs fail_unready_session")
}

// SaveSuspensionIntent: acceptance of Suspend owns its intent, readiness and
// Closing publication.
func SaveSuspensionIntent(session *core.SessionRecord, readyMove *core.MoveOperation, consent *core.SuspendConsent) error {
	panic("unported: mj-store/src/database/sessions.rs save_suspension_intent")
}

// LoadClosingConsent reads the persisted suspend consent for a session
// currently in Closing.
func LoadClosingConsent(sessionID string) (*core.SuspendConsent, error) {
	panic("unported: mj-store/src/database/sessions.rs load_closing_consent")
}

func SaveFailedClose(session *core.SessionRecord) (core.SessionRecord, error) {
	panic("unported: mj-store/src/database/sessions.rs save_failed_close")
}

func SaveClosedSession(session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/sessions.rs save_closed_session")
}

func ClearRecordedLifecycleFailure(previous *core.SessionRecord, updatedAt string) (bool, error) {
	panic("unported: mj-store/src/database/sessions.rs clear_recorded_lifecycle_failure")
}

func ReplaceCheckpointArchiveIfCurrent(sessionID string, expectedArchivePath string, expectedSha256 string, replacement *core.CheckpointMetadata) (bool, error) {
	panic("unported: mj-store/src/database/sessions.rs replace_checkpoint_archive_if_current")
}

func SaveCheckpointedSession(session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/sessions.rs save_checkpointed_session")
}

// RecoverInterruptedCheckpointingSessions recovers lifecycle rows stranded
// by a process exit during checkpoint creation. This must be called once by
// the top-level controller process while it owns the controller-store guard,
// not by per-operation reloads.
func RecoverInterruptedCheckpointingSessions(updatedAt string) (int, error) {
	panic("unported: mj-store/src/database/sessions.rs recover_interrupted_checkpointing_sessions")
}

// SetSessionTitleOverride changes only the user-owned display name. This
// avoids writing a stale SessionRecord over independently committed
// checkpoint or relay metadata.
func SetSessionTitleOverride(sessionID string, title string, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_title_override")
}

// RenameProfileReferences rewrites a configured profile id in every
// persisted session and recorded turn in one SQLite transaction.
// Configuration is stored separately, so the controller owns coordinating
// this update with the matching config-map rename.
func RenameProfileReferences(oldID string, newID string) (int, error) {
	panic("unported: mj-store/src/database/sessions.rs rename_profile_references")
}

func renameProfileReferencesAt(path string, oldID string, newID string) (int, error) {
	panic("unported: mj-store/src/database/sessions.rs rename_profile_references_at")
}

// RenameTargetReferences rewrites a configured target id in every persisted
// session in one SQLite transaction.
func RenameTargetReferences(oldID string, newID string) (int, error) {
	return renameSessionReference("target_template_id", oldID, newID)
}

func renameSessionReference(column string, oldID string, newID string) (int, error) {
	panic("unported: mj-store/src/database/sessions.rs rename_session_reference")
}

func renameSessionReferenceAt(path string, column string, oldID string, newID string) (int, error) {
	panic("unported: mj-store/src/database/sessions.rs rename_session_reference_at")
}

// SetSessionArchived changes only whether the resume dialog hides this
// session. Archiving is a display choice, so it has its own writer and never
// rewrites lifecycle, checkpoint, or title columns another task owns.
func SetSessionArchived(sessionID string, archived bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_archived")
}

// MarkSessionTargetMissing records that the managed target of an otherwise
// live session is definitively gone. A verified checkpoint keeps the session
// recoverable as an error on the dashboard; without one, the session is
// lost. The state predicate keeps a late poll result from overwriting a
// concurrent lifecycle transition. The bool is false when nothing changed.
func MarkSessionTargetMissing(sessionID string, detail string, updatedAt string) (core.SessionState, bool, error) {
	panic("unported: mj-store/src/database/sessions.rs mark_session_target_missing")
}

func markSessionTargetMissingTo(path string, sessionID string, detail string, updatedAt string) (core.SessionState, bool, error) {
	return markSessionTargetMissingIfCurrentTo(path, sessionID, detail, updatedAt, nil)
}

// MarkSessionTargetMissingIfCurrent records a definitive worker failure only
// while the observed session record is still current. A delayed background
// write must not invalidate a resume.
func MarkSessionTargetMissingIfCurrent(sessionID string, detail string, updatedAt string, observedUpdatedAt string) (core.SessionState, bool, error) {
	panic("unported: mj-store/src/database/sessions.rs mark_session_target_missing_if_current")
}

func markSessionTargetMissingIfCurrentTo(path string, sessionID string, detail string, updatedAt string, observedUpdatedAt *string) (core.SessionState, bool, error) {
	panic("unported: mj-store/src/database/sessions.rs mark_session_target_missing_if_current_to")
}

func setSessionArchivedTo(path string, sessionID string, archived bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_archived_to")
}

func SetSessionNotificationsIgnored(sessionID string, ignored bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_notifications_ignored")
}

func setSessionNotificationsIgnoredTo(path string, sessionID string, ignored bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_notifications_ignored_to")
}

// HiddenNativeSession is one entry of the hidden native session set: Rust's
// `(HarnessKind, String)`.
type HiddenNativeSession struct {
	Harness         core.HarnessKind
	NativeSessionID string
}

func HiddenNativeSessions() (map[HiddenNativeSession]struct{}, error) {
	return hiddenNativeSessionsFrom(DatabasePath())
}

func hiddenNativeSessionsFrom(path string) (map[HiddenNativeSession]struct{}, error) {
	panic("unported: mj-store/src/database/sessions.rs hidden_native_sessions_from")
}

func SetNativeSessionHidden(harness core.HarnessKind, nativeSessionID string, hidden bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_native_session_hidden")
}

func setNativeSessionHiddenTo(path string, harness core.HarnessKind, nativeSessionID string, hidden bool) error {
	panic("unported: mj-store/src/database/sessions.rs set_native_session_hidden_to")
}

func SetSessionContainerSettings(sessionID string, allocation core.SessionResourceAllocation, mounts []core.AdditionalMount, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_container_settings")
}

func setSessionContainerSettingsTo(path string, sessionID string, allocation core.SessionResourceAllocation, mounts []core.AdditionalMount, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_container_settings_to")
}

func writeContainerSettings(tx *sql.Tx, sessionID string, allocation core.SessionResourceAllocation, mounts []core.AdditionalMount, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs write_container_settings")
}

func setSessionTitleOverrideTo(path string, sessionID string, title string, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_title_override_to")
}

func SetSessionACPTitleAtProjection(sessionID string, appliedOrdinal uint64, appliedDigest string, title *string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_acp_title_at_projection")
}

func setSessionACPTitleAtProjectionWith(connection Queryer, sessionID string, appliedOrdinal uint64, appliedDigest string, title *string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_acp_title_at_projection_with")
}

// setSessionACPTitleTo is test-only in Rust.
func setSessionACPTitleTo(path string, sessionID string, title *string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_acp_title_to")
}

// MarkSessionWorkerConnected commits the successful handshake for a newly
// provisioned worker without replacing checkpoint or display metadata owned
// by other controller tasks.
func MarkSessionWorkerConnected(sessionID string, nativeSessionID *string, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs mark_session_worker_connected")
}

func AdoptNativeSessionID(sessionID string, nativeSessionID string) error {
	panic("unported: mj-store/src/database/sessions.rs adopt_native_session_id")
}

func adoptNativeSessionIDTo(path string, sessionID string, nativeSessionID string) error {
	panic("unported: mj-store/src/database/sessions.rs adopt_native_session_id_to")
}

func markSessionWorkerConnectedTo(path string, sessionID string, nativeSessionID *string, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs mark_session_worker_connected_to")
}

func markSessionWorkerConnectedWith(tx *sql.Tx, sessionID string, nativeSessionID *string, updatedAt string) error {
	panic("unported: mj-store/src/database/sessions.rs mark_session_worker_connected_with")
}

func recoverInterruptedCheckpointingSessionsTo(path string, updatedAt string) (int, error) {
	panic("unported: mj-store/src/database/sessions.rs recover_interrupted_checkpointing_sessions_to")
}

func SaveSessionTo(path string, session *core.SessionRecord) error {
	return saveSessionWithContainerSizeTo(path, session, nil)
}

func saveSessionWithContainerSizeTo(path string, session *core.SessionRecord, containerSize *HostContainerSizeChoice) error {
	panic("unported: mj-store/src/database/sessions.rs save_session_with_container_size_to")
}

func saveLifecycleSessionTo(path string, session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/sessions.rs save_lifecycle_session_to")
}

// SaveStartupCleanupOutcome settles teardown and archives failed children in
// the same owner transaction.
func SaveStartupCleanupOutcome(session *core.SessionRecord, archive bool) error {
	panic("unported: mj-store/src/database/sessions.rs save_startup_cleanup_outcome")
}

func saveCheckpointedSessionTo(path string, session *core.SessionRecord) error {
	panic("unported: mj-store/src/database/sessions.rs save_checkpointed_session_to")
}

func validateSessionRecord(session *core.SessionRecord) error {
	validation := core.NewStateState()
	validation.Sessions.Insert(session.ID, *session)
	return validation.Validate()
}

// DeleteSession removes one operational session while retaining its
// relational history context. A database trigger removes its prompt history
// with it.
func DeleteSession(sessionID string) error {
	panic("unported: mj-store/src/database/sessions.rs delete_session")
}

func deleteSessionFrom(path string, sessionID string) error {
	panic("unported: mj-store/src/database/sessions.rs delete_session_from")
}

// SetSessionDraftInput overwrites the unsent chat input carried across a
// detach. Unlike the read receipt this is not monotonic: a draft can shrink,
// and an empty string clears it.
func SetSessionDraftInput(sessionID string, draft string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_draft_input")
}

func setSessionDraftInputAt(path string, sessionID string, draft string) error {
	panic("unported: mj-store/src/database/sessions.rs set_session_draft_input_at")
}

// AppendSessionDraftInput appends recovered input on the writer lane,
// preserving every preceding durable edit instead of combining with a stale
// daemon snapshot.
func AppendSessionDraftInput(sessionID string, text string) error {
	panic("unported: mj-store/src/database/sessions.rs append_session_draft_input")
}

// ClearSessionDraftInputIfMatches retires a submitted shared draft without
// erasing a newer client's edit.
func ClearSessionDraftInputIfMatches(sessionID string, expected string) error {
	panic("unported: mj-store/src/database/sessions.rs clear_session_draft_input_if_matches")
}

// RebindSessionBundle re-associates a session with another project bundle.
//
// A session's bundle is otherwise fixed, because prompt history is grouped
// by it. Resume calls this when it converts a session between its raw and
// bundle representations: the project is the same, so its history follows
// it, and only the name Hel files it under changes. Files the session under
// bundleID's project and returns the bundle id the session context now
// holds: the canonical project when the catalog knows bundleID as an alias of
// another bundle. A caller keeping the session record must adopt the
// returned id, because resume publication checks the record's bundle against
// the context.
func RebindSessionBundle(sessionID string, bundleID string) (string, error) {
	panic("unported: mj-store/src/database/sessions.rs rebind_session_bundle")
}

func rebindSessionBundleTo(path string, sessionID string, bundleID string) (string, error) {
	panic("unported: mj-store/src/database/sessions.rs rebind_session_bundle_to")
}

// DismissRecordedFailure compares and dismisses under the database writer,
// so a concurrent new failure cannot inherit the receipt for the failure the
// person saw.
func DismissRecordedFailure(sessionID string, failure core.FailureIdentity) error {
	panic("unported: mj-store/src/database/sessions.rs dismiss_recorded_failure")
}

// SessionResumeOptions is what a resume may change about the session it
// restarts. A nil AdditionalMounts or ResourceAllocation is no request.
type SessionResumeOptions struct {
	AdditionalMounts   *[]core.AdditionalMount
	ResourceAllocation core.SessionResourceAllocation
	DiscardQueue       bool
}
