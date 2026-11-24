package cli

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// unsetHelpEnvironment clears the variables whose values the help shows
// (`[env: MJ_INSTANCE=]`), as the Rust captures were taken without them.
func unsetHelpEnvironment(t *testing.T) {
	t.Helper()
	for _, name := range []string{"MJ_INSTANCE", "MJ_NO_SPLASH"} {
		t.Setenv(name, "")
		if err := os.Unsetenv(name); err != nil {
			t.Fatal(err)
		}
	}
}

// The expected files were captured from the Rust mj, so these golden tests
// hold the Go build to the same command-line surface.
func TestGoldenRootHelpMatchesTheRustBuild(t *testing.T) {
	unsetHelpEnvironment(t)
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

// Every command whose arguments are ported prints, for `-h` and for
// `--help`, the help the Rust mj at the pinned commit prints. The files in
// testdata/help were captured from that build (`mj <command> --help`, with
// MJ_INSTANCE and MJ_NO_SPLASH unset); clap prints the same text for `-h`
// because none of these commands has a long help. The commands whose
// arguments arrive with their own files (image, go, simple, acp, ec2,
// import) are not here yet.
func TestGoldenHelpOfEveryPortedCommandMatchesTheRustBuild(t *testing.T) {
	unsetHelpEnvironment(t)
	entries, err := os.ReadDir(filepath.Join("testdata", "help"))
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) == 0 {
		t.Fatal("no help captures in testdata/help")
	}
	for _, entry := range entries {
		name := strings.TrimSuffix(entry.Name(), ".txt")
		command := strings.Split(name, "_")
		for _, flag := range []string{"--help", "-h"} {
			t.Run(name+flag, func(t *testing.T) {
				var stdout, stderr bytes.Buffer
				if code := Main(append(append([]string(nil), command...), flag), &stdout, &stderr); code != 0 {
					t.Fatalf("exit %d: %s", code, stderr.String())
				}
				assertGolden(t, filepath.Join("help", entry.Name()), stdout.String())
			})
		}
	}
}

func assertGolden(t *testing.T, name, actual string) {
	t.Helper()
	path := filepath.Join("testdata", name)
	expected, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(expected) != actual {
		t.Errorf("%s differs:\n--- expected\n%s\n--- actual\n%s", name, expected, actual)
	}
}
