// Port of the tests in mj-github-relevance/src/github_history.rs.

package githubrelevance

import (
	"bufio"
	_ "embed"
	"encoding/json"
	"fmt"
	"math"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

//go:embed testdata/tests/fixtures/github-history/reference.jsonl
var githubHistoryReference string

func TestGithubHistoryPythonFormatFixtures(t *testing.T) {
	scanner := bufio.NewScanner(strings.NewReader(githubHistoryReference))
	scanner.Buffer(nil, 1<<26)
	for scanner.Scan() {
		var f transcript.OrderedValue
		if err := json.Unmarshal(scanner.Bytes(), &f); err != nil {
			t.Fatal(err)
		}
		id := f.Index("pid").String()
		item := f.Index("item")
		transcriptValue := f.Index("transcript_text")
		transcriptText, _ := transcriptValue.AsStr()
		bounded := f.Index("bounded_evidence_without_history")
		session := bounded.Index("session")
		raw := transcript.Retrieve(&item, transcriptText, &session)
		snippets := make([]core.HistorySnippet, 0, len(raw))
		for _, span := range raw {
			snippets = append(snippets, span.Snippet)
		}
		retrieved, err := serdeJSON(snippets)
		if err != nil {
			t.Fatal(err)
		}
		if got, want := string(retrieved), orderedStr(f, "expected_retrieved_json"); got != want {
			t.Errorf("lexical %s:\n got %s\nwant %s", id, got, want)
		}
		var prepared core.GithubItemEvidence
		if err := json.Unmarshal([]byte(orderedStr(f, "expected_evidence_json")), &prepared); err != nil {
			t.Fatal(err)
		}
		itemSegment, sessionSegment, err := transcript.StudentSegments(&prepared)
		if err != nil {
			t.Fatal(err)
		}
		if want := orderedStr(f, "expected_item_segment"); itemSegment != want {
			t.Errorf("item segment %s:\n got %s\nwant %s", id, itemSegment, want)
		}
		if want := orderedStr(f, "expected_session_segment"); sessionSegment != want {
			t.Errorf("session segment %s:\n got %s\nwant %s", id, sessionSegment, want)
		}
		expectedSpansValue := f.Index("expected_raw_spans")
		expectedSpans, _ := expectedSpansValue.AsArray()
		for i := 0; i < len(raw) && i < len(expectedSpans); i++ {
			span, expected := raw[i], expectedSpans[i]
			start, end := expected.Index("start"), expected.Index("end")
			wantStart, _ := start.AsU64()
			wantEnd, _ := end.AsU64()
			if uint64(span.Start) != wantStart || uint64(span.End) != wantEnd {
				t.Errorf("span %s: %d..%d, want %d..%d", id, span.Start, span.End, wantStart, wantEnd)
			}
			if got, want := fmt.Sprintf("%016x", math.Float64bits(span.Score)), orderedStr(expected, "score_bits"); got != want {
				t.Errorf("score %s: %s, want %s", id, got, want)
			}
		}
		actual, err := Evidence(&item, transcriptText, &bounded)
		if err != nil {
			t.Fatal(err)
		}
		actualSession := actual.Evidence.Index("session")
		if got, want := actualSession.Index("history").String(), orderedStr(f, "expected_history_json"); got != want {
			t.Errorf("budget %s:\n got %s\nwant %s", id, got, want)
		}
		if got, want := actual.Evidence.String(), orderedStr(f, "expected_evidence_json"); got != want {
			t.Errorf("evidence %s:\n got %s\nwant %s", id, got, want)
		}
		stats := f.Index("expected_stats")
		historyTokens, totalTokens := stats.Index("history_tokens"), stats.Index("total_tokens")
		if want, _ := historyTokens.AsU64(); uint64(actual.HistoryTokens) != want {
			t.Errorf("tokens %s: %d, want %d", id, actual.HistoryTokens, want)
		}
		if want, _ := totalTokens.AsU64(); uint64(actual.TotalTokens) != want {
			t.Errorf("pair tokens %s: %d, want %d", id, actual.TotalTokens, want)
		}
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
}

// orderedStr is value[key].as_str().unwrap().
func orderedStr(value transcript.OrderedValue, key string) string {
	member := value.Index(key)
	text, ok := member.AsStr()
	if !ok {
		panic(fmt.Sprintf("fixture %s is not a string", key))
	}
	return text
}
