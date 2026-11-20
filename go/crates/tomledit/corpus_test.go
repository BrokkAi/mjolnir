package tomledit

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"math/rand"
	"os"
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"
)

// tomlTestCase is one case of the toml-test corpus, as packed into
// testdata/toml_test.jsonl.
//
// The file holds the corpus of the crate toml-test-data 1.3.1 (the cases of
// https://github.com/toml-lang/toml-test at the commit that crate vendors,
// MIT licensed), which the compliance tests of toml 0.8.2 and toml_edit
// 0.20.2 run on, and for each invalid case the error text toml_edit 0.20.2
// expects (its tests/fixtures/invalid/**/*.stderr). One JSON object per
// line: "name", then "toml" (or "toml_base64" when the document is not
// UTF-8), "toml_1_0" for the cases listed in the corpus's files-toml-1.0.0
// (the only ones the crates' harness runs), "json" with the expected decoding
// of a valid case, and "stderr" for an invalid one.
type tomlTestCase struct {
	Name       string          `json:"name"`
	Toml       string          `json:"toml"`
	TomlBase64 string          `json:"toml_base64"`
	Toml10     bool            `json:"toml_1_0"`
	JSON       json.RawMessage `json:"json"`
	Stderr     *string         `json:"stderr"`
}

// bytes returns the document as the corpus has it, which is not always
// UTF-8.
func (c *tomlTestCase) bytes(t testing.TB) []byte {
	if c.TomlBase64 != "" {
		raw, err := base64.StdEncoding.DecodeString(c.TomlBase64)
		if err != nil {
			t.Fatalf("%s: %v", c.Name, err)
		}
		return raw
	}
	return []byte(c.Toml)
}

func (c *tomlTestCase) valid() bool { return strings.HasPrefix(c.Name, "valid/") }

func loadTomlTest(t testing.TB) []tomlTestCase {
	t.Helper()
	file, err := os.Open("testdata/toml_test.jsonl")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	var cases []tomlTestCase
	scanner := bufio.NewScanner(file)
	scanner.Buffer(nil, 1<<22)
	for scanner.Scan() {
		var c tomlTestCase
		if err := json.Unmarshal(scanner.Bytes(), &c); err != nil {
			t.Fatal(err)
		}
		cases = append(cases, c)
	}
	if err := scanner.Err(); err != nil {
		t.Fatal(err)
	}
	if len(cases) == 0 {
		t.Fatal("testdata/toml_test.jsonl holds no cases")
	}
	return cases
}

// handwrittenDocuments are documents that aim at what the corpus leaves out:
// the layout a document keeps (comments, blank lines, indentation, CRLF), and
// the edges of each rule of the grammar.
var handwrittenDocuments = []string{
	"",
	"\n",
	" ",
	"\t# only a comment",
	"\ufeffa = 1\n",
	"a = 1",
	"a = 1 # trailing\n# after\n",
	"# before\n\n  a   =   1   # c\n\n\n[t]  # header\n  b = 2\n\n# end\n",
	"a = 1\r\nb = 2\r\n[t]\r\nc = \"x\"\r\n",
	"a = \"\"\"\r\nline one\r\nline two\\\r\n   joined\"\"\"\r\n",
	"a = '''\r\nraw\r\nlines'''\r\n",
	"[a]\n[a.b]\n[a.b.c]\nx = 1\n[a.d]\n",
	"[a.b.c]\nx = 1\n[a]\ny = 2\n[a.b]\nz = 3\n",
	"[[a]]\nx = 1\n[[a]]\nx = 2\n[[a.b]]\ny = 3\n[a.c]\nz = 4\n[[a]]\n",
	"a.b.c = 1\na.b.d = 2\na.e = 3\n[t]\nu.v = 4\n",
	"a . b . c = 1\n \"q\" . 'r' = 2\n",
	"a = { b = 1, c = { d = 2 }, e.f = 3 }\n",
	"a = {}\nb = {   }\nc = { }\n",
	"a = []\nb = [   ]\nc = [\n]\nd = [ # c\n  1, # one\n  2 # two\n  , 3,\n]\n",
	"a = [[1, 2], ['a', \"b\"], [{ x = 1 }, { y = [1.5] }]]\n",
	"a = 0\nb = +0\nc = -0\nd = 1_000\ne = 0xDEAD_beef\nf = 0o755\ng = 0b1010\n",
	"a = 9223372036854775807\nb = -9223372036854775808\n",
	"a = 0.0\nb = -0.0\nc = +1.5\nd = 1e10\ne = 1E-10\nf = 6.02_2e2_3\ng = inf\nh = -inf\ni = +inf\nj = nan\nk = -nan\nl = +nan\n",
	"a = 1e400\n",
	"a = -1e400\n",
	"a = 1e-400\n",
	"a = true\nb = false\n",
	"a = 1979-05-27T07:32:00Z\nb = 1979-05-27T00:32:00-07:00\nc = 1979-05-27T00:32:00.999999-07:00\nd = 1979-05-27 07:32:00Z\ne = 1979-05-27T07:32:00\nf = 1979-05-27\ng = 07:32:00\nh = 00:32:00.999999\ni = 1979-05-27t07:32:00z\n",
	"a = 1979-05-27T07:32:00.123456789123Z\nb = 23:59:60\nc = 1979-05-27 # date then comment\n",
	"a = \"\\b\\t\\n\\f\\r\\\"\\\\\\u00E9\\U0001F600\"\n",
	"a = \"é😀\"\n'é' = 'ü'\n\"😀\" = \"\"\"😀\"\"\"\n",
	"a = \"\"\"\"quoted\"\"\"\"\nb = \"\"\"\"\"two\"\"\"\"\"\nc = ''''one''''\nd = '''''two'''''\n",
	"a = \"\"\"\\\n   trimmed \\\n\n   \\\n   lines\"\"\"\n",
	"a = \"\"\"a\"b\"\"c\"\"\"\nb = '''a'b''c'''\n",
	"'' = 1\n\"\" . a = 2\n",
	"\"a b\" = 1\n'c.d' = 2\n\"e\\tf\" = 3\n1 = 4\n-_- = 5\n",
	"[\"quoted table\"]\na = 1\n[ spaced . 'key' ]\nb = 2\n[[ arr . \"x\" ]]\nc = 3\n",
	"a = 1 # comment with \ttab and é\n",
	"   \n\t\n  # c\n\n",
	"a=1\nb=2\n[t]\nc=3\n[[u]]\nd=4\n",
}

// invalidDocuments are documents that are wrong in the ways the request for
// this package names: truncation, bad escapes, duplicate keys, mixed types,
// invalid datetimes, deep nesting and what is not ASCII.
var invalidDocuments = []string{
	"a",
	"a =",
	"a = ",
	"a = \n",
	"= 1\n",
	"a = 1 b = 2\n",
	"a = 1\na = 2\n",
	"a.b = 1\na.b = 2\n",
	"a = 1\na.b = 2\n",
	"a.b = 1\na = 2\n",
	"[a]\nb = 1\n[a]\n",
	"[a]\nb = 1\n[a.b]\n",
	"[a.b]\n[a]\n[a]\n",
	"[[a]]\n[a]\n",
	"[a]\n[[a]]\n",
	"a = []\n[[a]]\n",
	"[a]\nb.c = 1\n[a.b]\n",
	"[a.b]\nc = 1\n[a]\nb.d = 2\n",
	"a = { b = 1, b = 2 }\n",
	"a = { b = 1, b.c = 2 }\n",
	"a = { b.c = 1, b = 2 }\n",
	"a = { b = 1,\n c = 2 }\n",
	"a = { b = 1, }\n",
	"a = { b = 1\n",
	"a = {\n",
	"a = [1, 2\n",
	"a = [1,, 2]\n",
	"a = [,]\n",
	"a = [1 2]\n",
	"a = [\n",
	"a = \"unterminated\n",
	"a = 'unterminated\n",
	"a = \"\"\"unterminated\n",
	"a = '''unterminated\n",
	"a = \"\\q\"\n",
	"a = \"\\",
	"a = \"\\u12\"\n",
	"a = \"\\uD800\"\n",
	"a = \"\\U00110000\"\n",
	"a = \"\\U0000000\"\n",
	"a = \"\"\"\\q\"\"\"\n",
	"a = \"\"\"\\  x\"\"\"\n",
	"a = \"tab\tok\"\nb = \"bell\x07bad\"\n",
	"a = 'del\x7fbad'\n",
	"a = \"\"\"nul\x00bad\"\"\"\n",
	"# comment with \x00 nul\n",
	"# comment with \x7f del\n",
	"a = 1 # comment with \x1f\n",
	"a = 1\r",
	"a = 1\rb = 2\n",
	"\r",
	"a = \"\r\"\n",
	"a = \"\"\"\r\"\"\"\n",
	"a = '''\r'''\n",
	"a = 01\n",
	"a = +01\n",
	"a = 1__0\n",
	"a = _1\n",
	"a = 1_\n",
	"a = 0x\n",
	"a = 0xG\n",
	"a = 0x_1\n",
	"a = 0x1_\n",
	"a = 0o8\n",
	"a = 0b2\n",
	"a = +0x1\n",
	"a = 0XFF\n",
	"a = 9223372036854775808\n",
	"a = -9223372036854775809\n",
	"a = 0xFFFFFFFFFFFFFFFFF\n",
	"a = 0o7777777777777777777777\n",
	"a = 0b11111111111111111111111111111111111111111111111111111111111111111\n",
	"a = 1.\n",
	"a = .5\n",
	"a = 1.e5\n",
	"a = 1e\n",
	"a = 1e+\n",
	"a = 1.5_\n",
	"a = 1._5\n",
	"a = 1e5.5\n",
	"a = 1e_5\n",
	"a = 1e5_\n",
	"a = inf2\n",
	"a = nanx\n",
	"a = infinity\n",
	"a = +\n",
	"a = -\n",
	"a = tru\n",
	"a = TRUE\n",
	"a = falsey\n",
	"a = t\n",
	"a = f\n",
	"a = i\n",
	"a = n\n",
	"a = x\n",
	"a = 1979-13-01\n",
	"a = 1979-00-01\n",
	"a = 1979-01-32\n",
	"a = 1979-01-00\n",
	"a = 1979-1-01\n",
	"a = 1979-01-1\n",
	"a = 1979-01\n",
	"a = 1979-01-01T\n",
	"a = 1979-01-01T25:00:00\n",
	"a = 1979-01-01T23:60:00\n",
	"a = 1979-01-01T23:59:61\n",
	"a = 1979-01-01T23:59\n",
	"a = 1979-01-01T23:5:00\n",
	"a = 1979-01-01T23:59:00.\n",
	"a = 1979-01-01T23:59:00+25:00\n",
	"a = 1979-01-01T23:59:00+02:60\n",
	"a = 1979-01-01T23:59:00+02\n",
	"a = 1979-01-01T23:59:00+0200\n",
	"a = 1979-01-01T23:59:00 Z\n",
	"a = 24:00:00\n",
	"a = 12:60:00\n",
	"a = 12:00:61\n",
	"a = 12:00\n",
	"a = 12:0:00\n",
	"a = 12:00:00Z\n",
	"a = 19790-01-01\n",
	"a = 197-01-01\n",
	"[\n",
	"[]\n",
	"[a\n",
	"[a.]\n",
	"[.a]\n",
	"[a..b]\n",
	"[a] b = 1\n",
	"[a]]\n",
	"[[a]\n",
	"[[a]]]\n",
	"[[]]\n",
	"[[",
	"[",
	"[a]x\n",
	"[ a b ]\n",
	"a.\n",
	".a = 1\n",
	"a..b = 1\n",
	"a. = 1\n",
	"a b = 1\n",
	"\"a\" \"b\" = 1\n",
	"é = 1\n",
	"a é = 1\n",
	"a = é\n",
	"a = 1 é\n",
	"\"a\nb\" = 1\n",
	"'a\nb' = 1\n",
	"a = \"é\" x\n",
	"\"éé\" = 1 x\n",
	"😀 = 1\n",
	"a = [ \"é\", x ]\n",
	"a = { \"é\" = 1, é = 2 }\n",
	"[\"é\" x]\n",
	"a = \"ééé\"\"\n",
}

// deeplyNested returns documents nested up to and beyond the parser's
// recursion limit.
func deeplyNested() []string {
	var out []string
	for _, depth := range []int{1, 2, 126, 127, 128, 129, 200} {
		out = append(out,
			"a = "+strings.Repeat("[", depth)+strings.Repeat("]", depth)+"\n",
			"a = "+strings.Repeat("{ b = ", depth)+"1"+strings.Repeat(" }", depth)+"\n",
			"a = "+strings.Repeat("[", depth)+"\n",
		)
		keys := make([]string, depth)
		for index := range keys {
			keys[index] = "k"
		}
		dotted := strings.Join(keys, ".")
		out = append(out, dotted+" = 1\n", "["+dotted+"]\n", "[["+dotted+"]]\n", "a = { "+dotted+" = 1 }\n")
	}
	return out
}

// mutationAlphabet holds the bytes and strings a mutation puts into a
// document: every character the grammar gives a meaning to, the controls it
// forbids, and text that is not ASCII.
var mutationAlphabet = []string{
	"\"", "'", "\\", "[", "]", "{", "}", "=", ".", ",", "#", "\n", "\r", "\r\n", "_", "+", "-",
	"e", "E", ":", "T", "Z", "t", "z", "0", "1", "9", " ", "\t", "\x00", "\x1f", "\x7f", "é", "😀",
	"\"\"\"", "'''", "[[", "]]", "true", "inf", "nan", "0x", "1979-05-27", "07:32:00", "\\u00e9", "\\q",
}

// mutate returns count documents made from document by one or two random
// changes: a truncation, or a deletion, insertion, replacement or
// duplication at a random place.
func mutate(random *rand.Rand, document string, count int) []string {
	var out []string
	runes := []rune(document)
	for range count {
		mutated := runes
		for range 1 + random.Intn(2) {
			position := 0
			if len(mutated) > 0 {
				position = random.Intn(len(mutated) + 1)
			}
			piece := []rune(mutationAlphabet[random.Intn(len(mutationAlphabet))])
			var next []rune
			switch random.Intn(6) {
			case 0: // truncate
				next = append(next, mutated[:position]...)
			case 1: // delete one
				next = append(next, mutated[:position]...)
				if position < len(mutated) {
					next = append(next, mutated[position+1:]...)
				}
			case 2, 3: // insert
				next = append(next, mutated[:position]...)
				next = append(next, piece...)
				next = append(next, mutated[position:]...)
			case 4: // replace one
				next = append(next, mutated[:position]...)
				next = append(next, piece...)
				if position < len(mutated) {
					next = append(next, mutated[position+1:]...)
				}
			default: // duplicate the line the position is on
				text := string(mutated)
				offset := len(string(mutated[:position]))
				lineStart := strings.LastIndexByte(text[:offset], '\n') + 1
				lineEnd := strings.IndexByte(text[offset:], '\n')
				if lineEnd < 0 {
					lineEnd = len(text)
				} else {
					lineEnd += offset + 1
				}
				next = []rune(text[:lineEnd] + text[lineStart:lineEnd] + text[lineEnd:])
			}
			mutated = next
		}
		text := string(mutated)
		if utf8.ValidString(text) {
			out = append(out, text)
		}
	}
	return out
}

// corpusDocuments returns every document the differential tests parse: the
// toml-test corpus, the handwritten documents, and seeded mutations of both.
// The order and the content are the same on every run.
func corpusDocuments(t testing.TB) []string {
	var documents []string
	seen := map[string]bool{}
	add := func(text string) {
		// The oracle takes a request as JSON, so only UTF-8 reaches it: a
		// Rust &str is never anything else.
		if utf8.ValidString(text) && !seen[text] {
			seen[text] = true
			documents = append(documents, text)
		}
	}
	var seeds []string
	for _, c := range loadTomlTest(t) {
		text := string(c.bytes(t))
		add(text)
		if utf8.ValidString(text) && len(text) < 2000 {
			seeds = append(seeds, text)
		}
	}
	for _, text := range handwrittenDocuments {
		add(text)
		seeds = append(seeds, text)
	}
	for _, text := range invalidDocuments {
		add(text)
	}
	for _, text := range deeplyNested() {
		add(text)
	}
	random := rand.New(rand.NewSource(20231002))
	rounds := stressRounds()
	for _, seed := range seeds {
		for _, text := range mutate(random, seed, 6*rounds) {
			if len(text) < 4000 {
				add(text)
			}
		}
	}
	for _, text := range invalidDocuments {
		for _, mutated := range mutate(random, text, 2*rounds) {
			add(mutated)
		}
	}
	return documents
}

// stressRounds multiplies the number of mutated documents. It is 1, which is
// what the recorded corpus holds, unless MJ_TOML_STRESS asks for more in a
// live comparison (`MJ_ORACLE=live MJ_TOML_STRESS=50`).
func stressRounds() int {
	if os.Getenv("MJ_ORACLE") != "live" {
		return 1
	}
	rounds, err := strconv.Atoi(os.Getenv("MJ_TOML_STRESS"))
	if err != nil || rounds < 1 {
		return 1
	}
	return rounds
}
