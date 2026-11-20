package strs

import (
	"encoding/hex"
	"encoding/json"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The error types print through the formatter in go/rs: `{}` calls their
// FmtDisplay, `{:?}` their FmtDebug. These tests hold both against Rust under
// a few sets of flags, which shows whether a Display impl pads (`f.pad`) or
// writes as it is (`write!`), and how the derived Debug hands the flags on to
// its fields.

// formatterReady says whether rs.Format has been ported. Until it is, it
// panics on every format spec but a bare `{}`.
func formatterReady() (ready bool) {
	defer func() {
		if recover() != nil {
			ready = false
		}
	}()
	return rs.Format("{:>3}", 1) == "  1"
}

// formatted is the oracle's reply for one error value.
func formatted(err error) map[string]any {
	return map[string]any{
		"display":         rs.Format("{}", err),
		"width":           rs.Format("{:>60}", err),
		"fill":            rs.Format("{:*^70}", err),
		"precision":       rs.Format("{:.7}", err),
		"width_precision": rs.Format("{:<12.4}|", err),
		"debug":           rs.Format("{:?}", err),
		"pretty":          rs.Format("{:#?}", err),
		"debug_width":     rs.Format("{:>4?}", err),
	}
}

func answerFmt(t *testing.T, raw json.RawMessage) any {
	_, members := decodeRequest(t, raw)
	s := text(t, members, "s")
	var err error
	switch kind := text(t, members, "kind"); kind {
	case "utf8":
		_, err = FromUtf8(unhex(t, text(t, members, "b")))
	case "from_utf8":
		_, err = StringFromUtf8(unhex(t, text(t, members, "b")))
	case "from_utf16":
		var units []uint16
		if e := json.Unmarshal(members["u"], &units); e != nil {
			t.Fatalf("request %s: %v", raw, e)
		}
		_, err = FromUtf16(units)
	case "int":
		_, err = ParseI8(s)
	case "float":
		_, err = ParseF64(s)
	case "bool":
		_, err = ParseBool(s)
	case "char":
		_, err = ParseChar(s)
	default:
		t.Fatalf("unknown fmt request %s", raw)
	}
	if err == nil {
		t.Fatalf("request %s: no error", raw)
	}
	return formatted(err)
}

func TestDiffAgainstRust_fmt(t *testing.T) {
	if !formatterReady() {
		t.Skip("rs.Format is not ported yet (go/rs/formatter_pending.go)")
	}
	oracle.Check(t, "std_str", "fmt", func(yield func(any)) {
		invalid := []string{
			"ff", "80", "c0", "c1", "c2", "e0", "e082", "e0a0", "e0a0c0", "ed", "eda080", "f0", "f08f", "f1",
			"f180", "f18080", "f4", "f490", "f5", "fe", "61ff", "c3a9ff", "6162c3a920ff20", "6162e282",
			hex.EncodeToString([]byte(strings.Repeat("a", 1234) + "\xff")),
			hex.EncodeToString([]byte(strings.Repeat("é", 70) + "\xe2\x82")),
		}
		for _, b := range invalid {
			yield(map[string]any{"op": "fmt", "kind": "utf8", "b": b})
			yield(map[string]any{"op": "fmt", "kind": "from_utf8", "b": b})
		}
		for _, u := range [][]uint16{{0xD800}, {0x61, 0xDC00}, {0xD800, 0xD800}} {
			yield(map[string]any{"op": "fmt", "kind": "from_utf16", "u": u})
		}
		for _, s := range []string{"", "+", "-", "x", "1x", "128", "-129", "99999999999999999999"} {
			yield(map[string]any{"op": "fmt", "kind": "int", "s": s})
		}
		for _, s := range []string{"", ".", "x", "1e", "--1"} {
			yield(map[string]any{"op": "fmt", "kind": "float", "s": s})
		}
		for _, s := range []string{"", "True", "yes"} {
			yield(map[string]any{"op": "fmt", "kind": "bool", "s": s})
		}
		for _, s := range []string{"", "ab", "é́"} {
			yield(map[string]any{"op": "fmt", "kind": "char", "s": s})
		}
	}, func(raw json.RawMessage) any { return answerFmt(t, raw) })
}
