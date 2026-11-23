// Port of the tests in mj-store/src/database/github_watch.rs.

package store

import (
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func githubTestUpdate(t *testing.T, update string) core.RelayObservation {
	t.Helper()
	var decoded acpschema.SessionUpdate
	if err := json.Unmarshal([]byte(update), &decoded); err != nil {
		t.Fatal(err)
	}
	return core.RelayObservationSessionUpdate{Update: decoded}
}

func githubTestAgentMessage(t *testing.T, text string, messageID string) core.RelayObservation {
	t.Helper()
	return githubTestUpdate(t, fmt.Sprintf(`{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":%q},"messageId":%q}`, text, messageID))
}

func TestCreationOutputComesOnlyFromCallsRunningAtCreation(t *testing.T) {
	path := filepath.Join(t.TempDir(), "creation.sqlite")
	record := session("session-1", "project-1")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
	connection, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	tool := func(id string, output string) string {
		return fmt.Sprintf(`{"kind":"tool","call":{"toolCallId":%q,"title":"script","status":"completed","content":[]},"terminal_outputs":[{"terminal_id":%q,"output":%q,"exit_code":0}]}`, id, id, output)
	}
	// This fixture uses SQL to exercise the persisted transcript format.
	for _, row := range []struct {
		position         int64
		started, changed int64
		body             string
	}{
		{1, 1000, 2000, tool("before", "early")},
		{2, 3000, 6000, tool("create", "https://github.com/owner/repo/issues/12")},
		// A later view or mention of the new item in the same turn would let
		// a session that never opened it claim it.
		{3, 7000, 7000, tool("view", "viewed later")},
		{4, 7000, 7000, `{"kind":"agent","chunks":[{"content":{"type":"text","text":"mentioned later"}}],"streaming":false}`},
	} {
		if _, err := connection.Exec("INSERT INTO materialized_transcript_items(session_id, stable_id, position, created_at_ms, last_changed_at_ms, body_json) VALUES ('session-1', ?1, ?2, ?3, ?4, ?5)",
			fmt.Sprint(row.position), row.position, row.started, row.changed, row.body); err != nil {
			t.Fatal(err)
		}
	}
	lines, err := loadGithubSessionCreationOutputFrom(path, "session-1", 5000)
	if err != nil {
		t.Fatal(err)
	}
	output := strings.Join(lines, "\n")
	if !strings.Contains(output, "https://github.com/owner/repo/issues/12") {
		t.Fatalf("creation output lacks the new item: %q", output)
	}
	if strings.Contains(output, "early") {
		t.Fatalf("creation output includes an earlier call: %q", output)
	}
	if strings.Contains(output, "later") {
		t.Fatalf("creation output includes later work: %q", output)
	}
}

// Hard-won: b4476dc76: comments a session posted in turns the harness started on its own still came back to it.
func TestTurnsTheHarnessStartsEndWithItsLastWorkBeforeTheNextTurnOrRestart(t *testing.T) {
	path := filepath.Join(t.TempDir(), "harness-turns.sqlite")
	record := session("session-1", "project-1")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
	observations := []struct {
		recordedAtMs int64
		observation  core.RelayObservation
	}{
		{1_000, core.RelayObservationHarnessTurnStarted{StartedAtMs: 1_000}},
		{1_500, githubTestUpdate(t, `{"sessionUpdate":"tool_call","toolCallId":"comment","title":"gh issue comment"}`)},
		{4_000, githubTestUpdate(t, `{"sessionUpdate":"tool_call_update","toolCallId":"comment","status":"completed"}`)},
		{5_000, githubTestAgentMessage(t, "Posted.", "reply")},
		{5_200, core.RelayObservationHarnessTurnSettled{}},
		// A notice while idle is not the agent's work.
		{60_000, core.RelayObservationNotice{Message: "Delivered 1 mailbox event"}},
		{100_000, core.RelayObservationHarnessTurnStarted{StartedAtMs: 100_000}},
		{100_500, githubTestAgentMessage(t, "Working.", "interrupted")},
		{200_000, core.RelayObservationSessionRestarted{}},
		// A message after the restart does not stretch the interrupted turn.
		{300_000, githubTestAgentMessage(t, "Resumed.", "resume-notice")},
		{400_000, core.RelayObservationHarnessTurnStarted{StartedAtMs: 400_000}},
		{400_500, githubTestAgentMessage(t, "Still working.", "running")},
	}
	current, err := LoadMaterializedSessionFrom(path, "session-1")
	if err != nil {
		t.Fatal(err)
	}
	if current == nil {
		t.Fatal("the saved session has no projection")
	}
	_, err = applyProjectionPageTo(path, "session-1", func(page *ProjectionPage) (struct{}, error) {
		for _, observed := range observations {
			event := &core.RelayEvent{
				Format:         core.RelayEventFormatV1,
				Ordinal:        current.AppliedEventOrdinal + 1,
				PreviousDigest: current.AppliedEventDigest,
				RecordedAtMs:   observed.recordedAtMs,
				Observation:    observed.observation,
			}
			digest, err := core.RelayEventDigest(event)
			if err != nil {
				return struct{}{}, err
			}
			event.Digest = digest
			projected, err := transcript.ProjectRelayEvent(current, event)
			if err != nil {
				return struct{}{}, err
			}
			mutation := projected.Mutation
			if _, err := page.Apply(event.Ordinal, event.PreviousDigest, event.Digest, &mutation); err != nil {
				return struct{}{}, err
			}
			if err := transcript.ApplyCommittedProjectionEvent(current, event, mutation); err != nil {
				return struct{}{}, err
			}
		}
		return struct{}{}, nil
	})
	if err != nil {
		t.Fatal(err)
	}
	window := func(startedAtMs int64, completedAtMs *int64) GithubSessionTurnWindow {
		return GithubSessionTurnWindow{StartedAtMs: startedAtMs, CompletedAtMs: completedAtMs}
	}
	at := func(ms int64) *int64 { return &ms }
	windows, err := loadGithubSessionTurnWindowsFrom(path, "session-1")
	if err != nil {
		t.Fatal(err)
	}
	want := []GithubSessionTurnWindow{
		window(1_000, at(5_200)),
		window(100_000, at(200_000)),
		window(400_000, nil),
	}
	if !reflect.DeepEqual(windows, want) {
		t.Fatalf("turn windows = %+v, want %+v", windows, want)
	}
}

// Hard-won: #1505: rows edited after item creation must never leak into historical evidence.
func TestGithubHistorySnapshotRetainsCutoffMetadataAndRawCallOrder(t *testing.T) {
	path := filepath.Join(t.TempDir(), "history.sqlite")
	record := session("session-1", "project-1")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
	connection, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	for _, row := range []struct {
		id               string
		position         int64
		created, changed int64
		body             string
	}{
		{"z", 1, 1000, 1000, `{"kind":"tool","call":{"title":"first","kind":"execute","toolCallId":"c"}}`},
		{"a", 1, 1000, 1000, `{"kind":"user","content":[{"type":"text","text":"before\r\ncutoff"}]}`},
		{"edited", 2, 1000, 3000, `{"kind":"agent","chunks":[{"content":{"type":"text","text":"future secret"}}]}`},
		{"thought", 3, 1000, 3000, `{"kind":"thought"}`},
		{"later", 4, 3000, 3000, `{"kind":"user","content":[]}`},
		{"notice", 5, 1000, 1000, `{"kind":"message","event":{"body":{"type":"new_github_item","number":42,"repo":"OWNER/REPO"}}}`},
	} {
		if _, err := connection.Exec("INSERT INTO materialized_transcript_items(session_id,stable_id,position,created_at_ms,last_changed_at_ms,body_json) VALUES ('session-1',?1,?2,?3,?4,?5)",
			row.id, row.position, row.created, row.changed, row.body); err != nil {
			t.Fatal(err)
		}
	}
	history := transcript.NewSessionHistory[string, *transcript.StringSink]("session-1", []transcript.HistoryPairSpec[*transcript.StringSink]{{
		CutoffMs: 2000,
		Repo:     "owner/repo",
		Number:   42,
		Sink:     &transcript.StringSink{},
	}})
	results, err := scanGithubHistoryFrom(path, "session-1", history, func() bool { return false })
	if err != nil {
		t.Fatal(err)
	}
	rendered := results[len(results)-1].Done
	want := "Session transcript as of 1970-01-01T00:00:02.000Z (UTC)\nSession id: session-1\nTranscript rows included: 2; withheld because changed after cutoff: 2; excluded because created later: 1; item-specific GitHub mailbox notices removed: 1.\nRows come only from Mjolnir materialized transcript records; timestamps are row creation times.\n\n[1970-01-01T00:00:01.000Z] USER\nbefore\ncutoff\n\n[1970-01-01T00:00:01.000Z] TOOL EXECUTE\nTITLE: first\nCALL: {\"title\":\"first\",\"kind\":\"execute\",\"toolCallId\":\"c\"}\n"
	if rendered != want {
		t.Fatalf("rendered history =\n%q\nwant\n%q", rendered, want)
	}
}

func TestHistoryScanCountsLateRowsWithoutReadingThemAndStopsWhenAsked(t *testing.T) {
	path := filepath.Join(t.TempDir(), "late.sqlite")
	record := session("session-1", "project-1")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
	connection, err := openReader(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	for _, row := range []struct {
		id       string
		position int64
		created  int64
		body     string
	}{
		{"a", 1, 1000, `{"kind":"user","content":[{"type":"text","text":"hello"}]}`},
		// Created after every cutoff: only counted, so its body is never parsed.
		{"b", 2, 5000, "{not json"},
		// Never shown, but a pair with an earlier cutoff still counts it as later.
		{"c", 3, 3000, `{"kind":"thought","text":"x"}`},
		// The renderer reads a non-string repo as absent, so this is a notice
		// for #42; json_extract alone would return the object as text.
		{"d", 4, 1000, `{"kind":"message","event":{"body":{"type":"new_github_item","number":42,"repo":{"x":1}}}}`},
	} {
		if _, err := connection.Exec("INSERT INTO materialized_transcript_items(session_id,stable_id,position,created_at_ms,last_changed_at_ms,body_json) VALUES ('session-1',?1,?2,?3,?3,?4)",
			row.id, row.position, row.created, row.body); err != nil {
			t.Fatal(err)
		}
	}
	history := func() *transcript.SessionHistory[string, *transcript.StringSink] {
		var pairs []transcript.HistoryPairSpec[*transcript.StringSink]
		for _, cutoffMs := range []int64{2000, 4000} {
			pairs = append(pairs, transcript.HistoryPairSpec[*transcript.StringSink]{
				CutoffMs: cutoffMs,
				Repo:     "owner/repo",
				Number:   42,
				Sink:     &transcript.StringSink{},
			})
		}
		return transcript.NewSessionHistory[string, *transcript.StringSink]("session-1", pairs)
	}
	// Hard-won: review of 331d49d5f: SQL kind filtering dropped rows of unshown kinds from the later/changed counts of earlier cutoffs.
	rendered, err := scanGithubHistoryFrom(path, "session-1", history(), func() bool { return false })
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(rendered[0].Done, "included: 1; withheld because changed after cutoff: 0; excluded because created later: 2; item-specific GitHub mailbox notices removed: 1.") {
		t.Fatalf("first cutoff header = %q", rendered[0].Done)
	}
	if !strings.Contains(rendered[1].Done, "included: 1; withheld because changed after cutoff: 0; excluded because created later: 1; item-specific GitHub mailbox notices removed: 1.") {
		t.Fatalf("second cutoff header = %q", rendered[1].Done)
	}
	_, err = scanGithubHistoryFrom(path, "session-1", history(), func() bool { return true })
	var cancelled GithubHistoryScanCancelled
	if !errors.As(err, &cancelled) {
		t.Fatalf("scan error = %v, want GithubHistoryScanCancelled", err)
	}
}
