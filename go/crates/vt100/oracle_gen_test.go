package vt100

import (
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
)

// This file makes the requests of the comparison with Rust: a few cases
// written by hand, matrices that try every sequence the terminal knows from
// a set of prepared screens, and streams drawn from a seeded generator.

// --- prepared screens -------------------------------------------------------

type preparedScreen struct {
	rows, cols uint16
	scrollback int
	resize     bool
	setup      string
}

// The text every matrix sequence is followed by: it shows where the cursor
// went, which attributes are in force, and whether rows wrap or scroll.
const matrixTail = "Z\x1b[7méデ\u0301\tq\r\n!"

var preparedScreens = []preparedScreen{
	// An empty screen of the usual size.
	{rows: 24, cols: 80, scrollback: 0, setup: ""},
	// Text in colours, wide and combining characters, two rows scrolled off,
	// the cursor in the middle of a row.
	{rows: 5, cols: 10, scrollback: 4, setup: "\x1b[31mred\x1b[42mgrn\x1b[0m plain\r\n" +
		"デネ e\u0301\r\nthird row wraps here\r\nfour\r\nfive\x1b[3;4H\x1b[1;4;38;5;200m"},
	// A scroll region with origin mode, the cursor at the bottom of it.
	{rows: 6, cols: 8, scrollback: 3, setup: "r1\r\nr2\r\nr3\r\nr4\r\nr5\r\nr6\x1b[2;4r\x1b[?6h\x1b[3;3H\x1b[1;44m"},
	// The alternate screen, entered with a saved cursor.
	{rows: 4, cols: 6, scrollback: 5, setup: "main\r\nscreen\r\nrows\r\nscroll\r\noff\x1b[2;3H\x1b[35m\x1b[?1049halt\r\nab\x1b[3;2H"},
	// The cursor past the last column of the last row, the row above wrapped.
	{rows: 3, cols: 4, scrollback: 2, setup: "\r\nabcdefgh"},
	// Wide characters against the right edge, the cursor on a second half.
	{rows: 3, cols: 5, scrollback: 0, setup: "aデネ\r\nデネb\x1b[1;3H"},
	// Every mode on, and every attribute.
	{rows: 4, cols: 10, scrollback: 0, setup: "\x1b=\x1b[?1;25;1003;1006;2004h\x1b[?25l\x1b[2;3;4;7;38;2;1;2;3;48;5;17mmodes\x1b7"},
	// A screen the application resized, which cut a wide character in two.
	{rows: 4, cols: 8, scrollback: 3, resize: true, setup: "abデネデ\r\nsecond\x1b[8;3;5t\x1b[1;1H"},
	// The smallest screens.
	{rows: 1, cols: 1, scrollback: 0, setup: ""},
	{rows: 1, cols: 6, scrollback: 2, setup: "ab"},
	{rows: 5, cols: 1, scrollback: 2, setup: "a\r\nb"},
	{rows: 2, cols: 2, scrollback: 1, setup: "a\r\nb"},
}

// The parameter strings of the control sequence matrix: nothing, the values
// the terminal gives a meaning to, values past the size of any screen, lists
// and subparameters. The largest values are left to explicitSequences,
// because the matrix gives every parameter to every final byte, and
// inserting cells (`CSI n @`) takes time that grows with the square of n in
// Rust as it does here: 65535 of them take seconds.
var matrixParams = []string{
	"", "0", "1", "2", "3", "4", "5", "8", "10", "25", "47", "100", "999", "1000", "1049", "2004", "4095",
	"1;1", "2;3", "3;2", "0;0", "5;", ";5", "4;100", "1:2", "8;3;4", "8;0;0", "8;;", "2;4;6", "6;1049",
}

// A shorter list, for the screens that are not given the whole matrix in the
// recorded corpus.
var matrixParamsShort = []string{"", "0", "1", "2", "3", "5", "100", "2;3", "8;3;4", "1:2"}

func explicitSequences() [][]byte {
	sequences := [][]byte{}
	add := func(sequence string) { sequences = append(sequences, []byte(sequence)) }

	// Every SGR parameter on its own, and the colour forms.
	for n := 0; n <= 110; n++ {
		add("\x1b[" + strconv.Itoa(n) + "m")
	}
	for _, params := range []string{
		"38;5;0", "38;5;7", "38;5;8", "38;5;15", "38;5;16", "38;5;255", "38;5;256", "38;5", "38;5;", "38;5;1;1",
		"48;5;0", "48;5;9", "48;5;200", "48;5;300", "48;5",
		"38;2;1;2;3", "38;2;255;255;255", "38;2;256;0;0", "38;2;0;256;0", "38;2;0;0;256", "38;2;1;2", "38;2", "38;2;1:1;2;3",
		"48;2;9;8;7", "48;2;1;2;300", "48;2;;;",
		"38:5:100", "38:5:300", "38:2:1:2:3", "38:2:1:2:300", "38:2::1:2:3", "38:2:1:2", "38:5", "38:9:1",
		"48:5:100", "48:2:1:2:3", "48:2:1:2:3:4", "48:5:1:2",
		"38", "48", "38;9", "48;9", "38;1", "38;5:1",
		"1;2;3;4;7", "1;22", "2;1", "3;23", "4;24", "7;27", "1;3;4;7;0", "31;42;1", "90;100", "97;107", "39;49",
		"1:2", "0:0", "4:3", "31:1", "999", "0;999;1", "256", "65535",
		"38;5;1;48;5;2;1", "38;2;1;2;3;48;2;4;5;6;7",
	} {
		add("\x1b[" + params + "m")
	}

	// The private modes, set and reset, alone and in lists.
	for _, mode := range []string{
		"1", "6", "9", "25", "47", "1000", "1002", "1003", "1005", "1006", "1049", "2004",
		"7", "12", "1004", "2026", "0", "65535",
		"1;25", "1049;6", "47;1049", "9;1000;1002;1003", "1005;1006", "1:2", "25:1", "1;999;2004", "",
	} {
		add("\x1b[?" + mode + "h")
		add("\x1b[?" + mode + "l")
		add("\x1b[?" + mode + "h\x1b[?" + mode + "l")
		add("\x1b[?" + mode + "l\x1b[?" + mode + "h")
	}
	add("\x1b[?1049h\x1b[?1049h")
	add("\x1b[?47h\x1b[?1049l")
	add("\x1b[?1049hX\x1b[?47l")
	add("\x1b[?9h\x1b[?1000l")
	add("\x1b[?1000h\x1b[?9l")
	add("\x1b[?1005h\x1b[?1006l")

	// Every escape sequence, bare and with intermediates.
	for final := 0x30; final <= 0x7e; final++ {
		add("\x1b" + string(rune(final)))
	}
	for _, intermediates := range []string{" ", "#", "(", "()", "(#$"} {
		for _, final := range []string{"0", "7", "8", "A", "B", "M", "c", "g", "~"} {
			add("\x1b" + intermediates + final)
		}
	}
	add("\x1b7\x1b[5;5H\x1b[31m\x1b8")
	add("\x1b8")
	add("\x1b7\x1bc\x1b8")
	add("\x1b[5;5H\x1b7\x1b[?1049h\x1b[2;2H\x1b7\x1b[?1049l\x1b8")

	// Every control character, as a byte and as the UTF-8 of a C1 control.
	for control := 0; control < 0x20; control++ {
		sequences = append(sequences, []byte{byte(control)})
	}
	sequences = append(sequences, []byte{0x7f})
	for control := 0x80; control < 0xa0; control++ {
		sequences = append(sequences, []byte{byte(control)})
		sequences = append(sequences, []byte{0xc2, byte(control)})
	}
	add("\n\n\n\n\n\n\n")
	add("\t\t\t")
	add("\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08")
	add("\r\n\r\n\r\n\r\n\r\n\r\n\r\n")
	add("\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM")

	// Operating system commands.
	for _, body := range []string{
		"", "0", "0;", "0;title", "1;icon", "2;title", "0;a;b", "2;デ\xff", "3;x", "00;x", "2", "1",
		"52;c;?", "52;c;YWJj", "52;c;YWJj==", "52;;", "52;;?", "52;cpqs01234567;QQ==", "52;x;?", "52;c;!!", "52;c", "52;c;a;b",
		"52;c;", "4;1;rgb:00/11/22", "8;;http://example.test", "10;?", "133;A",
	} {
		add("\x1b]" + body + "\x07")
		add("\x1b]" + body + "\x1b\\")
	}
	add("\x1b]0;open")
	add("\x1b]0;cancelled\x18")
	add("\x1b]2;c1 terminator\x9c\x07")

	// Device control strings and the other strings, which are ignored.
	add("\x1bPq#0;2;0;0;0#0~~@@vv@@~~@@~~$\x1b\\")
	add("\x1bP1$r0m\x9c")
	add("\x1b_application program command\x1b\\")
	add("\x1b^privacy message\x07still inside\x1b\\")
	add("\x1bXstart of string\x18")

	// Text.
	for _, text := range []string{
		"a", "ab", "abcdefghijklmnopqrstuvwxyz", strings.Repeat("0123456789", 9), " ", "~",
		"é", "e\u0301", "e\u0301\u0302\u0303\u0304\u0305\u0306\u0307\u0308\u0309\u030a", "\u0301", "\r\n\u0301",
		"デ", "デネ", "aデ", "デ\u0301", "デネデネデネデネ",
		"\U0001F389", "\U0001F468\u200d\U0001F469\u200d\U0001F467", "\U0001F1EB\U0001F1F7", "❤\ufe0f", "\u17d8", "\u17d8\u17d8\u17d8\u17d8",
		"\u00ad", "\u200b", "\ufeff", "\ufffd", "\u0085", "\u2028", "\U0010FFFF", "\ue000",
		"\xff", "\xc3", "\xe3\x83", "\xf0\x9f\x8e", "a\xe3\x83b", "\xed\xa0\x80", "\xc0\xaf", "\xf4\x90\x80\x80",
		"\x1b[5C\x1b[32m bar\x1b[H\x1b[31mfoo", "foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr",
		"\x1b[2J", "\x1b[H\x1b[2J\x1b[3J", "\x1b[1;1H\x1b[K", "\x1b[999;999Hx", "\x1b[999;999Hデ",
		"\x1b[2;3r\x1b[?6h\x1b[H\n\n\n", "\x1b[;r", "\x1b[3;2r", "\x1b[1;999r", "\x1b[0;0r", "\x1b[2;2r",
		"\x1b[999@", "\x1b[9999P", "\x1b[9999X", "\x1b[9999L", "\x1b[9999M", "\x1b[9999S", "\x1b[9999T",
		"\x1b[65535P", "\x1b[65535X", "\x1b[65535L", "\x1b[65535M", "\x1b[65535S", "\x1b[65535T", "\x1b[65535A", "\x1b[65535B", "\x1b[65535C", "\x1b[65535D",
		"\x1b[99999A", "\x1b[99999;99999H", "\x1b[99999d", "\x1b[99999G",
		"\x1b[::::::::::::::::::::::::::::::::@", "\x1b[::::::::::::::::::::::::::::::::H", "\x1b[::::::::::::::::::::::::::::::::r", "a\x1b[8888888X",
		"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33;34m",
		"\x1b[  @", "\x1b[1 $ %@", "\x1b[?1 h", "\x1b[>c", "\x1b[c", "\x1b[5n", "\x1b[6n", "\x1b[?6n", "\x1b[0c", "\x1b[=c",
	} {
		add(text)
	}
	return sequences
}

func matrixRequests(live bool) []vt100Request {
	sequences := explicitSequences()
	requests := []vt100Request{}
	for index, screen := range preparedScreens {
		params := matrixParamsShort
		prefixes := []string{"", "?"}
		intermediates := []string{""}
		if live || index == 1 || index == 2 {
			params = matrixParams
			prefixes = []string{"", "?", ">"}
			intermediates = []string{"", " "}
		}
		requests = append(requests, vt100Request{Op: "vt100", Matrix: &vt100Matrix{
			Rows:       screen.rows,
			Cols:       screen.cols,
			Scrollback: screen.scrollback,
			Resize:     screen.resize,
			Setup:      []byte(screen.setup),
			Tail:       []byte(matrixTail),
			Csi:        &csiMatrix{Prefixes: prefixes, Params: params, Inter: intermediates, Lo: 0x40, Hi: 0x7e},
			Seqs:       sequences,
		}})
	}
	return requests
}

// --- generated streams ------------------------------------------------------

var streamText = []string{
	"a", "b", "hello", "world", " ", "The quick brown fox jumps over the lazy dog", "0123456789", "~", "x y z",
	"é", "ñ", "ß", "Ω", "я", "ש", "ع", "ह", "ก", "한", "デ", "ネ", "中文", "ｆｕｌｌ",
	"e\u0301", "a\u0308\u0304", "\u0301", "\u200b", "\u200d", "\ufe0f", "\u00ad", "\U0001F389", "\U0001F980",
	"\U0001F468\u200d\U0001F469\u200d\U0001F467", "\U0001F1EB\U0001F1F7", "\u17d8", "\ufffd", "\u0085", "\u2028", "\x7f",
}

type streamBuilder struct {
	random     *rand.Rand
	rows, cols int
	out        []byte
}

func (self *streamBuilder) add(text string) { self.out = append(self.out, text...) }

// count is a parameter for something that repeats or moves: usually small,
// sometimes absent, zero, about the size of the screen, or huge.
func (self *streamBuilder) count() string {
	switch self.random.IntN(12) {
	case 0:
		return ""
	case 1:
		return "0"
	case 2:
		return strconv.Itoa(self.rows)
	case 3:
		return strconv.Itoa(self.cols)
	case 4:
		return strconv.Itoa(self.random.IntN(70000))
	case 5:
		return strconv.Itoa(1 + self.random.IntN(self.rows+self.cols))
	default:
		return strconv.Itoa(1 + self.random.IntN(4))
	}
}

func (self *streamBuilder) row() string {
	if self.random.IntN(10) == 0 {
		return self.count()
	}
	return strconv.Itoa(1 + self.random.IntN(self.rows+1))
}

func (self *streamBuilder) col() string {
	if self.random.IntN(10) == 0 {
		return self.count()
	}
	return strconv.Itoa(1 + self.random.IntN(self.cols+1))
}

func (self *streamBuilder) sgr() string {
	random := self.random
	parts := []string{}
	for n := 1 + random.IntN(3); n > 0; n-- {
		switch random.IntN(14) {
		case 0:
			parts = append(parts, "0")
		case 1:
			parts = append(parts, strconv.Itoa(30+random.IntN(8)))
		case 2:
			parts = append(parts, strconv.Itoa(40+random.IntN(8)))
		case 3:
			parts = append(parts, strconv.Itoa(90+random.IntN(8)))
		case 4:
			parts = append(parts, strconv.Itoa(100+random.IntN(8)))
		case 5:
			parts = append(parts, []string{"38", "48"}[random.IntN(2)]+";5;"+strconv.Itoa(random.IntN(260)))
		case 6:
			parts = append(parts, []string{"38", "48"}[random.IntN(2)]+";2;"+strconv.Itoa(random.IntN(260))+";"+strconv.Itoa(random.IntN(256))+";"+strconv.Itoa(random.IntN(256)))
		case 7:
			parts = append(parts, []string{"38", "48"}[random.IntN(2)]+":2:"+strconv.Itoa(random.IntN(256))+":"+strconv.Itoa(random.IntN(256))+":"+strconv.Itoa(random.IntN(256)))
		case 8:
			parts = append(parts, []string{"38", "48"}[random.IntN(2)]+":5:"+strconv.Itoa(random.IntN(256)))
		case 9:
			parts = append(parts, []string{"39", "49", "22", "23", "24", "27"}[random.IntN(6)])
		case 10:
			parts = append(parts, strconv.Itoa(random.IntN(120)))
		case 11:
			parts = append(parts, "")
		default:
			parts = append(parts, []string{"1", "2", "3", "4", "7"}[random.IntN(5)])
		}
	}
	return "\x1b[" + strings.Join(parts, ";") + "m"
}

var streamModes = []string{"1", "6", "9", "25", "47", "1000", "1002", "1003", "1005", "1006", "1049", "2004", "7", "1004"}

func (self *streamBuilder) fragment() {
	random := self.random
	switch choice := random.IntN(100); {
	case choice < 26:
		// Text, sometimes enough of it to fill rows.
		for n := 1 + random.IntN(3); n > 0; n-- {
			self.add(streamText[random.IntN(len(streamText))])
		}
		if random.IntN(6) == 0 {
			self.add(strings.Repeat(streamText[random.IntN(len(streamText))], 1+random.IntN(self.cols+2)))
		}
	case choice < 36:
		self.add([]string{"\r\n", "\n", "\r", "\t", "\x08", "\x07", "\x0b", "\x0c", "\r\n\r\n", "\n\n\n", "\x0e", "\x0f", "\x00"}[random.IntN(13)])
	case choice < 46:
		// Cursor movement.
		switch random.IntN(10) {
		case 0:
			self.add("\x1b[" + self.row() + ";" + self.col() + "H")
		case 1:
			self.add("\x1b[H")
		case 2:
			self.add("\x1b[" + self.col() + "G")
		case 3:
			self.add("\x1b[" + self.row() + "d")
		case 4:
			self.add("\x1b[" + self.row() + ";" + strconv.Itoa(self.cols) + "H")
		case 5:
			self.add("\x1b[" + strconv.Itoa(self.rows) + ";" + self.col() + "H")
		default:
			self.add("\x1b[" + self.count() + string("ABCDEF"[random.IntN(6)]))
		}
	case choice < 54:
		// Erasing.
		switch random.IntN(4) {
		case 0:
			self.add("\x1b[" + []string{"", "0", "1", "2", "3"}[random.IntN(5)] + "J")
		case 1:
			self.add("\x1b[" + []string{"", "0", "1", "2", "3"}[random.IntN(5)] + "K")
		case 2:
			self.add("\x1b[?" + []string{"", "0", "1", "2"}[random.IntN(4)] + string("JK"[random.IntN(2)]))
		default:
			self.add("\x1b[" + self.count() + "X")
		}
	case choice < 62:
		// Inserting, deleting and scrolling.
		self.add("\x1b[" + self.count() + string("@PLMST"[random.IntN(6)]))
	case choice < 67:
		// Scroll regions, origin mode, reverse index.
		switch random.IntN(6) {
		case 0:
			self.add("\x1b[r")
		case 1:
			self.add("\x1b[?6" + string("hl"[random.IntN(2)]))
		case 2:
			self.add("\x1bM")
		default:
			self.add("\x1b[" + self.row() + ";" + self.row() + "r")
		}
	case choice < 75:
		self.add(self.sgr())
	case choice < 81:
		// Modes, the alternate screen, saved cursors, reset.
		switch random.IntN(12) {
		case 0:
			self.add("\x1b7")
		case 1:
			self.add("\x1b8")
		case 2:
			self.add("\x1b" + string("=>"[random.IntN(2)]))
		case 3:
			if random.IntN(4) == 0 {
				self.add("\x1bc")
			}
		case 4:
			self.add("\x1b[?1049" + string("hl"[random.IntN(2)]))
		case 5:
			self.add("\x1b[?47" + string("hl"[random.IntN(2)]))
		default:
			self.add("\x1b[?" + streamModes[random.IntN(len(streamModes))])
			if random.IntN(4) == 0 {
				self.add(";" + streamModes[random.IntN(len(streamModes))])
			}
			self.add(string("hl"[random.IntN(2)]))
		}
	case choice < 85:
		// Operating system commands.
		self.add("\x1b]" + []string{"0;title", "1;icon", "2;デtitle", "0;a;b", "52;c;?", "52;c;YWJj", "52;p;!", "7;x", ""}[random.IntN(9)])
		self.add([]string{"\x07", "\x1b\\", "\x18", "", "\x9c\x07"}[random.IntN(5)])
	case choice < 87:
		// A window resize request, which the callbacks may apply.
		self.add("\x1b[8;" + self.count() + ";" + self.count() + "t")
	case choice < 89:
		// Strings the terminal ignores.
		self.add("\x1b" + string("PX^_"[random.IntN(4)]) + streamText[random.IntN(len(streamText))])
		self.add([]string{"\x1b\\", "\x18", "\x9c", ""}[random.IntN(4)])
	case choice < 93:
		// Any control sequence.
		self.add("\x1b[")
		if random.IntN(3) == 0 {
			self.add(string("<=>?"[random.IntN(4)]))
		}
		self.add(self.count())
		if random.IntN(2) == 0 {
			self.add(string(";:"[random.IntN(2)]) + self.count())
		}
		if random.IntN(6) == 0 {
			self.add(string(rune(0x20 + random.IntN(0x10))))
		}
		if random.IntN(10) != 0 {
			self.add(string(rune(0x40 + random.IntN(0x3f))))
		}
	case choice < 95:
		// Any escape sequence.
		self.add("\x1b")
		if random.IntN(4) == 0 {
			self.add(string(rune(0x20 + random.IntN(0x10))))
		}
		self.add(string(rune(0x30 + random.IntN(0x4f))))
	case choice < 98:
		// Noise.
		for n := 1 + random.IntN(5); n > 0; n-- {
			self.out = append(self.out, byte(random.IntN(256)))
		}
	default:
		// A character cut short.
		sample := streamText[9+random.IntN(len(streamText)-9)]
		self.add(sample[:1+random.IntN(len(sample))])
	}
}

// randomCase draws one case: a screen, a stream cut into chunks, and resizes
// and scrollback changes between the chunks.
func randomCase(random *rand.Rand, fragments int) vt100Case {
	c := vt100Case{}
	switch random.IntN(12) {
	case 0:
		c.Rows, c.Cols = 24, 80
	case 1:
		c.Rows, c.Cols = uint16(1+random.IntN(3)), uint16(1+random.IntN(3))
	default:
		c.Rows, c.Cols = uint16(1+random.IntN(8)), uint16(1+random.IntN(14))
	}
	if random.IntN(3) != 0 {
		c.Scrollback = random.IntN(12)
	}
	c.Resize = random.IntN(4) == 0

	builder := streamBuilder{random: random, rows: int(c.Rows), cols: int(c.Cols)}
	for n := 1 + random.IntN(fragments); n > 0; n-- {
		builder.fragment()
	}
	stream := builder.out

	// Cut the stream at random places, sometimes in the middle of a
	// character or a sequence, and put the other operations between.
	cuts := random.IntN(4)
	if random.IntN(8) == 0 {
		cuts = min(len(stream), 12)
	}
	for ; cuts > 0 && len(stream) > 0; cuts-- {
		size := random.IntN(len(stream) + 1)
		c.Steps = append(c.Steps, processStep(stream[:size]))
		stream = stream[size:]
		switch random.IntN(6) {
		case 0:
			c.Steps = append(c.Steps, sizeStep(uint16(1+random.IntN(8)), uint16(1+random.IntN(14))))
		case 1:
			c.Steps = append(c.Steps, scrollbackStep(random.IntN(14)))
		case 2:
			if random.IntN(3) == 0 {
				c.Steps = append(c.Steps, sizeStep(c.Rows, uint16(1+random.IntN(3))))
			}
		}
	}
	c.Steps = append(c.Steps, processStep(stream))
	if random.IntN(5) == 0 {
		c.Steps = append(c.Steps, scrollbackStep(1+random.IntN(8)))
	}
	return c
}

func randomCases(seed uint64, count int, fragments int) []vt100Case {
	random := rand.New(rand.NewPCG(seed, 0x7674313030))
	cases := make([]vt100Case, 0, count)
	for i := 0; i < count; i++ {
		cases = append(cases, randomCase(random, fragments))
	}
	return cases
}

// --- cases written by hand --------------------------------------------------

func steps(parts ...any) []step {
	out := []step{}
	for _, part := range parts {
		switch part := part.(type) {
		case string:
			out = append(out, processStep([]byte(part)))
		case [2]int:
			out = append(out, sizeStep(uint16(part[0]), uint16(part[1])))
		case int:
			out = append(out, scrollbackStep(part))
		default:
			panic("steps: a step is a string, a [2]int or an int")
		}
	}
	return out
}

// Cases whose dumps are recorded whole, so that the corpus shows what is
// compared: each is a behaviour worth reading.
func shownCases() []vt100Case {
	return []vt100Case{
		// Text, attributes, a wrapped row and a row scrolled into the
		// scrollback.
		{Rows: 2, Cols: 6, Scrollback: 2, Steps: steps("hello \x1b[1;31mworld\x1b[m\r\nデe\u0301", 1)},
		// A row that wraps on a screen of one row panics, as it does in Rust.
		{Rows: 1, Cols: 5, Steps: steps("hello", " world")},
		// A resize that cuts a wide character in two leaves a cell that the
		// next erase panics on, as it does in Rust.
		{Rows: 2, Cols: 4, Steps: steps("abデ", [2]int{2, 3}, "\x1b[1;1H\x1b[K")},
		// A wide character on a screen of one column.
		{Rows: 2, Cols: 1, Steps: steps("a", "デ")},
		// The alternate screen keeps the main screen and its saved cursor.
		{Rows: 3, Cols: 5, Scrollback: 1, Steps: steps("main\x1b[?1049halt", "\x1b[?1049l!")},
		// Callbacks: bell, title, clipboard, a resize that is applied, and
		// sequences the terminal does not implement.
		{Rows: 2, Cols: 8, Resize: true, Steps: steps("\x07\x1b]2;title\x07\x1b]52;c;YWJj\x1b\\\x1b[8;3;4t\x1b[6n\x1b(B\x01\ufffd")},
	}
}

// Cases for particular behaviours, without their dumps.
func fixedCases() []vt100Case {
	cases := []vt100Case{
		// The inputs of the crate's own tests that are not fixtures.
		{Rows: 24, Cols: 80, Steps: steps("foo\x1b[31m\x1b[32mb\x1b[3;7;42ma\x1b[23mr")},
		{Rows: 24, Cols: 80, Steps: steps([2]int{34, 8}, "\x1b[30;5H", [2]int{24, 80}, [2]int{34, 8}, "\x1b[?1049h", [2]int{24, 80}, "\x1b[?1049l",
			[2]int{34, 8}, "\x1bc01234567890123456789", [2]int{24, 80}, [2]int{34, 8})},
		{Rows: 24, Cols: 80, Steps: steps([2]int{30, 100}, "\x1b[75Cfoobar")},
		{Rows: 24, Cols: 80, Steps: steps("foo\x1b[20;70Hbar\x1b7", "\x1b[H", [2]int{15, 60}, "y\x1b8z")},
		{Rows: 24, Cols: 80, Resize: true, Steps: steps("\x1b[8;24;80t", "\x1b[8t", "\x1b[8;80;24t", "\x1b[8;24t")},
		{Rows: 24, Cols: 80, Resize: true, Steps: steps(strings.Repeat("a", 90), "\x1b[H\x1b[8;24;15tbbbbbbbbbbbbbbbbbbbb\x1b[8;24;80tcccccccccccccccccccc")},
		{Rows: 24, Cols: 80, Steps: steps("\x1b[31m\x1b[24;75Hfooba\x08r\x08\x1b[1@a")},
		{Rows: 24, Cols: 80, Steps: steps(":\x1b[K", "a", "\x1b[1;2H\x1b[K", "\x1b[H\x1b[J\x1b[4;80H", "a", "\n", "b")},
		{Rows: 24, Cols: 80, Steps: steps("\x1b[5C\x1b[32m bar", "\x1b[H\x1b[31mfoo", "\x1b[1;7H\x1b[32mbaz", "\x1b[1;8H\x1b[X")},
		{Rows: 24, Cols: 80, Steps: steps("foo\x1b[5;5Hbar", "\x1b[3D\x1b[2X", "\x1bcfoo\x1b[5;5Hbar", "\x1b[3D\x1b[3X")},
		{Rows: 3, Cols: 20, Scrollback: 10, Steps: steps("1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10", 4, 10, 0)},
		{Rows: 37, Cols: 193, Steps: steps("\x1b[H\x1b[2J\x1b[1;1H\x1b[38;5;15m\x1b[48;5;17m weechat ", "\x1b[m")},

		// Scrollback: filling it, scrolling back, output while scrolled
		// back, the alternate screen, a reset.
		{Rows: 3, Cols: 4, Scrollback: 5, Steps: steps("1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10", 2, "\r\n11", 99, "\r\n12\r\n13\r\n14", 0, 3, "\x1b[?1049h", 2, "\x1b[?1049l", 2, "\x1bc", 2)},
		// A scroll region keeps rows out of the scrollback.
		{Rows: 4, Cols: 4, Scrollback: 5, Steps: steps("a\r\nb\r\nc\r\nd\r\ne", "\x1b[2;3r\x1b[3;1H\n\n\n", 9, "\x1b[r\x1b[4;1H\n\n", 9)},
		// Scrollback rows keep the width they had.
		{Rows: 2, Cols: 6, Scrollback: 4, Steps: steps("abcdef\r\nghijkl\r\nmnopqr\r\n", [2]int{2, 3}, 2, [2]int{3, 9}, 3)},
		// Growing and shrinking with the cursor, the scroll region and the
		// saved cursor near the edges.
		{Rows: 6, Cols: 6, Steps: steps("\x1b[2;5r\x1b[5;6H\x1b7x", [2]int{3, 3}, "y\x1b8z", [2]int{8, 8}, "\x1b8w")},
		{Rows: 4, Cols: 4, Steps: steps("abcdefgh", [2]int{4, 5}, "i", [2]int{4, 2}, "j", [2]int{1, 1}, "k")},
		// Sizes of zero: the port must fail where Rust fails.
		{Rows: 0, Cols: 0, Steps: steps("a")},
		{Rows: 0, Cols: 5, Steps: steps("\x1b[H", "\n", "a")},
		{Rows: 3, Cols: 0, Steps: steps("\x1b[2J", "\x1b[K", "a")},
		{Rows: 2, Cols: 2, Steps: steps("ab", [2]int{0, 0}, "\x1b[H", [2]int{2, 2}, "c")},
		{Rows: 2, Cols: 2, Steps: steps([2]int{2, 0}, [2]int{2, 2}, "ab")},
		// The largest size a u16 holds in one direction.
		{Rows: 1, Cols: 65535, Steps: steps("\x1b[65535Gab\x1b[5D\t\t")},
		{Rows: 65535, Cols: 1, Steps: steps("\x1b[65535d\n\n", "\x1b[65535;1Ha")},
		// Many cells inserted at once: the row grows by one cell at a time
		// before it is cut back, here as in Rust.
		{Rows: 2, Cols: 6, Steps: steps("ab\u30c7cd\x1b[1;2H\x1b[20000@", "\x1b[1;4H\x1b[3@")},
		// U+17D8 is three columns wide to unicode-width, which the terminal
		// does not expect.
		{Rows: 3, Cols: 4, Steps: steps("ab\u17d8", "\u17d8\u17d8", "\x1b[1;3H\u17d8")},
		{Rows: 2, Cols: 2, Steps: steps("\u17d8")},
		// Zero-width characters where there is nothing to join them to.
		{Rows: 3, Cols: 3, Steps: steps("\u0301", "abc\u0301", "\u0301d", "\x1b[3;1H\u0301", "\x1b[1;1Hデ\u0301\u0302")},
		// A cell holds only so many combining characters.
		{Rows: 1, Cols: 4, Steps: steps("a" + strings.Repeat("\u0301", 12) + strings.Repeat("\U000E0100", 6))},
		// Wide characters overwritten one half at a time.
		{Rows: 2, Cols: 6, Steps: steps("デネデ", "\x1b[1;2Hx", "\x1b[1;3Hy", "\x1b[1;4Hネ", "\x1b[1;5Hネ", "\x1b[1;1H\x1b[2P", "\x1b[1;2H\x1b[2@")},
		// The split of a sequence and of a character over many chunks.
		{Rows: 3, Cols: 10, Steps: steps("\x1b", "[", "3", "1", ";", "4", "2", "m", "\xe3", "\x83", "\x87", "\x1b]", "2;t", "\x07", "\xc3", "A")},
	}
	return cases
}

func batch(cases []vt100Case) vt100Request {
	return vt100Request{Op: "vt100", Cases: cases}
}

func generateVt100(yield func(any)) {
	live := os.Getenv("MJ_ORACLE") == "live"

	yield(vt100Request{Op: "vt100", Cases: shownCases(), Full: true})
	yield(batch(fixedCases()))
	for _, request := range matrixRequests(live) {
		yield(request)
	}

	batches, seed := 40, uint64(1)
	if live {
		batches, seed = 600, 5000
	}
	for index := 0; index < batches; index++ {
		fragments := 8
		switch index % 4 {
		case 2:
			fragments = 30
		case 3:
			fragments = 3
		}
		if live && index%10 == 9 {
			fragments = 200
		}
		yield(batch(randomCases(seed+uint64(index), 100, fragments)))
	}
}

func generateWidth(yield func(any)) {
	yield(vt100Request{Op: "width", From: 0, To: 0x10FFFF})
}
