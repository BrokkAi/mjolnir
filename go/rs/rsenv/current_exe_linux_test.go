package rsenv

import (
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Linux names an executable that has been removed with a " (deleted)"
// suffix, and Rust's current_exe keeps it (os.Executable takes it off).
// Mjolnir's worker staging relies on seeing it. The test runs a copy of this
// test binary that removes itself and reports what CurrentExe says then.
func TestCurrentExeKeepsTheDeletedSuffix(t *testing.T) {
	if os.Getenv("MJ_RSENV_DELETED_EXE_CHILD") == "1" {
		exe := rs.Unwrap(CurrentExe())
		if err := os.Remove(exe); err != nil {
			t.Fatal(err)
		}
		os.Stdout.WriteString(rs.Unwrap(CurrentExe()))
		os.Exit(0)
	}

	self, err := os.Open("/proc/self/exe")
	if err != nil {
		t.Fatal(err)
	}
	defer self.Close()
	copyPath := filepath.Join(t.TempDir(), "rsenv-copy.test")
	copied, err := os.OpenFile(copyPath, os.O_CREATE|os.O_WRONLY, 0o755)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.Copy(copied, self); err != nil {
		t.Fatal(err)
	}
	if err := copied.Close(); err != nil {
		t.Fatal(err)
	}

	child := exec.Command(copyPath, "-test.run=^TestCurrentExeKeepsTheDeletedSuffix$")
	child.Env = append(os.Environ(), "MJ_RSENV_DELETED_EXE_CHILD=1")
	output, err := child.Output()
	if err != nil {
		t.Fatalf("the copy failed: %v\n%s", err, output)
	}
	if got, want := string(output), copyPath+" (deleted)"; got != want {
		t.Errorf("current_exe after removal = %q, expected %q", got, want)
	}
}
