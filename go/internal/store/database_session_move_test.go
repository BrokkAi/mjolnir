// Port of the tests in mj-store/src/database/session_move.rs.

package store

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// moveTestOperation is the tests' Move intent for session: Rust's
// operation().
func moveTestOperation(session *core.SessionRecord) core.MoveOperation {
	destination, local := "destination", "local"
	mounts := []core.AdditionalMount{}
	var checkpoint *core.CheckpointMetadata
	if session.Checkpoint != nil {
		copied := *session.Checkpoint
		checkpoint = &copied
	}
	recovery := *session
	return core.MoveOperation{
		OperationID: "move-one",
		Selection: core.MoveSelection{
			SessionID:        session.ID,
			ProfileID:        &destination,
			TargetTemplateID: &local,
			AdditionalMounts: &mounts,
		},
		SourceProfileID:          session.LastProfile,
		SourceTargetTemplateID:   session.TargetTemplateID,
		SourceTarget:             session.Target,
		SourceNativeSessionID:    session.NativeSessionID,
		SourceAdditionalMounts:   session.AdditionalMounts,
		SourceResourceAllocation: session.ResourceAllocation,
		ConfigurationFingerprint: "fingerprint",
		Checkpoint:               checkpoint,
		RecoverySession:          &recovery,
		Queue:                    core.ResumeQueueDispositionStart,
		Phase:                    core.MovePhaseCapturing,
		CreatedAt:                session.CreatedAt,
		UpdatedAt:                session.UpdatedAt,
	}
}

func moveTestOpen(t *testing.T, path string) *Connection {
	t.Helper()
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { connection.Close() })
	return connection
}

func moveTestLoad(t *testing.T, connection Queryer, sessionID string) *core.MoveOperation {
	t.Helper()
	operation, err := loadMoveOperationWith(connection, sessionID)
	if err != nil {
		t.Fatal(err)
	}
	return operation
}

func moveTestSave(t *testing.T, connection Queryer, operation *core.MoveOperation) {
	t.Helper()
	if err := saveMoveOperationWith(connection, operation); err != nil {
		t.Fatal(err)
	}
}

func moveTestSaveSession(t *testing.T, path string, record *core.SessionRecord) {
	t.Helper()
	if err := SaveSessionTo(path, record); err != nil {
		t.Fatal(err)
	}
}

// moveTestSet sets the member at the end of keys in value, an object tree
// in jsonvalue form, and returns the changed tree.
func moveTestSet(value any, member any, keys ...string) any {
	object, _ := value.(jsonvalue.Object)
	if len(keys) == 1 {
		object.Set(keys[0], member)
		return object
	}
	inner, _ := object.Get(keys[0])
	object.Set(keys[0], moveTestSet(inner, member, keys[1:]...))
	return object
}

func TestSchemaPreservesLegacyMoveOrchestratorChoices(t *testing.T) {
	path := filepath.Join(t.TempDir(), "legacy-move.sqlite")
	source := session("source", "project")
	source.Subagents = &core.SubagentPolicy{
		Mode:            core.SubagentPolicyMjolnir,
		Straightforward: &core.SubagentChoice{Model: "my-worker-model"},
		Orchestrators:   true,
	}
	connection := moveTestOpen(t, path)
	tx, err := connection.Begin()
	if err != nil {
		t.Fatal(err)
	}
	if err := insertSession(tx, &source); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}

	operation := moveTestOperation(&source)
	encoded, err := json.Marshal(&operation)
	if err != nil {
		t.Fatal(err)
	}
	intent, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	intent = moveTestSet(intent, false, "selection", "supervisors")
	intent = moveTestSet(intent, true, "recovery_session", "supervisors")
	recovery, _ := intent.(jsonvalue.Object).Get("recovery_session")
	subagents, _ := recovery.(jsonvalue.Object).Get("subagents")
	policy := subagents.(jsonvalue.Object)
	policy.Delete("orchestrators")
	intent = moveTestSet(intent, policy, "recovery_session", "subagents")
	text, err := jsonvalue.Marshal(intent)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := connection.Exec("INSERT INTO session_moves(session_id, operation_id, operation_json) VALUES (?1, ?2, ?3)",
		source.ID, operation.OperationID, string(text)); err != nil {
		t.Fatal(err)
	}
	restored := moveTestLoad(t, connection, source.ID)
	if restored == nil || restored.RecoverySession == nil {
		t.Fatal("the legacy intent must decode")
	}
	if !reflect.DeepEqual(restored.RecoverySession.Subagents, source.Subagents) {
		t.Fatalf("recovery policy = %+v", restored.RecoverySession.Subagents)
	}
	selected := restored.Selection.Subagents
	if selected == nil || selected.Mode != core.SubagentPolicyMjolnir || selected.Orchestrators ||
		selected.Straightforward == nil || selected.Straightforward.Model != "my-worker-model" {
		t.Fatalf("selection policy = %+v", selected)
	}
	canonical, err := json.Marshal(&restored.Selection)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(canonical), "supervisors") {
		t.Fatalf("canonical selection kept the legacy opt-in: %s", canonical)
	}

	intent = moveTestSet(intent, nil, "recovery_session")
	text, err = jsonvalue.Marshal(intent)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := connection.Exec("UPDATE session_moves SET operation_json=?2 WHERE session_id=?1", source.ID, string(text)); err != nil {
		t.Fatal(err)
	}
	withoutSnapshot := moveTestLoad(t, connection, source.ID)
	if withoutSnapshot == nil || !reflect.DeepEqual(withoutSnapshot.Selection, restored.Selection) {
		t.Fatalf("selection without a snapshot = %+v", withoutSnapshot)
	}
}

func TestMoveOutcomeCommitsTheMessageWithTheResultAndRollsBackBothOnFailure(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	record := session("moving", "project")
	record.State = core.SessionStateClosing
	record.DraftInput = "a concurrent draft"
	moveTestSaveSession(t, path, &record)
	connection := moveTestOpen(t, path)
	intent := moveTestOperation(&record)
	moveTestSave(t, connection, &intent)
	if _, err := connection.Exec(`CREATE TRIGGER refuse_move_message BEFORE UPDATE OF last_error ON sessions
             BEGIN SELECT RAISE(ABORT, 'message write failed'); END;`); err != nil {
		t.Fatal(err)
	}
	intent.Phase = core.MovePhaseFailed
	diagnostic, message := "private diagnostic", "public message"
	intent.Error = &diagnostic
	if saveMoveOutcomeWith(connection, &intent, &message, true) == nil {
		t.Fatal("the refused message must fail the outcome")
	}
	if loaded := moveTestLoad(t, connection, record.ID); loaded == nil || loaded.Phase != core.MovePhaseCapturing {
		t.Fatalf("intent after the failed outcome = %+v", loaded)
	}
	failedWrite, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	if stored := failedWrite.Sessions.Index(record.ID); stored.LastError != nil || stored.State != core.SessionStateClosing {
		t.Fatalf("session after the failed outcome = %v, %s", stored.LastError, stored.State)
	}
	if _, err := connection.Exec("DROP TRIGGER refuse_move_message"); err != nil {
		t.Fatal(err)
	}
	if err := saveMoveOutcomeWith(connection, &intent, &message, true); err != nil {
		t.Fatal(err)
	}
	connection.Close()
	restored, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	stored := restored.Sessions.Index(record.ID)
	if stored.State != core.SessionStateError {
		t.Fatalf("state = %s", stored.State)
	}
	if stored.LastError == nil || *stored.LastError != "public message" {
		t.Fatalf("last error = %v", stored.LastError)
	}
	if stored.DraftInput != "a concurrent draft" {
		t.Fatalf("draft = %q", stored.DraftInput)
	}
	reader, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	if loaded := moveTestLoad(t, reader, record.ID); loaded == nil || !reflect.DeepEqual(*loaded, intent) {
		t.Fatalf("stored intent = %+v", loaded)
	}
}

func TestCommittedMovesFollowPhaseChangesAndCascadingDeletion(t *testing.T) {
	path := filepath.Join(t.TempDir(), "controller.sqlite")
	record := session("moving", "project")
	moveTestSaveSession(t, path, &record)
	intent := moveTestOperation(&record)
	moveTestSave(t, moveTestOpen(t, path), &intent)
	writer := committedTestWriter(t, path)
	held := committedSnapshot(t, writer.writer)
	if !reflect.DeepEqual(*held.Moves.Index("moving"), intent) {
		t.Fatalf("bootstrapped move = %+v", *held.Moves.Index("moving"))
	}
	failed := intent
	failed.Phase = core.MovePhaseFailed
	if err := committedExecute(writer.writer, "fail move", func(connection *Connection) error {
		return saveMoveOperationWith(connection, &failed)
	}); err != nil {
		t.Fatal(err)
	}
	if phase := committedSnapshot(t, writer.writer).Moves.Index("moving").Phase; phase != core.MovePhaseFailed {
		t.Fatalf("published phase = %s", phase)
	}
	if phase := held.Moves.Index("moving").Phase; phase != core.MovePhaseCapturing {
		t.Fatalf("held phase = %s", phase)
	}
	if err := committedExecute(writer.writer, "delete moving session", func(connection *Connection) error {
		_, err := connection.Exec("DELETE FROM sessions WHERE session_id='moving'")
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if !committedSnapshot(t, writer.writer).Moves.IsEmpty() {
		t.Fatal("a cascading delete must withdraw the move")
	}
}

func TestMoveBoundariesSurviveDatabaseReopenAndRetainTheSourceLocator(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	record := session("move-reopen", "project")
	template, err := core.UnmarshalTargetTemplateJSON([]byte(`{"kind":"ssh-podman","host":"original.test","image":"test","user":"builder"}`))
	if err != nil {
		t.Fatal(err)
	}
	runtimeSettings := core.TargetRuntimeSettingsFromTemplate(template)
	record.TargetRuntime = &runtimeSettings
	record.Target = core.TargetLocatorSshPodman{
		Host:             "original.test",
		ContainerID:      "source-container",
		WorkspaceStorage: core.PodmanWorkspaceLocatorContainerLayer{},
	}
	moveTestSaveSession(t, path, &record)
	intent := moveTestOperation(&record)
	for _, phase := range []core.MovePhase{
		core.MovePhaseCapturing,
		core.MovePhaseTransferring,
		core.MovePhaseRestoring,
		core.MovePhaseDestinationOwns,
		core.MovePhaseFailed,
		core.MovePhaseCompleted,
	} {
		intent.Phase = phase
		if phase == core.MovePhaseDestinationOwns {
			intent.QueueAdmissionStarted = true
			intent.DestinationTarget = record.Target
			store := "durable-destination"
			intent.DestinationStoreID = &store
		}
		if phase == core.MovePhaseCompleted {
			intent.QueueAdmissionFinished = true
		}
		connection, err := open(path)
		if err != nil {
			t.Fatal(err)
		}
		moveTestSave(t, connection, &intent)
		connection.Close()
		reopened, err := openReader(path)
		if err != nil {
			t.Fatal(err)
		}
		restored := moveTestLoad(t, reopened, record.ID)
		reopened.Close()
		if restored == nil || !reflect.DeepEqual(*restored, intent) {
			t.Fatalf("%s: restored = %+v", phase, restored)
		}
		if !reflect.DeepEqual(restored.SourceTarget, record.Target) {
			t.Fatalf("%s: source target = %+v", phase, restored.SourceTarget)
		}
		if restored.RetainsCheckpoint() != (phase != core.MovePhaseCompleted) {
			t.Fatalf("%s: retains checkpoint = %v", phase, restored.RetainsCheckpoint())
		}
	}
}

// moveTestRemoveHarness rewrites a stored recovery snapshot to name a harness
// the current binary no longer knows.
func moveTestRemoveHarness(t *testing.T, connection *Connection) {
	t.Helper()
	result, err := connection.Exec(`UPDATE session_moves
                 SET operation_json = replace(operation_json, '"harness_kind":"codex"', '"harness_kind":"zcode"')
                 WHERE session_id = 'move-removed-harness'`)
	if err != nil {
		t.Fatal(err)
	}
	if rewritten, _ := result.RowsAffected(); rewritten != 1 {
		t.Fatalf("the test session must store a codex harness to rewrite: %d", rewritten)
	}
}

// Hard-won: 04d4b7f0: a completed move naming a removed harness stopped daemon startup.
func TestBulkLoadSkipsAMoveIntentWhoseHarnessNoLongerDecodes(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	good := session("move-good", "project")
	staleSession := session("move-removed-harness", "project")
	moveTestSaveSession(t, path, &good)
	moveTestSaveSession(t, path, &staleSession)
	connection := moveTestOpen(t, path)
	goodOperation := moveTestOperation(&good)
	moveTestSave(t, connection, &goodOperation)
	staleOperation := moveTestOperation(&staleSession)
	staleOperation.OperationID = "move-two"
	moveTestSave(t, connection, &staleOperation)
	// Simulate a row recorded before a harness was removed: rewrite the
	// stored recovery snapshot to name a harness the current binary no
	// longer knows, so the row fails to decode.
	moveTestRemoveHarness(t, connection)
	loaded, err := loadMoveOperationsWith(connection)
	if err != nil {
		t.Fatal(err)
	}
	if len(loaded) != 1 {
		t.Fatalf("the undecodable row must be skipped, not fail the load: %d", len(loaded))
	}
	if loaded[0].Selection.SessionID != good.ID {
		t.Fatalf("loaded %s", loaded[0].Selection.SessionID)
	}
}

func TestInPlaceIntentRoundTripsAndALegacyRowWithoutItReadsAsAFreshEnvironment(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	record := session("move-in-place", "project")
	moveTestSaveSession(t, path, &record)
	connection := moveTestOpen(t, path)
	intent := moveTestOperation(&record)
	intent.InPlace = true
	reason := "Weekly quota exhausted"
	continuation := core.QuotaMoveContinuationResumeGoal
	intent.QuotaStopReason = &reason
	intent.QuotaContinuation = &continuation
	moveTestSave(t, connection, &intent)
	var stored string
	if err := connection.QueryRow("SELECT operation_json FROM session_moves WHERE session_id=?1", record.ID).Scan(&stored); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(stored, `"in_place":true`) {
		t.Fatalf("the in-place choice must be durable: %s", stored)
	}
	if restored := moveTestLoad(t, connection, record.ID); restored == nil || !reflect.DeepEqual(*restored, intent) {
		t.Fatalf("restored = %+v", restored)
	}
	legacyValue, err := jsonvalue.Parse([]byte(stored))
	if err != nil {
		t.Fatal(err)
	}
	legacyObject := legacyValue.(jsonvalue.Object)
	legacyObject.Delete("quota_stop_reason")
	legacyObject.Delete("quota_continuation")
	legacyText, err := jsonvalue.Marshal(legacyObject)
	if err != nil {
		t.Fatal(err)
	}
	var legacyIntent core.MoveOperation
	if err := json.Unmarshal(legacyText, &legacyIntent); err != nil {
		t.Fatal(err)
	}
	if legacyIntent.QuotaStopReason != nil || legacyIntent.QuotaContinuation != nil {
		t.Fatal("a legacy intent has no quota continuation")
	}
	reencoded, err := json.Marshal(&legacyIntent)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(reencoded), "quota_stop_reason") || strings.Contains(string(reencoded), "quota_continuation") {
		t.Fatalf("a legacy intent writes no quota fields: %s", reencoded)
	}
	// Unknown fields from a newer daemon are skipped by every store reader.
	newerValue, err := jsonvalue.Parse([]byte(stored))
	if err != nil {
		t.Fatal(err)
	}
	newer := newerValue.(jsonvalue.Object)
	newer.Set("future_quota_field", "new intent")
	newerText, err := jsonvalue.Marshal(newer)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := connection.Exec("UPDATE session_moves SET operation_json=?2 WHERE session_id=?1", record.ID, string(newerText)); err != nil {
		t.Fatal(err)
	}
	if moveTestLoad(t, connection, record.ID) != nil {
		t.Fatal("an intent with unknown fields must read as absent")
	}
	if all, err := loadMoveOperationsWith(connection); err != nil || len(all) != 0 {
		t.Fatalf("bulk load = %d, %v", len(all), err)
	}
	moveTestSave(t, connection, &intent)
	// A row written before this field existed must still decode, as the
	// full fresh-environment move it was.
	result, err := connection.Exec(`UPDATE session_moves
                 SET operation_json = replace(operation_json, '"in_place":true,', '')
                 WHERE session_id = ?1`, record.ID)
	if err != nil {
		t.Fatal(err)
	}
	if rewritten, _ := result.RowsAffected(); rewritten != 1 {
		t.Fatalf("rewritten = %d", rewritten)
	}
	legacy := moveTestLoad(t, connection, record.ID)
	if legacy == nil {
		t.Fatal("a row without in_place must still decode")
	}
	if legacy.InPlace {
		t.Fatal("a row without in_place is a fresh-environment move")
	}
}

// Hard-won: 61db26ba: close failed on a completed move row after its harness was removed.
func TestPerSessionLoadTreatsAnIntentWhoseHarnessNoLongerDecodesAsAbsent(t *testing.T) {
	// The stop, checkpoint, recovery, and new-move paths each read the one
	// intent for their session. A completed move whose recovery snapshot
	// names a removed harness must not fail those operations.
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	staleSession := session("move-removed-harness", "project")
	moveTestSaveSession(t, path, &staleSession)
	connection := moveTestOpen(t, path)
	staleOperation := moveTestOperation(&staleSession)
	staleOperation.Phase = core.MovePhaseCompleted
	moveTestSave(t, connection, &staleOperation)
	moveTestRemoveHarness(t, connection)
	loaded, err := loadMoveOperationWith(connection, staleSession.ID)
	if err != nil {
		t.Fatalf("an undecodable intent must not fail the read: %v", err)
	}
	if loaded != nil {
		t.Fatal("the undecodable intent is treated as absent")
	}
}

// Hard-won: c5f1cbad: completed removed-harness move rows wedged daemon startup.
func TestReapingDeletesAFinishedMoveOnlyOnceItsCheckpointArchiveIsGone(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj.sqlite3")
	present := filepath.Join(directory, "present.hel.zip")
	if err := os.WriteFile(present, []byte("archive"), 0o644); err != nil {
		t.Fatal(err)
	}
	gone := filepath.Join(directory, "gone.hel.zip")
	// session id, phase, whether its archive is still on disk, whether its
	// queue admission is half done, and whether the row must survive.
	cases := []struct {
		sessionID      string
		phase          core.MovePhase
		archivePresent bool
		midAdmission   bool
		survives       bool
	}{
		{"completed-retained", core.MovePhaseCompleted, true, false, true},
		{"completed-gone", core.MovePhaseCompleted, false, false, false},
		{"completed-admitting", core.MovePhaseCompleted, false, true, true},
		{"cancelled-retained", core.MovePhaseCancelled, true, false, true},
		{"cancelled-gone", core.MovePhaseCancelled, false, false, false},
		{"failed-gone", core.MovePhaseFailed, false, false, true},
		{"running-gone", core.MovePhaseRestoring, false, false, true},
	}
	for _, c := range cases {
		record := session(c.sessionID, "project")
		moveTestSaveSession(t, path, &record)
		connection, err := open(path)
		if err != nil {
			t.Fatal(err)
		}
		intent := moveTestOperation(&record)
		intent.OperationID = fmt.Sprintf("%s-operation", c.sessionID)
		intent.Phase = c.phase
		intent.QueueAdmissionStarted = c.midAdmission
		archive := gone
		if c.archivePresent {
			archive = present
		}
		intent.Checkpoint = &core.CheckpointMetadata{
			ArchivePath:   archive,
			Sha256:        strings.Repeat("b", 64),
			CreatedAt:     record.CreatedAt,
			EventFrontier: 6,
		}
		moveTestSave(t, connection, &intent)
		connection.Close()
	}
	connection := moveTestOpen(t, path)
	reaped, err := reapFinishedMoveIntentsWith(connection)
	if err != nil {
		t.Fatal(err)
	}
	expected := 0
	for _, c := range cases {
		if !c.survives {
			expected++
		}
	}
	if reaped != expected {
		t.Fatalf("only the finished moves whose archive is gone are reaped: %d, want %d", reaped, expected)
	}
	for _, c := range cases {
		if survived := moveTestLoad(t, connection, c.sessionID) != nil; survived != c.survives {
			t.Fatalf("%s row survival = %v", c.sessionID, survived)
		}
	}
	if again, err := reapFinishedMoveIntentsWith(connection); err != nil || again != 0 {
		t.Fatalf("a second sweep finds nothing left to reap: %d, %v", again, err)
	}
}

func TestConcurrentPhaseSaveCannotEraseDurableCancellation(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	record := session("move-cancel", "project")
	moveTestSaveSession(t, path, &record)
	connection := moveTestOpen(t, path)
	intent := moveTestOperation(&record)
	moveTestSave(t, connection, &intent)
	cancelled := intent
	cancelled.CancellationRequested = true
	moveTestSave(t, connection, &cancelled)
	intent.Phase = core.MovePhaseTransferring
	moveTestSave(t, connection, &intent)
	restored := moveTestLoad(t, connection, record.ID)
	if restored == nil || !restored.CancellationRequested || restored.Phase != core.MovePhaseTransferring {
		t.Fatalf("restored = %+v", restored)
	}
	intent.OperationID = "explicit-new-operation"
	moveTestSave(t, connection, &intent)
	if restored := moveTestLoad(t, connection, record.ID); restored == nil || restored.CancellationRequested {
		t.Fatalf("a new operation starts uncancelled: %+v", restored)
	}
}

func TestCancellingPartialQueueAdmissionDoesNotReleaseItsArchive(t *testing.T) {
	record := session("move-queue", "project")
	intent := moveTestOperation(&record)
	intent.Phase = core.MovePhaseCancelled
	intent.QueueAdmissionStarted = true
	if !intent.RetainsCheckpoint() {
		t.Fatal("a half-admitted queue keeps its archive")
	}
	intent.QueueAdmissionFinished = true
	if intent.RetainsCheckpoint() {
		t.Fatal("a finished admission releases its archive")
	}
}

func TestReconnectRepairPreservesAdmissionForActiveInPlaceMovesOnly(t *testing.T) {
	record := session("move-admission-repair", "project")
	intent := moveTestOperation(&record)
	intent.InPlace = true
	for _, phase := range []core.MovePhase{
		core.MovePhaseCapturing,
		core.MovePhaseTransferring,
		core.MovePhaseRestoring,
		core.MovePhaseDestinationOwns,
	} {
		intent.Phase = phase
		if !moveHoldsSubagentGate(&intent) {
			t.Fatalf("%s", phase)
		}
	}
	for _, phase := range []core.MovePhase{
		core.MovePhaseCompleted,
		core.MovePhaseFailed,
		core.MovePhaseCancelled,
	} {
		intent.Phase = phase
		if moveHoldsSubagentGate(&intent) {
			t.Fatalf("%s", phase)
		}
	}
	intent.InPlace = false
	intent.Phase = core.MovePhaseTransferring
	if moveHoldsSubagentGate(&intent) {
		t.Fatal("a fresh-environment move does not hold the gate")
	}
}

func TestDestinationRecordInstallKeepsDraftsAndTitlesEditedDuringMove(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	stale := session("move-draft", "project")
	moveTestSaveSession(t, path, &stale)
	connection := moveTestOpen(t, path)
	operation := moveTestOperation(&stale)
	moveTestSave(t, connection, &operation)
	if _, err := connection.Exec("UPDATE sessions SET draft_input='keep this draft', session_title_override='new title' WHERE session_id=?1", stale.ID); err != nil {
		t.Fatal(err)
	}
	stale.LastProfile = "destination"
	stale.State = core.SessionStateProvisioning
	moveTestSaveSession(t, path, &stale)
	state, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	current, ok := state.Sessions.Remove(stale.ID)
	if !ok {
		t.Fatal("the session is missing")
	}
	if current.DraftInput != "keep this draft" {
		t.Fatalf("draft = %q", current.DraftInput)
	}
	if current.SessionTitleOverride == nil || *current.SessionTitleOverride != "new title" {
		t.Fatalf("title override = %v", current.SessionTitleOverride)
	}
	if current.LastProfile != "destination" {
		t.Fatalf("profile = %q", current.LastProfile)
	}

	// A no-op Move's omitted choices cannot overwrite a settings edit.
	allocation := core.SessionResourceAllocationContainer{Cpus: 8, MemoryBytes: 16 << 30}
	if err := setSessionContainerSettingsTo(path, stale.ID, allocation, current.AdditionalMounts, current.UpdatedAt); err != nil {
		t.Fatal(err)
	}
	selection := moveTestOperation(&stale).Selection
	selection.AdditionalMounts = nil
	writer := moveTestOpen(t, path)
	applyRequests := func() {
		t.Helper()
		tx, err := writer.Begin()
		if err != nil {
			t.Fatal(err)
		}
		defer tx.Rollback()
		if err := updateMoveRequests(tx, &selection); err != nil {
			t.Fatal(err)
		}
		if err := tx.Commit(); err != nil {
			t.Fatal(err)
		}
	}
	applyRequests()
	retained, err := loadSessionWith(writer, stale.ID)
	if err != nil || retained == nil {
		t.Fatalf("load retained: %v, %v", retained, err)
	}
	if !reflect.DeepEqual(retained.ResourceAllocation, core.SessionResourceAllocation(allocation)) {
		t.Fatalf("allocation = %+v", retained.ResourceAllocation)
	}
	if !reflect.DeepEqual(retained.AdditionalMounts, current.AdditionalMounts) {
		t.Fatalf("mounts = %+v", retained.AdditionalMounts)
	}

	// An explicit choice still applies when no environment change is
	// needed.
	selection.ClearResourceAllocation = true
	selection.AdditionalMounts = &[]core.AdditionalMount{}
	applyRequests()
	changed, err := loadSessionWith(writer, stale.ID)
	if err != nil || changed == nil {
		t.Fatalf("load changed: %v, %v", changed, err)
	}
	if changed.ResourceAllocation != nil {
		t.Fatalf("allocation = %+v", changed.ResourceAllocation)
	}
	if len(changed.AdditionalMounts) != 0 {
		t.Fatalf("mounts = %+v", changed.AdditionalMounts)
	}
}
