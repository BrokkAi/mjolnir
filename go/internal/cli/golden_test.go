package cli

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"
)

// The expected files were captured from the Rust mj, so these golden tests
// hold the Go build to the same command-line surface.
func TestGoldenRootHelpMatchesTheRustBuild(t *testing.T) {
	t.Setenv("MJ_INSTANCE", "")
	os.Unsetenv("MJ_INSTANCE")
	os.Unsetenv("MJ_NO_SPLASH")
	var stdout, stderr bytes.Buffer
	if code := Main([]string{"--help"}, &stdout, &stderr); code != 0 {
		t.Fatalf("exit %d: %s", code, stderr.String())
	}
	assertGolden(t, "root-help.golden", stdout.String())
}

func TestGoldenUnknownCommandMatchesTheRustBuild(t *testing.T) {
	var stdout, stderr bytes.Buffer
	if code := Main([]string{"nosuch"}, &stdout, &stderr); code != 2 {
		t.Fatalf("exit %d", code)
	}
	assertGolden(t, "unknown-command.golden", stderr.String())
}

func assertGolden(t *testing.T, name, actual string) {
	t.Helper()
	path := filepath.Join("testdata", name)
	if os.Getenv("MJ_UPDATE_GOLDEN") != "" {
		if err := os.WriteFile(path, []byte(actual), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	expected, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(expected) != actual {
		t.Errorf("%s differs:\n--- expected\n%s\n--- actual\n%s", name, expected, actual)
	}
}
