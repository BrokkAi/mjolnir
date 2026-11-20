package rs_test

import (
	"compress/gzip"
	"encoding/json"
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// TestFmtFormatStringsAsRustc holds the run-time parser against rustc.
//
// A format string is checked by rustc when a `format!` is compiled, so the
// oracle, which is compiled once, cannot be asked about a string it has not
// seen. testdata/fmt_format_strings.json.gz instead records what rustc
// 1.99.1 said about each of a list of format strings with each of a few
// argument lists: the output, or that it refused to compile the call. It is
// written by testdata/fmt_format_strings_record.py. rs.Format must give the
// same output where rustc accepted the call, and panic where rustc refused.
func TestFmtFormatStringsAsRustc(t *testing.T) {
	file, err := os.Open("testdata/fmt_format_strings.json.gz")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	unpacked, err := gzip.NewReader(file)
	if err != nil {
		t.Fatal(err)
	}
	var cases [][4]any
	if err := json.NewDecoder(unpacked).Decode(&cases); err != nil {
		t.Fatal(err)
	}
	if len(cases) < 1000 {
		t.Fatalf("only %d recorded cases", len(cases))
	}
	values := map[string]int{"5usize": 5, "7usize": 7, "2usize": 2, "3usize": 3}
	for _, c := range cases {
		format, argList, accepted, output := c[0].(string), c[1].(string), c[2].(bool), c[3].(string)
		var args []any
		if argList != "" {
			for _, arg := range strings.Split(argList, ", ") {
				if name, value, named := strings.Cut(arg, " = "); named {
					args = append(args, rs.Arg(name, values[value]))
				} else {
					args = append(args, values[arg])
				}
			}
		}
		got, panicked := formatOrPanic(format, args)
		switch {
		case accepted && panicked != "":
			t.Errorf("rs.Format(%q, %s) panicked: %s\nrustc accepts it and prints %q", format, argList, panicked, output)
		case accepted && got != output:
			t.Errorf("rs.Format(%q, %s) = %q, rustc prints %q", format, argList, got, output)
		case !accepted && panicked == "":
			t.Errorf("rs.Format(%q, %s) = %q, but rustc refuses it: %s", format, argList, got, output)
		}
	}
}

// formatOrPanic returns what rs.Format returns, or the message it panics
// with.
func formatOrPanic(format string, args []any) (out string, panicked string) {
	defer func() {
		if recovered := recover(); recovered != nil {
			panicked = rs.PanicInfoOf(recovered).Message
			if panicked == "" {
				panicked = "panic"
			}
		}
	}()
	return rs.Format(format, args...), ""
}
