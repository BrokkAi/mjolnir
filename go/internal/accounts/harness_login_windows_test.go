// Port of the Windows-only test in mj-accounts/src/harness_login.rs.

package accounts

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// Hard-won: 50a0277d2: Windows login passed bare npm commands to Rust, which only resolves .exe.
func TestLoginResolvesAndRunsAnNpmStyleCmdShimFromTheChildPath(t *testing.T) {
	directory := t.TempDir()
	shim := filepath.Join(directory, "claude.cmd")
	if err := os.WriteFile(shim, []byte("@echo off\r\necho harness-ok\r\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{"PATH": directory}

	program := resolveHarnessProgram("claude", environment)
	if !strings.EqualFold(filepath.Base(program), "claude.cmd") {
		t.Fatalf("program = %q", program)
	}
	output, err := exec.Command(program).Output()
	if err != nil {
		t.Fatalf("%v: %s", err, output)
	}
	if !strings.Contains(string(output), "harness-ok") {
		t.Fatalf("output = %q", output)
	}
}
