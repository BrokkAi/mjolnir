// Port of mj-core/src/golden.rs.

// Small helpers for checked-in, full-surface golden tests.

package core

import (
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

// AssertGolden compares rendered output with
// <manifestDir>/tests/golden/<name>.txt and fails t with a unified diff when
// they differ.
//
// Set MJ_UPDATE_GOLDEN=1 to write the current output to that file.
func AssertGolden(t testing.TB, manifestDir, name, actual string) {
	t.Helper()
	path := filepath.Join(manifestDir, "tests", "golden", name+".txt")
	if value, ok := os.LookupEnv("MJ_UPDATE_GOLDEN"); ok && value == "1" {
		parent := filepath.Dir(path)
		if err := os.MkdirAll(parent, 0o777); err != nil {
			t.Fatalf("could not create golden directory %s: %v", parent, err)
		}
		if err := os.WriteFile(path, []byte(normalizeOneTrailingNewline(actual)+"\n"), 0o666); err != nil {
			t.Fatalf("could not write golden %s: %v", path, err)
		}
		return
	}
	expectedBytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("could not read golden %s: %v\nset MJ_UPDATE_GOLDEN=1 to create or update it", path, err)
	}
	expected := normalizeOneTrailingNewline(string(expectedBytes))
	actual = normalizeOneTrailingNewline(actual)
	if expected != actual {
		t.Fatalf("golden output differs at %s\n%s\nset MJ_UPDATE_GOLDEN=1 to update it", path, unifiedLineDiff(expected, actual, "expected", "actual"))
	}
}

// AssertPlatformGolden is like AssertGolden for surfaces that show platform
// key names (Cmd-V on macOS): macOS compares with <name>-macos.txt.
func AssertPlatformGolden(t testing.TB, manifestDir, name, actual string) {
	t.Helper()
	if runtime.GOOS == "darwin" {
		AssertGolden(t, manifestDir, name+"-macos", actual)
	} else {
		AssertGolden(t, manifestDir, name, actual)
	}
}

func normalizeOneTrailingNewline(text string) string {
	return strings.TrimSuffix(text, "\n")
}

// unifiedLineDiff is a line diff of expected and actual in unified format
// with three lines of context, as `similar::TextDiff::from_lines(..)
// .unified_diff().header(..)` writes it for a failing golden.
func unifiedLineDiff(expected, actual, fromName, toName string) string {
	before, after := goldenLines(expected), goldenLines(actual)
	// common[i][j] is the length of the longest common subsequence of
	// before[i:] and after[j:].
	common := make([][]int, len(before)+1)
	for i := range common {
		common[i] = make([]int, len(after)+1)
	}
	for i := len(before) - 1; i >= 0; i-- {
		for j := len(after) - 1; j >= 0; j-- {
			if before[i] == after[j] {
				common[i][j] = common[i+1][j+1] + 1
			} else {
				common[i][j] = max(common[i+1][j], common[i][j+1])
			}
		}
	}
	type op struct {
		kind     byte
		line     string
		beforeAt int
		afterAt  int
	}
	var ops []op
	i, j := 0, 0
	for i < len(before) || j < len(after) {
		switch {
		case i < len(before) && j < len(after) && before[i] == after[j]:
			ops = append(ops, op{' ', before[i], i, j})
			i, j = i+1, j+1
		case j == len(after) || i < len(before) && common[i+1][j] >= common[i][j+1]:
			ops = append(ops, op{'-', before[i], i, j})
			i++
		default:
			ops = append(ops, op{'+', after[j], i, j})
			j++
		}
	}
	const context = 3
	var out strings.Builder
	fmt.Fprintf(&out, "--- %s\n+++ %s\n", fromName, toName)
	for start := 0; start < len(ops); {
		if ops[start].kind == ' ' {
			start++
			continue
		}
		first := max(start-context, 0)
		last := start
		for index := start; index < len(ops); index++ {
			if ops[index].kind != ' ' {
				last = index
			} else if index-last > 2*context {
				break
			}
		}
		end := min(last+context+1, len(ops))
		beforeCount, afterCount := 0, 0
		for _, entry := range ops[first:end] {
			if entry.kind != '+' {
				beforeCount++
			}
			if entry.kind != '-' {
				afterCount++
			}
		}
		fmt.Fprintf(&out, "@@ -%s +%s @@\n",
			goldenRange(ops[first].beforeAt, beforeCount), goldenRange(ops[first].afterAt, afterCount))
		for _, entry := range ops[first:end] {
			out.WriteByte(entry.kind)
			out.WriteString(entry.line)
			if !strings.HasSuffix(entry.line, "\n") {
				out.WriteString("\n\\ No newline at end of file\n")
			}
		}
		start = end
	}
	return out.String()
}

// goldenLines splits text into lines that keep their newlines.
func goldenLines(text string) []string {
	lines := []string{}
	for text != "" {
		end := strings.IndexByte(text, '\n') + 1
		if end == 0 {
			end = len(text)
		}
		lines = append(lines, text[:end])
		text = text[end:]
	}
	return lines
}

func goldenRange(start, count int) string {
	if count == 0 {
		return fmt.Sprintf("%d,0", start)
	}
	if count == 1 {
		return fmt.Sprint(start + 1)
	}
	return fmt.Sprintf("%d,%d", start+1, count)
}
