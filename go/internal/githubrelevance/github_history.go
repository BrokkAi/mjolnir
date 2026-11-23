// Port of mj-github-relevance/src/github_history.rs.

// Budget the retrieved GitHub history for the aliases600 classifier.

package githubrelevance

import (
	"encoding/json"

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
	panic("unported: mj-github-relevance/src/github_history.rs evidence_from_spans")
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
