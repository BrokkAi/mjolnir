//go:build ignore

// fmt_unicode_data_gen writes fmt_unicode_data.go from the Unicode tables of
// the Rust standard library, core/src/unicode/unicode_data.rs: the skip lists
// of the properties char::escape_debug_ext reads (Cf, Cn in planes 0 to 3,
// Default_Ignorable_Code_Point, Grapheme_Extend) and the White_Space map. The
// formatter keeps its own copy because go/rs/strs, which holds all the
// tables, imports this package.
//
// Run it from the repository root after go/port/fetch-reference.sh, whenever
// the Rust toolchain the port follows moves to another Unicode version:
//
//	go run go/rs/fmt_unicode_data_gen.go \
//	    -src ~/.cache/mjolnir/build/go-port/rust-src/src/rust/library/core/src/unicode/unicode_data.rs \
//	    -out go/rs/fmt_unicode_data.go
//
// It refuses input whose shape it does not recognise. The lookup code in
// fmt_unicode.go is a port of the functions of the same Rust file; when the
// generator starts to fail, read the new unicode_data.rs and update both.
package main

import (
	"flag"
	"fmt"
	"go/format"
	"os"
	"regexp"
	"strconv"
	"strings"
)

func main() {
	src := flag.String("src", "", "path to core/src/unicode/unicode_data.rs")
	out := flag.String("out", "fmt_unicode_data.go", "file to write")
	flag.Parse()
	data, err := os.ReadFile(*src)
	if err != nil {
		fail("%v", err)
	}
	text := string(data)

	version := regexp.MustCompile(`pub const UNICODE_VERSION: \(u8, u8, u8\) = \((\d+), (\d+), (\d+)\);`).FindStringSubmatch(text)
	if version == nil {
		fail("no UNICODE_VERSION")
	}

	var b strings.Builder
	b.WriteString("// Code generated from core 1.99.1 src/unicode/unicode_data.rs; DO NOT EDIT.\n")
	b.WriteString("//\n")
	b.WriteString("// The tables are the ones `./x run src/tools/unicode-table-generator` wrote\n")
	fmt.Fprintf(&b, "// into the Rust source for Unicode %s.%s.%s, copied number for number; only the\n", version[1], version[2], version[3])
	b.WriteString("// properties that char::escape_debug_ext and char::is_whitespace read are\n")
	b.WriteString("// carried over. The lookup functions that read them are in fmt_unicode.go.\n")
	b.WriteString("\n")
	b.WriteString("package rs\n")
	b.WriteString("\n")
	b.WriteString("// fmtUnicodeVersion is core::unicode::UNICODE_VERSION.\n")
	fmt.Fprintf(&b, "var fmtUnicodeVersion = [3]uint8{%s, %s, %s}\n", version[1], version[2], version[3])
	b.WriteString("\n")

	for _, table := range []struct{ rust, goName string }{
		{"cf", "Cf"},
		{"cn_planes_0_3", "CnPlanes03"},
		{"default_ignorable_code_point", "DefaultIgnorableCodePoint"},
		{"grapheme_extend", "GraphemeExtend"},
	} {
		headers, offsets, low := skipList(text, table.rust)
		fmt.Fprintf(&b, "// unicode_data::%s\n", table.rust)
		fmt.Fprintf(&b, "const fmtUnicode%sFirst = 0x%x\n", table.goName, low)
		b.WriteString("\n")
		fmt.Fprintf(&b, "var fmtUnicode%sShortOffsetRuns = [%d]uint32{\n", table.goName, len(headers))
		for i := 0; i < len(headers); i += 4 {
			var line []string
			for _, header := range headers[i:min(i+4, len(headers))] {
				line = append(line, fmt.Sprintf("%d<<21 | %d", header[0], header[1]))
			}
			b.WriteString("\t" + strings.Join(line, ", ") + ",\n")
		}
		b.WriteString("}\n\n")
		fmt.Fprintf(&b, "var fmtUnicode%sOffsets = [%d]uint8{\n", table.goName, len(offsets))
		writeInts(&b, offsets, 20)
		b.WriteString("}\n\n")
	}

	whiteSpace := module(text, "white_space")
	// The match in white_space::lookup is carried over by hand in
	// fmt_unicode.go; make sure it still reads the way that port assumes.
	const lookup = `match c as u32 >> 8 {
            0 => WHITESPACE_MAP[c as usize & 0xff] & 1 != 0,
            22 => c as u32 == 0x1680,
            32 => WHITESPACE_MAP[c as usize & 0xff] & 2 != 0,
            48 => c as u32 == 0x3000,
            _ => false,
        }`
	if !strings.Contains(whiteSpace, lookup) {
		fail("white_space::lookup changed")
	}
	whitespaceMap := regexp.MustCompile(`(?s)static WHITESPACE_MAP: \[u8; 256\] = \[(.*?)\];`).FindStringSubmatch(whiteSpace)
	if whitespaceMap == nil {
		fail("no WHITESPACE_MAP")
	}
	values := numbers(whitespaceMap[1])
	if len(values) != 256 {
		fail("WHITESPACE_MAP has %d entries", len(values))
	}
	b.WriteString("// unicode_data::white_space\n")
	b.WriteString("var fmtUnicodeWhitespaceMap = [256]uint8{\n")
	writeInts(&b, values, 32)
	b.WriteString("}\n")

	formatted, err := format.Source([]byte(b.String()))
	if err != nil {
		fail("generated code does not parse: %v", err)
	}
	if err := os.WriteFile(*out, formatted, 0o644); err != nil {
		fail("%v", err)
	}
}

// module returns the body of `pub mod name { ... }`.
func module(text, name string) string {
	match := regexp.MustCompile(`(?s)pub mod ` + name + ` \{(.*?)\n\}\n`).FindStringSubmatch(text)
	if match == nil {
		fail("no module %s", name)
	}
	return match[1]
}

// skipList returns a skip-list table's ShortOffsetRunHeader values (start
// index, prefix sum), its offsets, and the first code point lookup reads.
func skipList(text, name string) (headers [][2]int, offsets []int, low int) {
	body := module(text, name)
	runs := regexp.MustCompile(`(?s)static SHORT_OFFSET_RUNS: \[ShortOffsetRunHeader; (\d+)\] = \[(.*?)\];`).FindStringSubmatch(body)
	offsetTable := regexp.MustCompile(`(?s)static OFFSETS: \[u8; (\d+)\] = \[(.*?)\];`).FindStringSubmatch(body)
	if runs == nil || offsetTable == nil {
		fail("%s: no SHORT_OFFSET_RUNS or OFFSETS", name)
	}
	for _, header := range regexp.MustCompile(`ShortOffsetRunHeader::new\((\d+), (\d+)\)`).FindAllStringSubmatch(runs[2], -1) {
		start, _ := strconv.Atoi(header[1])
		prefixSum, _ := strconv.Atoi(header[2])
		headers = append(headers, [2]int{start, prefixSum})
	}
	if declared, _ := strconv.Atoi(runs[1]); declared != len(headers) {
		fail("%s: %d of %d headers", name, len(headers), declared)
	}
	offsets = numbers(offsetTable[2])
	if declared, _ := strconv.Atoi(offsetTable[1]); declared != len(offsets) {
		fail("%s: %d of %d offsets", name, len(offsets), declared)
	}
	first := regexp.MustCompile(`\(c as u32\) >= (0x[0-9a-f]+) && lookup_slow\(c\)`).FindStringSubmatch(body)
	if first == nil {
		fail("%s: no lookup bound", name)
	}
	bound, err := strconv.ParseInt(first[1][2:], 16, 64)
	if err != nil {
		fail("%s: %v", name, err)
	}
	return headers, offsets, int(bound)
}

func numbers(text string) []int {
	var values []int
	for _, field := range regexp.MustCompile(`\d+`).FindAllString(text, -1) {
		value, _ := strconv.Atoi(field)
		values = append(values, value)
	}
	return values
}

func writeInts(b *strings.Builder, values []int, perLine int) {
	for i := 0; i < len(values); i += perLine {
		var line []string
		for _, value := range values[i:min(i+perLine, len(values))] {
			line = append(line, strconv.Itoa(value))
		}
		b.WriteString("\t" + strings.Join(line, ", ") + ",\n")
	}
}

func fail(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "fmt_unicode_data_gen: "+format+"\n", args...)
	os.Exit(1)
}
