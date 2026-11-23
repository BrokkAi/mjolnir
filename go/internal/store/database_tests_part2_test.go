// Port of mj-store/src/database/tests.rs, continued (container settings
// through queue entry kinds).

package store

import (
	"errors"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func dtContainerAllocation(cpus, memoryBytes uint64) core.SessionResourceAllocation {
	return core.SessionResourceAllocationContainer{Cpus: cpus, MemoryBytes: memoryBytes}
}

func TestContainerSettingsWriteOverridesMountsAndRememberedSources(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	dtOK(t, setSessionContainerSettingsTo(
		database,
		"session-1",
		dtContainerAllocation(6, 12<<30),
		[]core.AdditionalMount{{
			Source:      "/host/models",
			Destination: "/mnt/models",
			Access:      core.MountAccessRo,
		}},
		"2026-08-13T00:00:00Z",
	))
	dtOK(t, replaceMountHistoryIn(database, "local", []string{"/host/models"}))

	loaded := dtMust(LoadStateFrom(database))
	session := dtSessionOf(loaded, "session-1")
	dtEqual(t, session.ResourceAllocation, dtContainerAllocation(6, 12<<30))
	dtEqual(t, session.AdditionalMounts, []core.AdditionalMount{{
		Source:      "/host/models",
		Destination: "/mnt/models",
		Access:      core.MountAccessRo,
	}})
	dtEqual(t, session.UpdatedAt, "2026-08-13T00:00:00Z")
	dtEqual(t, *loaded.MountHistory.Index("local"), []string{"/host/models"})
}

func TestMountReadOnlyRoundTripsThroughBothWriters(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.AdditionalMounts = []core.AdditionalMount{
		{Source: "/host/cache", Destination: "/mnt/cache", Access: core.MountAccessCow},
		{Source: "/net/share", Destination: "/mnt/share", Access: core.MountAccessRo},
		{Source: "/host/build-cache", Destination: "/mnt/build-cache", Access: core.MountAccessRw},
	}

	dtOK(t, SaveSessionTo(database, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").AdditionalMounts, record.AdditionalMounts)
}

func TestReadWriteMountsSurviveAnOlderWriterRewritingSessionMounts(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.AdditionalMounts = []core.AdditionalMount{
		{Source: "/host/build-cache", Destination: "/mnt/build-cache", Access: core.MountAccessRw},
		{Source: "/host/scratch", Destination: "/mnt/scratch", Access: core.MountAccessRw},
	}
	dtOK(t, SaveSessionTo(database, &record))

	// An older build rewrites `session_mounts` from its own view, where a
	// read-write mount is merely not read-only, and knows nothing of the
	// access table. Here it keeps the first mount and makes the second one
	// read-only.
	connection := dtOpen(t, database)
	dtExec(t, connection, "DELETE FROM session_mounts WHERE session_id = 'session-1'")
	for _, row := range []struct {
		ordinal  int64
		mount    core.AdditionalMount
		readOnly int64
	}{{0, record.AdditionalMounts[0], 0}, {1, record.AdditionalMounts[1], 1}} {
		dtExec(t, connection, `INSERT INTO session_mounts(session_id, ordinal, source, destination, read_only)
                 VALUES ('session-1', ?1, ?2, ?3, ?4)`,
			row.ordinal, pathToBlob(row.mount.Source), pathToBlob(row.mount.Destination), row.readOnly)
	}
	dtOK(t, connection.Close())

	loaded := dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").AdditionalMounts
	dtEqual(t, loaded[0].Access, core.MountAccessRw)
	dtEqual(t, loaded[1].Access, core.MountAccessRo)
}

// Hard-won: b91beebe: a stale lifecycle write erased newer mounts and container settings
func TestLifecycleSavePreservesContainerSettingsAndMounts(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	stale := session("session-1", "project-1")
	stale.AdditionalMounts = nil
	dtOK(t, SaveSessionTo(database, &stale))

	// A read-only mount proves the flag survives a stale lifecycle save too.
	attached := core.AdditionalMount{Source: "/host/models", Destination: "/mnt/models", Access: core.MountAccessRo}
	dtOK(t, setSessionContainerSettingsTo(
		database,
		"session-1",
		dtContainerAllocation(6, 12<<30),
		[]core.AdditionalMount{attached},
		"2026-08-15T00:00:00Z",
	))

	// The lifecycle writer still holds the record as it was before the
	// container settings were edited.
	stale.State = core.SessionStateDestroying
	stale.UpdatedAt = "2026-08-15T00:01:00Z"
	dtOK(t, saveLifecycleSessionTo(database, &stale))

	loaded := dtMust(LoadStateFrom(database))
	session := dtSessionOf(loaded, "session-1")
	dtEqual(t, session.State, core.SessionStateDestroying)
	dtEqual(t, session.AdditionalMounts, []core.AdditionalMount{attached})
	dtEqual(t, session.ResourceAllocation, dtContainerAllocation(6, 12<<30))
}

func dtPushReview(reviewers ...core.SubagentChoice) *core.SessionReview {
	review := core.SessionReview(core.SessionReviewPush{Reviewers: &reviewers})
	return &review
}

func TestASessionKeepsItsReviewChoiceThroughLifecycleWrites(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.Review = dtPushReview(core.SubagentChoice{Model: "gpt-6-astra", Effort: dtPtr("high")})
	dtOK(t, SaveSessionTo(database, &record))
	record.State = core.SessionStateRunning
	dtOK(t, saveLifecycleSessionTo(database, &record))

	loaded := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(loaded, "session-1").Review,
		dtPushReview(core.SubagentChoice{Model: "gpt-6-astra", Effort: dtPtr("high")}),
		"writes omit the legacy tier while preserving the review choice")
}

// Per-turn review was removed. A session stored with the `turn` mode (or the
// older `on`) by an earlier build loads as before-push review with the same
// reviewers, and the next write stores `push`.
func TestASessionStoredWithTurnReviewLoadsAsPushAndIsRewrittenAsPush(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	reviewers := `[{"model":"gpt-6-astra","effort":"high"}]`
	stored := []string{
		fmt.Sprintf(`{"mode":"turn","reviewers":%s}`, reviewers),
		fmt.Sprintf(`{"mode":"on","reviewers":%s}`, reviewers),
	}
	expected := dtPushReview(core.SubagentChoice{Model: "gpt-6-astra", Effort: dtPtr("high")})
	for _, reviewJSON := range stored {
		record := session("session-1", "project-1")
		record.Review = expected
		dtOK(t, SaveSessionTo(database, &record))
		dtExec(t, dtRaw(t, database), "UPDATE sessions SET review_json = ?1 WHERE session_id = 'session-1'", reviewJSON)
		loaded := dtMust(LoadStateFrom(database))
		dtEqual(t, dtSessionOf(loaded, "session-1").Review, expected)
		reloaded := dtSessionOf(loaded, "session-1")
		dtOK(t, SaveSessionTo(database, &reloaded))
		rewritten := dtQuery[string](t, dtRaw(t, database), "SELECT review_json FROM sessions WHERE session_id = 'session-1'")
		dtTrue(t, strings.Contains(rewritten, `"mode":"push"`), rewritten)
	}
}

// Hard-won: 06cfc1cfe: reviewer-only flags replaced before-push review with end-of-turn.
func TestInheritedSessionReviewSurvivesLifecycleWritesAndKeepsTriggerInheritance(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	threeReviewers := func() *[]core.SubagentChoice {
		choice := core.SubagentChoice{Model: "gpt-6-astra", Effort: dtPtr("high")}
		return &[]core.SubagentChoice{choice, choice, choice}
	}
	record := session("session-1", "project-1")
	inherit := core.SessionReview(core.SessionReviewInherit{Reviewers: threeReviewers()})
	record.Review = &inherit
	dtOK(t, SaveSessionTo(database, &record))
	record.State = core.SessionStateRunning
	dtOK(t, saveLifecycleSessionTo(database, &record))

	loaded := dtMust(LoadStateFrom(database))
	review := dtSessionOf(loaded, "session-1").Review
	dtTrue(t, review != nil, "the review choice survives")
	dtEqual(t, *review, core.SessionReview(core.SessionReviewInherit{Reviewers: threeReviewers()}))
	// The session reviewer choice enables review with either global list.
	for _, entry := range []struct {
		globalReviewers []core.SubagentChoice
		trigger         core.ReviewTrigger
	}{
		{nil, core.ReviewTriggerPush},
		{[]core.SubagentChoice{{Model: "gpt-6-sol"}}, core.ReviewTriggerPush},
	} {
		global := core.ReviewConfig{Reviewers: entry.globalReviewers}
		effective := global.ForSession(*review)
		dtEqual(t, effective.Trigger(), entry.trigger)
		dtEqual(t, len(effective.Reviewers), 3)
	}
}

func TestASessionKeepsItsProjectMemoryAndMailboxOptOutsThroughLifecycleWrites(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.NoProjectMemory = true
	record.NoGithubEvents = true
	dtOK(t, SaveSessionTo(database, &record))
	record.State = core.SessionStateRunning
	dtOK(t, saveLifecycleSessionTo(database, &record))

	loaded := dtMust(LoadStateFrom(database))
	dtTrue(t, dtSessionOf(loaded, "session-1").NoProjectMemory)
	dtTrue(t, dtSessionOf(loaded, "session-1").NoGithubEvents)
	ordinary := session("session-2", "project-1")
	dtOK(t, SaveSessionTo(database, &ordinary))
	loaded = dtMust(LoadStateFrom(database))
	dtTrue(t, !dtSessionOf(loaded, "session-2").NoProjectMemory)
	dtTrue(t, !dtSessionOf(loaded, "session-2").NoGithubEvents)
}

// Hard-won: d92ab7ce: the lifecycle UPDATE omitted build_cache_json and live sessions reloaded without their mounted cache
func TestProvisioningPersistsTheBuildCacheItResolved(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	// Provisioning resolves the cache once the container's mounts are fixed
	// and then saves through the lifecycle path, which is the only write a
	// newly provisioned session gets.
	record.State = core.SessionStateRunning
	record.BuildCache = &core.SessionBuildCache{
		Host:      "ssh:morannon",
		Directory: "/mnt/nvme/mbx",
		MaxSize:   dtPtr("1000GB"),
	}
	dtOK(t, saveLifecycleSessionTo(database, &record))

	loaded := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(loaded, "session-1").BuildCache, record.BuildCache,
		"a resumed session must find the cache its container is already mounting")
}

// dtMarked is the result of a missing-target report as Rust's
// Option<SessionState>: nil when nothing changed.
func dtMarked(state core.SessionState, changed bool, err error) *core.SessionState {
	if err != nil {
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
	if !changed {
		return nil
	}
	return &state
}

// Hard-won: #1444: repeated missing-target reports rewrote a recoverable error and announced it again.
func TestMissingTargetKeepsACheckpointedSessionRecoverableAndLosesOneWithoutCheckpoint(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	live := session("session-1", "project-1")
	live.State = core.SessionStateRunning
	dtOK(t, SaveSessionTo(database, &live))

	dtEqual(t, dtMarked(markSessionTargetMissingTo(database, "session-1", "managed container is missing", "2026-08-25T16:00:00Z")),
		dtPtr(core.SessionStateError))
	loaded := dtMust(LoadStateFrom(database))
	recoverable := dtSessionOf(loaded, "session-1")
	dtEqual(t, recoverable.State, core.SessionStateError)
	dtEqual(t, recoverable.LastError, dtPtr("managed container is missing"))

	dtEqual(t, dtMarked(markSessionTargetMissingTo(database, "session-1", "late duplicate", "2026-08-25T16:01:00Z")),
		(*core.SessionState)(nil))

	unrecoverable := session("session-2", "project-1")
	unrecoverable.State = core.SessionStateRunning
	unrecoverable.Checkpoint = nil
	dtOK(t, SaveSessionTo(database, &unrecoverable))
	dtEqual(t, dtMarked(markSessionTargetMissingTo(database, "session-2", "managed container is missing", "2026-08-25T16:02:00Z")),
		dtPtr(core.SessionStateLost))

	loaded = dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(loaded, "session-1").LastError, dtPtr("managed container is missing"))
	dtEqual(t, dtSessionOf(loaded, "session-1").UpdatedAt, "2026-08-25T16:00:00Z")
	dtEqual(t, dtSessionOf(loaded, "session-2").State, core.SessionStateLost)

	dtEqual(t, dtMarked(markSessionTargetMissingTo(database, "session-2", "late duplicate", "2026-08-25T16:03:00Z")),
		(*core.SessionState)(nil))
}

func TestALateMissingWorkspaceReportCannotInvalidateANewerSession(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	live := session("session-1", "project-1")
	live.State = core.SessionStateRunning
	live.UpdatedAt = "2026-09-08T00:00:00Z"
	dtOK(t, SaveSessionTo(database, &live))
	observed := live.UpdatedAt
	live.UpdatedAt = "2026-09-08T00:01:00Z"
	dtOK(t, SaveSessionTo(database, &live))

	dtEqual(t, dtMarked(markSessionTargetMissingIfCurrentTo(database, live.ID, "old working directory is missing", "2026-09-08T00:02:00Z", &observed)),
		(*core.SessionState)(nil))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), live.ID).State, core.SessionStateRunning)
	// A resume may enter Provisioning within the same timestamp second as
	// the old Running observation. State must guard that transition too.
	live.State = core.SessionStateProvisioning
	dtOK(t, SaveSessionTo(database, &live))
	dtEqual(t, dtMarked(markSessionTargetMissingIfCurrentTo(database, live.ID, "old working directory is missing", "2026-09-08T00:02:00Z", dtPtr(live.UpdatedAt))),
		(*core.SessionState)(nil))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), live.ID).State, core.SessionStateProvisioning)
	live.State = core.SessionStateRunning
	dtOK(t, SaveSessionTo(database, &live))
	dtEqual(t, dtMarked(markSessionTargetMissingIfCurrentTo(database, live.ID, "current working directory is missing", "2026-09-08T00:02:00Z", dtPtr(live.UpdatedAt))),
		dtPtr(core.SessionStateError))
	loaded := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(loaded, live.ID).Checkpoint, live.Checkpoint)
	dtEqual(t, dtSessionOf(loaded, live.ID).LastError, dtPtr("current working directory is missing"))
}

// Hard-won: b91beebe: a stale checkpoint write erased newer mounts and container settings
func TestCheckpointedSavePreservesContainerSettingsAndMounts(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	stale := session("session-1", "project-1")
	stale.AdditionalMounts = nil
	dtOK(t, SaveSessionTo(database, &stale))

	attached := core.AdditionalMount{Source: "/host/models", Destination: "/mnt/models", Access: core.MountAccessRo}
	dtOK(t, setSessionContainerSettingsTo(
		database,
		"session-1",
		dtContainerAllocation(6, 12<<30),
		[]core.AdditionalMount{attached},
		"2026-08-15T00:00:00Z",
	))

	verified := core.CheckpointMetadata{
		ArchivePath:   "sessions/verified.hel.zip",
		Sha256:        strings.Repeat("c", 64),
		CreatedAt:     "2026-08-15T00:02:00Z",
		EventFrontier: 21,
	}
	stale.State = core.SessionStateRunning
	stale.UpdatedAt = "2026-08-15T00:02:00Z"
	stale.NativeSessionID = dtPtr("native-checkpointed")
	stale.Checkpoint = dtPtr(verified)
	dtOK(t, saveCheckpointedSessionTo(database, &stale))

	loaded := dtMust(LoadStateFrom(database))
	session := dtSessionOf(loaded, "session-1")
	dtEqual(t, session.Checkpoint, &verified)
	dtEqual(t, session.NativeSessionID, dtPtr("native-checkpointed"))
	dtEqual(t, session.AdditionalMounts, []core.AdditionalMount{attached})
	dtEqual(t, session.ResourceAllocation, dtContainerAllocation(6, 12<<30))
}

func TestLifecycleSaveFailsForUnknownSession(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	missing := session("session-1", "project-1")

	for _, err := range []error{
		dtErr(t, saveLifecycleSessionTo(database, &missing)),
		dtErr(t, saveCheckpointedSessionTo(database, &missing)),
	} {
		dtTrue(t, strings.Contains(err.Error(), "unknown session session-1"), err)
	}

	dtTrue(t, dtMust(LoadStateFrom(database)).Sessions.IsEmpty())
}

func TestDestroyingSessionRoundTripIsDurable(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.State = core.SessionStateDestroying

	dtOK(t, SaveSessionTo(database, &record))

	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1"), record)
}

func TestInterruptedCheckpointRecoveryIsFieldScopedAndOneShot(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	checkpointing := session("session-1", "project-1")
	checkpointing.State = core.SessionStateCheckpointing
	checkpointing.LastError = dtPtr("preserve this diagnostic")
	closing := session("session-2", "project-2")
	closing.State = core.SessionStateClosing
	dtOK(t, SaveSessionTo(database, &checkpointing))
	dtOK(t, SaveSessionTo(database, &closing))

	dtEqual(t, dtMust(recoverInterruptedCheckpointingSessionsTo(database, "2026-08-14T12:00:00Z")), 1)

	recovered := dtMust(LoadStateFrom(database))
	session := dtSessionOf(recovered, "session-1")
	dtEqual(t, session.State, core.SessionStateRunning)
	dtEqual(t, session.UpdatedAt, "2026-08-14T12:00:00Z")
	dtEqual(t, session.LastError, dtPtr("preserve this diagnostic"))
	dtEqual(t, session.Target, checkpointing.Target)
	dtEqual(t, session.Checkpoint, checkpointing.Checkpoint)
	dtTrue(t, session.LastCheckpointError != nil && strings.Contains(*session.LastCheckpointError, "controller restart"))
	dtEqual(t, dtSessionOf(recovered, "session-2"), closing)
	dtEqual(t, dtMust(recoverInterruptedCheckpointingSessionsTo(database, "2026-08-14T12:01:00Z")), 0)
}

func TestDisplayUpdatesCannotRestoreAStaleCheckpoint(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	stale := session("session-1", "project-1")
	stale.State = core.SessionStateError
	stale.LastError = dtPtr("worker bootstrap failed: upload failed")
	dtOK(t, SaveSessionTo(database, &stale))
	recovered := core.CheckpointMetadata{
		ArchivePath:   "sessions/recovered.hel.zip",
		Sha256:        strings.Repeat("b", 64),
		CreatedAt:     "2026-08-14T12:00:00Z",
		EventFrontier: 42,
	}
	saved := stale
	saved.NativeSessionID = dtPtr("native-recovered")
	saved.Checkpoint = dtPtr(recovered)
	dtOK(t, saveCheckpointedSessionTo(database, &saved))

	dtOK(t, setSessionTitleOverrideTo(database, "session-1", "Renamed safely", "2026-08-14T12:01:00Z"))
	dtOK(t, setSessionACPTitleTo(database, "session-1", dtPtr("Harness title")))

	loaded := dtMust(LoadStateFrom(database))
	session := dtSessionOf(loaded, "session-1")
	dtEqual(t, session.Checkpoint, &recovered)
	dtEqual(t, session.NativeSessionID, dtPtr("native-recovered"))
	dtEqual(t, session.SessionTitleOverride, dtPtr("Renamed safely"))
	dtEqual(t, session.ACPSessionTitle, dtPtr("Harness title"))
	dtEqual(t, session.State, core.SessionStateError)
	dtEqual(t, session.LastError, dtPtr("worker bootstrap failed: upload failed"))

	dtOK(t, setSessionACPTitleTo(database, "session-1", nil))
	dtTrue(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").ACPSessionTitle == nil)
}

func TestARelayTitleCommitsAndRollsBackWithItsProjectionPage(t *testing.T) {
	path := dtDatabase(t, "relay-title-page.sqlite3")
	record := session("session-1", "project")
	dtOK(t, SaveSessionTo(path, &record))
	empty := core.MaterializedSessionEmpty("session-1")
	dtOK(t, SaveMaterializedSessionTo(path, &empty))

	_, rejected := applyProjectionPageTo(path, "session-1", func(page *ProjectionPage) (struct{}, error) {
		if err := page.SetResolvedTitle(dtPtr("Rejected title")); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, errors.New("fail after changing the title")
	})
	dtTrue(t, rejected != nil)
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(path)), "session-1").ACPSessionTitle, record.ACPSessionTitle)

	dtMust(applyProjectionPageTo(path, "session-1", func(page *ProjectionPage) (struct{}, error) {
		if _, err := page.Apply(1, core.RelayEventGenesisDigest, EventDigest(1), &core.MaterializedSessionMutation{}); err != nil {
			return struct{}{}, err
		}
		return struct{}{}, page.SetResolvedTitle(dtPtr("Committed title"))
	}))
	stored := dtMust(LoadStateFrom(path))
	dtEqual(t, dtSessionOf(stored, "session-1").ACPSessionTitle, dtPtr("Committed title"))
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(path, "session-1")).AppliedEventOrdinal, uint64(1))
}

func TestADelayedRelayTitleCannotReplaceTheTitleOfANewerProjection(t *testing.T) {
	path := dtDatabase(t, "relay-title.sqlite3")
	record := session("session-1", "project")
	dtOK(t, SaveSessionTo(path, &record))
	previousDigest := strings.Repeat("a", 64)
	currentDigest := strings.Repeat("b", 64)
	materialized := core.MaterializedSessionEmpty("session-1")
	materialized.AppliedEventOrdinal = 2
	materialized.AppliedEventDigest = currentDigest
	dtOK(t, SaveMaterializedSessionTo(path, &materialized))

	connection := dtOpen(t, path)
	dtOK(t, setSessionACPTitleAtProjectionWith(connection, "session-1", 2, currentDigest, dtPtr("New title")))
	dtOK(t, setSessionACPTitleAtProjectionWith(connection, "session-1", 1, previousDigest, dtPtr("Old title")))
	dtOK(t, setSessionACPTitleAtProjectionWith(connection, "session-1", 2, previousDigest, nil))
	stored := dtMust(LoadStateFrom(path))
	dtEqual(t, dtSessionOf(stored, "session-1").ACPSessionTitle, dtPtr("New title"))
}

func TestHarnessTitleWriteCapsLargeTitlesInStorage(t *testing.T) {
	path := dtDatabase(t, "title-write.sqlite3")
	record := session("session-1", "project")
	dtOK(t, SaveSessionTo(path, &record))
	dtOK(t, setSessionACPTitleTo(path, "session-1", dtPtr(strings.Repeat("word ", 20_000))))
	stored := dtQuery[string](t, dtRaw(t, path), "SELECT acp_session_title FROM sessions WHERE session_id='session-1'")
	dtEqual(t, stored, strings.Repeat("word ", 50)+"word…")
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(path)), "session-1").ACPSessionTitle, &stored)
}

func TestLifecycleWritePreservesIndependentlyOwnedSessionFields(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	stale := session("session-1", "project-1")
	stale.NativeSessionID = dtPtr("native-old")
	stale.ACPSessionTitle = dtPtr("Old harness title")
	stale.SessionTitleOverride = dtPtr("Old user title")
	stale.Checkpoint = &core.CheckpointMetadata{
		ArchivePath:   "sessions/old.hel.zip",
		Sha256:        strings.Repeat("a", 64),
		CreatedAt:     "2026-08-14T11:00:00Z",
		EventFrontier: 10,
	}
	dtOK(t, SaveSessionTo(database, &stale))
	recovered := core.CheckpointMetadata{
		ArchivePath:   "sessions/recovered.hel.zip",
		Sha256:        strings.Repeat("b", 64),
		CreatedAt:     "2026-08-14T12:00:00Z",
		EventFrontier: 42,
	}
	saved := stale
	saved.NativeSessionID = dtPtr("native-recovered")
	saved.Checkpoint = dtPtr(recovered)
	dtOK(t, saveCheckpointedSessionTo(database, &saved))
	dtOK(t, setSessionTitleOverrideTo(database, "session-1", "Current user title", "2026-08-14T12:01:00Z"))
	dtOK(t, setSessionACPTitleTo(database, "session-1", dtPtr("Current harness title")))

	stale.State = core.SessionStateDestroying
	dtOK(t, saveLifecycleSessionTo(database, &stale))

	loaded := dtMust(LoadStateFrom(database))
	session := dtSessionOf(loaded, "session-1")
	dtEqual(t, session.State, core.SessionStateDestroying)
	dtEqual(t, session.NativeSessionID, dtPtr("native-recovered"))
	dtEqual(t, session.Checkpoint, &recovered)
	dtEqual(t, session.SessionTitleOverride, dtPtr("Current user title"))
	dtEqual(t, session.ACPSessionTitle, dtPtr("Current harness title"))
}

// Archiving is a display choice with its own writer: it must not disturb
// the lifecycle state, checkpoint, or titles other writers own.
func TestTheArchivedFlagRoundTripsWithoutTouchingOtherSessionFields(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	dtTrue(t, !dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").Archived)

	dtOK(t, setSessionArchivedTo(database, "session-1", true))
	reloaded := dtSessionOf(dtMust(LoadStateFrom(database)), "session-1")
	dtTrue(t, reloaded.Archived)
	record.Archived = true
	dtEqual(t, reloaded.State, record.State)
	dtEqual(t, reloaded.Checkpoint, record.Checkpoint)
	dtEqual(t, reloaded.ACPSessionTitle, record.ACPSessionTitle)

	dtOK(t, setSessionArchivedTo(database, "session-1", false))
	dtTrue(t, !dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").Archived)
	dtTrue(t, setSessionArchivedTo(database, "missing", true) != nil)
}

// Ignoring notifications is a display choice with its own writer: it must not
// disturb the lifecycle state, checkpoint, or titles other writers own.
func TestTheNotificationsIgnoredFlagRoundTripsWithoutTouchingOtherSessionFields(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	dtTrue(t, !dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").NotificationsIgnored)

	dtOK(t, setSessionNotificationsIgnoredTo(database, "session-1", true))
	reloaded := dtSessionOf(dtMust(LoadStateFrom(database)), "session-1")
	dtTrue(t, reloaded.NotificationsIgnored)
	record.NotificationsIgnored = true
	dtEqual(t, reloaded.State, record.State)
	dtEqual(t, reloaded.Checkpoint, record.Checkpoint)
	dtEqual(t, reloaded.ACPSessionTitle, record.ACPSessionTitle)

	dtOK(t, setSessionNotificationsIgnoredTo(database, "session-1", false))
	dtTrue(t, !dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").NotificationsIgnored)
	dtTrue(t, setSessionNotificationsIgnoredTo(database, "missing", true) != nil)
}

// Hel never writes a harness home, so the hidden set for native sessions
// is Hel's own state and is keyed per harness.
func TestTheNativeHiddenSetIsKeyedByHarnessAndIsIdempotent(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	dtEqual(t, len(dtMust(hiddenNativeSessionsFrom(database))), 0)

	dtOK(t, setNativeSessionHiddenTo(database, core.HarnessKindCodex, "native-1", true))
	dtOK(t, setNativeSessionHiddenTo(database, core.HarnessKindCodex, "native-1", true))
	dtOK(t, setNativeSessionHiddenTo(database, core.HarnessKindClaude, "native-1", true))
	dtEqual(t, dtMust(hiddenNativeSessionsFrom(database)), map[HiddenNativeSession]struct{}{
		{Harness: core.HarnessKindClaude, NativeSessionID: "native-1"}: {},
		{Harness: core.HarnessKindCodex, NativeSessionID: "native-1"}:  {},
	})

	dtOK(t, setNativeSessionHiddenTo(database, core.HarnessKindCodex, "native-1", false))
	dtEqual(t, dtMust(hiddenNativeSessionsFrom(database)), map[HiddenNativeSession]struct{}{
		{Harness: core.HarnessKindClaude, NativeSessionID: "native-1"}: {},
	})
	// Revealing something that was never hidden is not an error.
	dtOK(t, setNativeSessionHiddenTo(database, core.HarnessKindGrok, "native-9", false))
	dtTrue(t, setNativeSessionHiddenTo(database, core.HarnessKindGrok, "  ", true) != nil)
}

func TestAFreshDatabaseAcceptsASessionForEveryHarnessKind(t *testing.T) {
	connection := dtOpen(t, dtDatabase(t, "hel.sqlite3"))

	for index, kind := range core.HarnessKindAll {
		sessionID := fmt.Sprintf("session-%d", index)
		dtExec(t, connection, `INSERT INTO session_contexts(session_id, bundle_id, created_at)
                 VALUES (?1, 'project-1', 'now')`, sessionID)
		if _, err := connection.Exec(`INSERT INTO sessions(
                         session_id, title, harness_kind, last_profile, target_template_id,
                         state, updated_at
                     ) VALUES (?1, ?2, ?3, 'profile-1', 'podman', 'running', 'now')`,
			sessionID, fmt.Sprintf("%v session", kind), kind.ID()); err != nil {
			t.Fatalf("the sessions harness_kind CHECK must admit %v (%q): %v", kind, kind.ID(), err)
		}
	}

	dtEqual(t, dtQuery[int64](t, connection, "SELECT count(*) FROM sessions"), int64(len(core.HarnessKindAll)))
}

// The ZCode and DeepSeek (`deepseek`) harnesses are gone but their rows
// remain readable, so a store that still holds one must list every other
// session instead of failing outright.
func TestASessionForARemovedHarnessIsSkippedWithoutHidingTheOthers(t *testing.T) {
	database := dtDatabase(t, "mj.sqlite3")
	for _, id := range []string{"supported", "legacy-zcode", "legacy-deepseek"} {
		record := session(id, "project")
		dtOK(t, SaveSessionTo(database, &record))
	}
	connection := dtOpen(t, database)
	for _, entry := range [][2]string{{"legacy-zcode", "zcode"}, {"legacy-deepseek", "deepseek"}} {
		if _, err := connection.Exec("UPDATE sessions SET harness_kind=?2 WHERE session_id=?1", entry[0], entry[1]); err != nil {
			t.Fatalf("the CHECK constraint still tolerates the stored value: %v", err)
		}
		dtExec(t, connection, "INSERT INTO hidden_native_sessions VALUES (?1,?2,'now')", entry[1], "native-"+entry[1])
	}
	dtOK(t, connection.Close())

	state, err := LoadStateFrom(database)
	if err != nil {
		t.Fatalf("the listing must not fail: %v", err)
	}
	dtTrue(t, state.Sessions.ContainsKey("supported"))
	for _, legacy := range []string{"legacy-zcode", "legacy-deepseek"} {
		dtTrue(t, !state.Sessions.ContainsKey(legacy),
			"a session whose harness was removed cannot be resumed, so it is not listed")
	}
	hidden, err := hiddenNativeSessionsFrom(database)
	if err != nil {
		t.Fatalf("hidden sessions must not fail: %v", err)
	}
	dtEqual(t, len(hidden), 0)
}

// Hard-won: c2617cceee: removed harness records blocked workspace removal but could never be suspended.
func TestWorkspaceRemovalIgnoresRemovedHarnessesAndPreservesTheirHistory(t *testing.T) {
	for _, discardDrafts := range []bool{false, true} {
		database := dtDatabase(t, "mj.sqlite3")
		workspace := dtMust(CreateWorkspaceAt(database, "BB26"))
		for _, entry := range [][2]string{
			{"supported", "codex"},
			{"legacy-zcode", "zcode"},
			{"legacy-deepseek", "deepseek"},
		} {
			record := session(entry[0], "project")
			record.WorkspaceID = workspace.ID
			record.State = core.SessionStateRunning
			dtOK(t, SaveSessionTo(database, &record))
			connection := dtOpen(t, database)
			dtExec(t, connection, "UPDATE sessions SET harness_kind=?2 WHERE session_id=?1", entry[0], entry[1])
			dtOK(t, connection.Close())
		}
		materialized := MaterializedSession("legacy-zcode")
		dtOK(t, SaveMaterializedSessionTo(database, &materialized))
		dtEqual(t, dtMust(LoadStateFrom(database)).Sessions.Len(), 1)
		dtEqual(t, dtMust(ListWorkspacesFrom(database))[0].SessionCount, uint64(1))
		remove := func(workspaceID string) error {
			if discardDrafts {
				return CloseWorkspaceAt(database, workspaceID)
			}
			return DeleteWorkspaceAt(database, workspaceID)
		}
		dtTrue(t, remove(workspace.ID) != nil)
		supported := dtSessionOf(dtMust(LoadStateFrom(database)), "supported")
		supported.State = core.SessionStateStopped
		dtOK(t, SaveSessionTo(database, &supported))
		dtEqual(t, dtMust(ListWorkspacesFrom(database))[0].SessionCount, uint64(0))
		dtOK(t, remove(workspace.ID))
		dtEqual(t, len(dtMust(ListWorkspacesFrom(database))), 0)
		connection := dtOpenReader(t, database)
		dtEqual(t, dtQuery[int64](t, connection,
			"SELECT count(*) FROM sessions WHERE session_id IN ('legacy-zcode', 'legacy-deepseek') AND state='running'"),
			int64(2), "removing the workspace must retain the original legacy records")
		dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "legacy-zcode")), &materialized)
	}
}

func TestQueueEntryKindsRoundTripAndDefaultToPrompt(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	materialized.QueuedPrompts = append(materialized.QueuedPrompts, core.MaterializedQueuedPrompt{
		CommandID:  "config-1",
		Kind:       core.QueuedCommandKindSetConfig{Key: "model", Value: "sonnet"},
		Content:    []any{map[string]any{"type": "text", "text": "/model sonnet"}},
		QueuedAtMs: 1_600,
	})
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))

	loaded := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, loaded.QueuedPrompts, materialized.QueuedPrompts)
	dtEqual(t, dtMust(loadMaterializedQueuedPromptsFrom(database))["session-1"], materialized.QueuedPrompts)

	// Rows written before queue entries carried a kind load as prompts.
	connection := dtOpen(t, database)
	dtExec(t, connection, `INSERT INTO materialized_queued_prompts(
                     session_id, ordinal, command_id, content_json, queued_at_ms
                 ) VALUES ('session-1', 9, 'legacy-1', ?1, 1700)`,
		`[{"text":"older","type":"text"}]`)
	dtOK(t, connection.Close())

	loaded = dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	last := loaded.QueuedPrompts[len(loaded.QueuedPrompts)-1]
	dtEqual(t, last.CommandID, "legacy-1")
	dtEqual(t, last.Kind, core.QueuedCommandKind(core.QueuedCommandKindPrompt{}))
}
