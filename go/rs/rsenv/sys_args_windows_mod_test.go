// Port of std 1.99.1 src/sys/args/windows/tests.rs

package rsenv

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

func chk(t *testing.T, s string, parts []string) {
	t.Helper()
	// The command line as GetCommandLineW gives it: the code units up to the
	// terminating zero.
	wide := rspath.OsStrEncodeWide(s)
	if end := slices.Index(wide, 0); end >= 0 {
		wide = wide[:end]
	}
	parsed := parseLpCmdLine(true, wide, func() string { return "TEST.EXE" })
	if !slices.Equal(parsed, parts) {
		t.Errorf("%q: parsed %q, expected %q", s, parsed, parts)
	}
}

func TestSysArgsWindows_empty(t *testing.T) {
	chk(t, "", []string{"TEST.EXE"})
	chk(t, "\x00", []string{"TEST.EXE"})
}

func TestSysArgsWindows_single_words(t *testing.T) {
	chk(t, "EXE one_word", []string{"EXE", "one_word"})
	chk(t, "EXE a", []string{"EXE", "a"})
	chk(t, "EXE 😅", []string{"EXE", "😅"})
	chk(t, "EXE 😅🤦", []string{"EXE", "😅🤦"})
}

func TestSysArgsWindows_official_examples(t *testing.T) {
	chk(t, `EXE "abc" d e`, []string{"EXE", "abc", "d", "e"})
	chk(t, `EXE a\\\b d"e f"g h`, []string{"EXE", `a\\\b`, "de fg", "h"})
	chk(t, `EXE a\\\"b c d`, []string{"EXE", `a\"b`, "c", "d"})
	chk(t, `EXE a\\\\"b c" d e`, []string{"EXE", `a\\b c`, "d", "e"})
}

func TestSysArgsWindows_whitespace_behavior(t *testing.T) {
	chk(t, " test", []string{"", "test"})
	chk(t, "  test", []string{"", "test"})
	chk(t, " test test2", []string{"", "test", "test2"})
	chk(t, " test  test2", []string{"", "test", "test2"})
	chk(t, "test test2 ", []string{"test", "test2"})
	chk(t, "test  test2 ", []string{"test", "test2"})
	chk(t, "test ", []string{"test"})
}

func TestSysArgsWindows_genius_quotes(t *testing.T) {
	chk(t, `EXE "" ""`, []string{"EXE", "", ""})
	chk(t, `EXE "" """`, []string{"EXE", "", `"`})
	chk(t,
		`EXE "this is """all""" in the same argument"`,
		[]string{"EXE", `this is "all" in the same argument`},
	)
	chk(t, `EXE "a"""`, []string{"EXE", `a"`})
	chk(t, `EXE "a"" a`, []string{"EXE", `a" a`})
	// quotes cannot be escaped in command names
	chk(t, `"EXE" check`, []string{"EXE", "check"})
	chk(t, `"EXE check"`, []string{"EXE check"})
	chk(t, `"EXE """for""" check`, []string{"EXE for check"})
	chk(t, `"EXE \"for\" check`, []string{`EXE \for\ check`})
	chk(t, `"EXE \" for \" check`, []string{`EXE \`, "for", `"`, "check"})
	chk(t, `E"X"E test`, []string{"EXE", "test"})
	chk(t, `EX""E test`, []string{"EXE", "test"})
}

// from https://daviddeley.com/autohotkey/parameters/parameters.htm#WINCRULESEX
func TestSysArgsWindows_post_2008(t *testing.T) {
	chk(t, "EXE CallMeIshmael", []string{"EXE", "CallMeIshmael"})
	chk(t, `EXE "Call Me Ishmael"`, []string{"EXE", "Call Me Ishmael"})
	chk(t, `EXE Cal"l Me I"shmael`, []string{"EXE", "Call Me Ishmael"})
	chk(t, `EXE CallMe\"Ishmael`, []string{"EXE", `CallMe"Ishmael`})
	chk(t, `EXE "CallMe\"Ishmael"`, []string{"EXE", `CallMe"Ishmael`})
	chk(t, `EXE "Call Me Ishmael\\"`, []string{"EXE", `Call Me Ishmael\`})
	chk(t, `EXE "CallMe\\\"Ishmael"`, []string{"EXE", `CallMe\"Ishmael`})
	chk(t, `EXE a\\\b`, []string{"EXE", `a\\\b`})
	chk(t, `EXE "a\\\b"`, []string{"EXE", `a\\\b`})
	chk(t, `EXE "\"Call Me Ishmael\""`, []string{"EXE", `"Call Me Ishmael"`})
	chk(t, `EXE "C:\TEST A\\"`, []string{"EXE", `C:\TEST A\`})
	chk(t, `EXE "\"C:\TEST A\\\""`, []string{"EXE", `"C:\TEST A\"`})
	chk(t, `EXE "a b c"  d  e`, []string{"EXE", "a b c", "d", "e"})
	chk(t, `EXE "ab\"c"  "\\"  d`, []string{"EXE", `ab"c`, `\`, "d"})
	chk(t, `EXE a\\\b d"e f"g h`, []string{"EXE", `a\\\b`, "de fg", "h"})
	chk(t, `EXE a\\\"b c d`, []string{"EXE", `a\"b`, "c", "d"})
	chk(t, `EXE a\\\\"b c" d e`, []string{"EXE", `a\\b c`, "d", "e"})
	// Double Double Quotes
	chk(t, `EXE "a b c""`, []string{"EXE", `a b c"`})
	chk(t, `EXE """CallMeIshmael"""  b  c`, []string{"EXE", `"CallMeIshmael"`, "b", "c"})
	chk(t, `EXE """Call Me Ishmael"""`, []string{"EXE", `"Call Me Ishmael"`})
	chk(t, `EXE """"Call Me Ishmael"" b c`, []string{"EXE", `"Call`, "Me", "Ishmael", "b", "c"})
}
