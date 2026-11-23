// Port of mj-store/src/database/tests.rs, continued (projection events,
// read receipts and projection pages).

package store

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func dtAgentText(t testing.TB, text string) json.RawMessage {
	t.Helper()
	return dtValue(t, map[string]any{"content": map[string]any{"type": "text", "text": text}})
}

func TestProjectionEventApplicationIsAtomicOrderedAndIdempotent(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	firstItem := core.TranscriptItem{
		StableID:                  "agent:1",
		Position:                  1,
		LatestContentEventOrdinal: dtPtr(uint64(1)),
		CreatedAtMs:               100,
		LastChangedAtMs:           100,
		Body: core.TranscriptBodyAgent{
			Chunks:    []json.RawMessage{dtAgentText(t, "hel")},
			Streaming: true,
		},
	}
	running := core.MaterializedExecutionState(core.MaterializedExecutionStateRunning{StartedAtMs: 90})
	title := dtPtr("Testing")
	first := core.MaterializedSessionMutation{
		LastActivityAtMs: dtPtr(int64(105)),
		Execution:        &running,
		SessionTitle:     &title,
		Configuration: &core.SessionConfiguration{
			Values: map[string]any{"model": "sol"},
		},
		Transcript: []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: firstItem}},
		QueuedPrompts: &[]core.MaterializedQueuedPrompt{{
			CommandID:  "prompt-2",
			Kind:       core.QueuedCommandKindPrompt{},
			Content:    []any{map[string]any{"type": "text", "text": "next"}},
			QueuedAtMs: 105,
		}},
	}
	firstDigest := EventDigest(1)
	secondDigest := EventDigest(2)
	thirdDigest := EventDigest(3)
	dtEqual(t, dtMust(applyProjectionEventTo(database, "session-1", 1, core.RelayEventGenesisDigest, firstDigest, &first)),
		core.ProjectionApplyOutcomeApplied)

	destructiveDuplicate := core.MaterializedSessionMutation{
		Transcript: []core.TranscriptMutation{core.TranscriptMutationRemove{StableID: firstItem.StableID}},
	}
	dtEqual(t, dtMust(applyProjectionEventTo(database, "session-1", 1, core.RelayEventGenesisDigest, firstDigest, &destructiveDuplicate)),
		core.ProjectionApplyOutcomeAlreadyApplied)
	dtProjectionRefused(t, "digest mismatch", func() error {
		_, err := applyProjectionEventTo(database, "session-1", 1, core.RelayEventGenesisDigest, EventDigest(99), &core.MaterializedSessionMutation{})
		return err
	})
	dtProjectionRefused(t, "expected ordinal 2", func() error {
		_, err := applyProjectionEventTo(database, "session-1", 3, firstDigest, thirdDigest, &core.MaterializedSessionMutation{})
		return err
	})
	dtProjectionRefused(t, "chain diverged", func() error {
		_, err := applyProjectionEventTo(database, "session-1", 2, EventDigest(99), secondDigest, &core.MaterializedSessionMutation{})
		return err
	})

	updatedItem := firstItem
	updatedItem.LatestContentEventOrdinal = dtPtr(uint64(2))
	updatedItem.LastChangedAtMs = 120
	updatedItem.Body = core.TranscriptBodyAgent{
		Chunks:    []json.RawMessage{dtAgentText(t, "hello")},
		Streaming: false,
	}
	dtMust(applyProjectionEventTo(database, "session-1", 2, firstDigest, secondDigest, &core.MaterializedSessionMutation{
		LastActivityAtMs: dtPtr(int64(120)),
		Transcript:       []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: updatedItem}},
	}))

	regressedContentOrdinal := updatedItem
	regressedContentOrdinal.LatestContentEventOrdinal = dtPtr(uint64(1))
	regressedContentOrdinal.LastChangedAtMs = 130
	dtProjectionRefused(t, "latest content ordinal backwards", func() error {
		_, err := applyProjectionEventTo(database, "session-1", 3, secondDigest, thirdDigest, &core.MaterializedSessionMutation{
			LastActivityAtMs: dtPtr(int64(130)),
			Transcript:       []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: regressedContentOrdinal}},
		})
		return err
	})

	invalidIdentity := updatedItem
	invalidIdentity.Position = 2
	dtProjectionRefused(t, "immutable identity", func() error {
		_, err := applyProjectionEventTo(database, "session-1", 3, secondDigest, thirdDigest, &core.MaterializedSessionMutation{
			LastActivityAtMs: dtPtr(int64(130)),
			Transcript:       []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: invalidIdentity}},
		})
		return err
	})
	loaded := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, loaded.AppliedEventOrdinal, uint64(2))
	dtEqual(t, loaded.AppliedEventDigest, secondDigest)
	dtEqual(t, loaded.GetLastActivityAtMs(), dtPtr(int64(120)))
	dtEqual(t, len(loaded.Transcript), 1)
	dtEqual(t, loaded.Transcript[0].LatestContentEventOrdinal, dtPtr(uint64(2)))
	dtEqual(t, loaded.Transcript[0].Body, core.TranscriptBody(core.TranscriptBodyAgent{
		Chunks:    []json.RawMessage{dtAgentText(t, "hello")},
		Streaming: false,
	}))
}

// dtProjectionRefused asserts that apply fails with an error naming want.
func dtProjectionRefused(t testing.TB, want string, apply func() error) {
	t.Helper()
	err := dtErr(t, apply())
	dtTrue(t, strings.Contains(err.Error(), want), err)
}

func TestDetachReceiptIsMonotonicAndCannotPassProjection(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	operational := session("session-1", "project-1")
	operational.ViewedThroughEventOrdinal = 0
	dtOK(t, SaveSessionTo(database, &operational))
	previousDigest := core.RelayEventGenesisDigest
	for ordinal := uint64(1); ordinal <= 2; ordinal++ {
		digest := EventDigest(ordinal)
		dtMust(applyProjectionEventTo(database, "session-1", ordinal, previousDigest, digest, &core.MaterializedSessionMutation{}))
		previousDigest = digest
	}

	dtEqual(t, dtMust(advanceViewedThroughEventOrdinalTo(database, "session-1", 2)), uint64(2))
	dtEqual(t, dtMust(advanceViewedThroughEventOrdinalTo(database, "session-1", 1)), uint64(2))
	dtProjectionRefused(t, "projection is at 2", func() error {
		_, err := advanceViewedThroughEventOrdinalTo(database, "session-1", 3)
		return err
	})
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").ViewedThroughEventOrdinal, uint64(2))

	connection := dtOpen(t, database)
	dtEqual(t, dtMust(persistReadReceiptWith(connection, "client-a", core.DefaultWorkspaceID, "session-1", 2)), uint64(2))
	dtEqual(t, dtMust(persistReadReceiptWith(connection, "notification-dismissal", core.DefaultWorkspaceID, "session-1", 1)), uint64(1))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").ViewedThroughEventOrdinal, uint64(2))
	dtProjectionRefused(t, "projection is at 2", func() error {
		_, err := persistReadReceiptWith(connection, "client-a", core.DefaultWorkspaceID, "session-1", 3)
		return err
	})
	dtEqual(t, dtMust(clientReadFrontierAt(database, "client-a", core.DefaultWorkspaceID, "session-1")), uint64(2))
}

func TestReadReceiptsSurviveReopenWithANewTerminalIdentity(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	record.ViewedThroughEventOrdinal = 0
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	through := materialized.AppliedEventOrdinal
	{
		connection := dtMust(open(database))
		dtMust(persistReadReceiptWith(connection, "tui-old", core.DefaultWorkspaceID, "session-1", through))
		dtOK(t, connection.Close())
	}
	// A lifecycle writer holding an older copy cannot erase the receipt.
	dtOK(t, SaveSessionTo(database, &record))
	dtEqual(t, dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").ViewedThroughEventOrdinal, through)
	dtEqual(t, dtMust(clientReadFrontierAt(database, "tui-new", core.DefaultWorkspaceID, "session-1")), through)
	restored := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, restored.UnreadAgentMessagesAfter(through), uint64(0))
}

func TestInterruptionSummaryMatchesFullProjectionAndLegacyOutcomes(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	materialized.LastTurnOutcome = &core.MaterializedTurnOutcome{
		CommandID:         "prompt",
		AcceptedOrdinal:   dtPtr(uint64(1)),
		TurnStartPosition: dtPtr(uint64(1)),
		CompletedOrdinal:  3,
		CompletedAtMs:     1_500,
		Outcome:           core.TurnOutcomeKindInterrupted{Message: "worker restarted"},
	}
	for _, withMarker := range []bool{false, true} {
		if withMarker {
			materialized.Transcript = append(materialized.Transcript, &core.TranscriptItem{
				StableID:        core.WorkInterruptedItemPrefix + "3",
				Position:        3,
				CreatedAtMs:     1_500,
				LastChangedAtMs: 1_500,
				Body:            core.TranscriptBodySystem{Text: "Work interrupted"},
			})
		}
		dtOK(t, SaveMaterializedSessionTo(database, &materialized))
		summary := dtMust(loadMaterializedSessionSummaryFrom(database, "session-1"))
		restored := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
		dtEqual(t, summary.InterruptionEventOrdinals, []uint64{3})
		dtEqual(t, summary.InterruptionEventOrdinals, restored.InterruptionEventOrdinals())
		dtEqual(t, restored.UnreadInterruptionsAfter(3), uint64(0))
	}
}

func TestSessionDraftInputRoundTripsAndAnEmptyDraftClearsIt(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	draft := func() string {
		return dtSessionOf(dtMust(LoadStateFrom(database)), "session-1").DraftInput
	}
	dtEqual(t, draft(), "")

	dtOK(t, setSessionDraftInputAt(database, "session-1", "half typed thought"))
	dtEqual(t, draft(), "half typed thought")

	// An ordinary session save must not roll the draft back.
	dtOK(t, SaveSessionTo(database, &record))
	dtEqual(t, draft(), "half typed thought")

	dtOK(t, setSessionDraftInputAt(database, "session-1", ""))
	dtEqual(t, draft(), "")

	err := dtErr(t, setSessionDraftInputAt(database, "missing", "text"))
	dtTrue(t, strings.Contains(err.Error(), "unknown session missing"), err)
}

func TestProjectionActivityWatermarkIsAtomicAndMonotonic(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	firstDigest := EventDigest(1)
	dtMust(applyProjectionEventTo(database, "session-1", 1, core.RelayEventGenesisDigest, firstDigest, &core.MaterializedSessionMutation{
		LastActivityAtMs: dtPtr(int64(500)),
		QueuedPrompts: &[]core.MaterializedQueuedPrompt{{
			CommandID:  "queued-1",
			Kind:       core.QueuedCommandKindPrompt{},
			Content:    []any{map[string]any{"type": "text", "text": "later"}},
			QueuedAtMs: 500,
		}},
	}))
	dtMust(applyProjectionEventTo(database, "session-1", 2, firstDigest, EventDigest(2), &core.MaterializedSessionMutation{
		LastActivityAtMs: dtPtr(int64(400)),
		QueuedPrompts:    &[]core.MaterializedQueuedPrompt{},
	}))

	loaded := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, len(loaded.QueuedPrompts), 0)
	dtEqual(t, loaded.GetLastActivityAtMs(), dtPtr(int64(500)))
	dtEqual(t, loaded.AppliedEventOrdinal, uint64(2))
}

// agentMessageMutation is one agent message per relay event, shaped so the
// projection can store it.
func agentMessageMutation(ordinal uint64) core.MaterializedSessionMutation {
	at := 1_000 + int64(ordinal)
	chunk := dtMust(json.Marshal(map[string]any{
		"content": map[string]any{"type": "text", "text": fmt.Sprintf("event %d", ordinal)},
	}))
	return core.MaterializedSessionMutation{
		LastActivityAtMs: &at,
		Transcript: []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: core.TranscriptItem{
			StableID:                  fmt.Sprintf("item-%d", ordinal),
			Position:                  ordinal,
			LatestContentEventOrdinal: dtPtr(ordinal),
			CreatedAtMs:               at,
			LastChangedAtMs:           at,
			Body: core.TranscriptBodyAgent{
				Chunks:    []json.RawMessage{chunk},
				Streaming: false,
			},
		}}},
	}
}

// dtApplyPage is a projection page applying each step in turn, as Rust's
// `page.apply(..)?` chains do.
func dtApplyPage(path, sessionID string, steps ...func(page *ProjectionPage) error) error {
	_, err := applyProjectionPageTo(path, sessionID, func(page *ProjectionPage) (struct{}, error) {
		for _, step := range steps {
			if err := step(page); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, nil
	})
	return err
}

// dtPageApply is one page.apply step.
func dtPageApply(ordinal uint64, previous, digest string, mutation core.MaterializedSessionMutation) func(page *ProjectionPage) error {
	return func(page *ProjectionPage) error {
		_, err := page.Apply(ordinal, previous, digest, &mutation)
		return err
	}
}

func TestProjectionPageAdvancesTheFrontierOnlyWhenTheWholePageCommits(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	// The second event breaks the chain only after the first has written
	// its rows, so the page has to unwind work it already did.
	interrupted := dtErr(t, dtApplyPage(database, "session-1",
		dtPageApply(1, core.RelayEventGenesisDigest, EventDigest(1), agentMessageMutation(1)),
		dtPageApply(3, EventDigest(1), EventDigest(3), agentMessageMutation(3)),
	))
	dtTrue(t, strings.Contains(interrupted.Error(), "expected ordinal 2"), "unexpected page failure: ", interrupted)

	// The relay retains everything past the last acknowledgement, so an
	// interrupted page must leave the durable frontier where it was.
	rolledBack := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, rolledBack.AppliedEventOrdinal, uint64(0))
	dtEqual(t, rolledBack.AppliedEventDigest, core.RelayEventGenesisDigest)
	dtEqual(t, len(rolledBack.Transcript), 0)
	dtEqual(t, rolledBack.GetLastActivityAtMs(), (*int64)(nil))

	dtOK(t, dtApplyPage(database, "session-1",
		dtPageApply(1, core.RelayEventGenesisDigest, EventDigest(1), agentMessageMutation(1)),
		dtPageApply(2, EventDigest(1), EventDigest(2), agentMessageMutation(2)),
	))

	committed := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, committed.AppliedEventOrdinal, uint64(2))
	dtEqual(t, committed.AppliedEventDigest, EventDigest(2))
	var stableIDs []string
	for _, item := range committed.Transcript {
		stableIDs = append(stableIDs, item.StableID)
	}
	dtEqual(t, stableIDs, []string{"item-1", "item-2"})
	dtEqual(t, committed.GetLastActivityAtMs(), dtPtr(int64(1_002)))
}

func TestProjectionPageCoalescesRepeatedItemUpdatesToTheFinalValue(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	first := agentMessageMutation(1)
	second := agentMessageMutation(2)
	secondItem := second.Transcript[0].(core.TranscriptMutationUpsert)
	secondItem.Item.StableID = "item-1"
	secondItem.Item.Position = 1
	secondItem.Item.CreatedAtMs = 1_001
	second.Transcript[0] = secondItem
	dtOK(t, dtApplyPage(database, "session-1",
		dtPageApply(1, core.RelayEventGenesisDigest, EventDigest(1), first),
		dtPageApply(2, EventDigest(1), EventDigest(2), second),
	))

	committed := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, committed.AppliedEventOrdinal, uint64(2))
	dtEqual(t, len(committed.Transcript), 1)
	dtEqual(t, committed.Transcript[0].StableID, "item-1")
	dtEqual(t, committed.Transcript[0].LatestContentEventOrdinal, dtPtr(uint64(2)))
	agent, ok := committed.Transcript[0].Body.(core.TranscriptBodyAgent)
	if !ok {
		t.Fatal("coalesced item stayed an agent message")
	}
	var chunk struct {
		Content struct {
			Text string `json:"text"`
		} `json:"content"`
	}
	dtOK(t, json.Unmarshal(agent.Chunks[0], &chunk))
	dtEqual(t, chunk.Content.Text, "event 2")
	dtEqual(t, committed.GetLastActivityAtMs(), dtPtr(int64(1_002)))
}

func TestProjectionPagePreservesRemoveThenReinsertIdentityBoundary(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	first := agentMessageMutation(1)
	dtMust(applyProjectionEventTo(database, "session-1", 1, core.RelayEventGenesisDigest, EventDigest(1), &first))

	removed := core.MaterializedSessionMutation{
		Transcript: []core.TranscriptMutation{core.TranscriptMutationRemove{StableID: "item-1"}},
	}
	reinserted := agentMessageMutation(3)
	reinsertedItem := reinserted.Transcript[0].(core.TranscriptMutationUpsert)
	reinsertedItem.Item.StableID = "item-1"
	reinserted.Transcript[0] = reinsertedItem
	dtOK(t, dtApplyPage(database, "session-1",
		dtPageApply(2, EventDigest(1), EventDigest(2), removed),
		dtPageApply(3, EventDigest(2), EventDigest(3), reinserted),
	))

	committed := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, committed.AppliedEventOrdinal, uint64(3))
	dtEqual(t, len(committed.Transcript), 1)
	dtEqual(t, committed.Transcript[0].StableID, "item-1")
	dtEqual(t, committed.Transcript[0].Position, uint64(3))
	dtEqual(t, committed.Transcript[0].CreatedAtMs, int64(1_003))
}

// The process caches which databases it has migrated. A database that is
// gone and recreated under the same path must still be migrated.
func TestReopeningARecreatedDatabaseMigratesItAgain(t *testing.T) {
	directory := t.TempDir()
	database := filepath.Join(directory, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	for _, suffix := range []string{"", "-wal", "-shm"} {
		sidecar := filepath.Join(directory, "hel.sqlite3"+suffix)
		if _, err := os.Stat(sidecar); err == nil {
			dtOK(t, os.Remove(sidecar))
		}
	}

	second := session("session-2", "project-1")
	dtOK(t, SaveSessionTo(database, &second))
	state := dtMust(LoadStateFrom(database))
	dtTrue(t, state.Sessions.ContainsKey("session-2"))
	dtTrue(t, !state.Sessions.ContainsKey("session-1"))
}

// Catch-up throughput: one durable commit per page instead of one per
// event. Ignored by default because it measures wall-clock time (Rust:
// #[ignore = "timing benchmark; run with --ignored --nocapture"]).
func TestProjectionPageApplyOutrunsPerEventApply(t *testing.T) {
	if os.Getenv(targets.IncludeIgnoredEnv) != "1" {
		t.Skip("timing benchmark; run with " + targets.IncludeIgnoredEnv + "=1")
	}
	const events = uint64(2_000)
	directory := t.TempDir()

	perEventDatabase := filepath.Join(directory, "per-event", "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(perEventDatabase, &record))
	started := time.Now()
	for ordinal := uint64(1); ordinal <= events; ordinal++ {
		mutation := agentMessageMutation(ordinal)
		dtMust(applyProjectionEventTo(perEventDatabase, "session-1", ordinal, EventDigest(ordinal-1), EventDigest(ordinal), &mutation))
	}
	perEvent := time.Since(started)

	perPageDatabase := filepath.Join(directory, "per-page", "hel.sqlite3")
	dtOK(t, SaveSessionTo(perPageDatabase, &record))
	started = time.Now()
	dtMust(applyProjectionPageTo(perPageDatabase, "session-1", func(page *ProjectionPage) (struct{}, error) {
		for ordinal := uint64(1); ordinal <= events; ordinal++ {
			mutation := agentMessageMutation(ordinal)
			if _, err := page.Apply(ordinal, EventDigest(ordinal-1), EventDigest(ordinal), &mutation); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, nil
	}))
	perPage := time.Since(started)

	fmt.Printf("%d events per-event: %v, one page: %v\n", events, perEvent, perPage)
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(perPageDatabase, "session-1")).AppliedEventOrdinal, events)
	dtTrue(t, perPage < perEvent, fmt.Sprintf("one page took %v against %v per event", perPage, perEvent))
}

func TestDeletingAnOperationalSessionRemovesItsPromptsAndKeepsItsContext(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	state := core.NewStateState()
	record := session("session-1", "project-1")
	state.Sessions.Insert(record.ID, record)
	dtOK(t, SaveStateTo(database, &state))
	connection := dtOpen(t, database)
	dtExec(t, connection, `INSERT INTO prompt_history(session_id, event_ordinal, submitted_at, text)
                 VALUES ('session-1', 8, '2026-08-12T02:00:00Z', 'remember this')`)

	dtOK(t, deleteSessionFrom(database, "session-1"))
	state.Sessions.Clear()
	dtOK(t, SaveStateTo(database, &state))

	count := func(query string) int64 { return dtQuery[int64](t, connection, query) }
	dtEqual(t, count("SELECT count(*) FROM prompt_history WHERE session_id = 'session-1'"), int64(0),
		"a destroyed session's prompts leave the history")
	dtEqual(t, count("SELECT count(*) FROM session_contexts WHERE session_id = 'session-1'"), int64(1))
}

func TestContextRejectsReassigningASessionToAnotherProject(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	state := core.NewStateState()
	state.Sessions.Insert("session-1", session("session-1", "project-1"))
	dtOK(t, SaveStateTo(database, &state))
	state.Sessions.GetMut("session-1").BundleID = "project-2"

	err := dtErr(t, SaveStateTo(database, &state))
	dtTrue(t, strings.Contains(err.Error(), "already associated"), err)
}
