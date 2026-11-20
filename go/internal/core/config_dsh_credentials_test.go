// Port of the tests in mj-core/src/config/dsh_credentials.rs.

package core

import "testing"

// DSH loads `.env` with Node's `util.parseEnv`; each expectation is what
// Node 24 returns for the same text.
func TestDotenvValuesMatchNodeParseEnv(t *testing.T) {
	some := func(value string) *string { return &value }
	cases := []struct {
		text     string
		expected *string
	}{
		{"A=plain # c", some("plain")},
		{"export A=sk-1", some("sk-1")},
		{"A=\"multi\nline\"", some("multi\nline")},
		{"A=\"esc\\nx\"", some("esc\nx")},
		{"A='single # x'", some("single # x")},
		{"A=`back`", some("back")},
		{"A=\"unclosed", some("\"unclosed")},
		{"A=\"unc\\nx", some("\"unc\\nx")},
		{"garbage line\nA=sk-x", some("sk-x")},
		{"\tA=\tx\t", some("x")},
		{"A=x\ty", some("x\ty")},
		{"  A =  spaced  ", some("spaced")},
		{"A=1\nA=2", some("2")},
		{"A=\"x\" trailing", some("x")},
		{"A=\"a\nb\" \nB=2", some("a\nb")},
		{"A=\"a\r\nb\"\r\n", some("a\nb")},
		{"#A=no", nil},
		{"A=", some("")},
	}
	for _, c := range cases {
		got := parseDotenv(c.text, "A")
		if (got == nil) != (c.expected == nil) || got != nil && *got != *c.expected {
			t.Errorf("%q: got %v, want %v", c.text, debugOptional(got), debugOptional(c.expected))
		}
	}
}

func debugOptional(value *string) string {
	if value == nil {
		return "None"
	}
	return "Some(" + debugString(*value) + ")"
}
