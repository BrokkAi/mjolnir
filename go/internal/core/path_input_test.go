// Port of the tests in mj-core/src/path_input.rs.

package core

import "testing"

func TestExpandsOnlyTheLeadingHomeComponent(t *testing.T) {
	home := "/home/test user"
	for _, row := range [][2]string{
		{"~", "/home/test user"},
		{"~/資料/a b", "/home/test user/資料/a b"},
		{"relative/~", "relative/~"},
		{"/absolute/~", "/absolute/~"},
		{"./~", "./~"},
		{"$HOME/file", "$HOME/file"},
		{"~/$(touch nope)", "/home/test user/$(touch nope)"},
	} {
		expanded, err := ExpandHome(row[0], &home)
		if err != nil {
			t.Fatalf("%q: %v", row[0], err)
		}
		// Rust compares the paths, so a trailing separator does not count.
		if comparePaths(expanded, row[1]) != 0 {
			t.Fatalf("%q expanded to %q, want %q", row[0], expanded, row[1])
		}
	}
	if _, err := ExpandHome("~/file", nil); err == nil {
		t.Fatal("expanded without a home")
	}
	if _, err := ExpandHome("~someone/file", &home); err == nil {
		t.Fatal("expanded a named user")
	}
	relative := "relative"
	if _, err := ExpandHome("~/file", &relative); err == nil {
		t.Fatal("expanded onto a relative home")
	}
}
