// Port of mj-core/src/github_item.rs.

// Bounded evidence for classifying GitHub issues and pull requests per session.

package core

import (
	"bytes"
	_ "embed"
	"encoding/json"
	"fmt"
)

// GithubItemMaxBodyBytes is MAX_BODY_BYTES (prefixed: collides with continuation).
const GithubItemMaxBodyBytes = 64 * 1024

// GithubItemQuestionsJSON is the checked-in QUESTIONS text (prefixed: collides
// with activity; renamed from the prefixed name because the function
// questions() takes GithubItemQuestions).
//
//go:embed data/github_item/questions_v2.json
var GithubItemQuestionsJSON string

// Relevance evidence budgets; conversation limits are selected by replay.
// Mirrored by the v2 proxy validator.
const (
	ItemBodyBytes     = 4096
	SessionTurns      = 3
	ConversationBytes = 8 * 1024
	MessageBytes      = 2 * 1024
	MaxFiles          = 32
	FileBytes         = 256
	MaxReferences     = 32
	ReferenceBytes    = 128
)

// Design budgets sizing item-conditioned evidence, motivated by the 83-pair replay.
const (
	MaxMentions  = 16
	MentionBytes = 256
)

const bodyTruncationMarker = "\n[GitHub item body truncated.]"

type GithubItemKind string

const (
	GithubItemKindIssue       GithubItemKind = "issue"
	GithubItemKindPullRequest GithubItemKind = "pull_request"
)

func (k *GithubItemKind) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch GithubItemKind(text) {
	case GithubItemKindIssue, GithubItemKindPullRequest:
		*k = GithubItemKind(text)
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `issue` or `pull_request`", text)
}

type GithubItem struct {
	Repo   string         `json:"repo"`
	Kind   GithubItemKind `json:"kind"`
	Number uint64         `json:"number"`
	Title  string         `json:"title"`
	Body   string         `json:"body"`
	Author string         `json:"author"`
	URL    string         `json:"url"`
}

func (g *GithubItem) UnmarshalJSON(data []byte) error {
	type wire GithubItem
	return unmarshalDenyUnknown(data, (*wire)(g))
}

type HistorySnippet struct {
	Time *string `json:"time"`
	Text string  `json:"text"`
}

func (h *HistorySnippet) UnmarshalJSON(data []byte) error {
	type wire HistorySnippet
	return unmarshalDenyUnknown(data, (*wire)(h))
}

// GithubMentionCandidate is one element of SessionContext.mention_candidates,
// a Rust `(GithubReference, String)` tuple.
type GithubMentionCandidate struct {
	Reference GithubReference
	Line      string
}

type SessionContext struct {
	Conversation string            `json:"conversation"`
	Files        []string          `json:"files"`
	References   []string          `json:"references"`
	Mentions     []string          `json:"mentions"`
	History      *[]HistorySnippet `json:"history,omitempty"`
	// Per-session reference index; never included in a provider request.
	MentionCandidates []GithubMentionCandidate `json:"-"`
}

func (s *SessionContext) UnmarshalJSON(data []byte) error {
	type wire SessionContext
	return unmarshalDenyUnknown(data, (*wire)(s))
}

// ForItem selects the lines that mention `item` or an issue its title or body
// references. A bare `#N` in the item means the item's repository. In the
// session it does only when `bareIsItemRepo`: a session whose project spans
// several GitHub repositories cannot say which one `#42` names, and must not
// have the line presented as a mention of every watched repository's #42.
func (s SessionContext) ForItem(item *GithubItem, bareIsItemRepo bool) SessionContext {
	panic("unported: mj-core/src/github_item.rs SessionContext::for_item")
}

// GithubItemEvidence: the GitHub item deliberately serializes before its
// session-specific context, allowing requests for different sessions to share
// the same item prefix.
type GithubItemEvidence struct {
	Item    GithubItem     `json:"item"`
	Session SessionContext `json:"session"`
}

func (e *GithubItemEvidence) UnmarshalJSON(data []byte) error {
	type wire GithubItemEvidence
	return unmarshalDenyUnknown(data, (*wire)(e))
}

type GithubItemVerdict struct {
	Interested bool
}

// githubItemUpstreamBody is the Rust private UpstreamBody.
type githubItemUpstreamBody struct {
	Model     string              `json:"model"`
	State     *GithubItemEvidence `json:"state"`
	Questions json.RawMessage     `json:"questions"`
}

// GithubItemQuestions is questions(): the checked-in questions as a JSON
// value. Raw JSON keeps the file's key order, as serde_json's preserve_order
// Value does.
func GithubItemQuestions() json.RawMessage {
	var compact bytes.Buffer
	if err := json.Compact(&compact, []byte(GithubItemQuestionsJSON)); err != nil {
		panic("checked-in GitHub item questions are valid JSON")
	}
	return compact.Bytes()
}

// BoundedForRequest returns evidence whose complete direct TypeSafe request,
// including the model and questions, fits the shared request limit.
func (e *GithubItemEvidence) BoundedForRequest() (GithubItemEvidence, error) {
	panic("unported: mj-core/src/github_item.rs GithubItemEvidence::bounded_for_request")
}

func (e *GithubItemEvidence) Validate() error {
	panic("unported: mj-core/src/github_item.rs GithubItemEvidence::validate")
}

// UpstreamBody builds the direct-provider envelope without converting the
// ordered evidence structs into a map-backed value.
func (e *GithubItemEvidence) UpstreamBody() any {
	return githubItemUpstreamBody{Model: "jev-latest", State: e, Questions: GithubItemQuestions()}
}

func (e *GithubItemEvidence) validateFixedFields() error {
	panic("unported: mj-core/src/github_item.rs GithubItemEvidence::validate_fixed_fields")
}

// GithubItemVerdictParse is GithubItemVerdict::parse. `response` is a decoded
// JSON value (`any` from encoding/json).
func GithubItemVerdictParse(response any) (GithubItemVerdict, error) {
	panic("unported: mj-core/src/github_item.rs GithubItemVerdict::parse")
}

func upstreamLen(evidence *GithubItemEvidence) (int, error) {
	body, err := json.Marshal(evidence.UpstreamBody())
	if err != nil {
		return 0, err
	}
	return len(body), nil
}

func utf8Prefix(text string, limit int) string {
	end := min(limit, len(text))
	for end > 0 && end < len(text) && !isUTF8Boundary(text[end]) {
		end--
	}
	return text[:end]
}

// isUTF8Boundary reports whether a byte starts a UTF-8 sequence.
func isUTF8Boundary(b byte) bool { return b&0xC0 != 0x80 }

// unmarshalDenyUnknown decodes like serde's `deny_unknown_fields`. Call it
// with a method-free alias of the target type so it does not recurse.
func unmarshalDenyUnknown(data []byte, v any) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode(v)
}

// GithubHistoryTokenizer is the frozen aliases600 tokenizer: it counts tokens
// without truncation or padding. Rust's `tokenizers::Tokenizer::encode` with
// an optional pair and the special-token flag.
type GithubHistoryTokenizer interface {
	Encode(first string, second *string, addSpecialTokens bool) ([]uint32, error)
}

// HistoryTokenizer is the frozen tokenizer for aliases600 evidence budgets,
// without truncation or padding. Built once: parsing the embedded 3.5 MB
// definition on every evidence build was a measurable share of the daemon's
// page faults. Rust builds it only with the `github-history` feature.
func HistoryTokenizer() (GithubHistoryTokenizer, error) {
	panic("unported: mj-core/src/github_item.rs history_tokenizer")
}
