// Port of mj-store/src/database/tests.rs, continued (materialized
// projections and transcript retention).

package store

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// dtValue is json!(..) as a Go value encoding/json writes with sorted keys,
// as serde_json writes a Value without preserve_order.
func dtValue(t testing.TB, value any) json.RawMessage {
	t.Helper()
	return dtJSON(t, value)
}

func TestMaterializedSessionRoundTripPreservesTypedProjection(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	for _, key := range []string{"delivery-one", "delivery-two"} {
		materialized.Transcript = append(materialized.Transcript, &core.TranscriptItem{
			StableID:        core.MessageItemID(key),
			Position:        7,
			CreatedAtMs:     1_500,
			LastChangedAtMs: 1_500,
			Body: core.TranscriptBodyMessage{
				Event: &core.MailboxEvent{
					Key:         key,
					Source:      "session_message",
					Wake:        true,
					CreatedAtMs: 1_450,
					Body: core.MailboxEventBodySessionMessage{
						From: core.Sender{Type: core.SenderSession, ID: "sender-session", Title: "Reviewer"},
						Text: fmt.Sprintf("%s\nFull delivered text 支持 Unicode", key),
					},
				},
			},
		})
	}

	large := strings.Repeat("terminal output supports Unicode λ\n", 4096)
	for _, item := range materialized.Transcript {
		tool, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		tool.Call.Edit(func(value *any) {
			object := (*value).(jsonvalue.Object)
			object.Set("rawInput", jsonvalue.ObjectOf(map[string]any{"command": large}))
			*value = object
		})
		tool.TerminalOutputs = append(tool.TerminalOutputs, core.TerminalOutputRecord{
			TerminalID: "large-terminal",
			Output:     core.NewPackedText(large),
			ExitCode:   dtPtr(uint32(0)),
		})
		tool.Presentation = &core.ToolCallPresentation{
			Summary:        "large command",
			Source:         core.NewPackedText(large),
			SourceKind:     core.ToolSummarySourceKindRawInput,
			ToolKind:       acp.ToolKindExecute,
			SummaryVersion: 0,
		}
		item.Body = tool
	}

	dtOK(t, SaveMaterializedSessionTo(database, &materialized))

	loaded := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	// Both durable rows and buffered wire envelopes must keep ACP JSON bytes.
	for index, before := range materialized.Transcript {
		after := loaded.Transcript[index]
		tool, ok := before.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		expected := string(dtMust(jsonvalue.Marshal(dtMust(jsonvalue.Parse(tool.Call.Value())))))
		loadedTool, ok := after.Body.(core.TranscriptBodyTool)
		if !ok {
			t.Fatal("tool body changed during store round trip")
		}
		dtEqual(t, string(dtMust(json.Marshal(loadedTool.Call))), expected)
		dtEqual(t, string(dtMust(json.Marshal(before.Body))), string(dtMust(json.Marshal(after.Body))))
		wire := client.TranscriptChange{
			Kind: client.TranscriptChangeItems,
			Items: client.TranscriptItems{
				Upserts: []*core.TranscriptItem{after},
			},
		}
		encoded := string(dtMust(json.Marshal(wire)))
		var decoded client.TranscriptChange
		dtOK(t, json.Unmarshal([]byte(encoded), &decoded))
		dtEqual(t, string(dtMust(json.Marshal(decoded))), encoded)
		dtTrue(t, strings.Contains(encoded, fmt.Sprintf(`"call":%s`, expected)))
	}
	dtEqual(t, loaded, &materialized)
	dtEqual(t, loaded.GetLastActivityAtMs(), dtPtr(int64(1_500)))
}

func TestMaterializedSummaryLoadsMessagesWithoutDeserializingFullHistory(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	materialized.Transcript = append(materialized.Transcript,
		&core.TranscriptItem{
			StableID:        "user:5",
			Position:        5,
			CreatedAtMs:     1_600,
			LastChangedAtMs: 1_600,
			Body: core.TranscriptBodyUser{
				Content: []json.RawMessage{dtValue(t, map[string]any{"type": "text", "text": "ship it"})},
			},
		},
		&core.TranscriptItem{
			StableID:        core.SessionRestartItemPrefix + "5",
			Position:        5,
			CreatedAtMs:     1_650,
			LastChangedAtMs: 1_650,
			Body:            core.TranscriptBodySystem{Text: core.SessionRestartText},
		},
		&core.TranscriptItem{
			StableID:                  "agent:6",
			Position:                  6,
			LatestContentEventOrdinal: dtPtr(uint64(7)),
			CreatedAtMs:               1_700,
			LastChangedAtMs:           1_700,
			Body: core.TranscriptBodyAgent{
				Chunks: []json.RawMessage{dtValue(t, map[string]any{
					"content": map[string]any{"type": "text", "text": "Finished"},
				})},
			},
		},
		&core.TranscriptItem{
			StableID:        "user:7",
			Position:        7,
			CreatedAtMs:     1_800,
			LastChangedAtMs: 1_800,
			Body: core.TranscriptBodyUser{
				Content: []json.RawMessage{dtValue(t, map[string]any{"type": "text", "text": "one more thing"})},
			},
		},
	)
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))

	// A large or damaged tool result must not be read just to build the
	// dashboard's two message snippets.
	connection := dtOpen(t, database)
	dtExec(t, connection, `UPDATE materialized_transcript_items
                 SET body_json = 'not-json' WHERE stable_id = 'tool:call-1'`)
	dtOK(t, connection.Close())

	summary := dtMust(loadMaterializedSessionSummaryFrom(database, "session-1"))
	dtEqual(t, summary.LastUserMessage, dtPtr("one more thing"))
	dtEqual(t, summary.LastAgentMessage, dtPtr("Finished"))
	dtTrue(t, !summary.LastAgentMessageFollowsLastUser)
	dtEqual(t, summary.AgentMessageLatestContentOrdinals, []uint64{2, 7})
	dtEqual(t, len(summary.InterruptionEventOrdinals), 0)
	dtEqual(t, summary.Execution, materialized.Execution)
	_, err := LoadMaterializedSessionFrom(database, "session-1")
	dtTrue(t, err != nil)

	// `/clear` starts a new conversation: the dashboard preview must not
	// carry the earlier conversation's messages past the boundary.
	connection = dtOpen(t, database)
	dtExec(t, connection, `INSERT INTO materialized_transcript_items(session_id, stable_id, position,
                 created_at_ms, last_changed_at_ms, body_json)
             VALUES ('session-1', ?1, 8, 1900, 1900, ?2)`,
		core.ContextBoundaryPrefix+"clear-1",
		string(dtMust(json.Marshal(core.TranscriptBody(core.TranscriptBodySystem{Text: "Context cleared"})))))
	dtOK(t, connection.Close())
	summary = dtMust(loadMaterializedSessionSummaryFrom(database, "session-1"))
	dtEqual(t, summary.LastUserMessage, (*string)(nil))
	dtEqual(t, summary.LastAgentMessage, (*string)(nil))
	dtTrue(t, !summary.LastAgentMessageFollowsLastUser)
}

func TestADamagedPromptQueueDoesNotHideOtherSessionsQueuesAtStartup(t *testing.T) {
	path := dtDatabase(t, "queued-prompts.sqlite3")
	for _, id := range []string{"damaged", "healthy"} {
		record := session(id, "project")
		dtOK(t, SaveSessionTo(path, &record))
		materialized := core.MaterializedSessionEmpty(id)
		for _, commandID := range []string{"first", "second", "third"} {
			materialized.QueuedPrompts = append(materialized.QueuedPrompts, core.MaterializedQueuedPrompt{
				CommandID:  commandID,
				Kind:       core.QueuedCommandKindPrompt{},
				Content:    []any{map[string]any{"type": "text", "text": commandID}},
				QueuedAtMs: 1_000,
			})
		}
		dtOK(t, SaveMaterializedSessionTo(path, &materialized))
	}
	connection := dtOpen(t, path)
	dtExec(t, connection, `UPDATE materialized_queued_prompts SET content_json = 'null'
         WHERE session_id = 'damaged' AND command_id = 'second'`)
	dtOK(t, connection.Close())

	var damaged []string
	queues := dtMust(readAllMaterializedQueuedPrompts(path, func(id string, _ error) error {
		damaged = append(damaged, id)
		return nil
	}))
	dtEqual(t, damaged, []string{"damaged"})
	_, present := queues["damaged"]
	dtTrue(t, !present)
	var healthy []string
	for _, prompt := range queues["healthy"] {
		healthy = append(healthy, prompt.CommandID)
	}
	dtEqual(t, healthy, []string{"first", "second", "third"})
	_, err := loadMaterializedQueuedPromptsFrom(path)
	dtTrue(t, err != nil)
}

func TestBulkMaterializedActivityMatchesSessionSummaries(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	for _, sessionID := range []string{"running", "idle", "without-projection"} {
		record := session(sessionID, "project-1")
		dtOK(t, SaveSessionTo(database, &record))
	}
	connection := dtOpen(t, database)
	dtExec(t, connection, "DELETE FROM materialized_sessions WHERE session_id = 'without-projection'")
	dtOK(t, connection.Close())

	running := MaterializedSession("running")
	running.LastActivityAtMs = dtPtr(int64(2_000))
	running.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: 1_900}
	dtOK(t, SaveMaterializedSessionTo(database, &running))

	idle := MaterializedSession("idle")
	idle.LastActivityAtMs = dtPtr(int64(3_000))
	idle.Execution = core.MaterializedExecutionStateIdle{}
	dtOK(t, SaveMaterializedSessionTo(database, &idle))

	activity := dtMust(loadMaterializedActivityFrom(database))
	for _, sessionID := range []string{"running", "idle"} {
		summary := dtMust(loadMaterializedSessionSummaryFrom(database, sessionID))
		found, ok := activity[sessionID]
		dtTrue(t, ok, sessionID)
		dtEqual(t, found, MaterializedActivity{LastActivityAtMs: summary.LastActivityAtMs, Execution: summary.Execution})
	}
	_, present := activity["without-projection"]
	dtTrue(t, !present)
}

func TestQueuedPromptLoaderDoesNotDeserializeTranscriptHistory(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	expected := materialized.QueuedPrompts
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	connection := dtOpen(t, database)
	dtExec(t, connection, "UPDATE materialized_transcript_items SET body_json = 'not-json'")
	dtOK(t, connection.Close())

	queues := dtMust(loadMaterializedQueuedPromptsFrom(database))

	found, ok := queues["session-1"]
	dtTrue(t, ok)
	dtEqual(t, found, expected)
	_, err := LoadMaterializedSessionFrom(database, "session-1")
	dtTrue(t, err != nil)
}

func toolItem(t testing.TB, position uint64, path, oldText, newText string) *core.TranscriptItem {
	t.Helper()
	diff := schema.Diff{Path: path, NewText: newText}
	diff.OldText = &oldText
	core.CompactDiff(&diff)
	var diffValue any
	dtOK(t, json.Unmarshal(dtJSON(t, schema.ToolCallContent{Diff: &diff}), &diffValue))
	return &core.TranscriptItem{
		StableID:        fmt.Sprintf("tool:call-%d", position),
		Position:        position,
		CreatedAtMs:     1_000,
		LastChangedAtMs: 1_000,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(dtValue(t, map[string]any{
				"toolCallId": fmt.Sprintf("call-%d", position),
				"title":      "Edit files",
				"kind":       "edit",
				"status":     "completed",
				"locations":  []any{map[string]any{"path": path}},
				"content": []any{
					diffValue,
					map[string]any{
						"type":    "content",
						"content": map[string]any{"type": "text", "text": strings.Repeat("x", 64*1024)},
					},
				},
				"rawInput":  map[string]any{"file_text": strings.Repeat("y", 64*1024)},
				"rawOutput": map[string]any{"formatted_output": strings.Repeat("z", 64*1024)},
			})),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{},
		},
	}
}

func dtNumberedLines(count int) string {
	var text strings.Builder
	for line := range count {
		fmt.Fprintf(&text, "line %d\n", line)
	}
	return text.String()
}

// The same release, on a record written before diffs were stored as patches:
// it holds two full copies of the file and no counts, so the counts have to be
// computed before the copies go.
func TestReleasingADiffWrittenBeforePatchesKeepsItsStat(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	oldText := dtNumberedLines(400)
	newText := strings.Replace(oldText, "line 200\n", "line 200 edited\n", -1)
	materialized := core.MaterializedSessionEmpty("session-1")
	materialized.AppliedEventOrdinal = 30
	materialized.AppliedEventDigest = EventDigest(30)
	materialized.Transcript = []*core.TranscriptItem{{
		StableID:        "tool:call-10",
		Position:        10,
		CreatedAtMs:     1_000,
		LastChangedAtMs: 1_000,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(dtValue(t, map[string]any{
				"toolCallId": "call-10",
				"title":      "Edit files",
				"kind":       "edit",
				"status":     "completed",
				"locations":  []any{map[string]any{"path": "src/legacy.rs"}},
				"content": []any{map[string]any{
					"type":    "diff",
					"path":    "src/legacy.rs",
					"oldText": oldText,
					"newText": newText,
				}},
			})),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{},
		},
	}}
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	before := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	statBefore, ok := transcript.MaterializedToolDiffstats(before.Transcript[0])
	dtTrue(t, ok)
	dtEqual(t, statBefore, []string{"src/legacy.rs  +1 −1"})

	retention := dtMust(compactMaterializedTranscriptIn(database, "session-1", 15))

	dtEqual(t, retention.Items, 1)
	dtTrue(t, retention.Bytes > 4*1024)
	after := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	statAfter, ok := transcript.MaterializedToolDiffstats(after.Transcript[0])
	dtTrue(t, ok)
	dtEqual(t, statAfter, statBefore, "the counts had to be computed from the copies before they were dropped")
}

func dtDiffstats(items []*core.TranscriptItem) [][]string {
	var stats [][]string
	for _, item := range items {
		if stat, ok := transcript.MaterializedToolDiffstats(item); ok {
			stats = append(stats, stat)
		}
	}
	return stats
}

// The projection only ever grew. A checkpoint archive holds the whole
// transcript up to its frontier, so what sits below that frontier is a second
// copy — but only the part nobody reads back may go, and the diffstat the
// transcript still shows must survive it.
func TestACheckpointReleasesTheToolOutputItCoversAndKeepsTheDiffstat(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	oldText := dtNumberedLines(400)
	newText := strings.Replace(oldText, "line 200\n", "line 200 edited\n", -1)
	materialized := core.MaterializedSessionEmpty("session-1")
	materialized.AppliedEventOrdinal = 30
	materialized.AppliedEventDigest = EventDigest(30)
	materialized.Transcript = []*core.TranscriptItem{
		{
			StableID:        "user:1",
			Position:        1,
			CreatedAtMs:     1_000,
			LastChangedAtMs: 1_000,
			Body: core.TranscriptBodyUser{
				Content: []json.RawMessage{dtValue(t, map[string]any{"type": "text", "text": "edit it"})},
			},
		},
		toolItem(t, 10, "src/covered.rs", oldText, newText),
		toolItem(t, 20, "src/live.rs", oldText, newText),
	}
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	before := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	statsBefore := dtDiffstats(before.Transcript)

	// A checkpoint at frontier 15 covers the first tool call, not the second.
	retention := dtMust(compactMaterializedTranscriptIn(database, "session-1", 15))

	dtEqual(t, retention.Items, 1, "only the covered tool call was rewritten")
	dtTrue(t, retention.Bytes > 128*1024)
	dtTrue(t, !retention.Remaining)

	after := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, dtDiffstats(after.Transcript), statsBefore, "the diffstat the transcript shows must survive the release")
	dtEqual(t, after.Transcript[0], before.Transcript[0], "a user message is never released")
	dtEqual(t, after.Transcript[2], before.Transcript[2], "a tool call the checkpoint does not cover is never released")
	tool, ok := after.Transcript[1].Body.(core.TranscriptBodyTool)
	if !ok {
		t.Fatal("expected a tool call")
	}
	var call map[string]any
	dtOK(t, json.Unmarshal(tool.Call.Value(), &call))
	_, hasRawInput := call["rawInput"]
	dtTrue(t, !hasRawInput)
	_, hasRawOutput := call["rawOutput"]
	dtTrue(t, !hasRawOutput)
	dtEqual(t, call["title"], "Edit files", "the call still says what it did")
	dtEqual(t, call["status"], "completed")
	dtEqual(t, call["locations"].([]any)[0].(map[string]any)["path"], "src/covered.rs")
	dtEqual(t, len(call["content"].([]any)), 1, "only the diff stays")

	// Running it again finds nothing left to release.
	dtEqual(t, dtMust(compactMaterializedTranscriptIn(database, "session-1", 15)), core.TranscriptRetention{})
}

func TestCompactionDrainsAllBatchesPastNonShrinkingToolRows(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	oldText := dtNumberedLines(400)
	newText := strings.Replace(oldText, "line 200\n", "line 200 edited\n", -1)
	nonShrinking := &core.TranscriptItem{
		StableID:        "tool:5",
		Position:        5,
		CreatedAtMs:     1_000,
		LastChangedAtMs: 1_000,
		Body: core.TranscriptBodyTool{
			Call: core.OpaqueToolCallFromValue(dtValue(t, map[string]any{
				"toolCallId": "call-5",
				"title":      strings.Repeat("x", 5*1024),
				"status":     "completed",
				"content":    []any{},
			})),
			TerminalOutputs: []core.TerminalOutputRecord{},
			TerminalRefs:    []string{},
		},
	}
	materialized := core.MaterializedSessionEmpty("session-1")
	materialized.AppliedEventOrdinal = 30
	materialized.AppliedEventDigest = EventDigest(30)
	materialized.Transcript = []*core.TranscriptItem{
		nonShrinking,
		toolItem(t, 10, "src/first.rs", oldText, newText),
		toolItem(t, 15, "src/second.rs", oldText, newText),
	}
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))

	// Batch size 1 forces multiple batches and starts on a >4 KiB tool row that
	// cannot shrink; the cursor must still advance and compact both later rows.
	retention := dtMust(compactMaterializedTranscriptAllIn(database, "session-1", 20, 1))
	dtEqual(t, retention.Items, 2)
	dtTrue(t, !retention.Remaining)
	dtTrue(t, retention.Bytes > 128*1024)
}

// The steady-state poll loads this on every change, so it must cost the
// window rather than the history — including the two facts that live outside
// the window, which are read rather than scanned for.
func TestTheBoundedProjectionCarriesAWindowAndTheFactsOutsideIt(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	materialized.SessionTitle = nil
	// A turn the harness started on its own is a turn boundary too, and it
	// sits outside the window here, so the read and the scan have to agree on
	// it rather than on the user message alone.
	harnessTurn := &core.TranscriptItem{
		StableID:        core.HarnessTurnItemPrefix + "2",
		Position:        2,
		CreatedAtMs:     1_150,
		LastChangedAtMs: 1_150,
		Body:            core.TranscriptBodySystem{Text: core.HarnessTurnText},
	}
	transcriptItems := append([]*core.TranscriptItem{}, materialized.Transcript[:2]...)
	transcriptItems = append(transcriptItems, harnessTurn)
	materialized.Transcript = append(transcriptItems, materialized.Transcript[2:]...)
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	whole := dtMust(LoadMaterializedSessionFrom(database, "session-1"))

	bounded, window, err := loadMaterializedProjectionTailFrom(database, "session-1", 2)
	dtOK(t, err)
	dtTrue(t, bounded != nil && window != nil)

	dtEqual(t, len(bounded.Transcript), 2, "the window is the size that was asked for")
	dtEqual(t, window.OmittedItems, len(whole.Transcript)-2)
	// Everything but the transcript is loaded whole: these are all bounded by
	// the session, not by its history.
	dtEqual(t, bounded.AppliedEventOrdinal, whole.AppliedEventOrdinal)
	dtEqual(t, bounded.AppliedEventDigest, whole.AppliedEventDigest)
	dtEqual(t, bounded.Configuration, whole.Configuration)
	dtEqual(t, bounded.QueuedPrompts, whole.QueuedPrompts)
	dtEqual(t, bounded.PendingElicitations, whole.PendingElicitations)
	dtEqual(t, bounded.Execution, whole.Execution)

	// The head is outside the window, and the two facts that live there are
	// still the ones a complete projection would have found by scanning.
	complete := core.ProjectionWindowOf(whole)
	dtEqual(t, complete.OmittedItems, 0)
	dtEqual(t, window.ProvisionalTitle, complete.ProvisionalTitle)
	dtEqual(t, window.LatestTurnStartPosition, complete.LatestTurnStartPosition)
	dtEqual(t, window.LatestTurnStartPosition, dtPtr(uint64(2)),
		"the newest turn start is the harness turn, not the user message")
	dtTrue(t, window.ProvisionalTitle != nil)
	for _, item := range bounded.Transcript {
		dtTrue(t, !item.IsTurnStart(), "the test only proves anything if the window excludes both turn starts")
	}
}

// Seeding a conversation shows the end of it, so the reader must cost the
// rows it returns rather than the rows that exist. Corrupting the head proves
// the head is never touched.
func TestTheTranscriptTailReaderReturnsTheEndWithoutReadingTheHead(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))
	connection := dtOpen(t, database)
	dtExec(t, connection, "UPDATE materialized_transcript_items SET body_json = 'not-json' WHERE position <= 2")
	dtOK(t, connection.Close())

	tail := dtMust(readMaterializedTranscript(dtOpenReader(t, database), "session-1", dtPtr(2)))

	type positioned struct {
		stableID string
		position uint64
	}
	var found []positioned
	for _, item := range tail {
		found = append(found, positioned{item.StableID, item.Position})
	}
	dtEqual(t, found, []positioned{{"tool:call-1", 3}, {"plan:1", 4}})
	// Asking for more than exists returns what exists, and the corrupt head is
	// what makes that an error rather than a short read.
	_, err := readMaterializedTranscript(dtOpenReader(t, database), "session-1", dtPtr(256))
	dtTrue(t, err != nil)
	dtEqual(t, len(dtMust(readMaterializedTranscript(dtOpenReader(t, database), "unknown", dtPtr(256)))), 0)
}

// dtFrontier is a materialized frontier as Rust's Option<(u64, String)>.
type dtFrontier struct {
	ordinal uint64
	digest  string
}

func dtFrontierOf(ordinal uint64, digest string, ok bool, err error) *dtFrontier {
	if err != nil {
		panic(fmt.Sprintf("called unwrap on an error: %v", err))
	}
	if !ok {
		return nil
	}
	return &dtFrontier{ordinal, digest}
}

// Resume compares frontiers to decide whether to rebuild a projection,
// and clears the queue without touching the transcript when it does not.
func TestAQueueReplacementKeepsTheProjectionFrontierAndTranscript(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	record := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &record))
	materialized := MaterializedSession("session-1")
	dtTrue(t, len(materialized.QueuedPrompts) != 0)
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))

	dtEqual(t, dtFrontierOf(materializedEventFrontierFrom(database, "session-1")),
		&dtFrontier{materialized.AppliedEventOrdinal, EventDigest(7)})
	dtEqual(t, dtFrontierOf(materializedEventFrontierFrom(database, "unknown")), (*dtFrontier)(nil))

	dtOK(t, replaceMaterializedQueuedPromptsIn(database, "session-1", nil))

	cleared := dtMust(LoadMaterializedSessionFrom(database, "session-1"))
	dtEqual(t, len(cleared.QueuedPrompts), 0)
	dtEqual(t, cleared.Transcript, materialized.Transcript)
	dtEqual(t, cleared.AppliedEventOrdinal, materialized.AppliedEventOrdinal)
	dtEqual(t, cleared.AppliedEventDigest, materialized.AppliedEventDigest)

	dtOK(t, replaceMaterializedQueuedPromptsIn(database, "session-1", materialized.QueuedPrompts))
	dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")), &materialized)
}

func TestOperationalSessionUpdatesDoNotDeleteItsProjection(t *testing.T) {
	database := dtDatabase(t, "hel.sqlite3")
	operational := session("session-1", "project-1")
	dtOK(t, SaveSessionTo(database, &operational))
	materialized := MaterializedSession("session-1")
	dtOK(t, SaveMaterializedSessionTo(database, &materialized))

	operational.SessionTitleOverride = dtPtr("renamed")
	dtOK(t, SaveSessionTo(database, &operational))

	dtEqual(t, dtMust(LoadMaterializedSessionFrom(database, "session-1")), &materialized)
}
