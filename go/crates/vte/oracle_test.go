package vte

import (
	"encoding/json"
	"hash/fnv"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The Rust side of these tests is the `vte` operation of
// go/port/oracle/src/vt100_oracle.rs, which runs vte 0.15.0. Both sides write
// one line for each Perform call and compare digests of the lines; the line
// formats here and there must be kept in step.

// vteLog is the performer of the comparison: it writes down every call.
type vteLog struct {
	lines []string
	// Calls since the driver last cleared the count.
	since int
	// Terminated holds once since reaches this; 0 means never.
	term int
}

func (self *vteLog) push(line string) {
	self.lines = append(self.lines, line)
	self.since++
}

func (self *vteLog) Print(c rune) {
	self.push("p " + strconv.FormatInt(int64(c), 16))
}

func (self *vteLog) Execute(byte_ uint8) {
	self.push("x " + hex2(byte_))
}

func (self *vteLog) Hook(params *Params, intermediates []uint8, ignore bool, action rune) {
	self.push("h " + paramsLine(params) + " " + esc(intermediates) + " " + bit(ignore) + " " + strconv.FormatInt(int64(action), 16))
}

func (self *vteLog) Put(byte_ uint8) {
	self.push("u " + hex2(byte_))
}

func (self *vteLog) Unhook() {
	self.push("U")
}

func (self *vteLog) OscDispatch(params [][]uint8, bellTerminated bool) {
	escaped := make([]string, len(params))
	for i, param := range params {
		escaped[i] = esc(param)
	}
	self.push("o " + strconv.Itoa(len(params)) + " " + strings.Join(escaped, "|") + " " + bit(bellTerminated))
}

func (self *vteLog) CsiDispatch(params *Params, intermediates []uint8, ignore bool, action rune) {
	self.push("c " + paramsLine(params) + " " + esc(intermediates) + " " + bit(ignore) + " " + strconv.FormatInt(int64(action), 16))
}

func (self *vteLog) EscDispatch(intermediates []uint8, ignore bool, byte_ uint8) {
	self.push("e " + esc(intermediates) + " " + bit(ignore) + " " + hex2(byte_))
}

func (self *vteLog) Terminated() bool {
	return self.term != 0 && self.since >= self.term
}

// paramsLine is `[1;2:3] len=3 empty=0`.
func paramsLine(params *Params) string {
	var out strings.Builder
	out.WriteByte('[')
	i := 0
	for iter := params.Iter(); ; i++ {
		param, ok := iter.Next().Get()
		if !ok {
			break
		}
		if i != 0 {
			out.WriteByte(';')
		}
		for j, subparam := range param {
			if j != 0 {
				out.WriteByte(':')
			}
			out.WriteString(strconv.Itoa(int(subparam)))
		}
	}
	out.WriteString("] len=" + strconv.Itoa(params.Len()) + " empty=" + bit(params.IsEmpty()))
	return out.String()
}

const hexDigits = "0123456789abcdef"

func hex2(byte_ uint8) string {
	return string([]byte{hexDigits[byte_>>4], hexDigits[byte_&15]})
}

func bit(value bool) string {
	if value {
		return "1"
	}
	return "0"
}

// esc writes bytes as printable ASCII: everything outside 0x20..=0x7e, the
// backslash and the separators `|` and `;` become `\xNN`.
func esc(bytes_ []uint8) string {
	out := make([]byte, 0, len(bytes_))
	for _, byte_ := range bytes_ {
		if byte_ >= 0x20 && byte_ <= 0x7e && byte_ != '\\' && byte_ != '|' && byte_ != ';' {
			out = append(out, byte_)
		} else {
			out = append(out, '\\', 'x', hexDigits[byte_>>4], hexDigits[byte_&15])
		}
	}
	return string(out)
}

// digest8 is the low 32 bits of the FNV-1a hash of the lines, as eight hex
// digits. With a separator it is written after the lines of each run.
type lineHash struct{ hash uint64 }

func newLineHash() lineHash { return lineHash{hash: 0xcbf29ce484222325} }

func (self *lineHash) write(text string) {
	for i := 0; i < len(text); i++ {
		self.hash ^= uint64(text[i])
		self.hash *= 0x100000001b3
	}
}

func (self *lineHash) lines(lines []string) {
	for _, line := range lines {
		self.write(line)
		self.write("\n")
	}
}

func (self *lineHash) digest8() string {
	const digits = "0123456789abcdef"
	value := uint32(self.hash)
	out := make([]byte, 8)
	for i := 7; i >= 0; i-- {
		out[i] = digits[value&15]
		value >>= 4
	}
	return string(out)
}

// runVteCase runs one case of a `cases` request: chunks passed to Advance,
// or, with a term, to AdvanceUntilTerminated until each is used up.
func runVteCase(chunks [][]byte, term int) []string {
	parser := ParserNew()
	log := &vteLog{term: term}
	for _, chunk := range chunks {
		if term == 0 {
			parser.Advance(log, chunk)
			continue
		}
		offset := 0
		for {
			read := parser.AdvanceUntilTerminated(log, chunk[offset:])
			log.lines = append(log.lines, "r "+strconv.Itoa(read))
			offset += read
			if offset >= len(chunk) {
				break
			}
			log.since = 0
		}
	}
	return log.lines
}

type vteMatrix struct {
	Prefix   []byte `json:"prefix"`
	Alphabet []byte `json:"alphabet"`
	Len      int    `json:"len"`
	Suffix   []byte `json:"suffix"`
	Split    bool   `json:"split"`
}

// runVteMatrix runs every sequence of 1 to Len bytes of the alphabet, as the
// oracle's vte_matrix does.
func runVteMatrix(spec vteMatrix) (calls int, each string) {
	var digests strings.Builder
	sequence := make([]byte, 0, spec.Len)
	for _, first := range spec.Alphabet {
		hash := newLineHash()
		for length := 1; length <= spec.Len; length++ {
			combinations := 1
			for i := 1; i < length; i++ {
				combinations *= len(spec.Alphabet)
			}
			for index := 0; index < combinations; index++ {
				sequence = sequence[:length]
				sequence[0] = first
				rest := index
				for position := length - 1; position >= 1; position-- {
					sequence[position] = spec.Alphabet[rest%len(spec.Alphabet)]
					rest /= len(spec.Alphabet)
				}
				masks := 1
				if spec.Split {
					masks = 1 << (length - 1)
				}
				for mask := 0; mask < masks; mask++ {
					parser := ParserNew()
					log := &vteLog{}
					parser.Advance(log, spec.Prefix)
					start := 0
					for gap := 1; gap < length; gap++ {
						if mask&(1<<(gap-1)) != 0 {
							parser.Advance(log, sequence[start:gap])
							start = gap
						}
					}
					parser.Advance(log, sequence[start:])
					parser.Advance(log, spec.Suffix)
					hash.lines(log.lines)
					hash.write("--\n")
					calls += len(log.lines)
				}
			}
		}
		digests.WriteString(hash.digest8())
	}
	return calls, digests.String()
}

type vteCase struct {
	Chunks [][]byte `json:"c"`
	Term   int      `json:"t"`
}

// MarshalJSON writes a case without a term as the bare list of its chunks,
// which keeps the recorded corpus small.
func (self vteCase) MarshalJSON() ([]byte, error) {
	if self.Term == 0 {
		return json.Marshal(self.Chunks)
	}
	type plain vteCase
	return json.Marshal(plain(self))
}

func (self *vteCase) UnmarshalJSON(text []byte) error {
	if len(text) > 0 && text[0] == '[' {
		self.Term = 0
		return json.Unmarshal(text, &self.Chunks)
	}
	type plain vteCase
	return json.Unmarshal(text, (*plain)(self))
}

type vteRequest struct {
	Op     string     `json:"op"`
	Cases  []vteCase  `json:"cases,omitempty"`
	Matrix *vteMatrix `json:"matrix,omitempty"`
	Full   bool       `json:"full,omitempty"`
}

func answerVte(request json.RawMessage) any {
	var decoded vteRequest
	if err := json.Unmarshal(request, &decoded); err != nil {
		panic(err)
	}
	if decoded.Matrix != nil {
		calls, each := runVteMatrix(*decoded.Matrix)
		return map[string]any{"n": calls, "each": each}
	}
	var each strings.Builder
	calls := 0
	logs := []string{}
	for _, c := range decoded.Cases {
		lines := runVteCase(c.Chunks, c.Term)
		hash := newLineHash()
		hash.lines(lines)
		each.WriteString(hash.digest8())
		calls += len(lines)
		logs = append(logs, strings.Join(lines, "\n"))
	}
	reply := map[string]any{"n": calls, "each": each.String()}
	if decoded.Full {
		reply["logs"] = logs
	}
	return reply
}

// The bytes after which the parser's states can be told apart: parameter
// characters, an intermediate, finals, the string terminators and a cancel.
var matrixSuffix = []byte("1;2:3 !pA\x07\x9c\x1b\\\x18z")

// One prefix for each state of the parser, and for the variations of a state
// that carry different parameters and intermediates into it.
var statePrefixes = []string{
	"",                // Ground
	"\x1b",            // Escape
	"\x1b ",           // EscapeIntermediate
	"\x1b #",          // EscapeIntermediate, two collected
	"\x1b #$",         // EscapeIntermediate, ignoring
	"\x1b[",           // CsiEntry
	"\x1b[1",          // CsiParam
	"\x1b[1;",         // CsiParam after a separator
	"\x1b[1:2",        // CsiParam in subparameters
	"\x1b[?",          // CsiParam with a private marker
	"\x1b[?25",        // CsiParam with a private marker and a parameter
	"\x1b[ ",          // CsiIntermediate
	"\x1b[5 ",         // CsiIntermediate after a parameter
	"\x1b[1 2",        // CsiIgnore
	"\x1b[1?",         // CsiIgnore
	"\x1bP",           // DcsEntry
	"\x1bP1",          // DcsParam
	"\x1bP1;2:3",      // DcsParam
	"\x1bP$",          // DcsIntermediate
	"\x1bP$1",         // DcsIgnore
	"\x1bP1$q",        // DcsPassthrough
	"\x1b]",           // OscString
	"\x1b]0;ab",       // OscString with a parameter
	"\x1b]5;6;7;8;9;", // OscString with several parameters
	"\x1bX",           // SosPmApcString (SOS)
	"\x1b^",           // SosPmApcString (PM)
	"\x1b_",           // SosPmApcString (APC)
	"\xc3",            // Ground with one byte of a two-byte character held
	"\xe2\x9c",        // Ground with two bytes of a three-byte character held
	"\xf0\x9f\x9a",    // Ground with three bytes of a four-byte character held
	strings.Repeat("\x1b[1;2:3", 3) + strings.Repeat("9;", 31), // CsiParam with the parameter list full
	"\x1b]" + strings.Repeat("x;", 16),                         // OscString with the parameter list full
}

// A byte of every class the states tell apart, with the ends of each range.
var classBytes = []byte{
	0x00, 0x07, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1f, 0x20, 0x2f, 0x30, 0x39, 0x3a, 0x3b,
	0x3c, 0x3f, 0x40, 0x4f, 0x50, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60,
	0x7e, 0x7f, 0x80, 0x9b, 0x9c, 0x9f, 0xa0, 0xc2, 0xe2, 0xf0, 0xff,
}

// The bytes UTF-8 decoding branches on: the ends of each range of lead and
// continuation bytes, with an ASCII letter and the escape character.
var utf8Bytes = []byte{
	0x41, 0x1b, 0x80, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc2, 0xe0, 0xe1, 0xed, 0xf0, 0xf1, 0xf4, 0xf5,
}

func allBytes() []byte {
	all := make([]byte, 256)
	for i := range all {
		all[i] = byte(i)
	}
	return all
}

// A stream builder: sequences the parser knows, malformed ones, text and
// noise, drawn with a seeded generator.
type streamBuilder struct {
	random *rand.Rand
	out    []byte
}

var textSamples = []string{
	"hello", "The quick brown fox", "a", " ", "~", "\x7f",
	"é", "ñ", "ß", "Ω", "я", "ש", "ع", "ह", "ก", "한", "デ", "ネ", "中文", "ｆｕｌｌ",
	"e\u0301", "a\u0308\u0304", "\u200b", "\u200d", "\ufe0f", "\u00ad", "🎉", "🦀", "👨\u200d👩\u200d👧", "🇫🇷",
	"\u17d8", "\ufffd", "\u0085", "\u009b", "\u2028",
}

func (self *streamBuilder) number() string {
	switch self.random.IntN(8) {
	case 0:
		return ""
	case 1:
		return "0"
	case 2:
		return strconv.Itoa(self.random.IntN(10))
	case 3:
		return strconv.Itoa(self.random.IntN(300))
	case 4:
		return strconv.Itoa(self.random.IntN(70000))
	case 5:
		return "99999999999999999999"
	default:
		return strconv.Itoa(1 + self.random.IntN(40))
	}
}

func (self *streamBuilder) params() string {
	var out strings.Builder
	count := self.random.IntN(5)
	if self.random.IntN(30) == 0 {
		count = 28 + self.random.IntN(10)
	}
	for i := 0; i < count; i++ {
		if i != 0 {
			if self.random.IntN(5) == 0 {
				out.WriteByte(':')
			} else {
				out.WriteByte(';')
			}
		}
		out.WriteString(self.number())
	}
	return out.String()
}

func (self *streamBuilder) randomBytes(limit int) []byte {
	out := make([]byte, self.random.IntN(limit)+1)
	for i := range out {
		out[i] = byte(self.random.IntN(256))
	}
	return out
}

func (self *streamBuilder) fragment() {
	random := self.random
	switch choice := random.IntN(100); {
	case choice < 30:
		self.out = append(self.out, textSamples[random.IntN(len(textSamples))]...)
	case choice < 38:
		self.out = append(self.out, byte(random.IntN(0x20)))
	case choice < 42:
		// C1 controls, raw and as UTF-8.
		if random.IntN(2) == 0 {
			self.out = append(self.out, byte(0x80+random.IntN(0x20)))
		} else {
			self.out = append(self.out, 0xc2, byte(0x80+random.IntN(0x20)))
		}
	case choice < 60:
		// CSI
		self.out = append(self.out, 0x1b, '[')
		if random.IntN(4) == 0 {
			self.out = append(self.out, "<=>?"[random.IntN(4)])
		}
		self.out = append(self.out, self.params()...)
		for i := random.IntN(4) / 2; i > 0; i-- {
			self.out = append(self.out, byte(0x20+random.IntN(0x10)))
		}
		if random.IntN(12) != 0 {
			self.out = append(self.out, byte(0x40+random.IntN(0x3f)))
		}
	case choice < 68:
		// ESC
		self.out = append(self.out, 0x1b)
		for i := random.IntN(5) / 2; i > 0; i-- {
			self.out = append(self.out, byte(0x20+random.IntN(0x10)))
		}
		if random.IntN(12) != 0 {
			self.out = append(self.out, byte(0x30+random.IntN(0x4f)))
		}
	case choice < 78:
		// OSC
		self.out = append(self.out, 0x1b, ']')
		for i := random.IntN(4); i > 0; i-- {
			if random.IntN(3) == 0 {
				self.out = append(self.out, self.randomBytes(6)...)
			} else {
				self.out = append(self.out, textSamples[random.IntN(len(textSamples))]...)
			}
			if random.IntN(3) != 0 {
				self.out = append(self.out, ';')
			}
		}
		if random.IntN(30) == 0 {
			self.out = append(self.out, strings.Repeat("p;", 20)...)
		}
		self.terminator()
	case choice < 84:
		// DCS
		self.out = append(self.out, 0x1b, 'P')
		self.out = append(self.out, self.params()...)
		if random.IntN(3) == 0 {
			self.out = append(self.out, byte(0x20+random.IntN(0x10)))
		}
		if random.IntN(10) != 0 {
			self.out = append(self.out, byte(0x40+random.IntN(0x3f)))
		}
		if random.IntN(2) == 0 {
			self.out = append(self.out, self.randomBytes(8)...)
		} else {
			self.out = append(self.out, textSamples[random.IntN(len(textSamples))]...)
		}
		self.terminator()
	case choice < 88:
		// SOS, PM, APC
		self.out = append(self.out, 0x1b, "X^_"[random.IntN(3)])
		self.out = append(self.out, textSamples[random.IntN(len(textSamples))]...)
		self.terminator()
	case choice < 94:
		self.out = append(self.out, self.randomBytes(6)...)
	default:
		// A character cut short.
		sample := textSamples[6+random.IntN(len(textSamples)-6)]
		self.out = append(self.out, sample[:1+random.IntN(len(sample))]...)
	}
}

func (self *streamBuilder) terminator() {
	switch self.random.IntN(8) {
	case 0:
		self.out = append(self.out, 0x1b, '\\')
	case 1:
		self.out = append(self.out, 0x9c)
	case 2:
		self.out = append(self.out, 0x18)
	case 3:
		self.out = append(self.out, 0x1a)
	case 4:
		// Left open.
	default:
		self.out = append(self.out, 0x07)
	}
}

// chunks cuts the stream at random places, sometimes at every byte.
func chunks(random *rand.Rand, stream []byte) [][]byte {
	out := [][]byte{}
	switch random.IntN(4) {
	case 0:
		return append(out, stream)
	case 1:
		for i := range stream {
			out = append(out, stream[i:i+1])
		}
		return out
	}
	for len(stream) > 0 {
		size := 1 + random.IntN(1+len(stream))
		if size > len(stream) {
			size = len(stream)
		}
		out = append(out, stream[:size])
		stream = stream[size:]
		if random.IntN(10) == 0 {
			out = append(out, []byte{})
		}
	}
	return out
}

func randomVteCases(seed uint64, count int, fragments int) []vteCase {
	random := rand.New(rand.NewPCG(seed, 0x767465))
	cases := make([]vteCase, 0, count)
	for i := 0; i < count; i++ {
		builder := streamBuilder{random: random}
		for n := 1 + random.IntN(fragments); n > 0; n-- {
			builder.fragment()
		}
		c := vteCase{Chunks: chunks(random, builder.out)}
		if random.IntN(6) == 0 {
			c.Term = 1 + random.IntN(4)
		}
		cases = append(cases, c)
	}
	return cases
}

func generateVte(yield func(any)) {
	// The inputs of the crate's own tests and a few that show a behaviour
	// worth reading, with their logs.
	fixed := []vteCase{}
	for _, input := range []string{
		string(oscBytes),
		"\x1b]\x07",
		"\x1b]" + strings.Repeat(";", paramsMaxParams+1) + "\x1b",
		"\x1b]11;ff/00/ff\x07",
		"\x1b]11;ff/00/ff\x1b\\",
		"\x1b]2;\xe6\x9c\xab\x1b\\",
		"\x1b[" + strings.Repeat("1;", paramsMaxParams-1) + "p",
		"\x1b[" + strings.Repeat("1;", paramsMaxParams) + "p",
		"\x1b[4;m", "\x1b[;4m", "\x1b[9223372036854775808m", "\x1b[3;1\x1b[?1049h",
		"\x1b[38:2:255:0:255;1m",
		"\x1bP" + strings.Repeat("1;", paramsMaxParams+1) + "p",
		"\x1b[3;1\x1bP1$tx\x9c", "\x1bP0;1|17/ab\x9c", "\x1bP=1sZZZ\x1b+\x5c", "\x1b[3;1\x1b(A",
		"\x1b[?2004l\x1b#8", "\x1b[::::::::::::::::::::::::::::::::x\x1b",
		"\xF0\x9F\x8E\x89_\xF0\x9F\xA6\x80\xF0\x9F\xA6\x80_\xF0\x9F\x8E\x89", "a\xEF\xBCb", "\xD8\x1b012",
		"\x00\x1f\x80\x90\x98\x9b\x9c\x9d\x9e\x9fa", "\x18\x1a",
		"", "\x7f", "\xc2\x80\xc2\x9f\xc2\xa0", "\x1b\x1b[m", "\x1b[1\x18m", "\x1b_app\x07still\x9cinside\x1b\\out",
	} {
		fixed = append(fixed, vteCase{Chunks: [][]byte{[]byte(input)}})
	}
	fixed = append(fixed,
		// Split characters.
		vteCase{Chunks: [][]byte{{0xF0}, {0x9F}, {0x9A}, {0x80}}},
		vteCase{Chunks: [][]byte{{0xC4}, {0xB8, 0xF0, 0x9F, 0x8E, 0x89}}},
		vteCase{Chunks: [][]byte{{'a'}, {0xEF}, {0xBC}, {'b'}}},
		vteCase{Chunks: [][]byte{{0xE4, 0xBF}, {0x99, 0xB5}}},
		// The character after a split one is dropped when an invalid byte
		// follows it within the three bytes the parser looks ahead.
		vteCase{Chunks: [][]byte{{0xC3}, {0xA9, 'A', 0xFF}}},
		vteCase{Chunks: [][]byte{{0xC3}, {0xA9, 'A', 'B', 0xFF}}},
		vteCase{Chunks: [][]byte{{0xC3}, {}, {0xA9}}},
		vteCase{Chunks: [][]byte{[]byte("abc\x1b[31mdef\x1bPq12\x1b\\xyz")}, Term: 2},
		vteCase{Chunks: [][]byte{[]byte("ab\xe2"), []byte("\x9c\x93c\x1b[1m")}, Term: 1},
	)
	yield(vteRequest{Op: "vte", Cases: fixed, Full: true})

	// Every byte, and every pair of bytes, from the ground state.
	yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte{}, Alphabet: allBytes(), Len: 2, Suffix: matrixSuffix, Split: true}})
	// Every byte from every state, and every pair of class bytes.
	for _, prefix := range statePrefixes {
		yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte(prefix), Alphabet: allBytes(), Len: 1, Suffix: matrixSuffix}})
		yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte(prefix), Alphabet: classBytes, Len: 2, Suffix: matrixSuffix, Split: true}})
	}
	// Three class bytes after an escape and inside a control sequence.
	for _, prefix := range []string{"\x1b", "\x1b["} {
		yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte(prefix), Alphabet: classBytes, Len: 3, Suffix: matrixSuffix, Split: true}})
	}
	// Every sequence of up to four of the bytes UTF-8 decoding branches on,
	// cut into chunks in every way.
	yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte{}, Alphabet: utf8Bytes, Len: 4, Suffix: []byte("z"), Split: true}})

	// Generated streams.
	live := os.Getenv("MJ_ORACLE") == "live"
	batches, seed := 30, uint64(1)
	if live {
		batches, seed = 400, 1000
	}
	for batch := 0; batch < batches; batch++ {
		fragments := 6
		if batch%3 == 2 {
			fragments = 40
		}
		yield(vteRequest{Op: "vte", Cases: randomVteCases(seed+uint64(batch), 100, fragments)})
	}
	if live {
		// Larger matrices than the recorded corpus holds.
		for _, prefix := range statePrefixes {
			yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte(prefix), Alphabet: classBytes, Len: 3, Suffix: matrixSuffix, Split: true}})
		}
		yield(vteRequest{Op: "vte", Matrix: &vteMatrix{Prefix: []byte("\x1b]0;"), Alphabet: utf8Bytes, Len: 4, Suffix: []byte("\x07z"), Split: true}})
	}
}

func TestVteAgainstRust(t *testing.T) {
	oracle.Check(t, "vt100", "vte", generateVte, answerVte)
}

// The lines hashed by this file are FNV-1a, as the oracle's are.
func TestLineHashIsFnv1a(t *testing.T) {
	want := fnv.New64a()
	want.Write([]byte("p 61\nx 0a\n"))
	got := newLineHash()
	got.lines([]string{"p 61", "x 0a"})
	if got.hash != want.Sum64() {
		t.Fatalf("lineHash %x, hash/fnv %x", got.hash, want.Sum64())
	}
}
