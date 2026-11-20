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
	"errors"
	"fmt"
	"hash/fnv"
	"io"
	"math"
	"regexp"
	"slices"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"golang.org/x/text/cases"
	"golang.org/x/text/language"

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
	var text strings.Builder
	v.display(&text)
	return text.String()
}

func (v *OrderedValue) display(text *strings.Builder) {
	switch v.Kind {
	case OrderedValueObject:
		text.WriteByte('{')
		for i := range v.Object {
			if i > 0 {
				text.WriteByte(',')
			}
			text.WriteString(serdeString(v.Object[i].Key))
			text.WriteByte(':')
			v.Object[i].Value.display(text)
		}
		text.WriteByte('}')
	case OrderedValueArray:
		text.WriteByte('[')
		for i := range v.Array {
			if i > 0 {
				text.WriteByte(',')
			}
			v.Array[i].display(text)
		}
		text.WriteByte(']')
	case OrderedValueNumber:
		text.WriteString(pythonNumber(v.Number))
	default:
		encoded, err := v.MarshalJSON()
		if err != nil {
			panic(fmt.Sprintf("serialize a JSON scalar: %v", err))
		}
		text.Write(encoded)
	}
}

// serdeString is a string as serde_json writes it.
func serdeString(s string) string {
	encoded, err := jsonvalue.Marshal(s)
	if err != nil {
		panic(fmt.Sprintf("serialize a JSON string: %v", err))
	}
	return string(encoded)
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
	token, err := decoder.Token()
	if err != nil {
		return OrderedValue{}, err
	}
	switch token := token.(type) {
	case nil:
		return OrderedValue{}, nil
	case bool:
		return OrderedValue{Kind: OrderedValueBool, Bool: token}, nil
	case string:
		return OrderedValue{Kind: OrderedValueString, Str: token}, nil
	case json.Number:
		number, err := orderedNumber(string(token))
		if err != nil {
			return OrderedValue{}, err
		}
		return OrderedValue{Kind: OrderedValueNumber, Number: number}, nil
	case json.Delim:
		switch token {
		case '[':
			array := []OrderedValue{}
			for decoder.More() {
				value, err := decodeOrderedValue(decoder)
				if err != nil {
					return OrderedValue{}, err
				}
				array = append(array, value)
			}
			if _, err := decoder.Token(); err != nil {
				return OrderedValue{}, err
			}
			return OrderedValue{Kind: OrderedValueArray, Array: array}, nil
		case '{':
			object := []OrderedMember{}
			for decoder.More() {
				keyToken, err := decoder.Token()
				if err != nil {
					return OrderedValue{}, err
				}
				key, ok := keyToken.(string)
				if !ok {
					return OrderedValue{}, fmt.Errorf("key must be a string")
				}
				value, err := decodeOrderedValue(decoder)
				if err != nil {
					return OrderedValue{}, err
				}
				// indexmap's insert keeps a repeated key at its first
				// position with its last value.
				replaced := false
				for i := range object {
					if object[i].Key == key {
						object[i].Value = value
						replaced = true
						break
					}
				}
				if !replaced {
					object = append(object, OrderedMember{Key: key, Value: value})
				}
			}
			if _, err := decoder.Token(); err != nil {
				return OrderedValue{}, err
			}
			return OrderedValue{Kind: OrderedValueObject, Object: object}, nil
		}
	}
	return OrderedValue{}, fmt.Errorf("unexpected JSON token %v", token)
}

// orderedNumber reads a JSON number as serde_json (with float_roundtrip)
// holds it: an integer that fits u64 as uint64, a negative one that fits
// i64 as int64, and anything else as a correctly rounded float64. "-0" is
// the float -0.0, as serde_json reads it.
func orderedNumber(literal string) (any, error) {
	if !strings.ContainsAny(literal, ".eE") && literal != "-0" {
		if strings.HasPrefix(literal, "-") {
			if value, err := strconv.ParseInt(literal, 10, 64); err == nil {
				return value, nil
			}
		} else if value, err := strconv.ParseUint(literal, 10, 64); err == nil {
			return value, nil
		}
	}
	value, err := strconv.ParseFloat(literal, 64)
	if err != nil && !errors.Is(err, strconv.ErrRange) {
		return nil, err
	}
	if math.IsInf(value, 0) {
		return nil, fmt.Errorf("number out of range")
	}
	return value, nil
}

// pythonNumber: Python repr uses the same shortest round-trip digits, but a
// different scientific-notation threshold, explicit exponent signs and
// two-digit exponents. number is an OrderedValue's Number.
func pythonNumber(number any) string {
	var raw string
	switch number := number.(type) {
	case uint64:
		return strconv.FormatUint(number, 10)
	case int64:
		return strconv.FormatInt(number, 10)
	case float64:
		// The shortest round-trip digits, as serde_json's ryu writes them;
		// only the digits and the exponent are read below.
		raw = strconv.FormatFloat(number, 'e', -1, 64)
	default:
		panic(fmt.Sprintf("an ordered JSON number is %T", number))
	}
	sign, unsigned := "", raw
	if rest, ok := strings.CutPrefix(raw, "-"); ok {
		sign, unsigned = "-", rest
	}
	mantissa, exponent := unsigned, 0
	if m, e, ok := strings.Cut(unsigned, "e"); ok {
		parsed, err := strconv.Atoi(e)
		if err != nil {
			panic("JSON exponent")
		}
		mantissa, exponent = m, parsed
	}
	decimal := strings.IndexByte(mantissa, '.')
	if decimal < 0 {
		decimal = len(mantissa)
	}
	digits := strings.ReplaceAll(mantissa, ".", "")
	leading := len(digits) - len(strings.TrimLeft(digits, "0"))
	if leading == len(digits) {
		return sign + "0.0"
	}
	exponent = exponent + decimal - leading - 1
	digits = strings.TrimRight(digits[leading:], "0")
	if exponent < -4 || exponent >= 16 {
		mantissa := digits
		if len(digits) > 1 {
			mantissa = digits[:1] + "." + digits[1:]
		}
		exponentSign := "+"
		if exponent < 0 {
			exponentSign = "-"
		}
		return fmt.Sprintf("%s%se%s%02d", sign, mantissa, exponentSign, absInt(exponent))
	}
	position := exponent + 1
	switch {
	case position <= 0:
		return sign + "0." + strings.Repeat("0", -position) + digits
	case position >= len(digits):
		return sign + digits + strings.Repeat("0", position-len(digits)) + ".0"
	default:
		return sign + digits[:position] + "." + digits[position:]
	}
}

func absInt(value int) int {
	if value < 0 {
		return -value
	}
	return value
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
	switch v.Kind {
	case OrderedValueObject:
		members := make([]string, len(v.Object))
		for i := range v.Object {
			members[i] = serdeString(v.Object[i].Key) + ": " + spaced(&v.Object[i].Value)
		}
		return "{" + strings.Join(members, ", ") + "}"
	case OrderedValueArray:
		values := make([]string, len(v.Array))
		for i := range v.Array {
			values[i] = spaced(&v.Array[i])
		}
		return "[" + strings.Join(values, ", ") + "]"
	}
	return v.String()
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
	titleValue := item.Index("title")
	title, _ := titleValue.AsStr()
	bodyValue := item.Index("body")
	body, _ := bodyValue.AsStr()
	text := title + "\n" + body
	patterns := historyIdentifierPatterns()
	var ids []historyIdentifier
	put := func(s string, weight float64) {
		for i := range ids {
			if ids[i].text == s {
				ids[i].weight = weight
				return
			}
		}
		if s != "" {
			ids = append(ids, historyIdentifier{text: s, weight: weight})
		}
	}
	isWord := func(c rune) bool { return core.Contains(core.Word, c) }
	// Boundaries are checked separately so a rejected match still advances
	// as Python does.
	for _, m := range patterns.issue.FindAllStringSubmatchIndex(text, -1) {
		next, size := utf8.DecodeRuneInString(text[m[1]:])
		if size == 0 || !isWord(next) {
			put(text[m[2]:m[3]], 1.3)
		}
	}
	for _, m := range patterns.path.FindAllStringIndex(text, -1) {
		match := text[m[0]:m[1]]
		// The eight code points before the match and its first three.
		from := m[0]
		for taken := 0; taken < 8 && from > 0; taken++ {
			_, size := utf8.DecodeLastRuneInString(text[:from])
			from -= size
		}
		to := m[0]
		for taken := 0; taken < 3 && to < len(text); taken++ {
			_, size := utf8.DecodeRuneInString(text[to:])
			to += size
		}
		nearby := text[from:to]
		if strings.Contains(nearby, "://") || patterns.hash.MatchString(match) {
			continue
		}
		put(match, 1.2)
		put(match[strings.LastIndexByte(match, '/')+1:], 1.0)
	}
	for _, a := range patterns.dotted.FindAllString(text, -1) {
		if patterns.hexOnly.MatchString(a) {
			continue
		}
		for _, s := range append([]string{a}, strings.Split(a, ".")...) {
			if len(s) >= 6 && (strings.Contains(s, "_") || patterns.camel.MatchString(s)) {
				put(s, 1.0)
			}
			bits := strings.Split(s, "_")
			if len(bits) > 2 {
				if p := strings.Join(bits[:2], "_"); len(p) >= 6 {
					put(p, 0.7)
				}
			}
		}
	}
	for _, pass := range []struct {
		source  string
		pattern *regexp.Regexp
		weight  float64
	}{
		{text, patterns.file, 1.0},
		{title, patterns.acronym, 0.5},
	} {
		source := pass.source
		offset := 0
		for offset <= len(source) {
			found := pass.pattern.FindStringIndex(source[offset:])
			if found == nil {
				break
			}
			start, end := offset+found[0], offset+found[1]
			previous, size := utf8.DecodeLastRuneInString(source[:start])
			if (size == 0 || !isWord(previous)) && patterns.right.MatchString(source[end:]) {
				put(source[start:end], pass.weight)
				offset = end
			} else {
				// Python's boundary is inside the regex: a rejected start
				// must not consume a later valid filename within this
				// candidate.
				_, width := utf8.DecodeRuneInString(source[start:])
				offset = start + width
			}
		}
	}
	return ids
}

// historyPatterns is the frozen expressions identifiers uses, compiled once.
type historyPatterns struct {
	issue, path, hash, dotted, hexOnly, camel, file, acronym, right *regexp.Regexp
}

var historyIdentifierPatterns = sync.OnceValue(func() historyPatterns {
	word := core.Class(core.Word)
	decimal := core.Class(core.Decimal)
	w := word[1 : len(word)-1]
	boundary := "(?:$|[^" + w + "])"
	return historyPatterns{
		issue:   re("#(" + decimal + "+)"),
		path:    re("[" + w + ".-]+(?:/[" + w + ".:-]+)+"),
		hash:    re("[a-f0-9]{16,}"),
		dotted:  re(`[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)*`),
		hexOnly: re("^[a-fA-F0-9]{7,}$"),
		camel:   re("[a-z][A-Z]"),
		file:    re("[A-Za-z_][" + w + `-]*\.(?:rs|py|sql|json|tsv)`),
		acronym: re("[A-Z]{3,}"),
		right:   re("^" + boundary),
	}
})

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
	a := &historyAutomaton{next: []map[byte]uint32{{}}, fail: []uint32{0}, matches: [][]int{nil}}
	for id, pattern := range patterns {
		state := uint32(0)
		for i := 0; i < len(pattern); i++ {
			next, ok := a.next[state][pattern[i]]
			if !ok {
				next = uint32(len(a.next))
				a.next = append(a.next, map[byte]uint32{})
				a.fail = append(a.fail, 0)
				a.matches = append(a.matches, nil)
				a.next[state][pattern[i]] = next
			}
			state = next
		}
		a.matches[state] = append(a.matches[state], id)
	}
	// Breadth-first failure links. With standard match semantics a state
	// also reports the matches of its failure state, after its own.
	queue := make([]uint32, 0, len(a.next))
	for _, child := range sortedTransitions(a.next[0]) {
		a.fail[child.state] = 0
		queue = append(queue, child.state)
	}
	for len(queue) > 0 {
		state := queue[0]
		queue = queue[1:]
		for _, child := range sortedTransitions(a.next[state]) {
			fail := a.fail[state]
			for {
				if next, ok := a.next[fail][child.b]; ok {
					a.fail[child.state] = next
					break
				}
				if fail == 0 {
					a.fail[child.state] = 0
					break
				}
				fail = a.fail[fail]
			}
			a.matches[child.state] = append(a.matches[child.state], a.matches[a.fail[child.state]]...)
			queue = append(queue, child.state)
		}
	}
	return a
}

// automatonTransition is one goto transition of a state.
type automatonTransition struct {
	b     byte
	state uint32
}

// sortedTransitions lists a state's transitions by byte, so the automaton
// is built the same way every time.
func sortedTransitions(next map[byte]uint32) []automatonTransition {
	transitions := make([]automatonTransition, 0, len(next))
	for b, state := range next {
		transitions = append(transitions, automatonTransition{b, state})
	}
	slices.SortFunc(transitions, func(x, y automatonTransition) int { return cmp.Compare(x.b, y.b) })
	return transitions
}

func (a *historyAutomaton) startState() uint32 {
	return 0
}

func (a *historyAutomaton) nextState(state uint32, b byte) uint32 {
	for {
		if next, ok := a.next[state][b]; ok {
			return next
		}
		if state == 0 {
			return 0
		}
		state = a.fail[state]
	}
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
	// ring holds the code points [seen - ringLen, seen), a circular buffer
	// of ringCap code points whose oldest is at ringHead. Large enough for a
	// window that starts 374 code points before a hit that ends one
	// identifier length later, plus the boundary character before the hit.
	ring     []rune
	ringHead int
	ringLen  int
	ringCap  int
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
	info := make([]idState, len(ids))
	longest := 0
	for i, id := range ids {
		numeric := true
		for _, c := range id.text {
			if !core.Contains(core.Isdigit, c) {
				numeric = false
				break
			}
		}
		info[i] = idState{cps: utf8.RuneCountInString(id.text), numeric: numeric}
		longest = max(longest, info[i].cps)
	}
	ringCap := longest + 400
	var automaton *historyAutomaton
	var state uint32
	if len(ids) != 0 {
		patterns := make([]string, len(ids))
		for i, id := range ids {
			patterns[i] = id.text
		}
		automaton = newHistoryAutomaton(patterns)
		state = automaton.startState()
	}
	conversation := session.Index("conversation")
	existing, _ := conversation.AsStr()
	return &HistoryScanner{
		existing:  existing,
		known:     newKnown(chars(spaced(session))),
		ids:       ids,
		info:      info,
		automaton: automaton,
		state:     state,
		ring:      make([]rune, ringCap),
		ringCap:   ringCap,
		lineStart: true,
	}
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
	return s.ring[(s.ringHead+index-(s.seen-s.ringLen))%s.ringCap]
}

// ringPush appends a code point, dropping the oldest when the ring is full.
func (s *HistoryScanner) ringPush(c rune) {
	if s.ringLen == s.ringCap {
		s.ring[s.ringHead] = c
		s.ringHead = (s.ringHead + 1) % s.ringCap
		return
	}
	s.ring[(s.ringHead+s.ringLen)%s.ringCap] = c
	s.ringLen++
}

// ringText is the code points [from, to) as text; both lie in the ring.
func (s *HistoryScanner) ringText(from, to int) []byte {
	text := make([]byte, 0, to-from)
	for index := from; index < to; index++ {
		text = utf8.AppendRune(text, s.ringAt(index))
	}
	return text
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
	hasPrevious := pos > 0
	var previous rune
	if hasPrevious {
		previous = s.ringAt(pos - 1)
	}
	var ok bool
	if s.info[id].numeric {
		ok = (!hasPrevious || !core.Contains(core.Isdigit, previous)) &&
			(!hasNext || !core.Contains(core.Isdigit, next))
	} else {
		ok = (!hasPrevious || !(core.Contains(core.Isalnum, previous) || previous == '_')) &&
			(!hasNext || !core.Contains(core.Isalnum, next))
	}
	if !ok {
		return
	}
	s.info[id].hits++
	start := max(pos/250*250-125, 0)
	key := uint32(start)
	at, found := s.find(key)
	if found {
		atBuilding, building := slices.BinarySearchFunc(s.building, key, func(b buildingWindow, key uint32) int {
			return cmp.Compare(b.start, key)
		})
		if !building {
			// Cannot happen before a window closes; keep it correct anyway.
			set := slices.Clone(s.sets.sets[s.live[at].ids])
			s.live[at].ids = buildingSet
			s.building = slices.Insert(s.building, atBuilding, buildingWindow{start: key, ids: set})
		}
		set := &s.building[atBuilding].ids
		if index, present := slices.BinarySearch(*set, uint32(id)); !present {
			*set = slices.Insert(*set, index, uint32(id))
		}
		return
	}
	stop := min(s.seen, start+500)
	text := s.ringText(start, stop)
	length := openLen
	if stop-start >= 500 {
		judged, ok := judge(s.existing, &s.known, string(text))
		if !ok {
			return
		}
		length = judged
	}
	ordinal, offset := headerOrdinal, uint32(start)
	for i := len(s.segments) - 1; i >= 0; i-- {
		if s.segments[i].at <= start {
			ordinal, offset = s.segments[i].ordinal, uint32(start-s.segments[i].at)
			break
		}
	}
	stamp := noStamp
	for i := len(s.stamps) - 1; i >= 0; i-- {
		if s.stamps[i] <= start {
			stamp = uint32(s.stamps[i])
			break
		}
	}
	if s.stampOpen != nil && s.stampOpen.pos <= start {
		s.awaitingStamp = append(s.awaitingStamp, key)
	}
	// A longer identifier's hit can start before a shorter one's.
	n := sort.Search(len(s.building), func(i int) bool { return s.building[i].start >= key })
	s.building = slices.Insert(s.building, n, buildingWindow{start: key, ids: []uint32{uint32(id)}})
	s.live = slices.Insert(s.live, at, candidate{
		start:   key,
		stamp:   stamp,
		ordinal: ordinal,
		offset:  offset,
		ids:     buildingSet,
		len:     length,
	})
	if length == openLen {
		s.open = append(s.open, openWindow{start: key, text: text})
		s.peakTexts = max(s.peakTexts, len(s.open))
	}
}

// closeWindows interns the identifiers of windows no later hit can reach: a
// hit ends at most ringCap - 400 code points (the longest identifier) past a
// start it can reach by 374.
func (s *HistoryScanner) closeWindows(all bool) {
	reach := s.ringCap - 400 + 376
	for len(s.building) > 0 {
		front := s.building[0]
		if !all && int(front.start)+reach > s.seen {
			break
		}
		s.building = s.building[1:]
		if at, found := s.find(front.start); found {
			s.live[at].ids = s.sets.intern(front.ids)
		}
	}
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
	if uint64(s.seen) >= math.MaxUint32-1 {
		s.tooLong = true
		return
	}
	// resolve never touches pending; keep its allocation across code points.
	pending := s.pending
	s.pending = nil
	for _, hit := range pending {
		s.resolve(hit.id, hit.pos, c, true)
	}
	s.pending = pending[:0]
	// Stamps: `(?m)^\[([^\]\n]+)\]`.
	if s.stampOpen != nil {
		if c == ']' || c == '\n' {
			pos, content := s.stampOpen.pos, s.stampOpen.content
			s.stampOpen = nil
			awaiting := s.awaitingStamp
			s.awaitingStamp = nil
			if c == ']' && content {
				for _, start := range awaiting {
					if at, found := s.find(start); found {
						s.live[at].stamp = uint32(pos)
					}
				}
				s.stamps = append(s.stamps, pos)
				for len(s.stamps) >= 2 && s.stamps[1]+s.ringCap <= s.seen {
					s.stamps = s.stamps[1:]
				}
			}
		} else {
			s.stampOpen.content = true
		}
	} else if s.lineStart && c == '[' {
		s.stampOpen = &stampProgress{pos: s.seen}
	}
	s.lineStart = c == '\n'
	s.ringPush(c)
	for i := 0; i < len(s.open); {
		window := &s.open[i]
		window.text = utf8.AppendRune(window.text, c)
		if s.seen+1-int(window.start) >= 500 {
			finished := *window
			s.open = slices.Delete(s.open, i, i+1)
			s.settle(finished.start, string(finished.text))
		} else {
			i++
		}
	}
	s.seen++
	s.closeWindows(false)
	if s.automaton != nil {
		var buffer [utf8.UTFMax]byte
		for _, b := range buffer[:utf8.EncodeRune(buffer[:], c)] {
			s.state = s.automaton.nextState(s.state, b)
			for _, id := range s.automaton.matches[s.state] {
				info := &s.info[id]
				pos := s.seen - info.cps
				// Per identifier, matches are leftmost and do not overlap,
				// whether or not the boundary checks accept them.
				if pos >= info.lastEnd {
					info.lastEnd = s.seen
					s.pending = append(s.pending, historyHit{id: id, pos: pos})
				}
			}
		}
	}
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
	if s.tooLong {
		return nil, errors.New("the transcript is longer than 4 billion code points")
	}
	pending := s.pending
	s.pending = nil
	for _, hit := range pending {
		s.resolve(hit.id, hit.pos, 0, false)
	}
	open := s.open
	s.open = nil
	for _, window := range open {
		s.settle(window.start, string(window.text))
	}
	s.closeWindows(true)
	total := s.seen
	// Rust uses libm's log1p, a port of fdlibm, rather than the platform's
	// C library, whose log1p is not correctly rounded and differs by an ulp
	// between platforms. Go's math.Log1p is the same fdlibm algorithm.
	weights := make([]float64, len(s.ids))
	for i, id := range s.ids {
		weights[i] = id.weight * (1.0 + math.Log1p(max(float64(total)/500.0, 1.0)/float64(1+s.info[i].hits)))
	}
	// Weights are summed in identifier order, once per distinct set.
	setWeights := make([]float64, len(s.sets.sets))
	for i, set := range s.sets.sets {
		sum := 0.0
		for _, id := range set {
			sum += weights[id]
		}
		setWeights[i] = sum
	}
	type rankedWindow struct {
		index int
		score float64
	}
	ranked := make([]rankedWindow, 0, len(s.live))
	for n, candidate := range s.live {
		if n%1024 == 0 {
			if err := check(); err != nil {
				return nil, err
			}
		}
		start := int(candidate.start)
		score := setWeights[candidate.ids] * (1.0 + 0.1*float64(start)/float64(max(total, 1)))
		ranked = append(ranked, rankedWindow{index: n, score: score})
	}
	if err := check(); err != nil {
		return nil, err
	}
	slices.SortStableFunc(ranked, func(a, b rankedWindow) int {
		if order := totalCompare(b.score, a.score); order != 0 {
			return order
		}
		return cmp.Compare(s.live[b.index].start, s.live[a.index].start)
	})
	var selected []Selected
	used := 0
	for n, window := range ranked {
		if n%1024 == 0 {
			if err := check(); err != nil {
				return nil, err
			}
		}
		candidate := s.live[window.index]
		start := int(candidate.start)
		end := start + min(500, total-start)
		if used+int(candidate.len) > 2100 || slices.ContainsFunc(selected, func(other Selected) bool {
			return start < other.End && end > other.Start
		}) {
			continue
		}
		used += int(candidate.len)
		var stamp *int
		if candidate.stamp != noStamp {
			at := int(candidate.stamp)
			stamp = &at
		}
		selected = append(selected, Selected{
			Start:   start,
			End:     end,
			Score:   window.score,
			Stamp:   stamp,
			Ordinal: candidate.ordinal,
			Offset:  candidate.offset,
		})
	}
	slices.SortStableFunc(selected, func(a, b Selected) int { return cmp.Compare(a.Start, b.Start) })
	return selected, nil
}

// totalCompare is Rust's f64::total_cmp.
func totalCompare(a, b float64) int {
	x, y := int64(math.Float64bits(a)), int64(math.Float64bits(b))
	x ^= int64(uint64(x>>63) >> 1)
	y ^= int64(uint64(y>>63) >> 1)
	return cmp.Compare(x, y)
}

// spansFromWholeText is the spans of selected windows whose text is cut out
// of a whole transcript.
func spansFromWholeText(selected []Selected, text string) []Span {
	characters := []rune(text)
	spans := make([]Span, 0, len(selected))
	for _, s := range selected {
		window := string(characters[min(s.Start, len(characters)):min(s.End, len(characters))])
		var stampTime *string
		if s.Stamp != nil {
			var stamp strings.Builder
			for _, c := range characters[min(*s.Stamp+1, len(characters)):] {
				if c == ']' {
					break
				}
				stamp.WriteRune(c)
			}
			value := stamp.String()
			stampTime = &value
		}
		spans = append(spans, Span{
			Start: s.Start,
			End:   s.End,
			Score: s.Score,
			Snippet: Snippet{
				Time: stampTime,
				Text: Redact(trimHistorySpace(window)),
			},
		})
	}
	return spans
}

// trimHistorySpace trims the Python whitespace class from both ends.
func trimHistorySpace(text string) string {
	return strings.TrimFunc(text, func(c rune) bool { return core.Contains(core.Space, c) })
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
	tokenizer, err := core.HistoryTokenizer()
	if err != nil {
		return HistoryEvidence{}, err
	}
	count := func(a string, b *string, special bool) (int, error) {
		var ids []uint32
		var err error
		if b != nil {
			first, second := tokenizersInput(a, *b)
			ids, err = tokenizer.Encode(first, second, special)
		} else {
			ids, err = tokenizer.Encode(a, nil, special)
		}
		if err != nil {
			return 0, err
		}
		return len(ids), nil
	}
	originalSnippets := len(work)
	work = slices.Clone(work)
	for i := range work {
		if work[i].Snippet.Time != nil {
			redacted := Redact(*work[i].Snippet.Time)
			work[i].Snippet.Time = &redacted
		}
	}
	sessionValue := base.Index("session")
	source, ok := sessionValue.AsObject()
	if !ok {
		return HistoryEvidence{}, errors.New("session must be object")
	}
	session := OrderedValue{Kind: OrderedValueObject, Object: []OrderedMember{}}
	insert := func(key string, value OrderedValue) {
		for i := range session.Object {
			if session.Object[i].Key == key {
				session.Object[i].Value = value
				return
			}
		}
		session.Object = append(session.Object, OrderedMember{Key: key, Value: value})
	}
	has := func(key string) bool {
		return slices.ContainsFunc(session.Object, func(member OrderedMember) bool { return member.Key == key })
	}
	for _, key := range []string{"conversation", "files", "references", "mentions"} {
		if value, ok := orderedMember(source, key); ok {
			insert(key, value)
		}
	}
	var extra []string
	for _, member := range source {
		if !has(member.Key) && member.Key != "history" {
			extra = append(extra, member.Key)
		}
	}
	sort.Strings(extra)
	for _, key := range extra {
		value, _ := orderedMember(source, key)
		insert(key, value)
	}
	itemValue := base.Index("item")
	item := itemValue.String()
	sessionText := session.String()
	baseTokens, err := count(item, &sessionText, true)
	if err != nil {
		return HistoryEvidence{}, err
	}
	var removed []HistoryRemoval
	for {
		history := OrderedValue{Kind: OrderedValueArray, Array: make([]OrderedValue, len(work))}
		for i := range work {
			stamp := OrderedValue{}
			if work[i].Snippet.Time != nil {
				stamp = OrderedValue{Kind: OrderedValueString, Str: *work[i].Snippet.Time}
			}
			history.Array[i] = OrderedValue{Kind: OrderedValueObject, Object: []OrderedMember{
				{Key: "time", Value: stamp},
				{Key: "text", Value: OrderedValue{Kind: OrderedValueString, Str: work[i].Snippet.Text}},
			}}
		}
		historyTokens, err := count(history.String(), nil, false)
		if err != nil {
			return HistoryEvidence{}, err
		}
		insert("history", history)
		sessionText := session.String()
		total, err := count(item, &sessionText, true)
		if err != nil {
			return HistoryEvidence{}, err
		}
		if len(work) == 0 || (historyTokens <= 800 && total <= 7999) {
			return HistoryEvidence{
				Evidence: OrderedValue{Kind: OrderedValueObject, Object: []OrderedMember{
					{Key: "item", Value: itemValue},
					{Key: "session", Value: session},
				}},
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
		for i := 1; i < len(work); i++ {
			order := totalCompare(work[i].Score, work[worst].Score)
			if order == 0 {
				order = cmp.Compare(work[i].Start, work[worst].Start)
			}
			if order < 0 {
				worst = i
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

// orderedMember is an object member's value by key.
func orderedMember(object []OrderedMember, key string) (OrderedValue, bool) {
	for i := range object {
		if object[i].Key == key {
			return object[i].Value, true
		}
	}
	return OrderedValue{}, false
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
	if ms < chronoMinMillis || ms > chronoMaxMillis {
		return "", errors.New("timestamp out of range")
	}
	t := time.UnixMilli(ms).UTC()
	year := t.Year()
	var text strings.Builder
	if year >= 0 && year <= 9999 {
		fmt.Fprintf(&text, "%04d", year)
	} else {
		// ISO 8601 requires the explicit sign for out-of-range years.
		fmt.Fprintf(&text, "%+05d", year)
	}
	fmt.Fprintf(&text, "-%02d-%02dT%02d:%02d:%02d.%03dZ",
		int(t.Month()), t.Day(), t.Hour(), t.Minute(), t.Second(), t.Nanosecond()/int(time.Millisecond))
	return text.String(), nil
}

// chronoMinMillis and chronoMaxMillis bound what chrono's
// DateTime::from_timestamp_millis accepts: its dates run from year -262143
// to the end of year 262142.
var (
	chronoMinMillis = time.Date(-262143, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli()
	chronoMaxMillis = time.Date(262143, time.January, 1, 0, 0, 0, 0, time.UTC).UnixMilli() - 1
)

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
	str := func(v OrderedValue) string {
		text, _ := v.AsStr()
		return text
	}
	at := func(v OrderedValue, keys ...string) OrderedValue {
		for _, key := range keys {
			v = v.Index(key)
		}
		return v
	}
	var notice *rowNotice
	var text *string
	kind := b.Index("kind")
	kindText, _ := kind.AsStr()
	switch kindText {
	case "user":
		var parts []string
		content := b.Index("content")
		items, _ := content.AsArray()
		for _, x := range items {
			if kind := x.Index("type"); kind.EqStr("text") {
				parts = append(parts, str(x.Index("text")))
			}
		}
		rendered := "USER\n" + strings.Join(parts, "\n")
		text = &rendered
	case "agent":
		var joined strings.Builder
		chunks := b.Index("chunks")
		items, _ := chunks.AsArray()
		for _, x := range items {
			if kind := at(x, "content", "type"); kind.EqStr("text") {
				joined.WriteString(str(at(x, "content", "text")))
			}
		}
		rendered := "AGENT\n" + joined.String()
		text = &rendered
	case "tool":
		call := b.Index("call")
		kindValue := call.Index("kind")
		callKind, ok := kindValue.AsStr()
		if !ok || callKind == "" {
			callKind = "call"
		}
		rendered := fmt.Sprintf("TOOL %s\nTITLE: %s\nCALL: %s",
			cases.Upper(language.Und).String(callKind), str(call.Index("title")), call.String())
		text = &rendered
	case "message":
		body := at(*b, "event", "body")
		typValue := body.Index("type")
		typ, _ := typValue.AsStr()
		if typ == "new_github_item" || strings.HasPrefix(typ, "github_") {
			notice = &rowNotice{}
			numberValue, repoValue := body.Index("number"), body.Index("repo")
			if number, ok := numberValue.AsU64(); ok {
				notice.number = &number
			}
			if repo, ok := repoValue.AsStr(); ok {
				notice.repo = &repo
			}
		}
		rendered := fmt.Sprintf("PEER MESSAGE %s\n%s", str(at(body, "from", "title")), str(body.Index("text")))
		text = &rendered
	}
	created, err := timestamp(createdAtMs)
	return rowRender{text: text, notice: notice, created: created, createdOK: err == nil}
}

// lineEndings turns `\r\n` and lone `\r` into `\n` across piece boundaries.
type lineEndings struct {
	afterCR bool
}

func (l *lineEndings) push(s string, sink TextSink) {
	if l.afterCR && s != "" {
		l.afterCR = false
		s = strings.TrimPrefix(s, "\n")
	}
	for !sink.IsFull() {
		i := strings.IndexByte(s, '\r')
		if i < 0 {
			break
		}
		sink.PushStr(s[:i])
		sink.PushStr("\n")
		s = s[i+1:]
		if rest, ok := strings.CutPrefix(s, "\n"); ok {
			s = rest
		} else if s == "" {
			l.afterCR = true
		}
	}
	if !sink.IsFull() {
		sink.PushStr(s)
	}
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
	if createdAtMs > p.cutoffMs || changedMs > p.cutoffMs {
		return nil
	}
	if row.text == nil {
		return nil
	}
	if row.notice != nil && noticeNamesItem(row.notice.number, row.notice.repo, p.number, p.repoLower) {
		p.seen.Notices++
		return nil
	}
	if p.seen.Included > 0 {
		p.put("\n\n")
	}
	p.seen.Included++
	if !row.createdOK {
		return errors.New("timestamp out of range")
	}
	p.sink.Mark(&ordinal)
	p.put("[")
	p.put(row.created)
	p.put("] ")
	p.put(*row.text)
	return nil
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
	for _, c := range text {
		if s.done() {
			return
		}
		if s.from <= s.pos && s.pos < s.to {
			s.out = utf8.AppendRune(s.out, c)
		}
		if s.stampState == 1 {
			if c == ']' {
				s.stampState = 2
			} else {
				s.stamp = utf8.AppendRune(s.stamp, c)
			}
		} else if s.stampState == 0 && s.stampAt != nil && *s.stampAt == s.pos {
			s.stampState = 1
		}
		s.pos++
	}
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
	for i := range h.pairs {
		pair := &h.pairs[i]
		counts := &pair.expected
		switch {
		case row.CreatedAtMs > pair.cutoffMs:
			counts.Later++
		case row.LastChangedAtMs > pair.cutoffMs:
			counts.Changed++
		case row.Kind == "message":
			if isNoticeType(row.EventType) && noticeNamesItem(row.EventNumber, row.EventRepo, pair.number, pair.repoLower) {
				counts.Notices++
			} else {
				counts.Included++
			}
		case kindRenders(row.Kind):
			counts.Included++
		}
	}
}

// Begin writes every header; call once between the passes.
func (h *SessionHistory[D, S]) Begin() error {
	for i := range h.pairs {
		pair := &h.pairs[i]
		c := pair.expected
		cutoff, err := timestamp(pair.cutoffMs)
		if err != nil {
			return err
		}
		header := fmt.Sprintf("Session transcript as of %s (UTC)\nSession id: %s\nTranscript rows included: %d; withheld because changed after cutoff: %d; excluded because created later: %d; item-specific GitHub mailbox notices removed: %d.\nRows come only from Mjolnir materialized transcript records; timestamps are row creation times.\n\n",
			cutoff, h.sessionID, c.Included, c.Changed, c.Later, c.Notices)
		pair.putHeader(header)
	}
	h.started = true
	return nil
}

// Row is the second pass, over the rows in order. key is how RowFetch finds
// the row again. Rows nobody includes are not decoded; rows of a kind that
// never renders may be left out.
func (h *SessionHistory[D, S]) Row(key, createdAtMs, lastChangedAtMs int64, bodyJSON string) error {
	if !h.started {
		return errors.New("history rows rendered before the headers")
	}
	info := rowInfo{key: key, createdAtMs: createdAtMs, lastChangedAtMs: lastChangedAtMs}
	if !slices.ContainsFunc(h.pairs, func(p pairRender[S]) bool {
		return createdAtMs <= p.cutoffMs && lastChangedAtMs <= p.cutoffMs
	}) {
		h.rows = append(h.rows, info)
		return nil
	}
	var body OrderedValue
	if err := json.Unmarshal([]byte(bodyJSON), &body); err != nil {
		return err
	}
	rendered := renderRow(createdAtMs, &body)
	if rendered.text == nil {
		return nil
	}
	ordinal := len(h.rows)
	h.rows = append(h.rows, info)
	if rendered.notice != nil {
		h.notices[ordinal] = *rendered.notice
	}
	for i := range h.pairs {
		if err := h.pairs[i].include(&rendered, ordinal, createdAtMs, lastChangedAtMs); err != nil {
			return err
		}
	}
	return nil
}

// Complete ends every transcript, lets each sink select its windows, and
// renders the rows around the selected windows again, once for all of them,
// with fetch reading the same rows as the passes did. Fails when the second
// pass disagrees with the counts in the headers, which means the rows
// changed between the passes or the counting query drifted from the
// renderer. check is called often; an error from it ends the work.
func (h *SessionHistory[D, S]) Complete(fetch RowFetch, check func() error) ([]HistoryResult[D], error) {
	rows, notices := h.rows, h.notices
	filters := make([]pairFilter, 0, len(h.pairs))
	dones := make([]D, 0, len(h.pairs))
	selections := make([][]Selected, 0, len(h.pairs))
	for i := range h.pairs {
		pair := &h.pairs[i]
		if err := check(); err != nil {
			return nil, err
		}
		if pair.seen.Included != pair.expected.Included || pair.seen.Notices != pair.expected.Notices {
			return nil, fmt.Errorf("history header counts %s disagree with the rendered rows %s",
				historyCountsDebug(pair.expected), historyCountsDebug(pair.seen))
		}
		pair.put("\n")
		filters = append(filters, pairFilter{cutoffMs: pair.cutoffMs, repoLower: pair.repoLower, number: pair.number, header: pair.header})
		done, selected, err := pair.sink.Select(check)
		if err != nil {
			return nil, err
		}
		dones = append(dones, done)
		selections = append(selections, selected)
	}
	included := func(pair *pairFilter, ordinal int) bool {
		row := &rows[ordinal]
		if row.createdAtMs > pair.cutoffMs || row.lastChangedAtMs > pair.cutoffMs {
			return false
		}
		notice, ok := notices[ordinal]
		return !(ok && noticeNamesItem(notice.number, notice.repo, pair.number, pair.repoLower))
	}
	// One replay per window; windows with the same stamp share one
	// collector.
	var replays []replay
	collectors := map[stampKey]bool{}
	for p, selected := range selections {
		for w, s := range selected {
			header := s.Ordinal == headerOrdinal
			segment := s.Start - int(s.Offset)
			var stampAt *uint64
			if s.Stamp != nil {
				if *s.Stamp < segment {
					return nil, errors.New("a window's stamp lies before its part of the transcript")
				}
				at := uint64(*s.Stamp - segment)
				stampAt = &at
			}
			key := stampKey{ordinal: s.Ordinal, stampAt: math.MaxUint32}
			if header {
				key.headerPair, key.hasHeaderPair = p, true
			}
			if stampAt != nil {
				key.stampAt = uint32(*stampAt)
			}
			collect := false
			if stampAt != nil && !collectors[key] {
				collectors[key] = true
				collect = true
			}
			r := replay{
				pair:   p,
				window: w,
				slice: slice{
					from: uint64(s.Offset),
					to:   uint64(int(s.Offset) + s.End - s.Start),
				},
				endingsStarted: header,
				stampKey:       key,
			}
			if collect {
				r.slice.stampAt = stampAt
			}
			if !header {
				r.next = int(s.Ordinal)
			}
			if header {
				r.endings.push(filters[p].header, &r.slice)
			}
			replays = append(replays, r)
		}
	}
	for {
		if err := check(); err != nil {
			return nil, err
		}
		target := -1
		for i := range replays {
			r := &replays[i]
			if r.done {
				continue
			}
			if r.slice.done() {
				r.done = true
				continue
			}
			filter := &filters[r.pair]
			anchor := r.next
			for r.next < len(rows) && !included(filter, r.next) {
				r.next++
			}
			if !(r.chunks > 0 || r.endingsStarted || r.next == anchor || r.next >= len(rows)) {
				return nil, errors.New("a window's row is not one its item includes")
			}
			if r.next < len(rows) {
				if target < 0 || r.next < target {
					target = r.next
				}
			} else {
				// No more rows: the transcript ends with a newline.
				r.endings.push("\n", &r.slice)
				r.done = true
			}
		}
		if target < 0 {
			break
		}
		body, err := fetch.Body(rows[target].key)
		if err != nil {
			return nil, err
		}
		var value OrderedValue
		if err := json.Unmarshal([]byte(body), &value); err != nil {
			return nil, err
		}
		rendered := renderRow(rows[target].createdAtMs, &value)
		if rendered.text == nil {
			return nil, fmt.Errorf("history row %d no longer renders", target)
		}
		if !rendered.createdOK {
			return nil, errors.New("timestamp out of range")
		}
		for i := range replays {
			r := &replays[i]
			if r.done || r.next != target {
				continue
			}
			if r.chunks > 0 {
				r.endings.push("\n\n", &r.slice)
			}
			for _, piece := range []string{"[", rendered.created, "] ", *rendered.text} {
				if r.slice.done() {
					break
				}
				r.endings.push(piece, &r.slice)
			}
			r.chunks++
			r.next++
		}
	}
	stamps := map[stampKey]string{}
	for i := range replays {
		r := &replays[i]
		if !(r.slice.pos >= r.slice.to && (r.slice.stampAt == nil || r.slice.stampState == 2)) {
			return nil, errors.New("history rows changed between reading them and recovering a window")
		}
		if r.slice.stampAt != nil {
			stamps[r.stampKey] = string(r.slice.stamp)
			r.slice.stamp = nil
		}
	}
	results := make([]HistoryResult[D], len(dones))
	for i, done := range dones {
		results[i] = HistoryResult[D]{Done: done}
	}
	for i := range replays {
		r := &replays[i]
		s := selections[r.pair][r.window]
		var stampTime *string
		if s.Stamp != nil {
			stamp := stamps[r.stampKey]
			stampTime = &stamp
		}
		results[r.pair].Spans = append(results[r.pair].Spans, Span{
			Start: s.Start,
			End:   s.End,
			Score: s.Score,
			Snippet: Snippet{
				Time: stampTime,
				Text: Redact(trimHistorySpace(string(r.slice.out))),
			},
		})
	}
	return results, nil
}

// historyCountsDebug is HistoryCounts as Rust's derived Debug writes it.
func historyCountsDebug(c HistoryCounts) string {
	return fmt.Sprintf("HistoryCounts { included: %d, changed: %d, later: %d, notices: %d }",
		c.Included, c.Changed, c.Later, c.Notices)
}

// HistoryScannerForItem is Rust's history_scanner (renamed: Go cannot give
// a function the name of the HistoryScanner type). A scanner for one item
// against one session. base must already be bounded and redacted once; the
// original item's full body is used only for retrieval. nil when the item
// has no searchable identifiers, which gives empty history.
func HistoryScannerForItem(original *core.GithubItem, base *core.GithubItemEvidence) (*HistoryScanner, error) {
	originalValue, err := orderedValueOf(original)
	if err != nil {
		return nil, err
	}
	baseValue, err := orderedValueOf(base)
	if err != nil {
		return nil, err
	}
	// An item without searchable identifiers can never select a snippet.
	ids := identifiers(&originalValue)
	if len(ids) == 0 {
		return nil, nil
	}
	session := baseValue.Index("session")
	return historyScannerWithIdentifiers(ids, &session), nil
}

// orderedValueOf is serde_json::from_str::<OrderedValue>(&serde_json::to_string(value)).
func orderedValueOf(value any) (OrderedValue, error) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return OrderedValue{}, err
	}
	var ordered OrderedValue
	err = json.Unmarshal(encoded, &ordered)
	return ordered, err
}

// serdeJSON writes value as serde_json::to_string does. encoding/json
// escapes <, >, &, U+2028 and U+2029, which serde_json writes as they are,
// and formats floats its own way, so its output is read back and written
// again in serde_json's form.
func serdeJSON(value any) (string, error) {
	ordered, err := orderedValueOf(value)
	if err != nil {
		return "", err
	}
	encoded, err := ordered.MarshalJSON()
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

// BuildEvidence builds classifier-ready evidence from the spans a scanner
// retrieved. Diagnostics retain overflow rather than trimming base.
func BuildEvidence(base *core.GithubItemEvidence, spans []Span) (core.GithubItemEvidence, HistoryEvidence, error) {
	baseValue, err := orderedValueOf(base)
	if err != nil {
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

// StudentSegments is the training-format segments, item first, with history
// last in the session. Evidence is already bounded/redacted; serialization
// never bounds it again.
func StudentSegments(evidence *core.GithubItemEvidence) (string, string, error) {
	item, err := serdeJSON(evidence.Item)
	if err != nil {
		return "", "", err
	}
	session, err := serdeJSON(evidence.Session)
	if err != nil {
		return "", "", err
	}
	return item, session, nil
}
