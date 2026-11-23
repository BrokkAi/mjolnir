//go:build linux

// Golden test of the engine's command line (mj-microvm/src/cli.rs).
//
// testdata/cli-parse.txt was captured from the Rust engine
// (`mj-worker microvm`, clap 4.6): for each case, the words given, what clap
// printed (help on stdout, usage errors on stderr) and the exit status. No
// case parses successfully, so none of them touches the engine's state.
// Version output is left out: it names the build's version.

package microvm

import (
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"
)

func TestMicrovmCommandLineHelpAndErrorsMatchClap(t *testing.T) {
	expected, err := os.ReadFile("testdata/cli-parse.txt")
	if err != nil {
		t.Fatal(err)
	}
	var actual strings.Builder
	for _, line := range strings.Split(string(expected), "\n") {
		header, ok := strings.CutPrefix(line, "=== ")
		if !ok {
			continue
		}
		fmt.Fprintf(&actual, "=== %s\n", header)
		_, err := microvmCLIParse(strings.Fields(header))
		var clapErr *microvmCLIError
		if !errors.As(err, &clapErr) {
			t.Fatalf("%q parsed: %v", header, err)
		}
		status := 0
		if clapErr.useStderr {
			status = microvmEngineFailure
		}
		fmt.Fprintf(&actual, "%sexit=%d\n", clapErr.text, status)
	}
	if actual.String() != string(expected) {
		expectedCases := strings.Split(string(expected), "=== ")
		actualCases := strings.Split(actual.String(), "=== ")
		for i := range min(len(expectedCases), len(actualCases)) {
			if expectedCases[i] != actualCases[i] {
				t.Fatalf("case differs\n--- Rust:\n%s\n--- Go:\n%s", expectedCases[i], actualCases[i])
			}
		}
		t.Fatalf("output differs in length: %d cases expected, %d produced", len(expectedCases), len(actualCases))
	}
}
