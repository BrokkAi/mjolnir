// Port of mj-transcript/src/github_history.rs.
//
// Byte-exact aliases600 retrieval and evidence envelope from the frozen
// Python format.
//
// Library replacements: Rust's regex crate becomes regexp (RE2, also
// leftmost-first); aho_corasick's contiguous NFA becomes historyAutomaton,
// a small byte-level Aho-Corasick automaton written here; the tokenizers
// crate is core.HistoryTokenizer; libm::log1p is math.Log1p (a pure Go
// fdlibm port, the same bits on every platform); chrono's RFC 3339 rendering
// is time.

package transcript

import (
	"bytes"
	"cmp"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"io"
	"regexp"
	"slices"
	"sort"
	"strings"
	"sync"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// OrderedValue is local ordered JSON, so the Python format never changes
// global serde_json features. Serialized untagged, as the JSON value itself.
// Rust's enum is one struct here; Kind says which field holds the value.
type OrderedValue struct {
	Kind OrderedValueKind
	Bool bool
	// Number is the number as serde_json::Number holds it: int64 for a
	// negative integer, uint64 or int64 for a non-negative one, float64 for
	// any other number (parsed correctly rounded, as with float_roundtrip).
	Number any
	Str    string
	Array  []OrderedValue
	// Object holds the members in order. A repeated key is kept once, at its
	// first position with its last value, as indexmap's insert keeps it.
	Object []OrderedMember
}

type OrderedValueKind uint8

const (
	OrderedValueNull OrderedValueKind = iota
	OrderedValueBool
	OrderedValueNumber
	OrderedValueString
	OrderedValueArray
	OrderedValueObject
)

type OrderedMember struct {
	Key   string
	Value OrderedValue
}

func (v *OrderedValue) AsStr() (string, bool) {
	if v.Kind == OrderedValueString {
		return v.Str, true
	}
	return "", false
}

func (v *OrderedValue) AsU64() (uint64, bool) {
	if v.Kind != OrderedValueNumber {
		return 0, false
	}
	switch number := v.Number.(type) {
	case uint64:
		return number, true
	case int64:
		return uint64(number), number >= 0
	}
	return 0, false
}

func (v *OrderedValue) AsObject() ([]OrderedMember, bool) {
	if v.Kind == OrderedValueObject {
		return v.Object, true
	}
	return nil, false
}

func (v *OrderedValue) AsArray() ([]OrderedValue, bool) {
	if v.Kind == OrderedValueArray {
		return v.Array, true
	}
	return nil, false
}

// Index is Rust's value[key]: the member's value, or Null when v is not an
// object or has no such member.
func (v *OrderedValue) Index(key string) OrderedValue {
	if v.Kind == OrderedValueObject {
		for i := range v.Object {
			if v.Object[i].Key == key {
				return v.Object[i].Value
			}
		}
	}
	return OrderedValue{}
}

// EqStr is Rust's PartialEq<&str>: whether v is the string s.
func (v *OrderedValue) EqStr(s string) bool {
	text, ok := v.AsStr()
	return ok && text == s
}

// String is Rust's Display: compact JSON with object members in order and
// numbers as Python's repr writes them.
func (v OrderedValue) String() string {
	panic("unported: mj-transcript/src/github_history.rs Display for OrderedValue")
}

// MarshalJSON writes v as serde_json writes the untagged enum.
func (v OrderedValue) MarshalJSON() ([]byte, error) {
	return jsonvalue.Marshal(v.jsonValue())
}

// jsonValue is v as a jsonvalue value.
func (v *OrderedValue) jsonValue() any {
	switch v.Kind {
	case OrderedValueBool:
		return v.Bool
	case OrderedValueNumber:
		return v.Number
	case OrderedValueString:
		return v.Str
	case OrderedValueArray:
		values := make([]any, len(v.Array))
		for i := range v.Array {
			values[i] = v.Array[i].jsonValue()
		}
		return values
	case OrderedValueObject:
		object := make(jsonvalue.Object, len(v.Object))
		for i := range v.Object {
			object[i] = jsonvalue.Member{Key: v.Object[i].Key, Value: v.Object[i].Value.jsonValue()}
		}
		return object
	}
	return nil
}

// UnmarshalJSON reads any JSON value, as serde reads the untagged enum.
func (v *OrderedValue) UnmarshalJSON(data []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	value, err := decodeOrderedValue(decoder)
	if err != nil {
		return err
	}
	if _, err := decoder.Token(); err != io.EOF {
		return fmt.Errorf("trailing characters after a JSON value")
	}
	*v = value
	return nil
}

// decodeOrderedValue reads one JSON value from a decoder that uses
// json.Number.
func decodeOrderedValue(decoder *json.Decoder) (OrderedValue, error) {
	panic("unported: mj-transcript/src/github_history.rs Deserialize for OrderedValue")
}

// pythonNumber: Python repr uses the same shortest round-trip digits, but a
// different scientific-notation threshold, explicit exponent signs and
// two-digit exponents. number is an OrderedValue's Number.
func pythonNumber(number any) string {
	panic("unported: mj-transcript/src/github_history.rs python_number")
}

// re compiles a frozen expression.
func re(s string) *regexp.Regexp {
	return regexp.MustCompile(s)
}

func chars(s string) []rune {
	return []rune(s)
}

var longTokenPattern = sync.OnceValue(func() *regexp.Regexp {
	return re(`[A-Za-z0-9_.+\-]{24,}`)
})

func Redact(s string) string {
	return longTokenPattern().ReplaceAllLiteralString(s, "[redacted]")
}

// spaced is Python json.dumps with its default separators, retaining source
// object order.
func spaced(v *OrderedValue) string {
	panic("unported: mj-transcript/src/github_history.rs spaced")
}

// Snippet is core.HistorySnippet (Rust `pub use ... as Snippet`).
type Snippet = core.HistorySnippet

type Span struct {
	Start   int
	End     int
	Score   float64
	Snippet Snippet
}

// historyIdentifier is one searchable identifier and its base weight (Rust's
// (String, f64)).
type historyIdentifier struct {
	text   string
	weight float64
}

func identifiers(item *OrderedValue) []historyIdentifier {
	panic("unported: mj-transcript/src/github_history.rs identifiers")
}

// TextSink receives rendered transcript text piece by piece. Rust's default
// methods have no Go counterpart: every sink implements all three.
type TextSink interface {
	PushStr(text string)
	// Mark says the text that follows belongs to a new part of the
	// transcript: the header (nil) or the row with this ordinal. A sink that
	// keeps positions uses it to say where in the stored rows a position
	// lies. Sinks without positions do nothing.
	Mark(ordinal *int)
	// IsFull reports that the sink takes no more text; the rest may be
	// skipped. Sinks without a limit return false.
	IsFull() bool
}

// HistorySink is a sink that ends in a selection of windows once the whole
// transcript has been pushed. D is what the sink returns besides the
// selection (Rust's associated type Done). Implementations are pointer types
// so pushed text reaches them through the interface.
type HistorySink[D any] interface {
	TextSink
	// Select is called once, after the last push. check is called often; an
	// error from it ends the selection.
	Select(check func() error) (D, []Selected, error)
}

// StringSink is Rust's TextSink and HistorySink for String: it keeps the
// whole text and selects nothing.
type StringSink struct {
	Text string
}

func (s *StringSink) PushStr(text string) { s.Text += text }
func (s *StringSink) Mark(*int)           {}
func (s *StringSink) IsFull() bool        { return false }

func (s *StringSink) Select(func() error) (string, []Selected, error) {
	return s.Text, nil, nil
}

func (s *HistoryScanner) IsFull() bool { return false }

func (s *HistoryScanner) Select(check func() error) (struct{}, []Selected, error) {
	selected, err := s.Finish(check)
	return struct{}{}, selected, err
}

var (
	_ HistorySink[string]   = (*StringSink)(nil)
	_ HistorySink[struct{}] = (*HistoryScanner)(nil)
	_ TextSink              = (*tee)(nil)
	_ TextSink              = (*slice)(nil)
)

// idState is one identifier's streaming match state.
type idState struct {
	// cps is the length in code points.
	cps     int
	numeric bool
	// lastEnd is the end (code point index) of the last raw occurrence:
	// later occurrences must start at or after it.
	lastEnd int
	// hits is the occurrences that passed the boundary checks.
	hits int
}

// noStamp is Rust's NO_STAMP.
const noStamp uint32 = 1<<32 - 1

// openLen is Rust's OPEN: a candidate's len while its text is incomplete.
const openLen uint16 = 1<<16 - 1

// buildingSet is Rust's BUILDING: candidate.ids while the window can still
// receive hits.
const buildingSet uint32 = 1<<32 - 1

// headerOrdinal is Rust's HEADER: the ordinal of the header, which is not a
// row.
const headerOrdinal uint32 = 1<<32 - 1

// candidate is a window of up to 500 code points around at least one hit
// that is not a duplicate of the session digest, in 24 bytes. Its text is
// held only while it is open; the text of the selected ones is recovered
// from the stored rows. Windows judged duplicates are not kept at all.
type candidate struct {
	start uint32
	// stamp is the position of the last stamp at or before the start.
	stamp uint32
	// ordinal and offset are the part of the transcript the start lies in,
	// and its offset in that part.
	ordinal uint32
	offset  uint32
	// ids is the index in the interned identifier sets, once the window
	// takes no more hits.
	ids uint32
	// len is the length of the trimmed text in code points, or openLen.
	len uint16
}

// idSets is identifier sets, interned when a window stops receiving hits: a
// window holds the index of its set.
type idSets struct {
	sets [][]uint32
	// index maps a set, encoded as 4 little-endian bytes per identifier, to
	// its index (Rust keys a HashMap by the Vec itself).
	index map[string]uint32
}

func (s *idSets) intern(set []uint32) uint32 {
	key := make([]byte, 4*len(set))
	for i, id := range set {
		binary.LittleEndian.PutUint32(key[4*i:], id)
	}
	if i, ok := s.index[string(key)]; ok {
		return i
	}
	if s.index == nil {
		s.index = map[string]uint32{}
	}
	i := uint32(len(s.sets))
	s.index[string(key)] = i
	s.sets = append(s.sets, set)
	return i
}

// known is the 100-code-point windows of the session digest, found by hash
// and compared exactly.
type known struct {
	digest  []rune
	windows []knownWindow
}

// knownWindow is a window's hash and start (Rust's (u64, usize)).
type knownWindow struct {
	hash  uint64
	index int
}

// windowHash hashes a window for lookup only. Rust uses DefaultHasher; any
// hash serves, since matches are compared exactly.
func windowHash(window []rune) uint64 {
	hash := fnv.New64a()
	var buffer [4]byte
	for _, character := range window {
		binary.LittleEndian.PutUint32(buffer[:], uint32(character))
		hash.Write(buffer[:])
	}
	return hash.Sum64()
}

func newKnown(digest []rune) known {
	windows := make([]knownWindow, max(len(digest)-99, 0))
	for i := range windows {
		windows[i] = knownWindow{hash: windowHash(digest[i : i+100]), index: i}
	}
	slices.SortFunc(windows, func(a, b knownWindow) int {
		if a.hash != b.hash {
			return cmp.Compare(a.hash, b.hash)
		}
		return cmp.Compare(a.index, b.index)
	})
	return known{digest: digest, windows: windows}
}

func (k *known) contains(window []rune) bool {
	hash := windowHash(window)
	from := sort.Search(len(k.windows), func(i int) bool { return k.windows[i].hash >= hash })
	for _, candidate := range k.windows[from:] {
		if candidate.hash != hash {
			break
		}
		if slices.Equal(k.digest[candidate.index:candidate.index+100], window) {
			return true
		}
	}
	return false
}

// judge is the trimmed length of a finished window; ok is false when it
// duplicates the session digest.
func judge(existing string, known *known, text string) (uint16, bool) {
	raw := strings.TrimFunc(text, func(c rune) bool { return core.Contains(core.Space, c) })
	redacted := Redact(raw)
	r := chars(redacted)
	if strings.Contains(existing, redacted) {
		return 0, false
	}
	for i := 0; i < max(len(r)-99, 0); i += 50 {
		if known.contains(r[i : i+100]) {
			return 0, false
		}
	}
	return uint16(utf8.RuneCountInString(raw)), true
}

// Selected is a window chosen for the evidence, before its text is
// recovered.
type Selected struct {
	Start int
	End   int
	Score float64
	// Stamp is the position of the governing stamp; nil when none.
	Stamp *int
	// Ordinal is where the start lies: the row ordinal, or the maximum
	// uint32 for the header (or when the sink was never marked); Offset is
	// the offset in that part.
	Ordinal uint32
	Offset  uint32
}

// historyAutomaton stands for aho_corasick's contiguous NFA built over the
// identifiers: a byte-level Aho-Corasick automaton, unanchored, stepped one
// byte at a time, that reports every identifier ending at the current byte.
type historyAutomaton struct {
	// next holds each state's goto transitions; fail its failure link;
	// matches the identifiers (by index) that end in it, own and inherited
	// through failure links.
	next    []map[byte]uint32
	fail    []uint32
	matches [][]int
}

func newHistoryAutomaton(patterns []string) *historyAutomaton {
	panic("unported: mj-transcript/src/github_history.rs NFA::new (aho_corasick)")
}

func (a *historyAutomaton) startState() uint32 {
	return 0
}

func (a *historyAutomaton) nextState(state uint32, b byte) uint32 {
	panic("unported: mj-transcript/src/github_history.rs Automaton::next_state (aho_corasick)")
}

// historyHit is a raw occurrence of an identifier (Rust's (usize, usize)).
type historyHit struct {
	id  int
	pos int
}

// buildingWindow is a window still taking hits and its identifiers.
type buildingWindow struct {
	start uint32
	ids   []uint32
}

// openWindow is a window still collecting text.
type openWindow struct {
	start uint32
	text  []byte
}

// stampProgress is a stamp in progress: its position and whether it has
// content yet.
type stampProgress struct {
	pos     int
	content bool
}

// historySegment is where a part of the transcript starts.
type historySegment struct {
	at      int
	ordinal uint32
}

// HistoryScanner is the single-pass form of the frozen retrieval: the
// transcript is pushed in pieces and only the last few hundred code points
// and a small record per window that has a hit are kept. Positions are code
// point indexes, as in the Python original.
type HistoryScanner struct {
	existing  string
	known     known
	ids       []historyIdentifier
	info      []idState
	automaton *historyAutomaton
	state     uint32
	seen      int
	// ring holds the code points [seen - len(ring), seen). Large enough for
	// a window that starts 374 code points before a hit that ends one
	// identifier length later, plus the boundary character before the hit.
	ring    []rune
	ringCap int
	// pending is raw occurrences ending at seen; the next code point decides
	// their boundary.
	pending []historyHit
	// live is windows kept, ordered by start.
	live []candidate
	sets idSets
	// building is windows still taking hits, sorted by start. Every window
	// closes at its start plus the same reach, so the oldest always closes
	// first.
	building []buildingWindow
	// open is windows still collecting text: start and text.
	open []openWindow
	// stamps is the positions of the stamps seen, pruned to those a future
	// window can still need.
	stamps    []int
	stampOpen *stampProgress
	// awaitingStamp is windows that began inside a stamp that was still open.
	awaitingStamp []uint32
	// segments is where each part of the transcript starts, pruned like the
	// stamps.
	segments  []historySegment
	lineStart bool
	// tooLong is set when the transcript outgrew the 32-bit positions of the
	// records.
	tooLong bool
	// peakTexts is test-only bookkeeping in Rust (#[cfg(test)]).
	peakTexts int
}

// NewHistoryScanner scans for item's identifiers. item is the original
// title/body; session the bounded session digest.
func NewHistoryScanner(item, session *OrderedValue) *HistoryScanner {
	return historyScannerWithIdentifiers(identifiers(item), session)
}

// historyScannerWithIdentifiers is like NewHistoryScanner with the item's
// identifiers already extracted.
func historyScannerWithIdentifiers(ids []historyIdentifier, session *OrderedValue) *HistoryScanner {
	panic("unported: mj-transcript/src/github_history.rs HistoryScanner::with_identifiers")
}

// Mark: see TextSink.Mark.
func (s *HistoryScanner) Mark(ordinal *int) {
	if s.automaton == nil {
		return
	}
	part := headerOrdinal
	if ordinal != nil {
		part = uint32(*ordinal)
	}
	s.segments = append(s.segments, historySegment{at: s.seen, ordinal: part})
	for len(s.segments) >= 2 && s.segments[1].at+s.ringCap <= s.seen {
		s.segments = s.segments[1:]
	}
}

func (s *HistoryScanner) ringAt(index int) rune {
	return s.ring[index-(s.seen-len(s.ring))]
}

// find is the live window starting at start (found), or where one would be
// inserted.
func (s *HistoryScanner) find(start uint32) (int, bool) {
	return slices.BinarySearchFunc(s.live, start, func(c candidate, start uint32) int {
		return cmp.Compare(c.start, start)
	})
}

// resolve decides one raw occurrence at pos; hasNext is false at the end of
// the transcript.
func (s *HistoryScanner) resolve(id, pos int, next rune, hasNext bool) {
	panic("unported: mj-transcript/src/github_history.rs HistoryScanner::resolve")
}

// closeWindows interns the identifiers of windows no later hit can reach: a
// hit ends at most ringCap - 400 code points (the longest identifier) past a
// start it can reach by 374.
func (s *HistoryScanner) closeWindows(all bool) {
	panic("unported: mj-transcript/src/github_history.rs HistoryScanner::close_windows")
}

// settle records the verdict on a window whose text is complete.
func (s *HistoryScanner) settle(start uint32, text string) {
	length, ok := judge(s.existing, &s.known, text)
	at, found := s.find(start)
	if !found {
		panic("an open window is live")
	}
	if ok {
		s.live[at].len = length
	} else {
		s.live = slices.Delete(s.live, at, at+1)
	}
}

func (s *HistoryScanner) step(c rune) {
	panic("unported: mj-transcript/src/github_history.rs HistoryScanner::step")
}

func (s *HistoryScanner) PushStr(text string) {
	if s.automaton == nil {
		return
	}
	for _, c := range text {
		s.step(c)
	}
}

// Finish scores and selects the windows once the whole transcript was
// pushed.
func (s *HistoryScanner) Finish(check func() error) ([]Selected, error) {
	panic("unported: mj-transcript/src/github_history.rs HistoryScanner::finish")
}

// spansFromWholeText is the spans of selected windows whose text is cut out
// of a whole transcript.
func spansFromWholeText(selected []Selected, text string) []Span {
	panic("unported: mj-transcript/src/github_history.rs spans_from_whole_text")
}

// Retrieve retrieves from a complete transcript text. Original title/body
// must be supplied, independently of the bounded classifier item.
func Retrieve(item *OrderedValue, text string, session *OrderedValue) []Span {
	scanner := NewHistoryScanner(item, session)
	scanner.PushStr(text)
	selected, err := scanner.Finish(func() error { return nil })
	if err != nil {
		panic("retrieval from a whole text cannot fail: " + err.Error())
	}
	return spansFromWholeText(selected, text)
}

type HistoryEvidence struct {
	Evidence         OrderedValue
	HistoryTokens    int
	TotalTokens      int
	BaseTokens       int
	OriginalSnippets int
	Retained         []Span
	Removed          []HistoryRemoval
	OverLimit        bool
}

// HistoryRemoval is a span the budget removed: its start and why (Rust's
// (usize, &'static str)).
type HistoryRemoval struct {
	Start  int
	Reason string
}

// GithubHistoryEvidence is Rust's github_history::evidence (prefixed: the
// authorization module's Evidence comes first). Base evidence is already
// bounded/redacted exactly once; this only budgets history.
func GithubHistoryEvidence(originalItem *OrderedValue, text string, base *OrderedValue) (HistoryEvidence, error) {
	session := base.Index("session")
	return EvidenceFromSpans(base, Retrieve(originalItem, text, &session))
}

// EvidenceFromSpans budgets already retrieved spans into base; the
// transcript is used only through them.
func EvidenceFromSpans(base *OrderedValue, work []Span) (HistoryEvidence, error) {
	panic("unported: mj-transcript/src/github_history.rs evidence_from_spans")
}

// tokenizersInput is the pair input of core.GithubHistoryTokenizer.Encode.
func tokenizersInput(a, b string) (string, *string) {
	return a, &b
}

// HistoryCounts is the counts that the transcript header reports; they fix
// the header's length and so every code point position after it.
type HistoryCounts struct {
	Included int
	Changed  int
	Later    int
	Notices  int
}

// RowMeta is what the first pass needs from each row. SQLite's
// `json_extract` yields these cheaply, without decoding the whole body.
type RowMeta struct {
	CreatedAtMs     int64
	LastChangedAtMs int64
	// Kind is `body.kind`, empty unless a string.
	Kind string
	// EventType is `body.event.body.type`, empty unless a string.
	EventType string
	// EventNumber is `body.event.body.number` when an unsigned integer.
	EventNumber *uint64
	// EventRepo is `body.event.body.repo` when a string.
	EventRepo *string
}

func kindRenders(kind string) bool {
	switch kind {
	case "user", "agent", "tool", "message":
		return true
	}
	return false
}

func isNoticeType(eventType string) bool {
	return eventType == "new_github_item" || strings.HasPrefix(eventType, "github_")
}

// noticeNamesItem reports whether a notice of a notice type names this
// pair's item. Rust's to_lowercase is full Unicode lowercasing;
// strings.ToLower agrees on repository names, which are ASCII.
func noticeNamesItem(number *uint64, repo *string, itemNumber uint64, itemRepoLower string) bool {
	return number != nil && *number == itemNumber && (repo == nil || strings.ToLower(*repo) == itemRepoLower)
}

// timestamp is ms as RFC 3339 UTC with milliseconds, as chrono's
// to_rfc3339_opts(Millis, true) writes it.
func timestamp(ms int64) (string, error) {
	panic("unported: mj-transcript/src/github_history.rs timestamp")
}

// rowNotice is the number and repository of a message row of a notice type,
// which decide per pair.
type rowNotice struct {
	number *uint64
	repo   *string
}

// rowRender is one decoded row, rendered once for every pair that includes
// it.
type rowRender struct {
	// text is nil for kinds the transcript never shows.
	text   *string
	notice *rowNotice
	// created is the row's timestamp; createdOK is false when it is out of
	// range (Rust's Result<String, ()>).
	created   string
	createdOK bool
}

func renderRow(createdAtMs int64, b *OrderedValue) rowRender {
	panic("unported: mj-transcript/src/github_history.rs render_row")
}

// lineEndings turns `\r\n` and lone `\r` into `\n` across piece boundaries.
type lineEndings struct {
	afterCR bool
}

func (l *lineEndings) push(s string, sink TextSink) {
	panic("unported: mj-transcript/src/github_history.rs LineEndings::push")
}

// tee copies a pair's header while it is emitted.
type tee struct {
	sink TextSink
	copy *string
}

func (t *tee) PushStr(text string) {
	*t.copy += text
	t.sink.PushStr(text)
}

func (t *tee) Mark(*int)    {}
func (t *tee) IsFull() bool { return false }

type pairRender[S TextSink] struct {
	cutoffMs  int64
	repoLower string
	number    uint64
	expected  HistoryCounts
	seen      HistoryCounts
	endings   lineEndings
	sink      S
	// header is the header as emitted.
	header string
}

func (p *pairRender[S]) put(s string) {
	p.endings.push(s, p.sink)
}

func (p *pairRender[S]) putHeader(s string) {
	p.sink.Mark(nil)
	p.endings.push(s, &tee{sink: p.sink, copy: &p.header})
}

func (p *pairRender[S]) include(row *rowRender, ordinal int, createdAtMs, changedMs int64) error {
	panic("unported: mj-transcript/src/github_history.rs PairRender::include")
}

// slice collects a range of code points and the stamp that starts at one
// position.
type slice struct {
	from uint64
	to   uint64
	pos  uint64
	out  []byte
	// stampAt is nil when this slice collects no stamp.
	stampAt *uint64
	// stampState is 0 before the stamp, 1 inside it, 2 after its closing
	// bracket.
	stampState uint8
	stamp      []byte
}

func (s *slice) done() bool {
	return s.pos >= s.to && (s.stampAt == nil || s.stampState == 2)
}

func (s *slice) IsFull() bool { return s.done() }
func (s *slice) Mark(*int)    {}

func (s *slice) PushStr(text string) {
	panic("unported: mj-transcript/src/github_history.rs TextSink for Slice")
}

// RowFetch gives back the stored rows the passes saw.
type RowFetch interface {
	// Body is the body of the row the second pass was given under key.
	Body(key int64) (string, error)
}

// rowInfo is what the second pass knows about each row it was given, 24
// bytes a row and shared by every item: enough to tell which rows an item
// includes and to fetch the ones a window needs.
type rowInfo struct {
	key             int64
	createdAtMs     int64
	lastChangedAtMs int64
}

// pairFilter is one item's cutoff and notice rule, kept to render rows
// again.
type pairFilter struct {
	cutoffMs  int64
	repoLower string
	number    uint64
	header    string
}

// stampKey identifies the stamp a replay collects (Rust's
// (Option<usize>, u32, u32)): the pair when the window starts in the header,
// the window's ordinal, and the stamp's position in its part (the maximum
// uint32 when none).
type stampKey struct {
	headerPair    int
	hasHeaderPair bool
	ordinal       uint32
	stampAt       uint32
}

// replay is a window being rendered again.
type replay struct {
	pair    int
	window  int
	slice   slice
	endings lineEndings
	// next is the ordinal to look from for the next row this item includes.
	next   int
	chunks int
	// endingsStarted is whether the header was already replayed, so the
	// first row may be any included one.
	endingsStarted bool
	done           bool
	// stampKey is the key of the stamp this replay collects, if it is the
	// one to.
	stampKey stampKey
}

// HistoryResult is one item's windows with their recovered text.
type HistoryResult[D any] struct {
	Done  D
	Spans []Span
}

// HistoryPairSpec is one item's transcript view of a session: its cutoff,
// the item it is about, and where its rendered text goes.
type HistoryPairSpec[S any] struct {
	CutoffMs int64
	Repo     string
	Number   uint64
	Sink     S
}

// SessionHistory renders one session's rows for several items at once. Rows
// are read twice, in the same snapshot: CountRow over all of them fixes each
// header, then Row over them in `(position, stable_id)` order renders each
// exactly once and hands it to every pair that includes it. The rendered
// text is byte-identical to the whole-string rendering of the frozen format;
// it is never held whole. Complete then renders again, once, only the rows
// around the windows the sinks selected.
type SessionHistory[D any, S HistorySink[D]] struct {
	sessionID string
	pairs     []pairRender[S]
	started   bool
	rows      []rowInfo
	// notices is rows that may be item-specific notices: ordinal to number
	// and repository.
	notices map[int]rowNotice
}

// NewSessionHistory starts a session's history for pairs. D cannot be
// inferred from S, so callers name both (NewSessionHistory[struct{},
// *HistoryScanner]).
func NewSessionHistory[D any, S HistorySink[D]](sessionID string, pairs []HistoryPairSpec[S]) *SessionHistory[D, S] {
	renders := make([]pairRender[S], len(pairs))
	for i, pair := range pairs {
		renders[i] = pairRender[S]{cutoffMs: pair.CutoffMs, repoLower: strings.ToLower(pair.Repo), number: pair.Number, sink: pair.Sink}
	}
	return &SessionHistory[D, S]{sessionID: sessionID, pairs: renders, notices: map[int]rowNotice{}}
}

// MaxCutoffMs is the latest cutoff of any pair. A row created or changed
// after it is withheld from every pair, so it only needs counting. ok is
// false when there are no pairs.
func (h *SessionHistory[D, S]) MaxCutoffMs() (int64, bool) {
	if len(h.pairs) == 0 {
		return 0, false
	}
	latest := h.pairs[0].cutoffMs
	for _, pair := range h.pairs[1:] {
		latest = max(latest, pair.cutoffMs)
	}
	return latest, true
}

// CountRow is the first pass.
func (h *SessionHistory[D, S]) CountRow(row *RowMeta) {
	panic("unported: mj-transcript/src/github_history.rs SessionHistory::count_row")
}

// Begin writes every header; call once between the passes.
func (h *SessionHistory[D, S]) Begin() error {
	panic("unported: mj-transcript/src/github_history.rs SessionHistory::begin")
}

// Row is the second pass, over the rows in order. key is how RowFetch finds
// the row again. Rows nobody includes are not decoded; rows of a kind that
// never renders may be left out.
func (h *SessionHistory[D, S]) Row(key, createdAtMs, lastChangedAtMs int64, bodyJSON string) error {
	panic("unported: mj-transcript/src/github_history.rs SessionHistory::row")
}

// Complete ends every transcript, lets each sink select its windows, and
// renders the rows around the selected windows again, once for all of them,
// with fetch reading the same rows as the passes did. Fails when the second
// pass disagrees with the counts in the headers, which means the rows
// changed between the passes or the counting query drifted from the
// renderer. check is called often; an error from it ends the work.
func (h *SessionHistory[D, S]) Complete(fetch RowFetch, check func() error) ([]HistoryResult[D], error) {
	panic("unported: mj-transcript/src/github_history.rs SessionHistory::complete")
}

// HistoryScannerForItem is Rust's history_scanner (renamed: Go cannot give
// a function the name of the HistoryScanner type). A scanner for one item
// against one session. base must already be bounded and redacted once; the
// original item's full body is used only for retrieval. nil when the item
// has no searchable identifiers, which gives empty history.
func HistoryScannerForItem(original *core.GithubItem, base *core.GithubItemEvidence) (*HistoryScanner, error) {
	panic("unported: mj-transcript/src/github_history.rs history_scanner")
}

// BuildEvidence builds classifier-ready evidence from the spans a scanner
// retrieved. Diagnostics retain overflow rather than trimming base.
func BuildEvidence(base *core.GithubItemEvidence, spans []Span) (core.GithubItemEvidence, HistoryEvidence, error) {
	panic("unported: mj-transcript/src/github_history.rs build_evidence")
}

// StudentSegments is the training-format segments, item first, with history
// last in the session. Evidence is already bounded/redacted; serialization
// never bounds it again.
func StudentSegments(evidence *core.GithubItemEvidence) (string, string, error) {
	panic("unported: mj-transcript/src/github_history.rs student_segments")
}
