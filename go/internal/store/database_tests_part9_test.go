// Port of mj-store/src/database/tests.rs, continued (sub-agent relations,
// snapshot reads, workspace close, continuation evidence and connection
// behaviour).

package store

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// registerTestSubagent writes a parent, a child, and the relation between
// them, the way the controller writes them.
func registerTestSubagent(path string, session *core.SessionRecord, relation *core.SubagentRecord) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := insertSessionWithParent(tx, session, &relation.ParentSessionID); err != nil {
		return err
	}
	if err := recordChildAccounting(tx, relation, nil); err != nil {
		return err
	}
	recordJSON, err := json.Marshal(relation)
	if err != nil {
		return err
	}
	if _, err := tx.Exec(
		"INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES (?1,?2,?3,?4)",
		relation.ChildSessionID, relation.ParentSessionID, relation.Kind.RequestKey(relation.RequestKey), string(recordJSON),
	); err != nil {
		return err
	}
	return tx.Commit()
}

func subagentPair(t testing.TB, path string) (core.SessionRecord, core.SessionRecord) {
	t.Helper()
	parent := session("11111111111111111111111111111111", "bundle-1")
	child := session("22222222222222222222222222222222", "bundle-1")
	child.Title = "child session"
	state := core.NewStateState()
	state.Sessions.Insert(parent.ID, parent)
	relation := testSubagent(parent.ID, child.ID)
	dtOK(t, SaveStateTo(path, &state))
	dtOK(t, registerTestSubagent(path, &child, &relation))
	state.Sessions.Insert(child.ID, child)
	state.Subagents.Insert(child.ID, core.SubagentMetadataFrom(&relation))
	dtOK(t, SaveStateTo(path, &state))
	return parent, child
}

// testSubagent is a worker child relation from parentID to childID.
func testSubagent(parentID, childID string) core.SubagentRecord {
	return core.SubagentRecord{
		Kind:             core.SubagentKindWorker,
		ChildSessionID:   childID,
		ParentSessionID:  parentID,
		TaskName:         "probe",
		ProfileID:        "codex",
		WorkingDirectory: "",
		InitialPrompt:    "say ready",
		RequestKey:       childID,
		CreatedAt:        "2026-09-18T00:00:00Z",
	}
}

func TestMetadataStateSavesPreserveCompleteSubagentLaunchIntent(t *testing.T) {
	path := dtDatabase(t, "metadata-launch.sqlite3")
	_, child := subagentPair(t, path)
	connection := dtOpen(t, path)
	stored := dtQuery[string](t, connection, "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1", child.ID)
	var launch core.SubagentRecord
	dtOK(t, json.Unmarshal([]byte(stored), &launch))
	launch.Kind = core.SubagentKindReviewer
	launch.InitialPrompt = strings.Repeat("private durable launch 🙂\n", 10_000)
	launch.ReviewerLaunch = &core.ReviewerLaunchConfig{
		Role:                core.ReviewChildRoleReviewer,
		ProfileID:           "reviewer",
		Harness:             core.HarnessKindCodex,
		BridgeCommand:       "review-bridge",
		BridgeArgs:          []string{"--private-launch"},
		Environment:         map[string]string{"PRIVATE_LAUNCH": "retain in the daemon"},
		ExcludedEnvironment: []string{},
		ExecutionPolicy:     core.ExecutionPolicyConfiguredApprovals,
		Model:               dtPtr("review-model"),
		Generation:          0,
		MCPServers:          []core.ReviewMcpServer{},
	}
	dtExec(t, connection, "UPDATE subagent_sessions SET record_json=?2 WHERE child_session_id=?1",
		child.ID, string(dtJSON(t, launch)))
	dtOK(t, connection.Close())

	state := dtMust(LoadStateFrom(path))
	var published map[string]any
	dtOK(t, json.Unmarshal(dtJSON(t, state), &published))
	metadata := published["subagents"].(map[string]any)[child.ID].(map[string]any)
	_, hasPrompt := metadata["initial_prompt"]
	dtTrue(t, !hasPrompt)
	_, hasLaunch := metadata["reviewer_launch"]
	dtTrue(t, !hasLaunch)
	state.Subagents.GetMut(child.ID).NoticedTurn = dtPtr(uint64(42))
	dtOK(t, SaveStateTo(path, &state))

	reader := dtOpenReader(t, path)
	stored = dtQuery[string](t, reader, "SELECT record_json FROM subagent_sessions WHERE child_session_id=?1", child.ID)
	var reloaded core.SubagentRecord
	dtOK(t, json.Unmarshal([]byte(stored), &reloaded))
	launch.NoticedTurn = dtPtr(uint64(42))
	dtEqual(t, reloaded, launch, "metadata edits must retain every durable launch field")
}

func TestStateReadsKeepSessionsAndSubagentsInOneSnapshotDuringConcurrentChanges(t *testing.T) {
	for _, creatingChild := range []bool{true, false} {
		directory := t.TempDir()
		path := filepath.Join(directory, "state.sqlite3")
		_, child := subagentPair(t, path)
		launchJSON := dtQuery[string](t, dtOpen(t, path),
			"SELECT record_json FROM subagent_sessions WHERE child_session_id=?1", child.ID)
		var launch core.SubagentRecord
		dtOK(t, json.Unmarshal([]byte(launchJSON), &launch))
		withChild := dtMust(LoadStateFrom(path))
		withChild.MountHistory.Insert("host", []string{filepath.Join(directory, "with-child")})
		withoutChild := withChild
		withoutChild.Sessions.Remove(child.ID)
		withoutChild.Subagents.Remove(child.ID)
		withoutChild.EnvironmentOwners.Remove(child.ID)
		withoutChild.MountHistory.Insert("host", []string{filepath.Join(directory, "without-child")})
		before, after := withChild, withoutChild
		if creatingChild {
			before, after = withoutChild, withChild
		}
		if creatingChild {
			dtOK(t, deleteSessionFrom(path, child.ID))
		}
		dtOK(t, SaveStateTo(path, &before))
		writerPath := path
		committed := after
		hookFailures := make(chan error, 1)
		hook := func() {
			// Commit a real creation/deletion between the reader's queries.
			// WAL permits this while a read transaction remains open.
			hookFailures <- func() error {
				if creatingChild {
					if err := registerTestSubagent(writerPath, &child, &launch); err != nil {
						return err
					}
				}
				if !creatingChild {
					if err := deleteSessionFrom(writerPath, child.ID); err != nil {
						return err
					}
				}
				if err := SaveStateTo(writerPath, &committed); err != nil {
					return err
				}
				writer, err := open(writerPath)
				if err != nil {
					return err
				}
				defer writer.Close()
				var violations int64
				if err := writer.QueryRow("SELECT COUNT(*) FROM pragma_foreign_key_check").Scan(&violations); err != nil {
					return err
				}
				if violations != 0 {
					return fmt.Errorf("assertion failed: %d foreign key violations", violations)
				}
				return nil
			}()
		}
		afterStateSessionsReadHook.Store(&hook)
		observed := dtMust(LoadStateFrom(path))
		dtTrue(t, afterStateSessionsReadHook.Load() == nil)
		dtOK(t, <-hookFailures)
		dtEqual(t, observed, before, fmt.Sprintf("mixed state while creating_child=%v", creatingChild))
		dtEqual(t, dtMust(LoadStateFrom(path)), after)
	}
}

// Deleting a child takes its sub-agent relation with it, so this is not how
// a relation is left behind. Kept as the control for the test below.
func TestDeletingAChildSessionDeletesItsSubagentRelation(t *testing.T) {
	path := dtDatabase(t, "hel.sqlite3")
	_, child := subagentPair(t, path)

	dtOK(t, deleteSessionFrom(path, child.ID))

	state := dtMust(LoadStateFrom(path))
	dtTrue(t, !state.Sessions.ContainsKey(child.ID))
	dtTrue(t, state.Subagents.IsEmpty(), "the foreign key cascade removes the relation with the child")
}

// A sub-agent relation with no child session refuses the whole state load,
// and every later operation fails with it. That is how a spawn came to be
// answered "sub-agent ... has no child session" long after the child in
// question was gone (#1065).
//
// The row is seeded with the foreign key off, because the schema's cascade is
// what normally prevents it; the point of the test is what the load does when
// the row exists anyway, whatever left it there.
// Hard-won: cac34041: one orphan relation made every state load fail
func TestASubagentRelationWithNoSessionDoesNotRefuseTheWholeState(t *testing.T) {
	path := dtDatabase(t, "hel.sqlite3")
	parent, child := subagentPair(t, path)

	connection := dtOpen(t, path)
	dtExec(t, connection, "PRAGMA foreign_keys = OFF;")
	dtExec(t, connection, "DELETE FROM sessions WHERE session_id = ?1", child.ID)
	dtOK(t, connection.Close())
	dtEqual(t, countRows(t, path, "subagent_sessions"), int64(1), "the residue this test is about")

	state, err := LoadStateFrom(path)
	if err != nil {
		t.Fatalf("residue must not refuse the load: %v", err)
	}

	dtTrue(t, state.Sessions.ContainsKey(parent.ID))
	dtTrue(t, state.Subagents.IsEmpty(), "a relation with no child session describes nothing and is dropped")
	if err := state.Validate(); err != nil {
		t.Fatalf("the loaded state must pass its own check: %v", err)
	}

	// The next save clears the row durably, so the residue does not come back.
	dtOK(t, SaveStateTo(path, &state))
	dtEqual(t, countRows(t, path, "subagent_sessions"), int64(0))
}

func countRows(t testing.TB, path, table string) int64 {
	t.Helper()
	connection := dtMust(open(path))
	defer connection.Close()
	return dtQuery[int64](t, connection, fmt.Sprintf("SELECT count(*) FROM %s", table))
}

func TestWorkspaceCloseDiscardsAllDraftsButRetainsResumableHistory(t *testing.T) {
	database := dtDatabase(t, "close.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Close"))
	record := session("session-close", "project-1")
	record.WorkspaceID = workspace.ID
	record.DraftInput = "unsent legacy input"
	dtOK(t, SaveSessionTo(database, &record))
	dtMust(saveDetachedDraftAt(database, workspace.ID, dtPtr(record.ID), "terminal", dtPtr(uint32(42)), "detached input"))
	dtOK(t, CloseWorkspaceAt(database, workspace.ID))
	stored := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(stored, record.ID).Checkpoint, record.Checkpoint)
	dtEqual(t, dtSessionOf(stored, record.ID).State, record.State)
	dtEqual(t, dtSessionOf(stored, record.ID).DraftInput, "")
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 0)
	dtTrue(t, !dtContains(dtWorkspaceIDs(dtMust(ListWorkspacesFrom(database))), workspace.ID))
	destination := dtMust(CreateWorkspaceAt(database, "Resume here"))
	dtOK(t, ReassignResumableSessionWorkspaceAt(database, record.ID, destination.ID))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).WorkspaceID, destination.ID)
}

func TestWorkspaceCloseRefusesNewActiveSessionsWithoutDiscardingDrafts(t *testing.T) {
	database := dtDatabase(t, "close.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Concurrent create"))
	record := session("session-new", "project-1")
	record.WorkspaceID = workspace.ID
	record.State = core.SessionStateProvisioning
	record.DraftInput = "keep me"
	dtOK(t, SaveSessionTo(database, &record))
	dtMust(saveDetachedDraftAt(database, workspace.ID, nil, "terminal", dtPtr(uint32(42)), "keep this too"))
	dtTrue(t, CloseWorkspaceAt(database, workspace.ID) != nil)
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, "keep me")
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 1)
	record.State = core.SessionStateStopped
	dtOK(t, SaveSessionTo(database, &record))
	dtOK(t, CloseWorkspaceAt(database, workspace.ID))
	late := record
	late.ID = "session-too-late"
	late.State = core.SessionStateProvisioning
	dtTrue(t, SaveSessionTo(database, &late) != nil, "creation cannot resurrect a removed workspace")
}

// Hard-won: 767a0c57: a streamed commit between frontier and transcript reads made a valid projection fail session loading.
func TestProjectionReadsKeepOneSnapshotWhenAWriterCommitsAfterTheFrontierRead(t *testing.T) {
	for _, mode := range []string{"whole", "tail", "summary"} {
		database := dtDatabase(t, "hel.sqlite3")
		record := session("session-1", "project-1")
		dtOK(t, SaveSessionTo(database, &record))
		before := MaterializedSession("session-1")
		before.Transcript = before.Transcript[:2]
		dtOK(t, SaveMaterializedSessionTo(database, &before))
		beforeSummary := dtMust(loadMaterializedSessionSummaryFrom(database, "session-1"))
		after := before
		after.Transcript = append([]*core.TranscriptItem{}, before.Transcript...)
		after.AppliedEventOrdinal = 8
		after.AppliedEventDigest = EventDigest(8)
		message := *after.Transcript[1]
		message.LatestContentEventOrdinal = dtPtr(uint64(8))
		if agent, ok := message.Body.(core.TranscriptBodyAgent); ok {
			agent.Chunks = append(append([]json.RawMessage{}, agent.Chunks...), dtAgentText(t, "New streamed content"))
			message.Body = agent
		}
		after.Transcript[1] = &message
		after.Transcript = append(after.Transcript, &core.TranscriptItem{
			StableID:        "system:8",
			Position:        8,
			CreatedAtMs:     2_000,
			LastChangedAtMs: 2_000,
			Body:            core.TranscriptBodySystem{Text: "New publication"},
		})
		writerPath := database
		hookFailures := make(chan error, 1)
		hook := func() {
			// A separate WAL connection commits between the reader's queries.
			// This must succeed without waiting for the reader to finish.
			hookFailures <- SaveMaterializedSessionTo(writerPath, &after)
		}
		afterMaterializedFrontierReadHook.Store(&hook)
		switch mode {
		case "whole":
			dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")), &before)
		case "tail":
			actual, window, err := loadMaterializedProjectionTailFrom(database, "session-1", 1)
			dtOK(t, err)
			dtTrue(t, actual != nil && window != nil)
			dtEqual(t, window.OmittedItems, 1)
			before.Transcript = before.Transcript[1:]
			dtEqual(t, actual, &before)
		default:
			dtEqual(t, dtMust(loadMaterializedSessionSummaryFrom(database, "session-1")), beforeSummary)
		}
		dtTrue(t, afterMaterializedFrontierReadHook.Load() == nil)
		dtOK(t, <-hookFailures)
		dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")).AppliedEventOrdinal, uint64(8))
	}
}

func TestContinuationReadsEarlierAuthorizationOutsideTheUiWindowAtOneFrontier(t *testing.T) {
	database := dtDatabase(t, "continuation.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	materialized.Transcript = []*core.TranscriptItem{
		{
			StableID:        "user:request",
			Position:        1,
			CreatedAtMs:     1,
			LastChangedAtMs: 1,
			Body:            core.TranscriptBodyUser{Content: dtTextContent(t, "Implement the parser and run tests")},
		},
		{
			StableID:        "system:notice",
			Position:        2,
			CreatedAtMs:     2,
			LastChangedAtMs: 2,
			Body:            core.TranscriptBodySystem{Text: "not authorization"},
		},
		{
			StableID:                  "agent:reply",
			Position:                  3,
			LatestContentEventOrdinal: dtPtr(uint64(3)),
			CreatedAtMs:               3,
			LastChangedAtMs:           3,
			Body: core.TranscriptBodyAgent{
				Chunks:    []json.RawMessage{dtAgentText(t, "Implemented. Shall I test?")},
				Streaming: false,
			},
		},
	}
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	window, metadata, err := loadMaterializedProjectionTailFrom(database, "session-1", 1)
	dtOK(t, err)
	dtTrue(t, window != nil && metadata != nil)
	dtTrue(t, metadata.OmittedItems > 0)
	_, err = transcript.Evidence(window)
	dtTrue(t, err != nil)
	collected := dtMust(loadContinuationEvidenceFrom(database, "session-1", materialized.AppliedEventOrdinal, materialized.AppliedEventDigest))
	dtEqual(t, collected, dtMust(transcript.Evidence(&materialized)))
	dtEqual(t, collected.Messages[0].Text, "Implement the parser and run tests")
	_, err = loadContinuationEvidenceFrom(database, "session-1", materialized.AppliedEventOrdinal+1, materialized.AppliedEventDigest)
	dtTrue(t, err != nil)
	_, err = loadContinuationEvidenceFrom(database, "session-1", materialized.AppliedEventOrdinal, "wrong-digest")
	dtTrue(t, err != nil)
}

func TestQuotaRecoveryMigrationAdvancesTheBreakingFloorAndPreservesCache(t *testing.T) {
	path := dtDatabase(t, "isolated-quota-migration.sqlite3")
	dtOK(t, dtMust(open(path)).Close())
	forgetVerifiedSchema(path)
	raw := dtMust(OpenConnection(path))
	restoreGithubV84Schema(t, raw)
	restorePre101SubagentPreference(t, raw)
	restoreReviewStateSchema(t, raw, 43)
	restorePre153TranscriptLayout(t, raw)
	dtExec(t, raw, `DROP TABLE quota_reset_cache;
        DELETE FROM schema_migrations WHERE version >= 44;
        UPDATE schema_compatibility SET minimum_compatible_version = 43;
        DROP TABLE IF EXISTS operator_away_requests; DROP TABLE IF EXISTS operator_away_workers; DROP TABLE IF EXISTS operator_away; DROP TABLE IF EXISTS subagent_accounting; DROP TABLE IF EXISTS session_turn_selections; ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; ALTER TABLE session_restart_intents DROP COLUMN automatic; ALTER TABLE mailbox_outbox DROP COLUMN failure; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql=replace(sql, '''startup-cleanup'',', '') WHERE type='table' AND name='sessions'; PRAGMA writable_schema=RESET; PRAGMA user_version = 43;`)
	dtOK(t, raw.Close())
	connection := dtMust(open(path))
	state := dtMust(readSchemaState(connection))
	dtEqual(t, state.revision, schemaVersion)
	floor := dtQuery[int64](t, connection, "SELECT minimum_compatible_version FROM schema_compatibility")
	dtTrue(t, floor >= 44, "older JSON readers must be refused")
	dtExec(t, connection, "INSERT INTO quota_reset_cache VALUES ('account', '{}')")
	dtOK(t, connection.Close())
	forgetVerifiedSchema(path)
	connection = dtOpen(t, path)
	body := dtQuery[string](t, connection, "SELECT body FROM quota_reset_cache WHERE identity='account'")
	dtEqual(t, body, "{}")
}

// A deferred transaction that has already read cannot wait for the WAL write
// lock: SQLite only calls the busy handler when the connection holds no
// transaction, so the upgrade returns `SQLITE_BUSY` at once. Writer-capable
// connections therefore begin IMMEDIATE (issue 1117).
// Hard-won: 3c668c3d: an activity write returned SQLITE_BUSY and stopped the API
func TestWriterConnectionsWaitForAConcurrentWriterInsteadOfFailing(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	recorder := dtOpen(t, database)
	holder := dtOpen(t, database)
	holding := make(chan struct{}, 1)
	held := make(chan error, 1)
	go func() {
		transaction, err := holder.BeginWith(TransactionImmediate)
		if err != nil {
			held <- err
			holding <- struct{}{}
			return
		}
		holding <- struct{}{}
		// Well under the writer's five-second busy timeout.
		time.Sleep(300 * time.Millisecond)
		held <- transaction.Commit()
	}()
	<-holding

	activity := core.ApiActivityState{
		State:           "running",
		IsIdle:          false,
		WaitingForInput: false,
		CapacityRetry:   false,
	}
	dtOK(t, recordAPIActivitiesWith(recorder, []APIActivityUpdate{{SessionID: "session-1", Activity: activity}}, 1_000))
	dtOK(t, <-held)

	body := dtQuery[string](t, recorder, "SELECT body FROM api_session_activity WHERE session_id = ?1", "session-1")
	var stored core.ApiActivityState
	dtOK(t, json.Unmarshal([]byte(body), &stored))
	dtEqual(t, stored, activity)
}

// The `cfg(test)` `open_reader` alias must stay DEFERRED: fixture reads may
// not take the write lock, or a reader would block the writer under test.
func TestTestReaderConnectionsKeepDeferredTransactions(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	reader := dtOpenReader(t, database)
	snapshot := dtBegin(t, reader.Connection)
	sessions := dtQuery[int64](t, snapshot, "SELECT count(*) FROM sessions")
	dtEqual(t, sessions, int64(1))

	writer := dtOpen(t, database)
	write := dtBegin(t, writer)
	dtExec(t, write, "DELETE FROM api_session_activity")
	dtOK(t, write.Commit())
	dtOK(t, snapshot.Rollback())
}
