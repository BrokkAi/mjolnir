// Port of mj-store/src/database/tests.rs, continued (workspaces, pane
// sizes, layouts and detached drafts).

package store

import (
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func workspacePaneSizeRowCount(t testing.TB, path, workspaceID string) int64 {
	t.Helper()
	return dtQuery[int64](t, dtRaw(t, path), "SELECT count(*) FROM workspace_pane_sizes WHERE workspace_id = ?1", workspaceID)
}

func TestWorkspacePaneSizesRoundTripAfterReopeningTheDatabase(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Roundtrip"))
	sizes := core.PaneSizes{
		Sessions: core.PaneSizeMaximized,
		Targets:  core.PaneSizeMinimized,
		Quota:    core.PaneSizeStandard,
	}

	dtOK(t, SaveWorkspacePaneSizesTo(database, workspace.ID, sizes))
	dtOK(t, dtMust(open(database)).Close())
	forgetVerifiedSchema(database)

	dtEqual(t, dtMust(LoadWorkspacePaneSizesFrom(database, workspace.ID)), sizes)
}

func TestWorkspacePaneSizesAreIsolatedBetweenWorkspaces(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	first := dtMust(CreateWorkspaceAt(database, "First"))
	second := dtMust(CreateWorkspaceAt(database, "Second"))
	firstSizes := core.PaneSizes{
		Sessions: core.PaneSizeMinimized,
		Targets:  core.PaneSizeMaximized,
		Quota:    core.PaneSizeStandard,
	}
	secondSizes := core.PaneSizes{
		Sessions: core.PaneSizeStandard,
		Targets:  core.PaneSizeMinimized,
		Quota:    core.PaneSizeMaximized,
	}

	dtOK(t, SaveWorkspacePaneSizesTo(database, first.ID, firstSizes))
	dtOK(t, SaveWorkspacePaneSizesTo(database, second.ID, secondSizes))

	dtEqual(t, dtMust(LoadWorkspacePaneSizesFrom(database, first.ID)), firstSizes)
	dtEqual(t, dtMust(LoadWorkspacePaneSizesFrom(database, second.ID)), secondSizes)
}

func TestWorkspacePaneSizesSurviveRenameAndCascadeThroughBothDeletions(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	renamed := dtMust(CreateWorkspaceAt(database, "Before"))
	sizes := core.PaneSizes{
		Sessions: core.PaneSizeStandard,
		Targets:  core.PaneSizeMaximized,
		Quota:    core.PaneSizeMinimized,
	}
	dtOK(t, SaveWorkspacePaneSizesTo(database, renamed.ID, sizes))

	dtOK(t, RenameWorkspaceAt(database, renamed.ID, "After"))
	dtEqual(t, dtMust(LoadWorkspacePaneSizesFrom(database, renamed.ID)), sizes,
		"renaming changes the display name, not the stable settings owner")
	dtOK(t, DeleteWorkspaceAt(database, renamed.ID))
	dtEqual(t, workspacePaneSizeRowCount(t, database, renamed.ID), int64(0))

	forceDeleted := dtMust(CreateWorkspaceAt(database, "Force"))
	dtOK(t, SaveWorkspacePaneSizesTo(database, forceDeleted.ID, core.PaneSizes{
		Sessions: core.PaneSizeMinimized,
		Targets:  core.PaneSizeStandard,
		Quota:    core.PaneSizeStandard,
	}))
	dtOK(t, CloseWorkspaceAt(database, forceDeleted.ID))
	dtEqual(t, workspacePaneSizeRowCount(t, database, forceDeleted.ID), int64(0))
}

func TestInvalidPaneSizeSavePreservesThePreviousRowAndUnknownWorkspacesFail(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Validated"))
	previous := core.PaneSizes{
		Sessions: core.PaneSizeMaximized,
		Targets:  core.PaneSizeMinimized,
		Quota:    core.PaneSizeStandard,
	}
	dtOK(t, SaveWorkspacePaneSizesTo(database, workspace.ID, previous))

	invalid := core.PaneSizes{
		Sessions: core.PaneSizeMaximized,
		Targets:  core.PaneSizeMaximized,
		Quota:    core.PaneSizeStandard,
	}
	dtTrue(t, SaveWorkspacePaneSizesTo(database, workspace.ID, invalid) != nil)
	dtEqual(t, dtMust(LoadWorkspacePaneSizesFrom(database, workspace.ID)), previous)
	_, err := LoadWorkspacePaneSizesFrom(database, "missing-workspace")
	dtTrue(t, err != nil)
	dtTrue(t, SaveWorkspacePaneSizesTo(database, "missing-workspace", previous) != nil)
}

func TestMalformedPersistedPaneSizeIsReportedOnRead(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Malformed"))
	dtOK(t, SaveWorkspacePaneSizesTo(database, workspace.ID, core.PaneSizes{
		Sessions: core.PaneSizeStandard,
		Targets:  core.PaneSizeMinimized,
		Quota:    core.PaneSizeStandard,
	}))

	connection := dtRaw(t, database)
	dtExec(t, connection, "PRAGMA ignore_check_constraints = ON;")
	dtExec(t, connection, "UPDATE workspace_pane_sizes SET sessions = 'corrupt' WHERE workspace_id = ?1", workspace.ID)
	dtExec(t, connection, "PRAGMA ignore_check_constraints = OFF;")

	_, err := LoadWorkspacePaneSizesFrom(database, workspace.ID)
	dtTrue(t, err != nil && strings.Contains(err.Error(), "unknown pane size"), err)
}

func workspaceLayoutRowCount(t testing.TB, path, workspaceID string) int64 {
	t.Helper()
	return dtQuery[int64](t, dtRaw(t, path), "SELECT count(*) FROM workspace_layouts WHERE workspace_id = ?1", workspaceID)
}

func splitLayout(firstSession, secondSession string) core.ConversationLayout {
	return core.ConversationLayout{
		Pins: map[string]uint32{},
		Root: core.LayoutNodeSplit{
			Axis:   core.SplitAxisHorizontal,
			Ratio:  0.6,
			First:  core.LayoutNodePane{ID: 1},
			Second: core.LayoutNodePane{ID: 2},
		},
		Focus: 2,
		Sessions: map[uint32]string{
			1: firstSession,
			2: secondSession,
		},
	}
}

func TestWorkspaceLayoutRoundTripsAfterReopeningTheDatabase(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Roundtrip"))
	layout := splitLayout("session-1", "session-2")
	layout.Browse = dtPtr(uint32(2))
	layout.Pins["session-1"] = 5

	dtOK(t, SaveWorkspaceLayoutTo(database, workspace.ID, &layout))
	dtOK(t, dtMust(open(database)).Close())
	forgetVerifiedSchema(database)

	dtEqual(t, dtMust(LoadWorkspaceLayoutFrom(database, workspace.ID)), layout)
}

func TestWorkspaceLayoutsAreIsolatedBetweenWorkspaces(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	first := dtMust(CreateWorkspaceAt(database, "First"))
	second := dtMust(CreateWorkspaceAt(database, "Second"))
	firstLayout := splitLayout("session-1", "session-2")
	secondLayout := core.NewConversationLayout()
	secondLayout.Browse = nil
	secondLayout.Pins = map[string]uint32{}
	secondLayout.Focus = 1
	secondLayout.Sessions = map[uint32]string{1: "session-3"}

	dtOK(t, SaveWorkspaceLayoutTo(database, first.ID, &firstLayout))
	dtOK(t, SaveWorkspaceLayoutTo(database, second.ID, &secondLayout))

	dtEqual(t, dtMust(LoadWorkspaceLayoutFrom(database, first.ID)), firstLayout)
	dtEqual(t, dtMust(LoadWorkspaceLayoutFrom(database, second.ID)), secondLayout)
}

func TestWorkspaceLayoutsCascadeThroughWorkspaceRemoval(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	deleted := dtMust(CreateWorkspaceAt(database, "Deleted"))
	deletedLayout := splitLayout("a", "b")
	dtOK(t, SaveWorkspaceLayoutTo(database, deleted.ID, &deletedLayout))
	dtOK(t, DeleteWorkspaceAt(database, deleted.ID))
	dtEqual(t, workspaceLayoutRowCount(t, database, deleted.ID), int64(0))

	forceDeleted := dtMust(CreateWorkspaceAt(database, "Force"))
	forceLayout := splitLayout("c", "d")
	dtOK(t, SaveWorkspaceLayoutTo(database, forceDeleted.ID, &forceLayout))
	dtOK(t, CloseWorkspaceAt(database, forceDeleted.ID))
	dtEqual(t, workspaceLayoutRowCount(t, database, forceDeleted.ID), int64(0))
}

func TestInvalidLayoutSavePreservesThePreviousRowAndUnknownWorkspacesFail(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Validated"))
	previous := splitLayout("session-1", "session-2")
	dtOK(t, SaveWorkspaceLayoutTo(database, workspace.ID, &previous))

	invalid := splitLayout("session-1", "session-2")
	invalid.Root = core.LayoutNodePane{ID: 1}
	dtTrue(t, SaveWorkspaceLayoutTo(database, workspace.ID, &invalid) != nil,
		"a session recorded for a pane outside the tree must be refused")
	dtEqual(t, dtMust(LoadWorkspaceLayoutFrom(database, workspace.ID)), previous)
	_, err := LoadWorkspaceLayoutFrom(database, "missing-workspace")
	dtTrue(t, err != nil)
	dtTrue(t, SaveWorkspaceLayoutTo(database, "missing-workspace", &previous) != nil)
}

func TestMalformedPersistedLayoutIsReportedOnRead(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Malformed"))
	layout := splitLayout("a", "b")
	dtOK(t, SaveWorkspaceLayoutTo(database, workspace.ID, &layout))

	dtExec(t, dtRaw(t, database),
		`UPDATE workspace_layouts SET layout = '{"root":{"kind":"pane","id":1},"focus":9,"sessions":{}}' WHERE workspace_id = ?1`,
		workspace.ID)

	_, err := LoadWorkspaceLayoutFrom(database, workspace.ID)
	dtTrue(t, err != nil && strings.Contains(err.Error(), "focused pane 9"), err)
}

func dtWorkspaceIDs(workspaces []core.WorkspaceRecord) []string {
	ids := []string{}
	for _, workspace := range workspaces {
		ids = append(ids, workspace.ID)
	}
	return ids
}

func TestWorkspaceCrudPreservesHistoryAndBlocksActiveSessionsAndDrafts(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "  Bifrost  "))
	dtEqual(t, workspace.Name, "Bifrost")
	_, err := CreateWorkspaceAt(database, "bIFROST")
	dtTrue(t, err != nil)

	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	dtOK(t, AssignNewSessionWorkspaceAt(database, "session-1", workspace.ID))
	materialized := MaterializedSession("session-1")
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	dtOK(t, recordPromptTo(database, "session-1", "project-1", 1, dtPtr("2026-09-03T00:00:00Z"), "remember this prompt"))
	dtTrue(t, AssignNewSessionWorkspaceAt(database, "session-1", core.DefaultWorkspaceID) != nil)
	dtEqual(t, dtMust(ListWorkspacesFrom(database))[0].SessionCount, uint64(0))

	dtOK(t, DeleteWorkspaceAt(database, workspace.ID))
	preserved := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(preserved, "session-1").WorkspaceID, workspace.ID)
	dtEqual(t, dtSessionOf(preserved, "session-1").Checkpoint, session("session-1", "project-1").Checkpoint)
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")), &materialized)
	dtEqual(t, len(dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeSession, "remember this prompt"))), 1)

	activeWorkspace := dtMust(CreateWorkspaceAt(database, "Active"))
	active := session("session-active", "project-1")
	active.WorkspaceID = activeWorkspace.ID
	active.State = core.SessionStateRunning
	dtOK(t, SaveSessionTo(database, &active))
	err = dtErr(t, DeleteWorkspaceAt(database, activeWorkspace.ID))
	dtTrue(t, strings.Contains(err.Error(), "1 active sessions, 0 drafts"), err)

	draftWorkspace := dtMust(CreateWorkspaceAt(database, "Drafts"))
	dtMust(saveDetachedDraftAt(database, draftWorkspace.ID, nil, "terminal", dtPtr(uint32(42)), "unfinished"))
	err = dtErr(t, DeleteWorkspaceAt(database, draftWorkspace.ID))
	dtTrue(t, strings.Contains(err.Error(), "0 active sessions, 1 drafts"), err)

	empty := dtMust(CreateWorkspaceAt(database, "Empty"))
	dtOK(t, RenameWorkspaceAt(database, empty.ID, "Renamed"))
	dtOK(t, DeleteWorkspaceAt(database, empty.ID))
	dtTrue(t, !dtContains(dtWorkspaceIDs(dtMust(ListWorkspacesFrom(database))), empty.ID))
}

func TestCloseWorkspaceDropsDraftsAndPreservesStoppedHistories(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Force"))

	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	dtOK(t, AssignNewSessionWorkspaceAt(database, "session-1", workspace.ID))
	materialized := MaterializedSession("session-1")
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	dtOK(t, recordPromptTo(database, "session-1", "project-1", 1, dtPtr("2026-09-03T00:00:00Z"), "remember this prompt"))
	dtMust(saveDetachedDraftAt(database, workspace.ID, nil, "terminal", dtPtr(uint32(42)), "unfinished"))

	dtOK(t, CloseWorkspaceAt(database, workspace.ID))

	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 0)
	preserved := dtMust(LoadStateFrom(database))
	dtEqual(t, dtSessionOf(preserved, "session-1").WorkspaceID, workspace.ID)
	dtEqual(t, dtSessionOf(preserved, "session-1").Checkpoint, session("session-1", "project-1").Checkpoint)
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")), &materialized)
	dtEqual(t, len(dtMust(searchPromptsFrom(database, "session-1", "project-1", core.HistoryScopeSession, "remember this prompt"))), 1)
	dtTrue(t, !dtContains(dtWorkspaceIDs(dtMust(ListWorkspacesFrom(database))), workspace.ID))
}

func TestCloseWorkspaceRefusesRemainingActiveSessions(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Active"))
	active := session("session-active", "project-1")
	active.WorkspaceID = workspace.ID
	active.State = core.SessionStateRunning
	dtOK(t, SaveSessionTo(database, &active))
	dtMust(saveDetachedDraftAt(database, workspace.ID, nil, "terminal", dtPtr(uint32(42)), "unfinished"))

	err := dtErr(t, CloseWorkspaceAt(database, workspace.ID))
	dtTrue(t, strings.Contains(err.Error(), "1 active sessions remain"), err)
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 1,
		"a refused deletion must not drop the workspace's drafts")
}

// dtSaveTerminalDraft is save_terminal_draft_in on a fresh writer connection.
func dtSaveTerminalDraft(database, sessionID, source string, ownerPid uint32, revision int64, draft core.DetachedSessionDraft) error {
	connection, err := open(database)
	if err != nil {
		return err
	}
	defer connection.Close()
	return saveTerminalDraftIn(connection, sessionID, source, ownerPid, revision, &draft)
}

func TestOnlyResumableSessionsCanMoveToANewWorkspace(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	source := dtMust(CreateWorkspaceAt(database, "Source"))
	destination := dtMust(CreateWorkspaceAt(database, "Destination"))
	stopped := session("session-stopped", "project-1")
	stopped.WorkspaceID = source.ID
	dtOK(t, SaveSessionTo(database, &stopped))
	dtOK(t, dtSaveTerminalDraft(database, stopped.ID, "terminal-a", 1234, 1, core.DetachedSessionDraft{
		Text: "kept across resume placement",
	}))
	draftID := dtMust(listDetachedDraftsAt(database, source.ID))[0].ID

	dtOK(t, ReassignResumableSessionWorkspaceAt(database, stopped.ID, destination.ID))
	dtEqual(t, dtMust(WorkspaceForSessionAt(database, stopped.ID)), dtPtr(destination.ID))
	movedDrafts := dtMust(listDetachedDraftsAt(database, destination.ID))
	dtEqual(t, len(movedDrafts), 1)
	dtEqual(t, movedDrafts[0].ID, draftID)
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, source.ID))), 0)
	dtOK(t, ReassignResumableSessionWorkspaceAt(database, stopped.ID, destination.ID))

	running := session("session-running", "project-1")
	running.WorkspaceID = source.ID
	running.State = core.SessionStateRunning
	dtOK(t, SaveSessionTo(database, &running))
	dtTrue(t, ReassignResumableSessionWorkspaceAt(database, running.ID, destination.ID) != nil)
	dtTrue(t, ReassignResumableSessionWorkspaceAt(database, stopped.ID, "missing-workspace") != nil)
}

func TestAssigningNewSessionWorkspaceMovesManagedTerminalDraft(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	destination := dtMust(CreateWorkspaceAt(database, "Assigned"))
	record := session("session-assigned", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	dtOK(t, dtSaveTerminalDraft(database, record.ID, "terminal-a", 1234, 1, core.DetachedSessionDraft{
		Text: "keep across initial placement",
	}))
	draftID := dtMust(listDetachedDraftsAt(database, core.DefaultWorkspaceID))[0].ID

	dtOK(t, AssignNewSessionWorkspaceAt(database, record.ID, destination.ID))

	drafts := dtMust(listDetachedDraftsAt(database, destination.ID))
	dtEqual(t, len(drafts), 1)
	dtEqual(t, drafts[0].ID, draftID)
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, core.DefaultWorkspaceID))), 0)
}

func TestARunningSessionAndItsSubAgentChangeWorkspaceTogether(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	source := dtMust(CreateWorkspaceAt(database, "Source"))
	destination := dtMust(CreateWorkspaceAt(database, "Destination"))
	parent := session("session-parent", "project-1")
	parent.WorkspaceID = source.ID
	parent.State = core.SessionStateRunning
	child := session("session-child", "project-1")
	child.WorkspaceID = source.ID
	child.State = core.SessionStateRunning
	dtOK(t, SaveSessionTo(database, &parent))
	dtOK(t, SaveSessionTo(database, &child))

	ids := []string{parent.ID, child.ID}
	dtOK(t, SetSessionsWorkspaceAt(database, ids, destination.ID))
	for _, id := range ids {
		dtEqual(t, dtMust(WorkspaceForSessionAt(database, id)), dtPtr(destination.ID))
	}

	// A missing destination or session changes nothing.
	dtTrue(t, SetSessionsWorkspaceAt(database, ids, "missing-workspace") != nil)
	withMissing := []string{parent.ID, "no-such-session"}
	dtTrue(t, SetSessionsWorkspaceAt(database, withMissing, source.ID) != nil)
	dtEqual(t, dtMust(WorkspaceForSessionAt(database, parent.ID)), dtPtr(destination.ID))
}

func TestSetupWorkspaceCreationReturnsTheConcurrentNameWinner(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	dtMust(ListWorkspacesFrom(database))
	var ready, start sync.WaitGroup
	ready.Add(2)
	start.Add(1)
	results := make([]core.WorkspaceRecord, 2)
	failures := make([]error, 2)
	var done sync.WaitGroup
	for index, name := range []string{"  Bifrost  ", "bIFROST"} {
		done.Go(func() {
			// Rust's Barrier::new(2): both creators start together.
			ready.Done()
			start.Wait()
			results[index], failures[index] = CreateOrGetWorkspaceAt(database, name)
		})
	}
	ready.Wait()
	start.Done()
	done.Wait()
	dtOK(t, failures[0])
	dtOK(t, failures[1])
	winner, follower := results[0], results[1]

	dtEqual(t, follower, winner)
	dtEqual(t, dtMust(ListWorkspacesFrom(database)), []core.WorkspaceRecord{winner})
}

func TestReadFrontiersAreIndependentPerClientWithTheSessionCursorAsBaseline(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Readers"))
	record := session("session-1", "project-1")
	record.WorkspaceID = workspace.ID
	dtOK(t, SaveSessionTo(database, &record))

	dtEqual(t, dtMust(clientReadFrontierAt(database, "client-a", workspace.ID, "session-1")), uint64(7))
	dtEqual(t, dtMust(advanceClientReadFrontierAt(database, "client-a", workspace.ID, "session-1", 12)), uint64(12))
	dtEqual(t, dtMust(clientReadFrontierAt(database, "client-b", workspace.ID, "session-1")), uint64(7))
}

func TestDetachingRetiresInheritedInputEvenAfterTheComposerIsCleared(t *testing.T) {
	for _, current := range []string{"", "an edited unfinished thought"} {
		database := dtDatabase(t, "hel.sqlite3")
		workspace := dtMust(CreateWorkspaceAt(database, "Drafts"))
		record := session("session-1", "project-1")
		record.WorkspaceID = workspace.ID
		record.DraftInput = "/model g"
		dtOK(t, SaveSessionTo(database, &record))
		dtOK(t, setSessionDraftInputAt(database, record.ID, record.DraftInput))

		dtOK(t, dtSaveTerminalDraft(database, record.ID, "tui-client-a", 1234, 1, core.DetachedSessionDraft{
			Text:           current,
			InheritedInput: dtPtr(record.DraftInput),
		}))

		reloaded := dtMust(LoadStateFrom(database))
		dtEqual(t, dtSessionOf(reloaded, record.ID).DraftInput, "")
		drafts := dtMust(listDetachedDraftsAt(database, workspace.ID))
		if current == "" {
			dtEqual(t, len(drafts), 0)
		} else {
			dtEqual(t, len(drafts), 1)
			dtEqual(t, drafts[0].Text, current)
			dtEqual(t, drafts[0].Source, "tui-client-a")
			dtEqual(t, drafts[0].OwnerPid, dtPtr(uint32(1234)))
			// Explicit recovery is still supported after retiring the seed.
			dtMust(recoverDetachedDraftAt(database, drafts[0].ID))
			dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, current)
		}
	}
}

func TestDetachingPreservesANewerRecoveryAndRollsBackWhenSavingFails(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Drafts"))
	record := session("session-1", "project-1")
	record.WorkspaceID = workspace.ID
	record.DraftInput = "newly recovered input"
	dtOK(t, SaveSessionTo(database, &record))
	dtOK(t, setSessionDraftInputAt(database, record.ID, record.DraftInput))
	connection := dtOpen(t, database)

	dtOK(t, saveTerminalDraftIn(connection, record.ID, "tui-client-a", 1234, 1, &core.DetachedSessionDraft{
		Text:           "",
		InheritedInput: dtPtr("older inherited input"),
	}))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, record.DraftInput)

	result := saveTerminalDraftIn(connection, record.ID, "failing-client", 0, 1, &core.DetachedSessionDraft{
		Text:           "must survive the failed save",
		InheritedInput: dtPtr(record.DraftInput),
	})
	dtTrue(t, result != nil)
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, record.DraftInput)
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 0)
}

// Hard-won: #1486: a moved composer must retire its inherited seed without a client workspace snapshot.
func TestAMovedSessionRetiresItsInheritedSeedOnFirstTerminalSave(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	owner := dtMust(CreateWorkspaceAt(database, "Owner"))
	previous := dtMust(CreateWorkspaceAt(database, "Previous"))
	record := session("session-1", "project-1")
	record.WorkspaceID = owner.ID
	dtOK(t, SaveSessionTo(database, &record))
	dtOK(t, setSessionDraftInputAt(database, record.ID, "inherited input"))
	dtOK(t, SetSessionsWorkspaceAt(database, []string{record.ID}, previous.ID))

	for _, entry := range []struct {
		revision int64
		text     string
	}{{1, ""}, {2, "unsent after moving"}} {
		dtOK(t, dtSaveTerminalDraft(database, record.ID, "old-client", 1234, entry.revision, core.DetachedSessionDraft{
			Text:           entry.text,
			InheritedInput: dtPtr("inherited input"),
		}))
		dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, "")
	}
	drafts := dtMust(listDetachedDraftsAt(database, previous.ID))
	dtEqual(t, len(drafts), 1)
	dtEqual(t, drafts[0].Text, "unsent after moving")
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, owner.ID))), 0)
}

// Hard-won: #1486: workspace snapshots could block inherited-seed retirement after a move.
func TestMovingSessionKeepsTerminalDraftCanonicalAcrossWorkspaceCloseAndLateSaves(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	source := dtMust(CreateWorkspaceAt(database, "Source"))
	destination := dtMust(CreateWorkspaceAt(database, "Destination"))
	record := session("session-moved", "project-1")
	record.WorkspaceID = source.ID
	dtOK(t, SaveSessionTo(database, &record))

	save := func(revision int64, text string) error {
		return dtSaveTerminalDraft(database, record.ID, "terminal-a", 1234, revision, core.DetachedSessionDraft{Text: text})
	}
	dtOK(t, save(1, "unchanged composer"))
	draftID := dtMust(listDetachedDraftsAt(database, source.ID))[0].ID

	dtOK(t, SetSessionsWorkspaceAt(database, []string{record.ID}, destination.ID))
	moved := dtMust(listDetachedDraftsAt(database, destination.ID))
	dtEqual(t, len(moved), 1)
	dtEqual(t, moved[0].ID, draftID)
	dtEqual(t, moved[0].Text, "unchanged composer")
	dtOK(t, CloseWorkspaceAt(database, source.ID))

	// A delayed old-workspace edit follows the session's canonical placement.
	dtOK(t, save(2, "newer edit"))
	// A stale lower revision cannot restore the removed source placement.
	dtOK(t, save(1, "old text"))
	dtOK(t, save(3, "latest edit"))

	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, source.ID))), 0)
	latest := dtMust(listDetachedDraftsAt(database, destination.ID))
	dtEqual(t, len(latest), 1)
	dtEqual(t, latest[0].ID, draftID)
	dtEqual(t, latest[0].Text, "latest edit")
	connection := dtOpen(t, database)
	var revision int64
	var workspaceID string
	dtOK(t, connection.QueryRow("SELECT revision, workspace_id FROM detached_drafts WHERE draft_id = ?1", draftID).
		Scan(&revision, &workspaceID))
	dtEqual(t, revision, int64(3))
	dtEqual(t, workspaceID, destination.ID)
}

func TestTerminalDraftRevisionsKeepOneRowAndIgnoreLateClearedWrites(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Drafts"))
	record := session("session-1", "project-1")
	record.WorkspaceID = workspace.ID
	record.DraftInput = "adopt me"
	dtOK(t, SaveSessionTo(database, &record))

	save := func(revision int64, text string) error {
		draft := core.DetachedSessionDraft{Text: text}
		if revision == 1 {
			draft.InheritedInput = dtPtr("adopt me")
		}
		return dtSaveTerminalDraft(database, record.ID, "terminal-a", 1234, revision, draft)
	}
	dtOK(t, save(1, "first draft"))
	rowID := dtMust(listDetachedDraftsAt(database, workspace.ID))[0].ID
	dtOK(t, save(2, "newer draft"))
	dtOK(t, save(3, ""))
	dtOK(t, save(2, "late nonempty draft"))

	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 0)
	connection := dtOpen(t, database)
	var id, text string
	var revision int64
	dtOK(t, connection.QueryRow(`SELECT draft_id, revision, text FROM detached_drafts
              WHERE source='terminal-a' AND workspace_id=?1 AND session_id=?2`, workspace.ID, record.ID).
		Scan(&id, &revision, &text))
	dtEqual(t, id, rowID)
	dtEqual(t, revision, int64(3))
	dtEqual(t, text, "")
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, "")

	dtOK(t, save(4, "terminal-a restored"))
	dtOK(t, dtSaveTerminalDraft(database, record.ID, "terminal-b", 5678, 1, core.DetachedSessionDraft{
		Text: "independent client",
	}))
	visible := dtMust(listDetachedDraftsAt(database, workspace.ID))
	dtEqual(t, len(visible), 2)
}

func TestTerminalDraftRecoverySurvivesIdenticalSaveButNewTextReopensIt(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Drafts"))
	record := session("session-1", "project-1")
	record.WorkspaceID = workspace.ID
	record.DraftInput = "seed"
	dtOK(t, SaveSessionTo(database, &record))
	save := func(revision int64, text string) error {
		return dtSaveTerminalDraft(database, record.ID, "terminal-a", 1234, revision, core.DetachedSessionDraft{
			Text:           text,
			InheritedInput: dtPtr("seed"),
		})
	}
	dtOK(t, save(1, "seed"))
	draftID := dtMust(listDetachedDraftsAt(database, workspace.ID))[0].ID
	dtMust(recoverDetachedDraftAt(database, draftID))

	// Lost ACK retry after recovery must not compare-clear the recovered seed.
	dtOK(t, save(1, "seed"))
	dtOK(t, save(2, "seed"))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, "seed")
	dtEqual(t, len(dtMust(listDetachedDraftsAt(database, workspace.ID))), 0)
	dtOK(t, save(3, "new unsent edit"))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), record.ID).DraftInput, "seed")
	visible := dtMust(listDetachedDraftsAt(database, workspace.ID))
	dtEqual(t, len(visible), 1)
	dtEqual(t, visible[0].ID, draftID)
	dtEqual(t, visible[0].Text, "new unsent edit")
}

func TestDetachedDraftsKeepSourcePidAndWorkspaceWithoutOverwritingEachOther(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	workspace := dtMust(CreateWorkspaceAt(database, "Drafts"))
	record := session("session-1", "project-1")
	record.WorkspaceID = workspace.ID
	dtOK(t, SaveSessionTo(database, &record))

	dtMust(saveDetachedDraftAt(database, workspace.ID, dtPtr("session-1"), "tui-client-a", dtPtr(uint32(1234)), "first unfinished thought"))
	dtMust(saveDetachedDraftAt(database, workspace.ID, dtPtr("session-1"), "tui-client-b", dtPtr(uint32(5678)), "second unfinished thought"))

	drafts := dtMust(listDetachedDraftsAt(database, workspace.ID))
	dtEqual(t, len(drafts), 2)
	has := func(source string, pid uint32, text string) bool {
		for _, draft := range drafts {
			if draft.Source == source && draft.OwnerPid != nil && *draft.OwnerPid == pid && draft.Text == text {
				return true
			}
		}
		return false
	}
	dtTrue(t, has("tui-client-a", 1234, "first unfinished thought"))
	dtTrue(t, has("tui-client-b", 5678, "second unfinished thought"))
}
