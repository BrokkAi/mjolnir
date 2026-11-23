// Port of the tests in mj-transcript/src/github_history.rs.

package transcript

import (
	"cmp"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"testing"
	"unicode/utf8"
	"unsafe"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func historyParse(t testing.TB, text string) OrderedValue {
	t.Helper()
	var value OrderedValue
	if err := json.Unmarshal([]byte(text), &value); err != nil {
		t.Fatalf("parse %.200s: %v", text, err)
	}
	return value
}

// historyValue builds an OrderedValue from a jsonvalue value, keeping the
// member order as Rust's json! macro writes it.
func historyValue(t testing.TB, value any) OrderedValue {
	t.Helper()
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return historyParse(t, string(encoded))
}

func historySlice(s []rune, a, b int) string {
	return string(s[a:b])
}

// historyRetrieveReference is the implementation that streaming replaced,
// kept only as the oracle for the differential test: whole-text retrieval
// over code points. Original title/body must be supplied, independently of
// the bounded classifier item.
func historyRetrieveReference(item *OrderedValue, text string, session *OrderedValue) []Span {
	t := chars(text)
	var bytePositions []int
	for byteIndex := range text {
		bytePositions = append(bytePositions, byteIndex)
	}
	digest := chars(spaced(session))
	known := map[string]bool{}
	for i := 0; i < max(len(digest)-99, 0); i++ {
		known[historySlice(digest, i, i+100)] = true
	}
	conversation := session.Index("conversation")
	existing, _ := conversation.AsStr()
	type reference struct {
		score float64
		end   int
		ids   map[string]bool
	}
	candidates := map[int]*reference{}
	for _, identifier := range identifiers(item) {
		id := identifier.text
		numeric := true
		for _, c := range id {
			numeric = numeric && core.Contains(core.Isdigit, c)
		}
		var hits []int
		offset := 0
		for {
			p := strings.Index(text[offset:], id)
			if p < 0 {
				break
			}
			b := offset + p
			pos := sort.Search(len(bytePositions), func(i int) bool { return bytePositions[i] >= b })
			end := pos + utf8.RuneCountInString(id)
			offset = b + len(id)
			var ok bool
			if numeric {
				ok = (pos == 0 || !core.Contains(core.Isdigit, t[pos-1])) &&
					(end == len(t) || !core.Contains(core.Isdigit, t[end]))
			} else {
				ok = (pos == 0 || !(core.Contains(core.Isalnum, t[pos-1]) || t[pos-1] == '_')) &&
					(end == len(t) || !core.Contains(core.Isalnum, t[end]))
			}
			if ok {
				hits = append(hits, pos)
			}
		}
		// Rust uses libm's fdlibm log1p; Go's math.Log1p is the same
		// algorithm and gives the same bits on every platform.
		weight := identifier.weight * (1.0 + math.Log1p(max(float64(len(t))/500.0, 1.0)/float64(1+len(hits))))
		for _, pos := range hits {
			start := max(pos/250*250-125, 0)
			c, ok := candidates[start]
			if !ok {
				c = &reference{end: min(start+500, len(t)), ids: map[string]bool{}}
				candidates[start] = c
			}
			if !c.ids[id] {
				c.ids[id] = true
				c.score += weight
			}
		}
	}
	type stamp struct {
		pos  int
		text string
	}
	var stamps []stamp
	for _, m := range re(`(?m)^\[([^\]\n]+)\]`).FindAllStringSubmatchIndex(text, -1) {
		pos := sort.Search(len(bytePositions), func(i int) bool { return bytePositions[i] >= m[0] })
		stamps = append(stamps, stamp{pos, text[m[2]:m[3]]})
	}
	type rankedSpan struct {
		span Span
		len  int
	}
	var ranked []rankedSpan
	starts := make([]int, 0, len(candidates))
	for start := range candidates {
		starts = append(starts, start)
	}
	slices.Sort(starts)
	for _, start := range starts {
		c := candidates[start]
		raw := trimHistorySpace(historySlice(t, start, c.end))
		rt := Redact(raw)
		r := chars(rt)
		duplicate := strings.Contains(existing, rt)
		for i := 0; !duplicate && i < max(len(r)-99, 0); i += 50 {
			duplicate = known[historySlice(r, i, i+100)]
		}
		if duplicate {
			continue
		}
		score := c.score * (1.0 + 0.1*float64(start)/float64(max(len(t), 1)))
		var time *string
		for i := len(stamps) - 1; i >= 0; i-- {
			if stamps[i].pos <= start {
				text := stamps[i].text
				time = &text
				break
			}
		}
		ranked = append(ranked, rankedSpan{
			span: Span{Start: start, End: c.end, Score: score, Snippet: Snippet{Time: time, Text: rt}},
			len:  utf8.RuneCountInString(raw),
		})
	}
	slices.SortStableFunc(ranked, func(a, b rankedSpan) int {
		if order := totalCompare(b.span.Score, a.span.Score); order != 0 {
			return order
		}
		return cmp.Compare(b.span.Start, a.span.Start)
	})
	var selected []Span
	used := 0
	for _, r := range ranked {
		if used+r.len > 2100 || slices.ContainsFunc(selected, func(s Span) bool {
			return r.span.Start < s.End && r.span.End > s.Start
		}) {
			continue
		}
		used += r.len
		selected = append(selected, r.span)
	}
	slices.SortStableFunc(selected, func(a, b Span) int { return cmp.Compare(a.Start, b.Start) })
	return selected
}

// historyRow is a raw persisted row: it retains tool-call JSON order and
// unknown row kinds for cutoff counts.
type historyRow struct {
	stableID        string
	position        int64
	createdAtMs     int64
	lastChangedAtMs int64
	body            OrderedValue
}

func historyOrdered(rows []historyRow) []*historyRow {
	ordered := make([]*historyRow, len(rows))
	for i := range rows {
		ordered[i] = &rows[i]
	}
	slices.SortStableFunc(ordered, func(a, b *historyRow) int {
		if order := cmp.Compare(a.position, b.position); order != 0 {
			return order
		}
		return strings.Compare(a.stableID, b.stableID)
	})
	return ordered
}

// historyRenderReference renders one shared snapshot at the item's creation
// time; never use edited later bodies.
func historyRenderReference(sessionID string, rows []historyRow, cutoffMs int64, repo string, number uint64) (string, error) {
	changed, later, notices := 0, 0, 0
	var chunks []string
	for _, row := range historyOrdered(rows) {
		if row.createdAtMs > cutoffMs {
			later++
			continue
		}
		if row.lastChangedAtMs > cutoffMs {
			changed++
			continue
		}
		b := &row.body
		str := func(v OrderedValue) string {
			text, _ := v.AsStr()
			return text
		}
		var rendered *string
		kindValue := b.Index("kind")
		kind, _ := kindValue.AsStr()
		switch kind {
		case "user":
			var parts []string
			content := b.Index("content")
			items, _ := content.AsArray()
			for _, x := range items {
				if typ := x.Index("type"); typ.EqStr("text") {
					parts = append(parts, str(x.Index("text")))
				}
			}
			text := "USER\n" + strings.Join(parts, "\n")
			rendered = &text
		case "agent":
			var joined strings.Builder
			chunksValue := b.Index("chunks")
			items, _ := chunksValue.AsArray()
			for _, x := range items {
				content := x.Index("content")
				if typ := content.Index("type"); typ.EqStr("text") {
					joined.WriteString(str(content.Index("text")))
				}
			}
			text := "AGENT\n" + joined.String()
			rendered = &text
		case "tool":
			call := b.Index("call")
			callKindValue := call.Index("kind")
			callKind, ok := callKindValue.AsStr()
			if !ok || callKind == "" {
				callKind = "call"
			}
			text := fmt.Sprintf("TOOL %s\nTITLE: %s\nCALL: %s", strings.ToUpper(callKind), str(call.Index("title")), call.String())
			rendered = &text
		case "message":
			event := b.Index("event")
			body := event.Index("body")
			typValue := body.Index("type")
			typ, _ := typValue.AsStr()
			numberValue, repoValue := body.Index("number"), body.Index("repo")
			itemNumber, numberOK := numberValue.AsU64()
			itemRepo, repoOK := repoValue.AsStr()
			if (typ == "new_github_item" || strings.HasPrefix(typ, "github_")) &&
				numberOK && itemNumber == number &&
				(!repoOK || strings.ToLower(itemRepo) == strings.ToLower(repo)) {
				notices++
				continue
			}
			from := body.Index("from")
			text := fmt.Sprintf("PEER MESSAGE %s\n%s", str(from.Index("title")), str(body.Index("text")))
			rendered = &text
		}
		if rendered != nil {
			created, err := timestamp(row.createdAtMs)
			if err != nil {
				return "", err
			}
			chunks = append(chunks, fmt.Sprintf("[%s] %s", created, *rendered))
		}
	}
	cutoff, err := timestamp(cutoffMs)
	if err != nil {
		return "", err
	}
	text := fmt.Sprintf("Session transcript as of %s (UTC)\nSession id: %s\nTranscript rows included: %d; withheld because changed after cutoff: %d; excluded because created later: %d; item-specific GitHub mailbox notices removed: %d.\nRows come only from Mjolnir materialized transcript records; timestamps are row creation times.\n\n%s\n",
		cutoff, sessionID, len(chunks), changed, later, notices, strings.Join(chunks, "\n\n"))
	return strings.ReplaceAll(strings.ReplaceAll(text, "\r\n", "\n"), "\r", "\n"), nil
}

// historyMemoryRows runs both passes of the streaming renderer over rows
// held in memory, the way the store feeds them.
type historyMemoryRows []string

func (m historyMemoryRows) Body(key int64) (string, error) {
	return m[key], nil
}

func historyStreamPairs[D any, S HistorySink[D]](t testing.TB, sessionID string, rows []historyRow, pairs []HistoryPairSpec[S]) []HistoryResult[D] {
	t.Helper()
	ordered := historyOrdered(rows)
	memory := make(historyMemoryRows, len(ordered))
	for i, row := range ordered {
		encoded, err := row.body.MarshalJSON()
		if err != nil {
			t.Fatal(err)
		}
		memory[i] = string(encoded)
	}
	history := NewSessionHistory[D, S](sessionID, pairs)
	for _, row := range ordered {
		b := &row.body
		eventValue := b.Index("event")
		event := eventValue.Index("body")
		kindValue, typValue := b.Index("kind"), event.Index("type")
		numberValue, repoValue := event.Index("number"), event.Index("repo")
		kind, _ := kindValue.AsStr()
		typ, _ := typValue.AsStr()
		meta := RowMeta{
			CreatedAtMs:     row.createdAtMs,
			LastChangedAtMs: row.lastChangedAtMs,
			Kind:            kind,
			EventType:       typ,
		}
		if number, ok := numberValue.AsU64(); ok {
			meta.EventNumber = &number
		}
		if repo, ok := repoValue.AsStr(); ok {
			meta.EventRepo = &repo
		}
		history.CountRow(&meta)
	}
	if err := history.Begin(); err != nil {
		t.Fatal(err)
	}
	for n, row := range ordered {
		if err := history.Row(int64(n), row.createdAtMs, row.lastChangedAtMs, memory[n]); err != nil {
			t.Fatal(err)
		}
	}
	results, err := history.Complete(memory, func() error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	return results
}

func historyStreamRender(t testing.TB, sessionID string, rows []historyRow, cutoffMs int64, repo string, number uint64) string {
	t.Helper()
	results := historyStreamPairs[string, *StringSink](t, sessionID, rows, []HistoryPairSpec[*StringSink]{{
		CutoffMs: cutoffMs,
		Repo:     repo,
		Number:   number,
		Sink:     &StringSink{},
	}})
	return results[len(results)-1].Done
}

// historyRng is the xorshift generator of the Rust test.
type historyRng struct{ state uint64 }

func (r *historyRng) next() uint64 {
	r.state ^= r.state << 13
	r.state ^= r.state >> 7
	r.state ^= r.state << 17
	return r.state
}

func (r *historyRng) below(n int) int {
	return int(r.next() % uint64(n))
}

var historyPieces = []string{
	"#42",
	"#420",
	"x#42",
	"#42y",
	"#4",
	"42",
	"src/foo_bar.rs",
	"foo_bar.rs",
	"foo_bar",
	"foo_bar_baz_qux",
	"FooBar",
	"fooBar_x",
	"CAFÉ",
	"CAFE",
	"AB",
	"ABC",
	"ABCD",
	"é",
	"😀",
	"日本語",
	"²",
	"٣",
	"_",
	".",
	"-",
	"/",
	" ",
	" ",
	" ",
	"word",
	"words",
	"\n",
	"\n",
	"\r\n",
	"\r",
	"\r\n\r\n",
	"\n[2024-01-01 10:00]\n",
	"\n[x",
	"]",
	"[]",
	"\n[a\nb]",
	"\u00a0",
	"\t",
	"tokenaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
	"https://example.com/src/foo_bar.rs",
	"the quick brown fox jumps over the lazy dog ",
}

func historyRandomText(rng *historyRng, pieces int) string {
	var text strings.Builder
	for range pieces {
		text.WriteString(historyPieces[rng.below(len(historyPieces))])
		if rng.below(6) == 0 {
			text.WriteString(strings.Repeat("lorem ipsum ", rng.below(60)))
		}
	}
	return text.String()
}

func historyRandomRows(t testing.TB, rng *historyRng, count int) []historyRow {
	rows := make([]historyRow, 0, count)
	for n := range count {
		var text string
		if rng.below(40) == 0 {
			// More than a pipe buffer in one row.
			text = historyRandomText(rng, 30) + strings.Repeat("padding ", 9000)
		} else {
			pieces := 1 + rng.below(40)
			text = historyRandomText(rng, pieces)
		}
		created := 500 + int64(rng.below(1200))
		changed := created
		if rng.below(6) == 0 {
			changed = created + int64(rng.below(900))
		}
		var body jsonvalue.Object
		switch rng.below(7) {
		case 0:
			body = jsonvalue.Object{
				{Key: "kind", Value: "user"},
				{Key: "content", Value: []any{
					jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: text}},
					jsonvalue.Object{{Key: "type", Value: "image"}},
					jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: "#42"}},
				}},
			}
		case 1:
			body = jsonvalue.Object{
				{Key: "kind", Value: "agent"},
				{Key: "chunks", Value: []any{
					jsonvalue.Object{{Key: "content", Value: jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: text}}}},
					jsonvalue.Object{{Key: "content", Value: jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: "src/foo_bar.rs"}}}},
				}},
			}
		case 2:
			kind := ""
			if rng.below(2) == 0 {
				kind = "execute"
			}
			body = jsonvalue.Object{
				{Key: "kind", Value: "tool"},
				{Key: "call", Value: jsonvalue.Object{
					{Key: "title", Value: text},
					{Key: "kind", Value: kind},
					{Key: "rawInput", Value: jsonvalue.Object{
						{Key: "n", Value: 1.5e300},
						{Key: "z", Value: 0.1},
						{Key: "t", Value: "#42\r\n"},
					}},
					{Key: "toolCallId", Value: "c"},
				}},
			}
		case 3, 4:
			typ := []string{"github_issue", "new_github_item", "peer", "github_comment"}[rng.below(4)]
			repo := []any{"Owner/Repo", "other/repo", nil, 7}[rng.below(4)]
			number := []any{42, 43, 42.0, "42"}[rng.below(4)]
			body = jsonvalue.Object{
				{Key: "kind", Value: "message"},
				{Key: "event", Value: jsonvalue.Object{
					{Key: "body", Value: jsonvalue.Object{
						{Key: "type", Value: typ},
						{Key: "number", Value: number},
						{Key: "repo", Value: repo},
						{Key: "text", Value: text},
						{Key: "from", Value: jsonvalue.Object{{Key: "title", Value: "peer"}}},
					}},
				}},
			}
		case 5:
			body = jsonvalue.Object{{Key: "kind", Value: "thought"}, {Key: "text", Value: text}}
		default:
			body = jsonvalue.Object{{Key: "kind", Value: 5}, {Key: "x", Value: text}}
		}
		rows = append(rows, historyRow{
			stableID:        fmt.Sprintf("row-%d", rng.below(count+3)),
			position:        int64(n) / 2,
			createdAtMs:     created,
			lastChangedAtMs: changed,
			body:            historyValue(t, body),
		})
	}
	return rows
}

func historyRandomItem(t testing.TB, rng *historyRng) OrderedValue {
	title := historyRandomText(rng, 4)
	first := historyRandomText(rng, 8)
	second := historyRandomText(rng, 8)
	body := fmt.Sprintf("%s #42 src/foo_bar.rs foo_bar_baz_qux FooBar CAFÉ ABC %s", first, second)
	return historyValue(t, jsonvalue.Object{{Key: "title", Value: title}, {Key: "body", Value: body}})
}

func historySpansJSON(spans []Span) string {
	lines := make([]string, len(spans))
	for i, s := range spans {
		time := "None"
		if s.Snippet.Time != nil {
			time = fmt.Sprintf("Some(%q)", *s.Snippet.Time)
		}
		lines[i] = fmt.Sprintf("%d:%d:%016x:%s:%q", s.Start, s.End, math.Float64bits(s.Score), time, s.Snippet.Text)
	}
	return strings.Join(lines, "\n")
}

func historyFinishWhole(t testing.TB, scanner *HistoryScanner, text string) []Span {
	t.Helper()
	selected, err := scanner.Finish(func() error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	return spansFromWholeText(selected, text)
}

// historyPushPieces pushes text in pieces of the given size, each extended
// to a character boundary.
func historyPushPieces(scanner *HistoryScanner, text string, size func() int) {
	for rest := text; rest != ""; {
		cut := min(size(), len(rest))
		for cut < len(rest) && !utf8.RuneStart(rest[cut]) {
			cut++
		}
		scanner.PushStr(rest[:cut])
		rest = rest[cut:]
	}
}

// An identifier in every window of a long transcript, with a megabyte-long
// bracketed line that opens a stamp under hundreds of windows, must not make
// the scanner keep a string per window.
func TestScannerHoldsAConstantNumberOfWindowStrings(t *testing.T) {
	item := historyParse(t, `{"title": "t", "body": "src/zz_marker.rs"}`)
	session := historyParse(t, `{"conversation": ""}`)
	var text strings.Builder
	text.WriteString("[2024-01-01T00:00:00.000Z] USER\n")
	for range 4000 {
		text.WriteString("src/zz_marker.rs lorem ipsum dolor sit amet ééé 😀 consectetur\n")
	}
	text.WriteString("\n[")
	for range 40_000 {
		text.WriteString("src/zz_marker.rs padding padding padding ")
	}
	text.WriteString("]\n")
	for range 4000 {
		text.WriteString("lorem src/zz_marker.rs ipsum dolor sit amet\n")
	}
	whole := text.String()
	if len(whole) <= 1_000_000 {
		t.Fatalf("transcript of %d bytes", len(whole))
	}
	scanner := NewHistoryScanner(&item, &session)
	historyPushPieces(scanner, whole, func() int { return 4096 })
	if len(scanner.live) <= 8000 {
		t.Fatalf("%d live windows", len(scanner.live))
	}
	if size := unsafe.Sizeof(candidate{}); size != 24 {
		t.Fatalf("a candidate takes %d bytes", size)
	}
	if size := unsafe.Sizeof(rowInfo{}); size != 24 {
		t.Fatalf("a row takes %d bytes", size)
	}
	if scanner.peakTexts > 3 || len(scanner.open) > 3 {
		t.Fatalf("peak %d, open %d", scanner.peakTexts, len(scanner.open))
	}
	expected := historyRetrieveReference(&item, whole, &session)
	if len(expected) == 0 {
		t.Fatal("the reference retrieved nothing")
	}
	if got, want := historySpansJSON(historyFinishWhole(t, scanner, whole)), historySpansJSON(expected); got != want {
		t.Fatalf("spans differ:\n%s\n---\n%s", got, want)
	}
}

// Streaming must reproduce the whole-text implementation exactly: header
// counts, line-ending replacement, notices, rows that changed after the
// cutoff, multi-byte text, identifiers across the window grid and the row
// boundaries, bracket lines, and rows larger than a pipe buffer.
func TestStreamingHistoryMatchesTheWholeTextReference(t *testing.T) {
	rng := &historyRng{state: 0x9e3779b97f4a7c15}
	retrieved := 0
	for round := range 30 {
		count := 1 + rng.below(40)
		rows := historyRandomRows(t, rng, count)
		item := historyRandomItem(t, rng)
		conversation := ""
		if round%3 == 0 {
			conversation = "lorem ipsum lorem ipsum"
		}
		references := []any{}
		if round%2 == 0 {
			references = []any{strings.Repeat("lorem ipsum ", 30)}
		}
		session := historyValue(t, jsonvalue.Object{
			{Key: "conversation", Value: conversation},
			{Key: "files", Value: []any{}},
			{Key: "references", Value: references},
		})
		pairs := []struct {
			cutoffMs int64
			repo     string
			number   uint64
		}{
			{1200, "owner/repo", 42},
			{900, "OWNER/REPO", 42},
			{700, "owner/repo", 43},
		}
		specs := make([]HistoryPairSpec[*HistoryScanner], len(pairs))
		for i, pair := range pairs {
			specs[i] = HistoryPairSpec[*HistoryScanner]{
				CutoffMs: pair.cutoffMs,
				Repo:     pair.repo,
				Number:   pair.number,
				Sink:     NewHistoryScanner(&item, &session),
			}
		}
		outputs := historyStreamPairs[struct{}, *HistoryScanner](t, "session-é", rows, specs)
		for i, result := range outputs {
			pair := pairs[i]
			text, err := historyRenderReference("session-é", rows, pair.cutoffMs, pair.repo, pair.number)
			if err != nil {
				t.Fatal(err)
			}
			expected := historyRetrieveReference(&item, text, &session)
			retrieved += len(expected)
			if got, want := historySpansJSON(result.Spans), historySpansJSON(expected); got != want {
				t.Fatalf("round %d pair %d %d:\n%s\n---\n%s", round, pair.cutoffMs, pair.number, got, want)
			}
			if got := historyStreamRender(t, "session-é", rows, pair.cutoffMs, pair.repo, pair.number); got != text {
				t.Fatalf("round %d text:\n%q\n---\n%q", round, got, text)
			}
			// Pushing the text in arbitrary pieces changes nothing.
			scanner := NewHistoryScanner(&item, &session)
			historyPushPieces(scanner, text, func() int { return 1 + rng.below(700) })
			if got, want := historySpansJSON(historyFinishWhole(t, scanner, text)), historySpansJSON(expected); got != want {
				t.Fatalf("round %d pieces:\n%s\n---\n%s", round, got, want)
			}
		}
	}
	if retrieved <= 100 {
		t.Fatalf("the generator must produce retrievals; got %d", retrieved)
	}
}

func historyFixture(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", "tests", "fixtures", "github-history", name))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

// Hard-won: #1505: JSON float exponents and notation differed from Python tool rendering.
func TestGithubHistoryPythonToolNumbers(t *testing.T) {
	fixture := historyParse(t, string(historyFixture(t, "tool-numbers.json")))
	row := historyRow{
		stableID:        "tool",
		position:        1,
		createdAtMs:     1000,
		lastChangedAtMs: 1000,
		body:            fixture.Index("body"),
	}
	expectedValue := fixture.Index("expected")
	expected, _ := expectedValue.AsStr()
	if got := historyStreamRender(t, "floats", []historyRow{row}, 2000, "owner/repo", 42); got != expected {
		t.Fatalf("rendered:\n%q\nexpected:\n%q", got, expected)
	}
}

// historySnippetsJSON is serde_json::to_string of the spans' snippets.
func historySnippetsJSON(t testing.TB, spans []Span) string {
	t.Helper()
	values := make([]any, len(spans))
	for i, span := range spans {
		var time any
		if span.Snippet.Time != nil {
			time = *span.Snippet.Time
		}
		values[i] = jsonvalue.Object{{Key: "time", Value: time}, {Key: "text", Value: span.Snippet.Text}}
	}
	encoded, err := jsonvalue.Marshal(values)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

// Hard-won: #1505: Rust word/digit categories differ from the training Python interpreter.
func TestGithubHistoryPythonUnicodeBoundaries(t *testing.T) {
	f := historyParse(t, string(historyFixture(t, "unicode-adversarial.json")))
	item := f.Index("item")
	ids := identifiers(&item)
	pairs := make([]any, len(ids))
	for i, id := range ids {
		pairs[i] = []any{id.text, id.weight}
	}
	encoded, err := jsonvalue.Marshal(pairs)
	if err != nil {
		t.Fatal(err)
	}
	if want := f.Index("identifiers"); string(encoded) != want.String() {
		t.Fatalf("identifiers:\n%s\n%s", encoded, want.String())
	}
	transcriptValue := f.Index("transcript")
	transcriptText, _ := transcriptValue.AsStr()
	session := f.Index("session")
	raw := Retrieve(&item, transcriptText, &session)
	if got, want := historySnippetsJSON(t, raw), f.Index("snippets"); got != want.String() {
		t.Fatalf("snippets:\n%s\n%s", got, want.String())
	}
}
