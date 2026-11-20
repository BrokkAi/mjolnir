// Port of toml_edit 0.20.2 tests/testsuite/datetime.rs

package tomledit_test

import (
	"strings"
	"testing"
)

// datetime.rs defines the same `bad!` macro as parse.rs: itBad.

func TestItTestsuiteDatetime_times(t *testing.T) {
	dogood := func(s string, serialized string) {
		t.Helper()
		toParse := "foo = " + s
		document := itParseDocument(t, toParse)
		itAssertEq(t,
			document.Index("foo").AsDatetime().Unwrap().String(),
			serialized,
		)
	}
	good := func(s string) {
		t.Helper()
		dogood(s, s)
		dogood(strings.ReplaceAll(s, "T", " "), s)
		dogood(strings.ReplaceAll(s, "T", "t"), s)
		dogood(strings.ReplaceAll(s, "Z", "z"), s)
	}

	good("1997-09-09T09:09:09Z")
	good("1997-09-09T09:09:09+09:09")
	good("1997-09-09T09:09:09-09:09")
	good("1997-09-09T09:09:09")
	good("1997-09-09")
	dogood("1997-09-09 ", "1997-09-09")
	dogood("1997-09-09 # comment", "1997-09-09")
	good("09:09:09")
	good("1997-09-09T09:09:09.09Z")
	good("1997-09-09T09:09:09.09+09:09")
	good("1997-09-09T09:09:09.09-09:09")
	good("1997-09-09T09:09:09.09")
	good("09:09:09.09")
}

func TestItTestsuiteDatetime_bad_times(t *testing.T) {
	itBad(t, "foo = 199-09-09", "TOML parse error at line 1, column 10\n  |\n1 | foo = 199-09-09\n  |          ^\nexpected newline, `#`\n")
	itBad(t, "foo = 199709-09", "TOML parse error at line 1, column 13\n  |\n1 | foo = 199709-09\n  |             ^\nexpected newline, `#`\n")
	itBad(t, "foo = 1997-9-09", "TOML parse error at line 1, column 12\n  |\n1 | foo = 1997-9-09\n  |            ^\ninvalid date-time\n")
	itBad(t, "foo = 1997-09-9", "TOML parse error at line 1, column 15\n  |\n1 | foo = 1997-09-9\n  |               ^\ninvalid date-time\n")
	itBad(t, "foo = 1997-09-0909:09:09", "TOML parse error at line 1, column 17\n  |\n1 | foo = 1997-09-0909:09:09\n  |                 ^\nexpected newline, `#`\n")
	itBad(t, "foo = 1997-09-09T09:09:09.", "TOML parse error at line 1, column 26\n  |\n1 | foo = 1997-09-09T09:09:09.\n  |                          ^\nexpected newline, `#`\n")
	itBad(t, "foo = T", "TOML parse error at line 1, column 7\n  |\n1 | foo = T\n  |       ^\ninvalid string\nexpected `\"`, `'`\n")
	itBad(t, "foo = T.", "TOML parse error at line 1, column 7\n  |\n1 | foo = T.\n  |       ^\ninvalid string\nexpected `\"`, `'`\n")
	itBad(t, "foo = TZ", "TOML parse error at line 1, column 7\n  |\n1 | foo = TZ\n  |       ^\ninvalid string\nexpected `\"`, `'`\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09+", "TOML parse error at line 1, column 30\n  |\n1 | foo = 1997-09-09T09:09:09.09+\n  |                              ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09+09", "TOML parse error at line 1, column 32\n  |\n1 | foo = 1997-09-09T09:09:09.09+09\n  |                                ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09+09:9", "TOML parse error at line 1, column 33\n  |\n1 | foo = 1997-09-09T09:09:09.09+09:9\n  |                                 ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09+0909", "TOML parse error at line 1, column 32\n  |\n1 | foo = 1997-09-09T09:09:09.09+0909\n  |                                ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09-", "TOML parse error at line 1, column 30\n  |\n1 | foo = 1997-09-09T09:09:09.09-\n  |                              ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09-09", "TOML parse error at line 1, column 32\n  |\n1 | foo = 1997-09-09T09:09:09.09-09\n  |                                ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09-09:9", "TOML parse error at line 1, column 33\n  |\n1 | foo = 1997-09-09T09:09:09.09-09:9\n  |                                 ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-09-09T09:09:09.09-0909", "TOML parse error at line 1, column 32\n  |\n1 | foo = 1997-09-09T09:09:09.09-0909\n  |                                ^\ninvalid time offset\n")
	itBad(t, "foo = 1997-00-09T09:09:09.09Z", "TOML parse error at line 1, column 12\n  |\n1 | foo = 1997-00-09T09:09:09.09Z\n  |            ^\ninvalid date-time\nvalue is out of range\n")
	itBad(t, "foo = 1997-09-00T09:09:09.09Z", "TOML parse error at line 1, column 15\n  |\n1 | foo = 1997-09-00T09:09:09.09Z\n  |               ^\ninvalid date-time\nvalue is out of range\n")
	itBad(t, "foo = 1997-09-09T30:09:09.09Z", "TOML parse error at line 1, column 17\n  |\n1 | foo = 1997-09-09T30:09:09.09Z\n  |                 ^\nexpected newline, `#`\n")
	itBad(t, "foo = 1997-09-09T12:69:09.09Z", "TOML parse error at line 1, column 21\n  |\n1 | foo = 1997-09-09T12:69:09.09Z\n  |                     ^\ninvalid date-time\nvalue is out of range\n")
	itBad(t, "foo = 1997-09-09T12:09:69.09Z", "TOML parse error at line 1, column 24\n  |\n1 | foo = 1997-09-09T12:09:69.09Z\n  |                        ^\ninvalid date-time\nvalue is out of range\n")
}
