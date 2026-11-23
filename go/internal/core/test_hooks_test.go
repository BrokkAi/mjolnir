// Port of the tests in mj-core/src/test_hooks.rs.

package core

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

func TestTestHooksStayOffWithoutExplicitOptIn(t *testing.T) {
	if TestHooksEnabled() || TestFixturesEnabled() {
		t.Fatal("test hooks or fixtures are on without an opt-in")
	}
}

func TestFakeCommandPreservesArgumentsOutputAndStatusWithRestrictedPath(t *testing.T) {
	skipUnlessUnix(t)
	directory := filepath.Join(t.TempDir(), "tools with spaces")
	if err := os.Mkdir(directory, 0o777); err != nil {
		t.Fatal(err)
	}
	InstallFakeCommand(directory, "fake tool", "printf '<%s>\\n' \"$@\"; printf 'diagnostic\\n' >&2; exit 23\n")
	command := func() *exec.Cmd {
		// Rust resolves the program against the child's PATH; here it is
		// looked up in the same restricted directory.
		command := exec.Command(filepath.Join(directory, "fake tool"), "two words", "", "quote'and$dollar")
		command.Args[0] = "fake tool"
		command.Env = []string{"PATH=" + directory}
		return command
	}
	output, err := RunWithInput(command(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status.ExitCode() != 23 {
		t.Fatalf("status %v", output.Status)
	}
	if string(output.Stdout) != "<two words>\n<>\n<quote'and$dollar>\n" {
		t.Fatalf("stdout %q", output.Stdout)
	}
	if string(output.Stderr) != "diagnostic\n" {
		t.Fatalf("stderr %q", output.Stderr)
	}

	InstallFakeCommand(directory, "fake tool", "printf 'replacement\\n'\n")
	output, err = RunWithInput(command(), nil)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("status %v", output.Status)
	}
	if string(output.Stdout) != "replacement\n" || len(output.Stderr) != 0 {
		t.Fatalf("stdout %q stderr %q", output.Stdout, output.Stderr)
	}
}

func TestFakeCommandStreamsInputWhileItsScriptIsOpenForWriting(t *testing.T) {
	skipUnlessUnix(t)
	directory := t.TempDir()
	InstallFakeCommand(directory, "echo-input", "exec /bin/cat\n")
	// An inherited writer can outlive installation. Reading the script
	// through an interpreter must still work while that writer is open.
	writer, err := os.OpenFile(filepath.Join(directory, "echo-input.script"), os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	input := bytes.Repeat([]byte{'x'}, 200*1024)
	output, err := RunWithInput(exec.Command(filepath.Join(directory, "echo-input")), input)
	_ = writer.Close()
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("status %v", output.Status)
	}
	if !bytes.Equal(output.Stdout, input) || len(output.Stderr) != 0 {
		t.Fatalf("stdout %d bytes, stderr %q", len(output.Stdout), output.Stderr)
	}
}
