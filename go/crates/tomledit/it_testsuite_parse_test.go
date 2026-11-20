// Port of toml_edit 0.20.2 tests/testsuite/parse.rs

package tomledit_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// parseKey is the `parse!(s, Key)` macro: `s.parse::<Key>()`, asserting it
// succeeds.
func parseKey(t *testing.T, s string) tomledit.Key {
	t.Helper()
	v, err := tomledit.KeyFromStr(s)
	if err != nil {
		t.Fatalf("Failed with %v", err)
	}
	return v
}

// parseValue is the `parse_value!` macro: `parse!(s, Value)`.
func parseValue(t *testing.T, s string) tomledit.Value {
	t.Helper()
	v, err := tomledit.ValueFromStr(s)
	if err != nil {
		t.Fatalf("Failed with %v", err)
	}
	return v
}

// testKey is the `test_key!` macro.
func testKey(t *testing.T, s string, expected string) {
	t.Helper()
	key := parseKey(t, s)
	itAssertEq(t, expected, key.Get())
}

func TestItTestsuiteParse_test_key_from_str(t *testing.T) {
	testKey(t, "a", "a")
	testKey(t, "'hello key'", "hello key")
	testKey(t,
		"\"Jos\\u00E9\\U000A0000\\n\\t\\r\\f\\b\\\"\"",
		"Jos\u00e9\U000a0000\n\t\r\x0c\x08\"",
	)
	testKey(t, "\"\"", "")
	testKey(t, "\"'hello key'bla\"", "'hello key'bla")
	testKey(t,
		"'C:\\Users\\appveyor\\AppData\\Local\\Temp\\1\\cargo-edit-test.YizxPxxElXn9'",
		"C:\\Users\\appveyor\\AppData\\Local\\Temp\\1\\cargo-edit-test.YizxPxxElXn9",
	)
}

func TestItTestsuiteParse_test_value_from_str(t *testing.T) {
	itAssert(t, parseValue(t, "1979-05-27T00:32:00.999999-07:00").IsDatetime(), "is_datetime")
	itAssert(t, parseValue(t, "1979-05-27T00:32:00.999999Z").IsDatetime(), "is_datetime")
	itAssert(t, parseValue(t, "1979-05-27T00:32:00.999999").IsDatetime(), "is_datetime")
	itAssert(t, parseValue(t, "1979-05-27T00:32:00").IsDatetime(), "is_datetime")
	itAssert(t, parseValue(t, "1979-05-27").IsDatetime(), "is_datetime")
	itAssert(t, parseValue(t, "00:32:00").IsDatetime(), "is_datetime")
	itAssert(t, parseValue(t, "-239").IsInteger(), "is_integer")
	itAssert(t, parseValue(t, "1e200").IsFloat(), "is_float")
	itAssert(t, parseValue(t, "9_224_617.445_991_228_313").IsFloat(), "is_float")
	itAssert(t, parseValue(t, "\"basic string\\nJos\\u00E9\\n\"").IsStr(), "is_str")
	itAssert(t, parseValue(t,
		"\"\"\"\nmultiline basic string\n\"\"\"",
	).IsStr(), "is_str")
	itAssert(t, parseValue(t, "'literal string\\ \\'").IsStr(), "is_str")
	itAssert(t, parseValue(t,
		"'''multiline\nliteral \\ \\\nstring'''",
	).IsStr(), "is_str")
	itAssert(t, parseValue(t, "{ hello = \"world\", a = 1}").IsInlineTable(), "is_inline_table")
	itAssert(t,
		parseValue(t, "[ { x = 1, a = \"2\" }, {a = \"a\",b = \"b\",     c =    \"c\"} ]").IsArray(),
		"is_array",
	)
	wp := "C:\\Users\\appveyor\\AppData\\Local\\Temp\\1\\cargo-edit-test.YizxPxxElXn9"
	lwp := "'C:\\Users\\appveyor\\AppData\\Local\\Temp\\1\\cargo-edit-test.YizxPxxElXn9'"
	itAssertEq(t, tomledit.ValueFromString(wp).AsStr(), parseValue(t, lwp).AsStr())
	itAssert(t, parseValue(t, "\"\\\\\\\"\\b\\f\\n\\r\\t\\u00E9\\U000A0000\"").IsStr(), "is_str")
}

func TestItTestsuiteParse_test_key_unification(t *testing.T) {
	toml := "\n[a]\n[a.'b'.c]\n[a.\"b\".c.e]\n[a.b.c.d]\n"
	expected := "\n[a]\n[a.'b'.c]\n[a.'b'.c.e]\n[a.'b'.c.d]\n"
	doc, err := tomledit.DocumentFromStr(toml)
	itAssert(t, err == nil, "doc.is_ok()")

	snapboxAssertEq(t, expected, doc.String())
}

// itBad is the `bad!` macro: the document must fail to parse, with exactly
// the message msg (as snapbox compares it).
func itBad(t *testing.T, toml string, msg string) {
	t.Helper()
	s, err := tomledit.DocumentFromStr(toml)
	if err == nil {
		t.Fatalf("parsed to: %s", rs.PrettyDebugString(&s))
	}
	snapboxAssertEq(t, msg, err.Error())
}

func TestItTestsuiteParse_crlf(t *testing.T) {
	itParseDocument(t, "[project]\r\n\r\nname = \"splay\"\r\nversion = \"0.1.0\"\r\nauthors = [\"alex@crichton.co\"]\r\n\r\n[[lib]]\r\n\r\npath = \"lib.rs\"\r\nname = \"splay\"\r\ndescription = \"\"\"A Rust implementation of a TAR file reader and writer. This library does not\r\ncurrently handle compression, but it is abstract over all I/O readers and\r\nwriters. Additionally, great lengths are taken to ensure that the entire\r\ncontents are never required to be entirely resident in memory all at once.\r\n\"\"\"")
}

func TestItTestsuiteParse_fun_with_strings(t *testing.T) {
	table := itParseDocument(t, "\nbar = \"\\U00000000\"\nkey1 = \"One\\nTwo\"\nkey2 = \"\"\"One\\nTwo\"\"\"\nkey3 = \"\"\"\nOne\nTwo\"\"\"\n\nkey4 = \"The quick brown fox jumps over the lazy dog.\"\nkey5 = \"\"\"\nThe quick brown \\\n\n\nfox jumps over \\\nthe lazy dog.\"\"\"\nkey6 = \"\"\"\\\n   The quick brown \\\n   fox jumps over \\\n   the lazy dog.\\\n   \"\"\"\n# What you see is what you get.\nwinpath  = 'C:\\Users\\nodejs\\templates'\nwinpath2 = '\\\\ServerX\\admin$\\system32\\'\nquoted   = 'Tom \"Dubs\" Preston-Werner'\nregex    = '<\\i\\c*\\s*>'\n\nregex2 = '''I [dw]on't need \\d{2} apples'''\nlines  = '''\nThe first newline is\ntrimmed in raw strings.\nAll other whitespace\nis preserved.\n'''\n")
	itAssertEq(t, table.Index("bar").AsStr(), rs.Some("\x00"))
	itAssertEq(t, table.Index("key1").AsStr(), rs.Some("One\nTwo"))
	itAssertEq(t, table.Index("key2").AsStr(), rs.Some("One\nTwo"))
	itAssertEq(t, table.Index("key3").AsStr(), rs.Some("One\nTwo"))

	msg := "The quick brown fox jumps over the lazy dog."
	itAssertEq(t, table.Index("key4").AsStr(), rs.Some(msg))
	itAssertEq(t, table.Index("key5").AsStr(), rs.Some(msg))
	itAssertEq(t, table.Index("key6").AsStr(), rs.Some(msg))

	itAssertEq(t,
		table.Index("winpath").AsStr(),
		rs.Some("C:\\Users\\nodejs\\templates"),
	)
	itAssertEq(t,
		table.Index("winpath2").AsStr(),
		rs.Some("\\\\ServerX\\admin$\\system32\\"),
	)
	itAssertEq(t,
		table.Index("quoted").AsStr(),
		rs.Some("Tom \"Dubs\" Preston-Werner"),
	)
	itAssertEq(t, table.Index("regex").AsStr(), rs.Some("<\\i\\c*\\s*>"))
	itAssertEq(t,
		table.Index("regex2").AsStr(),
		rs.Some("I [dw]on't need \\d{2} apples"),
	)
	itAssertEq(t,
		table.Index("lines").AsStr(),
		rs.Some(
			"The first newline is\ntrimmed in raw strings.\nAll other whitespace\nis preserved.\n",
		),
	)
}

func TestItTestsuiteParse_tables_in_arrays(t *testing.T) {
	table := itParseDocument(t, "\n[[foo]]\n#…\n[foo.bar]\n#…\n\n[[foo]] # ...\n#…\n[foo.bar]\n#...\n")
	table.Index("foo").Index(0).Index("bar").AsTable().Unwrap()
	table.Index("foo").Index(1).Index("bar").AsTable().Unwrap()
}

func TestItTestsuiteParse_empty_table(t *testing.T) {
	table := itParseDocument(t, "\n[foo]")
	table.Index("foo").AsTable().Unwrap()
}

func TestItTestsuiteParse_mixed_table_issue_527(t *testing.T) {
	input := "\n[package]\nmetadata.msrv = \"1.65.0\"\n\n[package.metadata.release.pre-release-replacements]\n"
	document := itParseDocument(t, input)
	actual := document.String()
	snapboxAssertEq(t, input, actual)
}

func TestItTestsuiteParse_fruit(t *testing.T) {
	table := itParseDocument(t, "\n[[fruit]]\nname = \"apple\"\n\n[fruit.physical]\ncolor = \"red\"\nshape = \"round\"\n\n[[fruit.variety]]\nname = \"red delicious\"\n\n[[fruit.variety]]\nname = \"granny smith\"\n\n[[fruit]]\nname = \"banana\"\n\n[[fruit.variety]]\nname = \"plantain\"\n")
	itAssertEq(t, table.Index("fruit").Index(0).Index("name").AsStr(), rs.Some("apple"))
	itAssertEq(t, table.Index("fruit").Index(0).Index("physical").Index("color").AsStr(), rs.Some("red"))
	itAssertEq(t,
		table.Index("fruit").Index(0).Index("physical").Index("shape").AsStr(),
		rs.Some("round"),
	)
	itAssertEq(t,
		table.Index("fruit").Index(0).Index("variety").Index(0).Index("name").AsStr(),
		rs.Some("red delicious"),
	)
	itAssertEq(t,
		table.Index("fruit").Index(0).Index("variety").Index(1).Index("name").AsStr(),
		rs.Some("granny smith"),
	)
	itAssertEq(t, table.Index("fruit").Index(1).Index("name").AsStr(), rs.Some("banana"))
	itAssertEq(t,
		table.Index("fruit").Index(1).Index("variety").Index(0).Index("name").AsStr(),
		rs.Some("plantain"),
	)
}

func TestItTestsuiteParse_stray_cr(t *testing.T) {
	itBad(t, "\r", "TOML parse error at line 1, column 1\n  |\n1 | \r\n  | ^\n\n")
	itBad(t, "a = [ \r ]", "TOML parse error at line 1, column 7\n  |\n1 | a = [ \r\n ]\n  |       ^\ninvalid array\nexpected `]`\n")
	itBad(t, "a = \"\"\"\r\"\"\"", "TOML parse error at line 1, column 8\n  |\n1 | a = \"\"\"\r\n\"\"\"\n  |        ^\ninvalid multiline basic string\n")
	itBad(t, "a = \"\"\"\\  \r  \"\"\"", "TOML parse error at line 1, column 10\n  |\n1 | a = \"\"\"\\  \r\n  \"\"\"\n  |          ^\ninvalid escape sequence\nexpected `b`, `f`, `n`, `r`, `t`, `u`, `U`, `\\`, `\"`\n")
	itBad(t, "a = '''\r'''", "TOML parse error at line 1, column 8\n  |\n1 | a = '''\r\n'''\n  |        ^\ninvalid multiline literal string\n")
	itBad(t, "a = '\r'", "TOML parse error at line 1, column 6\n  |\n1 | a = '\r\n'\n  |      ^\ninvalid literal string\n")
	itBad(t, "a = \"\r\"", "TOML parse error at line 1, column 6\n  |\n1 | a = \"\r\n\"\n  |      ^\ninvalid basic string\n")
}

func TestItTestsuiteParse_blank_literal_string(t *testing.T) {
	table := itParseDocument(t, "foo = ''")
	itAssertEq(t, table.Index("foo").AsStr(), rs.Some(""))
}

func TestItTestsuiteParse_many_blank(t *testing.T) {
	table := itParseDocument(t, "foo = \"\"\"\n\n\n\"\"\"")
	itAssertEq(t, table.Index("foo").AsStr(), rs.Some("\n\n"))
}

func TestItTestsuiteParse_literal_eats_crlf(t *testing.T) {
	table := itParseDocument(t, "\n        foo = \"\"\"\\\r\n\"\"\"\n        bar = \"\"\"\\\r\n   \r\n   \r\n   a\"\"\"\n    ")
	itAssertEq(t, table.Index("foo").AsStr(), rs.Some(""))
	itAssertEq(t, table.Index("bar").AsStr(), rs.Some("a"))
}

func TestItTestsuiteParse_string_no_newline(t *testing.T) {
	itBad(t, "a = \"\n\"", "TOML parse error at line 1, column 6\n  |\n1 | a = \"\n  |      ^\ninvalid basic string\n")
	itBad(t, "a = '\n'", "TOML parse error at line 1, column 6\n  |\n1 | a = '\n  |      ^\ninvalid literal string\n")
}

func TestItTestsuiteParse_bad_leading_zeros(t *testing.T) {
	itBad(t, "a = 00", "TOML parse error at line 1, column 6\n  |\n1 | a = 00\n  |      ^\nexpected newline, `#`\n")
	itBad(t, "a = -00", "TOML parse error at line 1, column 7\n  |\n1 | a = -00\n  |       ^\nexpected newline, `#`\n")
	itBad(t, "a = +00", "TOML parse error at line 1, column 7\n  |\n1 | a = +00\n  |       ^\nexpected newline, `#`\n")
	itBad(t, "a = 00.0", "TOML parse error at line 1, column 6\n  |\n1 | a = 00.0\n  |      ^\nexpected newline, `#`\n")
	itBad(t, "a = -00.0", "TOML parse error at line 1, column 7\n  |\n1 | a = -00.0\n  |       ^\nexpected newline, `#`\n")
	itBad(t, "a = +00.0", "TOML parse error at line 1, column 7\n  |\n1 | a = +00.0\n  |       ^\nexpected newline, `#`\n")
	itBad(t, "a = 9223372036854775808", "TOML parse error at line 1, column 5\n  |\n1 | a = 9223372036854775808\n  |     ^\nnumber too large to fit in target type\n")
	itBad(t, "a = -9223372036854775809", "TOML parse error at line 1, column 5\n  |\n1 | a = -9223372036854775809\n  |     ^\nnumber too small to fit in target type\n")
}

func TestItTestsuiteParse_bad_floats(t *testing.T) {
	itBad(t, "a = 0.", "TOML parse error at line 1, column 7\n  |\n1 | a = 0.\n  |       ^\ninvalid floating-point number\nexpected digit\n")
	itBad(t, "a = 0.e", "TOML parse error at line 1, column 7\n  |\n1 | a = 0.e\n  |       ^\ninvalid floating-point number\nexpected digit\n")
	itBad(t, "a = 0.E", "TOML parse error at line 1, column 7\n  |\n1 | a = 0.E\n  |       ^\ninvalid floating-point number\nexpected digit\n")
	itBad(t, "a = 0.0E", "TOML parse error at line 1, column 9\n  |\n1 | a = 0.0E\n  |         ^\ninvalid floating-point number\n")
	itBad(t, "a = 0.0e", "TOML parse error at line 1, column 9\n  |\n1 | a = 0.0e\n  |         ^\ninvalid floating-point number\n")
	itBad(t, "a = 0.0e-", "TOML parse error at line 1, column 10\n  |\n1 | a = 0.0e-\n  |          ^\ninvalid floating-point number\n")
	itBad(t, "a = 0.0e+", "TOML parse error at line 1, column 10\n  |\n1 | a = 0.0e+\n  |          ^\ninvalid floating-point number\n")
}

func TestItTestsuiteParse_floats(t *testing.T) {
	check := func(actual string, expected float64) {
		t.Helper()
		f := "foo = " + actual
		t.Log(f)
		a := itParseDocument(t, f)
		itAssertEq(t, a.Index("foo").AsFloat().Unwrap(), expected)
	}

	check("1.0", 1.0)
	check("1.0e0", 1.0)
	check("1.0e+0", 1.0)
	check("1.0e-0", 1.0)
	check("1E-0", 1.0)
	check("1.001e-0", 1.001)
	check("2e10", 2e10)
	check("2e+10", 2e10)
	check("2e-10", 2e-10)
	check("2_0.0", 20.0)
	check("2_0.0_0e1_0", 20.0e10)
	check("2_0.1_0e1_0", 20.1e10)
}

func TestItTestsuiteParse_bare_key_names(t *testing.T) {
	a := itParseDocument(t, "\n        foo = 3\n        foo_3 = 3\n        foo_-2--3--r23f--4-f2-4 = 3\n        _ = 3\n        - = 3\n        8 = 8\n        \"a\" = 3\n        \"!\" = 3\n        \"a^b\" = 3\n        \"\\\"\" = 3\n        \"character encoding\" = \"value\"\n        'ʎǝʞ' = \"value\"\n    ")
	_ = a.Index("foo")
	_ = a.Index("-")
	_ = a.Index("_")
	_ = a.Index("8")
	_ = a.Index("foo_3")
	_ = a.Index("foo_-2--3--r23f--4-f2-4")
	_ = a.Index("a")
	_ = a.Index("!")
	_ = a.Index("\"")
	_ = a.Index("character encoding")
	_ = a.Index("ʎǝʞ")
}

func TestItTestsuiteParse_bad_keys(t *testing.T) {
	itBad(t, "key\n=3", "TOML parse error at line 1, column 4\n  |\n1 | key\n  |    ^\nexpected `.`, `=`\n")
	itBad(t, "key=\n3", "TOML parse error at line 1, column 5\n  |\n1 | key=\n  |     ^\ninvalid string\nexpected `\"`, `'`\n")
	itBad(t, "key|=3", "TOML parse error at line 1, column 4\n  |\n1 | key|=3\n  |    ^\nexpected `.`, `=`\n")
	itBad(t, "=3", "TOML parse error at line 1, column 1\n  |\n1 | =3\n  | ^\ninvalid key\n")
	itBad(t, "\"\"|=3", "TOML parse error at line 1, column 3\n  |\n1 | \"\"|=3\n  |   ^\nexpected `.`, `=`\n")
	itBad(t, "\"\n\"|=3", "TOML parse error at line 1, column 2\n  |\n1 | \"\n  |  ^\ninvalid basic string\n")
	itBad(t, "\"\r\"|=3", "TOML parse error at line 1, column 2\n  |\n1 | \"\r\"|=3\n  |  ^\ninvalid basic string\n")
	itBad(t, "''''''=3", "TOML parse error at line 1, column 3\n  |\n1 | ''''''=3\n  |   ^\nexpected `.`, `=`\n")
	itBad(t, "\"\"\"\"\"\"=3", "TOML parse error at line 1, column 3\n  |\n1 | \"\"\"\"\"\"=3\n  |   ^\nexpected `.`, `=`\n")
	itBad(t, "'''key'''=3", "TOML parse error at line 1, column 3\n  |\n1 | '''key'''=3\n  |   ^\nexpected `.`, `=`\n")
	itBad(t, "\"\"\"key\"\"\"=3", "TOML parse error at line 1, column 3\n  |\n1 | \"\"\"key\"\"\"=3\n  |   ^\nexpected `.`, `=`\n")
}

func TestItTestsuiteParse_bad_table_names(t *testing.T) {
	itBad(t, "[]", "TOML parse error at line 1, column 2\n  |\n1 | []\n  |  ^\ninvalid key\n")
	itBad(t, "[.]", "TOML parse error at line 1, column 2\n  |\n1 | [.]\n  |  ^\ninvalid key\n")
	itBad(t, "[a.]", "TOML parse error at line 1, column 3\n  |\n1 | [a.]\n  |   ^\ninvalid table header\nexpected `.`, `]`\n")
	itBad(t, "[!]", "TOML parse error at line 1, column 2\n  |\n1 | [!]\n  |  ^\ninvalid key\n")
	itBad(t, "[\"\n\"]", "TOML parse error at line 1, column 3\n  |\n1 | [\"\n  |   ^\ninvalid basic string\n")
	itBad(t, "[a.b]\n[a.\"b\"]", "TOML parse error at line 2, column 1\n  |\n2 | [a.\"b\"]\n  | ^\ninvalid table header\nduplicate key `b` in table `a`\n")
	itBad(t, "[']", "TOML parse error at line 1, column 4\n  |\n1 | [']\n  |    ^\ninvalid literal string\n")
	itBad(t, "[''']", "TOML parse error at line 1, column 4\n  |\n1 | [''']\n  |    ^\ninvalid table header\nexpected `.`, `]`\n")
	itBad(t, "['''''']", "TOML parse error at line 1, column 4\n  |\n1 | ['''''']\n  |    ^\ninvalid table header\nexpected `.`, `]`\n")
	itBad(t, "['''foo''']", "TOML parse error at line 1, column 4\n  |\n1 | ['''foo''']\n  |    ^\ninvalid table header\nexpected `.`, `]`\n")
	itBad(t, "[\"\"\"bar\"\"\"]", "TOML parse error at line 1, column 4\n  |\n1 | [\"\"\"bar\"\"\"]\n  |    ^\ninvalid table header\nexpected `.`, `]`\n")
	itBad(t, "['\n']", "TOML parse error at line 1, column 3\n  |\n1 | ['\n  |   ^\ninvalid literal string\n")
	itBad(t, "['\r\n']", "TOML parse error at line 1, column 3\n  |\n1 | ['\n  |   ^\ninvalid literal string\n")
}

func TestItTestsuiteParse_table_names(t *testing.T) {
	a := itParseDocument(t, "\n        [a.\"b\"]\n        [\"f f\"]\n        [\"f.f\"]\n        [\"\\\"\"]\n        ['a.a']\n        ['\"\"']\n    ")
	t.Log(rs.DebugString(&a))
	_ = a.Index("a").Index("b")
	_ = a.Index("f f")
	_ = a.Index("f.f")
	_ = a.Index("\"")
	_ = a.Index("\"\"")
}

func TestItTestsuiteParse_invalid_bare_numeral(t *testing.T) {
	itBad(t, "4", "TOML parse error at line 1, column 2\n  |\n1 | 4\n  |  ^\nexpected `.`, `=`\n")
}

func TestItTestsuiteParse_inline_tables(t *testing.T) {
	itParseDocument(t, "a = {}")
	itParseDocument(t, "a = {b=1}")
	itParseDocument(t, "a = {   b   =   1    }")
	itParseDocument(t, "a = {a=1,b=2}")
	itParseDocument(t, "a = {a=1,b=2,c={}}")

	itBad(t, "a = {a=1,}", "TOML parse error at line 1, column 9\n  |\n1 | a = {a=1,}\n  |         ^\ninvalid inline table\nexpected `}`\n")
	itBad(t, "a = {,}", "TOML parse error at line 1, column 6\n  |\n1 | a = {,}\n  |      ^\ninvalid inline table\nexpected `}`\n")
	itBad(t, "a = {a=1,a=1}", "TOML parse error at line 1, column 6\n  |\n1 | a = {a=1,a=1}\n  |      ^\nduplicate key `a`\n")
	itBad(t, "a = {\n}", "TOML parse error at line 1, column 6\n  |\n1 | a = {\n  |      ^\ninvalid inline table\nexpected `}`\n")
	itBad(t, "a = {", "TOML parse error at line 1, column 6\n  |\n1 | a = {\n  |      ^\ninvalid inline table\nexpected `}`\n")

	itParseDocument(t, "a = {a=[\n]}")
	itParseDocument(t, "a = {\"a\"=[\n]}")
	itParseDocument(t, "a = [\n{},\n{},\n]")
}

func TestItTestsuiteParse_number_underscores(t *testing.T) {
	check := func(actual string, expected int64) {
		t.Helper()
		f := "foo = " + actual
		table := itParseDocument(t, f)
		itAssertEq(t, table.Index("foo").AsInteger().Unwrap(), expected)
	}

	check("1_0", 10)
	check("1_0_0", 100)
	check("1_000", 1000)
	check("+1_000", 1000)
	check("-1_000", -1000)
}

func TestItTestsuiteParse_bad_underscores(t *testing.T) {
	itBad(t, "foo = 0_", "TOML parse error at line 1, column 8\n  |\n1 | foo = 0_\n  |        ^\nexpected newline, `#`\n")
	itBad(t, "foo = 0__0", "TOML parse error at line 1, column 8\n  |\n1 | foo = 0__0\n  |        ^\nexpected newline, `#`\n")
	itBad(t, "foo = __0", "TOML parse error at line 1, column 7\n  |\n1 | foo = __0\n  |       ^\ninvalid integer\nexpected leading digit\n")
	itBad(t, "foo = 1_0_", "TOML parse error at line 1, column 11\n  |\n1 | foo = 1_0_\n  |           ^\ninvalid integer\nexpected digit\n")
}

func TestItTestsuiteParse_bad_unicode_codepoint(t *testing.T) {
	itBad(t, "foo = \"\\uD800\"", "TOML parse error at line 1, column 10\n  |\n1 | foo = \"\\uD800\"\n  |          ^\ninvalid unicode 4-digit hex code\nvalue is out of range\n")
}

func TestItTestsuiteParse_bad_strings(t *testing.T) {
	itBad(t, "foo = \"\\uxx\"", "TOML parse error at line 1, column 10\n  |\n1 | foo = \"\\uxx\"\n  |          ^\ninvalid unicode 4-digit hex code\n")
	itBad(t, "foo = \"\\u\"", "TOML parse error at line 1, column 10\n  |\n1 | foo = \"\\u\"\n  |          ^\ninvalid unicode 4-digit hex code\n")
	itBad(t, "foo = \"\\", "TOML parse error at line 1, column 8\n  |\n1 | foo = \"\\\n  |        ^\ninvalid basic string\n")
	itBad(t, "foo = '", "TOML parse error at line 1, column 8\n  |\n1 | foo = '\n  |        ^\ninvalid literal string\n")
}

func TestItTestsuiteParse_empty_string(t *testing.T) {
	document := itParseDocument(t, "foo = \"\"")
	itAssertEq(t,
		document.Index("foo").
			AsStr().
			Unwrap(),
		"",
	)
}

func TestItTestsuiteParse_booleans(t *testing.T) {
	table := itParseDocument(t, "foo = true")
	itAssertEq(t, table.Index("foo").AsBool(), rs.Some(true))

	table = itParseDocument(t, "foo = false")
	itAssertEq(t, table.Index("foo").AsBool(), rs.Some(false))

	itBad(t, "foo = true2", "TOML parse error at line 1, column 11\n  |\n1 | foo = true2\n  |           ^\nexpected newline, `#`\n")
	itBad(t, "foo = false2", "TOML parse error at line 1, column 12\n  |\n1 | foo = false2\n  |            ^\nexpected newline, `#`\n")
	itBad(t, "foo = t1", "TOML parse error at line 1, column 7\n  |\n1 | foo = t1\n  |       ^\ninvalid string\nexpected `\"`, `'`\n")
	itBad(t, "foo = f2", "TOML parse error at line 1, column 7\n  |\n1 | foo = f2\n  |       ^\ninvalid string\nexpected `\"`, `'`\n")
}

func TestItTestsuiteParse_bad_nesting(t *testing.T) {
	itBad(t, "\n        a = [2]\n        [[a]]\n        b = 5\n        ", "TOML parse error at line 3, column 9\n  |\n3 |         [[a]]\n  |         ^\ninvalid table header\nduplicate key `a` in document root\n")
	itBad(t, "\n        a = 1\n        [a.b]\n        ", "TOML parse error at line 3, column 9\n  |\n3 |         [a.b]\n  |         ^\ninvalid table header\ndotted key `a` attempted to extend non-table type (integer)\n")
	itBad(t, "\n        a = []\n        [a.b]\n        ", "TOML parse error at line 3, column 9\n  |\n3 |         [a.b]\n  |         ^\ninvalid table header\ndotted key `a` attempted to extend non-table type (array)\n")
	itBad(t, "\n        a = []\n        [[a.b]]\n        ", "TOML parse error at line 3, column 9\n  |\n3 |         [[a.b]]\n  |         ^\ninvalid table header\ndotted key `a` attempted to extend non-table type (array)\n")
	itBad(t, "\n        [a]\n        b = { c = 2, d = {} }\n        [a.b]\n        c = 2\n        ", "TOML parse error at line 4, column 9\n  |\n4 |         [a.b]\n  |         ^\ninvalid table header\nduplicate key `b` in table `a`\n")
}

func TestItTestsuiteParse_bad_table_redefine(t *testing.T) {
	itBad(t, "\n        [a]\n        foo=\"bar\"\n        [a.b]\n        foo=\"bar\"\n        [a]\n        ", "TOML parse error at line 6, column 9\n  |\n6 |         [a]\n  |         ^\ninvalid table header\nduplicate key `a` in document root\n")
	itBad(t, "\n        [a]\n        foo=\"bar\"\n        b = { foo = \"bar\" }\n        [a]\n        ", "TOML parse error at line 5, column 9\n  |\n5 |         [a]\n  |         ^\ninvalid table header\nduplicate key `a` in document root\n")
	itBad(t, "\n        [a]\n        b = {}\n        [a.b]\n        ", "TOML parse error at line 4, column 9\n  |\n4 |         [a.b]\n  |         ^\ninvalid table header\nduplicate key `b` in table `a`\n")
	itBad(t, "\n        [a]\n        b = {}\n        [a]\n        ", "TOML parse error at line 4, column 9\n  |\n4 |         [a]\n  |         ^\ninvalid table header\nduplicate key `a` in document root\n")
}

func TestItTestsuiteParse_datetimes(t *testing.T) {
	check := func(actual string) {
		t.Helper()
		f := "foo = " + actual
		toml, err := tomledit.DocumentFromStr(f)
		if err != nil {
			t.Fatalf("failed: %s: %v", f, err)
		}
		itAssertEq(t, toml.Index("foo").AsDatetime().Unwrap().String(), actual)
	}

	check("2016-09-09T09:09:09Z")
	check("2016-09-09T09:09:09.1Z")
	check("2016-09-09T09:09:09.2+10:00")
	check("2016-09-09T09:09:09.123456789-02:00")
	itBad(t, "foo = 2016-09-09T09:09:09.Z", "TOML parse error at line 1, column 26\n  |\n1 | foo = 2016-09-09T09:09:09.Z\n  |                          ^\nexpected newline, `#`\n")
	itBad(t, "foo = 2016-9-09T09:09:09Z", "TOML parse error at line 1, column 12\n  |\n1 | foo = 2016-9-09T09:09:09Z\n  |            ^\ninvalid date-time\n")
	itBad(t, "foo = 2016-09-09T09:09:09+2:00", "TOML parse error at line 1, column 27\n  |\n1 | foo = 2016-09-09T09:09:09+2:00\n  |                           ^\ninvalid time offset\n")
	itBad(t, "foo = 2016-09-09T09:09:09-2:00", "TOML parse error at line 1, column 27\n  |\n1 | foo = 2016-09-09T09:09:09-2:00\n  |                           ^\ninvalid time offset\n")
	itBad(t, "foo = 2016-09-09T09:09:09Z-2:00", "TOML parse error at line 1, column 27\n  |\n1 | foo = 2016-09-09T09:09:09Z-2:00\n  |                           ^\nexpected newline, `#`\n")
}

func TestItTestsuiteParse_require_newline_after_value(t *testing.T) {
	itBad(t, "0=0r=false", "TOML parse error at line 1, column 4\n  |\n1 | 0=0r=false\n  |    ^\nexpected newline, `#`\n")
	itBad(t, "\n0=\"\"o=\"\"m=\"\"r=\"\"00=\"0\"q=\"\"\"0\"\"\"e=\"\"\"0\"\"\"\n", "TOML parse error at line 2, column 5\n  |\n2 | 0=\"\"o=\"\"m=\"\"r=\"\"00=\"0\"q=\"\"\"0\"\"\"e=\"\"\"0\"\"\"\n  |     ^\nexpected newline, `#`\n")
	itBad(t, "\n[[0000l0]]\n0=\"0\"[[0000l0]]\n0=\"0\"[[0000l0]]\n0=\"0\"l=\"0\"\n", "TOML parse error at line 3, column 6\n  |\n3 | 0=\"0\"[[0000l0]]\n  |      ^\nexpected newline, `#`\n")
	itBad(t, "\n0=[0]00=[0,0,0]t=[\"0\",\"0\",\"0\"]s=[1000-00-00T00:00:00Z,2000-00-00T00:00:00Z]\n", "TOML parse error at line 2, column 6\n  |\n2 | 0=[0]00=[0,0,0]t=[\"0\",\"0\",\"0\"]s=[1000-00-00T00:00:00Z,2000-00-00T00:00:00Z]\n  |      ^\nexpected newline, `#`\n")
	itBad(t, "\n0=0r0=0r=false\n", "TOML parse error at line 2, column 4\n  |\n2 | 0=0r0=0r=false\n  |    ^\nexpected newline, `#`\n")
	itBad(t, "\n0=0r0=0r=falsefal=false\n", "TOML parse error at line 2, column 4\n  |\n2 | 0=0r0=0r=falsefal=false\n  |    ^\nexpected newline, `#`\n")
}

func TestItTestsuiteParse_dont_use_dotted_key_prefix_on_table_fuzz_57049(t *testing.T) {
	// This could generate
	// ```toml
	// [
	// p.o]
	// ```
	input := "\np.a=4\n[p.o]\n"
	document := itParseDocument(t, input)
	actual := document.String()
	snapboxAssertEq(t, input, actual)
}

func TestItTestsuiteParse_despan_keys(t *testing.T) {
	doc := itParseDocument(t, "aaaaaa = 1")
	key, err := tomledit.KeyFromStr("bbb")
	if err != nil {
		t.Fatal(err)
	}
	table := doc.AsTableMut()
	table.InsertFormatted(
		&key,
		&tomledit.Item_Value{F0: &tomledit.Value_Integer{F0: tomledit.FormattedNew[int64](2)}},
	)

	itAssertEq(t, doc.String(), "aaaaaa = 1\nbbb = 2\n")
}
