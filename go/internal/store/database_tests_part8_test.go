// Port of mj-store/src/database/tests.rs, continued (turn summaries and
// transcript paging).

package store

import (
	"encoding/json"
	"fmt"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// dtEditUpsert edits the item of the upsert at index in mutation's
// transcript (Rust: `let TranscriptMutation::Upsert(item) = &mut ..`).
func dtEditUpsert(mutation *core.MaterializedSessionMutation, index int, edit func(item *core.TranscriptItem)) {
	upsert, ok := mutation.Transcript[index].(core.TranscriptMutationUpsert)
	if !ok {
		panic("internal error: entered unreachable code")
	}
	edit(&upsert.Item)
	mutation.Transcript[index] = upsert
}

// dtPreviousDigest is the digest an event at ordinal follows.
func dtPreviousDigest(ordinal uint64) string {
	if ordinal == 1 {
		return core.RelayEventGenesisDigest
	}
	return EventDigest(ordinal - 1)
}

// dtApplyEach applies one mutation per relay event, from ordinal 1.
func dtApplyEach(t testing.TB, database string, mutations []core.MaterializedSessionMutation) {
	t.Helper()
	for index := range mutations {
		ordinal := uint64(index) + 1
		dtMust(applyProjectionEventTo(database, "session-1", ordinal, dtPreviousDigest(ordinal), EventDigest(ordinal), &mutations[index]))
	}
}

func dtUpsertOnly(item core.TranscriptItem) core.MaterializedSessionMutation {
	return core.MaterializedSessionMutation{
		Transcript: []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: item}},
	}
}

func dtTextContent(t testing.TB, text string) []json.RawMessage {
	t.Helper()
	return []json.RawMessage{dtValue(t, map[string]any{"type": "text", "text": text})}
}

// The wait endpoint reports a turn number and a final message for the span
// one turn covers. A message the harness records after the turn ended — a
// resume notice is one — belongs to no turn and must not displace the answer.
// Hard-won: 529a5144: a later harness notice replaced a completed turn answer and duration
func TestATurnSummaryCountsTurnStartsAndReadsThatTurnSFinalMessage(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	user := func(position uint64, text string) core.TranscriptItem {
		return core.TranscriptItem{
			StableID:        fmt.Sprintf("user:%d", position),
			Position:        position,
			CreatedAtMs:     int64(position) * 100,
			LastChangedAtMs: int64(position) * 100,
			Body:            core.TranscriptBodyUser{Content: dtTextContent(t, text)},
		}
	}
	agent := func(position uint64, text string) core.TranscriptItem {
		return core.TranscriptItem{
			StableID:                  fmt.Sprintf("agent:%d", position),
			Position:                  position,
			LatestContentEventOrdinal: dtPtr(position),
			CreatedAtMs:               int64(position) * 100,
			LastChangedAtMs:           int64(position)*100 + 50,
			Body: core.TranscriptBodyAgent{
				Chunks:    []json.RawMessage{dtAgentText(t, text)},
				Streaming: false,
			},
		}
	}
	items := []core.TranscriptItem{
		user(1, "first"),
		agent(2, "first answer"),
		user(3, "second"),
		agent(4, "second answer"),
		// Recorded after the second turn completed, the way a harness reports
		// the model a resumed session opened on.
		agent(5, "Warning: this session was recorded with another model"),
	}
	var mutations []core.MaterializedSessionMutation
	for _, item := range items {
		mutations = append(mutations, dtUpsertOnly(item))
	}
	dtApplyEach(t, database, mutations)

	first := dtMust(loadMaterializedTurnSummaryFrom(database, "session-1", 1, 2))
	dtEqual(t, first.TurnNumber, uint64(1))
	dtEqual(t, first.TurnStartedAtMs, int64(100))
	dtEqual(t, first.LastChangedAtMs, int64(250))
	dtEqual(t, first.FinalMessage, dtPtr("first answer"), "a turn's summary stops where the turn ended")

	second := dtMust(loadMaterializedTurnSummaryFrom(database, "session-1", 3, 4))
	dtEqual(t, second.TurnNumber, uint64(2))
	dtEqual(t, second.TurnStartedAtMs, int64(300))
	dtEqual(t, second.LastChangedAtMs, int64(450))
	dtEqual(t, second.FinalMessage, dtPtr("second answer"),
		"a message recorded after the turn ended is not the turn's answer")
}

// A turn's summary counts the tool calls inside the turn's own span, so a
// wait can say how much work the turn did without loading its transcript.
func TestATurnSummaryCountsTheToolCallsTheTurnMade(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	item := func(position uint64, body core.TranscriptBody) core.TranscriptItem {
		return core.TranscriptItem{
			StableID:        fmt.Sprintf("item:%d", position),
			Position:        position,
			CreatedAtMs:     int64(position) * 100,
			LastChangedAtMs: int64(position) * 100,
			Body:            body,
		}
	}
	user := func(position uint64) core.TranscriptItem {
		user := item(position, core.TranscriptBodyUser{Content: dtTextContent(t, "go")})
		user.StableID = fmt.Sprintf("user:%d", position)
		return user
	}
	tool := func(position uint64) core.TranscriptItem {
		return item(position, core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(dtValue(t, map[string]any{
				"toolCallId": fmt.Sprintf("call-%d", position),
				"title":      "Read",
			})),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{},
		})
	}
	agent := func(position uint64) core.TranscriptItem {
		agent := item(position, core.TranscriptBodyAgent{
			Chunks:    []json.RawMessage{dtAgentText(t, "done")},
			Streaming: false,
		})
		agent.LatestContentEventOrdinal = dtPtr(position)
		return agent
	}
	items := []core.TranscriptItem{
		user(1),
		tool(2),
		tool(3),
		agent(4),
		user(5),
		tool(6),
		agent(7),
		user(8),
		agent(9),
	}
	var mutations []core.MaterializedSessionMutation
	for _, entry := range items {
		mutations = append(mutations, dtUpsertOnly(entry))
	}
	dtApplyEach(t, database, mutations)
	calls := func(start, end uint64) uint64 {
		return dtMust(loadMaterializedTurnSummaryFrom(database, "session-1", start, end)).ToolCalls
	}
	dtEqual(t, calls(1, 4), uint64(2))
	dtEqual(t, calls(5, 7), uint64(1), "the first turn's calls are not counted again")
	dtEqual(t, calls(8, 9), uint64(0))
}

// A finished child session reports the answer of the turn it ran, so a
// harness notice recorded after that turn — a resume warning, for instance —
// must not take its place. The session-wide newest agent message does take
// its place, which is why the report is read from the turn's own span.
// Hard-won: 529a5144: a later resume notice replaced a finished child report
func TestAFinishedTurnReportsItsOwnAnswerAndNotALaterHarnessNotice(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	agent := func(position uint64, text string) core.TranscriptItem {
		return core.TranscriptItem{
			StableID:                  fmt.Sprintf("agent:%d", position),
			Position:                  position,
			LatestContentEventOrdinal: dtPtr(position),
			CreatedAtMs:               int64(position) * 100,
			LastChangedAtMs:           int64(position) * 100,
			Body: core.TranscriptBodyAgent{
				Chunks:    []json.RawMessage{dtAgentText(t, text)},
				Streaming: false,
			},
		}
	}
	running := core.MaterializedExecutionState(core.MaterializedExecutionStateRunning{StartedAtMs: 100})
	activeTurn := &core.MaterializedTurn{
		CommandID:         "prompt-1",
		AcceptedOrdinal:   dtPtr(uint64(1)),
		TurnStartPosition: 1,
		StartedAtMs:       100,
	}
	idle := core.MaterializedExecutionState(core.MaterializedExecutionStateIdle{})
	var noTurn *core.MaterializedTurn
	mutations := []core.MaterializedSessionMutation{
		{
			Transcript: []core.TranscriptMutation{core.TranscriptMutationUpsert{Item: core.TranscriptItem{
				StableID:        "user:1",
				Position:        1,
				CreatedAtMs:     100,
				LastChangedAtMs: 100,
				Body:            core.TranscriptBodyUser{Content: dtTextContent(t, "do the work")},
			}}},
			Execution:  &running,
			ActiveTurn: &activeTurn,
		},
		dtUpsertOnly(agent(2, "the handoff report")),
		{
			Execution:  &idle,
			ActiveTurn: &noTurn,
			LastTurnOutcome: &core.MaterializedTurnOutcome{
				CommandID:         "prompt-1",
				AcceptedOrdinal:   dtPtr(uint64(1)),
				TurnStartPosition: dtPtr(uint64(1)),
				CompletedOrdinal:  3,
				CompletedAtMs:     300,
				Outcome:           core.TurnOutcomeKindCompleted{StopReason: "EndTurn"},
			},
		},
		dtUpsertOnly(agent(4, "Warning: this session was recorded with another model")),
	}
	dtApplyEach(t, database, mutations)

	message, ok, err := loadMaterializedFinishedTurnMessageFrom(database, "session-1")
	dtOK(t, err)
	dtTrue(t, ok)
	dtEqual(t, message, "the handoff report")
	summary := dtMust(loadMaterializedSessionSummaryFrom(database, "session-1"))
	dtEqual(t, summary.LastAgentMessage, dtPtr("Warning: this session was recorded with another model"),
		"the session-wide newest message is the notice, which is what made this worth bounding")
}

func dtFilteredPage(t testing.TB, path, sessionID string, afterSeq uint64, limit int, roles []core.TranscriptRole, finishedOnly bool) *core.TranscriptPage {
	t.Helper()
	page := dtMust(loadMaterializedTranscriptFilteredFrom(path, sessionID, afterSeq, limit, roles, finishedOnly))
	if page == nil {
		t.Fatal("called unwrap on a None value")
	}
	return page
}

func dtFirstChunkText(t testing.TB, body core.TranscriptBody, failure string) string {
	t.Helper()
	agent, ok := body.(core.TranscriptBodyAgent)
	if !ok {
		t.Fatal(failure)
	}
	var chunk struct {
		Content struct {
			Text string `json:"text"`
		} `json:"content"`
	}
	dtOK(t, json.Unmarshal(agent.Chunks[0], &chunk))
	return chunk.Content.Text
}

func TestTranscriptPagingBySequenceReturnsARewrittenAgentMessageOnce(t *testing.T) {
	// An agent message is rewritten while it streams. Paging by position would
	// hand a caller the message as first created and never the finished text;
	// paging by sequence sends it again exactly when it changed.
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))

	user := agentMessageMutation(2)
	dtEditUpsert(&user, 0, func(item *core.TranscriptItem) {
		item.StableID = "user-1"
		item.LatestContentEventOrdinal = nil
		item.Body = core.TranscriptBodyUser{Content: dtTextContent(t, "go")}
	})

	rewritten := agentMessageMutation(3)
	dtEditUpsert(&rewritten, 0, func(item *core.TranscriptItem) {
		item.StableID = "item-1"
		item.Position = 1
	})

	dtOK(t, dtApplyPage(database, "session-1",
		dtPageApply(1, core.RelayEventGenesisDigest, EventDigest(1), agentMessageMutation(1)),
		dtPageApply(2, EventDigest(1), EventDigest(2), user),
		dtPageApply(3, EventDigest(2), EventDigest(3), rewritten),
	))

	page := dtFilteredPage(t, database, "session-1", 0, 10, nil, false)
	type sequenced struct {
		stableID string
		seq      uint64
	}
	var found []sequenced
	for _, item := range page.Items {
		found = append(found, sequenced{item.StableID, item.Seq()})
	}
	dtEqual(t, found, []sequenced{{"user-1", 2}, {"item-1", 3}},
		"the rewritten agent message sorts after the user message it now follows")
	dtEqual(t, page.LatestSeq, uint64(3))

	resumed := dtFilteredPage(t, database, "session-1", 2, 10, nil, false)
	dtEqual(t, len(resumed.Items), 1)
	dtEqual(t, resumed.Items[0].StableID, "item-1")
	dtEqual(t, dtFirstChunkText(t, resumed.Items[0].Body, "the rewritten item stayed an agent message"), "event 3",
		"a caller resuming from the sequence it saw gets the finished text, once")

	unknown := dtMust(loadMaterializedTranscriptFilteredFrom(database, "unknown", 0, 10, nil, false))
	dtTrue(t, unknown == nil, "a session with no projection row has no transcript to page")
}

func TestFilteredTranscriptPagesIncludeTiesAndAdvanceAcrossGaps(t *testing.T) {
	path := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	dtMust(applyProjectionPageTo(path, "session-1", func(page *ProjectionPage) (struct{}, error) {
		for ordinal := uint64(1); ordinal <= 6; ordinal++ {
			mutation := agentMessageMutation(ordinal)
			if ordinal%2 == 1 {
				dtEditUpsert(&mutation, 0, func(item *core.TranscriptItem) {
					item.LatestContentEventOrdinal = nil
					item.Body = core.TranscriptBodySystem{Text: "gap"}
				})
			}
			if ordinal == 2 {
				tied := mutation.Transcript[0].(core.TranscriptMutationUpsert).Item
				tied.StableID = "tied-agent"
				mutation.Transcript = append(mutation.Transcript, core.TranscriptMutationUpsert{Item: tied})
			}
			if _, err := page.Apply(ordinal, dtPreviousDigest(ordinal), EventDigest(ordinal), &mutation); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, nil
	}))
	agentAndSystem := []core.TranscriptRole{core.TranscriptRoleAgent, core.TranscriptRoleSystem}
	mixedFirst := dtFilteredPage(t, path, "session-1", 0, 1, agentAndSystem, false)
	dtEqual(t, len(mixedFirst.Items), 1)
	dtEqual(t, mixedFirst.Items[0].Seq(), uint64(1))
	dtEqual(t, core.TranscriptItemRole(mixedFirst.Items[0].Body), "system")
	dtEqual(t, mixedFirst.NextAfterSeq, uint64(1))
	mixedNext := dtFilteredPage(t, path, "session-1", mixedFirst.NextAfterSeq, 1, agentAndSystem, false)
	dtEqual(t, len(mixedNext.Items), 2, "a page must not split a sequence tie")
	dtEqual(t, mixedNext.NextAfterSeq, uint64(2))

	agentOnly := []core.TranscriptRole{core.TranscriptRoleAgent}
	first := dtFilteredPage(t, path, "session-1", 0, 1, agentOnly, false)
	dtEqual(t, len(first.Items), 2, "a page must not split a sequence tie")
	dtEqual(t, first.NextAfterSeq, uint64(2))
	second := dtFilteredPage(t, path, "session-1", first.NextAfterSeq, 1, agentOnly, false)
	dtEqual(t, second.Items[0].Seq(), uint64(4))
	last := dtFilteredPage(t, path, "session-1", 4, 1, agentOnly, false)
	dtEqual(t, last.NextAfterSeq, uint64(6))
	empty := dtFilteredPage(t, path, "session-1", 0, 1, []core.TranscriptRole{core.TranscriptRoleTool}, false)
	dtEqual(t, len(empty.Items), 0)
	dtEqual(t, empty.NextAfterSeq, uint64(6))

	laterTool := agentMessageMutation(7)
	dtEditUpsert(&laterTool, 0, func(item *core.TranscriptItem) {
		item.StableID = "tool:call-7"
		item.LatestContentEventOrdinal = nil
		item.Body = core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(dtValue(t, map[string]any{
				"toolCallId": "call-7",
				"status":     "completed",
			})),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{},
		}
	})
	dtOK(t, dtApplyPage(path, "session-1", dtPageApply(7, EventDigest(6), EventDigest(7), laterTool)))
	resumed := dtFilteredPage(t, path, "session-1", empty.NextAfterSeq, 1, []core.TranscriptRole{core.TranscriptRoleTool}, false)
	dtEqual(t, len(resumed.Items), 1)
	dtEqual(t, resumed.Items[0].Seq(), uint64(7))
	dtEqual(t, resumed.NextAfterSeq, uint64(7))
}

func TestFinishedTranscriptCursorStopsBeforeOpenAgentAndReleasesItOnceClosed(t *testing.T) {
	path := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(path, &record))

	hiddenUser := agentMessageMutation(1)
	dtEditUpsert(&hiddenUser, 0, func(item *core.TranscriptItem) {
		item.StableID = "user-1"
		item.LatestContentEventOrdinal = nil
		item.Body = core.TranscriptBodyUser{Content: dtTextContent(t, "go")}
	})

	openAgent := agentMessageMutation(2)
	dtEditUpsert(&openAgent, 0, func(item *core.TranscriptItem) {
		item.StableID = "agent-1"
		agent, ok := item.Body.(core.TranscriptBodyAgent)
		if !ok {
			panic("internal error: entered unreachable code")
		}
		agent.Streaming = true
		item.Body = agent
	})

	dtOK(t, dtApplyPage(path, "session-1",
		dtPageApply(1, core.RelayEventGenesisDigest, EventDigest(1), hiddenUser),
		dtPageApply(2, EventDigest(1), EventDigest(2), openAgent),
	))

	blocked := dtFilteredPage(t, path, "session-1", 0, 10, nil, true)
	dtEqual(t, len(blocked.Items), 0)
	dtEqual(t, blocked.LatestSeq, uint64(2))
	dtEqual(t, blocked.NextAfterSeq, uint64(1),
		"the cursor crosses the hidden user row but stops before the open agent")

	closedAgent := agentMessageMutation(3)
	dtEditUpsert(&closedAgent, 0, func(item *core.TranscriptItem) {
		item.StableID = "agent-1"
		item.Position = 2
		item.CreatedAtMs = 1_002
	})
	dtOK(t, dtApplyPage(path, "session-1", dtPageApply(3, EventDigest(2), EventDigest(3), closedAgent)))

	released := dtFilteredPage(t, path, "session-1", blocked.NextAfterSeq, 10, nil, true)
	dtEqual(t, len(released.Items), 1)
	dtEqual(t, released.Items[0].StableID, "agent-1")
	dtEqual(t, released.Items[0].Seq(), uint64(3))
	dtEqual(t, released.NextAfterSeq, uint64(3))

	resumed := dtFilteredPage(t, path, "session-1", released.NextAfterSeq, 10, nil, true)
	dtEqual(t, len(resumed.Items), 0)
	dtEqual(t, resumed.NextAfterSeq, uint64(3))
}

func TestFinishedTranscriptCursorAdvancesAcrossNonAgentRows(t *testing.T) {
	path := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(path, &record))

	dtMust(applyProjectionPageTo(path, "session-1", func(page *ProjectionPage) (struct{}, error) {
		for ordinal := uint64(1); ordinal <= 3; ordinal++ {
			mutation := agentMessageMutation(ordinal)
			dtEditUpsert(&mutation, 0, func(item *core.TranscriptItem) {
				item.StableID = fmt.Sprintf("user-%d", ordinal)
				item.LatestContentEventOrdinal = nil
				item.Body = core.TranscriptBodyUser{Content: dtTextContent(t, fmt.Sprintf("message %d", ordinal))}
			})
			if _, err := page.Apply(ordinal, dtPreviousDigest(ordinal), EventDigest(ordinal), &mutation); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, nil
	}))

	page := dtFilteredPage(t, path, "session-1", 0, 1, nil, true)
	dtEqual(t, len(page.Items), 0)
	dtEqual(t, page.LatestSeq, uint64(3))
	dtEqual(t, page.NextAfterSeq, uint64(3),
		"non-agent rows do not consume the finished-message page limit or stall the cursor")
}
