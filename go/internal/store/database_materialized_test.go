// Port of the tests in mj-store/src/database/materialized.rs.

package store

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestTranscriptPageAndOldUnsettledScanUseMetadataIndexes(t *testing.T) {
	path := filepath.Join(t.TempDir(), "mj.sqlite3")
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	explain := func(query string, arguments ...any) string {
		t.Helper()
		rows, err := connection.Query("EXPLAIN QUERY PLAN "+query, arguments...)
		if err != nil {
			t.Fatal(err)
		}
		defer rows.Close()
		var details []string
		for rows.Next() {
			var id, parent, unused any
			var detail string
			if err := rows.Scan(&id, &parent, &unused, &detail); err != nil {
				t.Fatal(err)
			}
			details = append(details, detail)
		}
		if err := rows.Err(); err != nil {
			t.Fatal(err)
		}
		return strings.Join(details, "\n")
	}
	page := explain(filteredTranscriptPageSQL(), "session", 0, 10, "[]", 1, 20)
	if !strings.Contains(page, "materialized_transcript_update_ordinal") {
		t.Fatal(page)
	}
	if strings.Contains(page, "TEMP B-TREE") {
		t.Fatal(page)
	}
	unsettled := explain(actorUnsettledItemsSQL, "session", 20)
	if !strings.Contains(unsettled, "materialized_transcript_unsettled") {
		t.Fatal(unsettled)
	}
	if strings.Contains(unsettled, "SCAN") || strings.Contains(unsettled, "TEMP B-TREE") {
		t.Fatal(unsettled)
	}
	ranges := explain(actorUnsettledTurnsSQL(), "session", 20)
	if !strings.Contains(ranges, "materialized_transcript_unsettled") {
		t.Fatal(ranges)
	}
	if strings.Contains(ranges, "TEMP B-TREE") {
		t.Fatal(ranges)
	}
}

func TestLoadToolCallDetailReadsInlineSpillsAndReportsMissingOrCorruptFrames(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "hel.sqlite3")
	sessionID := "session-detail"
	stableID := "terminal:parked"
	record := TestSession(sessionID, "project")
	if err := SaveSessionTo(path, &record); err != nil {
		t.Fatal(err)
	}
	exitCode := uint32(1)
	full := core.TranscriptBodyTerminalOutputOf(core.TerminalOutputRecord{
		TerminalID: "parked",
		Output:     core.NewPackedText(strings.Repeat("captured output\n", 8192)),
		ExitCode:   &exitCode,
	})
	projection := core.MaterializedSessionEmpty(sessionID)
	projection.AppliedEventOrdinal = 1
	projection.AppliedEventDigest = strings.Repeat("a", 64)
	projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
		StableID:        stableID,
		Position:        1,
		CreatedAtMs:     100,
		LastChangedAtMs: 100,
		Body:            full,
	})
	if err := SaveMaterializedSessionTo(path, &projection); err != nil {
		t.Fatal(err)
	}
	lookup := func() core.ToolCallDetailResult {
		t.Helper()
		result, err := LoadToolCallDetailFrom(path, sessionID, stableID)
		if err != nil {
			t.Fatal(err)
		}
		return result
	}
	if missing, err := LoadToolCallDetailFrom(path, sessionID, "missing"); err != nil || missing != nil {
		t.Fatalf("missing item = %#v, %v", missing, err)
	}
	if got, want := lookup(), core.ToolCallDetailResult(core.ToolCallDetailResultFull{Body: full}); !reflect.DeepEqual(got, want) {
		t.Fatalf("inline detail = %#v, want %#v", got, want)
	}

	spilled, err := SpillAndTrimSettledToolsThroughFrom(path, sessionID, 1)
	if err != nil {
		t.Fatal(err)
	}
	if len(spilled) != 1 {
		t.Fatalf("expected one spill, got %d", len(spilled))
	}
	if spilled[0].StableID != stableID {
		t.Fatalf("spilled %s, want %s", spilled[0].StableID, stableID)
	}
	summary := spilled[0].Body
	if !core.TranscriptBodyDetailTrimmed(summary) {
		t.Fatal("the spilled summary is not marked trimmed")
	}
	if got, want := lookup(), core.ToolCallDetailResult(core.ToolCallDetailResultFull{Body: full}); !reflect.DeepEqual(got, want) {
		t.Fatalf("spilled detail = %#v, want %#v", got, want)
	}
	framePath := filepath.Join(directory, "tool-details", "session-detail.mjtd")
	if err := os.WriteFile(framePath, []byte("corrupt spill"), 0o644); err != nil {
		t.Fatal(err)
	}
	unavailable, ok := lookup().(core.ToolCallDetailResultUnavailable)
	if !ok || !reflect.DeepEqual(unavailable.Summary, summary) {
		t.Fatalf("corrupt frame detail = %#v", unavailable)
	}
	if err := os.Remove(framePath); err != nil {
		t.Fatal(err)
	}
	unavailable, ok = lookup().(core.ToolCallDetailResultUnavailable)
	if !ok || !reflect.DeepEqual(unavailable.Summary, summary) || !strings.Contains(unavailable.Reason, ".mjtd") {
		t.Fatalf("missing frame detail = %#v", unavailable)
	}
}
