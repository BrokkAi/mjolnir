// Port of mj-core/src/github_item.rs.

// Bounded evidence for classifying GitHub issues and pull requests per session.

package core

import (
	"bytes"
	"container/heap"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"slices"
	"strings"
	"sync"
	"unicode"
	"unicode/utf8"

	"github.com/sugarme/tokenizer"
	"github.com/sugarme/tokenizer/model"
	"github.com/sugarme/tokenizer/model/bpe"
	"github.com/sugarme/tokenizer/pretokenizer"
	"github.com/sugarme/tokenizer/pretrained"
	"golang.org/x/text/unicode/norm"
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
	if err := requireFields(data, "repo", "kind", "number", "title", "body", "author", "url"); err != nil {
		return err
	}
	type wire GithubItem
	return unmarshalDenyUnknown(data, (*wire)(g))
}

type HistorySnippet struct {
	Time *string `json:"time"`
	Text string  `json:"text"`
}

func (h *HistorySnippet) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "text"); err != nil {
		return err
	}
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

// MarshalJSON writes empty lists as `[]`, as serde writes an empty Vec.
func (s SessionContext) MarshalJSON() ([]byte, error) {
	type wire SessionContext
	value := wire(s)
	value.Files = nonNilStrings(value.Files)
	value.References = nonNilStrings(value.References)
	value.Mentions = nonNilStrings(value.Mentions)
	if value.History != nil && *value.History == nil {
		empty := []HistorySnippet{}
		value.History = &empty
	}
	return marshalSerdeJSON(value)
}

func (s *SessionContext) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "conversation", "files", "references"); err != nil {
		return err
	}
	type wire SessionContext
	return unmarshalDenyUnknown(data, (*wire)(s))
}

// nonNilStrings is a present empty list for a nil one.
func nonNilStrings(values []string) []string {
	if values == nil {
		return []string{}
	}
	return values
}

// ForItem selects the lines that mention `item` or an issue its title or body
// references. A bare `#N` in the item means the item's repository. In the
// session it does only when `bareIsItemRepo`: a session whose project spans
// several GitHub repositories cannot say which one `#42` names, and must not
// have the line presented as a mention of every watched repository's #42.
func (s SessionContext) ForItem(item *GithubItem, bareIsItemRepo bool) SessionContext {
	targets := GithubReferences(item.Title + "\n" + item.Body)
	targets = append(targets, GithubReferences(fmt.Sprintf("%s#%d", item.Repo, item.Number))...)
	type identity struct {
		repo   string
		number uint64
	}
	identify := func(reference *GithubReference, bareIsItemRepo bool) (identity, bool) {
		var repo string
		switch {
		case reference.Owner != nil && reference.Repo != nil:
			repo = *reference.Owner + "/" + *reference.Repo
		case bareIsItemRepo:
			repo = item.Repo
		default:
			return identity{}, false
		}
		return identity{repo: asciiLowercase(repo), number: reference.Number}, true
	}
	var wanted []identity
	for i := range targets {
		if target, ok := identify(&targets[i], true); ok {
			wanted = append(wanted, target)
		}
	}
	s.Mentions = nil
	for i := len(s.MentionCandidates) - 1; i >= 0; i-- {
		candidate := &s.MentionCandidates[i]
		target, ok := identify(&candidate.Reference, bareIsItemRepo)
		if ok && slices.Contains(wanted, target) && !slices.Contains(s.Mentions, candidate.Line) {
			s.Mentions = append(s.Mentions, candidate.Line)
			if len(s.Mentions) == MaxMentions {
				break
			}
		}
	}
	if s.Mentions == nil {
		s.Mentions = []string{}
	}
	s.MentionCandidates = nil
	return s
}

// asciiLowercase is Rust's `str::to_ascii_lowercase`: only ASCII letters
// change.
func asciiLowercase(text string) string {
	return strings.Map(func(c rune) rune {
		if c >= 'A' && c <= 'Z' {
			return c + ('a' - 'A')
		}
		return c
	}, text)
}

// GithubItemEvidence: the GitHub item deliberately serializes before its
// session-specific context, allowing requests for different sessions to share
// the same item prefix.
type GithubItemEvidence struct {
	Item    GithubItem     `json:"item"`
	Session SessionContext `json:"session"`
}

func (e *GithubItemEvidence) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "item", "session"); err != nil {
		return err
	}
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
	if err := e.validateFixedFields(); err != nil {
		return GithubItemEvidence{}, err
	}
	bounded := *e
	bounded.Session.Files = slices.Clone(e.Session.Files)
	bounded.Session.References = slices.Clone(e.Session.References)
	bounded.Session.Mentions = slices.Clone(e.Session.Mentions)
	bounded.Session.MentionCandidates = slices.Clone(e.Session.MentionCandidates)
	if e.Session.History != nil {
		history := slices.Clone(*e.Session.History)
		bounded.Session.History = &history
	}
	if len(bounded.Item.Body) > ItemBodyBytes {
		bounded.Item.Body = utf8Prefix(bounded.Item.Body, ItemBodyBytes) + bodyTruncationMarker
	}
	if err := bounded.Validate(); err != nil {
		return GithubItemEvidence{}, err
	}
	return bounded, nil
}

func (e *GithubItemEvidence) Validate() error {
	if err := e.validateFixedFields(); err != nil {
		return err
	}
	if len(e.Session.Conversation) > ConversationBytes {
		return errors.New("GitHub session conversation is too long")
	}
	bounded := func(values []string, count, size int) bool {
		if len(values) > count {
			return false
		}
		for _, value := range values {
			if strings.TrimSpace(value) == "" || len(value) > size {
				return false
			}
		}
		return true
	}
	if !bounded(e.Session.Files, MaxFiles, FileBytes) {
		return errors.New("invalid GitHub session files")
	}
	if !bounded(e.Session.References, MaxReferences, ReferenceBytes) {
		return errors.New("invalid GitHub session references")
	}
	if !bounded(e.Session.Mentions, MaxMentions, MentionBytes) {
		return errors.New("invalid GitHub session mentions")
	}
	if len(e.Item.Body) > ItemBodyBytes+len(bodyTruncationMarker) {
		return errors.New("GitHub item body is too long")
	}
	length, err := upstreamLen(e)
	if err != nil {
		return err
	}
	if length > GithubItemMaxBodyBytes {
		return errors.New("GitHub item request exceeds byte limit")
	}
	return nil
}

// UpstreamBody builds the direct-provider envelope without converting the
// ordered evidence structs into a map-backed value.
func (e *GithubItemEvidence) UpstreamBody() any {
	return githubItemUpstreamBody{Model: "jev-latest", State: e, Questions: GithubItemQuestions()}
}

func (e *GithubItemEvidence) validateFixedFields() error {
	item := &e.Item
	if strings.TrimSpace(item.Repo) == "" || len(item.Repo) > 256 {
		return errors.New("invalid GitHub repository")
	}
	if item.Number == 0 || item.Number > 9_007_199_254_740_991 {
		return errors.New("invalid GitHub item number")
	}
	if strings.TrimSpace(item.Title) == "" || len(item.Title) > 1024 {
		return errors.New("invalid GitHub item title")
	}
	if len(item.Author) > 256 {
		return errors.New("GitHub item author is too long")
	}
	if strings.TrimSpace(item.URL) == "" || len(item.URL) > 2048 {
		return errors.New("invalid GitHub item URL")
	}
	return nil
}

// GithubItemVerdictParse is GithubItemVerdict::parse. `response` is a decoded
// JSON value (`any` from encoding/json).
func GithubItemVerdictParse(response any) (GithubItemVerdict, error) {
	member := func(value any, key string) any {
		if object, ok := value.(map[string]any); ok {
			return object[key]
		}
		return nil
	}
	answers, ok := member(response, "answers").(map[string]any)
	if !ok {
		return GithubItemVerdict{}, errors.New("missing GitHub item answers")
	}
	if _, interested := answers["interested"]; len(answers) != 1 || !interested {
		return GithubItemVerdict{}, errors.New("incomplete GitHub item answers")
	}
	answer := answers["interested"]
	if kind, ok := member(answer, "type").(string); !ok || kind != "noul" {
		return GithubItemVerdict{}, errors.New("invalid GitHub item answer type")
	}
	probability, ok := jsonFloat(member(answer, "noul"))
	if !ok {
		return GithubItemVerdict{}, errors.New("missing GitHub item probability")
	}
	if math.IsNaN(probability) || math.IsInf(probability, 0) || probability < 0 || probability > 1 {
		return GithubItemVerdict{}, errors.New("invalid GitHub item probability")
	}
	// The 83-pair production replay found relevant items just below 0.5.
	return GithubItemVerdict{Interested: probability >= 0.4}, nil
}

// jsonFloat is serde_json's `Value::as_f64` over a decoded JSON number.
func jsonFloat(value any) (float64, bool) {
	switch number := value.(type) {
	case float64:
		return number, true
	case json.Number:
		parsed, err := number.Float64()
		return parsed, err == nil
	case int64:
		return float64(number), true
	case uint64:
		return float64(number), true
	case int:
		return float64(number), true
	}
	return 0, false
}

func upstreamLen(evidence *GithubItemEvidence) (int, error) {
	body, err := marshalSerdeJSON(evidence.UpstreamBody())
	if err != nil {
		return 0, err
	}
	return len(body), nil
}

// marshalSerdeJSON encodes as serde_json::to_vec does: `<`, `>` and `&`
// stay literal and there is no trailing newline.
func marshalSerdeJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// marshalSerdeJSONPretty encodes as serde_json::to_vec_pretty does.
func marshalSerdeJSONPretty(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
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

//go:embed data/github_item/ettin-tokenizer.json
var githubHistoryTokenizerJSON []byte

// githubHistoryTokenizer is the embedded HuggingFace tokenizer definition.
// github.com/sugarme/tokenizer loads it; its BPE vocabulary and merges are
// used here. Its added vocabulary does not match tokens that are also in
// the model vocabulary (the space runs and the redaction markers here), so
// the added tokens are split out first as HuggingFace's AddedVocabulary
// does, and the template's special tokens are added here. Its ByteLevel
// pre-tokenizer and its BPE merge order differ from HuggingFace's, so
// historyByteLevelWords and historyMergeWord do those steps.
type githubHistoryTokenizer struct {
	tokenizer *tokenizer.Tokenizer
	model     *bpe.BPE
	// rawTokens match the input as written, normalizedTokens the NFC text.
	rawTokens, normalizedTokens []historyAddedToken
	single, pair                []historyTemplatePiece
}

type historyAddedToken struct {
	Content    string `json:"content"`
	ID         uint32 `json:"id"`
	Lstrip     bool   `json:"lstrip"`
	Rstrip     bool   `json:"rstrip"`
	Normalized bool   `json:"normalized"`
}

// historyTemplatePiece is one element of a TemplateProcessing template: a
// special token's ids, or the first (A) or second (B) sequence.
type historyTemplatePiece struct {
	ids      []uint32
	sequence string
}

func (t *githubHistoryTokenizer) Encode(first string, second *string, addSpecialTokens bool) ([]uint32, error) {
	firstIDs, err := t.encodeSequence(first)
	if err != nil {
		return nil, err
	}
	var secondIDs []uint32
	if second != nil {
		if secondIDs, err = t.encodeSequence(*second); err != nil {
			return nil, err
		}
	}
	if !addSpecialTokens {
		return append(firstIDs, secondIDs...), nil
	}
	template := t.single
	if second != nil {
		template = t.pair
	}
	var ids []uint32
	for _, piece := range template {
		switch piece.sequence {
		case "A":
			ids = append(ids, firstIDs...)
		case "B":
			ids = append(ids, secondIDs...)
		default:
			ids = append(ids, piece.ids...)
		}
	}
	return ids, nil
}

// encodeSequence is HuggingFace's extract_and_normalize followed by the
// model: added tokens written verbatim are split out of the raw text, the
// rest is normalized and split on the normalized added tokens, and each
// remaining piece is tokenized on its own.
func (t *githubHistoryTokenizer) encodeSequence(sequence string) ([]uint32, error) {
	var ids []uint32
	for _, raw := range splitHistoryAddedTokens(sequence, t.rawTokens) {
		if raw.token != nil {
			ids = append(ids, raw.token.ID)
			continue
		}
		for _, piece := range splitHistoryAddedTokens(norm.NFC.String(raw.text), t.normalizedTokens) {
			if piece.token != nil {
				ids = append(ids, piece.token.ID)
				continue
			}
			// sugarme's ByteLevel pre-tokenizer drops the regex's
			// whitespace alternatives and reads `\s` as ASCII, so each word
			// is split here and merged as HuggingFace's BPE merges it.
			for _, word := range historyByteLevelWords(piece.text) {
				var mapped strings.Builder
				for i := 0; i < len(word); i++ {
					mapped.WriteString(pretokenizer.BytesChar[word[i]])
				}
				ids = append(ids, historyMergeWord(mapped.String(), *t.model.Vocab, *t.model.Merges)...)
			}
		}
	}
	return ids, nil
}

// historySymbol is one symbol of a word being merged, linked to its
// neighbours (HuggingFace's bpe::word::Symbol).
type historySymbol struct {
	id         int
	prev, next int
	len        int
}

// historyMerge is a pending merge of the symbol at pos with its next one.
type historyMerge struct {
	pos, rank, newID int
}

// historyMergeQueue is a min-heap by rank, then by position, as
// HuggingFace's BinaryHeap of merges orders them. sugarme orders equal
// ranks by an unset timestamp, which merges repeated characters, such as a
// line of box-drawing characters, differently.
type historyMergeQueue []historyMerge

func (q historyMergeQueue) Len() int { return len(q) }
func (q historyMergeQueue) Less(i, j int) bool {
	if q[i].rank != q[j].rank {
		return q[i].rank < q[j].rank
	}
	return q[i].pos < q[j].pos
}
func (q historyMergeQueue) Swap(i, j int) { q[i], q[j] = q[j], q[i] }
func (q *historyMergeQueue) Push(x any)   { *q = append(*q, x.(historyMerge)) }
func (q *historyMergeQueue) Pop() any {
	old := *q
	last := old[len(old)-1]
	*q = old[:len(old)-1]
	return last
}

// historyMergeWord is HuggingFace's BPE::merge_word followed by
// Word::merge_all without dropout: the word's characters as vocabulary ids
// (a character the vocabulary lacks is dropped, as with no unknown token),
// merged lowest rank first and leftmost first among equal ranks.
func historyMergeWord(word string, vocab model.Vocab, merges bpe.Merges) []uint32 {
	var symbols []historySymbol
	for _, c := range word {
		id, ok := vocab[string(c)]
		if !ok {
			continue
		}
		symbol := historySymbol{id: id, prev: -1, next: -1, len: utf8.RuneLen(c)}
		if count := len(symbols); count > 0 {
			symbols[count-1].next = count
			symbol.prev = count - 1
		}
		symbols = append(symbols, symbol)
	}
	queue := &historyMergeQueue{}
	for index := 0; index+1 < len(symbols); index++ {
		if merge, ok := merges[bpe.Pair{C1: symbols[index].id, C2: symbols[index+1].id}]; ok {
			*queue = append(*queue, historyMerge{pos: index, rank: merge.Rank, newID: merge.NewId})
		}
	}
	heap.Init(queue)
	for queue.Len() > 0 {
		top := heap.Pop(queue).(historyMerge)
		if symbols[top.pos].len == 0 || symbols[top.pos].next == -1 {
			continue
		}
		nextPos := symbols[top.pos].next
		right := symbols[nextPos]
		// Skip an expired queue entry.
		merge, ok := merges[bpe.Pair{C1: symbols[top.pos].id, C2: right.id}]
		if !ok || merge.NewId != top.newID {
			continue
		}
		symbols[top.pos].id = top.newID
		symbols[top.pos].len += right.len
		symbols[top.pos].next = right.next
		symbols[nextPos].len = 0
		if right.next > -1 && right.next < len(symbols) {
			symbols[right.next].prev = top.pos
		}
		current := symbols[top.pos]
		if current.prev >= 0 {
			if merge, ok := merges[bpe.Pair{C1: symbols[current.prev].id, C2: current.id}]; ok {
				heap.Push(queue, historyMerge{pos: current.prev, rank: merge.Rank, newID: merge.NewId})
			}
		}
		if next := current.next; next > -1 && next < len(symbols) {
			if merge, ok := merges[bpe.Pair{C1: current.id, C2: symbols[next].id}]; ok {
				heap.Push(queue, historyMerge{pos: top.pos, rank: merge.Rank, newID: merge.NewId})
			}
		}
	}
	var ids []uint32
	for _, symbol := range symbols {
		if symbol.len != 0 {
			ids = append(ids, uint32(symbol.id))
		}
	}
	return ids
}

// historyByteLevelWords splits text as HuggingFace's ByteLevel
// pre-tokenizer does with use_regex, by the GPT-2 expression
// `'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+`
// with Unicode `\s`. Go's regexp has no lookahead, so the alternatives are
// tried here in order at each position, as the leftmost-first regex does.
func historyByteLevelWords(text string) []string {
	isSpace := unicode.IsSpace
	isLetter := func(c rune) bool { return unicode.IsLetter(c) }
	isNumber := func(c rune) bool { return unicode.IsNumber(c) }
	isOther := func(c rune) bool { return !isSpace(c) && !isLetter(c) && !isNumber(c) }
	// run is the end of the longest run of characters in class from i.
	run := func(i int, class func(rune) bool) int {
		for i < len(text) {
			c, size := utf8.DecodeRuneInString(text[i:])
			if !class(c) {
				break
			}
			i += size
		}
		return i
	}
	var words []string
	for i := 0; i < len(text); {
		end := i
		rest := text[i:]
		for _, contraction := range []string{"'s", "'t", "'re", "'ve", "'m", "'ll", "'d"} {
			if strings.HasPrefix(rest, contraction) {
				end = i + len(contraction)
				break
			}
		}
		if end == i {
			for _, class := range []func(rune) bool{isLetter, isNumber, isOther} {
				start := i
				if text[start] == ' ' {
					if c, _ := utf8.DecodeRuneInString(text[start+1:]); start+1 < len(text) && class(c) {
						start++
					}
				}
				if c, _ := utf8.DecodeRuneInString(text[start:]); class(c) {
					end = run(start, class)
					break
				}
			}
		}
		if end == i {
			// Whitespace: the run, less its last character when a
			// non-space follows, or that one character alone.
			stop := run(i, isSpace)
			if stop < len(text) {
				_, last := utf8.DecodeLastRuneInString(text[i:stop])
				if stop-last > i {
					stop -= last
				}
			}
			end = stop
		}
		words = append(words, text[i:end])
		i = end
	}
	return words
}

type historyPiece struct {
	text  string
	token *historyAddedToken
}

// splitHistoryAddedTokens is AddedVocabulary::find_matches: leftmost-longest
// matches of the added tokens, with `lstrip` and `rstrip` taking the
// neighbouring whitespace, and the text between them as unmatched pieces.
func splitHistoryAddedTokens(sentence string, tokens []historyAddedToken) []historyPiece {
	if sentence == "" {
		return []historyPiece{{text: ""}}
	}
	var pieces []historyPiece
	startOffset := 0
	for position := 0; position < len(sentence); {
		var match *historyAddedToken
		for i := range tokens {
			if strings.HasPrefix(sentence[position:], tokens[i].Content) &&
				(match == nil || len(tokens[i].Content) > len(match.Content)) {
				match = &tokens[i]
			}
		}
		if match == nil {
			position++
			continue
		}
		start, stop := position, position+len(match.Content)
		position = stop
		if match.Lstrip {
			start = max(len(strings.TrimRightFunc(sentence[:start], unicode.IsSpace)), startOffset)
		}
		if match.Rstrip {
			stop += len(sentence[stop:]) - len(strings.TrimLeftFunc(sentence[stop:], unicode.IsSpace))
		}
		if startOffset < start {
			pieces = append(pieces, historyPiece{text: sentence[startOffset:start]})
		}
		pieces = append(pieces, historyPiece{text: sentence[start:stop], token: match})
		startOffset = stop
	}
	if startOffset != len(sentence) {
		pieces = append(pieces, historyPiece{text: sentence[startOffset:]})
	}
	return pieces
}

var historyTokenizer = sync.OnceValues(func() (GithubHistoryTokenizer, error) {
	loaded, err := pretrained.FromReader(bytes.NewReader(githubHistoryTokenizerJSON))
	if err != nil {
		return nil, err
	}
	loaded.WithPadding(nil)
	loaded.WithTruncation(nil)
	var definition struct {
		AddedTokens   []historyAddedToken `json:"added_tokens"`
		PostProcessor struct {
			Single        []map[string]struct{ ID string } `json:"single"`
			Pair          []map[string]struct{ ID string } `json:"pair"`
			SpecialTokens map[string]struct {
				IDs []uint32 `json:"ids"`
			} `json:"special_tokens"`
		} `json:"post_processor"`
	}
	if err := json.Unmarshal(githubHistoryTokenizerJSON, &definition); err != nil {
		return nil, err
	}
	history := &githubHistoryTokenizer{tokenizer: loaded}
	switch model := loaded.GetModel().(type) {
	case *bpe.BPE:
		history.model = model
	case bpe.BPE:
		history.model = &model
	default:
		return nil, fmt.Errorf("history tokenizer model is %T, not BPE", model)
	}
	for _, token := range definition.AddedTokens {
		if token.Normalized {
			history.normalizedTokens = append(history.normalizedTokens, token)
		} else {
			history.rawTokens = append(history.rawTokens, token)
		}
	}
	template := func(pieces []map[string]struct{ ID string }) ([]historyTemplatePiece, error) {
		var parsed []historyTemplatePiece
		for _, piece := range pieces {
			if sequence, ok := piece["Sequence"]; ok {
				parsed = append(parsed, historyTemplatePiece{sequence: sequence.ID})
				continue
			}
			special, ok := definition.PostProcessor.SpecialTokens[piece["SpecialToken"].ID]
			if !ok {
				return nil, fmt.Errorf("history tokenizer template names unknown special token %q", piece["SpecialToken"].ID)
			}
			parsed = append(parsed, historyTemplatePiece{ids: special.IDs})
		}
		return parsed, nil
	}
	if history.single, err = template(definition.PostProcessor.Single); err != nil {
		return nil, err
	}
	if history.pair, err = template(definition.PostProcessor.Pair); err != nil {
		return nil, err
	}
	return history, nil
})

// HistoryTokenizer is the frozen tokenizer for aliases600 evidence budgets,
// without truncation or padding. Built once: parsing the embedded 3.5 MB
// definition on every evidence build was a measurable share of the daemon's
// page faults. Rust builds it only with the `github-history` feature.
func HistoryTokenizer() (GithubHistoryTokenizer, error) {
	return historyTokenizer()
}
