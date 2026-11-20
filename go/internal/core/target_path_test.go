// Port of the tests in mj-core/src/target_path.rs.

package core

import (
	"runtime"
	"testing"
)

// The POSIX behavior only differs from the standard library on Windows.
// Hard-won: #1238: home-relative SSH worker roots and remote clones were joined with `\` on Windows.
func TestTargetPathsKeepPosixSeparatorsOnAWindowsController(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("POSIX target paths only differ from host paths on Windows")
	}
	// The raw spelling, not Text, which would hide a `\`.
	for _, row := range [][3]string{
		{".local/share/hel/workers/s", "control.sock", ".local/share/hel/workers/s/control.sock"},
		{"/home/dev/project", `.mj\clones\s`, "/home/dev/project/.mj/clones/s"},
		{"/home/dev/project", "", "/home/dev/project"},
		{"", "memory", "memory"},
		{"/workspace", "/home/dev", "/home/dev"},
		{`C:\Users\dev`, ".codex", `C:\Users\dev\.codex`},
	} {
		if got := Join(row[0], row[1]); got != row[2] {
			t.Errorf("Join(%q, %q) = %q, want %q", row[0], row[1], got, row[2])
		}
	}
	resolved, err := ResolveGitPath("/home/dev/project", ".git/info/exclude")
	if err != nil || resolved != "/home/dev/project/.git/info/exclude" {
		t.Fatalf("resolved = %q, %v", resolved, err)
	}
}
