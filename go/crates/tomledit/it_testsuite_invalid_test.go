// Port of toml_edit 0.20.2 tests/testsuite/invalid.rs

package tomledit_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
)

// unwrapErr is `s.parse::<toml_edit::Document>().unwrap_err()`.
func unwrapErr(t *testing.T, s string) error {
	t.Helper()
	document, err := tomledit.DocumentFromStr(s)
	if err == nil {
		t.Fatalf("called `Result::unwrap_err()` on an `Ok` value: %s", document.String())
	}
	return err
}

func TestItTestsuiteInvalid_incomplete_inline_table_issue_296(t *testing.T) {
	err := unwrapErr(t, "native = {")
	snapboxAssertEq(t,
		"TOML parse error at line 1, column 11\n  |\n1 | native = {\n  |           ^\ninvalid inline table\nexpected `}`\n",
		err.Error(),
	)
}

func TestItTestsuiteInvalid_bare_value_disallowed_issue_293(t *testing.T) {
	err := unwrapErr(t, "value=zzz")
	snapboxAssertEq(t,
		"TOML parse error at line 1, column 7\n  |\n1 | value=zzz\n  |       ^\ninvalid string\nexpected `\"`, `'`\n",
		err.Error(),
	)
}

func TestItTestsuiteInvalid_bare_value_in_array_disallowed_issue_293(t *testing.T) {
	err := unwrapErr(t, "value=[zzz]")
	snapboxAssertEq(t,
		"TOML parse error at line 1, column 8\n  |\n1 | value=[zzz]\n  |        ^\ninvalid array\nexpected `]`\n",
		err.Error(),
	)
}

func TestItTestsuiteInvalid_duplicate_table_after_dotted_key_issue_509(t *testing.T) {
	err := unwrapErr(t, "\n[dependencies.foo]\nversion = \"0.16\"\n\n[dependencies]\nlibc = \"0.2\"\n\n[dependencies]\nrand = \"0.3.14\"\n")
	snapboxAssertEq(t,
		"TOML parse error at line 8, column 1\n  |\n8 | [dependencies]\n  | ^\ninvalid table header\nduplicate key `dependencies` in document root\n",
		err.Error(),
	)
}

func TestItTestsuiteInvalid_bad(t *testing.T) {
	check := func(tomlInput string, expectedErr string) {
		t.Helper()
		err := unwrapErr(t, tomlInput)
		snapboxAssertEq(t, expectedErr, err.Error())
	}

	check("a = 01", "TOML parse error at line 1, column 6\n  |\n1 | a = 01\n  |      ^\nexpected newline, `#`\n")

	check("a = 1__1", "TOML parse error at line 1, column 7\n  |\n1 | a = 1__1\n  |       ^\ninvalid integer\nexpected digit\n")

	check("a = 1_", "TOML parse error at line 1, column 7\n  |\n1 | a = 1_\n  |       ^\ninvalid integer\nexpected digit\n")

	check("''", "TOML parse error at line 1, column 3\n  |\n1 | ''\n  |   ^\nexpected `.`, `=`\n")

	check("a = 9e99999", "TOML parse error at line 1, column 5\n  |\n1 | a = 9e99999\n  |     ^\ninvalid floating-point number\n")

	check("a = \"\x7f\"", "TOML parse error at line 1, column 6\n  |\n1 | a = \"\x7f\"\n  |      ^\ninvalid basic string\n")

	check("a = '\x7f'", "TOML parse error at line 1, column 6\n  |\n1 | a = '\x7f'\n  |      ^\ninvalid literal string\n")

	check("a = -0x1", "TOML parse error at line 1, column 7\n  |\n1 | a = -0x1\n  |       ^\nexpected newline, `#`\n")

	check("a = 0x-1", "TOML parse error at line 1, column 7\n  |\n1 | a = 0x-1\n  |       ^\ninvalid hexadecimal integer\n")

	// Dotted keys.
	check("a.b.c = 1\n         a.b = 2\n        ", "TOML parse error at line 2, column 10\n  |\n2 |          a.b = 2\n  |          ^\nduplicate key `b` in document root\n")

	check("a = 1\n         a.b = 2", "TOML parse error at line 2, column 10\n  |\n2 |          a.b = 2\n  |          ^\ndotted key `a` attempted to extend non-table type (integer)\n")

	check("a = {k1 = 1, k1.name = \"joe\"}", "TOML parse error at line 1, column 6\n  |\n1 | a = {k1 = 1, k1.name = \"joe\"}\n  |      ^\ndotted key `k1` attempted to extend non-table type (integer)\n")
}
