// Port of mj-github-relevance/src/github_history.rs.

// Budget the retrieved GitHub history for the aliases600 classifier.

package githubrelevance

import (
	"cmp"
	"encoding/json"
	"errors"
	"math"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// historyTokenizer is the frozen tokenizer for aliases600 evidence budgets,
// without truncation or padding. Built once: parsing the embedded 3.5 MB
// definition on every evidence build was a measurable share of the daemon's
// page faults.
//
// The Go port of the tokenizer and its embedded ettin-tokenizer.json still
// live in core (github_item.go), where they were ported before Rust moved
// them into this crate.
func historyTokenizer() (core.GithubHistoryTokenizer, error) {
	return core.HistoryTokenizer()
}

type HistoryEvidence struct {
	Evidence         transcript.OrderedValue
	HistoryTokens    int
	TotalTokens      int
	BaseTokens       int
	OriginalSnippets int
	Retained         []transcript.Span
	Removed          []HistoryRemoval
	OverLimit        bool
}

// HistoryRemoval is a span the budget removed: its start and why,
// "history_budget" or "input_capacity" (Rust's (usize, &'static str)).
type HistoryRemoval struct {
	Start  int
	Reason string
}

// Evidence budgets the history retrieved from text into base. Base evidence
// is already bounded/redacted exactly once; this only budgets history.
func Evidence(originalItem *transcript.OrderedValue, text string, base *transcript.OrderedValue) (HistoryEvidence, error) {
	session := base.Index("session")
	return EvidenceFromSpans(base, transcript.Retrieve(originalItem, text, &session))
}

// EvidenceFromSpans budgets already retrieved spans into base; the
// transcript is used only through them.
func EvidenceFromSpans(base *transcript.OrderedValue, work []transcript.Span) (HistoryEvidence, error) {
	tokenizer, err := historyTokenizer()
	if err != nil {
		return HistoryEvidence{}, err
	}
	count := func(a string, b *string, special bool) (int, error) {
		first, second := a, (*string)(nil)
		if b != nil {
			first, second = tokenizersInput(a, *b)
		}
		ids, err := tokenizer.Encode(first, second, special)
		if err != nil {
			return 0, err
		}
		return len(ids), nil
	}
	// Rust owns work; the caller's spans stay as they were.
	work = slices.Clone(work)
	originalSnippets := len(work)
	for i := range work {
		if work[i].Snippet.Time != nil {
			redacted := transcript.Redact(*work[i].Snippet.Time)
			work[i].Snippet.Time = &redacted
		}
	}
	sessionValue := base.Index("session")
	source, ok := sessionValue.AsObject()
	if !ok {
		return HistoryEvidence{}, errors.New("session must be object")
	}
	var session []transcript.OrderedMember
	has := func(key string) bool {
		return slices.ContainsFunc(session, func(member transcript.OrderedMember) bool { return member.Key == key })
	}
	for _, key := range []string{"conversation", "files", "references", "mentions"} {
		if value, ok := orderedGet(source, key); ok {
			session = append(session, transcript.OrderedMember{Key: key, Value: value})
		}
	}
	var extra []string
	for _, member := range source {
		if !has(member.Key) && member.Key != "history" {
			extra = append(extra, member.Key)
		}
	}
	slices.Sort(extra)
	for _, key := range extra {
		value, _ := orderedGet(source, key)
		session = append(session, transcript.OrderedMember{Key: key, Value: value})
	}
	object := func(members []transcript.OrderedMember) transcript.OrderedValue {
		return transcript.OrderedValue{Kind: transcript.OrderedValueObject, Object: slices.Clone(members)}
	}
	itemValue := base.Index("item")
	item := itemValue.String()
	sessionText := object(session).String()
	baseTokens, err := count(item, &sessionText, true)
	if err != nil {
		return HistoryEvidence{}, err
	}
	var removed []HistoryRemoval
	for {
		snippets := make([]core.HistorySnippet, 0, len(work))
		for _, span := range work {
			snippets = append(snippets, span.Snippet)
		}
		encoded, err := serdeJSON(snippets)
		if err != nil {
			return HistoryEvidence{}, err
		}
		var history transcript.OrderedValue
		if err := json.Unmarshal(encoded, &history); err != nil {
			return HistoryEvidence{}, err
		}
		historyTokens, err := count(history.String(), nil, false)
		if err != nil {
			return HistoryEvidence{}, err
		}
		// IndexMap::insert: a known key keeps its place.
		if index := slices.IndexFunc(session, func(member transcript.OrderedMember) bool { return member.Key == "history" }); index >= 0 {
			session[index].Value = history
		} else {
			session = append(session, transcript.OrderedMember{Key: "history", Value: history})
		}
		sessionText := object(session).String()
		total, err := count(item, &sessionText, true)
		if err != nil {
			return HistoryEvidence{}, err
		}
		if len(work) == 0 || (historyTokens <= 800 && total <= 7999) {
			return HistoryEvidence{
				Evidence: object([]transcript.OrderedMember{
					{Key: "item", Value: itemValue},
					{Key: "session", Value: object(session)},
				}),
				HistoryTokens:    historyTokens,
				TotalTokens:      total,
				BaseTokens:       baseTokens,
				OriginalSnippets: originalSnippets,
				Retained:         work,
				Removed:          removed,
				OverLimit:        total > 7999,
			}, nil
		}
		worst := 0
		for candidate := 1; candidate < len(work); candidate++ {
			order := totalCmp(work[candidate].Score, work[worst].Score)
			if order < 0 || (order == 0 && work[candidate].Start < work[worst].Start) {
				worst = candidate
			}
		}
		reason := "input_capacity"
		if historyTokens > 800 {
			reason = "history_budget"
		}
		removed = append(removed, HistoryRemoval{Start: work[worst].Start, Reason: reason})
		work = slices.Delete(work, worst, worst+1)
	}
}

// orderedGet is IndexMap::get on an object's members.
func orderedGet(members []transcript.OrderedMember, key string) (transcript.OrderedValue, bool) {
	for _, member := range members {
		if member.Key == key {
			return member.Value, true
		}
	}
	return transcript.OrderedValue{}, false
}

// totalCmp is f64::total_cmp: IEEE 754 totalOrder.
func totalCmp(a, b float64) int {
	left := int64(math.Float64bits(a))
	right := int64(math.Float64bits(b))
	left ^= int64(uint64(left>>63) >> 1)
	right ^= int64(uint64(right>>63) >> 1)
	return cmp.Compare(left, right)
}

// tokenizersInput is the pair input of core.GithubHistoryTokenizer.Encode
// (Rust's tokenizers::EncodeInput from (a, b)).
func tokenizersInput(a, b string) (string, *string) {
	return a, &b
}

// BuildEvidence builds classifier-ready evidence from the spans a scanner
// retrieved. Diagnostics retain overflow rather than trimming base.
func BuildEvidence(base *core.GithubItemEvidence, spans []transcript.Span) (core.GithubItemEvidence, HistoryEvidence, error) {
	encoded, err := json.Marshal(base)
	if err != nil {
		return core.GithubItemEvidence{}, HistoryEvidence{}, err
	}
	var baseValue transcript.OrderedValue
	if err := json.Unmarshal(encoded, &baseValue); err != nil {
		return core.GithubItemEvidence{}, HistoryEvidence{}, err
	}
	result, err := EvidenceFromSpans(&baseValue, spans)
	if err != nil {
		return core.GithubItemEvidence{}, HistoryEvidence{}, err
	}
	var typed core.GithubItemEvidence
	if err := json.Unmarshal([]byte(result.Evidence.String()), &typed); err != nil {
		return core.GithubItemEvidence{}, HistoryEvidence{}, err
	}
	return typed, result, nil
}
