// Port of mj-store/src/database/tests.rs, continued (the actor's history
// window and history pages).

package store

import (
	"cmp"
	"encoding/json"
	"fmt"
	"math"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func longConversation(t testing.TB) core.MaterializedSession {
	t.Helper()
	projection := core.MaterializedSessionEmpty("history-window")
	projection.AppliedEventOrdinal = 2500
	projection.AppliedEventDigest = EventDigest(2500)
	answer := dtAgentText(t, strings.Repeat("answer ", 50))
	projection.Transcript = make([]*core.TranscriptItem, 0, 2500)
	for position := uint64(1); position <= 2500; position++ {
		request := position%100 == 1
		kind := "agent"
		if request {
			kind = "user"
		}
		item := &core.TranscriptItem{
			StableID:        fmt.Sprintf("%s:%04d", kind, position),
			Position:        position,
			CreatedAtMs:     int64(position),
			LastChangedAtMs: int64(position),
		}
		if request {
			item.Body = core.TranscriptBodyUser{Content: dtTextContent(t, fmt.Sprintf("request %d", position))}
		} else {
			item.LatestContentEventOrdinal = dtPtr(position)
			item.Body = core.TranscriptBodyAgent{Chunks: []json.RawMessage{answer}, Streaming: false}
		}
		projection.Transcript = append(projection.Transcript, item)
	}
	return projection
}

// dtCloneSession is MaterializedSession::clone: its own transcript list,
// sharing the items as Rust's Arc clones do.
func dtCloneSession(session core.MaterializedSession) core.MaterializedSession {
	session.Transcript = slices.Clone(session.Transcript)
	return session
}

// dtMakeMut is Arc::make_mut on transcript[index]: the item, copied so that
// clones holding the old item keep it.
func dtMakeMut(transcript []*core.TranscriptItem, index int) *core.TranscriptItem {
	item := *transcript[index]
	transcript[index] = &item
	return &item
}

// dtEditCall is OpaqueToolCall::edit on a tool item's call.
func dtEditCall(item *core.TranscriptItem, edit func(object *jsonvalue.Object)) {
	tool, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return
	}
	tool.Call.Edit(func(value *any) {
		object := (*value).(jsonvalue.Object)
		edit(&object)
		*value = object
	})
	item.Body = tool
}

// dtFixtureTool is the fixture projection's tool item, a fresh copy.
func dtFixtureTool(sessionID string) *core.TranscriptItem {
	return MaterializedSession(sessionID).Transcript[2]
}

func dtActorProjection(t testing.TB, path, sessionID string) (core.MaterializedSession, core.ProjectionWindow) {
	t.Helper()
	projection, window, err := loadMaterializedActorProjectionFrom(path, sessionID)
	dtOK(t, err)
	if projection == nil || window == nil {
		t.Fatal("called unwrap on a None value")
	}
	return *projection, *window
}

func dtContainsItem(items []*core.TranscriptItem, wanted *core.TranscriptItem) bool {
	return slices.ContainsFunc(items, func(item *core.TranscriptItem) bool {
		return dtLooseEqual(reflect.ValueOf(item), reflect.ValueOf(wanted))
	})
}

func dtHasStableID(items []*core.TranscriptItem, stableID string) bool {
	return slices.ContainsFunc(items, func(item *core.TranscriptItem) bool { return item.StableID == stableID })
}

// Hard-won: 312cdbd23: stale pending content made the live window load nearly all history.
// Hard-won: f7df24176: store queries split turns at elicitation replies and missed native prompts.
// Hard-won: 8c2f98de2: unread output outside the transcript window lost its mark.
func TestActorHistoryWindowKeepsOpenStreamsAndRehydratesOldTools(t *testing.T) {
	path := dtDatabase(t, "history.sqlite3")
	base := longConversation(t)
	// An elicitation reply inside the old turn is not a turn start, and the
	// newest turn begins with a native prompt; store and trim must agree.
	reply := *base.Transcript[50]
	reply.StableID = core.ElicitationReplyItemPrefix + "0051"
	reply.Body = core.TranscriptBodyUser{Content: dtTextContent(t, "approve")}
	reply.LatestContentEventOrdinal = nil
	base.Transcript[50] = &reply
	dtMakeMut(base.Transcript, 2400).StableID = "native-user:2401"
	record := session(base.SessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	for _, pending := range []string{"none", "tool", "stream"} {
		full := dtCloneSession(base)
		item := dtMakeMut(full.Transcript, 1)
		if agent, ok := item.Body.(core.TranscriptBodyAgent); ok {
			agent.Streaming = pending == "stream"
			item.Body = agent
		}
		if pending == "tool" {
			tool := dtFixtureTool(full.SessionID)
			dtEditCall(tool, func(object *jsonvalue.Object) { object.Set("status", "in_progress") })
			full.Transcript[2] = tool
		}
		dtOK(t, SaveMaterializedSessionTo(path, &full))
		loaded, window := dtActorProjection(t, path, full.SessionID)
		trimmed := dtCloneSession(full)
		expected := core.ProjectionWindowOf(&full)
		expected.Trim(&trimmed, ProjectionTailItems)
		dtEqual(t, loaded, trimmed)
		dtEqual(t, window, expected)
		wantLength := 700
		if pending == "none" {
			wantLength = 600
		}
		dtEqual(t, len(loaded.Transcript), wantLength)
		if pending != "none" {
			dtEqual(t, loaded.Transcript[:100], full.Transcript[:100])
			dtEqual(t, loaded.Transcript[100], full.Transcript[1900])
		}
		dtEqual(t, window.ProvisionalTitle, dtPtr("request 1"))
		dtEqual(t, window.LatestTurnStartPosition, dtPtr(uint64(2401)))
		dtEqual(t, dtMust(LoadMaterializedSessionFrom(path, full.SessionID)), &full,
			"the durable source for checkpoints must stay complete")
	}
	// Completed tool rows can push every unread item outside the actor window.
	for _, interrupted := range []bool{false, true} {
		history := longConversation(t)
		toolItem := dtFixtureTool(history.SessionID)
		if _, ok := toolItem.Body.(core.TranscriptBodyTool); !ok {
			t.Fatal("tool fixture")
		}
		dtEditCall(toolItem, func(object *jsonvalue.Object) { object.Set("status", "completed") })
		toolBody := toolItem.Body
		for index := range history.Transcript {
			item := dtMakeMut(history.Transcript, index)
			if !item.IsTurnStart() && item.Position != 2 {
				item.Body = toolBody
				item.StableID = fmt.Sprintf("tool:%d", item.Position)
				item.LatestContentEventOrdinal = nil
			}
			if interrupted && item.Position == 3 {
				item.Body = core.TranscriptBodySystem{Text: "Work interrupted"}
				item.StableID = core.WorkInterruptedItemPrefix + "3"
			}
		}
		expected := uint64(2)
		if interrupted {
			expected = 3
		}
		dtOK(t, SaveMaterializedSessionTo(path, &history))
		live := dtCloneSession(history)
		liveWindow := core.ProjectionWindowOf(&history)
		liveWindow.Trim(&live, ProjectionTailItems)
		for _, item := range live.Transcript {
			dtTrue(t, !item.IsNonemptyAgentMessage() && !item.IsWorkInterruption())
		}
		dtEqual(t, liveWindow.LatestUnreadEventOrdinal, dtPtr(expected))
		// This valid JSON is not a TranscriptBody. Loading all history would fail.
		connection := dtOpen(t, path)
		dtExec(t, connection,
			"UPDATE materialized_transcript_items SET body_json = ?2 WHERE session_id = ?1 AND stable_id = 'tool:50'",
			history.SessionID, `{"kind":"unknown-old-row"}`)
		dtOK(t, connection.Close())
		for range 2 {
			reloaded, window := dtActorProjection(t, path, history.SessionID)
			dtEqual(t, window.LatestUnreadEventOrdinal, dtPtr(expected))
			dtEqual(t, window, liveWindow)
			dtEqual(t, reloaded.Transcript, live.Transcript)
			tail := client.SessionTailOf(&reloaded, &window, 0)
			dtEqual(t, tail.Window.LatestUnreadEventOrdinal, dtPtr(expected))
		}
		_, err := LoadMaterializedSessionFrom(path, history.SessionID)
		dtTrue(t, err != nil)
	}
}

func dtPositionKey(item *core.TranscriptItem) (uint64, string) {
	return item.Position, item.StableID
}

func TestHistoryPagesHaveNoGapsAcrossPositionTiesAndConcurrentAppends(t *testing.T) {
	path := dtDatabase(t, "history.sqlite3")
	full := longConversation(t)
	// Streaming content revisions do not change a message's history position.
	for index := range full.Transcript {
		item := dtMakeMut(full.Transcript, index)
		item.Position = (item.Position + 1) / 2
		if _, ok := item.Body.(core.TranscriptBodyAgent); ok {
			item.LatestContentEventOrdinal = dtPtr(uint64(2500))
		}
	}
	for _, key := range []string{"history-one", "history-two"} {
		full.Transcript = append(full.Transcript, &core.TranscriptItem{
			StableID:        core.MessageItemID(key),
			Position:        624,
			CreatedAtMs:     1000,
			LastChangedAtMs: 1000,
			Body: core.TranscriptBodyMessage{
				Event: &core.MailboxEvent{
					Key:         key,
					Source:      "session_message",
					Wake:        false,
					CreatedAtMs: 900,
					Body:        core.MailboxEventBodyParentMessage{Text: fmt.Sprintf("%s\nComplete historical text", key)},
				},
			},
		})
	}
	slices.SortFunc(full.Transcript, func(a, b *core.TranscriptItem) int {
		return cmp.Or(cmp.Compare(a.Position, b.Position), strings.Compare(a.StableID, b.StableID))
	})
	record := session(full.SessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	dtOK(t, SaveMaterializedSessionTo(path, &full))
	var before *core.TranscriptCursor
	var pages [][]*core.TranscriptItem
	for {
		page := dtMust(loadTranscriptHistoryFrom(path, full.SessionID, before, 127))
		if page == nil {
			t.Fatal("called unwrap on a None value")
		}
		dtTrue(t, len(page.Items) <= 127)
		for index := 1; index < len(page.Items); index++ {
			previousPosition, previousID := dtPositionKey(page.Items[index-1])
			position, stableID := dtPositionKey(page.Items[index])
			dtTrue(t, previousPosition < position || (previousPosition == position && previousID < stableID))
		}
		pages = append(pages, page.Items)
		before = page.Before
		if len(pages) == 1 {
			appended := dtCloneSession(full)
			item := *full.Transcript[len(full.Transcript)-1]
			item.StableID = "new-message"
			item.Position = 2501
			item.LatestContentEventOrdinal = dtPtr(uint64(2501))
			appended.Transcript = append(appended.Transcript, &item)
			appended.AppliedEventOrdinal = 2501
			appended.AppliedEventDigest = EventDigest(2501)
			dtOK(t, SaveMaterializedSessionTo(path, &appended))
		}
		if before == nil {
			break
		}
	}
	var read []*core.TranscriptItem
	for index := len(pages) - 1; index >= 0; index-- {
		read = append(read, pages[index]...)
	}
	dtEqual(t, read, full.Transcript)
	all := dtMust(loadTranscriptHistoryFrom(path, full.SessionID, nil, math.MaxInt))
	dtTrue(t, all != nil)
	dtEqual(t, len(all.Items), 256)
	dtTrue(t, dtMust(loadTranscriptHistoryFrom(path, "missing", nil, 10)) == nil)
}

// dtRelayEvent is a v1 relay event following projection, with its digest.
func dtRelayEvent(t testing.TB, ordinal uint64, previousDigest string, observation core.RelayObservation) core.RelayEvent {
	t.Helper()
	event := core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        ordinal,
		PreviousDigest: previousDigest,
		RecordedAtMs:   5000,
		Observation:    observation,
	}
	event.Digest = dtMust(core.RelayEventDigest(&event))
	return event
}

// dtCommitEvent projects event onto live, commits the mutation through a
// projection page at path, and applies it to live, as the session actor does.
// It returns the mutation.
func dtCommitEvent(t testing.TB, path, sessionID string, live *core.MaterializedSession, event *core.RelayEvent) core.MaterializedSessionMutation {
	t.Helper()
	mutation := dtMust(transcript.ProjectRelayEvent(live, event)).Mutation
	dtOK(t, dtApplyPage(path, sessionID, dtPageApply(event.Ordinal, event.PreviousDigest, event.Digest, mutation)))
	dtOK(t, transcript.ApplyCommittedProjectionEvent(live, event, mutation))
	return mutation
}

func TestHistoryWindowDeduplicatesDeliveriesAfterTheirRowLeavesTheLiveTail(t *testing.T) {
	message := core.MailboxEvent{
		Key:         "old-delivery",
		Source:      "session_message",
		Wake:        true,
		CreatedAtMs: 200,
		Body:        core.MailboxEventBodyParentMessage{Text: "The original message.\nStill complete."},
	}
	reviewEvent := core.MailboxEvent{
		Key:         "github:repo#7:review:1",
		Source:      "github",
		Wake:        true,
		CreatedAtMs: 200,
		Body: core.MailboxEventBodyGithubReview{
			ItemKind:    core.GithubItemKindPullRequest,
			Number:      7,
			Title:       "Delivery rows",
			Author:      "coderabbitai[bot]",
			Body:        "",
			ReviewState: dtPtr("commented"),
		},
	}
	// A message has a row of its own; external events share one row per
	// delivery. Both must be found again when a replay names them.
	messageCopy := message
	for _, entry := range []struct {
		message  core.MailboxEvent
		stableID string
		body     core.TranscriptBody
	}{
		{message, core.MessageItemID(message.Key), core.TranscriptBodyMessage{Event: &messageCopy}},
		{
			reviewEvent,
			"system:mailbox:delivered:github:repo#7:review:1",
			core.TranscriptBodySystem{Text: "Mailbox events:\n  - Review on your PR #7"},
		},
	} {
		path := dtDatabase(t, "message-history.sqlite3")
		full := longConversation(t)
		original := &core.TranscriptItem{
			StableID:        entry.stableID,
			Position:        3,
			CreatedAtMs:     300,
			LastChangedAtMs: 300,
			Body:            entry.body,
		}
		full.Transcript[2] = original
		record := session(full.SessionID, "project-1")
		dtOK(t, SaveSessionTo(path, &record))
		dtOK(t, SaveMaterializedSessionTo(path, &full))
		live, window := dtActorProjection(t, path, full.SessionID)
		for _, observation := range []core.RelayObservation{
			core.RelayObservationMailboxEventsDelivered{
				EventKeys: []string{entry.message.Key},
				Path:      core.MailboxDeliveryPathToolHook,
				HookEvent: dtPtr("PostToolUse"),
				Events:    []core.MailboxEvent{entry.message},
				LeaseID:   dtPtr("replayed-lease"),
			},
			core.RelayObservationCommandQueued{
				CommandID:   "mailbox-wake-replayed",
				Command:     core.RelayCommandMailboxWake{Events: []core.MailboxEvent{entry.message}},
				CreatedAtMs: 5000,
			},
		} {
			dtTrue(t, !dtHasStableID(live.Transcript, original.StableID))
			event := dtRelayEvent(t, live.AppliedEventOrdinal+1, live.AppliedEventDigest, observation)
			restored := dtMust(loadProjectionReferencesFrom(path, &live, []core.RelayEvent{event}))
			dtEqual(t, restored, []*core.TranscriptItem{original})
			window.OmittedItems -= len(restored)
			live.Transcript = append(slices.Clone(restored), live.Transcript...)
			mutation := dtMust(transcript.ProjectRelayEvent(&live, &event)).Mutation
			dtEqual(t, len(mutation.Transcript), 0)
			dtOK(t, dtApplyPage(path, full.SessionID, dtPageApply(event.Ordinal, event.PreviousDigest, event.Digest, mutation)))
			dtOK(t, transcript.ApplyCommittedProjectionEvent(&live, &event, mutation))
			window.Trim(&live, ProjectionTailItems)
			saved := dtMust(LoadMaterializedSessionFrom(path, full.SessionID))
			dtEqual(t, len(saved.Transcript), len(full.Transcript))
			dtEqual(t, saved.Transcript[2], original)
		}
	}
}

// dtToolCallUpdate is a session update for call-1's tool call.
func dtToolCallUpdate(update schema.ToolCallUpdate) core.RelayObservation {
	update.ToolCallID = "call-1"
	return core.RelayObservationSessionUpdate{
		Update: schema.SessionUpdate{ToolCallUpdate: &update},
	}
}

// Hard-won: 312cdbd23: late completion must preserve evicted tools and uniquely claim parked output.
func TestHistoryWindowRehydratesLateToolUpdatesBeforeDurableProjection(t *testing.T) {
	path := dtDatabase(t, "history.sqlite3")
	full := longConversation(t)
	oldTool := dtFixtureTool(full.SessionID)
	output := strings.Repeat("old output ", 10_000)
	rawOutput := map[string]any{"partial": output}
	dtEditCall(oldTool, func(object *jsonvalue.Object) {
		object.Set("status", "completed")
		object.Set("rawOutput", jsonvalue.ObjectOf(rawOutput))
	})
	full.Transcript[2] = oldTool
	record := session(full.SessionID, "project-1")
	dtOK(t, SaveSessionTo(path, &record))
	dtOK(t, SaveMaterializedSessionTo(path, &full))
	live, window := dtActorProjection(t, path, full.SessionID)
	dtTrue(t, !dtHasStableID(live.Transcript, oldTool.StableID))
	completed := schema.ToolCallStatusCompleted
	event := dtRelayEvent(t, 2501, full.AppliedEventDigest, dtToolCallUpdate(schema.ToolCallUpdate{
		Status: &completed,
		Title:  dtPtr("Late result"),
	}))
	restored := dtMust(loadProjectionReferencesFrom(path, &live, []core.RelayEvent{event}))
	dtEqual(t, len(restored), 1)
	dtTrue(t, dtContainsItem(restored, oldTool))
	window.OmittedItems -= len(restored)
	live.Transcript = append(slices.Clone(restored), live.Transcript...)
	dtEqual(t, len(dtMust(loadProjectionReferencesFrom(path, &live, []core.RelayEvent{event}))), 0)
	dtCommitEvent(t, path, full.SessionID, &live, &event)
	window.Trim(&live, ProjectionTailItems)
	saved := dtMust(LoadMaterializedSessionFrom(path, full.SessionID))
	dtEqual(t, len(saved.Transcript), 2500)
	index := slices.IndexFunc(saved.Transcript, func(item *core.TranscriptItem) bool { return item.StableID == oldTool.StableID })
	dtTrue(t, index >= 0)
	tool := saved.Transcript[index]
	dtEqual(t, tool.Position, oldTool.Position)
	dtEqual(t, tool.CreatedAtMs, oldTool.CreatedAtMs)
	body, ok := tool.Body.(core.TranscriptBodyTool)
	if !ok {
		t.Fatal("tool remains a tool")
	}
	var call map[string]any
	dtOK(t, json.Unmarshal(body.Call.Value(), &call))
	dtEqual(t, call["title"], "Late result")
	dtEqual(t, call["rawOutput"], rawOutput)
	dtEqual(t, len(body.TerminalRefs), 0)
	dtEqual(t, len(body.TerminalOutputs), 0)
	dtTrue(t, !dtHasStableID(live.Transcript, oldTool.StableID))
	dtEqual(t, len(live.Transcript)+window.OmittedItems, len(saved.Transcript))
}

// Hard-won: 83536df64: evicting an unfinished tool's turn separated it from its parked terminal output
func TestHistoryWindowKeepsPendingTurnForLateCompletion(t *testing.T) {
	for _, rawEarlier := range []bool{false, true} {
		for _, standalone := range []bool{false, true} {
			path := dtDatabase(t, "history.sqlite3")
			full := longConversation(t)
			tool := dtFixtureTool(full.SessionID)
			output := strings.Repeat("late output ", 10_000)
			// serde writes `output.as_bytes()` as an array of byte values.
			outputBytes := make([]int, len(output))
			for index := range len(output) {
				outputBytes[index] = int(output[index])
			}
			raw := dtJSON(t, map[string]any{"exit_code": 0, "output": outputBytes})
			dtEditCall(tool, func(object *jsonvalue.Object) {
				object.Set("status", "pending")
				if rawEarlier {
					object.Set("rawOutput", dtMust(jsonvalue.Parse(raw)))
				} else {
					object.Set("rawOutput", nil)
				}
			})
			dtEqual(t, len(tool.Body.(core.TranscriptBodyTool).TerminalRefs), 0)
			full.Transcript[2] = tool
			record := core.TerminalOutputRecord{
				TerminalID: "late-terminal",
				Output:     core.NewPackedText(output),
				ExitCode:   dtPtr(uint32(0)),
			}
			fallback := core.FallbackTerminalToolCall("late-terminal", "cargo test")
			completed := schema.ToolCallStatusCompleted
			fallback.Status = &completed
			var parkedID string
			var body core.TranscriptBody
			if standalone {
				parkedID = "terminal:late-terminal"
				body = core.TranscriptBodyTerminalOutputOf(record)
			} else {
				parkedID = fmt.Sprintf("tool:%s", fallback.ToolCallID)
				body = core.TranscriptBodyTool{
					Call:            core.OpaqueToolCallFromValue(dtJSON(t, fallback)),
					TerminalOutputs: []core.TerminalOutputRecord{record},
					TerminalRefs:    []string{"late-terminal"},
				}
			}
			full.Transcript[3] = &core.TranscriptItem{
				StableID:        parkedID,
				Position:        4,
				CreatedAtMs:     1200,
				LastChangedAtMs: 1400,
				Body:            body,
			}
			sessionRecord := session(full.SessionID, "project-1")
			dtOK(t, SaveSessionTo(path, &sessionRecord))
			dtOK(t, SaveMaterializedSessionTo(path, &full))
			live, window := dtActorProjection(t, path, full.SessionID)
			dtTrue(t, dtContainsItem(live.Transcript, tool))
			dtTrue(t, dtHasStableID(live.Transcript, parkedID))
			update := schema.ToolCallUpdate{Status: &completed}
			if !rawEarlier {
				update.RawOutput = raw
			}
			event := dtRelayEvent(t, 2501, full.AppliedEventDigest, dtToolCallUpdate(update))
			dtEqual(t, len(dtMust(loadProjectionReferencesFrom(path, &live, []core.RelayEvent{event}))), 0)
			dtCommitEvent(t, path, full.SessionID, &live, &event)
			saved := dtMust(LoadMaterializedSessionFrom(path, full.SessionID))
			for _, projection := range []*core.MaterializedSession{&live, saved} {
				index := slices.IndexFunc(projection.Transcript, func(item *core.TranscriptItem) bool { return item.StableID == tool.StableID })
				dtTrue(t, index >= 0)
				owner, ok := projection.Transcript[index].Body.(core.TranscriptBodyTool)
				if !ok {
					t.Fatal("owner remains tool")
				}
				dtEqual(t, owner.TerminalRefs, []string{"late-terminal"})
				dtEqual(t, owner.TerminalOutputs[0].Output.Text(), output)
				dtTrue(t, !dtHasStableID(projection.Transcript, parkedID))
			}
			window.Trim(&live, ProjectionTailItems)
			loaded, loadedWindow := dtActorProjection(t, path, full.SessionID)
			dtEqual(t, live, loaded)
			dtEqual(t, window, loadedWindow)
			dtEqual(t, len(live.Transcript)+window.OmittedItems, len(saved.Transcript))
		}
	}
}
